//! AHCI (SATA) driver.
//!
//! Detects an AHCI HBA (PCI class 0x01/0x06), maps its ABAR (BAR5), enables
//! AHCI mode, finds the first port with a plain SATA disk attached, and
//! drives it with a single command slot — no NCQ, no interrupts, everything
//! polled with a bounded spin count (never `panic!()` on a timeout; see
//! PLAN.md's Known Issues for why an unbounded/under-budgeted timeout during
//! boot-time driver init is dangerous — this driver returns/logs on failure
//! instead so a flaky or absent AHCI controller can never hang the boot).
//!
//! Public API mirrors `nvme.rs`'s shape (init, then read/write by LBA):
//!   `init(devs)` — call once during boot; returns true if a usable SATA
//!                  disk was found and the port was brought up.
//!   `read_sectors(lba, count, buf_phys)` / `write_sectors(...)` — LBA48 DMA,
//!                  one contiguous physical buffer per call (no scatter-gather).
//!
//! Not yet wired into HepFS (which is NVMe-only today) — see PLAN.md.
//!
//! **Third driver migrated to userspace**, reusing the RTL8139/HDA pattern:
//! the one-time bring-up above (PCI enable, ABAR mapping, port reset,
//! CLB/FB/CTBA allocation, IDENTIFY) stays here in the kernel — it needs
//! `pmm`/PCI access no ring-3 process has — but the *ongoing* per-request
//! command-issue-and-poll work now runs in a persistent userspace process,
//! `userspace/ahcid`, talking to this module through a shared-memory
//! `Mailbox` page. Unlike RTL8139/HDA (fire-and-forget async work polled
//! once per frame), disk I/O is a *synchronous* request from every caller's
//! perspective — `read_sectors()`/`write_sectors()` keep their exact old
//! blocking signatures, just spin-waiting on the mailbox's `status` field
//! instead of driving the hardware directly, relying on this project's real
//! preemptive scheduler to actually run `ahcid` while they wait.

use spin::Mutex;
use crate::{paging, pci, pmm, process, serial, syscall, vmm};

// ── HBA (global) registers — byte offsets from ABAR ───────────────────────────
const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_IS:  usize = 0x08;
const HBA_PI:  usize = 0x0C;

const GHC_AE: u32 = 1 << 31;

// ── Port registers — byte offsets from a port's own base (0x100 + n*0x80) ────
const PORT_CLB:  usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB:   usize = 0x08;
const PORT_FBU:  usize = 0x0C;
const PORT_IS:   usize = 0x10;
const PORT_CMD:  usize = 0x18;
const PORT_TFD:  usize = 0x20;
const PORT_SIG:  usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SERR: usize = 0x30;
const PORT_CI:   usize = 0x38;

const CMD_ST:  u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR:  u32 = 1 << 14;
const CMD_CR:  u32 = 1 << 15;

const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;

const IS_TFES: u32 = 1 << 30; // Task File Error Status

const SIG_ATA: u32 = 0x0000_0101; // plain SATA disk (not ATAPI/PM/enclosure)

const ATA_CMD_READ_DMA_EXT:  u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_IDENTIFY:      u8 = 0xEC;

fn r32(base: *mut u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_volatile() }
}
fn w32(base: *mut u8, off: usize, v: u32) {
    unsafe { (base.add(off) as *mut u32).write_volatile(v) }
}

// ── On-disk (well, on-HBA) structures ─────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct CmdHeader {
    dw0:   u16, // CFL[4:0] | A<<5 | W<<6 | P<<7 | R<<8 | B<<9 | C<<10 | rsvd | PMP[15:12]
    prdtl: u16, // PRDT entry count
    prdbc: u32, // bytes transferred (written by HBA)
    ctba:  u32,
    ctbau: u32,
    _rsvd: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PrdtEntry {
    dba:   u32,
    dbau:  u32,
    _rsvd: u32,
    dbc:   u32, // bits[21:0] = byte count - 1 (must be even); bit31 = interrupt-on-completion
}

// Register Host-to-Device FIS (20 bytes) — the command we send the drive.
#[repr(C)]
#[derive(Clone, Copy)]
struct FisRegH2D {
    fis_type: u8, // 0x27
    flags:    u8, // bit7 = C (1 = this is a command, not a control update)
    command:  u8,
    featurel: u8,
    lba0: u8, lba1: u8, lba2: u8, device: u8,
    lba3: u8, lba4: u8, lba5: u8, featureh: u8,
    countl: u8, counth: u8,
    icc: u8, control: u8,
    _rsvd: [u8; 4],
}

// Command-table layout: 64B CFIS + 16B ACMD + 48B reserved = 128B header,
// then the PRDT starting at offset 128. We only ever use 1 PRDT entry (one
// contiguous physical buffer per call), so a single 4K page is far more than
// enough room.
const CTBA_PRDT_OFFSET: usize = 128;

struct AhciPort {
    abar:      *mut u8,
    abar_phys: u64,
    port_base: usize,
    clb_phys:  u64,
    ctba_virt: *mut u8,
    ctba_phys: u64,
}
unsafe impl Send for AhciPort {}

/// Shared memory mailbox — one physical page, mapped into both the kernel
/// (via `vmm::phys_to_virt`) and the `ahcid` userspace process (via
/// `SYS_MMAP_MMIO`, using the physical address handed to it as its one
/// launch argument). **Layout must stay byte-for-byte identical to the copy
/// in `userspace/ahcid/src/main.rs`** — there's no shared crate between the
/// two to enforce that, since userspace crates can't depend on kernel code
/// (different target, different address space, no `std`).
#[repr(C)]
struct Mailbox {
    abar_phys:   u64,
    clb_phys:    u64,
    ctba_phys:   u64,
    port_base:   u32,
    sector_size: u32,
    /// 0 = idle, 1 = read request, 2 = write request. Kernel writes this
    /// last (after `lba`/`count`, and after `data` for a write) to hand off
    /// a request; `ahcid` clears it back to 0 once `status` is final.
    op:     u32,
    /// 0 = request still pending, 1 = done ok, 2 = done with an error.
    /// Kernel spin-waits on this going non-zero, then clears it back to 0
    /// once it's consumed the result.
    status: u32,
    lba:    u64,
    count:  u32,
    _pad0:  u32,
    /// Fixed bounce buffer both sides DMA through — sized to exactly match
    /// `hepfs::BLOCK_SIZE` (4096 bytes = 8 sectors), the only transfer size
    /// any real caller ever uses, so no request needs more than one
    /// mailbox's worth of data and `ahcid` never needs a fresh MMIO grant
    /// for the caller's own (arbitrary, per-call) physical buffer.
    data: [u8; 4096],
}

/// `Mailbox`'s header fields plus its 4096-byte `data` buffer add up to just
/// over one page — needs 2 contiguous physical pages, not 1.
const MAILBOX_PAGES: usize = 2;
const _: () = assert!(core::mem::size_of::<Mailbox>() <= MAILBOX_PAGES * 4096);

pub struct AhciController {
    port:            AhciPort,
    pub sectors:     u64,
    pub sector_size: u32,
    mailbox:         Option<*mut Mailbox>,
}
unsafe impl Send for AhciController {}

pub static CONTROLLER: Mutex<Option<AhciController>> = Mutex::new(None);

pub fn is_available() -> bool { CONTROLLER.lock().is_some() }

/// Mailbox physical address waiting to be handed to a freshly-spawned
/// `ahcid` task, set by `init()` and consumed once by
/// `spawn_pending_driver()` — same deferred-spawn pattern as
/// `rtl8139::spawn_pending_driver()`/`hda::spawn_pending_driver()` (`init()`
/// runs too early, before the scheduler's idle/blink tasks are registered,
/// for `scheduler::spawn()` to be safe to call directly).
static PENDING_DRIVER_MAILBOX: Mutex<Option<u64>> = Mutex::new(None);

/// Launches the queued `ahcid` driver process, if `init()` found a disk and
/// queued one. Must be called only after the scheduler's idle/blink tasks
/// are registered and the timer is running (i.e. from within `task_blink`'s
/// own loop). A no-op on every call after the first (or if nothing was ever
/// queued).
pub fn spawn_pending_driver() {
    let Some(mailbox_phys) = PENDING_DRIVER_MAILBOX.lock().take() else { return };
    match process::exec_async_with_arg(usize::MAX, "<ahcid>", AHCID_ELF, mailbox_phys) {
        Ok(()) => serial::print("ahci: ahcid launched\n"),
        Err(_) => serial::print("ahci: ahcid launch failed\n"),
    }
}

// Baked-in ahcid ELF (generated by build.rs from userspace/target/.../ahcid).
// Empty slice if userspace hasn't been rebuilt since this driver was added.
include!(concat!(env!("OUT_DIR"), "/ahcid_elf.rs"));

// ── Init ──────────────────────────────────────────────────────────────────────

pub fn init(devs: &[pci::PciDevice]) -> bool {
    let dev = match devs.iter().find(|d| d.class == 0x01 && d.subclass == 0x06) {
        Some(d) => d,
        None => { serial::print("AHCI: no controller found\n"); return false; }
    };
    serial::print("AHCI: found controller\n");

    // Enable Memory Space + Bus Mastering
    let cmd = pci::config_read16(dev.bus, dev.dev, dev.func, 0x04);
    pci::config_write32(dev.bus, dev.dev, dev.func, 0x04, (cmd | 0x06) as u32);

    // ABAR is BAR5, always 32-bit per the AHCI spec (no 64-bit-pair handling needed).
    let bar5 = dev.bar(5);
    let abar_phys = (bar5 & !0xF) as u64;
    if abar_phys == 0 { serial::print("AHCI: no ABAR\n"); return false; }

    // Covers the HBA registers plus port registers up to port 31.
    let abar = paging::map_mmio(abar_phys, 0x1100);
    serial::print("AHCI: ABAR mapped\n");

    // Enable AHCI mode (some controllers reset with AE already set; harmless either way).
    let ghc = r32(abar, HBA_GHC);
    w32(abar, HBA_GHC, ghc | GHC_AE);

    let pi = r32(abar, HBA_PI);
    let mut found: Option<u8> = None;
    for i in 0..32u32 {
        if pi & (1 << i) == 0 { continue; }
        let port_base = 0x100 + i as usize * 0x80;
        let ssts = r32(abar, port_base + PORT_SSTS);
        let det  = ssts & 0xF;
        let ipm  = (ssts >> 8) & 0xF;
        if det != 3 || ipm != 1 { continue; } // device present + PHY communication established
        let sig = r32(abar, port_base + PORT_SIG);
        if sig != SIG_ATA { continue; } // skip ATAPI/port-multiplier/enclosure signatures
        found = Some(i as u8);
        break;
    }
    let port_num = match found {
        Some(p) => p,
        None => { serial::print("AHCI: no SATA disk on any implemented port\n"); return false; }
    };
    serial::print_hex("AHCI: SATA disk on port", port_num as u64);
    serial::print("\n");

    let port_base = 0x100 + port_num as usize * 0x80;

    // Stop the command engine before reprogramming CLB/FB (spec requirement).
    if !stop_port(abar, port_base) {
        serial::print("AHCI: port didn't stop cleanly\n");
        return false;
    }

    let clb_phys = match pmm::alloc_page() { Some(p) => p, None => { serial::print("AHCI: OOM (CLB)\n"); return false; } };
    let fb_phys  = match pmm::alloc_page() { Some(p) => p, None => { serial::print("AHCI: OOM (FB)\n");  return false; } };
    let ctba_phys = match pmm::alloc_page() { Some(p) => p, None => { serial::print("AHCI: OOM (CTBA)\n"); return false; } };
    unsafe {
        core::ptr::write_bytes(vmm::phys_to_virt(clb_phys), 0, 4096);
        core::ptr::write_bytes(vmm::phys_to_virt(fb_phys), 0, 4096);
        core::ptr::write_bytes(vmm::phys_to_virt(ctba_phys), 0, 4096);
    }

    w32(abar, port_base + PORT_CLB,  clb_phys as u32);
    w32(abar, port_base + PORT_CLBU, (clb_phys >> 32) as u32);
    w32(abar, port_base + PORT_FB,   fb_phys as u32);
    w32(abar, port_base + PORT_FBU,  (fb_phys >> 32) as u32);

    // Point command slot 0's header at our one command table.
    let hdr0 = vmm::phys_to_virt(clb_phys) as *mut CmdHeader;
    unsafe {
        (*hdr0).ctba  = ctba_phys as u32;
        (*hdr0).ctbau = (ctba_phys >> 32) as u32;
        (*hdr0).prdtl = 0;
    }

    // Clear any stale error/interrupt status, then bring the port back up:
    // FIS receive enable first, then start the command engine.
    w32(abar, port_base + PORT_SERR, 0xFFFF_FFFF);
    w32(abar, port_base + PORT_IS,   0xFFFF_FFFF);
    w32(abar, port_base + PORT_CMD, r32(abar, port_base + PORT_CMD) | CMD_FRE);
    w32(abar, port_base + PORT_CMD, r32(abar, port_base + PORT_CMD) | CMD_ST);

    let mut ctrl = AhciController {
        port: AhciPort {
            abar, abar_phys, port_base,
            clb_phys, ctba_virt: vmm::phys_to_virt(ctba_phys), ctba_phys,
        },
        sectors: 0,
        sector_size: 512,
        mailbox: None,
    };

    if !identify(&mut ctrl) {
        serial::print("AHCI: IDENTIFY failed\n");
        return false;
    }
    serial::print_hex("AHCI: sectors", ctrl.sectors);
    serial::print("\n");

    // Shared mailbox — the only thing the kernel and the `ahcid` userspace
    // process both touch going forward. Its header fields plus the fixed
    // 4096-byte `data` bounce buffer add up to just over one page, so this
    // needs `alloc_contiguous(MAILBOX_PAGES)`, not a single `alloc_page()`
    // (which is what RTL8139/HDA's smaller mailboxes could get away with).
    if let Some(mailbox_phys) = pmm::alloc_contiguous(MAILBOX_PAGES) {
        let mailbox_virt = vmm::phys_to_virt(mailbox_phys) as *mut Mailbox;
        unsafe {
            core::ptr::write_bytes(mailbox_virt as *mut u8, 0, MAILBOX_PAGES * 4096);
            (*mailbox_virt).abar_phys   = abar_phys;
            (*mailbox_virt).clb_phys    = clb_phys;
            (*mailbox_virt).ctba_phys   = ctba_phys;
            (*mailbox_virt).port_base   = port_base as u32;
            (*mailbox_virt).sector_size = ctrl.sector_size;
        }
        ctrl.mailbox = Some(mailbox_virt);

        // Grant the runtime-discovered ranges ahcid needs — the kernel's
        // fixed compile-time allowlist obviously can't cover a PCI BAR or
        // pmm-allocated pages whose addresses are only known now.
        syscall::grant_mmio_range(abar_phys, 0x1100);
        syscall::grant_mmio_range(clb_phys, 4096);
        syscall::grant_mmio_range(ctba_phys, 4096);
        syscall::grant_mmio_range(mailbox_phys, (MAILBOX_PAGES * 4096) as u64);

        if AHCID_ELF.is_empty() {
            serial::print("ahci: ahcid ELF not built (run `cargo build --release` in userspace/) — disk R/W will not work\n");
        } else {
            // Don't spawn the driver task here — `init()` runs during early
            // hardware bring-up, before `kmain` registers the scheduler's
            // idle/blink tasks (see `spawn_pending_driver()`'s doc comment).
            *PENDING_DRIVER_MAILBOX.lock() = Some(mailbox_phys);
            serial::print("ahci: ahcid queued to launch once the scheduler is up\n");
        }
    } else {
        serial::print("ahci: mailbox OOM — disk R/W will not work\n");
    }

    *CONTROLLER.lock() = Some(ctrl);
    serial::print("AHCI: init OK\n");
    true
}

/// Clear ST and FRE, then wait for CR and FR to actually drop — spec requires
/// this before touching CLB/FB. Bounded: returns false rather than hanging if
/// the controller never clears them (matches the NVMe timeout-bug fix
/// elsewhere in this codebase — under-budgeted *or* unbounded polling loops
/// during boot-time driver init are exactly the class of bug that freezes
/// the whole OS on the splash screen).
fn stop_port(abar: *mut u8, port_base: usize) -> bool {
    let cmd = r32(abar, port_base + PORT_CMD);
    w32(abar, port_base + PORT_CMD, cmd & !(CMD_ST | CMD_FRE));
    for _ in 0..50_000_000u32 {
        if r32(abar, port_base + PORT_CMD) & (CMD_CR | CMD_FR) == 0 { return true; }
        core::hint::spin_loop();
    }
    false
}

/// Wait for BSY and DRQ to clear (drive ready for a new command). Bounded.
fn wait_not_busy(abar: *mut u8, port_base: usize) -> bool {
    for _ in 0..200_000_000u32 {
        if r32(abar, port_base + PORT_TFD) & (TFD_BSY | TFD_DRQ) == 0 { return true; }
        core::hint::spin_loop();
    }
    false
}

/// Build the command-table's CFIS + (optional) single PRDT entry, issue slot
/// 0, and poll for completion. Bounded — returns false on timeout or a
/// task-file error instead of hanging or panicking.
fn issue_command(ctrl: &AhciController, fis: FisRegH2D, xfer: Option<(u64, u32)>, write: bool) -> bool {
    let port = &ctrl.port;
    unsafe { (port.ctba_virt as *mut FisRegH2D).write_volatile(fis); }

    // CLB lives wherever we mapped it at init — re-derive the header pointer
    // from the port's own CLB register rather than caching it, so this stays
    // correct even though we only ever use slot 0.
    let clb_lo = r32(port.abar, port.port_base + PORT_CLB) as u64;
    let clb_hi = r32(port.abar, port.port_base + PORT_CLBU) as u64;
    let clb_phys = (clb_hi << 32) | clb_lo;
    let hdr_ptr = vmm::phys_to_virt(clb_phys) as *mut CmdHeader;

    let prdt_count: u16 = if xfer.is_some() { 1 } else { 0 };
    unsafe {
        if let Some((phys, len)) = xfer {
            let prdt_ptr = port.ctba_virt.add(CTBA_PRDT_OFFSET) as *mut PrdtEntry;
            prdt_ptr.write_volatile(PrdtEntry {
                dba: phys as u32, dbau: (phys >> 32) as u32,
                _rsvd: 0, dbc: len.saturating_sub(1),
            });
        }
        (*hdr_ptr).dw0   = 5 /* CFL = 5 DWORDS (20-byte Register H2D FIS) */
            | if write { 1 << 6 } else { 0 };
        (*hdr_ptr).prdtl = prdt_count;
        (*hdr_ptr).prdbc = 0;
    }

    if !wait_not_busy(port.abar, port.port_base) {
        serial::print("AHCI: port busy, command not issued\n");
        return false;
    }
    w32(port.abar, port.port_base + PORT_CI, 1);

    for _ in 0..500_000_000u32 {
        let is = r32(port.abar, port.port_base + PORT_IS);
        if is & IS_TFES != 0 {
            serial::print("AHCI: task file error\n");
            w32(port.abar, port.port_base + PORT_IS, 0xFFFF_FFFF);
            return false;
        }
        if r32(port.abar, port.port_base + PORT_CI) & 1 == 0 { return true; }
        core::hint::spin_loop();
    }
    serial::print("AHCI: command timeout\n");
    false
}

fn lba_fis(command: u8, lba: u64, count: u16) -> FisRegH2D {
    FisRegH2D {
        fis_type: 0x27, flags: 0x80 /* C = 1 (command) */, command,
        featurel: 0,
        lba0: lba as u8, lba1: (lba >> 8) as u8, lba2: (lba >> 16) as u8,
        device: 0x40, // LBA mode
        lba3: (lba >> 24) as u8, lba4: (lba >> 32) as u8, lba5: (lba >> 40) as u8,
        featureh: 0,
        countl: count as u8, counth: (count >> 8) as u8,
        icc: 0, control: 0, _rsvd: [0; 4],
    }
}

fn identify(ctrl: &mut AhciController) -> bool {
    let buf_phys = match pmm::alloc_page() { Some(p) => p, None => return false };
    unsafe { core::ptr::write_bytes(vmm::phys_to_virt(buf_phys), 0, 512); }

    let fis = FisRegH2D {
        fis_type: 0x27, flags: 0x80, command: ATA_CMD_IDENTIFY,
        featurel: 0, lba0: 0, lba1: 0, lba2: 0, device: 0,
        lba3: 0, lba4: 0, lba5: 0, featureh: 0,
        countl: 0, counth: 0, icc: 0, control: 0, _rsvd: [0; 4],
    };
    let ok = issue_command(ctrl, fis, Some((buf_phys, 512)), false);
    if !ok { pmm::free_page(buf_phys); return false; }

    let words = vmm::phys_to_virt(buf_phys) as *const u16;
    unsafe {
        let word83  = words.add(83).read_volatile();
        let lba48   = word83 & (1 << 10) != 0;
        if lba48 {
            let mut sectors: u64 = 0;
            for i in 0..4u64 { sectors |= (words.add(100 + i as usize).read_volatile() as u64) << (16 * i); }
            ctrl.sectors = sectors;
        } else {
            let lo = words.add(60).read_volatile() as u64;
            let hi = words.add(61).read_volatile() as u64;
            ctrl.sectors = (hi << 16) | lo;
        }
    }
    pmm::free_page(buf_phys);
    true
}

/// Number of spin iterations to wait for `ahcid` to service one mailbox
/// request. Generous — the driver only polls once per ~10ms timer tick (see
/// `userspace/ahcid`'s `SYS_WAIT_IRQ` rate-limiting), and this loop itself
/// does nothing to *cause* that tick; it just needs to not give up before
/// several real ticks (and therefore several scheduling opportunities for
/// `ahcid`) have had a chance to happen.
const MAILBOX_WAIT_SPINS: u32 = 500_000_000;

/// Serializes whole read/write transactions (request write → spin-wait →
/// result copy) against each other. There's only one command slot and one
/// mailbox, so two concurrent callers interleaving their request fields (or
/// one stomping the other's still-in-flight `data` buffer) would silently
/// corrupt a transfer — held for the *entire* transaction, unlike
/// `CONTROLLER`'s own lock, which is only ever taken briefly to read out
/// `mailbox`/`sector_size` (never across the spin-wait, so an unrelated
/// caller checking `is_available()`/`sectors` mid-transfer doesn't block on
/// a disk operation that might still be spinning for a while).
static AHCI_IO_LOCK: Mutex<()> = Mutex::new(());

fn mailbox_and_sector_size() -> Option<(*mut Mailbox, u32)> {
    let guard = CONTROLLER.lock();
    let ctrl = guard.as_ref()?;
    Some((ctrl.mailbox?, ctrl.sector_size))
}

/// Read `count` sectors (max one `hepfs::BLOCK_SIZE`'s worth — 8 sectors —
/// per call, the mailbox's fixed bounce buffer) starting at `lba` into the
/// contiguous physical buffer `buf_phys`. Returns false on any failure (no
/// disk/driver ready, timeout, task-file error, or a request too large).
pub fn read_sectors(lba: u64, count: u16, buf_phys: u64) -> bool {
    let _io = AHCI_IO_LOCK.lock();
    let Some((mb, sector_size)) = mailbox_and_sector_size() else { return false; };
    let len = count as u32 * sector_size;
    if len == 0 || len as usize > 4096 { return false; }

    unsafe {
        core::ptr::write_volatile(&mut (*mb).lba, lba);
        core::ptr::write_volatile(&mut (*mb).count, count as u32);
        core::ptr::write_volatile(&mut (*mb).status, 0);
        core::ptr::write_volatile(&mut (*mb).op, 1); // release — hands off to ahcid
    }
    let mut ok = false;
    for _ in 0..MAILBOX_WAIT_SPINS {
        if unsafe { core::ptr::read_volatile(&(*mb).status) } != 0 {
            ok = unsafe { core::ptr::read_volatile(&(*mb).status) } == 1;
            break;
        }
        core::hint::spin_loop();
    }
    if ok {
        unsafe {
            core::ptr::copy_nonoverlapping((*mb).data.as_ptr(), vmm::phys_to_virt(buf_phys), len as usize);
        }
    } else if unsafe { core::ptr::read_volatile(&(*mb).status) } == 0 {
        serial::print("ahci: mailbox request timeout\n");
    }
    unsafe { core::ptr::write_volatile(&mut (*mb).status, 0); }
    ok
}

/// Write `count` sectors starting at `lba` from the contiguous physical
/// buffer `buf_phys`. Same bounds/failure behavior as `read_sectors`.
pub fn write_sectors(lba: u64, count: u16, buf_phys: u64) -> bool {
    let _io = AHCI_IO_LOCK.lock();
    let Some((mb, sector_size)) = mailbox_and_sector_size() else { return false; };
    let len = count as u32 * sector_size;
    if len == 0 || len as usize > 4096 { return false; }

    unsafe {
        core::ptr::copy_nonoverlapping(vmm::phys_to_virt(buf_phys), (*mb).data.as_mut_ptr(), len as usize);
        core::ptr::write_volatile(&mut (*mb).lba, lba);
        core::ptr::write_volatile(&mut (*mb).count, count as u32);
        core::ptr::write_volatile(&mut (*mb).status, 0);
        core::ptr::write_volatile(&mut (*mb).op, 2); // release — hands off to ahcid
    }
    let mut ok = false;
    for _ in 0..MAILBOX_WAIT_SPINS {
        let status = unsafe { core::ptr::read_volatile(&(*mb).status) };
        if status != 0 { ok = status == 1; break; }
        core::hint::spin_loop();
    }
    if !ok && unsafe { core::ptr::read_volatile(&(*mb).status) } == 0 {
        serial::print("ahci: mailbox request timeout\n");
    }
    unsafe { core::ptr::write_volatile(&mut (*mb).status, 0); }
    ok
}
