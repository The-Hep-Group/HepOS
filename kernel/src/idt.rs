use core::arch::asm;
use crate::gdt::KERNEL_CS;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_lo:  u16,
    selector:   u16,
    ist:        u8,
    type_attr:  u8,
    offset_mid: u16,
    offset_hi:  u32,
    _reserved:  u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        Self { offset_lo: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_hi: 0, _reserved: 0 }
    }

    fn new(handler: u64) -> Self {
        Self {
            offset_lo:  handler as u16,
            selector:   KERNEL_CS as u16,
            ist:        0,
            type_attr:  0x8E, // present | ring0 | interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_hi:  (handler >> 32) as u32,
            _reserved:  0,
        }
    }
}

#[repr(C, packed)]
struct Idtr {
    size:   u16,
    offset: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::missing(); 256];

// CPU exception names
static EXCEPTION_NAMES: [&str; 32] = [
    "Division By Zero",       "Debug",
    "NMI",                    "Breakpoint",
    "Overflow",               "Bound Range Exceeded",
    "Invalid Opcode",         "Device Not Available",
    "Double Fault",           "Coprocessor Segment Overrun",
    "Invalid TSS",            "Segment Not Present",
    "Stack-Segment Fault",    "General Protection Fault",
    "Page Fault",             "Reserved",
    "x87 FPU Error",          "Alignment Check",
    "Machine Check",          "SIMD FP Exception",
    "Virtualization",         "Control Protection",
    "Reserved",               "Reserved",
    "Reserved",               "Reserved",
    "Reserved",               "Reserved",
    "Hypervisor Injection",   "VMM Communication",
    "Security Exception",     "Reserved",
];

#[repr(C)]
pub struct ExceptionFrame {
    // registers pushed by our stub
    pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64,
    pub r11: u64, pub r10: u64, pub r9:  u64, pub r8:  u64,
    pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64,
    pub rcx: u64, pub rbx: u64, pub rax: u64,
    // pushed by stub / CPU
    pub vector:    u64,
    pub error_code: u64,
    // pushed by CPU
    pub rip: u64, pub cs: u64, pub rflags: u64, pub rsp: u64, pub ss: u64,
}

#[unsafe(no_mangle)]
extern "C" fn exception_handler(frame: &ExceptionFrame) {
    let vec = frame.vector as usize;
    let name = if vec < 32 { EXCEPTION_NAMES[vec] } else { "Unknown" };

    crate::serial::print("\nKERNEL EXCEPTION: ");
    crate::serial::print(name);
    crate::serial::print(&alloc::format!(
        "  vector={:#x} error={:#x} rip={:#x} cr2={:#x} rsp={:#x} cs={:#x}\n",
        frame.vector, frame.error_code, frame.rip,
        unsafe { let cr2: u64; core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); cr2 },
        frame.rsp, frame.cs,
    ));
    crate::serial::print(&alloc::format!(
        "  rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x} rbp={:#x}\n",
        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp,
    ));
    crate::serial::print(&alloc::format!(
        "  r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}\n",
        frame.r8, frame.r9, frame.r10, frame.r11, frame.r12, frame.r13, frame.r14, frame.r15,
    ));

    // paint screen red and show exception info
    if let Some(display) = crate::DISPLAY.lock().as_mut() {
        use crate::framebuffer::Color;
        let red   = Color::from_hex(0x8B0000);
        let white = Color::from_hex(0xFFFFFF);
        let dim   = Color::from_hex(0xCCCCCC);

        display.clear(red);
        display.draw_text(24, 24, "!! KERNEL EXCEPTION !!", white, 2);
        display.draw_text(24, 60, name, white, 2);

        // print vector and error code as hex using a small helper
        let mut buf = [0u8; 32];
        let s = fmt_hex(frame.vector, &mut buf);
        display.draw_text(24, 96, "Vector: 0x", dim, 1);
        display.draw_text(114, 96, s, dim, 1);

        let mut buf2 = [0u8; 32];
        let s2 = fmt_hex(frame.error_code, &mut buf2);
        display.draw_text(24, 112, "Error:  0x", dim, 1);
        display.draw_text(114, 112, s2, dim, 1);

        let mut buf3 = [0u8; 32];
        let s3 = fmt_hex(frame.rip, &mut buf3);
        display.draw_text(24, 128, "RIP:    0x", dim, 1);
        display.draw_text(114, 128, s3, dim, 1);
    }

    loop { unsafe { asm!("hlt"); } }
}

fn fmt_hex<'a>(mut val: u64, buf: &'a mut [u8; 32]) -> &'a str {
    let digits = b"0123456789ABCDEF";
    let mut i = 32usize;
    loop {
        i -= 1;
        buf[i] = digits[(val & 0xF) as usize];
        val >>= 4;
        if val == 0 { break; }
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

// Macro to generate exception stubs
macro_rules! exception_stub {
    ($name:ident, $vec:literal, no_err) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "push 0",           // fake error code
                "push {0}",         // vector number
                "jmp {1}",
                const $vec as u64,
                sym common_stub,
            );
        }
    };
    ($name:ident, $vec:literal, has_err) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "push {0}",         // vector number (error code already on stack from CPU)
                "jmp {1}",
                const $vec as u64,
                sym common_stub,
            );
        }
    };
}

#[unsafe(naked)]
unsafe extern "C" fn common_stub() {
    core::arch::naked_asm!(
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8",  "push r9",  "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "mov rdi, rsp",          // pass frame pointer as first arg
        "call {0}",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "add rsp, 16",           // pop vector + error code
        "iretq",
        sym exception_handler,
    );
}

exception_stub!(ex0,  0,  no_err);
exception_stub!(ex1,  1,  no_err);
exception_stub!(ex2,  2,  no_err);
exception_stub!(ex3,  3,  no_err);
exception_stub!(ex4,  4,  no_err);
exception_stub!(ex5,  5,  no_err);
exception_stub!(ex6,  6,  no_err);
exception_stub!(ex7,  7,  no_err);
exception_stub!(ex8,  8,  has_err);
exception_stub!(ex9,  9,  no_err);
exception_stub!(ex10, 10, has_err);
exception_stub!(ex11, 11, has_err);
exception_stub!(ex12, 12, has_err);
exception_stub!(ex13, 13, has_err);
exception_stub!(ex14, 14, has_err);
exception_stub!(ex15, 15, no_err);
exception_stub!(ex16, 16, no_err);
exception_stub!(ex17, 17, has_err);
exception_stub!(ex18, 18, no_err);
exception_stub!(ex19, 19, no_err);
exception_stub!(ex20, 20, no_err);
exception_stub!(ex21, 21, has_err);
exception_stub!(ex22, 22, no_err);
exception_stub!(ex23, 23, no_err);
exception_stub!(ex24, 24, no_err);
exception_stub!(ex25, 25, no_err);
exception_stub!(ex26, 26, no_err);
exception_stub!(ex27, 27, no_err);
exception_stub!(ex28, 28, no_err);
exception_stub!(ex29, 29, has_err);
exception_stub!(ex30, 30, has_err);
exception_stub!(ex31, 31, no_err);

/// Naked timer interrupt stub — saves ALL caller-saved regs, runs scheduler, EOI.
#[unsafe(naked)]
pub unsafe extern "C" fn timer_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rsi", "push rdi",
        "push r8",  "push r9",  "push r10", "push r11",
        "call {tick}",          // may switch stacks internally; lock dropped before switch
        "call {eoi}",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rdi",  "pop rsi",
        "pop rdx",  "pop rcx",  "pop rax",
        "iretq",
        tick = sym crate::scheduler::tick,
        eoi  = sym crate::apic::eoi,
    );
}

/// Register an interrupt handler after init (for APIC timer etc.)
pub fn set_handler(vector: u8, handler: u64) {
    unsafe {
        #[allow(static_mut_refs)]
        IDT[vector as usize] = IdtEntry::new(handler);
    }
}

pub fn init() {
    let handlers: [u64; 32] = [
        ex0  as *const () as u64, ex1  as *const () as u64,
        ex2  as *const () as u64, ex3  as *const () as u64,
        ex4  as *const () as u64, ex5  as *const () as u64,
        ex6  as *const () as u64, ex7  as *const () as u64,
        ex8  as *const () as u64, ex9  as *const () as u64,
        ex10 as *const () as u64, ex11 as *const () as u64,
        ex12 as *const () as u64, ex13 as *const () as u64,
        ex14 as *const () as u64, ex15 as *const () as u64,
        ex16 as *const () as u64, ex17 as *const () as u64,
        ex18 as *const () as u64, ex19 as *const () as u64,
        ex20 as *const () as u64, ex21 as *const () as u64,
        ex22 as *const () as u64, ex23 as *const () as u64,
        ex24 as *const () as u64, ex25 as *const () as u64,
        ex26 as *const () as u64, ex27 as *const () as u64,
        ex28 as *const () as u64, ex29 as *const () as u64,
        ex30 as *const () as u64, ex31 as *const () as u64,
    ];

    unsafe {
        for (i, &h) in handlers.iter().enumerate() {
            IDT[i] = IdtEntry::new(h);
        }

        let idtr = Idtr {
            size:   (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
                #[allow(static_mut_refs)] offset: IDT.as_ptr() as u64,
        };

        asm!("lidt [{0}]", in(reg) &idtr, options(nostack, readonly));
    }
}
