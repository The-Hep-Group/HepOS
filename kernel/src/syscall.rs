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
extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, _a4: u64, _a5: u64) -> u64 {
    match num {
        SYS_WRITE      => sys_write(a1, a2, a3),
        SYS_EXIT       => sys_exit(a1),
        SYS_GETPID     => crate::process::current_pid() as u64,
        SYS_MMAP_MMIO  => sys_mmap_mmio(a1, a2),
        SYS_PORT_IN    => sys_port_in(a1, a2),
        SYS_PORT_OUT   => sys_port_out(a1, a2, a3),
        SYS_WAIT_IRQ   => sys_wait_irq(a1),
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

