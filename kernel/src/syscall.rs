//! SYSCALL/SYSRET gate and syscall dispatcher.
//!
//! Calling convention (Linux-compatible):
//!   RAX = syscall number
//!   RDI, RSI, RDX, R10, R8, R9 = arguments 1–6  (note: R10 not RCX for arg4)
//!   Return value in RAX; negative values are errors (-errno)
//!
//! Supported syscalls:
//!   1  = write(fd, buf, len)  — writes bytes to serial
//!   60 = exit(code)           — spins/halts (no processes yet)

use core::arch::asm;
use crate::{gdt, pmm, serial, vmm};

// MSR addresses
const MSR_EFER:           u32 = 0xC000_0080;
const MSR_STAR:           u32 = 0xC000_0081;
const MSR_LSTAR:          u32 = 0xC000_0082;
const MSR_SFMASK:         u32 = 0xC000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

// Syscall numbers (Linux x86-64 ABI)
pub const SYS_READ:   u64 = 0;
pub const SYS_WRITE:  u64 = 1;
pub const SYS_EXIT:   u64 = 60;
pub const SYS_GETPID: u64 = 39;

// HepOS-specific extensions (no Linux equivalent) — the foundational
// MMIO/port-IO passthrough syscalls a userspace driver process would use.
// Numbered well above the Linux range so they can never collide with a
// syscall number this project later decides to add real Linux-ABI support for.
pub const SYS_MMAP_MMIO: u64 = 500; // (phys_addr, len)         -> user VA (0 = fail)
pub const SYS_PORT_IN:   u64 = 501; // (port, width 1/2/4)      -> value
pub const SYS_PORT_OUT:  u64 = 502; // (port, width 1/2/4, val) -> 0
pub const SYS_WAIT_IRQ:  u64 = 503; // (vector)                 -> 0 once it fires

// ── Phase 1 of the desktop-to-userspace migration (see PLAN.md's "Service
// management"/driver-migration writeups for the 5 hardware drivers already
// moved — these are the foundational syscalls a future userspace *compositor*
// would need, none of which existed before: anonymous memory beyond the tiny
// static per-process heap, and access to input/filesystem/process-control
// that today only kernel-internal code can reach). Proven independently with
// small `userspace/` test programs before anything real is built on them.
pub const SYS_MMAP_ANON:   u64 = 504; // (len)      -> user VA (0 = fail)
pub const SYS_INPUT_STATE: u64 = 505; // (buf_ptr)   -> key_count written (see `InputStateOut`)

// Phase 1, item 3: async submit/poll wrappers around the existing HepFS
// functions (`kernel/src/hepfs.rs`) — **not synchronous calls**, despite
// looking like ordinary one-shot syscalls at a glance. See the "HepFS
// syscalls" doc comment further down for why: the actual disk I/O routes
// through `nvmed`'s mailbox, and waiting for that from inside a syscall
// (interrupts disabled the whole time) either hangs `nvmed` out of ever
// running or — what was actually observed — spins the full ~500M-iteration
// mailbox-wait budget before giving up, then trips the same `assert!()`
// every unrecoverable disk error already does, panicking the kernel on a
// scheduler/interrupt mismatch that isn't a real hardware failure. These 4
// syscalls just *submit* a request (returning `0` if queued, or a negative
// -errno like `EAGAIN`/`EBUSY` if one's already outstanding); `SYS_FS_POLL`
// (declared further down, next to its implementation) reports the result
// once `fs_service()` — called from `task_blink`'s own interrupts-enabled
// context, same as `net::poll()`/`hda::poll()` — actually finishes it.
pub const SYS_FS_LIST_DIR:   u64 = 506; // (path_ptr, path_len, _, _) -> 0 (queued) or -errno; see SYS_FS_POLL
pub const SYS_FS_READ_FILE:  u64 = 507; // (path_ptr, path_len, _, _) -> 0 (queued) or -errno; see SYS_FS_POLL
pub const SYS_FS_WRITE_FILE: u64 = 508; // (path_ptr, path_len, data_ptr, data_len) -> 0 (queued) or -errno; see SYS_FS_POLL
pub const SYS_FS_CREATE:     u64 = 509; // (path_ptr, path_len, is_dir) -> 0 (queued) or -errno; see SYS_FS_POLL

const ENOSYS: i64 = -38;
const EBADF:  i64 = -9;
const EPERM:  i64 = -1;

// ── MMIO/port-I/O capability allowlist ────────────────────────────────────────
//
// Previously `sys_mmap_mmio`/`sys_port_in`/`sys_port_out` let any process
// touch any physical address or I/O port at all — fine for a single
// proof-of-concept program (`userspace/hwtest`), not once more than one
// process can reach these. This is a real (if intentionally minimal) fix:
// every request is checked against a fixed allowlist before it's allowed to
// proceed, and anything outside it is refused instead of silently granted.
//
// **Scoped simplification, not hidden:** these ranges are a hardcoded table,
// not per-process *granted* capabilities (no process manifest/registration
// exists to hand out narrower or wider access to different processes) — they
// cover exactly what this project's one real userspace client
// (`userspace/hwtest`) actually needs: the RTC index/data ports (`0x70`/
// `0x71`, read via `SYS_PORT_IN`) and the Local APIC's 4 KB MMIO page
// (`0xFEE00000`, mapped via `SYS_MMAP_MMIO`). A real capability system would
// let each process declare (or be granted) its own ranges instead of every
// process sharing one global table — worth building before a second
// concurrently-untrusted userspace driver ever needs *different* hardware
// than this one does.
const ALLOWED_PORTS: &[(u16, u16)] = &[
    (0x70, 0x71), // RTC index/data
];
const ALLOWED_MMIO: &[(u64, u64)] = &[
    (0xFEE0_0000, 0xFEE0_1000), // Local APIC, 4 KB
];

// ── Dynamic (runtime-granted) allowlist entries ───────────────────────────────
//
// The fixed tables above cover hardware whose address is a compile-time
// constant (RTC ports, the Local APIC's architectural physical address).
// RTL8139's I/O base (a PCI BAR) and its DMA buffer physical addresses are
// only known once `rtl8139::init()` actually discovers/allocates them at
// boot — there's no way to hardcode those into a `const` table. Rather than
// widen the static allowlist into something unbounded (which would defeat
// its whole point), the kernel *grants* exactly the ranges a specific driver
// needs, once, right after discovering them, before spawning that driver's
// userspace process. Still not per-process scoped (any process can use any
// granted range, same caveat as the static table above) — just extended to
// cover runtime-discovered hardware too.
static DYNAMIC_PORTS: spin::Mutex<alloc::vec::Vec<(u16, u16)>> = spin::Mutex::new(alloc::vec::Vec::new());
static DYNAMIC_MMIO:  spin::Mutex<alloc::vec::Vec<(u64, u64)>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Grant every process access to I/O ports `lo..=hi` — call once, right
/// after discovering a runtime-only port range (e.g. a PCI BAR), before
/// spawning whatever userspace driver process needs it.
pub fn grant_port_range(lo: u16, hi: u16) {
    DYNAMIC_PORTS.lock().push((lo, hi));
}

/// Grant every process access to the physical range `[phys, phys+len)` via
/// `SYS_MMAP_MMIO` — same idea as `grant_port_range()`, for a
/// runtime-discovered physical address (e.g. a DMA buffer `pmm::alloc_page()`
/// just returned).
pub fn grant_mmio_range(phys: u64, len: u64) {
    DYNAMIC_MMIO.lock().push((phys, phys + len));
}

fn port_allowed(port: u16) -> bool {
    ALLOWED_PORTS.iter().any(|&(lo, hi)| port >= lo && port <= hi)
        || DYNAMIC_PORTS.lock().iter().any(|&(lo, hi)| port >= lo && port <= hi)
}

/// `[phys, phys+len)` must fall entirely within one allowed range — no
/// partial overlap lets a request straddle into disallowed territory.
fn mmio_allowed(phys: u64, len: u64) -> bool {
    let Some(end) = phys.checked_add(len) else { return false };
    ALLOWED_MMIO.iter().any(|&(lo, hi)| phys >= lo && end <= hi)
        || DYNAMIC_MMIO.lock().iter().any(|&(lo, hi)| phys >= lo && end <= hi)
}

// Kernel stack for syscall handling (16 KB, 4 pages)
const KSTACK_PAGES: usize = 4;
const KSTACK_SIZE:  usize = KSTACK_PAGES * 4096;

// ── Per-CPU scratch data (accessed via GS segment after SWAPGS) ───────────────
//
// Layout is fixed — the offsets (0 and 8) are hard-coded in the asm stub.
#[repr(C)]
struct PercpuData {
    kernel_stack: u64,  // offset 0 — kernel RSP on syscall entry
    user_rsp:     u64,  // offset 8 — saved user RSP during syscall
}

static mut PERCPU: PercpuData = PercpuData { kernel_stack: 0, user_rsp: 0 };

/// Repoint the SYSCALL-entry kernel stack at `top` — called by
/// `scheduler.rs` on every task switch so whichever task is "current"
/// handles its own syscalls on its own dedicated stack, not a single shared
/// buffer every process used to fight over (see `scheduler::Task::_kstack`'s
/// doc comment for the real bug this fixes).
pub fn set_kernel_stack(top: u64) {
    unsafe { PERCPU.kernel_stack = top; }
}

/// Read/write the single shared `user_rsp` scratch slot (`gs:[8]`) —
/// `syscall_entry`'s asm stashes the calling task's user-mode RSP here on
/// entry and restores it from here on exit (`sysretq`). Exposed so
/// `scheduler::block_on_irq()` can save/restore it around a context switch
/// that happens *mid-syscall* — see that function's doc comment for the real
/// cross-task corruption bug this closes (the same shape as the `swapgs`
/// fix right above it: this slot is per-CPU, not per-task, so a second
/// task's syscall entry overwrites it while the first task is still
/// "mid-syscall" but blocked, corrupting the first task's RSP once it
/// resumes and its own `sysretq` restores the wrong value).
pub fn get_user_rsp() -> u64 { unsafe { PERCPU.user_rsp } }
pub fn set_user_rsp(v: u64) { unsafe { PERCPU.user_rsp = v; } }

// ── MSR helpers ───────────────────────────────────────────────────────────────

unsafe fn wrmsr(msr: u32, val: u64) {
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") val as u32,
        in("edx") (val >> 32) as u32,
        options(nostack, nomem),
    );
}

/// Public wrapper — used by terminal's `syscallinfo` command for MSR readback.
pub unsafe fn rdmsr_pub(msr: u32) -> u64 { rdmsr(msr) }

unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    asm!(
        "rdmsr",
        in("ecx")  msr,
        out("eax") lo,
        out("edx") hi,
        options(nostack, nomem),
    );
    lo as u64 | ((hi as u64) << 32)
}

// ── Public init ───────────────────────────────────────────────────────────────

pub fn init() {
    unsafe {
        // Allocate a kernel stack for syscall handling
        let phys = pmm::alloc_contiguous(KSTACK_PAGES)
            .expect("syscall: out of memory for kernel stack");
        let stack_top = vmm::phys_to_virt(phys) as u64 + KSTACK_SIZE as u64;

        PERCPU.kernel_stack = stack_top;

        // IA32_KERNEL_GS_BASE — the "shadow" GS base that SWAPGS loads into GS.
        // On SYSCALL entry we do SWAPGS → GS = &PERCPU, then access percpu via gs:[0]/gs:[8].
        // On return we do SWAPGS again → GS restored to whatever userspace had (0 for now).
        wrmsr(MSR_KERNEL_GS_BASE, core::ptr::addr_of!(PERCPU) as u64);

        // Enable SYSCALL/SYSRET in EFER (bit 0 = SCE)
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | 1);

        // STAR layout:
        //   bits[47:32] = 0x0008  SYSCALL  → CS=0x08 (kcode), SS=0x10 (kdata)
        //   bits[63:48] = 0x0010  SYSRETQ  → CS=0x10+16=0x20|3 (ucode), SS=0x10+8=0x18|3 (udata)
        wrmsr(MSR_STAR, (0x0010_u64 << 48) | (0x0008_u64 << 32));

        // LSTAR — 64-bit SYSCALL entry point
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);

        // SFMASK — RFLAGS bits to clear on SYSCALL entry
        //   bit 9 = IF (disable interrupts), bit 8 = TF (no single-step in kernel)
        wrmsr(MSR_SFMASK, 0x300);

        // RSP0 in TSS — used when a hardware interrupt fires while in ring 3
        gdt::set_tss_rsp0(stack_top);
    }

    serial::print("Syscall gate ready\n");
}

// ── Entry stub ────────────────────────────────────────────────────────────────
//
// On SYSCALL entry from ring 3:
//   RCX  = saved user RIP   (written by SYSCALL instruction)
//   R11  = saved user RFLAGS (written by SYSCALL instruction)
//   RSP  = user stack        (untrusted, don't touch)
//   RAX  = syscall number
//   RDI, RSI, RDX, R10, R8, R9 = args 1–6
//   IF = 0 (cleared by SFMASK), TF = 0

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // 1. Switch GS from user GS to kernel per-CPU data
        "swapgs",

        // 2. Save user RSP; switch to kernel stack.
        //    GS offsets: [0]=kernel_stack, [8]=user_rsp  (match PercpuData layout)
        "mov gs:[8], rsp",
        "mov rsp, gs:[0]",

        // 3. Callee-saved + return-path registers onto kernel stack
        "push rcx",     // user RIP  (restored into RCX by SYSRETQ)
        "push r11",     // user RFLAGS (restored into R11 by SYSRETQ)
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // 4. Re-arrange registers so dispatcher receives (num, a1, a2, a3, a4, a5)
        //    in SystemV order (rdi, rsi, rdx, rcx, r8, r9).
        //    arg4 comes from R10 (SYSCALL clobbered RCX with user RIP).
        "mov r9,  r8",
        "mov r8,  r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",   // syscall number

        // 5. Call the Rust dispatcher
        "call {dispatch}",
        // Return value in RAX

        // 6. Restore callee-saved and return-path registers
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "pop r11",          // → user RFLAGS (read by SYSRETQ)
        "pop rcx",          // → user RIP    (read by SYSRETQ)

        // 7. Restore user RSP; restore user GS
        "mov rsp, gs:[8]",
        "swapgs",

        // 8. Return to 64-bit userspace
        "sysretq",

        dispatch = sym syscall_dispatch,
    );
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

// Called with SystemV AMD64 ABI: (num, a1, a2, a3, a4, a5) → return value.
// `num` is the syscall number (was RAX); a1..a5 are user args (a6 = old R9 is dropped).
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64, _a5: u64) -> u64 {
    match num {
        SYS_WRITE      => sys_write(a1, a2, a3),
        SYS_EXIT       => sys_exit(a1),
        SYS_GETPID     => crate::process::current_pid() as u64,
        SYS_MMAP_MMIO  => sys_mmap_mmio(a1, a2),
        SYS_PORT_IN    => sys_port_in(a1, a2),
        SYS_PORT_OUT   => sys_port_out(a1, a2, a3),
        SYS_WAIT_IRQ   => sys_wait_irq(a1),
        SYS_MMAP_ANON  => crate::process::map_anon_for_user(a1),
        SYS_INPUT_STATE   => sys_input_state(a1),
        SYS_FS_LIST_DIR   => sys_fs_list_dir(a1, a2, a3, a4),
        SYS_FS_READ_FILE  => sys_fs_read_file(a1, a2, a3, a4),
        SYS_FS_WRITE_FILE => sys_fs_write_file(a1, a2, a3, a4),
        SYS_FS_CREATE     => sys_fs_create(a1, a2, a3),
        SYS_FS_POLL       => sys_fs_poll(a1, a2),
        SYS_SERVICE_CTL   => sys_service_ctl(a1, a2, a3, a4),
        SYS_SERVICE_POLL  => sys_service_poll(),
        SYS_SPAWN_BYTES   => sys_spawn_bytes(a1, a2, a3),
        _              => ENOSYS as u64,
    }
}

// ── Syscall implementations ───────────────────────────────────────────────────

/// write(fd, buf, len) — writes bytes to COM1 serial.
/// fd 1 = stdout, fd 2 = stderr; others return EBADF.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    if fd != 1 && fd != 2 { return EBADF as u64; }
    let count = (len as usize).min(4096);
    if count == 0 { return 0; }

    // NOTE: buf is a userspace virtual address. Without separate page tables
    // all kernel and "user" addresses are in the same address space for now,
    // so this direct dereference works. Validate properly once ring-3 pages land.
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    for &b in bytes {
        serial::write_byte(b);
    }
    // Also buffer output so the terminal can display it after exec returns.
    crate::process::proc_write(bytes);
    count as u64
}

/// exit(code) — terminates the running user process (if any) via longjmp,
/// otherwise returns -ENOSYS.
fn sys_exit(code: u64) -> u64 {
    if crate::process::is_user_running() {
        unsafe { crate::process::do_exit(code) }
        // do_exit is -> !, so this branch never reaches here; Rust knows it.
    } else {
        serial::print("sys_exit: no process running\n");
        ENOSYS as u64
    }
}

/// mmap_mmio(phys_addr, len) — maps a physical MMIO region into the calling
/// process's own page tables and returns the user virtual address (0 = fail:
/// no process running, a too-large/zero request, or `phys..phys+len` isn't
/// entirely covered by `ALLOWED_MMIO`). Once mapped, the caller reads/writes
/// it directly with no further syscalls — the actual point of "passthrough",
/// since a real driver needs fast polled MMIO access.
fn sys_mmap_mmio(phys: u64, len: u64) -> u64 {
    if !mmio_allowed(phys, len) { return 0; }
    crate::process::map_mmio_for_user(phys, len)
}

/// port_in(port, width) — privileged IN, done here (not in ring 3) because
/// ring-3 IN/OUT needs IOPL=3 or a TSS I/O bitmap, neither of which this
/// project sets up; the syscall boundary *is* the permission check for now.
/// width must be 1, 2, or 4 (bytes); `port` must be in `ALLOWED_PORTS`;
/// anything else returns -ENOSYS/-EPERM respectively.
fn sys_port_in(port: u64, width: u64) -> u64 {
    let port = port as u16;
    if !port_allowed(port) { return EPERM as u64; }
    unsafe {
        match width {
            1 => { let v: u8;  asm!("in al, dx",  out("al")  v, in("dx") port, options(nomem, nostack)); v as u64 }
            2 => { let v: u16; asm!("in ax, dx",  out("ax")  v, in("dx") port, options(nomem, nostack)); v as u64 }
            4 => { let v: u32; asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack)); v as u64 }
            _ => ENOSYS as u64,
        }
    }
}

/// port_out(port, width, value) — privileged OUT; see `sys_port_in` for why
/// this runs in the kernel rather than granting ring-3 IOPL, and for the
/// same `ALLOWED_PORTS` check.
fn sys_port_out(port: u64, width: u64, val: u64) -> u64 {
    let port = port as u16;
    if !port_allowed(port) { return EPERM as u64; }
    unsafe {
        match width {
            1 => { asm!("out dx, al",  in("dx") port, in("al")  val as u8,  options(nomem, nostack)); 0 }
            2 => { asm!("out dx, ax",  in("dx") port, in("ax")  val as u16, options(nomem, nostack)); 0 }
            4 => { asm!("out dx, eax", in("dx") port, in("eax") val as u32, options(nomem, nostack)); 0 }
            _ => ENOSYS as u64,
        }
    }
}

/// wait_irq(vector) — blocks the calling process until interrupt `vector`
/// fires, instead of busy-polling for it. Backed by
/// `scheduler::block_on_irq()`/`wake_irq_waiters()`: this blocks the
/// scheduler task hosting the calling ring-3 process the same way
/// `sleep_ms()` already blocks a task from inside a syscall, just woken by
/// an interrupt firing instead of a deadline passing. Always returns 0 —
/// there's no failure mode (any `u8` vector value is a legal thing to wait
/// on, even one nothing ever signals; if there's no other task ready to run
/// meanwhile, `block_on_irq()` falls straight through without blocking at
/// all instead, same as `sleep_ms()`'s own documented fallback).
fn sys_wait_irq(vector: u64) -> u64 {
    crate::scheduler::block_on_irq(vector as u8);
    0
}

/// Fixed-layout struct `SYS_INPUT_STATE` writes into the caller's buffer.
/// **Must stay byte-for-byte identical to the copy in
/// `userspace/inputtest/src/main.rs`** (and any future userspace consumer)
/// — no shared crate between kernel and userspace to enforce that, same
/// constraint every driver mailbox already has.
#[repr(C)]
struct InputStateOut {
    /// 1 if `mouse_x`/`mouse_y`/`mouse_buttons` were actually refreshed this
    /// call, 0 if `mouse::MOUSE`'s lock was momentarily busy (see
    /// `sys_input_state()`'s doc comment) — callers should keep their own
    /// last-known values rather than treat `mouse_x`/`y`/`buttons` as valid
    /// when this is 0, since they're left at 0 in that case, not "unchanged".
    mouse_valid: u32,
    mouse_x: i32,
    mouse_y: i32,
    mouse_buttons: u32,
    /// How many of `keys` below are actually populated this call (0..=16).
    key_count: u32,
    /// Pending keyboard chars already decoded to ASCII by `ps2.rs`, drained
    /// from its existing ring buffer (`ps2::read_char()`) — oldest first,
    /// same order they were typed in.
    keys: [u8; 16],
}

/// input_state(buf_ptr) — snapshot of current input for a userspace
/// process: mouse `x`/`y`/`buttons` (already tracked in `mouse::MOUSE`,
/// read fresh every call — mouse input has no "queue," just current state,
/// same as the kernel's own render loop reads it) and up to 16 pending
/// keyboard chars drained from `ps2::read_char()`'s existing ring buffer.
/// No new kernel-side state at all — purely a syscall-reachable wrapper
/// around what the in-kernel desktop already reads every frame. Returns the
/// number of keyboard chars written (0 if none were pending), or 0 if
/// `buf_ptr` is null.
///
/// **A real deadlock found via testing, not by inspection**: this was
/// originally written with plain `.lock()` calls on `mouse::MOUSE` and
/// `ps2::KEYBUF` — both spin locks also briefly held by `task_blink`'s own
/// in-kernel code (`mouse::poll()`, `ps2::poll()`). Every syscall handler
/// runs with interrupts disabled (SFMASK clears IF on `SYSCALL` entry), so
/// if the timer interrupt happened to preempt `task_blink` mid-lock-hold
/// (entirely possible — nothing brackets those brief critical sections
/// with `cli`), and the task that got scheduled next called this syscall,
/// `.lock()` would spin forever waiting for a lock only a *future timer
/// interrupt* could ever free — except that interrupt can never fire while
/// we're stuck spinning inside this syscall with IF=0. A genuine deadlock,
/// confirmed by an isolation test: a userspace loop calling only
/// `SYS_WAIT_IRQ` (no `SYS_INPUT_STATE`) ran thousands of iterations
/// cleanly under the same 6-concurrent-task load that made this syscall
/// hang within the first ~10-200 calls. Every prior syscall never had this
/// problem because none of them reached into a lock `task_blink` also
/// holds directly from a genuinely different, concurrently-scheduled task
/// — this is the first one that does. Fixed with `try_lock()`/
/// `ps2::try_read_char()`: if either lock is momentarily busy, this call
/// just reports "no new data this time" instead of risking the deadlock —
/// input state is inherently a snapshot, so a rare skipped poll is
/// harmless, unlike spinning forever.
fn sys_input_state(buf: u64) -> u64 {
    if buf == 0 { return 0; }
    let (mouse_valid, mx, my, btn) = match crate::mouse::MOUSE.try_lock() {
        Some(m) => (1u32, m.x, m.y, m.buttons),
        None => (0u32, 0, 0, 0), // lock momentarily busy — caller keeps its own last value
    };
    let mut keys = [0u8; 16];
    let mut key_count: usize = 0;
    while key_count < keys.len() {
        match crate::ps2::try_read_char() {
            Some(c) => { keys[key_count] = c as u8; key_count += 1; }
            None => break,
        }
    }
    unsafe {
        let out = buf as *mut InputStateOut;
        core::ptr::write_volatile(out, InputStateOut {
            mouse_valid,
            mouse_x: mx,
            mouse_y: my,
            mouse_buttons: btn as u32,
            key_count: key_count as u32,
            keys,
        });
    }
    key_count as u64
}

// ── HepFS syscalls (SYS_FS_LIST_DIR/READ_FILE/WRITE_FILE/CREATE/POLL) ────────
//
// **Async submit/poll, not a synchronous call — a real redesign forced by
// testing, not a choice made up front.** The first version of these made
// the HepFS call directly, inline, using `nvme::CONTROLLER.try_lock()` the
// same way `SYS_INPUT_STATE` fixed its own deadlock. That's necessary but
// not sufficient here: once handed off, HepFS's actual disk I/O routes
// through `nvmed`'s mailbox (`nvme.rs`), and *waiting* for that mailbox is
// a tight, non-yielding spin loop (`read_blocks()`/`write_blocks()`) that
// only ever finishes because `nvmed` gets a scheduling turn *while task_blink
// is spinning with interrupts enabled*. A syscall handler runs with
// interrupts disabled for its entire duration (SFMASK clears IF on `SYSCALL`
// entry) — so `nvmed` can *never* be scheduled while a syscall is doing that
// same spin, and the wait always runs its full ~500 million iterations
// before giving up and returning failure — which then hit the same
// `assert!()` every disk I/O failure already does, **panicking the whole
// kernel** on what was actually just a syscall/scheduler impedance
// mismatch, not a real disk error. Confirmed by testing: it wasn't a true
// hang (an earlier hypothesis), it was a very slow, deterministic march to
// a kernel panic.
//
// Fixed the same way `net.rs`'s `ping`/`wget` were: made asynchronous.
// `SYS_FS_LIST_DIR`/`READ_FILE`/`WRITE_FILE`/`CREATE` now just *submit* a
// job (`FS_JOB`) and return immediately; `fs_service()` (called once per
// frame from `task_blink`, the same place `net::poll()`/`hda::poll()`
// already run — interrupts enabled, safe to actually wait on `nvmed`) does
// the real HepFS work; a new `SYS_FS_POLL` syscall lets userspace check for
// completion. Every one of these still only ever touches `FS_JOB`/
// `nvme::CONTROLLER` via `try_lock()` — submitting/polling are quick,
// non-blocking operations in their own right, and could *themselves* still
// hit `task_blink`'s `SYS_INPUT_STATE`-class lock contention if not careful.
pub const SYS_FS_POLL: u64 = 510; // (out_ptr, out_cap) -> see `sys_fs_poll()`

const EAGAIN:  i64 = -11;
const EBUSY:   i64 = -16;
const ENOENT:  i64 = -2;
const ENOTDIR: i64 = -20;
const EISDIR:  i64 = -21;

/// Read a bounded UTF-8 path string out of user memory. `None` on a null
/// pointer, zero length, an over-long request (paths this OS deals with are
/// always short), or invalid UTF-8.
fn read_user_path(ptr: u64, len: u64) -> Option<alloc::string::String> {
    if ptr == 0 || len == 0 || len > 256 { return None; }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    core::str::from_utf8(bytes).ok().map(alloc::string::String::from)
}

/// One directory entry as written into a completed `ListDir` job's result
/// buffer, then copied out verbatim by `SYS_FS_POLL`. Fixed-size so the
/// caller can index straight into an array of these. **Must stay
/// byte-for-byte identical to any userspace consumer's copy.**
#[repr(C)]
struct DirEntryOut {
    ino: u32,
    /// How many bytes of `name` are actually the entry's name (it's
    /// truncated, not null-terminated, if longer than 60 bytes — no HepFS
    /// name is anywhere near that long today).
    name_len: u32,
    name: [u8; 60],
}

enum FsOp {
    ListDir,
    ReadFile,
    WriteFile { data: alloc::vec::Vec<u8> },
    Create { is_dir: bool },
}

/// One in-flight (or just-finished, not yet polled) HepFS request. Only one
/// system-wide at a time — same restriction `net.rs`'s own async jobs
/// already have, for the same reason (the state a synchronous version would
/// have used was never built for two in flight together).
struct FsJob {
    op: FsOp,
    path: alloc::string::String,
    done: bool,
    /// Once `done`: the same -errno/byte-or-entry-count convention the old
    /// synchronous versions returned.
    result: i64,
    /// Once `done`: raw result bytes — a `DirEntryOut` array for `ListDir`,
    /// or file contents for `ReadFile`. Empty for `WriteFile`/`Create`.
    out_data: alloc::vec::Vec<u8>,
}

static FS_JOB: spin::Mutex<Option<FsJob>> = spin::Mutex::new(None);

/// Submit a new job if none is currently in flight/unpolled. Returns `Ok(())`
/// if queued, or `Err(EAGAIN)` if `FS_JOB`'s lock is momentarily busy (same
/// deadlock-avoidance reasoning as `SYS_INPUT_STATE` — this lock is also
/// touched from `fs_service()`, called from `task_blink`'s own context) or a
/// job is already outstanding.
fn submit_fs_job(op: FsOp, path: alloc::string::String) -> Result<(), i64> {
    let Some(mut guard) = FS_JOB.try_lock() else { return Err(EAGAIN); };
    if guard.is_some() { return Err(EBUSY); }
    *guard = Some(FsJob { op, path, done: false, result: 0, out_data: alloc::vec::Vec::new() });
    Ok(())
}

fn sys_fs_list_dir(path_ptr: u64, path_len: u64, _out_ptr: u64, _out_cap: u64) -> u64 {
    let Some(path) = read_user_path(path_ptr, path_len) else { return EAGAIN as u64; };
    match submit_fs_job(FsOp::ListDir, path) {
        Ok(()) => 0,
        Err(e) => e as u64,
    }
}

fn sys_fs_read_file(path_ptr: u64, path_len: u64, _out_ptr: u64, _out_cap: u64) -> u64 {
    let Some(path) = read_user_path(path_ptr, path_len) else { return EAGAIN as u64; };
    match submit_fs_job(FsOp::ReadFile, path) {
        Ok(()) => 0,
        Err(e) => e as u64,
    }
}

fn sys_fs_write_file(path_ptr: u64, path_len: u64, data_ptr: u64, data_len: u64) -> u64 {
    let Some(path) = read_user_path(path_ptr, path_len) else { return EAGAIN as u64; };
    if data_ptr == 0 && data_len != 0 { return EAGAIN as u64; }
    // Copy the caller's data into kernel-owned memory *now* — `fs_service()`
    // runs later, possibly several frames from now, by which point the
    // calling process's own buffer could have changed or (in a future with
    // real process teardown timing) gone away entirely.
    let data = unsafe { core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize) }.to_vec();
    match submit_fs_job(FsOp::WriteFile { data }, path) {
        Ok(()) => 0,
        Err(e) => e as u64,
    }
}

fn sys_fs_create(path_ptr: u64, path_len: u64, is_dir: u64) -> u64 {
    let Some(path) = read_user_path(path_ptr, path_len) else { return EAGAIN as u64; };
    match submit_fs_job(FsOp::Create { is_dir: is_dir != 0 }, path) {
        Ok(()) => 0,
        Err(e) => e as u64,
    }
}

/// poll(out_ptr, out_cap) — check whether the outstanding job (if any) has
/// finished. Returns:
/// - `EAGAIN` (-11) if `FS_JOB`'s lock is momentarily busy — retry.
/// - `EBUSY` (-16) if a job is queued but `fs_service()` hasn't finished it
///   yet — retry after `SYS_WAIT_IRQ`, same as any pending async job.
/// - `ENOENT` (-2, reused here for "no job at all") if nothing was ever
///   submitted or the last result was already collected.
/// - Otherwise: the job's own real result (a -errno, or a byte/entry
///   count) — same convention the old synchronous calls used — with up to
///   `out_cap` bytes of `out_data` copied into the caller's buffer. Clears
///   the completed job either way, so a second poll after this one reports
///   "no job" until something new is submitted.
fn sys_fs_poll(out_ptr: u64, out_cap: u64) -> u64 {
    let Some(mut guard) = FS_JOB.try_lock() else { return EAGAIN as u64; };
    let Some(job) = guard.as_ref() else { return ENOENT as u64; };
    if !job.done { return EBUSY as u64; }
    let result = job.result;
    if out_ptr != 0 && out_cap != 0 {
        let n = job.out_data.len().min(out_cap as usize);
        unsafe { core::ptr::copy_nonoverlapping(job.out_data.as_ptr(), out_ptr as *mut u8, n); }
    }
    *guard = None;
    result as u64
}

/// True while either job queue has unfinished work — lets `task_svc_worker`
/// (see `main.rs`) tell "actively driving a job forward" apart from "nothing
/// to do right now", so it only busy-spins while real work is in flight and
/// sleeps the rest of the time instead of fighting `task_blink` for every
/// single timeslice.
pub fn has_pending_job() -> bool {
    let fs_pending = FS_JOB.lock().as_ref().map(|j| !j.done).unwrap_or(false);
    let svc_pending = SVC_JOB.lock().as_ref().map(|j| !j.done).unwrap_or(false);
    fs_pending || svc_pending
}

/// Advance the in-progress HepFS job, if any — call this once per
/// `task_blink` frame, same as `net::poll()`/`hda::poll()`. Runs with
/// interrupts enabled (this is ordinary kernel code, not a syscall), so
/// unlike the syscalls above, it's safe to actually let a HepFS call wait
/// on `nvmed`'s mailbox here — see this module's own doc comment for why
/// that distinction is exactly what forced this whole redesign.
///
/// No longer called from `task_blink` itself — moved to its own scheduler
/// task (`task_svc_worker`, `main.rs`) so a slow disk job or driver
/// start/stop can't stall cursor/window rendering. See that task's doc
/// comment for the full reasoning.
pub fn fs_service() {
    let mut guard = FS_JOB.lock();
    let Some(job) = guard.as_mut() else { return; };
    if job.done { return; }

    let Some(mut ctrl_guard) = crate::nvme::CONTROLLER.try_lock() else { return; }; // retry next frame
    let Some(ctrl) = ctrl_guard.as_mut() else {
        job.result = ENOENT as i64;
        job.done = true;
        return;
    };
    let mut dev = crate::hepfs::BlockDev::Nvme(ctrl);

    match &job.op {
        FsOp::ListDir => {
            match crate::hepfs::lookup(&mut dev, &job.path) {
                None => { job.result = ENOENT as i64; }
                Some(dir_ino) => {
                    let (is_dir, _) = crate::hepfs::stat(&mut dev, dir_ino);
                    if !is_dir {
                        job.result = ENOTDIR as i64;
                    } else {
                        let entries = crate::hepfs::list_dir(&mut dev, dir_ino);
                        let mut out = alloc::vec::Vec::with_capacity(entries.len() * core::mem::size_of::<DirEntryOut>());
                        for (ino, name) in &entries {
                            let mut name_buf = [0u8; 60];
                            let bytes = name.as_bytes();
                            let copy_len = bytes.len().min(60);
                            name_buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
                            let entry = DirEntryOut { ino: *ino, name_len: copy_len as u32, name: name_buf };
                            let entry_bytes = unsafe {
                                core::slice::from_raw_parts(&entry as *const DirEntryOut as *const u8, core::mem::size_of::<DirEntryOut>())
                            };
                            out.extend_from_slice(entry_bytes);
                        }
                        job.out_data = out;
                        job.result = entries.len() as i64;
                    }
                }
            }
        }
        FsOp::ReadFile => {
            match crate::hepfs::lookup(&mut dev, &job.path) {
                None => { job.result = ENOENT as i64; }
                Some(ino) => {
                    let (is_dir, _) = crate::hepfs::stat(&mut dev, ino);
                    if is_dir {
                        job.result = EISDIR as i64;
                    } else {
                        let data = crate::hepfs::read_file(&mut dev, ino);
                        job.result = data.len() as i64;
                        job.out_data = data;
                    }
                }
            }
        }
        FsOp::WriteFile { data } => {
            let ino = match crate::hepfs::lookup(&mut dev, &job.path) {
                Some(ino) => {
                    let (is_dir, _) = crate::hepfs::stat(&mut dev, ino);
                    if is_dir { job.result = EISDIR as i64; job.done = true; return; }
                    Some(ino)
                }
                None => {
                    let (parent, name) = split_path(&job.path);
                    match crate::hepfs::lookup(&mut dev, parent) {
                        None => { job.result = ENOENT as i64; None }
                        Some(parent_ino) => Some(crate::hepfs::create_file(&mut dev, parent_ino, name)),
                    }
                }
            };
            if let Some(ino) = ino {
                crate::hepfs::write_file(&mut dev, ino, data);
                job.result = data.len() as i64;
            }
        }
        FsOp::Create { is_dir } => {
            let (parent, name) = split_path(&job.path);
            match crate::hepfs::lookup(&mut dev, parent) {
                None => { job.result = ENOENT as i64; }
                Some(parent_ino) => {
                    if *is_dir { crate::hepfs::create_dir(&mut dev, parent_ino, name); }
                    else { crate::hepfs::create_file(&mut dev, parent_ino, name); }
                    job.result = 0;
                }
            }
        }
    }
    job.done = true;
}

/// Split `"/a/b/c"` into (`"/a/b"`, `"c"`) — the parent directory's own
/// path (fed straight back into `hepfs::lookup()`) and the new entry's bare
/// name. A path with no `/` at all (shouldn't happen for an absolute path,
/// but handled defensively) is treated as directly under root.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

// ── Phase 1, item 4: SYS_SERVICE_CTL / SYS_SERVICE_POLL / SYS_SPAWN_BYTES ────
//
// `SYS_SERVICE_CTL`'s status/enable/disable actions are plain atomic
// reads/stores (`is_enabled()`/`is_running()`/`set_enabled()`) — safe to run
// inline in the syscall, same as `SYS_INPUT_STATE`. Its start/stop actions
// are NOT: every driver's `start_service()`/`stop_service()`
// (`rtl8139.rs`/`hda.rs`/`ahci.rs`/`xhci.rs`) spin-waits for `is_running()`
// to flip, which only happens once the scheduler actually runs the
// driver/service task — exactly the `SYS_FS_*`-class bug (interrupts are
// disabled for a syscall's whole duration, so that task can never be
// scheduled, so the wait always exhausts its budget). So start/stop reuse
// the same submit/poll shape as the FS syscalls above: `SYS_SERVICE_CTL`
// only queues the job, `svc_service()` (called from `task_blink`, same as
// `fs_service()`) actually calls into the spinning driver function, and
// `SYS_SERVICE_POLL` retrieves the result.
pub const SYS_SERVICE_CTL:  u64 = 511; // (service_id, action, _, _) -> 0 (queued, start/stop) or immediate result (status/enable/disable); see sys_service_ctl()
pub const SYS_SERVICE_POLL: u64 = 512; // () -> see sys_service_poll()
pub const SYS_SPAWN_BYTES:  u64 = 513; // (data_ptr, data_len, arg) -> 0 on success, -errno on failure; see sys_spawn_bytes()

/// `service_id`: 0=rtl8139d, 1=hdad, 2=ahcid, 3=xhcid — same order as
/// `terminal.rs`'s `SERVICE_TABLE`. nvmed isn't included: it's core storage,
/// never user-toggleable (see `nvme.rs`'s module doc comment).
fn service_name(id: u64) -> Option<&'static str> {
    match id { 0 => Some("rtl8139d"), 1 => Some("hdad"), 2 => Some("ahcid"), 3 => Some("xhcid"), _ => None }
}

fn service_is_enabled(name: &str) -> bool {
    match name {
        "rtl8139d" => crate::rtl8139::is_enabled(),
        "hdad"     => crate::hda::is_enabled(),
        "ahcid"    => crate::ahci::is_enabled(),
        "xhcid"    => crate::xhci::is_enabled(),
        _ => false,
    }
}

fn service_is_running(name: &str) -> bool {
    match name {
        "rtl8139d" => crate::rtl8139::is_running(),
        "hdad"     => crate::hda::is_running(),
        "ahcid"    => crate::ahci::is_running(),
        "xhcid"    => crate::xhci::is_running(),
        _ => false,
    }
}

fn service_set_enabled(name: &str, v: bool) {
    match name {
        "rtl8139d" => crate::rtl8139::set_enabled(v),
        "hdad"     => crate::hda::set_enabled(v),
        "ahcid"    => crate::ahci::set_enabled(v),
        "xhcid"    => crate::xhci::set_enabled(v),
        _ => {}
    }
}

/// action: 0=Status, 1=Start, 2=Stop, 3=Enable, 4=Disable.
enum SvcAction { Status, Start, Stop, Enable, Disable }

fn parse_svc_action(a: u64) -> Option<SvcAction> {
    match a {
        0 => Some(SvcAction::Status), 1 => Some(SvcAction::Start), 2 => Some(SvcAction::Stop),
        3 => Some(SvcAction::Enable), 4 => Some(SvcAction::Disable), _ => None,
    }
}

/// One in-flight (or just-finished, not yet polled) start/stop request.
/// Same one-at-a-time restriction as `FsJob`.
struct SvcJob {
    name: &'static str,
    start: bool, // true = start_service(), false = stop_service()
    done: bool,
    result: i64, // 0 on success, -errno on failure
}

static SVC_JOB: spin::Mutex<Option<SvcJob>> = spin::Mutex::new(None);

/// service_ctl(service_id, action) — see the module-level doc comment above
/// for why start/stop are async (submit-only here; poll via
/// `SYS_SERVICE_POLL`) while status/enable/disable are answered immediately.
fn sys_service_ctl(service_id: u64, action: u64, _a3: u64, _a4: u64) -> u64 {
    let Some(name) = service_name(service_id) else { return ENOENT as u64; };
    let Some(action) = parse_svc_action(action) else { return ENOSYS as u64; };
    match action {
        SvcAction::Status => {
            ((service_is_enabled(name) as u64) << 1) | (service_is_running(name) as u64)
        }
        SvcAction::Enable  => { service_set_enabled(name, true);  0 }
        SvcAction::Disable => { service_set_enabled(name, false); 0 }
        SvcAction::Start | SvcAction::Stop => {
            let Some(mut guard) = SVC_JOB.try_lock() else { return EAGAIN as u64; };
            if guard.is_some() { return EBUSY as u64; }
            *guard = Some(SvcJob { name, start: matches!(action, SvcAction::Start), done: false, result: 0 });
            0
        }
    }
}

/// service_poll() — same `EAGAIN`/`EBUSY`/`ENOENT`/result convention as
/// `sys_fs_poll()`, just with no output buffer (start/stop have no data to
/// return beyond success/failure).
fn sys_service_poll() -> u64 {
    let Some(mut guard) = SVC_JOB.try_lock() else { return EAGAIN as u64; };
    let Some(job) = guard.as_ref() else { return ENOENT as u64; };
    if !job.done { return EBUSY as u64; }
    let result = job.result;
    *guard = None;
    result as u64
}

/// Advance the in-progress service start/stop, if any — call once per
/// `task_svc_worker` iteration (see `main.rs`), same as `fs_service()`.
/// Interrupts are enabled here, so it's safe to let `start_service()`/
/// `stop_service()` actually spin waiting for the driver task to be
/// scheduled. No longer called from `task_blink` — see `task_svc_worker`'s
/// doc comment.
pub fn svc_service() {
    let mut guard = SVC_JOB.lock();
    let Some(job) = guard.as_mut() else { return; };
    if job.done { return; }
    let result = if job.start {
        match job.name {
            "rtl8139d" => crate::rtl8139::start_service(),
            "hdad"     => crate::hda::start_service(),
            "ahcid"    => crate::ahci::start_service(),
            "xhcid"    => crate::xhci::start_service(),
            _ => Err("unknown service"),
        }
    } else {
        match job.name {
            "rtl8139d" => crate::rtl8139::stop_service(),
            "hdad"     => crate::hda::stop_service(),
            "ahcid"    => crate::ahci::stop_service(),
            "xhcid"    => crate::xhci::stop_service(),
            _ => Err("unknown service"),
        }
    };
    job.result = match result { Ok(()) => 0, Err(_) => -1 };
    job.done = true;
}

/// spawn_bytes(data_ptr, data_len, arg) — run an ELF image already sitting in
/// the caller's own memory (e.g. just read via `SYS_FS_READ_FILE`+
/// `SYS_FS_POLL`). A thin wrapper around `process::exec_async_with_arg()`,
/// which is already non-blocking (it only queues a job and calls
/// `scheduler::spawn()` — no spin-wait), so unlike the FS/service syscalls
/// above this one is safe to run synchronously. Deliberately takes bytes,
/// not a path: reading a file is already its own async submit/poll dance
/// (`SYS_FS_READ_FILE`), and chaining a second async operation inside this
/// syscall would need a job type of its own for no real benefit — the
/// caller already has the bytes in hand right after its own read completes.
fn sys_spawn_bytes(data_ptr: u64, data_len: u64, arg: u64) -> u64 {
    if data_ptr == 0 || data_len == 0 { return EAGAIN as u64; }
    let data = unsafe { core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize) };
    match crate::process::exec_async_with_arg(usize::MAX, "userspawn", data, arg) {
        Ok(()) => 0,
        Err(_) => ENOSYS as u64,
    }
}

