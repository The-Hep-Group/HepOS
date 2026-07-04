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

/// Write bytes to a file descriptor (fd=1 = stdout → kernel terminal).
pub fn sys_write(fd: u64, buf: &[u8]) {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 1u64,
            in("rdi") fd,
            in("rsi") buf.as_ptr(),
            in("rdx") buf.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
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
            in("rax") 39u64,
            lateout("rax") pid,
            lateout("rcx") _,
            lateout("r11") _,
            options(preserves_flags),
        );
    }
    pid as u32
}
