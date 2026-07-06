//! HepFS — flat-inode filesystem for HepOS
//!
//! Disk layout (4 KB blocks):
//!   Block 0      : Superblock
//!   Block 1      : Inode bitmap  (1 block  = 32 768 bits)
//!   Blocks 2–5   : Block bitmap  (4 blocks = 131 072 bits)
//!   Blocks 6–37  : Inode table   (32 blocks × 32 inodes = 1 024 inodes)
//!   Blocks 38+   : Data blocks
//!
//! Max file size: 12 direct × 4 KB + 1024 indirect × 4 KB + 1024×1024 double-indirect × 4 KB
//!              = 48 KB + 4 MB + 4 GB ≈ 4 GB (bounded in practice by disk capacity)

use alloc::{string::String, vec::Vec};
use crate::{nvme::NvmeController, pmm, vmm};

// ── Constants ────────────────────────────────────────────────────────────────
pub const MAGIC:           u64   = 0x48657046_53000001; // "HepFS\0\0\1"
pub const BLOCK_SIZE:      usize = 4096;
const INODES_PER_BLK:      usize = BLOCK_SIZE / 128;   // 32
const MAX_INODES:          u32   = 1024;
const DIRECT_PTRS:         usize = 12;

const SB_BLOCK:            u64 = 0;
const INODE_BM_BLOCK:      u64 = 1;
const BLOCK_BM_BLOCK:      u64 = 2;
const BLOCK_BM_LEN:        u64 = 4;
const INODE_TBL_BLOCK:     u64 = 6;
const INODE_TBL_LEN:       u64 = 32;
pub const DATA_BLOCK_START: u64 = 38; // first data block

pub const ROOT_INO:        u32 = 0;
pub const F_FREE:          u32 = 0;
pub const F_FILE:          u32 = 1;
pub const F_DIR:           u32 = 2;

const DIR_ENTRY_SIZE:      usize = 32;
const ENTRIES_PER_BLK:     usize = BLOCK_SIZE / DIR_ENTRY_SIZE; // 128
const PTRS_PER_INDIRECT:   usize = BLOCK_SIZE / 4;              // 1024 block pointers per indirect block
const DINDIRECT_MAX_BLOCKS: usize = PTRS_PER_INDIRECT * PTRS_PER_INDIRECT; // 1024×1024 blocks via double-indirect

// ── On-disk structures ───────────────────────────────────────────────────────
#[repr(C)]
struct Superblock {
    magic:         u64,
    block_size:    u32,
    total_blocks:  u64,
    free_blocks:   u64,
    total_inodes:  u32,
    free_inodes:   u32,
    _pad: [u8; BLOCK_SIZE - 40],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Inode {
    pub flags:    u32,
    pub size:     u64,
    pub nblocks:  u32,
    pub ctime:    u64,
    pub mtime:    u64,
    pub direct:    [u32; DIRECT_PTRS],  // absolute block numbers (0 = unused)
    pub indirect:  u32,
    pub dindirect: u32, // double-indirect: block of 1024 pointers to indirect blocks
    _pad: [u8; 40],
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            flags: 0, size: 0, nblocks: 0, ctime: 0, mtime: 0,
            direct: [0u32; 12], indirect: 0, dindirect: 0, _pad: [0u8; 40],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub inode:    u32,     // 0 = unused slot
    pub name_len: u8,
    pub name:     [u8; 27],
}

impl DirEntry {
    pub fn name_str(&self) -> &str {
        let len = self.name_len as usize;
        core::str::from_utf8(&self.name[..len.min(27)]).unwrap_or("")
    }
}

// ── I/O page (DMA-safe, HHDM-backed) ────────────────────────────────────────
struct Page {
    phys: u64,
    virt: *mut u8,
}
impl Page {
    fn alloc() -> Self {
        let phys = pmm::alloc_page().expect("hepfs: OOM");
        let virt = vmm::phys_to_virt(phys);
        unsafe { core::ptr::write_bytes(virt, 0, BLOCK_SIZE); }
        Page { phys, virt }
    }
    fn as_slice(&self)     -> &[u8]     { unsafe { core::slice::from_raw_parts(self.virt, BLOCK_SIZE) } }
    fn as_mut_slice(&self) -> &mut [u8] { unsafe { core::slice::from_raw_parts_mut(self.virt, BLOCK_SIZE) } }
}

// ── Low-level block I/O ──────────────────────────────────────────────────────
fn sectors_per_block(ctrl: &NvmeController) -> u16 {
    (BLOCK_SIZE / ctrl.lba_size as usize) as u16
}

fn read_block(ctrl: &mut NvmeController, block: u64) -> Page {
    let page = Page::alloc();
    let spb  = sectors_per_block(ctrl);
    ctrl.read_blocks(block * spb as u64, spb, page.phys);
    page
}

fn write_block(ctrl: &mut NvmeController, block: u64, page: &Page) {
    let spb = sectors_per_block(ctrl);
    ctrl.write_blocks(block * spb as u64, spb, page.phys);
}

// ── Bitmap helpers ───────────────────────────────────────────────────────────
fn bitmap_alloc(ctrl: &mut NvmeController, bm_start: u64, bm_len: u64, skip: u64) -> Option<u64> {
    for bi in 0..bm_len {
        let page = read_block(ctrl, bm_start + bi);
        let buf  = page.as_slice();
        for byte in 0..BLOCK_SIZE {
            if buf[byte] == 0xFF { continue; }
            for bit in 0..8u64 {
                if buf[byte] & (1 << bit) == 0 {
                    let idx = bi * BLOCK_SIZE as u64 * 8 + byte as u64 * 8 + bit;
                    if idx < skip { continue; }
                    // claim it
                    let mpage = read_block(ctrl, bm_start + bi);
                    let mbuf  = mpage.as_mut_slice();
                    mbuf[byte] |= 1 << bit;
                    write_block(ctrl, bm_start + bi, &mpage);
                    return Some(idx);
                }
            }
        }
    }
    None
}

fn bitmap_set(ctrl: &mut NvmeController, bm_start: u64, idx: u64, used: bool) {
    let block = bm_start + idx / (BLOCK_SIZE as u64 * 8);
    let byte  = (idx / 8) as usize % BLOCK_SIZE;
    let bit   = idx % 8;
    let page  = read_block(ctrl, block);
    let buf   = page.as_mut_slice();
    if used { buf[byte] |= 1 << bit; } else { buf[byte] &= !(1 << bit); }
    write_block(ctrl, block, &page);
}

// ── Inode I/O ────────────────────────────────────────────────────────────────
pub fn read_inode(ctrl: &mut NvmeController, ino: u32) -> Inode {
    let blk = INODE_TBL_BLOCK + (ino as u64 / INODES_PER_BLK as u64);
    let off = (ino as usize % INODES_PER_BLK) * 128;
    let page = read_block(ctrl, blk);
    unsafe { *(page.as_slice().as_ptr().add(off) as *const Inode) }
}

pub fn write_inode(ctrl: &mut NvmeController, ino: u32, inode: &Inode) {
    let blk  = INODE_TBL_BLOCK + (ino as u64 / INODES_PER_BLK as u64);
    let off  = (ino as usize % INODES_PER_BLK) * 128;
    let page = read_block(ctrl, blk);
    unsafe { *(page.as_mut_slice().as_mut_ptr().add(off) as *mut Inode) = *inode; }
    write_block(ctrl, blk, &page);
}

// ── Block allocation ──────────────────────────────────────────────────────────
fn alloc_block(ctrl: &mut NvmeController) -> u32 {
    // skip the first DATA_BLOCK_START blocks (they are system blocks)
    let blk = bitmap_alloc(ctrl, BLOCK_BM_BLOCK, BLOCK_BM_LEN, DATA_BLOCK_START)
        .expect("hepfs: disk full");
    blk as u32
}

fn alloc_inode(ctrl: &mut NvmeController) -> u32 {
    let ino = bitmap_alloc(ctrl, INODE_BM_BLOCK, 1, 0)
        .expect("hepfs: inode table full") as u32;
    assert!(ino < MAX_INODES, "hepfs: too many inodes");
    ino
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Format a fresh filesystem onto the NVMe controller.
pub fn format(ctrl: &mut NvmeController) {
    let total_blocks = ctrl.lba_count * ctrl.lba_size as u64 / BLOCK_SIZE as u64;

    // Write superblock
    let sb_page = Page::alloc();
    let sb = unsafe { &mut *(sb_page.virt as *mut Superblock) };
    sb.magic        = MAGIC;
    sb.block_size   = BLOCK_SIZE as u32;
    sb.total_blocks = total_blocks;
    sb.free_blocks  = total_blocks - DATA_BLOCK_START;
    sb.total_inodes = MAX_INODES;
    sb.free_inodes  = MAX_INODES - 1; // root takes inode 0
    write_block(ctrl, SB_BLOCK, &sb_page);

    // Clear bitmaps
    let empty = Page::alloc();
    for i in 0..BLOCK_BM_LEN + 1 {
        write_block(ctrl, INODE_BM_BLOCK + i, &empty);
    }

    // Mark system blocks (0–37) as used in block bitmap
    for b in 0..DATA_BLOCK_START {
        bitmap_set(ctrl, BLOCK_BM_BLOCK, b, true);
    }

    // Mark inode 0 (root) as used
    bitmap_set(ctrl, INODE_BM_BLOCK, 0, true);

    // Clear inode table
    for i in 0..INODE_TBL_LEN {
        write_block(ctrl, INODE_TBL_BLOCK + i, &empty);
    }

    // Create root directory inode
    let root_blk = alloc_block(ctrl);
    let mut root = Inode::default();
    root.flags    = F_DIR;
    root.size     = 0;
    root.nblocks  = 1;
    root.direct[0] = root_blk;
    write_inode(ctrl, ROOT_INO, &root);

    // Clear root directory data block
    write_block(ctrl, root_blk as u64, &empty);
}

/// Verify the magic number. Returns true if a valid HepFS is present.
pub fn probe(ctrl: &mut NvmeController) -> bool {
    let page = read_block(ctrl, SB_BLOCK);
    let magic = unsafe { *(page.virt as *const u64) };
    magic == MAGIC
}

/// Resolve an absolute path like "/foo/bar" to an inode number.
pub fn lookup(ctrl: &mut NvmeController, path: &str) -> Option<u32> {
    let mut ino = ROOT_INO;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        ino = find_in_dir(ctrl, ino, part)?;
    }
    Some(ino)
}

/// List entries in a directory. Returns Vec of (inode, name).
pub fn list_dir(ctrl: &mut NvmeController, dir_ino: u32) -> Vec<(u32, String)> {
    let inode = read_inode(ctrl, dir_ino);
    assert!(inode.flags == F_DIR, "not a directory");
    let mut out = Vec::new();
    for &blk in inode.direct.iter().filter(|&&b| b != 0) {
        let page = read_block(ctrl, blk as u64);
        let buf  = page.as_slice();
        for i in 0..ENTRIES_PER_BLK {
            let e = unsafe { *(buf.as_ptr().add(i * DIR_ENTRY_SIZE) as *const DirEntry) };
            if e.inode != 0 {
                out.push((e.inode, String::from(e.name_str())));
            }
        }
    }
    out
}

fn find_in_dir(ctrl: &mut NvmeController, dir_ino: u32, name: &str) -> Option<u32> {
    let inode = read_inode(ctrl, dir_ino);
    for &blk in inode.direct.iter().filter(|&&b| b != 0) {
        let page = read_block(ctrl, blk as u64);
        let buf  = page.as_slice();
        for i in 0..ENTRIES_PER_BLK {
            let e = unsafe { *(buf.as_ptr().add(i * DIR_ENTRY_SIZE) as *const DirEntry) };
            if e.inode != 0 && e.name_str() == name {
                return Some(e.inode);
            }
        }
    }
    None
}

fn add_dir_entry(ctrl: &mut NvmeController, dir_ino: u32, name: &str, ino: u32) {
    let mut inode = read_inode(ctrl, dir_ino);
    let name_bytes = name.as_bytes();
    assert!(name_bytes.len() <= 27, "filename too long (max 27)");

    // Find a free slot in existing blocks, or allocate a new block
    for slot_blk in 0..DIRECT_PTRS {
        if inode.direct[slot_blk] == 0 {
            // Allocate new data block for this directory
            let new_blk = alloc_block(ctrl);
            inode.direct[slot_blk] = new_blk;
            inode.nblocks += 1;
            write_inode(ctrl, dir_ino, &inode);
        }
        let blk  = inode.direct[slot_blk];
        let page = read_block(ctrl, blk as u64);
        let buf  = page.as_mut_slice();
        for i in 0..ENTRIES_PER_BLK {
            let ep = unsafe { buf.as_mut_ptr().add(i * DIR_ENTRY_SIZE) as *mut DirEntry };
            if unsafe { (*ep).inode } == 0 {
                let mut entry = DirEntry { inode: ino, name_len: name_bytes.len() as u8, name: [0; 27] };
                entry.name[..name_bytes.len()].copy_from_slice(name_bytes);
                unsafe { *ep = entry; }
                write_block(ctrl, blk as u64, &page);
                inode.size += DIR_ENTRY_SIZE as u64;
                write_inode(ctrl, dir_ino, &inode);
                return;
            }
        }
    }
    panic!("hepfs: directory full");
}

/// Create a file inside parent directory. Returns the new inode number.
pub fn create_file(ctrl: &mut NvmeController, parent: u32, name: &str) -> u32 {
    let ino  = alloc_inode(ctrl);
    let inode = Inode { flags: F_FILE, ..Default::default() };
    write_inode(ctrl, ino, &inode);
    add_dir_entry(ctrl, parent, name, ino);
    ino
}

/// Create a subdirectory inside parent directory. Returns the new inode number.
pub fn create_dir(ctrl: &mut NvmeController, parent: u32, name: &str) -> u32 {
    let ino  = alloc_inode(ctrl);
    let blk  = alloc_block(ctrl);
    let inode = Inode { flags: F_DIR, nblocks: 1, direct: { let mut d = [0u32; 12]; d[0] = blk; d }, ..Default::default() };
    write_inode(ctrl, ino, &inode);
    // clear the dir data block
    let empty = Page::alloc();
    write_block(ctrl, blk as u64, &empty);
    add_dir_entry(ctrl, parent, name, ino);
    ino
}

/// Write data to a file (overwrites from offset 0).
/// Max size: 12 direct blocks + 1024 indirect blocks = ~4.1 MB.
pub fn write_file(ctrl: &mut NvmeController, ino: u32, data: &[u8]) {
    let mut inode = read_inode(ctrl, ino);
    assert!(inode.flags == F_FILE, "not a file");

    let mut remaining = data;
    let mut block_idx = 0usize;

    // Phase 1: direct blocks (0..12)
    while !remaining.is_empty() && block_idx < DIRECT_PTRS {
        if inode.direct[block_idx] == 0 {
            inode.direct[block_idx] = alloc_block(ctrl);
            inode.nblocks += 1;
        }
        let blk   = inode.direct[block_idx] as u64;
        let chunk = remaining.len().min(BLOCK_SIZE);
        let page  = Page::alloc();
        page.as_mut_slice()[..chunk].copy_from_slice(&remaining[..chunk]);
        write_block(ctrl, blk, &page);
        remaining  = &remaining[chunk..];
        block_idx += 1;
    }

    // Phase 2: single indirect block (logical blocks 12..12+1024)
    if !remaining.is_empty() {
        if inode.indirect == 0 {
            inode.indirect = alloc_block(ctrl);
            inode.nblocks += 1;
        }
        // Load indirect table (array of 1024 u32 block numbers)
        let ind_page = read_block(ctrl, inode.indirect as u64);
        let ind_buf  = ind_page.as_mut_slice();
        let mut ind_idx = 0usize;

        while !remaining.is_empty() && ind_idx < PTRS_PER_INDIRECT {
            let ptr_off = ind_idx * 4;
            let existing = u32::from_le_bytes([
                ind_buf[ptr_off], ind_buf[ptr_off+1],
                ind_buf[ptr_off+2], ind_buf[ptr_off+3],
            ]);
            let blk = if existing != 0 {
                existing
            } else {
                let b = alloc_block(ctrl);
                inode.nblocks += 1;
                ind_buf[ptr_off..ptr_off+4].copy_from_slice(&b.to_le_bytes());
                b
            };
            let chunk = remaining.len().min(BLOCK_SIZE);
            let page  = Page::alloc();
            page.as_mut_slice()[..chunk].copy_from_slice(&remaining[..chunk]);
            write_block(ctrl, blk as u64, &page);
            remaining = &remaining[chunk..];
            ind_idx  += 1;
        }
        // Flush indirect table (may have new block pointers written in)
        write_block(ctrl, inode.indirect as u64, &ind_page);
    }

    // Phase 3: double indirect (logical blocks 12+1024 .. 12+1024+1024×1024)
    if !remaining.is_empty() {
        if inode.dindirect == 0 {
            inode.dindirect = alloc_block(ctrl);
            inode.nblocks += 1;
        }
        let mut counter = 0usize;

        while !remaining.is_empty() {
            assert!(counter < DINDIRECT_MAX_BLOCKS, "file too large (max ~4GB)");
            let l1_idx = counter / PTRS_PER_INDIRECT;
            let l2_idx = counter % PTRS_PER_INDIRECT;

            // Get/alloc the level-1 indirect block pointer from the double-indirect table.
            let dind_page = read_block(ctrl, inode.dindirect as u64);
            let dind_buf  = dind_page.as_mut_slice();
            let l1_off = l1_idx * 4;
            let existing_l1 = u32::from_le_bytes(dind_buf[l1_off..l1_off+4].try_into().unwrap());
            let l1_blk = if existing_l1 != 0 {
                existing_l1
            } else {
                let b = alloc_block(ctrl);
                inode.nblocks += 1;
                dind_buf[l1_off..l1_off+4].copy_from_slice(&b.to_le_bytes());
                write_block(ctrl, inode.dindirect as u64, &dind_page);
                b
            };

            // Get/alloc the actual data block pointer from that level-1 indirect block.
            let l1_page = read_block(ctrl, l1_blk as u64);
            let l1_buf  = l1_page.as_mut_slice();
            let l2_off  = l2_idx * 4;
            let existing_data = u32::from_le_bytes(l1_buf[l2_off..l2_off+4].try_into().unwrap());
            let data_blk = if existing_data != 0 {
                existing_data
            } else {
                let b = alloc_block(ctrl);
                inode.nblocks += 1;
                l1_buf[l2_off..l2_off+4].copy_from_slice(&b.to_le_bytes());
                write_block(ctrl, l1_blk as u64, &l1_page);
                b
            };

            let chunk = remaining.len().min(BLOCK_SIZE);
            let page  = Page::alloc();
            page.as_mut_slice()[..chunk].copy_from_slice(&remaining[..chunk]);
            write_block(ctrl, data_blk as u64, &page);
            remaining = &remaining[chunk..];
            counter  += 1;
        }
    }

    inode.size = data.len() as u64;
    write_inode(ctrl, ino, &inode);
}

/// Remove a file or empty directory from its parent. Returns true on success.
pub fn remove(ctrl: &mut NvmeController, parent_ino: u32, name: &str) -> bool {
    let ino = match find_in_dir(ctrl, parent_ino, name) {
        Some(i) => i,
        None    => return false,
    };
    let inode = read_inode(ctrl, ino);

    // Don't remove root or non-empty dirs
    if ino == ROOT_INO { return false; }
    if inode.flags == F_DIR && !list_dir(ctrl, ino).is_empty() { return false; }

    // Free direct data blocks
    for &blk in inode.direct.iter().filter(|&&b| b != 0) {
        bitmap_set(ctrl, BLOCK_BM_BLOCK, blk as u64, false);
    }

    // Free indirect data blocks, then the indirect block itself
    if inode.indirect != 0 {
        let ind_page = read_block(ctrl, inode.indirect as u64);
        let ind_buf  = ind_page.as_slice();
        for i in 0..PTRS_PER_INDIRECT {
            let ptr_off = i * 4;
            let blk = u32::from_le_bytes([
                ind_buf[ptr_off], ind_buf[ptr_off+1],
                ind_buf[ptr_off+2], ind_buf[ptr_off+3],
            ]);
            if blk == 0 { break; }
            bitmap_set(ctrl, BLOCK_BM_BLOCK, blk as u64, false);
        }
        bitmap_set(ctrl, BLOCK_BM_BLOCK, inode.indirect as u64, false);
    }

    // Free double-indirect data blocks, level-1 indirect blocks, then the dindirect block itself
    if inode.dindirect != 0 {
        let dind_page = read_block(ctrl, inode.dindirect as u64);
        let dind_buf  = dind_page.as_slice();
        for l1_idx in 0..PTRS_PER_INDIRECT {
            let l1_off = l1_idx * 4;
            let l1_blk = u32::from_le_bytes(dind_buf[l1_off..l1_off+4].try_into().unwrap());
            if l1_blk == 0 { break; }
            let l1_page = read_block(ctrl, l1_blk as u64);
            let l1_buf  = l1_page.as_slice();
            for l2_idx in 0..PTRS_PER_INDIRECT {
                let l2_off = l2_idx * 4;
                let data_blk = u32::from_le_bytes(l1_buf[l2_off..l2_off+4].try_into().unwrap());
                if data_blk == 0 { break; }
                bitmap_set(ctrl, BLOCK_BM_BLOCK, data_blk as u64, false);
            }
            bitmap_set(ctrl, BLOCK_BM_BLOCK, l1_blk as u64, false);
        }
        bitmap_set(ctrl, BLOCK_BM_BLOCK, inode.dindirect as u64, false);
    }

    // Free inode
    bitmap_set(ctrl, INODE_BM_BLOCK, ino as u64, false);
    let mut freed = Inode::default();
    freed.flags = F_FREE;
    write_inode(ctrl, ino, &freed);

    // Remove dir entry from parent
    let parent = read_inode(ctrl, parent_ino);
    for &blk in parent.direct.iter().filter(|&&b| b != 0) {
        let page = read_block(ctrl, blk as u64);
        let buf  = page.as_mut_slice();
        for i in 0..ENTRIES_PER_BLK {
            let ep = unsafe { buf.as_mut_ptr().add(i * DIR_ENTRY_SIZE) as *mut DirEntry };
            if unsafe { (*ep).inode } == ino {
                unsafe { *ep = DirEntry { inode: 0, name_len: 0, name: [0; 27] }; }
                write_block(ctrl, blk as u64, &page);
                return true;
            }
        }
    }
    true
}

/// Lookup an entry by name in a directory. Returns (inode_id, is_dir).
pub fn stat(ctrl: &mut NvmeController, ino: u32) -> (bool, u64) {
    let inode = read_inode(ctrl, ino);
    (inode.flags == F_DIR, inode.size)
}

/// Read all data from a file. Returns a Vec<u8>.
pub fn read_file(ctrl: &mut NvmeController, ino: u32) -> Vec<u8> {
    let inode = read_inode(ctrl, ino);
    assert!(inode.flags == F_FILE, "not a file");
    let mut out = Vec::with_capacity(inode.size as usize);
    let mut left = inode.size as usize;

    // Phase 1: direct blocks
    for &blk in inode.direct.iter().filter(|&&b| b != 0) {
        if left == 0 { break; }
        let page  = read_block(ctrl, blk as u64);
        let chunk = left.min(BLOCK_SIZE);
        out.extend_from_slice(&page.as_slice()[..chunk]);
        left -= chunk;
    }

    // Phase 2: single indirect block
    if left > 0 && inode.indirect != 0 {
        let ind_page = read_block(ctrl, inode.indirect as u64);
        let ind_buf  = ind_page.as_slice();
        for i in 0..PTRS_PER_INDIRECT {
            if left == 0 { break; }
            let ptr_off = i * 4;
            let blk = u32::from_le_bytes([
                ind_buf[ptr_off], ind_buf[ptr_off+1],
                ind_buf[ptr_off+2], ind_buf[ptr_off+3],
            ]);
            if blk == 0 { break; }
            let page  = read_block(ctrl, blk as u64);
            let chunk = left.min(BLOCK_SIZE);
            out.extend_from_slice(&page.as_slice()[..chunk]);
            left -= chunk;
        }
    }

    // Phase 3: double indirect
    if left > 0 && inode.dindirect != 0 {
        let dind_page = read_block(ctrl, inode.dindirect as u64);
        let dind_buf  = dind_page.as_slice();
        'outer: for l1_idx in 0..PTRS_PER_INDIRECT {
            if left == 0 { break; }
            let l1_off = l1_idx * 4;
            let l1_blk = u32::from_le_bytes(dind_buf[l1_off..l1_off+4].try_into().unwrap());
            if l1_blk == 0 { break; }
            let l1_page = read_block(ctrl, l1_blk as u64);
            let l1_buf  = l1_page.as_slice();
            for l2_idx in 0..PTRS_PER_INDIRECT {
                if left == 0 { break 'outer; }
                let l2_off = l2_idx * 4;
                let data_blk = u32::from_le_bytes(l1_buf[l2_off..l2_off+4].try_into().unwrap());
                if data_blk == 0 { break; }
                let page  = read_block(ctrl, data_blk as u64);
                let chunk = left.min(BLOCK_SIZE);
                out.extend_from_slice(&page.as_slice()[..chunk]);
                left -= chunk;
            }
        }
    }

    out
}
