#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace AHCI (SATA) driver — third real driver migration,
// reusing the RTL8139/HDA pattern. The kernel (kernel/src/ahci.rs) does the
// one-time bring-up (PCI enable, ABAR mapping, port reset/CLB/FB/CTBA setup,
// IDENTIFY) — it needs `pmm`/PCI access no ring-3 process has — and then
// launches this process, handing it the physical address of a shared
// `Mailbox` page as its one launch argument. From then on this process runs
// forever, servicing one read/write request at a time: build the command
// FIS + PRDT, issue it, poll for completion, report the result back through
// the mailbox. `ahci::read_sectors()`/`write_sectors()` just write/read
// that mailbox now, never touching the hardware directly again.
//
// Unlike RTL8139/HDA (fire-and-forget async work polled once per frame),
// disk I/O here is a *synchronous* request from the kernel's perspective —
// `read_sectors()`/`write_sectors()` block (spin, relying on this project's
// real preemptive scheduler to actually run this process while they wait)
// until `status` goes non-zero. Transfers always land in the mailbox's own
// fixed 4KB `data` buffer (never the caller's original physical buffer —
// avoids granting this driver a fresh MMIO range on every single call) —
// this exactly matches HepFS's own block size, so no request ever needs
// more than one mailbox's worth of data.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/ahci.rs`'s `Mailbox` struct) — there's no shared crate
// between the two to enforce that, since userspace crates can't depend on
// kernel code at all (different target, no `std`, different address space).

use hepos_std::println;

// ── Port/HBA register offsets (byte offsets, same subset the in-kernel
// driver used before this migration) ──────────────────────────────────────
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_IS:  usize = 0x10;
const PORT_CI:  usize = 0x38;

const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;
const IS_TFES: u32 = 1 << 30;

const ATA_CMD_READ_DMA_EXT:  u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;

const CTBA_PRDT_OFFSET: usize = 128;
const ABAR_LEN: u64 = 0x1100;

#[repr(C)]
struct CmdHeader {
    dw0:   u16,
    prdtl: u16,
    prdbc: u32,
    ctba:  u32,
    ctbau: u32,
    _rsvd: [u32; 4],
}

#[repr(C)]
struct PrdtEntry {
    dba:   u32,
    dbau:  u32,
    _rsvd: u32,
    dbc:   u32,
}

#[repr(C)]
struct FisRegH2D {
    fis_type: u8,
    flags:    u8,
    command:  u8,
    featurel: u8,
    lba0: u8, lba1: u8, lba2: u8, device: u8,
    lba3: u8, lba4: u8, lba5: u8, featureh: u8,
    countl: u8, counth: u8,
    icc: u8, control: u8,
    _rsvd: [u8; 4],
}

#[repr(C)]
struct Mailbox {
    abar_phys:   u64,
    clb_phys:    u64,
    ctba_phys:   u64,
    port_base:   u32,
    sector_size: u32,
    op:          u32, // 0 = idle, 1 = read request, 2 = write request
    status:      u32, // 0 = pending, 1 = ok, 2 = error
    lba:         u64,
    count:       u32,
    _pad0:       u32,
    data:        [u8; 4096],
}

fn r32(base: *mut u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_volatile() }
}
fn w32(base: *mut u8, off: usize, v: u32) {
    unsafe { (base.add(off) as *mut u32).write_volatile(v) }
}

fn wait_not_busy(abar: *mut u8, port_base: usize) -> bool {
    for _ in 0..200_000_000u32 {
        if r32(abar, port_base + PORT_TFD) & (TFD_BSY | TFD_DRQ) == 0 { return true; }
        core::hint::spin_loop();
    }
    false
}

/// Build the command table (CFIS + one PRDT entry pointing at the mailbox's
/// own `data` buffer), issue slot 0, and poll for completion. Mirrors the
/// kernel's old `issue_command()` exactly, just driven by MMIO the kernel
/// mapped for us via `SYS_MMAP_MMIO` instead of direct register access.
fn issue_command(
    abar: *mut u8, port_base: usize, clb_virt: *mut u8, ctba_virt: *mut u8,
    fis: FisRegH2D, data_phys: u64, len: u32, write: bool,
) -> bool {
    unsafe { (ctba_virt as *mut FisRegH2D).write_volatile(fis); }

    let hdr_ptr = clb_virt as *mut CmdHeader;
    unsafe {
        let prdt_ptr = ctba_virt.add(CTBA_PRDT_OFFSET) as *mut PrdtEntry;
        prdt_ptr.write_volatile(PrdtEntry {
            dba: data_phys as u32, dbau: (data_phys >> 32) as u32,
            _rsvd: 0, dbc: len.saturating_sub(1),
        });
        (*hdr_ptr).dw0   = 5 | if write { 1 << 6 } else { 0 };
        (*hdr_ptr).prdtl = 1;
        (*hdr_ptr).prdbc = 0;
    }

    if !wait_not_busy(abar, port_base) {
        println!("ahcid: port busy, command not issued");
        return false;
    }
    w32(abar, port_base + PORT_CI, 1);

    for _ in 0..500_000_000u32 {
        let is = r32(abar, port_base + PORT_IS);
        if is & IS_TFES != 0 {
            w32(abar, port_base + PORT_IS, 0xFFFF_FFFF);
            return false;
        }
        if r32(abar, port_base + PORT_CI) & 1 == 0 { return true; }
        core::hint::spin_loop();
    }
    println!("ahcid: command timeout");
    false
}

fn lba_fis(command: u8, lba: u64, count: u16) -> FisRegH2D {
    FisRegH2D {
        fis_type: 0x27, flags: 0x80, command,
        featurel: 0,
        lba0: lba as u8, lba1: (lba >> 8) as u8, lba2: (lba >> 16) as u8,
        device: 0x40,
        lba3: (lba >> 24) as u8, lba4: (lba >> 32) as u8, lba5: (lba >> 40) as u8,
        featureh: 0,
        countl: count as u8, counth: (count >> 8) as u8,
        icc: 0, control: 0, _rsvd: [0; 4],
    }
}

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("ahcid: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("ahcid: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = &mut *(mb_va as *mut Mailbox);

    let abar_va = hepos_rt::sys_mmap_mmio(mb.abar_phys, ABAR_LEN);
    let clb_va  = hepos_rt::sys_mmap_mmio(mb.clb_phys, 4096);
    let ctba_va = hepos_rt::sys_mmap_mmio(mb.ctba_phys, 4096);
    if abar_va == 0 || clb_va == 0 || ctba_va == 0 {
        println!("ahcid: failed to map ABAR/CLB/CTBA — exiting");
        hepos_rt::sys_exit(1);
    }
    let abar = abar_va as *mut u8;
    let clb_virt  = clb_va as *mut u8;
    let ctba_virt = ctba_va as *mut u8;
    let port_base = mb.port_base as usize;
    let sector_size = mb.sector_size;
    let data_phys = mailbox_phys + core::mem::offset_of!(Mailbox, data) as u64;

    println!("ahcid: ready (port_base {:#x})", port_base);

    loop {
        let op = core::ptr::read_volatile(&mb.op);
        if op != 0 {
            let lba   = core::ptr::read_volatile(&mb.lba);
            let count = core::ptr::read_volatile(&mb.count);
            let len   = count * sector_size;
            let write = op == 2;
            let fis = lba_fis(
                if write { ATA_CMD_WRITE_DMA_EXT } else { ATA_CMD_READ_DMA_EXT },
                lba, count as u16);
            let ok = issue_command(abar, port_base, clb_virt, ctba_virt, fis, data_phys, len, write);
            core::ptr::write_volatile(&mut mb.status, if ok { 1 } else { 2 });
            core::ptr::write_volatile(&mut mb.op, 0);
        }

        // Rate-limit to once per timer tick instead of spinning at 100% CPU —
        // same reasoning as rtl8139d/hdad (see their doc comments): a driver
        // process that never yields starves the host's own I/O handling
        // under this project's single-vCPU TCG emulation.
        hepos_rt::sys_wait_irq(0x20);
    }
}
