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
use core::sync::atomic::Ordering;
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

const ENOSYS: i64 = -38;
const EBADF:  i64 = -9;

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
        SYS_WRITE  => sys_write(a1, a2, a3),
        SYS_EXIT   => sys_exit(a1),
        SYS_GETPID => crate::process::CURRENT_PID.load(Ordering::Relaxed) as u64,
        _          => ENOSYS as u64,
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
    if unsafe { crate::process::USER_RUNNING } {
        unsafe { crate::process::do_exit(code) }
        // do_exit is -> !, so this branch never reaches here; Rust knows it.
    } else {
        serial::print("sys_exit: no process running\n");
        ENOSYS as u64
    }
}

