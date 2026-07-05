use core::sync::atomic::{AtomicU64, Ordering};
use crate::bootinfo::MemRegion;

const PAGE_SIZE: usize = 4096;
const MAX_PAGES: usize = 512 * 1024; // 2 GB max tracked = 512k pages

// Bitmap: 1 bit per page. 0 = free, 1 = used.
// 512k pages / 64 bits per u64 = 8192 u64s = 64 KB of bitmap
static BITMAP: [AtomicU64; MAX_PAGES / 64] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_PAGES / 64]
};

static TOTAL_PAGES: AtomicU64 = AtomicU64::new(0);
static FREE_PAGES:  AtomicU64 = AtomicU64::new(0);

fn set_used(page: usize) {
    BITMAP[page / 64].fetch_or(1 << (page % 64), Ordering::Relaxed);
}

fn set_free(page: usize) {
    BITMAP[page / 64].fetch_and(!(1 << (page % 64)), Ordering::Relaxed);
}

fn is_used(page: usize) -> bool {
    BITMAP[page / 64].load(Ordering::Relaxed) & (1 << (page % 64)) != 0
}

pub fn init(memmap: &[MemRegion]) {
    // Start with everything marked used, then free usable regions
    for word in &BITMAP {
        word.store(u64::MAX, Ordering::Relaxed);
    }

    let mut total = 0u64;
    let mut free  = 0u64;

    for entry in memmap {
        let base  = entry.base as usize;
        let end   = base + entry.len as usize;
        let first = base / PAGE_SIZE;
        let last  = end  / PAGE_SIZE;

        for page in first..last {
            if page >= MAX_PAGES { break; }
            total += 1;
            // Only use pages above 1 MB — avoids the reserved hole at
            // 0xA0000–0xFFFFF (VGA/BIOS) which breaks contiguous heap assumptions.
            if entry.typ == 1 && base >= 0x10_0000 {
                set_free(page);
                free += 1;
            }
        }
    }

    TOTAL_PAGES.store(total, Ordering::Relaxed);
    FREE_PAGES .store(free,  Ordering::Relaxed);
}

/// Allocate one 4KB physical page. Returns physical address.
pub fn alloc_page() -> Option<u64> {
    for (i, word) in BITMAP.iter().enumerate() {
        let val = word.load(Ordering::Relaxed);
        if val == u64::MAX { continue; } // all used

        // find first free bit
        let bit = val.trailing_ones() as usize;
        let page = i * 64 + bit;
        if page >= MAX_PAGES { return None; }

        // try to claim it atomically
        let old = word.fetch_or(1 << bit, Ordering::Relaxed);
        if old & (1 << bit) == 0 {
            // we claimed it
            FREE_PAGES.fetch_sub(1, Ordering::Relaxed);
            return Some((page * PAGE_SIZE) as u64);
        }
        // someone else claimed it (won't happen single-core, but be safe)
    }
    None
}

/// Free a previously allocated physical page.
pub fn free_page(addr: u64) {
    let page = addr as usize / PAGE_SIZE;
    if page >= MAX_PAGES { return; }
    set_free(page);
    FREE_PAGES.fetch_add(1, Ordering::Relaxed);
}

pub fn free_pages()  -> u64 { FREE_PAGES .load(Ordering::Relaxed) }
pub fn total_pages() -> u64 { TOTAL_PAGES.load(Ordering::Relaxed) }

/// Allocate `count` physically contiguous pages. Returns base physical address.
pub fn alloc_contiguous(count: usize) -> Option<u64> {
    'outer: for i in 0..MAX_PAGES {
        // Check if pages i..i+count are all free
        for j in 0..count {
            if i + j >= MAX_PAGES { break 'outer; }
            let page = i + j;
            if BITMAP[page / 64].load(Ordering::Relaxed) & (1 << (page % 64)) != 0 {
                continue 'outer; // page i+j is used, try next
            }
        }
        // Found: claim all pages
        for j in 0..count {
            let page = i + j;
            BITMAP[page / 64].fetch_or(1 << (page % 64), Ordering::Relaxed);
        }
        FREE_PAGES.fetch_sub(count as u64, Ordering::Relaxed);
        return Some(i as u64 * PAGE_SIZE as u64);
    }
    None
}
