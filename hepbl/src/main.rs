//! HepBL — the HepOS bootloader, written from scratch.
//!
//! A UEFI application: firmware hands us a 64-bit long-mode environment, so
//! the entire loader is Rust.  The only assembly is the final handoff (load
//! CR3, switch stack, jump to the kernel entry).
//!
//! Boot protocol (HepBL v1):
//!  - Kernel is an ELF64 loaded from `\kernel.elf` on the boot volume.
//!  - PT_LOAD segments are mapped at their linked virtual addresses
//!    (higher half, 0xffffffff80000000).
//!  - HHDM: all of phys 0..4 GiB is mapped at 0xffff800000000000 using
//!    4 KiB pages (the kernel's `map_page` walks tables assuming 4K leaves).
//!  - PML4[0] carries a transitional identity map so the loader survives
//!    the CR3 switch; the kernel clears it immediately.
//!  - Entry: sysv64, RDI = virtual pointer to `BootInfo` (see bootinfo module,
//!    kept in sync with kernel/src/bootinfo.rs).

#![no_std]
#![no_main]

use core::ffi::c_void;

// ═════════════════════════════════════════════════════════════════════════════
// Boot protocol — MUST match kernel/src/bootinfo.rs
// ═════════════════════════════════════════════════════════════════════════════

pub const BOOTINFO_MAGIC: u64 = 0x4865_7042_4C21_0001; // "HepBL!" + version 1
pub const HHDM_OFFSET:    u64 = 0xffff_8000_0000_0000;
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

// ═════════════════════════════════════════════════════════════════════════════
// Minimal UEFI FFI (hand-written from the UEFI spec)
// ═════════════════════════════════════════════════════════════════════════════

type Status = usize;
type Handle = *mut c_void;
const EFI_SUCCESS: Status = 0;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const GOP_GUID: Guid = Guid(0x9042a9de, 0x23dc, 0x4a38,
    [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]);
const LOADED_IMAGE_GUID: Guid = Guid(0x5b1b31a1, 0x9562, 0x11d2,
    [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
const SFS_GUID: Guid = Guid(0x964e5b22, 0x6459, 0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
const FILE_INFO_GUID: Guid = Guid(0x09576e92, 0x6d3f, 0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);

#[repr(C)]
struct TableHeader {
    signature:   u64,
    revision:    u32,
    header_size: u32,
    crc32:       u32,
    reserved:    u32,
}

#[repr(C)]
struct SimpleTextOutput {
    reset:         usize,
    output_string: extern "efiapi" fn(*mut SimpleTextOutput, *const u16) -> Status,
    // remaining members unused
}

#[repr(C)]
struct SystemTable {
    hdr:                TableHeader,
    fw_vendor:          *const u16,
    fw_revision:        u32,
    console_in_handle:  Handle,
    con_in:             *mut c_void,
    console_out_handle: Handle,
    con_out:            *mut SimpleTextOutput,
    stderr_handle:      Handle,
    std_err:            *mut c_void,
    runtime_services:   *mut c_void,
    boot_services:      *mut BootServices,
    num_table_entries:  usize,
    config_table:       *mut c_void,
}

/// EFI_BOOT_SERVICES — field order is normative (UEFI spec §4.4).
#[repr(C)]
struct BootServices {
    hdr: TableHeader,
    raise_tpl:   usize,
    restore_tpl: usize,
    allocate_pages: extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    free_pages:     extern "efiapi" fn(u64, usize) -> Status,
    get_memory_map: extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status,
    allocate_pool:  extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    free_pool:      extern "efiapi" fn(*mut u8) -> Status,
    create_event:  usize,
    set_timer:     usize,
    wait_for_event: usize,
    signal_event:  usize,
    close_event:   usize,
    check_event:   usize,
    install_protocol_interface:   usize,
    reinstall_protocol_interface: usize,
    uninstall_protocol_interface: usize,
    handle_protocol: extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    reserved: usize,
    register_protocol_notify: usize,
    locate_handle:      usize,
    locate_device_path: usize,
    install_configuration_table: usize,
    load_image:    usize,
    start_image:   usize,
    exit:          usize,
    unload_image:  usize,
    exit_boot_services: extern "efiapi" fn(Handle, usize) -> Status,
    get_next_monotonic_count: usize,
    stall: extern "efiapi" fn(usize) -> Status,
    set_watchdog_timer: extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    connect_controller:    usize,
    disconnect_controller: usize,
    open_protocol:  usize,
    close_protocol: usize,
    open_protocol_information: usize,
    protocols_per_handle: usize,
    locate_handle_buffer: usize,
    locate_protocol: extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> Status,
}

#[repr(C)]
struct LoadedImage {
    revision:      u32,
    parent_handle: Handle,
    system_table:  *mut SystemTable,
    device_handle: Handle,
    // remaining members unused
}

#[repr(C)]
struct SimpleFileSystem {
    revision:    u64,
    open_volume: extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut FileProtocol) -> Status,
}

#[repr(C)]
struct FileProtocol {
    revision: u64,
    open:  extern "efiapi" fn(*mut FileProtocol, *mut *mut FileProtocol, *const u16, u64, u64) -> Status,
    close: extern "efiapi" fn(*mut FileProtocol) -> Status,
    delete: usize,
    read:  extern "efiapi" fn(*mut FileProtocol, *mut usize, *mut u8) -> Status,
    write: usize,
    get_position: usize,
    set_position: usize,
    get_info: extern "efiapi" fn(*mut FileProtocol, *const Guid, *mut usize, *mut u8) -> Status,
    // remaining members unused
}

// GOP
#[repr(C)]
struct GopModeInfo {
    version:       u32,
    h_res:         u32,
    v_res:         u32,
    pixel_format:  u32, // 0=RGBX, 1=BGRX, 2=bitmask, 3=blt-only
    red_mask:      u32,
    green_mask:    u32,
    blue_mask:     u32,
    reserved_mask: u32,
    pixels_per_scanline: u32,
}

#[repr(C)]
struct GopMode {
    max_mode:     u32,
    mode:         u32,
    info:         *mut GopModeInfo,
    size_of_info: usize,
    fb_base:      u64,
    fb_size:      usize,
}

#[repr(C)]
struct Gop {
    query_mode: extern "efiapi" fn(*mut Gop, u32, *mut usize, *mut *mut GopModeInfo) -> Status,
    set_mode:   extern "efiapi" fn(*mut Gop, u32) -> Status,
    blt:        usize,
    mode:       *mut GopMode,
}

/// UEFI memory descriptor. Iterate using the runtime `desc_size`, not size_of.
#[repr(C)]
struct MemoryDescriptor {
    typ:        u32,
    _pad:       u32,
    phys_start: u64,
    virt_start: u64,
    num_pages:  u64,
    attribute:  u64,
}
const EFI_CONVENTIONAL_MEMORY: u32 = 7;

// ═════════════════════════════════════════════════════════════════════════════
// Console output helpers
// ═════════════════════════════════════════════════════════════════════════════

static mut CON_OUT: *mut SimpleTextOutput = core::ptr::null_mut();

fn print(s: &str) {
    unsafe {
        if CON_OUT.is_null() { return; }
        let mut buf = [0u16; 128];
        let mut i = 0;
        for c in s.chars() {
            if i >= 126 { break; }
            if c == '\n' { buf[i] = b'\r' as u16; i += 1; }
            buf[i] = c as u16; i += 1;
        }
        buf[i] = 0;
        ((*CON_OUT).output_string)(CON_OUT, buf.as_ptr());
    }
}

fn print_hex(v: u64) {
    let mut buf = [0u16; 20];
    buf[0] = b'0' as u16; buf[1] = b'x' as u16;
    for i in 0..16 {
        let nib = ((v >> ((15 - i) * 4)) & 0xF) as u8;
        buf[2 + i] = (if nib < 10 { b'0' + nib } else { b'a' + nib - 10 }) as u16;
    }
    buf[18] = 0;
    unsafe {
        if !CON_OUT.is_null() { ((*CON_OUT).output_string)(CON_OUT, buf.as_ptr()); }
    }
}

fn die(msg: &str, status: Status) -> ! {
    print("HepBL FATAL: ");
    print(msg);
    print(" status=");
    print_hex(status as u64);
    print("\n");
    loop { unsafe { core::arch::asm!("hlt"); } }
}

// ═════════════════════════════════════════════════════════════════════════════
// Page-table construction (identity + HHDM + kernel high-half)
// ═════════════════════════════════════════════════════════════════════════════

const PTE_P: u64 = 1;
const PTE_W: u64 = 1 << 1;

unsafe fn alloc_zeroed_page(bs: &BootServices) -> u64 {
    let mut addr: u64 = 0;
    // 0 = AllocateAnyPages, 2 = EfiLoaderData
    let s = (bs.allocate_pages)(0, 2, 1, &mut addr);
    if s != EFI_SUCCESS { die("page alloc", s); }
    core::ptr::write_bytes(addr as *mut u8, 0, 4096);
    addr
}

/// Map one 4 KiB page into `pml4` (tables addressed physically — we're still
/// on UEFI's identity mapping while building these).
unsafe fn map4k(bs: &BootServices, pml4: u64, virt: u64, phys: u64) {
    let idx = [
        ((virt >> 39) & 0x1FF) as usize,
        ((virt >> 30) & 0x1FF) as usize,
        ((virt >> 21) & 0x1FF) as usize,
        ((virt >> 12) & 0x1FF) as usize,
    ];
    let mut table = pml4;
    for level in 0..3 {
        let entry = (table as *mut u64).add(idx[level]);
        let e = entry.read();
        table = if e & PTE_P != 0 {
            e & 0x000F_FFFF_FFFF_F000
        } else {
            let p = alloc_zeroed_page(bs);
            entry.write(p | PTE_P | PTE_W);
            p
        };
    }
    (table as *mut u64).add(idx[3]).write(phys | PTE_P | PTE_W);
}

/// Build the full boot page tables. Returns the PML4 physical address.
///
/// - PML4[0]   → identity 0..4 GiB (transitional; kernel clears it)
/// - PML4[256] → HHDM alias of the same 0..4 GiB
/// Both share one PDPT whose PDs point to 4 KiB PTs (2048 PTs ≈ 8 MiB) so
/// the kernel's 4K-walking `map_page`/`map_mmio` can modify any mapping.
unsafe fn build_page_tables(bs: &BootServices) -> u64 {
    let pml4 = alloc_zeroed_page(bs);

    // One contiguous run for the whole HHDM tree: 1 PDPT + 4 PDs + 2048 PTs
    let total_pages = 1 + 4 + 2048;
    let mut base: u64 = 0;
    let s = (bs.allocate_pages)(0, 2, total_pages, &mut base);
    if s != EFI_SUCCESS { die("hhdm tables alloc", s); }
    core::ptr::write_bytes(base as *mut u8, 0, total_pages * 4096);

    let pdpt = base;
    let pds  = base + 4096;              // 4 pages
    let pts  = base + 4096 * 5;          // 2048 pages

    for g in 0..4u64 {                    // 4 × 1 GiB
        let pd = pds + g * 4096;
        (pdpt as *mut u64).add(g as usize).write(pd | PTE_P | PTE_W);
        for i in 0..512u64 {              // 512 × 2 MiB per PD
            let pt = pts + (g * 512 + i) * 4096;
            (pd as *mut u64).add(i as usize).write(pt | PTE_P | PTE_W);
            let frame_base = (g << 30) | (i << 21);
            for j in 0..512u64 {          // 512 × 4 KiB per PT
                (pt as *mut u64).add(j as usize)
                    .write((frame_base | (j << 12)) | PTE_P | PTE_W);
            }
        }
    }

    (pml4 as *mut u64).add(0).write(pdpt | PTE_P | PTE_W);   // identity (transitional)
    (pml4 as *mut u64).add(256).write(pdpt | PTE_P | PTE_W); // HHDM
    pml4
}

// ═════════════════════════════════════════════════════════════════════════════
// ELF64 loading
// ═════════════════════════════════════════════════════════════════════════════

unsafe fn load_kernel_elf(bs: &BootServices, data: *const u8, len: usize, pml4: u64) -> u64 {
    if len < 64 { die("kernel too small", len); }
    let magic = core::ptr::read_unaligned(data as *const u32);
    if magic != 0x464C_457F { die("bad ELF magic", magic as usize); }
    if *data.add(4) != 2 { die("not ELF64", 0); }

    let e_entry     = core::ptr::read_unaligned(data.add(24) as *const u64);
    let e_phoff     = core::ptr::read_unaligned(data.add(32) as *const u64) as usize;
    let e_phentsize = core::ptr::read_unaligned(data.add(54) as *const u16) as usize;
    let e_phnum     = core::ptr::read_unaligned(data.add(56) as *const u16) as usize;

    for i in 0..e_phnum {
        let ph = data.add(e_phoff + i * e_phentsize);
        let p_type = core::ptr::read_unaligned(ph as *const u32);
        if p_type != 1 { continue; } // PT_LOAD
        let p_offset = core::ptr::read_unaligned(ph.add(8)  as *const u64) as usize;
        let p_vaddr  = core::ptr::read_unaligned(ph.add(16) as *const u64);
        let p_filesz = core::ptr::read_unaligned(ph.add(32) as *const u64) as usize;
        let p_memsz  = core::ptr::read_unaligned(ph.add(40) as *const u64) as usize;

        let vstart = p_vaddr & !0xFFF;
        let vend   = (p_vaddr + p_memsz as u64 + 0xFFF) & !0xFFF;
        let pages  = ((vend - vstart) / 4096) as usize;

        let mut phys: u64 = 0;
        let s = (bs.allocate_pages)(0, 2, pages, &mut phys);
        if s != EFI_SUCCESS { die("segment alloc", s); }
        core::ptr::write_bytes(phys as *mut u8, 0, pages * 4096);

        core::ptr::copy_nonoverlapping(
            data.add(p_offset),
            (phys + (p_vaddr - vstart)) as *mut u8,
            p_filesz,
        );

        for pg in 0..pages as u64 {
            map4k(bs, pml4, vstart + pg * 4096, phys + pg * 4096);
        }
    }
    e_entry
}

// ═════════════════════════════════════════════════════════════════════════════
// Entry point
// ═════════════════════════════════════════════════════════════════════════════

#[no_mangle]
extern "efiapi" fn efi_main(image: Handle, st: *mut SystemTable) -> Status {
    unsafe { efi_main_inner(image, st) }
}

unsafe fn efi_main_inner(image: Handle, st: *mut SystemTable) -> Status {
    CON_OUT = (*st).con_out;
    let bs = &*(*st).boot_services;

    print("HepBL v0.1 - HepOS boot loader\n");
    (bs.set_watchdog_timer)(0, 0, 0, core::ptr::null());

    // ── 1. Graphics: locate GOP, prefer 1280x800 or 1024x768 ────────────────
    let mut gop_ptr: *mut c_void = core::ptr::null_mut();
    let s = (bs.locate_protocol)(&GOP_GUID, core::ptr::null_mut(), &mut gop_ptr);
    if s != EFI_SUCCESS { die("no GOP", s); }
    let gop = &mut *(gop_ptr as *mut Gop);

    let mut best_mode: Option<u32> = None;
    let mut best_score = 0u64;
    let max_mode = (*gop.mode).max_mode;
    for m in 0..max_mode {
        let mut info: *mut GopModeInfo = core::ptr::null_mut();
        let mut info_size = 0usize;
        if (gop.query_mode)(gop, m, &mut info_size, &mut info) != EFI_SUCCESS { continue; }
        let i = &*info;
        if i.pixel_format > 2 { continue; } // need a linear framebuffer
        let (w, h) = (i.h_res as u64, i.v_res as u64);
        // Prefer exactly 1280x800, then 1024x768, then largest fitting 1440x900
        let score = if w == 1280 && h == 800 { 1 << 40 }
                    else if w == 1024 && h == 768 { 1 << 39 }
                    else if w <= 1440 && h <= 900 { w * h }
                    else { 0 };
        if score > best_score { best_score = score; best_mode = Some(m); }
    }
    if let Some(m) = best_mode {
        if m != (*gop.mode).mode {
            let _ = (gop.set_mode)(gop, m); // non-fatal: keep current on failure
        }
    }

    let mode = &*gop.mode;
    let info = &*mode.info;
    let (fb_base, fb_size) = (mode.fb_base, mode.fb_size);
    let (fb_w, fb_h, ppsl) = (info.h_res, info.v_res, info.pixels_per_scanline);
    let (rs, gs, bl) = match info.pixel_format {
        0 => (0u8, 8u8, 16u8),  // RGBX
        1 => (16, 8, 0),        // BGRX
        _ => (
            info.red_mask.trailing_zeros() as u8,
            info.green_mask.trailing_zeros() as u8,
            info.blue_mask.trailing_zeros() as u8,
        ),
    };
    print("GOP framebuffer OK\n");

    // ── 2. Read \kernel.elf from the boot volume ─────────────────────────────
    let mut li_ptr: *mut c_void = core::ptr::null_mut();
    let s = (bs.handle_protocol)(image, &LOADED_IMAGE_GUID, &mut li_ptr);
    if s != EFI_SUCCESS { die("LoadedImage", s); }
    let device = (*(li_ptr as *mut LoadedImage)).device_handle;

    let mut sfs_ptr: *mut c_void = core::ptr::null_mut();
    let s = (bs.handle_protocol)(device, &SFS_GUID, &mut sfs_ptr);
    if s != EFI_SUCCESS { die("SimpleFS", s); }
    let sfs = &mut *(sfs_ptr as *mut SimpleFileSystem);

    let mut root: *mut FileProtocol = core::ptr::null_mut();
    let s = (sfs.open_volume)(sfs, &mut root);
    if s != EFI_SUCCESS { die("open volume", s); }

    let name: [u16; 11] = [
        b'k' as u16, b'e' as u16, b'r' as u16, b'n' as u16, b'e' as u16,
        b'l' as u16, b'.' as u16, b'e' as u16, b'l' as u16, b'f' as u16, 0,
    ];
    let mut file: *mut FileProtocol = core::ptr::null_mut();
    let s = ((*root).open)(root, &mut file, name.as_ptr(), 1 /* READ */, 0);
    if s != EFI_SUCCESS { die("open kernel.elf", s); }

    // File size via GetInfo (EFI_FILE_INFO.FileSize at byte offset 8)
    let mut fi_buf = [0u8; 512];
    let mut fi_size = fi_buf.len();
    let s = ((*file).get_info)(file, &FILE_INFO_GUID, &mut fi_size, fi_buf.as_mut_ptr());
    if s != EFI_SUCCESS { die("file info", s); }
    let file_size = core::ptr::read_unaligned(fi_buf.as_ptr().add(8) as *const u64) as usize;

    let mut kbuf: u64 = 0;
    let kpages = (file_size + 4095) / 4096;
    let s = (bs.allocate_pages)(0, 2, kpages, &mut kbuf);
    if s != EFI_SUCCESS { die("kernel buf alloc", s); }

    let mut read_len = file_size;
    let s = ((*file).read)(file, &mut read_len, kbuf as *mut u8);
    if s != EFI_SUCCESS || read_len != file_size { die("kernel read", s); }
    ((*file).close)(file);
    print("kernel.elf loaded\n");

    // ── 3. Page tables + ELF mapping ─────────────────────────────────────────
    let pml4  = build_page_tables(bs);
    let entry = load_kernel_elf(bs, kbuf as *const u8, file_size, pml4);
    print("kernel mapped, entry=");
    print_hex(entry);
    print("\n");

    // ── 4. Kernel stack (64 KiB) + BootInfo page ────────────────────────────
    let mut stack_phys: u64 = 0;
    let s = (bs.allocate_pages)(0, 2, 16, &mut stack_phys);
    if s != EFI_SUCCESS { die("stack alloc", s); }
    // SysV: RSP ≡ 8 (mod 16) at function entry
    let stack_top = HHDM_OFFSET + stack_phys + 16 * 4096 - 8;

    let mut bi_phys: u64 = 0;
    let s = (bs.allocate_pages)(0, 2, 1, &mut bi_phys);
    if s != EFI_SUCCESS { die("bootinfo alloc", s); }
    core::ptr::write_bytes(bi_phys as *mut u8, 0, 4096);
    let bi = &mut *(bi_phys as *mut BootInfo);
    bi.magic          = BOOTINFO_MAGIC;
    bi.hhdm_offset    = HHDM_OFFSET;
    bi.fb_addr        = HHDM_OFFSET + fb_base;
    bi.fb_width       = fb_w as u64;
    bi.fb_height      = fb_h as u64;
    bi.fb_pitch       = ppsl as u64 * 4;
    bi.fb_bpp         = 32;
    bi.fb_red_shift   = rs;
    bi.fb_green_shift = gs;
    bi.fb_blue_shift  = bl;
    let _ = fb_size;

    // ── 5. Memory map + ExitBootServices ─────────────────────────────────────
    // Buffer allocated up front: no further allocations between the final
    // GetMemoryMap and ExitBootServices (that would invalidate the map key).
    let mut mm_buf: u64 = 0;
    let s = (bs.allocate_pages)(0, 2, 16, &mut mm_buf); // 64 KiB
    if s != EFI_SUCCESS { die("memmap buf", s); }

    print("exiting boot services...\n");
    let mut mm_size;
    let mut desc_size = 0usize;
    loop {
        mm_size = 16 * 4096;
        let mut key = 0usize;
        let mut ver = 0u32;
        let s = (bs.get_memory_map)(&mut mm_size, mm_buf as *mut u8, &mut key,
                                    &mut desc_size, &mut ver);
        if s != EFI_SUCCESS { die("get memory map", s); }
        if (bs.exit_boot_services)(image, key) == EFI_SUCCESS { break; }
        // Map changed between calls — retry per spec.
    }
    // Boot services are gone: no printing, no allocation beyond this point.

    // Convert UEFI memmap → BootInfo regions (merge adjacent same-type)
    let count = mm_size / desc_size;
    let mut n = 0usize;
    for i in 0..count {
        let d = &*((mm_buf as usize + i * desc_size) as *const MemoryDescriptor);
        let typ: u64 = if d.typ == EFI_CONVENTIONAL_MEMORY { 1 } else { 0 };
        let base = d.phys_start;
        let len  = d.num_pages * 4096;
        if n > 0 {
            let prev = &mut bi.memmap[n - 1];
            if prev.typ == typ && prev.base + prev.len == base {
                prev.len += len;
                continue;
            }
        }
        if n >= MAX_MEMMAP { break; }
        bi.memmap[n] = MemRegion { base, len, typ };
        n += 1;
    }
    bi.memmap_count = n as u64;

    // ── 6. Handoff: CR3, stack, jump. RDI = &BootInfo (HHDM virtual). ───────
    let bi_virt = HHDM_OFFSET + bi_phys;
    core::arch::asm!(
        "cli",
        "mov cr3, {pml4}",
        "mov rsp, {stack}",
        "xor rbp, rbp",
        "jmp {entry}",
        pml4  = in(reg) pml4,
        stack = in(reg) stack_top,
        entry = in(reg) entry,
        in("rdi") bi_virt,
        options(noreturn),
    );
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    print("HepBL panic\n");
    loop { unsafe { core::arch::asm!("hlt"); } }
}
