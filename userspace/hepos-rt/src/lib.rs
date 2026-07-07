//! HepOS ring-3 runtime: allocator, panic handler, and raw syscall wrappers.
//! Every userspace binary must link this crate (via `extern crate hepos_rt`)
//! to get a global allocator and panic handler — required for `alloc` to work.
#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Global bump allocator (256 KB static arena, no dealloc) ──────────────────

static mut HEAP: [u8; 262144] = [0u8; 262144];
static HEAP_OFF: AtomicUsize   = AtomicUsize::new(0);

struct BumpAlloc;

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size  = layout.size();
        let mut off = HEAP_OFF.load(Ordering::Relaxed);
        loop {
            let aligned = (off + align - 1) & !(align - 1);
            let end     = aligned + size;
            if end > 262144 { return core::ptr::null_mut(); }
            match HEAP_OFF.compare_exchange(off, end, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_)  => return core::ptr::addr_of_mut!(HEAP).cast::<u8>().add(aligned),
                Err(e) => off = e,
            }
        }
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

// ── Panic handler ─────────────────────────────────────────────────────────────

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys_exit(101);
}

// ── Syscall wrappers ──────────────────────────────────────────────────────────
//
// The kernel's SYSCALL entry stub (kernel/src/syscall.rs) always shuffles
// rdi/rsi/rdx/r10/r8/r9 into SysV call-argument registers for the dispatcher,
// and rcx/r11 hold the saved user RIP/RFLAGS — every one of those registers
// comes back from `syscall` holding something other than what the caller put
// in, *regardless of how many arguments this particular call actually uses*.
// Every register in that set must be declared `inout`/`lateout` (never left
// as a bare `in`), or the compiler may keep some other live value pinned in
// one of them across the call, believing it survives — a real, previously
// unnoticed bug here corrupted a caller's return address this way (see
// PLAN.md's "Userspace drivers" writeup for how it was found).

/// Write bytes to a file descriptor (fd=1 = stdout → kernel terminal).
pub fn sys_write(fd: u64, buf: &[u8]) {
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 1u64 => _,
            inout("rdi") fd => _,
            inout("rsi") buf.as_ptr() => _,
            inout("rdx") buf.len() as u64 => _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
}

/// Terminate the process with exit code `code`.
pub fn sys_exit(code: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 60u64,
            in("rdi") code,
            options(noreturn),
        );
    }
}

/// Return the PID of the calling process.
pub fn sys_getpid() -> u32 {
    let pid: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 39u64 => pid,
            out("rdi") _,
            out("rsi") _,
            out("rdx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    pid as u32
}

// ── HepOS-specific hardware-access syscalls ───────────────────────────────────
// Foundational primitives for userspace drivers: map a physical MMIO region
// into this process, and do privileged port I/O via the kernel (ring 3 has
// no IOPL/I/O-bitmap here, so these always go through SYSCALL).

/// Map `len` bytes of physical MMIO space into this process. Returns the
/// mapped user virtual address, or 0 on failure.
pub fn sys_mmap_mmio(phys_addr: u64, len: u64) -> u64 {
    let va: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 500u64 => va,
            inout("rdi") phys_addr => _,
            inout("rsi") len => _,
            out("rdx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    va
}

/// Read `width` bytes (1, 2, or 4) from an I/O port.
pub fn sys_port_in(port: u16, width: u8) -> u32 {
    let val: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 501u64 => val,
            inout("rdi") port as u64 => _,
            inout("rsi") width as u64 => _,
            out("rdx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    val as u32
}

/// Write `width` bytes (1, 2, or 4) to an I/O port.
pub fn sys_port_out(port: u16, width: u8, value: u32) {
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 502u64 => _,
            inout("rdi") port as u64 => _,
            inout("rsi") width as u64 => _,
            inout("rdx") value as u64 => _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
}
