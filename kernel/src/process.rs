//! Ring-3 process support.
//!
//! `run_elf(data)` loads an ELF64 binary into a fresh user address space
//! and enters ring 3 via IRETQ.  The process calls back into the kernel
//! via SYSCALL; when it calls exit(N) the longjmp in `do_exit` returns
//! control here and `run_elf` returns N.
//!
//! The APIC timer is left *unmasked* for the duration (this comment used to
//! say the opposite — stale, left over from before the scheduler supported
//! real preemption): the timer ISR can fire while ring 3 is running, and its
//! usual `context_switch` can carry a ring-3 CPU state across the switch
//! just like any other task's — `iretq` back into ring 3 on resume works
//! the same way `iretq` back into ring 0 does. That's what `exec_async()`
//! below relies on: it runs `run_elf()` as its own scheduler task instead of
//! calling it inline from whichever task issued the command, so the rest of
//! the desktop (`task_blink`) keeps running concurrently instead of freezing
//! for the process's entire lifetime.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

// ── MMIO-passthrough syscall support ──────────────────────────────────────────
//
// Foundational piece of PLAN.md's "move drivers to userspace" item: lets a
// ring-3 process ask the kernel (via SYS_MMAP_MMIO) to map a physical MMIO
// region into its own page tables, then poll it directly with no further
// syscalls — the same shape a real userspace driver would use. Only one
// process ever runs at a time (`run_elf` blocks until exit), so a single
// global bump cursor is enough; it resets on every `exec()` since each run
// gets a fresh PML4 anyway.
//
// Not yet scoped by permission (any process can map any physical address) —
// fine for this proof-of-concept, but real driver isolation needs a
// capability/allowlist check here before this is trusted with more than one
// cooperating process.
const USER_MMIO_BASE: u64 = 0x6000_0000;
static MMIO_NEXT_VA: AtomicU64 = AtomicU64::new(USER_MMIO_BASE);

/// Maps `len` bytes of physical MMIO space (rounded to page granularity) into
/// the calling process's currently-loaded page tables. Returns the mapped
/// user virtual address (with `phys`'s in-page offset preserved), or 0 if no
/// process is running or the request is out of bounds.
pub fn map_mmio_for_user(phys: u64, len: u64) -> u64 {
    if unsafe { !USER_RUNNING } { return 0; }
    if len == 0 || len > 16 * 1024 * 1024 { return 0; }
    let page_off  = phys & 0xFFF;
    let phys_base = phys & !0xFFF;
    let pages     = (page_off + len + 0xFFF) / 4096;
    let virt_base = MMIO_NEXT_VA.fetch_add(pages * 4096, Ordering::Relaxed);
    for i in 0..pages {
        paging::map_page_current_user(virt_base + i * 4096, phys_base + i * 4096,
            paging::WRITE | paging::NOCACHE);
    }
    virt_base + page_off
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
// sys_write appends bytes here (in addition to serial) while USER_RUNNING.
// After exec() returns the caller drains and displays the buffer in the terminal.
static PROC_OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Append bytes to the process output capture buffer.
/// Called from syscall::sys_write while a process is running.
pub fn proc_write(bytes: &[u8]) {
    PROC_OUT.lock().extend_from_slice(bytes);
}

/// Take and return all buffered process output, clearing the buffer.
fn take_proc_output() -> Vec<u8> {
    core::mem::take(&mut *PROC_OUT.lock())
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

/// PID of the currently-executing user process (0 = none).
pub static CURRENT_PID: AtomicU32 = AtomicU32::new(0);

// ── Process state ─────────────────────────────────────────────────────────────

/// Set while a user process is executing; sys_exit checks this.
pub static mut USER_RUNNING: bool = false;

/// Saved kernel RSP from enter_ring3; do_exit restores it to return.
static mut KERNEL_RETURN_RSP: u64 = 0;

/// Exit code written by do_exit; read by run_elf after return.
static mut EXIT_CODE: u64 = 0;

// ── CR3 helpers ───────────────────────────────────────────────────────────────

fn read_cr3() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) v, options(nostack, nomem)); }
    v & !0xFFF
}

unsafe fn write_cr3(phys: u64) {
    core::arch::asm!("mov cr3, {}", in(reg) phys, options(nostack, nomem));
}

// ── Page table setup ──────────────────────────────────────────────────────────

/// Allocate a fresh PML4 with the kernel high-half entries copied in.
fn create_user_pml4() -> u64 {
    let phys = pmm::alloc_page().expect("process: OOM for PML4");
    let virt = vmm::phys_to_virt(phys);
    unsafe {
        core::ptr::write_bytes(virt, 0, 4096);
        let cur = vmm::phys_to_virt(read_cr3()) as *const u64;
        let new = virt as *mut u64;
        for i in 256..512usize {
            new.add(i).write_volatile(cur.add(i).read_volatile());
        }
    }
    phys
}

// ── Entry / exit ──────────────────────────────────────────────────────────────

/// IRETQ into ring 3 at `entry` / `stack_top`.
///
/// Saves callee-saved registers + RSP to KERNEL_RETURN_RSP so that
/// `do_exit()` can restore them and "return" from this function.
#[unsafe(naked)]
unsafe extern "C" fn enter_ring3(entry: u64, stack_top: u64) {
    // entry    → RDI (SysV arg 1)
    // stack_top → RSI (SysV arg 2)
    core::arch::naked_asm!(
        // Save callee-saved registers.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // Record kernel stack so do_exit can longjmp back.
        "lea rax, [rip + {krsp}]",
        "mov [rax], rsp",
        // Build IRETQ frame.  CPU pops: RIP, CS, RFLAGS, RSP, SS.
        "push 0x1b",    // SS  = USER_DS | RPL3
        "push rsi",     // RSP = stack_top (arg 2)
        "push 0x202",   // RFLAGS: IF=1, bit-1 always set
        "push 0x23",    // CS  = USER_CS | RPL3
        "push rdi",     // RIP = entry    (arg 1)
        "iretq",
        // do_exit longjmps here by restoring RSP to the saved value above,
        // then executing the pop sequence + ret.
        krsp = sym KERNEL_RETURN_RSP,
    );
}

/// Called by sys_exit from inside a syscall handler.
/// Restores the kernel frame left by enter_ring3 and returns from it.
///
/// SAFETY: must only be called while USER_RUNNING == true.
pub unsafe fn do_exit(code: u64) -> ! {
    EXIT_CODE = code;
    USER_RUNNING = false;
    core::arch::asm!(
        // syscall_entry did swapgs (GS=kernel, IA32_KERNEL_GS_BASE=user).
        // Undo it so next ring-3 entry starts with GS=user, KERNEL_GS_BASE=kernel.
        "swapgs",
        "mov rsp, [{rsp}]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        rsp = sym KERNEL_RETURN_RSP,
        options(noreturn),
    );
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load and run an ELF64 binary, recording it in the process table, and
/// block the calling task until it exits. Only ever called from
/// `async_task_entry()` below now — a *task's own* execution blocking on
/// this is fine and expected (that's exactly what frees up every other task,
/// `task_blink` included, to keep running); nothing outside this module
/// calls it directly anymore, see `exec_async()`.
fn exec_blocking(name: &str, data: &[u8]) -> Result<u64, &'static str> {
    // Clear any stale output from a previous run.
    PROC_OUT.lock().clear();

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    CURRENT_PID.store(pid, Ordering::Relaxed);

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

    let result = run_elf(data);

    {
        let mut tab = PROCTAB.lock();
        if let Some(e) = tab.iter_mut().find(|e| e.pid == pid) {
            e.state     = ProcState::Exited;
            e.exit_code = result.unwrap_or(u64::MAX);
        }
    }

    CURRENT_PID.store(0, Ordering::Relaxed);
    result
}

/// Iterate the process table, calling `f` for each entry.
/// Arguments: (pid, name, is_running, exit_code)
pub fn for_each_proc(mut f: impl FnMut(u32, &str, bool, u64)) {
    for e in PROCTAB.lock().iter() {
        f(e.pid, &e.name, e.state == ProcState::Running, e.exit_code);
    }
}

/// Load an ELF64 binary from `data`, run it in a fresh user address space,
/// and return its exit code.
fn run_elf(data: &[u8]) -> Result<u64, &'static str> {
    MMIO_NEXT_VA.store(USER_MMIO_BASE, Ordering::Relaxed);
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

    // Switch to user PML4 and enter ring 3.
    // Timer remains unmasked — the timer ISR saves the full interrupt frame and
    // can preempt ring-3 via context_switch; iretq in the ISR resumes ring-3.
    let orig_cr3 = read_cr3();
    unsafe {
        write_cr3(pml4);
        USER_RUNNING = true;
        enter_ring3(loaded.entry, USER_STACK_TOP);
        // Returns here after do_exit longjmp
    }
    unsafe { write_cr3(orig_cr3); }

    // Free user memory
    for p in stack_pages { pmm::free_page(p); }
    for phys in loaded.pages { pmm::free_page(phys); }
    // Note: intermediate page-table pages (PT/PD/PDP) are leaked here.
    // A full cleanup would walk the low-half of pml4.  Acceptable for now.
    pmm::free_page(pml4);

    Ok(unsafe { EXIT_CODE })
}

// Baked-in hello ELF (generated by build.rs from userspace/target/.../hello).
// Empty slice if userspace hasn't been built yet.
include!(concat!(env!("OUT_DIR"), "/hello_elf.rs"));

// Baked-in hwtest ELF (generated by build.rs from userspace/target/.../hwtest).
include!(concat!(env!("OUT_DIR"), "/hwtest_elf.rs"));

// ── Async process execution ───────────────────────────────────────────────────
//
// `exec_blocking()` above is only ever safe to block *something* on if that
// something is a task the scheduler already knows how to switch away from —
// which task_blink (the desktop loop) and kmain's own boot sequence aren't,
// as ordinary function calls. Calling it inline from `task_blink` (as the
// terminal commands used to) would freeze the entire desktop — mouse,
// rendering, every other window — for as long as the ring-3 process runs,
// since `task_blink`'s own call stack would be the one sitting inside
// `enter_ring3` the whole time.
//
// The fix: give the process its own scheduler task (`scheduler::spawn()`)
// instead. `task_blink` keeps running concurrently, round-robining with it
// via ordinary timer preemption — same mechanism that already lets it share
// the CPU with `task_idle`, now just with a third participant whose "task
// body" happens to include a ring 3 excursion. Same single-job-at-a-time
// restriction `net.rs`'s async network commands (`start_ping`/`start_wget`)
// already use, for the same reason: `exec_blocking()`'s process-global state
// (`USER_RUNNING`, `MMIO_NEXT_VA`, `CURRENT_PID`) was never built to have two
// processes in flight at once, and giving it that isn't in scope here.
struct AsyncJob { issuer: usize, name: String, data: Vec<u8> }

/// Set by `exec_async()`, consumed once by `async_task_entry()` when its task
/// first actually runs (which may be a tick or more later — `spawn()` only
/// queues it `Ready`, it doesn't run synchronously).
static PENDING: Mutex<Option<AsyncJob>> = Mutex::new(None);

/// True from `exec_async()` succeeding until the spawned task's exit-code
/// message lands in `ASYNC_DONE` — the single-job-at-a-time guard.
static ASYNC_BUSY: AtomicBool = AtomicBool::new(false);

/// `(issuer, result message)` — set once by `async_task_entry()` when the
/// process exits, polled (and cleared) by `poll_async()`. Same
/// `Option`-as-a-single-shot-mailbox shape `net::poll()` already uses, for
/// the same reason: the caller (`main.rs`'s per-frame loop) needs to catch
/// this exactly once, whichever frame it happens to land on.
static ASYNC_DONE: Mutex<Option<(usize, String)>> = Mutex::new(None);

/// True while a background process is running (or queued to run) — the
/// guard `exec_async()` checks so a second launch can't stomp on
/// `exec_blocking()`'s single-process-at-a-time global state.
pub fn job_in_progress() -> bool { ASYNC_BUSY.load(Ordering::Relaxed) }

/// Launches `data` as a background process without blocking the caller —
/// `issuer` is the terminal window id `poll_async()` should deliver the
/// eventual "exited: N" (or error) message to, same convention
/// `net::start_ping()`/`start_wget()` use. Returns immediately, before the
/// process has necessarily even started running.
pub fn exec_async(issuer: usize, name: &str, data: &[u8]) -> Result<(), &'static str> {
    if job_in_progress() { return Err("a process is already running"); }
    if data.is_empty() { return Err("nothing to run"); }
    ASYNC_BUSY.store(true, Ordering::Relaxed);
    *PENDING.lock() = Some(AsyncJob { issuer, name: String::from(name), data: data.to_vec() });
    scheduler::spawn("user_proc", async_task_entry);
    Ok(())
}

/// The spawned task's entire body: run the queued job to completion, stash
/// the result for `poll_async()`, then exit. Never returns — `entry: fn()
/// -> !` is all `scheduler::spawn()` accepts, so this can't capture
/// anything; the job itself is threaded through the `PENDING` mailbox.
fn async_task_entry() -> ! {
    if let Some(AsyncJob { issuer, name, data }) = PENDING.lock().take() {
        let result = exec_blocking(&name, &data);
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
        *ASYNC_DONE.lock() = Some((issuer, msg));
    }
    ASYNC_BUSY.store(false, Ordering::Relaxed);
    scheduler::exit_current();
}

/// Advance the in-progress background process, if any finished since the
/// last call. Call this once per main-loop frame, same as `net::poll()` —
/// returns `Some((issuer, message))` exactly once, when the process exits;
/// the caller prints `message` into the issuing terminal window.
pub fn poll_async() -> Option<(usize, String)> {
    ASYNC_DONE.lock().take()
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
