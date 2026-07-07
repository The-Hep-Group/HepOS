use crate::{paging, pci, pmm, vmm};
use core::sync::atomic::{fence, Ordering};

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

pub struct NvmeController {
    regs:       *mut u8,
    admin:      Queue,
    io:         Queue,
    cid:        u16,
    pub lba_size:  u32,
    pub lba_count: u64,
}

unsafe impl Send for NvmeController {}
unsafe impl Sync for NvmeController {}

pub static CONTROLLER: spin::Mutex<Option<NvmeController>> = spin::Mutex::new(None);

impl NvmeController {
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

    pub fn read_blocks(&mut self, lba: u64, count: u16, buf_phys: u64) {
        let cid = self.next_cid();
        q_submit(&mut self.io, cid, SqEntry {
            cdw0: 0x02, nsid: 1, prp1: buf_phys,
            cdw10: lba as u32, cdw11: (lba >> 32) as u32,
            cdw12: (count - 1) as u32, ..Default::default()
        });
        let s = q_wait(&mut self.io, cid);
        assert!(s == 0, "NVMe read failed: {}", s);
    }

    pub fn write_blocks(&mut self, lba: u64, count: u16, buf_phys: u64) {
        let cid = self.next_cid();
        q_submit(&mut self.io, cid, SqEntry {
            cdw0: 0x01, nsid: 1, prp1: buf_phys,
            cdw10: lba as u32, cdw11: (lba >> 32) as u32,
            cdw12: (count - 1) as u32, ..Default::default()
        });
        let s = q_wait(&mut self.io, cid);
        assert!(s == 0, "NVMe write failed: {}", s);
    }
}

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
        admin,
        io: io_q,
        cid: 0,
        lba_size: 512,
        lba_count: 0,
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
