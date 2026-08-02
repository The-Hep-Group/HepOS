//! Ring-3 process support.
//!
//! `run_elf(data)` loads an ELF64 binary into a fresh user address space
//! and enters ring 3 via IRETQ.  The process calls back into the kernel
//! via SYSCALL; when it calls exit(N) the longjmp in `do_exit` returns
//! control here and `run_elf` returns N.
//!
//! The APIC timer is left *unmasked* for the duration: the timer ISR can
//! fire while ring 3 is running, and its usual `context_switch` can carry a
//! ring-3 CPU state across the switch just like any other task's — `iretq`
//! back into ring 3 on resume works the same way `iretq` back into ring 0
//! does. `exec_async()` below relies on this: it runs `run_elf()` as its own
//! scheduler task instead of calling it inline from whichever task issued
//! the command, so the rest of the desktop (`task_blink`) keeps running
//! concurrently instead of freezing for the process's entire lifetime.
//!
//! ## Multi-process state
//!
//! Every piece of ring-3 bookkeeping that used to be a single global
//! (`USER_RUNNING`, `KERNEL_RETURN_RSP`, `EXIT_CODE`, `MMIO_NEXT_VA`,
//! `CURRENT_PID`, the captured-stdout buffer) — safe only because at most
//! one process ever ran at a time — is now a per-task `ProcSlot`, indexed by
//! `scheduler::current_task_index()`. Each `exec_async()` call gets its own
//! scheduler task (see that fn's doc comment) with its own private page
//! tables (`create_user_pml4()`), so several processes can genuinely be
//! in-flight at once: one blocked in `SYS_WAIT_IRQ`, another mid-syscall,
//! another just starting, without stepping on each other's state. The one
//! piece of *machine* state this still requires care around is CR3 — see
//! `scheduler::Task::cr3`'s doc comment for the other half of this fix.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use alloc::{string::String, vec::Vec};
use spin::Mutex;
use crate::{elf, paging, pmm, scheduler, vmm};

// ── User virtual address layout ───────────────────────────────────────────────

// 8 pages (32 KB) — a single page turned out too thin for format!-heavy code
// (hepos-std's println!/format! machinery, once nested a couple of calls
// deep with real arguments, faulted past the top of a 1-page stack; see the
// hwtest proof-of-concept in PLAN.md's "Userspace drivers" writeup).
const USER_STACK_PAGES: u64 = 8;
const USER_STACK_TOP:   u64 = 0x7FFF_F000;   // RSP starts here (top of the region)
const USER_STACK_BASE:  u64 = USER_STACK_TOP - USER_STACK_PAGES * 4096;

const USER_MMIO_BASE: u64 = 0x6000_0000;

// ── Per-task ring-3 process state ─────────────────────────────────────────────
//
// A fixed-size table indexed by `scheduler::current_task_index()` (the
// bounded *array position* of the currently-running task, not its
// ever-incrementing `id`) — one slot per concurrently-alive task capable of
// hosting a ring-3 process. `MAX_TASKS` is a soft cap on how many tasks can
// be alive *at once*, not how many ever run over a session (dead slots are
// reused by `scheduler::spawn()`, so this doesn't grow with uptime) —
// generous for this project's scale; `slot_index()` clamps rather than
// panics if it's ever exceeded, trading a wrong-but-harmless slot collision
// for never crashing the kernel over it.
const MAX_TASKS: usize = 32;

#[derive(Clone, Copy)]
struct ProcSlot {
    /// Saved kernel RSP from `enter_ring3`; `do_exit` restores it to return.
    kernel_return_rsp: u64,
    /// Written by `do_exit`, read by `run_elf` after it returns.
    exit_code: u64,
    /// Set while *this task's* user process is executing; `sys_exit` checks
    /// the current task's copy of this before calling `do_exit`.
    user_running: bool,
    /// Bump allocator cursor for `SYS_MMAP_MMIO`, private per process since
    /// each has its own low-half page tables — no cross-process collision
    /// risk, so every process can safely start from the same base.
    mmio_next_va: u64,
    pid: u32,
}

impl ProcSlot {
    const fn empty() -> Self {
        Self { kernel_return_rsp: 0, exit_code: 0, user_running: false, mmio_next_va: 0, pid: 0 }
    }
}

static mut PROC_SLOTS: [ProcSlot; MAX_TASKS] = [ProcSlot::empty(); MAX_TASKS];

fn slot_index() -> usize {
    scheduler::current_task_index().min(MAX_TASKS - 1)
}

fn slot() -> &'static mut ProcSlot {
    unsafe { &mut PROC_SLOTS[slot_index()] }
}

/// PID of whichever process is running on the *calling* task right now (0 if
/// none) — backs `SYS_GETPID`.
pub fn current_pid() -> u32 { slot().pid }

/// Whether the calling task currently has a ring-3 process running — backs
/// `sys_exit`'s "is there actually anything to longjmp out of" check.
pub fn is_user_running() -> bool { slot().user_running }

/// Maps `len` bytes of physical MMIO space (rounded to page granularity) into
/// the calling process's currently-loaded page tables. Returns the mapped
/// user virtual address (with `phys`'s in-page offset preserved), or 0 if no
/// process is running on the calling task or the request is out of bounds.
pub fn map_mmio_for_user(phys: u64, len: u64) -> u64 {
    let s = slot();
    if !s.user_running { return 0; }
    if len == 0 || len > 16 * 1024 * 1024 { return 0; }
    let page_off  = phys & 0xFFF;
    let phys_base = phys & !0xFFF;
    let pages     = (page_off + len + 0xFFF) / 4096;
    let virt_base = s.mmio_next_va;
    s.mmio_next_va += pages * 4096;
    for i in 0..pages {
        paging::map_page_current_user(virt_base + i * 4096, phys_base + i * 4096,
            paging::WRITE | paging::NOCACHE);
    }
    virt_base + page_off
}

/// Allocate `len` bytes (rounded up to page granularity) of fresh, zeroed,
/// anonymous physical memory and map it into the calling process's page
/// tables. Backs `SYS_MMAP_ANON` — unlike `map_mmio_for_user()`, this isn't
/// physical-address passthrough (no `phys` the caller supplies, no
/// allowlist check makes sense), it's ordinary kernel-owned RAM handed to
/// the process, needed because the 256KB static bump heap every userspace
/// program gets (`userspace/hepos-rt`'s `HEAP` array) is nowhere near enough
/// for something like a full-resolution framebuffer backbuffer. Shares the
/// same per-process bump-VA cursor as `map_mmio_for_user()` (`mmio_next_va`)
/// — one arena, not tracked separately, since both just need "the next free
/// slice of this process's low-half address space."  Returns the mapped
/// user virtual address, or 0 if no process is running on the calling task,
/// the request is zero/too large, or a page allocation fails partway
/// through (already-mapped pages are left mapped — process teardown reclaims
/// all of a process's page-table-reachable memory anyway, so a partial
/// mapping here isn't a leak beyond the process's own lifetime).
pub fn map_anon_for_user(len: u64) -> u64 {
    let s = slot();
    if !s.user_running { return 0; }
    if len == 0 || len > 64 * 1024 * 1024 { return 0; }
    let pages = (len + 0xFFF) / 4096;
    let virt_base = s.mmio_next_va;
    s.mmio_next_va += pages * 4096;
    for i in 0..pages {
        let Some(phys) = pmm::alloc_page() else { return 0; };
        unsafe { core::ptr::write_bytes(vmm::phys_to_virt(phys), 0, 4096); }
        paging::map_page_current_user(virt_base + i * 4096, phys, paging::WRITE);
    }
    virt_base
}

// ── Embedded test ELF ─────────────────────────────────────────────────────────
//
// A minimal ELF64 executable that calls write(1, "Hello from ring 3!\n", 19)
// and then exit(0).  Entry point = 0x400078 (first byte after ELF + phdr).
//
// Layout:
//   bytes   0 –  63: ELF header (64 bytes)
//   bytes  64 – 119: PT_LOAD program header (56 bytes)
//   bytes 120 – 182: code + message (63 bytes)  → loaded at VA 0x400078
//
// The lea rsi,[rip+0x17] at offset 14 within the code resolves to 0x4000A4
// (RIP after the 7-byte instruction = 0x40008D; 0x40008D + 0x17 = 0x4000A4),
// where the 19-byte message string lives.
static TEST_ELF: [u8; 183] = [
    // ── ELF header (64 bytes) ────────────────────────────────────────────────
    0x7f, b'E', b'L', b'F',                          // magic
    0x02,                                              // EI_CLASS  = ELFCLASS64
    0x01,                                              // EI_DATA   = ELFDATA2LSB
    0x01,                                              // EI_VERSION
    0x00,                                              // EI_OSABI  = System V
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // padding
    0x02, 0x00,                                        // e_type    = ET_EXEC
    0x3e, 0x00,                                        // e_machine = x86-64
    0x01, 0x00, 0x00, 0x00,                            // e_version = 1
    0x78, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,  // e_entry   = 0x400078
    0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // e_phoff   = 64
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // e_shoff   = 0
    0x00, 0x00, 0x00, 0x00,                            // e_flags   = 0
    0x40, 0x00,                                        // e_ehsize  = 64
    0x38, 0x00,                                        // e_phentsize = 56
    0x01, 0x00,                                        // e_phnum   = 1
    0x40, 0x00,                                        // e_shentsize = 64
    0x00, 0x00,                                        // e_shnum   = 0
    0x00, 0x00,                                        // e_shstrndx = 0
    // ── PT_LOAD program header (56 bytes) ────────────────────────────────────
    0x01, 0x00, 0x00, 0x00,                            // p_type   = PT_LOAD
    0x05, 0x00, 0x00, 0x00,                            // p_flags  = PF_R | PF_X
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_offset = 0 (load whole file)
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_vaddr  = 0x400000
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_paddr  = 0x400000
    0xb7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_filesz = 183
    0xb7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_memsz  = 183
    0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // p_align  = 0x1000
    // ── Code + message (63 bytes, loaded at VA 0x400078) ─────────────────────
    // mov rax, 1   (SYS_WRITE)
    0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00,
    // mov rdi, 1   (fd = stdout)
    0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
    // lea rsi, [rip + 0x17]  → message at 0x4000A4
    0x48, 0x8D, 0x35, 0x17, 0x00, 0x00, 0x00,
    // mov rdx, 19  (length)
    0x48, 0xC7, 0xC2, 0x13, 0x00, 0x00, 0x00,
    // syscall
    0x0F, 0x05,
    // mov rax, 60  (SYS_EXIT)
    0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
    // xor rdi, rdi (exit code 0)
    0x48, 0x31, 0xFF,
    // syscall
    0x0F, 0x05,
    // jmp -2  (fallback loop — sys_exit longjmps back before this)
    0xEB, 0xFE,
    // "Hello from ring 3!\n"
    b'H', b'e', b'l', b'l', b'o', b' ',
    b'f', b'r', b'o', b'm', b' ',
    b'r', b'i', b'n', b'g', b' ',
    b'3', b'!', b'\n',
];

// ── Process stdout capture buffer ─────────────────────────────────────────────
//
// sys_write appends bytes here (in addition to serial) while the calling
// task's process is running. Keyed by task index (same "sparse list of
// (key, value)" pattern `HEPFS_NAVS`/`EXTRA_TERMINALS` already use elsewhere
// in this codebase) rather than one shared buffer, so two processes running
// concurrently on different tasks don't interleave each other's output.
static PROC_OUT: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());

/// Append bytes to the calling task's process output capture buffer.
/// Called from syscall::sys_write while a process is running.
pub fn proc_write(bytes: &[u8]) {
    let idx = slot_index();
    let mut out = PROC_OUT.lock();
    match out.iter_mut().find(|(i, _)| *i == idx) {
        Some((_, buf)) => buf.extend_from_slice(bytes),
        None => out.push((idx, bytes.to_vec())),
    }
}

/// Take and return the calling task's buffered process output, clearing it.
fn take_proc_output() -> Vec<u8> {
    let idx = slot_index();
    let mut out = PROC_OUT.lock();
    match out.iter().position(|(i, _)| *i == idx) {
        Some(pos) => out.remove(pos).1,
        None => Vec::new(),
    }
}

// ── Process table ─────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum ProcState { Running, Exited }

struct ProcEntry {
    pid:       u32,
    name:      String,
    state:     ProcState,
    exit_code: u64,
}

const MAX_PROCS: usize = 32;

static PROCTAB:  Mutex<Vec<ProcEntry>> = Mutex::new(Vec::new());
static NEXT_PID: AtomicU32            = AtomicU32::new(1);

// ── Page table setup ──────────────────────────────────────────────────────────

fn read_cr3() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) v, options(nostack, nomem)); }
    v & !0xFFF
}

unsafe fn write_cr3(phys: u64) {
    core::arch::asm!("mov cr3, {}", in(reg) phys, options(nostack, nomem));
}

/// Allocate a fresh PML4 with the kernel high-half entries copied in.
fn create_user_pml4() -> u64 {
    let phys = pmm::alloc_page().expect("process: OOM for PML4");
    let virt = vmm::phys_to_virt(phys);
    let cur_cr3 = read_cr3();
    unsafe {
        core::ptr::write_bytes(virt, 0, 4096);
        let cur = vmm::phys_to_virt(cur_cr3) as *const u64;
        let new = virt as *mut u64;
        for i in 256..512usize {
            new.add(i).write_volatile(cur.add(i).read_volatile());
        }
    }
    phys
}

// ── Entry / exit ──────────────────────────────────────────────────────────────

/// IRETQ into ring 3 at `entry` / `stack_top`, handing the process one
/// launch argument (in RDI, so a `_start(arg: u64)` sees it as an ordinary
/// SysV first parameter — existing programs that declare `_start()` with no
/// parameters simply ignore it). Used for persistent driver processes (see
/// `rtl8139.rs`) to tell them where to find their shared mailbox page,
/// discovered at runtime and so impossible to bake into the ELF itself;
/// ordinary one-shot programs (`hello`/`hwtest`/`exec <file>`) just pass 0.
///
/// Saves callee-saved registers + RSP to `*krsp_ptr` — the calling task's own
/// `ProcSlot::kernel_return_rsp` (computed by `run_elf`, a plain Rust
/// function, and passed in as a third argument) rather than a single shared
/// global, so `do_exit()` can restore them and "return" from this function
/// even with multiple processes each mid-ring-3-excursion on different tasks.
#[unsafe(naked)]
unsafe extern "C" fn enter_ring3(entry: u64, stack_top: u64, krsp_ptr: u64, arg: u64) {
    // entry     → RDI (SysV arg 1)
    // stack_top → RSI (SysV arg 2)
    // krsp_ptr  → RDX (SysV arg 3)
    // arg       → RCX (SysV arg 4)
    core::arch::naked_asm!(
        // Save callee-saved registers.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // Record kernel stack (this task's own slot) so do_exit can longjmp back.
        "mov [rdx], rsp",
        // Build IRETQ frame.  CPU pops: RIP, CS, RFLAGS, RSP, SS.
        "push 0x1b",    // SS  = USER_DS | RPL3
        "push rsi",     // RSP = stack_top (arg 2)
        "push 0x202",   // RFLAGS: IF=1, bit-1 always set
        "push 0x23",    // CS  = USER_CS | RPL3
        "push rdi",     // RIP = entry    (arg 1)
        // rdi/rsi/rdx already captured on the stack above — free to reuse.
        // Ring 3 sees `arg` in RDI, matching a `_start(arg: u64)`'s own
        // first SysV parameter.
        "mov rdi, rcx",
        "iretq",
        // do_exit longjmps here by restoring RSP to the saved value above,
        // then executing the pop sequence + ret.
    );
}

/// Called by sys_exit from inside a syscall handler.
/// Restores the kernel frame left by enter_ring3 (for the *calling task's*
/// process — see `ProcSlot`) and returns from it.
///
/// SAFETY: must only be called while the calling task's `ProcSlot::user_running`
/// is true.
pub unsafe fn do_exit(code: u64) -> ! {
    let s = slot();
    s.exit_code = code;
    s.user_running = false;
    let krsp = s.kernel_return_rsp;
    core::arch::asm!(
        // syscall_entry did swapgs (GS=kernel, IA32_KERNEL_GS_BASE=user).
        // Undo it so next ring-3 entry starts with GS=user, KERNEL_GS_BASE=kernel.
        "swapgs",
        "mov rsp, {krsp}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        krsp = in(reg) krsp,
        options(noreturn),
    );
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load and run an ELF64 binary, recording it in the process table, and
/// block the calling task until it exits. Only ever called from
/// `async_task_entry()` below now — a *task's own* execution blocking on
/// this is fine and expected (that's exactly what frees up every other task
/// to keep running); nothing outside this module calls it directly, see
/// `exec_async()`.
fn exec_blocking(name: &str, data: &[u8], arg: u64) -> Result<u64, &'static str> {
    // Clear any stale output left over if this task's slot is being reused
    // (see `PROC_OUT`'s doc comment).
    let _ = take_proc_output();

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

    {
        let mut tab = PROCTAB.lock();
        // Drop oldest exited entry if table is full
        if tab.len() >= MAX_PROCS {
            if let Some(i) = tab.iter().position(|e| e.state == ProcState::Exited) {
                tab.remove(i);
            }
        }
        tab.push(ProcEntry {
            pid,
            name: String::from(name),
            state: ProcState::Running,
            exit_code: 0,
        });
    }

    let result = run_elf(pid, data, arg);

    {
        let mut tab = PROCTAB.lock();
        if let Some(e) = tab.iter_mut().find(|e| e.pid == pid) {
            e.state     = ProcState::Exited;
            e.exit_code = result.unwrap_or(u64::MAX);
        }
    }

    result
}

/// Iterate the process table, calling `f` for each entry.
/// Arguments: (pid, name, is_running, exit_code)
pub fn for_each_proc(mut f: impl FnMut(u32, &str, bool, u64)) {
    for e in PROCTAB.lock().iter() {
        f(e.pid, &e.name, e.state == ProcState::Running, e.exit_code);
    }
}

/// True if any process named `name` currently has `ProcState::Running`.
/// Backs the `service`/`kill` terminal commands' status checks — a driver
/// process's name (`<rtl8139d>` etc.) is always the same literal across
/// stop/start cycles, so this is enough to answer "is it running right now"
/// without the service layer needing to separately track a PID.
pub fn is_process_running(name: &str) -> bool {
    let mut found = false;
    for_each_proc(|_, n, running, _| { if running && n == name { found = true; } });
    found
}

/// The PID of the (at most one, by convention) currently-running process
/// named `name`, if any. Used by `kill <pid>` to map a PID back to a
/// service name.
pub fn pid_for_name(name: &str) -> Option<u32> {
    let mut found = None;
    for_each_proc(|pid, n, running, _| { if running && n == name { found = Some(pid); } });
    found
}

/// The name of the process currently holding `pid`, if it's still running.
/// Used by `kill <pid>` to map a PID back to a service name.
pub fn name_for_running_pid(pid: u32) -> Option<String> {
    let mut found = None;
    for_each_proc(|p, n, running, _| { if running && p == pid { found = Some(String::from(n)); } });
    found
}

/// Load an ELF64 binary from `data`, run it in a fresh user address space
/// (recording `pid` in this task's `ProcSlot` so `SYS_GETPID` sees it),
/// handing it `arg` as its one launch argument (see `enter_ring3`'s doc
/// comment — 0 for an ordinary program, a runtime-discovered value like a
/// shared mailbox's physical address for a persistent driver process), and
/// return its exit code.
fn run_elf(pid: u32, data: &[u8], arg: u64) -> Result<u64, &'static str> {
    let idx = slot_index();
    unsafe {
        PROC_SLOTS[idx] = ProcSlot { pid, mmio_next_va: USER_MMIO_BASE, ..ProcSlot::empty() };
    }
    let pml4 = create_user_pml4();

    let loaded = elf::load(data, pml4)?;

    // Map user stack (USER_STACK_PAGES pages, read/write)
    let mut stack_pages = Vec::with_capacity(USER_STACK_PAGES as usize);
    for i in 0..USER_STACK_PAGES {
        let p = pmm::alloc_page().ok_or("run_elf: OOM for stack")?;
        unsafe { core::ptr::write_bytes(vmm::phys_to_virt(p), 0, 4096); }
        paging::map_page_into(pml4, USER_STACK_BASE + i * 4096, p, paging::USER | paging::WRITE);
        stack_pages.push(p);
    }

    // Switch to user PML4 and enter ring 3. `set_current_cr3()` keeps this
    // task's own scheduler entry in sync so a preemption mid-ring-3 (or
    // mid-syscall, e.g. SYS_WAIT_IRQ blocking) restores the right address
    // space on resume — see `scheduler::Task::cr3`'s doc comment.
    let orig_cr3 = read_cr3();
    let krsp_ptr = unsafe { &mut PROC_SLOTS[idx].kernel_return_rsp as *mut u64 as u64 };
    unsafe {
        write_cr3(pml4);
        scheduler::set_current_cr3(pml4);
        PROC_SLOTS[idx].user_running = true;
        enter_ring3(loaded.entry, USER_STACK_TOP, krsp_ptr, arg);
        // Returns here after do_exit longjmp
    }
    unsafe { write_cr3(orig_cr3); }
    scheduler::set_current_cr3(orig_cr3);

    // Free user memory
    for p in stack_pages { pmm::free_page(p); }
    for phys in loaded.pages { pmm::free_page(phys); }
    // Note: intermediate page-table pages (PT/PD/PDP) are leaked here.
    // A full cleanup would walk the low-half of pml4.  Acceptable for now.
    pmm::free_page(pml4);

    Ok(unsafe { PROC_SLOTS[idx].exit_code })
}

// Baked-in hello ELF (generated by build.rs from userspace/target/.../hello).
// Empty slice if userspace hasn't been built yet.
include!(concat!(env!("OUT_DIR"), "/hello_elf.rs"));

// Baked-in hwtest ELF (generated by build.rs from userspace/target/.../hwtest).
include!(concat!(env!("OUT_DIR"), "/hwtest_elf.rs"));

// Baked-in memtest ELF (generated by build.rs from userspace/target/.../memtest)
// — proves out SYS_MMAP_ANON, see PLAN.md's Phase-1 desktop-migration writeup.
include!(concat!(env!("OUT_DIR"), "/memtest_elf.rs"));

// Baked-in inputtest ELF (generated by build.rs from
// userspace/target/.../inputtest) — proves out SYS_INPUT_STATE, see
// PLAN.md's Phase-1 desktop-migration writeup.
include!(concat!(env!("OUT_DIR"), "/inputtest_elf.rs"));

// Baked-in fstest ELF (generated by build.rs from userspace/target/.../fstest)
// — proves out the SYS_FS_* syscalls, see PLAN.md's Phase-1 desktop-migration
// writeup.
include!(concat!(env!("OUT_DIR"), "/fstest_elf.rs"));
include!(concat!(env!("OUT_DIR"), "/svctest_elf.rs"));

// ── Async process execution ───────────────────────────────────────────────────
//
// `exec_blocking()` above is only ever safe to block *something* on if that
// something is a task the scheduler already knows how to switch away from —
// which task_blink (the desktop loop) and kmain's own boot sequence aren't,
// as ordinary function calls. Calling it inline from `task_blink` (as the
// terminal commands used to) would freeze the entire desktop — mouse,
// rendering, every other window — for as long as the ring-3 process runs.
//
// The fix: give the process its own scheduler task (`scheduler::spawn()`)
// instead. `task_blink` keeps running concurrently, round-robining with it
// via ordinary timer preemption. Multiple background processes can now run
// truly concurrently (see this module's top doc comment on `ProcSlot`) —
// there's no single-job-at-a-time guard anymore; `PENDING_QUEUE`/`DONE_QUEUE`
// hold as many in-flight jobs as there are tasks to run them.
struct AsyncJob { issuer: usize, name: String, data: Vec<u8>, arg: u64 }

/// Jobs queued by `exec_async()`, each consumed by exactly one
/// `async_task_entry()` invocation (its own freshly-spawned task) — which
/// specific job a given task pops doesn't matter, since jobs are
/// independent and each spawn corresponds to exactly one push.
static PENDING_QUEUE: Mutex<Vec<AsyncJob>> = Mutex::new(Vec::new());

/// Finished jobs' `(issuer, result message)`, popped by `poll_async()` — one
/// entry per completed job, same mailbox shape `net::poll()` uses, just a
/// queue instead of a single slot now that more than one can finish between
/// polls.
static DONE_QUEUE: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

/// Count of background jobs currently queued or running.
static ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);

/// True while at least one background process is running (or queued to
/// run). No longer a single-job guard — `exec_async()` doesn't refuse a
/// second launch — just an informational count for callers (e.g. the
/// boot-time self-test) that want to know "is anything going on".
pub fn job_in_progress() -> bool { ACTIVE_JOBS.load(Ordering::Relaxed) > 0 }

/// Launches `data` as a background process without blocking the caller —
/// `issuer` is the terminal window id `poll_async()` should deliver the
/// eventual "exited: N" (or error) message to, same convention
/// `net::start_ping()`/`start_wget()` use. Returns immediately, before the
/// process has necessarily even started running. Multiple calls can be
/// in flight at once — each gets its own scheduler task and `ProcSlot`.
pub fn exec_async(issuer: usize, name: &str, data: &[u8]) -> Result<(), &'static str> {
    exec_async_with_arg(issuer, name, data, 0)
}

/// Same as `exec_async()`, but hands the new process `arg` as its one launch
/// argument (see `enter_ring3`'s doc comment) — for a persistent driver
/// process that needs to know a runtime-discovered address (e.g. its shared
/// mailbox's physical address) it has no other way to learn.
pub fn exec_async_with_arg(issuer: usize, name: &str, data: &[u8], arg: u64) -> Result<(), &'static str> {
    if data.is_empty() { return Err("nothing to run"); }
    ACTIVE_JOBS.fetch_add(1, Ordering::Relaxed);
    PENDING_QUEUE.lock().push(AsyncJob { issuer, name: String::from(name), data: data.to_vec(), arg });
    let _task_id = scheduler::spawn("user_proc", async_task_entry);
    // TEMPORARY DIAGNOSTIC: scheduler.rs reverted to the pre-session original
    // (plain flat round-robin, no set_priority_task()) for an isolation test
    // — see kernel/src/scheduler.rs's own note. Restore this block once that
    // test concludes.
    // if name == crate::xhci::SERVICE_NAME {
    //     scheduler::set_priority_task(_task_id);
    // }
    Ok(())
}

/// The spawned task's entire body: run one queued job to completion, stash
/// the result for `poll_async()`, then exit. Never returns — `entry: fn()
/// -> !` is all `scheduler::spawn()` accepts, so this can't capture
/// anything; the job itself is threaded through the `PENDING_QUEUE` mailbox.
fn async_task_entry() -> ! {
    // Deliberately NOT `if let Some(...) = PENDING_QUEUE.lock().pop() { ... }`
    // — that's a real Rust footgun: the temporary `MutexGuard` from `.lock()`
    // lives for the *entire if-let body* (its "scrutinee" expression, per
    // Rust's temporary-lifetime rules), not just the match itself. Since a
    // persistent driver process (RTL8139/HDA) never returns from
    // `exec_blocking()` below — it loops forever once in ring 3 — that would
    // hold `PENDING_QUEUE`'s lock *forever*, deadlocking every other task
    // that ever tries to pop its own job from the same queue. Popping into a
    // `let` first drops the guard immediately, before the body ever runs.
    let popped = PENDING_QUEUE.lock().pop();
    if let Some(AsyncJob { issuer, name, data, arg }) = popped {
        let result = exec_blocking(&name, &data, arg);
        let output = take_proc_output();
        let msg = match result {
            Ok(code) => {
                let mut s = String::new();
                if !output.is_empty() {
                    s.push_str(&String::from_utf8_lossy(&output));
                    if !s.ends_with('\n') { s.push('\n'); }
                }
                s.push_str(&alloc::format!("{} exited: {}", name, code));
                s
            }
            Err(e) => alloc::format!("{}: {}", name, e),
        };
        DONE_QUEUE.lock().push((issuer, msg));
    }
    ACTIVE_JOBS.fetch_sub(1, Ordering::Relaxed);
    scheduler::exit_current();
}

/// Advance the in-progress background processes, if any finished since the
/// last call. Call this once per main-loop frame, same as `net::poll()` —
/// returns `Some((issuer, message))` for one finished job per call (call in
/// a loop, or accept up to one delivered per frame, same as the existing
/// call site does); the caller prints `message` into the issuing terminal
/// window.
pub fn poll_async() -> Option<(usize, String)> {
    DONE_QUEUE.lock().pop()
}

/// Launch the embedded ELF test binary (prints "Hello from ring 3!") in the
/// background.
pub fn run_test_async(issuer: usize) -> Result<(), &'static str> {
    exec_async(issuer, "<test>", &TEST_ELF)
}

/// Launch the hello ELF (built from userspace/hello — exercises hepos-rt +
/// hepos-std) in the background.
pub fn run_hello_async(issuer: usize) -> Result<(), &'static str> {
    if HELLO_ELF.is_empty() {
        return Err("hello ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<hello>", HELLO_ELF)
}

/// Launch the hwtest ELF (built from userspace/hwtest — a proof-of-concept
/// userspace "driver" that reads real hardware entirely through
/// SYS_PORT_IN/OUT and SYS_MMAP_MMIO, see PLAN.md's "Userspace drivers"
/// writeup) in the background.
pub fn run_hwtest_async(issuer: usize) -> Result<(), &'static str> {
    if HWTEST_ELF.is_empty() {
        return Err("hwtest ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<hwtest>", HWTEST_ELF)
}

/// Launch the memtest ELF (built from userspace/memtest — proves out
/// SYS_MMAP_ANON, Phase 1 of the desktop-to-userspace migration, see
/// PLAN.md) in the background.
pub fn run_memtest_async(issuer: usize) -> Result<(), &'static str> {
    if MEMTEST_ELF.is_empty() {
        return Err("memtest ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<memtest>", MEMTEST_ELF)
}

/// Launch the inputtest ELF (built from userspace/inputtest — proves out
/// SYS_INPUT_STATE, Phase 1 of the desktop-to-userspace migration, see
/// PLAN.md) in the background.
pub fn run_inputtest_async(issuer: usize) -> Result<(), &'static str> {
    if INPUTTEST_ELF.is_empty() {
        return Err("inputtest ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<inputtest>", INPUTTEST_ELF)
}

/// Launch the fstest ELF (built from userspace/fstest — proves out the
/// SYS_FS_* syscalls, Phase 1 of the desktop-to-userspace migration, see
/// PLAN.md) in the background.
pub fn run_fstest_async(issuer: usize) -> Result<(), &'static str> {
    if FSTEST_ELF.is_empty() {
        return Err("fstest ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<fstest>", FSTEST_ELF)
}

/// Launch the svctest ELF (built from userspace/svctest — proves out
/// SYS_SERVICE_CTL/SYS_SERVICE_POLL/SYS_SPAWN_BYTES, Phase 1 item 4 — the
/// last of the desktop-to-userspace migration's foundational syscalls, see
/// PLAN.md) in the background.
pub fn run_svctest_async(issuer: usize) -> Result<(), &'static str> {
    if SVCTEST_ELF.is_empty() {
        return Err("svctest ELF not built — run build.ps1 first");
    }
    exec_async(issuer, "<svctest>", SVCTEST_ELF)
}
