//! ELF64 loader.
//!
//! Parses an ELF64 executable byte slice and maps all PT_LOAD segments
//! into an existing user PML4 (physical address) using `paging::map_page_into`.
//! BSS (p_memsz > p_filesz) is zeroed automatically.
//!
//! Returns the entry point virtual address and a list of physical pages
//! that the caller must free once the process has exited.

use alloc::vec::Vec;
use crate::{paging, pmm, vmm};

const PAGE_SIZE: usize = 4096;

pub struct Loaded {
    pub entry: u64,
    pub pages: Vec<u64>,   // physical pages to free after the process exits
}

pub fn load(data: &[u8], pml4_phys: u64) -> Result<Loaded, &'static str> {
    if data.len() < 64 { return Err("ELF too small"); }

    // ELF magic + class + data + type + machine
    if &data[0..4] != b"\x7fELF"  { return Err("bad ELF magic"); }
    if data[4] != 2                { return Err("not ELF64"); }
    if data[5] != 1                { return Err("not little-endian"); }
    let e_type    = u16::from_le_bytes([data[16], data[17]]);
    let e_machine = u16::from_le_bytes([data[18], data[19]]);
    if e_type    != 2    { return Err("not an executable ELF"); }
    if e_machine != 0x3E { return Err("not x86-64"); }

    let e_entry     = u64::from_le_bytes(data[24..32].try_into().unwrap());
    let e_phoff     = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes([data[54], data[55]]) as usize;
    let e_phnum     = u16::from_le_bytes([data[56], data[57]]) as usize;

    if e_phentsize < 56 { return Err("phentsize too small"); }

    let mut pages: Vec<u64> = Vec::new();
    // Tracks virt-page -> phys-page for pages already allocated by an earlier
    // PT_LOAD segment in this same file, so a later segment that shares a
    // page (e.g. a small RELRO/GOT segment tail-packed into the same page as
    // the preceding code/rodata segment — common, since segments only need
    // page alignment, not exclusivity) writes into the SAME physical page
    // instead of allocating a fresh, zeroed one and silently discarding
    // everything the earlier segment had already placed there.
    let mut page_map: Vec<(u64, u64)> = Vec::new();

    for i in 0..e_phnum {
        let ph_off = e_phoff + i * e_phentsize;
        if ph_off + 56 > data.len() { return Err("program header out of bounds"); }
        let ph = &data[ph_off..ph_off + 56];

        let p_type  = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        if p_type != 1 { continue; }  // skip non PT_LOAD

        let p_flags  = u32::from_le_bytes(ph[4..8].try_into().unwrap());
        let p_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap()) as usize;
        let p_vaddr  = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap()) as usize;
        let p_memsz  = u64::from_le_bytes(ph[40..48].try_into().unwrap()) as usize;

        if p_memsz == 0 { continue; }
        if p_filesz > 0 && p_offset + p_filesz > data.len() {
            return Err("segment data extends past end of file");
        }

        // paging flags: USER always; WRITE if segment is writable (PF_W = bit 1)
        let pg_flags = paging::USER | paging::WRITE |
            if p_flags & 2 != 0 { 0 } else { 0 };  // NX not implemented; WRITE always for now

        let page_start = (p_vaddr as usize) & !(PAGE_SIZE - 1);
        let page_end   = (p_vaddr as usize + p_memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages  = (page_end - page_start) / PAGE_SIZE;

        for p in 0..num_pages {
            let virt = (page_start + p * PAGE_SIZE) as u64;

            let phys = if let Some(&(_, existing)) = page_map.iter().find(|&&(v, _)| v == virt) {
                // Already mapped by an earlier segment sharing this page —
                // reuse it (don't re-zero: that would erase that segment's data).
                existing
            } else {
                let phys = pmm::alloc_page().ok_or("ELF load: out of physical memory")?;
                pages.push(phys);
                page_map.push((virt, phys));
                // Zero the page first (covers BSS)
                let dest = unsafe {
                    core::slice::from_raw_parts_mut(vmm::phys_to_virt(phys), PAGE_SIZE)
                };
                dest.fill(0);
                paging::map_page_into(pml4_phys, virt, phys, pg_flags);
                phys
            };

            // Copy file bytes that fall within [p_vaddr .. p_vaddr + p_filesz)
            // intersected with [page_va_lo .. page_va_hi)
            let page_va_lo = page_start + p * PAGE_SIZE;
            let page_va_hi = page_va_lo + PAGE_SIZE;
            let file_va_lo = p_vaddr as usize;
            let file_va_hi = file_va_lo + p_filesz;

            let copy_lo = file_va_lo.max(page_va_lo);
            let copy_hi = file_va_hi.min(page_va_hi);

            if copy_lo < copy_hi {
                let dst_off = copy_lo - page_va_lo;
                let src_off = p_offset + (copy_lo - file_va_lo);
                let len     = copy_hi - copy_lo;
                let dest = unsafe {
                    core::slice::from_raw_parts_mut(vmm::phys_to_virt(phys), PAGE_SIZE)
                };
                dest[dst_off..dst_off + len].copy_from_slice(&data[src_off..src_off + len]);
            }
        }
    }

    if pages.is_empty() { return Err("no PT_LOAD segments found"); }

    Ok(Loaded { entry: e_entry, pages })
}
