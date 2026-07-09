#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace RTL8139 NIC driver — HepOS's first real "driver
// moved to userspace" migration (PLAN.md's "move drivers to userspace
// libOS" item). The kernel (kernel/src/rtl8139.rs) does the one-time PCI
// enable/reset/DMA-buffer-allocation dance (needs `pmm`/PCI access no
// ring-3 process has) and then launches this process, handing it the
// physical address of a shared `Mailbox` page as its one launch argument.
// From then on this process runs forever, polling the actual hardware
// registers (via SYS_PORT_IN/OUT) and the RX DMA ring (via SYS_MMAP_MMIO,
// read directly — no more syscalls needed once mapped, same "passthrough"
// idea `userspace/hwtest` already proved out) — the kernel's
// `Rtl8139::send()`/`recv()` just write/read the mailbox, never touching
// the hardware directly again.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/rtl8139.rs`'s `Mailbox` struct) — there's no shared crate
// between the two to enforce that, since userspace crates can't depend on
// kernel code at all (different target, no `std`, different address space).

use hepos_std::println;

const TX_SLOTS: usize = 4;
const TX_SLOT_STRIDE: u64 = 4096;

// Register offsets (from I/O base) — same subset the old in-kernel driver used.
const TXADDR0: u16 = 0x20;
const TXSTAT0: u16 = 0x10;
const CAPR:    u16 = 0x38;
const CBA:     u16 = 0x3A;

const TXSTAT_OWN: u32 = 1 << 13;
const RX_ROK:     u16 = 1 << 0;

#[repr(C)]
struct Mailbox {
    io_base: u32,
    tx_phys: u32,
    rx_phys: u32,
    _pad0:   u32,
    tx_len:  u32,
    tx_buf:  [u8; 1792],
    rx_ready: u32,
    rx_len:   u32,
    rx_buf:   [u8; 1600],
}

const RX_BUF_LEN: usize = 65536 + 16;

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("rtl8139d: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("rtl8139d: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = &mut *(mb_va as *mut Mailbox);

    let io      = mb.io_base as u16;
    let tx_phys = mb.tx_phys as u64;
    let rx_phys = mb.rx_phys as u64;

    let tx_va = hepos_rt::sys_mmap_mmio(tx_phys, TX_SLOTS as u64 * TX_SLOT_STRIDE);
    let rx_va = hepos_rt::sys_mmap_mmio(rx_phys, RX_BUF_LEN as u64);
    if tx_va == 0 || rx_va == 0 {
        println!("rtl8139d: failed to map TX/RX DMA buffers — exiting");
        hepos_rt::sys_exit(1);
    }

    println!("rtl8139d: ready (io_base {:#x})", io);

    let mut tx_slot = 0usize;
    let mut rx_off  = 0usize;

    loop {
        // ── TX: kernel queued a packet — send it for real ──────────────────
        let tx_len = core::ptr::read_volatile(&mb.tx_len) as usize;
        if tx_len != 0 {
            let slot = tx_slot % TX_SLOTS;
            // Wait for the hardware to release this slot (OWN bit set).
            for _ in 0..1_000_000u32 {
                if hepos_rt::sys_port_in(io + TXSTAT0 + (slot as u16) * 4, 4) & TXSTAT_OWN != 0 { break; }
            }
            let slot_va = (tx_va as *mut u8).add(slot * TX_SLOT_STRIDE as usize);
            core::ptr::copy_nonoverlapping(mb.tx_buf.as_ptr(), slot_va, tx_len);
            let slot_phys = tx_phys + slot as u64 * TX_SLOT_STRIDE;
            hepos_rt::sys_port_out(io + TXADDR0 + (slot as u16) * 4, 4, slot_phys as u32);
            hepos_rt::sys_port_out(io + TXSTAT0 + (slot as u16) * 4, 4, (tx_len as u32) & 0x1FFF);
            tx_slot = tx_slot.wrapping_add(1);
            core::ptr::write_volatile(&mut mb.tx_len, 0);
        }

        // ── RX: hardware may have a packet waiting — pick it up ────────────
        if core::ptr::read_volatile(&mb.rx_ready) == 0 {
            let cbr  = hepos_rt::sys_port_in(io + CBA, 2) as usize;
            let capr = hepos_rt::sys_port_in(io + CAPR, 2) as usize;
            if (capr + 16) % 65536 != cbr % 65536 {
                let rx_ptr = rx_va as *const u8;
                let off = rx_off;
                let b0 = core::ptr::read_volatile(rx_ptr.add(off % 65536));
                let b1 = core::ptr::read_volatile(rx_ptr.add((off + 1) % 65536));
                let hdr_status = u16::from_le_bytes([b0, b1]);
                let b2 = core::ptr::read_volatile(rx_ptr.add((off + 2) % 65536));
                let b3 = core::ptr::read_volatile(rx_ptr.add((off + 3) % 65536));
                let pkt_len = u16::from_le_bytes([b2, b3]) as usize;

                if hdr_status & RX_ROK == 0 || pkt_len < 14 || pkt_len > 1518 {
                    // Bad packet — advance past it, same as the old driver did.
                    rx_off = (off + 4 + pkt_len + 3) & !3;
                    hepos_rt::sys_port_out(io + CAPR, 2, rx_off.wrapping_sub(16) as u32);
                } else {
                    let frame_len = pkt_len.saturating_sub(4).min(mb.rx_buf.len());
                    for i in 0..frame_len {
                        let byte = core::ptr::read_volatile(rx_ptr.add((off + 4 + i) % 65536));
                        core::ptr::write_volatile(&mut mb.rx_buf[i], byte);
                    }
                    core::ptr::write_volatile(&mut mb.rx_len, frame_len as u32);
                    rx_off = (off + 4 + pkt_len + 3) & !3;
                    hepos_rt::sys_port_out(io + CAPR, 2, rx_off.wrapping_sub(16) as u32);
                    core::ptr::write_volatile(&mut mb.rx_ready, 1);
                }
            }
        }

        // Rate-limit to once per timer tick instead of spinning at 100% CPU.
        // Beyond just being wasteful, a driver process that never yields at
        // all starves *everything else* of real CPU time under this
        // project's single-vCPU TCG emulation — including QEMU's own
        // host-side I/O/SLiRP handling, which runs on a separate thread that
        // needs the host OS to actually schedule it. `task_idle`'s `hlt`
        // used to give the host that breathing room implicitly every
        // scheduling round; this process's tight loop (once it was added as
        // a third task) removed that idle time on its share of the
        // round-robin. `SYS_WAIT_IRQ` on the timer vector (see
        // `hepos_rt::sys_wait_irq`'s doc comment for why it's hardcoded)
        // gives it back, at the cost of ~10ms of added latency per
        // send/receive — an easy trade for an NIC that's not remotely
        // latency-critical.
        hepos_rt::sys_wait_irq(0x20);
    }
}
