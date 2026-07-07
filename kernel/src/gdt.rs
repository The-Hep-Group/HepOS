use core::arch::asm;

// Segment selectors
pub const KERNEL_CS: u64 = 0x08;
pub const KERNEL_SS: u64 = 0x10;
pub const USER_DS:   u64 = 0x1B; // GDT index 3, RPL=3
pub const USER_CS:   u64 = 0x23; // GDT index 4, RPL=3
pub const TSS_SEL:   u16 = 0x28; // GDT index 5 (16-byte TSS descriptor)

// ── TSS (64-bit, 104 bytes) ────────────────────────────────────────────────────

const TSS_SIZE: usize = core::mem::size_of::<Tss>();

#[repr(C, packed)]
pub struct Tss {
    _res0:  u32,
    pub rsp0: u64,  // kernel stack pointer for ring-0 entry from ring 3
    rsp1:   u64,
    rsp2:   u64,
    _res1:  u64,
    ist:    [u64; 7],
    _res2:  u64,
    _res3:  u16,
    iopb:   u16,    // IO permission bitmap offset; TSS_SIZE = no IOPB
}

impl Tss {
    const fn new() -> Self {
        Self {
            _res0: 0, rsp0: 0, rsp1: 0, rsp2: 0, _res1: 0,
            ist: [0u64; 7], _res2: 0, _res3: 0, iopb: TSS_SIZE as u16,
        }
    }
}

static mut TSS: Tss = Tss::new();

// ── GDT entries ───────────────────────────────────────────────────────────────

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GdtEntry {
    limit_low:   u16,
    base_low:    u16,
    base_mid:    u8,
    access:      u8,
    granularity: u8,
    base_high:   u8,
}

impl GdtEntry {
    const fn null() -> Self {
        Self { limit_low: 0, base_low: 0, base_mid: 0, access: 0, granularity: 0, base_high: 0 }
    }
    const fn code64(dpl: u8) -> Self {
        // access byte: P=1, DPL, S=1 (code/data), type=0xA (exec+read, 64-bit)
        // granularity: L=1 (64-bit), D=0
        let access = 0x9A | ((dpl & 3) << 5);
        Self { limit_low: 0xFFFF, base_low: 0, base_mid: 0, access, granularity: 0x20, base_high: 0 }
    }
    const fn data64(dpl: u8) -> Self {
        // access byte: P=1, DPL, S=1, type=2 (read/write data)
        let access = 0x92 | ((dpl & 3) << 5);
        Self { limit_low: 0xFFFF, base_low: 0, base_mid: 0, access, granularity: 0x00, base_high: 0 }
    }
}

// ── GDT table ─────────────────────────────────────────────────────────────────
//
// Selector layout required by STAR MSR for SYSCALL/SYSRET:
//   STAR[47:32] = 0x08  → SYSCALL CS=0x08 (kcode), SS=0x10 (kdata)
//   STAR[63:48] = 0x10  → SYSRETQ CS=0x10+16=0x20|3 (ucode), SS=0x10+8=0x18|3 (udata)
//
// Therefore user data MUST be at 0x18 and user code MUST be at 0x20.
#[repr(C)]
struct Gdt {
    null:  GdtEntry,  // 0x00 — null descriptor
    kcode: GdtEntry,  // 0x08 — kernel code64, DPL=0
    kdata: GdtEntry,  // 0x10 — kernel data64, DPL=0
    udata: GdtEntry,  // 0x18 — user data64, DPL=3  (USER_DS = 0x1B)
    ucode: GdtEntry,  // 0x20 — user code64, DPL=3  (USER_CS = 0x23)
    tss:   [u64; 2],  // 0x28 — 64-bit TSS system descriptor (16 bytes)
}

// SAFETY: must be in writable memory — ltr sets the TSS "busy" bit in-place.
static mut GDT: Gdt = Gdt {
    null:  GdtEntry::null(),
    kcode: GdtEntry::code64(0),
    kdata: GdtEntry::data64(0),
    udata: GdtEntry::data64(3),
    ucode: GdtEntry::code64(3),
    tss:   [0u64; 2],
};

#[repr(C, packed)]
struct Gdtr { size: u16, offset: u64 }

// Build the two 8-byte words of a 64-bit TSS/system segment descriptor.
fn tss_descriptor(base: u64, limit: u32) -> [u64; 2] {
    let lo =  (limit as u64 & 0x0000_FFFF)               // limit[15:0]
           | ((base  & 0x0000_FFFF) << 16)                // base[15:0]
           | (((base >> 16) & 0xFF) << 32)                // base[23:16]
           |  (0x89u64 << 40)                              // P=1, DPL=0, type=9 (avail 64-bit TSS)
           | (((limit as u64 >> 16) & 0xF) << 48)         // limit[19:16]
           | (((base >> 24) & 0xFF) << 56);               // base[31:24]
    let hi = (base >> 32) & 0xFFFF_FFFF;                  // base[63:32]
    [lo, hi]
}

pub fn init() {
    unsafe {
        // Fill in the TSS descriptor using the runtime address of TSS.
        let tss_addr  = core::ptr::addr_of!(TSS) as u64;
        GDT.tss = tss_descriptor(tss_addr, TSS_SIZE as u32 - 1);

        let gdtr = Gdtr {
            size:   core::mem::size_of::<Gdt>() as u16 - 1,
            offset: core::ptr::addr_of!(GDT) as u64,
        };
        asm!("lgdt [{}]", in(reg) &gdtr, options(nostack, readonly));

        // lgdt only repoints GDTR — it does NOT reload any segment register's
        // hidden descriptor cache. CS/SS/DS/ES/FS/GS all keep whatever stale
        // selectors HepBL/UEFI left them as (e.g. HepBL's own 64-bit code
        // segment, wherever it happened to sit in HepBL's GDT). That's
        // invisible during normal execution — the CPU only re-validates a
        // segment selector against the *current* GDT when the register is
        // explicitly reloaded (far call/jmp/ret, iretq, task switch) — but
        // the first genuine `iretq` (resuming a preempted task) reloads CS
        // from the interrupt frame's stale selector, which is almost
        // certainly out of range for our much smaller GDT → #GP. Reload
        // every segment register now, while nothing has taken a fault yet.
        // CS can't be loaded via `mov`, so do it via a far return.
        asm!(
            "push {cs_sel}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            cs_sel = const KERNEL_CS,
            out("rax") _,
            options(nostack),
        );
        asm!(
            "mov ss, {sel:x}",
            "mov ds, {sel:x}",
            "mov es, {sel:x}",
            "mov fs, {sel:x}",
            "mov gs, {sel:x}",
            sel = in(reg) KERNEL_SS as u16,
            options(nostack),
        );

        // Load the Task Register so the CPU can find RSP0 for ring-3→0 transitions.
        asm!("ltr {0:x}", in(reg) TSS_SEL, options(nostack));
    }
}

/// Update RSP0 in the TSS. Called by syscall::init() with the kernel stack top.
pub fn set_tss_rsp0(stack_top: u64) {
    unsafe { TSS.rsp0 = stack_top; }
}
