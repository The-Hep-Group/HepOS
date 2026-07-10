#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace NVMe I/O-queue driver — fifth real driver migration,
// and the first one backing the actual boot filesystem. The kernel
// (kernel/src/nvme.rs) does all the one-time bring-up (PCI enable, MMIO
// mapping, controller disable/enable, admin queue setup, Identify
// Controller/Namespace, I/O queue *creation*) and keeps using its own
// in-kernel I/O-queue path for everything that happens before this process
// can even exist (early boot). Once the scheduler is up, the kernel hands
// off — this process then owns all further reads/writes on the I/O queue,
// talking to the kernel through a shared-memory `Mailbox` page, the same
// synchronous request/response shape `ahcid` established.
//
// **Persists its own live queue position back into the mailbox every loop
// iteration** (`sq_tail`/`cq_head`/`phase`) — NVMe's SQ/CQ doorbell
// registers are write-only per spec, so unlike `rtl8139d` (which can just
// re-read the hardware's `CAPR` register after a restart) there's no way to
// recover this from hardware. Without persisting it, a `service stop` +
// `service start` cycle would resume from stale state and desync from
// wherever the hardware and this process's own prior instance actually
// left off — the exact bug found (and fixed) in `xhcid`'s event/HID rings.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/nvme.rs`'s `Mailbox` struct) — there's no shared crate
// between the two to enforce that, since userspace crates can't depend on
// kernel code at all (different target, no `std`, different address space).

use hepos_std::println;
use core::sync::atomic::{fence, Ordering};

#[repr(C, align(64))]
#[derive(Clone, Copy, Default)]
struct SqEntry {
    cdw0:  u32,
    nsid:  u32,
    cdw2:  u32, cdw3: u32,
    mptr:  u64,
    prp1:  u64,
    prp2:  u64,
    cdw10: u32, cdw11: u32, cdw12: u32,
    cdw13: u32, cdw14: u32, cdw15: u32,
}

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

#[repr(C)]
struct Mailbox {
    regs_phys:  u64,
    io_sq_phys: u64,
    io_cq_phys: u64,
    dstrd:      u32,
    qd:         u32,
    sq_tail: u32,
    cq_head: u32,
    phase:   u32,
    op:     u32,
    status: u32,
    lba:    u64,
    count:  u32,
    _pad0:  u32,
    data: [u8; 4096],
    stop: u32,
}

unsafe fn trb_write_sq(sq: *mut u8, idx: usize, e: SqEntry) {
    (sq.add(idx * 64) as *mut SqEntry).write_volatile(e);
}
unsafe fn trb_read_cq(cq: *const u8, idx: usize) -> CqEntry {
    (cq.add(idx * 16) as *const CqEntry).read_volatile()
}

struct IoQueue {
    sq: *mut u8,
    cq: *mut u8,
    db_sq: *mut u32,
    db_cq: *mut u32,
    qd: u32,
    sq_tail: u32,
    cq_head: u32,
    phase: bool,
}

unsafe fn q_submit(q: &mut IoQueue, cid: u16, mut cmd: SqEntry) {
    cmd.cdw0 = (cmd.cdw0 & 0xFFFF) | ((cid as u32) << 16);
    trb_write_sq(q.sq, q.sq_tail as usize, cmd);
    fence(Ordering::SeqCst);
    q.sq_tail = (q.sq_tail + 1) % q.qd;
    q.db_sq.write_volatile(q.sq_tail);
}

/// Wait for the completion matching `cid`. Bounded (mirrors the kernel's
/// original `q_wait` budget) — returns `None` on timeout rather than
/// hanging forever, since a wedged NVMe queue shouldn't take the whole
/// driver process down with it.
unsafe fn q_wait(q: &mut IoQueue, cid: u16) -> Option<u16> {
    for _ in 0..200_000_000u32 {
        let e = trb_read_cq(q.cq, q.cq_head as usize);
        if (e.status & 1) == q.phase as u16 && e.cid == cid {
            let s = (e.status >> 1) & 0x7FF;
            q.cq_head = (q.cq_head + 1) % q.qd;
            if q.cq_head == 0 { q.phase = !q.phase; }
            q.db_cq.write_volatile(q.cq_head);
            return Some(s);
        }
        core::hint::spin_loop();
    }
    None
}

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("nvmed: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("nvmed: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = &mut *(mb_va as *mut Mailbox);

    let regs_va = hepos_rt::sys_mmap_mmio(mb.regs_phys, 65536) as *mut u8;
    let sq_va   = hepos_rt::sys_mmap_mmio(mb.io_sq_phys, 4096) as *mut u8;
    let cq_va   = hepos_rt::sys_mmap_mmio(mb.io_cq_phys, 4096) as *mut u8;
    if regs_va.is_null() || sq_va.is_null() || cq_va.is_null() {
        println!("nvmed: failed to map regs/SQ/CQ — exiting");
        hepos_rt::sys_exit(1);
    }

    let dstrd = mb.dstrd as usize;
    // I/O queue is qid=1 — doorbell layout per NVMe spec: db_base + 0x1000 +
    // (2*qid + {0=SQ,1=CQ}) * (4 << dstrd).
    let db_sq = regs_va.add(0x1000 + 2 * (4 << dstrd)) as *mut u32;
    let db_cq = regs_va.add(0x1000 + 3 * (4 << dstrd)) as *mut u32;

    let mut q = IoQueue {
        sq: sq_va, cq: cq_va,
        db_sq, db_cq, qd: mb.qd,
        sq_tail: mb.sq_tail, cq_head: mb.cq_head, phase: mb.phase != 0,
    };
    let mut cid: u16 = 0;

    println!("nvmed: ready (sq_tail={} cq_head={} phase={})", q.sq_tail, q.cq_head, q.phase);

    loop {
        if core::ptr::read_volatile(&mb.stop) != 0 {
            println!("nvmed: stop requested, exiting");
            hepos_rt::sys_exit(0);
        }

        let op = core::ptr::read_volatile(&mb.op);
        if op != 0 {
            let lba   = core::ptr::read_volatile(&mb.lba);
            let count = core::ptr::read_volatile(&mb.count);
            let data_phys = mailbox_phys + core::mem::offset_of!(Mailbox, data) as u64;
            cid = cid.wrapping_add(1);

            let cmd = SqEntry {
                cdw0: if op == 2 { 0x01 } else { 0x02 }, // write : read
                nsid: 1,
                prp1: data_phys,
                cdw10: lba as u32, cdw11: (lba >> 32) as u32,
                cdw12: count.saturating_sub(1),
                ..Default::default()
            };
            q_submit(&mut q, cid, cmd);
            let status = q_wait(&mut q, cid);
            core::ptr::write_volatile(&mut mb.status, match status {
                Some(0) => 1,
                _ => { println!("nvmed: I/O error/timeout status={:?}", status); 2 }
            });
            core::ptr::write_volatile(&mut mb.op, 0);
        }

        // Persist live queue position — see the module doc comment for why
        // this (not a hardware register) is the only source of truth a
        // restart can recover from.
        core::ptr::write_volatile(&mut mb.sq_tail, q.sq_tail);
        core::ptr::write_volatile(&mut mb.cq_head, q.cq_head);
        core::ptr::write_volatile(&mut mb.phase, q.phase as u32);

        // Rate-limit to once per timer tick, same reasoning as every other
        // userspace driver in this kernel.
        hepos_rt::sys_wait_irq(0x20);
    }
}
