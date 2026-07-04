//! Ring-3 process support.
//!
//! `run_elf(data)` loads an ELF64 binary into a fresh user address space
//! and enters ring 3 via IRETQ.  The process calls back into the kernel
//! via SYSCALL; when it calls exit(N) the longjmp in `do_exit` returns
//! control here and `run_elf` returns N.
//!
//! `run_test()` runs an embedded ELF that prints "Hello from ring 3!" and
//! exits — useful as a quick sanity check without touching HepFS.
//!
//! The APIC timer is masked for the duration so the scheduler does not
//! try to context-switch away from a process it doesn't know about.

use core::sync::atomic::{AtomicU32, Ordering};
use alloc::{string::String, vec::Vec};
use spin::Mutex;
use crate::{apic, elf, paging, pmm, vmm};

// ── User virtual address layout ───────────────────────────────────────────────

const USER_STACK_PAGE: u64 = 0x7FFF_E000;   // one page mapped for the stack
const USER_STACK_TOP:  u64 = 0x7FFF_F000;   // RSP starts at page top (one page above)

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
pub fn take_proc_output() -> Vec<u8> {
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

/// Load and run an ELF64 binary, recording it in the process table.
/// This is the primary public entry point for running user programs.
pub fn exec(name: &str, data: &[u8]) -> Result<u64, &'static str> {
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
    let pml4 = create_user_pml4();

    let loaded = elf::load(data, pml4)?;

    // Map user stack (one page, read/write)
    let stack_phys = pmm::alloc_page().ok_or("run_elf: OOM for stack")?;
    unsafe { core::ptr::write_bytes(vmm::phys_to_virt(stack_phys), 0, 4096); }
    paging::map_page_into(pml4, USER_STACK_PAGE, stack_phys, paging::USER | paging::WRITE);

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
    pmm::free_page(stack_phys);
    for phys in loaded.pages { pmm::free_page(phys); }
    // Note: intermediate page-table pages (PT/PD/PDP) are leaked here.
    // A full cleanup would walk the low-half of pml4.  Acceptable for now.
    pmm::free_page(pml4);

    Ok(unsafe { EXIT_CODE })
}

/// Run the embedded ELF test binary (prints "Hello from ring 3!" via serial).
pub fn run_test() -> u64 {
    exec("<test>", &TEST_ELF).unwrap_or(u64::MAX)
}

// Baked-in hello ELF (generated by build.rs from userspace/target/.../hello).
// Empty slice if userspace hasn't been built yet.
include!(concat!(env!("OUT_DIR"), "/hello_elf.rs"));

/// Run the hello ELF built from userspace/hello — exercises hepos-rt + hepos-std.
pub fn run_hello() -> Result<u64, &'static str> {
    if HELLO_ELF.is_empty() {
        return Err("hello ELF not built — run build.ps1 first");
    }
    exec("<hello>", HELLO_ELF)
}
