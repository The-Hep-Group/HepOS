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

/// Block until interrupt `vector` fires, instead of busy-polling for it.
/// No real device IRQ exists to wait on yet (every driver in this kernel
/// still polls) — the one real, always-firing interrupt today is the timer
/// (vector 0x20, `apic::TIMER_VECTOR` kernel-side; hardcoded here the same
/// way the RTC ports/Local APIC physical address above are, since userspace
/// can't `use` a kernel-crate constant across the ELF boundary).
pub fn sys_wait_irq(vector: u8) {
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 503u64 => _,
            inout("rdi") vector as u64 => _,
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
}

/// Allocate `len` bytes of fresh, zeroed, anonymous memory (not tied to any
/// physical MMIO allowlist) and map it into this process. Returns the
/// mapped user virtual address, or 0 on failure — needed for anything
/// bigger than the ~256KB static bump heap this crate's global allocator
/// works from (see `HEAP` above).
pub fn sys_mmap_anon(len: u64) -> u64 {
    let va: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 504u64 => va,
            inout("rdi") len => _,
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
    va
}

/// Fixed-layout struct `SYS_INPUT_STATE` fills in — **must stay
/// byte-for-byte identical to the kernel's copy** (`InputStateOut` in
/// `kernel/src/syscall.rs`), same constraint every driver mailbox already
/// has, since there's no shared crate between kernel and userspace to
/// enforce it.
#[repr(C)]
pub struct InputState {
    /// 1 if `mouse_x`/`y`/`buttons` were actually refreshed this call, 0 if
    /// the kernel's mouse-state lock was momentarily busy — in that case
    /// treat this whole snapshot's mouse fields as stale/absent (they're
    /// left at 0, not "same as last time") and keep whatever value you had
    /// from a previous call instead.
    pub mouse_valid: u32,
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub mouse_buttons: u32,
    pub key_count: u32,
    pub keys: [u8; 16],
}

/// Snapshot current mouse position/buttons and drain up to 16 pending
/// keyboard chars into `out`. Returns the number of keyboard chars written
/// (`out.key_count`, mirrored in the return value for convenience).
pub fn sys_input_state(out: &mut InputState) -> u64 {
    let count: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 505u64 => count,
            inout("rdi") (out as *mut InputState as u64) => _,
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
    count
}

/// One directory entry as filled in by a completed `sys_fs_list_dir` job,
/// via `sys_fs_poll()` — **must stay byte-for-byte identical to the
/// kernel's copy** (`DirEntryOut` in `kernel/src/syscall.rs`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntryOut {
    pub ino: u32,
    pub name_len: u32,
    pub name: [u8; 60],
}

// ── HepFS syscalls: async submit + poll ───────────────────────────────────
//
// These 4 `sys_fs_*` calls only *submit* a request — they return `0` if
// queued, or a negative -errno immediately if one's already in flight
// (`-16` = EBUSY) or the submit itself hit a momentarily-busy kernel lock
// (`-11` = EAGAIN, retry). The actual result (byte/entry count, or a
// -errno like `-2` ENOENT/`-20` ENOTDIR/`-21` EISDIR) only becomes
// available later, via `sys_fs_poll()` — call it in a loop with
// `sys_wait_irq()` in between until it stops returning `-16` (EBUSY,
// "still working on it"). See `kernel/src/syscall.rs`'s "HepFS syscalls"
// doc comment for why this can't just be one blocking call: the real disk
// I/O runs through `nvmed`'s mailbox, which needs the scheduler/timer to
// make progress — impossible from inside an interrupt-disabled syscall.

/// Submit a directory-listing request for `path`. Call `sys_fs_poll()`
/// afterward (looping on EBUSY) to get the entry count and fill `out`.
pub fn sys_fs_list_dir(path: &str) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 506u64 => ret,
            inout("rdi") path.as_ptr() as u64 => _,
            inout("rsi") path.len() as u64 => _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Submit a read request for the file at `path`. Call `sys_fs_poll()`
/// afterward (looping on EBUSY) to get the byte count and fill `out`.
pub fn sys_fs_read_file(path: &str) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 507u64 => ret,
            inout("rdi") path.as_ptr() as u64 => _,
            inout("rsi") path.len() as u64 => _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Submit a write request overwriting the file at `path` with `data`
/// (creating it first if it doesn't exist). `data` is copied into the
/// kernel immediately by this call, so it's safe to reuse/free `data`
/// before polling for completion. Call `sys_fs_poll()` afterward (looping
/// on EBUSY) for the final result.
pub fn sys_fs_write_file(path: &str, data: &[u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 508u64 => ret,
            inout("rdi") path.as_ptr() as u64 => _,
            inout("rsi") path.len() as u64 => _,
            inout("rdx") (data.as_ptr() as u64) => _,
            inout("r10") data.len() as u64 => _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Submit a create request for an empty file (`is_dir = false`) or
/// directory (`is_dir = true`) at `path`. Call `sys_fs_poll()` afterward
/// (looping on EBUSY) for the final result (0 on success).
pub fn sys_fs_create(path: &str, is_dir: bool) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 509u64 => ret,
            inout("rdi") path.as_ptr() as u64 => _,
            inout("rsi") path.len() as u64 => _,
            inout("rdx") (is_dir as u64) => _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Check whether the currently-outstanding HepFS job (submitted via one of
/// the 4 calls above) has finished, copying up to `out.len()` bytes of its
/// result data into `out` if so (a `DirEntryOut` array for `list_dir`, raw
/// file bytes for `read_file`; unused for `write_file`/`create`). Returns:
/// - `-16` (EBUSY) — still working on it, call again after `sys_wait_irq`.
/// - `-11` (EAGAIN) — the poll itself hit a momentarily-busy kernel lock;
///   retry the same way.
/// - `-2` (ENOENT, reused here for "no job") — nothing was submitted, or
///   the last result was already collected by an earlier poll.
/// - Otherwise: the job's real result (byte/entry count, or its own
///   -errno) — same convention a synchronous call would have used.
pub fn sys_fs_poll(out: &mut [u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 510u64 => ret,
            inout("rdi") (out.as_mut_ptr() as u64) => _,
            inout("rsi") out.len() as u64 => _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

// ── SYS_SERVICE_CTL / SYS_SERVICE_POLL / SYS_SPAWN_BYTES ────────────────────
//
// `service_id`: 0=rtl8139d, 1=hdad, 2=ahcid, 3=xhcid.
// `action`: 0=status, 1=start, 2=stop, 3=enable, 4=disable.
//
// status/enable/disable answer immediately (0, or for status a bitfield:
// bit0=running, bit1=enabled). start/stop only submit a job — the actual
// driver restart involves a spin-wait only safe to run from the kernel's
// own scheduled context, not inside a syscall (same reasoning as the FS
// syscalls) — poll with `sys_service_poll()` afterward, looping on EBUSY.

/// Submit a service control action. For `action` 1 (start) or 2 (stop), this
/// only queues the request (returns 0) — call `sys_service_poll()`
/// afterward, retrying on `-16` (EBUSY, still working) with `sys_wait_irq`
/// between attempts, for the real result. For `action` 0/3/4 (status/
/// enable/disable) the return value IS the final result already.
pub fn sys_service_ctl(service_id: u64, action: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 511u64 => ret,
            inout("rdi") service_id => _,
            inout("rsi") action => _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Poll for the result of an outstanding `sys_service_ctl()` start/stop.
/// Returns `-16` (EBUSY, still working), `-11` (EAGAIN, a lock was
/// momentarily busy, retry), `-2` (ENOENT, no job outstanding), or the job's
/// own result (0 on success, negative on failure).
pub fn sys_service_poll() -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 512u64 => ret,
            out("rdi") _,
            out("rsi") _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}

/// Run an ELF image already sitting in the caller's own memory (e.g. just
/// read via `sys_fs_read_file()`+`sys_fs_poll()`). Synchronous and
/// immediate — spawning itself is already non-blocking kernel-side (it only
/// queues a scheduler task), so unlike the FS/service calls above there's
/// nothing to poll for here. Returns 0 on success, negative on failure.
pub fn sys_spawn_bytes(data: &[u8], arg: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 513u64 => ret,
            inout("rdi") (data.as_ptr() as u64) => _,
            inout("rsi") data.len() as u64 => _,
            inout("rdx") arg => _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
            options(preserves_flags),
        );
    }
    ret as i64
}
