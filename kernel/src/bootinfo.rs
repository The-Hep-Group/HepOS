//! HepBL boot protocol — MUST stay in sync with hepbl/src/main.rs.
//!
//! HepBL passes a pointer to `BootInfo` in RDI when jumping to `kmain`.
//! The struct lives in a loader-allocated page (EfiLoaderData, never marked
//! usable in the memmap) so it remains valid for the kernel's lifetime.

pub const BOOTINFO_MAGIC: u64 = 0x4865_7042_4C21_0001; // "HepBL!" + version 1
pub const MAX_MEMMAP:     usize = 128;

#[repr(C)]
pub struct MemRegion {
    pub base: u64,
    pub len:  u64,
    pub typ:  u64, // 1 = usable RAM, 0 = reserved
}

#[repr(C)]
pub struct BootInfo {
    pub magic:        u64,
    pub hhdm_offset:  u64,
    pub fb_addr:      u64, // virtual (HHDM) address of framebuffer
    pub fb_width:     u64,
    pub fb_height:    u64,
    pub fb_pitch:     u64, // bytes per scanline
    pub fb_bpp:       u16, // bits per pixel
    pub fb_red_shift:   u8,
    pub fb_green_shift: u8,
    pub fb_blue_shift:  u8,
    pub _pad:         [u8; 3],
    pub memmap_count: u64,
    pub memmap:       [MemRegion; MAX_MEMMAP],
}
