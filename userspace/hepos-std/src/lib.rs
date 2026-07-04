//! HepOS standard library facade.
//!
//! Re-exports `alloc` types under `std`-like paths and provides `print!` /
//! `println!` macros backed by the `sys_write` syscall.  Userspace programs
//! add `hepos-std` as a dependency, `extern crate hepos_rt` to pull in the
//! allocator/panic handler, and can then use familiar Rust idioms.
#![no_std]
extern crate alloc;

// ── Re-export alloc types at std-like paths ───────────────────────────────────

pub use alloc::borrow;
pub use alloc::boxed;
pub use alloc::collections;
pub use alloc::format;
pub use alloc::string::{self, String};
pub use alloc::vec::{self, Vec};

// ── I/O ───────────────────────────────────────────────────────────────────────

pub mod io {
    pub fn write_bytes(buf: &[u8]) {
        hepos_rt::sys_write(1, buf);
    }
}

// ── Process ───────────────────────────────────────────────────────────────────

pub mod process {
    pub fn exit(code: i32) -> ! {
        hepos_rt::sys_exit(code as u64);
    }
    pub fn id() -> u32 {
        hepos_rt::sys_getpid()
    }
}

// ── Macros ───────────────────────────────────────────────────────────────────

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        extern crate alloc;
        $crate::io::write_bytes(alloc::format!($($arg)*).as_bytes());
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::io::write_bytes(b"\n"); };
    ($($arg:tt)*) => {{
        extern crate alloc;
        $crate::io::write_bytes(alloc::format!($($arg)*).as_bytes());
        $crate::io::write_bytes(b"\n");
    }};
}
