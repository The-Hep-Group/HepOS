//! NVMe driver.
//!
//! **Fifth driver migrated to userspace, and the highest-stakes one** — this
//! is the actual HepFS boot disk, unlike AHCI (migrated earlier, but never
//! actually mounted by HepFS in this build). The one-time bring-up (PCI
//! enable, MMIO mapping, controller disable/enable, admin queue setup,
//! Identify Controller/Namespace, I/O queue creation) stays entirely in the
//! kernel, unchanged — all of it runs on the *admin* queue, which this
//! migration doesn't touch at all.
//!
//! **A constraint none of the previous 4 migrations had**: `read_blocks()`/
//! `write_blocks()` are called synchronously during early boot (HepFS
//! format/mount, `kernel.txt`/demo file writes, desktop icon sync) — all of
//! it *before* the scheduler even exists, let alone a userspace process.
//! `nvmed` can only launch once `task_blink`'s own loop starts (same
//! deferred-spawn constraint every other driver has), which happens *after*
//! all of that early-boot I/O already completed. So this module keeps the
//! original, fully-synchronous in-kernel I/O-queue implementation as
//! `read_blocks_direct()`/`write_blocks_direct()` — early boot uses these
//! directly (there's no other option), and `spawn_pending_driver()` hands
//! off to `nvmed` only once, reading the I/O queue's *live* software
//! position (`sq_tail`/`cq_head`/`phase`) at that exact moment (not a
//! boot-time snapshot) so the handoff is seamless regardless of how much
//! direct-path I/O already happened. From that point on, `read_blocks()`/
//! `write_blocks()` (same public signatures, zero caller changes) route
//! through `nvmed`'s mailbox instead.
//!
//! **Also unlike AHCI**: NVMe's submission/completion queue doorbell
//! registers are write-only per spec — there's no hardware register to
//! recover `sq_tail`/`cq_head`/`phase` from after a `nvmed` restart (unlike
//! RTL8139's readable `CAPR`). So `nvmed` persists its own live queue
//! position back into the mailbox every loop iteration, the same fix
//! `xhcid` needed for its event/HID rings — this was built in from the
//! start here rather than discovered the hard way again.

use crate::{paging, pci, pmm, process, serial, syscall, vmm};
use core::sync::atomic::{fence, AtomicBool, Ordering};

// â"€â"€ NVMe register offsets â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€
const REG_CAP:  usize = 0x00;
const REG_VS:   usize = 0x08;
const REG_CC:   usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA:  usize = 0x24;
const REG_ASQ:  usize = 0x28;
const REG_ACQ:  usize = 0x30;

// â"€â"€ Queue depth â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€
const QD: usize = 64; // entries per queue (must fit in one page each)

// â"€â"€ NVMe submission queue entry (64 bytes) â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€
#[repr(C, align(64))]
#[derive(Clone, Copy, Default)]
struct SqEntry {
    cdw0:  u32,  // opcode[7:0] | flags[15:8] | cid[31:16]
    nsid:  u32,
    cdw2:  u32, cdw3: u32,
    mptr:  u64,
    prp1:  u64,
    prp2:  u64,
    cdw10: u32, cdw11: u32, cdw12: u32,
    cdw13: u32, cdw14: u32, cdw15: u32,
}

// â"€â"€ NVMe completion queue entry (16 bytes) â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct CqEntry {
    cdw0:    u32,
    _rsvd:   u32,
    sq_head: u16,
    sq_id:   u16,
    cid:     u16,
    status:  u16, // bit0 = phase, bits[15:1] = status code
}

// ── Identify Namespace structure (relevant fields) ───────────────────────────
// Per NVMe spec: NSZE/NCAP/NUSE/NSFEAT/NLBAF/FLBAS at offsets 0-26, then the
// LBAF (LBA Format) array starts at byte offset 128 — not 108, which the
// previous version of this struct had wrong (never noticed because this
// struct was never actually populated: Identify was only ever called with
// CNS=1 for Identify *Controller*, never CNS=0 for Identify *Namespace*).
#[repr(C)]
struct IdNs {
    nsze:   u64, // namespace size in blocks
    ncap:   u64,
    nuse:   u64,
    nsfeat: u8,
    nlbaf:  u8,  // number of LBA formats - 1
    flbas:  u8,  // current LBA format index (bits[3:0])
    _pad:   [u8; 128 - 27],
    lbaf:   [u32; 16], // LBA format descriptors — bits[23:16] of each = LBADS (2^LBADS byte size)
}

struct Queue {
    sq:       *mut SqEntry,
    cq:       *mut CqEntry,
    sq_phys:  u64,
    cq_phys:  u64,
    sq_tail:  u32,
    cq_head:  u32,
    phase:    bool,
    db_sq:    *mut u32,
    db_cq:    *mut u32,
}

unsafe impl Send for Queue {}

/// Shared memory mailbox — one physical page, mapped into both the kernel
/// (via `vmm::phys_to_virt`) and the `nvmed` userspace process (via
/// `SYS_MMAP_MMIO`, using the physical address handed to it as its one
/// launch argument). **Layout must stay byte-for-byte identical to the copy
/// in `userspace/nvmed/src/main.rs`.**
#[repr(C)]
struct Mailbox {
    regs_phys:  u64,
    io_sq_phys: u64,
    io_cq_phys: u64,
    dstrd:      u32,
    qd:         u32,
    /// Live I/O-queue software position, read once by `spawn_pending_driver()`
    /// from the kernel's own in-progress `Queue` state at the moment of
    /// handoff (not a boot-time snapshot — see the module doc comment for
    /// why that distinction matters). `nvmed` writes these back every loop
    /// iteration so a `service stop`/`start` cycle resumes from wherever it
    /// actually left off — NVMe's SQ/CQ doorbells are write-only per spec,
    /// so unlike RTL8139's `CAPR` there's no hardware register to recover
    /// this from after a restart.
    sq_tail: u32,
    cq_head: u32,
    phase:   u32, // bool as u32
    /// 0 = idle, 1 = read request, 2 = write request. Kernel writes this
    /// last (after `lba`/`count`, and after `data` for a write) to hand off
    /// a request; `nvmed` clears it back to 0 once `status` is final.
    op:     u32,
    /// 0 = request still pending, 1 = done ok, 2 = done with an error.
    status: u32,
    lba:    u64,
    count:  u32,
    _pad0:  u32,
    /// Fixed bounce buffer both sides DMA through — sized to exactly match
    /// `hepfs::BLOCK_SIZE` (4096 bytes), the only transfer size any real
    /// caller ever uses (see `hepfs.rs`'s `spb`/`read_blocks`/`write_blocks`
    /// call sites) — same reasoning as `ahci.rs`'s identical bounce buffer.
    data: [u8; 4096],
    /// 0 = keep running. Kernel writes 1 to request a cooperative shutdown
    /// (`service stop nvmed` / `kill <pid>`) — see `stop_service()`'s doc
    /// comment for why this is cooperative rather than a true forced kill.
    stop: u32,
}

/// `Mailbox`'s header fields plus its 4096-byte `data` buffer add up to just
/// over one page — needs 2 contiguous physical pages, not 1 (same lesson
/// learned the hard way with `ahci.rs`'s identically-shaped mailbox).
const MAILBOX_PAGES: usize = 2;
const _: () = assert!(core::mem::size_of::<Mailbox>() <= MAILBOX_PAGES * 4096);

pub struct NvmeController {
    regs:       *mut u8,
    regs_phys:  u64,
    dstrd:      usize,
    admin:      Queue,
    io:         Queue,
    cid:        u16,
    pub lba_size:  u32,
    pub lba_count: u64,
    /// `Some` exactly when `nvmed` is the current authority over the I/O
    /// queue — `read_blocks()`/`write_blocks()` check this every call.
    /// `None` means the in-kernel direct path (`self.io`) is authoritative,
    /// which is true at boot and again any time `nvmed` is stopped (see
    /// `stop_service()` — it syncs `self.io`'s position back from the
    /// mailbox before clearing this, so falling back is always safe).
    mailbox: Option<*mut Mailbox>,
    /// The mailbox page itself, allocated once on the very first handoff
    /// and reused for every subsequent stop/start cycle (not reallocated —
    /// `nvmed`'s launch argument, and every grant, always refers to the
    /// same physical page for this controller's whole lifetime).
    mailbox_page_phys: Option<u64>,
}

unsafe impl Send for NvmeController {}
unsafe impl Sync for NvmeController {}

pub static CONTROLLER: spin::Mutex<Option<NvmeController>> = spin::Mutex::new(None);

impl NvmeController {
    /// True once handoff has happened and `read_blocks()`/`write_blocks()`
    /// are routing through `nvmed`'s mailbox instead of the direct in-kernel
    /// path — mainly useful for diagnostics/tests.
    pub fn mailbox_is_active(&self) -> bool { self.mailbox.is_some() }

    fn read32(&self, off: usize) -> u32 {
        unsafe { (self.regs.add(off) as *const u32).read_volatile() }
    }
    fn read64(&self, off: usize) -> u64 {
        unsafe { (self.regs.add(off) as *const u64).read_volatile() }
    }
    fn write32(&self, off: usize, v: u32) {
        unsafe { (self.regs.add(off) as *mut u32).write_volatile(v) }
    }
    fn write64(&self, off: usize, v: u64) {
        unsafe { (self.regs.add(off) as *mut u64).write_volatile(v) }
    }

    fn next_cid(&mut self) -> u16 {
        self.cid = self.cid.wrapping_add(1);
        self.cid
    }

    fn admin_cmd(&mut self, cmd: SqEntry) -> u16 {
        let cid = self.next_cid();
        q_submit(&mut self.admin, cid, cmd);
        q_wait(&mut self.admin, cid)
    }

    fn identify(&mut self, cns: u32, nsid: u32, buf_phys: u64) {
        let s = self.admin_cmd(SqEntry {
            cdw0:  0x06,
            nsid,
            prp1:  buf_phys,
            cdw10: cns,
            ..Default::default()
        });
        assert!(s == 0, "NVMe Identify failed: {}", s);
    }

    fn create_io_cq(&mut self, qid: u16, phys: u64, size: u16) {
        let s = self.admin_cmd(SqEntry {
            cdw0: 0x05,
            prp1: phys,
            cdw10: ((size as u32 - 1) << 16) | qid as u32,
            cdw11: 1, // physically contiguous
            ..Default::default()
        });
        assert!(s == 0, "NVMe Create I/O CQ failed: {}", s);
    }

    fn create_io_sq(&mut self, qid: u16, phys: u64, size: u16, cqid: u16) {
        let s = self.admin_cmd(SqEntry {
            cdw0: 0x01,
            prp1: phys,
            cdw10: ((size as u32 - 1) << 16) | qid as u32,
            cdw11: ((cqid as u32) << 16) | 1, // cqid + physically contiguous
            ..Default::default()
        });
        assert!(s == 0, "NVMe Create I/O SQ failed: {}", s);
    }

    /// Original, fully in-kernel I/O-queue path — used directly for all
    /// early-boot I/O (before `nvmed` can exist at all) and as the
    /// underlying implementation `spawn_pending_driver()` hands off *from*.
    /// Never called again once handoff has happened (see module doc).
    fn read_blocks_direct(&mut self, lba: u64, count: u16, buf_phys: u64) {
        let cid = self.next_cid();
        q_submit(&mut self.io, cid, SqEntry {
            cdw0: 0x02, nsid: 1, prp1: buf_phys,
            cdw10: lba as u32, cdw11: (lba >> 32) as u32,
            cdw12: (count - 1) as u32, ..Default::default()
        });
        let s = q_wait(&mut self.io, cid);
        assert!(s == 0, "NVMe read failed: {}", s);
    }

    fn write_blocks_direct(&mut self, lba: u64, count: u16, buf_phys: u64) {
        let cid = self.next_cid();
        q_submit(&mut self.io, cid, SqEntry {
            cdw0: 0x01, nsid: 1, prp1: buf_phys,
            cdw10: lba as u32, cdw11: (lba >> 32) as u32,
            cdw12: (count - 1) as u32, ..Default::default()
        });
        let s = q_wait(&mut self.io, cid);
        assert!(s == 0, "NVMe write failed: {}", s);
    }

    /// Read `count` blocks (max one `hepfs::BLOCK_SIZE`'s worth per call —
    /// see the mailbox's fixed `data` buffer) starting at `lba` into the
    /// contiguous physical buffer `buf_phys`. Routes through `nvmed`'s
    /// mailbox once handoff has happened; falls back to the original
    /// direct in-kernel path before that (see module doc comment) —
    /// **same public signature as before this migration, zero caller
    /// changes needed** in `hepfs.rs`/`main.rs`.
    pub fn read_blocks(&mut self, lba: u64, count: u16, buf_phys: u64) {
        if self.mailbox.is_none() {
            self.read_blocks_direct(lba, count, buf_phys);
            return;
        }
        let len = count as u32 * self.lba_size;
        assert!(len as usize <= 4096, "NVMe: mailbox request too large for one block ({} bytes)", len);
        let _io = IO_LOCK.lock();
        let mb = self.mailbox.unwrap();
        unsafe {
            core::ptr::write_volatile(&mut (*mb).lba, lba);
            core::ptr::write_volatile(&mut (*mb).count, count as u32);
            core::ptr::write_volatile(&mut (*mb).status, 0);
            core::ptr::write_volatile(&mut (*mb).op, 1); // release — hands off to nvmed
        }
        let mut ok = false;
        for _ in 0..MAILBOX_WAIT_SPINS {
            let status = unsafe { core::ptr::read_volatile(&(*mb).status) };
            if status != 0 { ok = status == 1; break; }
            core::hint::spin_loop();
        }
        if ok {
            unsafe {
                core::ptr::copy_nonoverlapping((*mb).data.as_ptr(), vmm::phys_to_virt(buf_phys), len as usize);
            }
        } else {
            serial::print("nvme: mailbox read timeout/error\n");
        }
        unsafe { core::ptr::write_volatile(&mut (*mb).status, 0); }
        assert!(ok, "NVMe read failed (mailbox)");
    }

    /// Same as `read_blocks()`, mirrored for writes.
    pub fn write_blocks(&mut self, lba: u64, count: u16, buf_phys: u64) {
        if self.mailbox.is_none() {
            self.write_blocks_direct(lba, count, buf_phys);
            return;
        }
        let len = count as u32 * self.lba_size;
        assert!(len as usize <= 4096, "NVMe: mailbox request too large for one block ({} bytes)", len);
        let _io = IO_LOCK.lock();
        let mb = self.mailbox.unwrap();
        unsafe {
            core::ptr::copy_nonoverlapping(vmm::phys_to_virt(buf_phys), (*mb).data.as_mut_ptr(), len as usize);
            core::ptr::write_volatile(&mut (*mb).lba, lba);
            core::ptr::write_volatile(&mut (*mb).count, count as u32);
            core::ptr::write_volatile(&mut (*mb).status, 0);
            core::ptr::write_volatile(&mut (*mb).op, 2); // release — hands off to nvmed
        }
        let mut ok = false;
        for _ in 0..MAILBOX_WAIT_SPINS {
            let status = unsafe { core::ptr::read_volatile(&(*mb).status) };
            if status != 0 { ok = status == 1; break; }
            core::hint::spin_loop();
        }
        if !ok { serial::print("nvme: mailbox write timeout/error\n"); }
        unsafe { core::ptr::write_volatile(&mut (*mb).status, 0); }
        assert!(ok, "NVMe write failed (mailbox)");
    }
}

/// Number of spin iterations to wait for `nvmed` to service one mailbox
/// request — generous for the same reason `ahci.rs`'s identical constant
/// is (the driver only polls once per ~10ms timer tick).
const MAILBOX_WAIT_SPINS: u32 = 500_000_000;

/// Serializes whole read/write transactions against each other and against
/// `spawn_pending_driver()`'s one-time handoff — same reasoning as
/// `ahci.rs`'s `AHCI_IO_LOCK`.
static IO_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn q_submit(q: &mut Queue, cid: u16, mut cmd: SqEntry) {
    cmd.cdw0 = (cmd.cdw0 & 0xFFFF) | ((cid as u32) << 16);
    unsafe { q.sq.add(q.sq_tail as usize).write_volatile(cmd); }
    fence(Ordering::SeqCst);
    q.sq_tail = (q.sq_tail + 1) % QD as u32;
    unsafe { q.db_sq.write_volatile(q.sq_tail); }
}

fn q_wait(q: &mut Queue, cid: u16) -> u16 {
    use crate::serial;
    let mut spins = 0u64;
    loop {
        let e = unsafe { q.cq.add(q.cq_head as usize).read_volatile() };
        if (e.status & 1) == q.phase as u16 && e.cid == cid {
            let s = (e.status >> 1) & 0x7FF;
            q.cq_head = (q.cq_head + 1) % QD as u32;
            if q.cq_head == 0 { q.phase = !q.phase; }
            unsafe { q.db_cq.write_volatile(q.cq_head); }
            return s;
        }
        core::hint::spin_loop();
        spins += 1;
        if spins == 50_000_000 {
            serial::print_hex("NVMe: waiting for cid", cid as u64);
            serial::print_hex("NVMe: CQ entry status", e.status as u64);
            serial::print_hex("NVMe: CQ entry cid",    e.cid as u64);
            serial::print_hex("NVMe: phase expected",  q.phase as u64);
        }
        if spins > 200_000_000 {
            serial::print("NVMe: cmd timeout, giving up\n");
            return 0xFFF;
        }
    }
}

fn alloc_dma_page() -> (u64, *mut u8) {
    let phys = pmm::alloc_page().expect("nvme: OOM");
    let virt = vmm::phys_to_virt(phys);
    unsafe { core::ptr::write_bytes(virt, 0, 4096); }
    (phys, virt)
}

fn make_queue(db_base: *mut u8, qid: usize, dstrd: usize) -> Queue {
    let (sq_phys, sq_virt) = alloc_dma_page();
    let (cq_phys, cq_virt) = alloc_dma_page();
    let db_sq = unsafe { db_base.add(0x1000 + (2 * qid)     * (4 << dstrd)) as *mut u32 };
    let db_cq = unsafe { db_base.add(0x1000 + (2 * qid + 1) * (4 << dstrd)) as *mut u32 };
    Queue {
        sq:      sq_virt as *mut SqEntry,
        cq:      cq_virt as *mut CqEntry,
        sq_phys, cq_phys,
        sq_tail: 0, cq_head: 0, phase: true,
        db_sq, db_cq,
    }
}

pub fn init(devices: &[pci::PciDevice]) -> Option<NvmeController> {
    use crate::serial;
    serial::print("NVMe: searching...\n");

    let dev = devices.iter().find(|d| {
        d.class == pci::CLASS_STORAGE && d.subclass == pci::SUB_NVME
    })?;

    serial::print("NVMe: found device\n");

    // Enable memory space + bus mastering on the PCI device
    let cmd = pci::config_read16(dev.bus, dev.dev, dev.func, 0x04);
    pci::config_write32(dev.bus, dev.dev, dev.func, 0x04, (cmd | 0x06) as u32);

    // Read 64-bit BAR0
    let bar0 = pci::config_read32(dev.bus, dev.dev, dev.func, 0x10) as u64;
    let bar1 = pci::config_read32(dev.bus, dev.dev, dev.func, 0x14) as u64;
    let bar_phys = if (bar0 & 0x6) == 0x4 {
        (bar1 << 32) | (bar0 & !0xF)
    } else {
        bar0 & !0xF
    };

    serial::print_hex("NVMe: BAR phys", bar_phys);

    // Map 64KB of NVMe MMIO
    serial::print("NVMe: mapping MMIO...\n");
    let regs = paging::map_mmio(bar_phys, 65536);
    serial::print("NVMe: MMIO mapped\n");

    // Read capabilities
    let cap = unsafe { (regs as *const u64).read_volatile() };
    serial::print_hex("NVMe: CAP", cap);
    let dstrd  = ((cap >> 32) & 0xF) as usize;
    // CSTS.RDY timeout in ms. `.max(500)` guards against a controller that
    // reports CAP.TO == 0 (undefined/unusual, but seen on some emulators) —
    // without a floor, the panic threshold below would be 0 and the very
    // first spin iteration where RDY hasn't already flipped would panic.
    let to_ms  = ((((cap >> 24) & 0xFF) as u64) * 500).max(500);

    // Disable controller
    let csts0 = unsafe { (regs.add(REG_CSTS) as *const u32).read_volatile() };
    serial::print_hex("NVMe: initial CSTS", csts0 as u64);

    serial::print("NVMe: disabling controller...\n");
    let cc = unsafe { (regs.add(REG_CC) as *const u32).read_volatile() };
    unsafe { (regs.add(REG_CC) as *mut u32).write_volatile(cc & !1); }

    // Wait for RDY = 0. Uses the same generous per-spin budget as the RDY=1
    // wait below (`to_ms * 10_000_000`) — this used to be `to_ms * 1_000`,
    // ~10,000x smaller for no good reason (both loops wait on the same
    // CAP.TO-bounded hardware condition). `spin_loop()` iterations don't
    // reliably correspond to a fixed amount of wall-clock time, so that
    // undersized budget could — and, per user reports, intermittently did —
    // panic well before the controller's real spec'd timeout had elapsed on
    // slower/busier host machines. Since panic() only prints to serial and
    // then spins forever (see panic.rs), this looked exactly like the whole
    // boot silently hanging on the splash screen (the last thing drawn to
    // the framebuffer before all driver init runs) — not a crash dialog, not
    // a reboot, just a permanently frozen screen requiring a manual restart.
    let mut spins = 0u64;
    while unsafe { (regs.add(REG_CSTS) as *const u32).read_volatile() } & 1 != 0 {
        core::hint::spin_loop();
        spins += 1;
        if spins > to_ms * 10_000_000 { panic!("NVMe disable timeout"); }
    }
    serial::print("NVMe: controller disabled\n");

    // Build admin queues (queue 0)
    let admin = make_queue(regs, 0, dstrd);

    // Set admin queue attributes and base addresses
    unsafe {
        (regs.add(REG_AQA) as *mut u32).write_volatile(
            ((QD as u32 - 1) << 16) | (QD as u32 - 1)
        );
        (regs.add(REG_ASQ) as *mut u64).write_volatile(admin.sq_phys);
        (regs.add(REG_ACQ) as *mut u64).write_volatile(admin.cq_phys);

        // CC: IOCQES=4 (bits 23:20, 2^4=16B), IOSQES=6 (bits 19:16, 2^6=64B), EN=1
        (regs.add(REG_CC) as *mut u32).write_volatile((4 << 20) | (6 << 16) | 1);
    }

    serial::print("NVMe: enabling controller...\n");
    // Wait for RDY = 1
    spins = 0;
    while unsafe { (regs.add(REG_CSTS) as *const u32).read_volatile() } & 1 == 0 {
        core::hint::spin_loop();
        spins += 1;
        if spins > to_ms * 10_000_000 { panic!("NVMe enable timeout"); }
    }
    serial::print("NVMe: controller ready\n");

    serial::print("NVMe: creating IO queue...\n");
    let io_q = make_queue(regs, 1, dstrd);
    serial::print("NVMe: IO queue created\n");

    let mut ctrl = NvmeController {
        regs,
        regs_phys: bar_phys,
        dstrd,
        admin,
        io: io_q,
        cid: 0,
        lba_size: 512,
        lba_count: 0,
        mailbox: None,
        mailbox_page_phys: None,
    };
    serial::print("NVMe: ctrl struct built\n");

    // Identify Controller (CNS=1, NSID=0 is valid for controller identify)
    serial::print("NVMe: sending Identify Controller...\n");
    let (id_phys, _) = alloc_dma_page();
    ctrl.identify(1, 0, id_phys);
    serial::print("NVMe: Identify Controller OK\n");

    // Identify Namespace 1 (CNS=0, NSID=1) — gives the real block size + block
    // count instead of the previous hardcoded 512B/0-blocks placeholder.
    serial::print("NVMe: sending Identify Namespace...\n");
    let (ns_phys, ns_virt) = alloc_dma_page();
    ctrl.identify(0, 1, ns_phys);
    let id_ns = unsafe { &*(ns_virt as *const IdNs) };
    let lbaf_idx  = (id_ns.flbas & 0x0F) as usize;
    let lbads     = (id_ns.lbaf[lbaf_idx] >> 16) & 0xFF;
    ctrl.lba_size  = 1u32 << lbads;
    ctrl.lba_count = id_ns.nsze;
    serial::print_hex("NVMe: LBA size", ctrl.lba_size as u64);
    serial::print_hex("NVMe: LBA count", ctrl.lba_count);

    // Create I/O queues (qid=1, size=QD, linked to cqid=1)
    let io_sq_phys = ctrl.io.sq_phys;
    let io_cq_phys = ctrl.io.cq_phys;
    serial::print("NVMe: Create IO CQ...\n");
    ctrl.create_io_cq(1, io_cq_phys, QD as u16);
    serial::print("NVMe: Create IO SQ...\n");
    ctrl.create_io_sq(1, io_sq_phys, QD as u16, 1);
    serial::print("NVMe: IO queues ready\n");

    Some(ctrl)
}

// ── Service management (`service`/`kill` terminal commands) ──────────────────
//
// **A real, previously-latent design mistake found and fixed via actual
// testing, not by inspection**: an early version of this migration treated
// `nvmed` like every other driver — once handed off, `stop` was meant to
// genuinely disable I/O until `start` relaunched it, matching the other 3
// services' "stop means stop" behavior. On real hardware/QEMU that's
// tolerable (no mouse, no audio, no network for a while); for NVMe it's
// not: this is the actual HepFS boot filesystem, and something on the
// desktop touches it constantly (icon refresh, file listings, ...) whether
// or not a human just ran `service stop nvmed`. The very first real test of
// a stop/start cycle — background desktop activity, not even the test
// itself — hit `read_blocks()`/`write_blocks()` while `nvmed` was down,
// spin-waited forever for a mailbox response that would never come, and
// **panicked the whole kernel** via the same `assert!` that's always fired
// on a genuine unrecoverable disk error (this migration didn't introduce
// that assert, but it did make "the service is stopped" — an expected,
// user-reachable state — trigger the exact same fatal path as real hardware
// failure).
//
// Fixed by making `stop`/`start` a genuine two-way handoff instead of a
// one-way trip: `stop_service()` syncs `self.io`'s software position
// (`sq_tail`/`cq_head`/`phase`) *back* from the mailbox before clearing
// `ctrl.mailbox`, so the in-kernel direct path (`read_blocks_direct()`/
// `write_blocks_direct()`) picks up exactly where `nvmed` left off and
// stays fully functional while the service is down — the boot filesystem
// never actually goes away, only the userspace process does. `start_service()`
// (and the very first `spawn_pending_driver()` handoff) then does the
// mirror image: builds the mailbox from `self.io`'s *current* live position
// (by definition up to date, since the direct path was authoritative the
// whole time `nvmed` was stopped) before relaunching. Both directions share
// `handoff_to_nvmed()`/`handoff_from_nvmed()` below.
static ENABLED: AtomicBool = AtomicBool::new(true);
pub const SERVICE_NAME: &str = "<nvmed>";
const STOP_WAIT_SPINS: u32 = 500_000_000;
static STARTING: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool { ENABLED.load(Ordering::Relaxed) }
pub fn set_enabled(v: bool) { ENABLED.store(v, Ordering::Relaxed); }
pub fn is_running() -> bool { process::is_process_running(SERVICE_NAME) }

// Baked-in nvmed ELF (generated by build.rs from userspace/target/.../nvmed).
// Empty slice if userspace hasn't been rebuilt since this driver was added.
include!(concat!(env!("OUT_DIR"), "/nvmed_elf.rs"));

/// Build (or reuse the already-allocated) mailbox from `ctrl.io`'s *current*
/// live position, grant the ranges `nvmed` needs, launch it, and wait for
/// it to actually register as running before declaring `ctrl.mailbox`
/// active — never finalizes the handoff on a launch that didn't land (see
/// the "why the direct path must stay safe" note in the module/service
/// doc comments above). Must be called with `CONTROLLER` unlocked (it locks
/// internally); safe to call whether this is the very first handoff or a
/// restart after `handoff_from_nvmed()`.
fn handoff_to_nvmed() -> Result<(), &'static str> {
    if NVMED_ELF.is_empty() { return Err("nvmed ELF not built"); }
    let mailbox_phys = {
        let mut guard = CONTROLLER.lock();
        let Some(ctrl) = guard.as_mut() else { return Err("driver not initialized"); };
        let mailbox_phys = match ctrl.mailbox_page_phys {
            Some(p) => p,
            None => {
                let Some(p) = pmm::alloc_contiguous(MAILBOX_PAGES) else {
                    return Err("mailbox OOM");
                };
                ctrl.mailbox_page_phys = Some(p);
                p
            }
        };
        let mailbox_virt = vmm::phys_to_virt(mailbox_phys) as *mut Mailbox;
        unsafe {
            core::ptr::write_bytes(mailbox_virt as *mut u8, 0, MAILBOX_PAGES * 4096);
            (*mailbox_virt).regs_phys  = ctrl.regs_phys;
            (*mailbox_virt).io_sq_phys = ctrl.io.sq_phys;
            (*mailbox_virt).io_cq_phys = ctrl.io.cq_phys;
            (*mailbox_virt).dstrd      = ctrl.dstrd as u32;
            (*mailbox_virt).qd         = QD as u32;
            // Live handoff — captures wherever the direct path (the sole
            // authority while nvmed was down/never yet started) actually
            // left the queue, not a stale snapshot.
            (*mailbox_virt).sq_tail = ctrl.io.sq_tail;
            (*mailbox_virt).cq_head = ctrl.io.cq_head;
            (*mailbox_virt).phase   = ctrl.io.phase as u32;
        }
        syscall::grant_mmio_range(ctrl.regs_phys, 65536);
        syscall::grant_mmio_range(ctrl.io.sq_phys, 4096);
        syscall::grant_mmio_range(ctrl.io.cq_phys, 4096);
        syscall::grant_mmio_range(mailbox_phys, (MAILBOX_PAGES * 4096) as u64);
        mailbox_phys
        // NOT setting ctrl.mailbox yet — read_blocks()/write_blocks() must
        // keep using the fully-working direct path until nvmed is actually
        // confirmed alive below.
    };

    process::exec_async_with_arg(usize::MAX, SERVICE_NAME, NVMED_ELF, mailbox_phys)?;
    for _ in 0..STOP_WAIT_SPINS {
        if is_running() { break; }
        core::hint::spin_loop();
    }
    if !is_running() { return Err("nvmed did not come up"); }

    let mut guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_mut() else { return Err("driver not initialized"); };
    ctrl.mailbox = Some(vmm::phys_to_virt(mailbox_phys) as *mut Mailbox);
    Ok(())
}

/// The reverse of `handoff_to_nvmed()`: sync `ctrl.io`'s software position
/// back from the mailbox (nvmed's last-persisted values, always current —
/// it writes them every loop iteration) and clear `ctrl.mailbox`, restoring
/// the in-kernel direct path to full authority. Must be called only after
/// confirming `nvmed` has actually exited (its stop-flag check happens once
/// per loop iteration, so its very last mailbox write is guaranteed to have
/// already landed by the time `is_process_running()` reports it gone).
fn handoff_from_nvmed() {
    let mut guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_mut() else { return; };
    let Some(mb) = ctrl.mailbox else { return; };
    unsafe {
        ctrl.io.sq_tail = core::ptr::read_volatile(&(*mb).sq_tail);
        ctrl.io.cq_head = core::ptr::read_volatile(&(*mb).cq_head);
        ctrl.io.phase   = core::ptr::read_volatile(&(*mb).phase) != 0;
    }
    ctrl.mailbox = None;
}

/// Hands off from the in-kernel direct I/O-queue path to a freshly-spawned
/// `nvmed` process, if this is the first call and hardware was found. Must
/// be called only after the scheduler's idle/blink tasks are registered and
/// the timer is running (i.e. from within `task_blink`'s own loop) — see
/// every other driver's identical constraint. A no-op on every call after
/// the first (or if there's no controller).
pub fn spawn_pending_driver() {
    static HANDED_OFF: AtomicBool = AtomicBool::new(false);
    if HANDED_OFF.swap(true, Ordering::AcqRel) { return; }
    if CONTROLLER.lock().is_none() { return; }
    match handoff_to_nvmed() {
        Ok(()) => serial::print("nvme: nvmed launched and confirmed running — I/O now routed through it\n"),
        Err(e) => serial::print(&alloc::format!("nvme: nvmed handoff failed ({}) — staying on the in-kernel direct I/O path\n", e)),
    }
}

pub fn stop_service() -> Result<(), &'static str> {
    if !is_running() { return Err("not running"); }
    let mb = { let g = CONTROLLER.lock(); g.as_ref().and_then(|c| c.mailbox) };
    let Some(mb) = mb else { return Err("driver not initialized") };
    unsafe { core::ptr::write_volatile(&mut (*mb).stop, 1); }
    for _ in 0..STOP_WAIT_SPINS {
        if !is_running() {
            // Restore the in-kernel direct path to authority *before*
            // returning — see the doc comment above for the real crash
            // this closes (background I/O landing on a dead mailbox).
            handoff_from_nvmed();
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("timeout waiting for driver to stop")
}

pub fn start_service() -> Result<(), &'static str> {
    if !is_enabled() { return Err("disabled"); }
    if is_running() { return Err("already running"); }
    if STARTING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("start already in progress");
    }
    let result = handoff_to_nvmed();
    STARTING.store(false, Ordering::Release);
    result
}
