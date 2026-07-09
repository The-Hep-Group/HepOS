#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace XHCI HID poller — fourth real driver migration,
// reusing the RTL8139 async fire-and-forget pattern (not AHCI's synchronous
// request/response one). The kernel (kernel/src/xhci.rs) does all the
// delicate one-time bring-up (HC reset, port power/reset, and the
// Enable-Slot/Address-Device/Configure-Endpoint command sequence for the
// mouse and optional keyboard) — those need `pmm`/PCI access no ring-3
// process has, and are synchronous command/wait-for-completion exchanges
// only ever run once per device. The *ongoing* work — draining the event
// ring for completed HID interrupt-IN transfers and re-queuing the next
// one — now runs here, forever, talking to the kernel through a
// shared-memory `Mailbox` page.
//
// This process only ever touches raw bytes: it copies each completed HID
// report (8 raw bytes, straight off the wire) into a small ring in the
// mailbox and lets the kernel do the actual translation (USB HID boot
// report → PS/2 Set-1 scancode / absolute mouse position) — that logic
// stays in `xhci.rs`'s `handle_mouse_report()`/`handle_kbd_report()`
// unchanged, now just driven by mailbox reports instead of direct hardware
// polling. `xhci::poll_mouse()` needed zero changes to its signature or
// its callers in `main.rs`.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/xhci.rs`'s `Mailbox` struct) — there's no shared crate
// between the two to enforce that, since userspace crates can't depend on
// kernel code at all (different target, no `std`, different address space).

use hepos_std::println;

const RING_N: usize = 64;
const IR0_ERDP: usize = 0x18;
const TRB_NORMAL:  u32 = 1;
const TRB_LINK:    u32 = 6;
const TRB_EV_XFER: u32 = 32;
const CC_SUCCESS:  u32 = 1;
const CC_SHORT:    u32 = 13;

const REPORT_RING_N: usize = 32;

#[repr(C)]
struct DeviceInfo {
    present:      u32,
    slot:         u32,
    hid_i:        u32,
    hid_c:        u32,
    hid_phys:     u64,
    hid_buf_phys: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Report {
    kind: u32, // 0 = empty, 1 = mouse, 2 = keyboard
    data: [u8; 8],
}

#[repr(C)]
struct Mailbox {
    bar_phys: u64,
    evt_phys: u64,
    cap_len:  u32,
    db_off:   u32,
    rt_off:   u32,
    evt_i:    u32,
    evt_c:    u32,
    _pad0:    u32,
    mouse: DeviceInfo,
    kbd:   DeviceInfo,
    head: u32,
    tail: u32,
    reports: [Report; REPORT_RING_N],
}

unsafe fn w64(b: *mut u8, o: usize, v: u64) { (b.add(o) as *mut u64).write_volatile(v) }

unsafe fn trb_w(base: *mut u8, idx: usize, w: [u32; 4]) {
    let p = base.add(idx * 16) as *mut u32;
    for i in 0..4 { p.add(i).write_volatile(w[i]); }
}
unsafe fn trb_r(base: *const u8, idx: usize) -> [u32; 4] {
    let p = base.add(idx * 16) as *const u32;
    [p.read_volatile(), p.add(1).read_volatile(), p.add(2).read_volatile(), p.add(3).read_volatile()]
}

/// Live per-device ring state — mirrors `xhci.rs`'s `HidEp`, minus the
/// control-endpoint fields (only ever used during kernel-side bring-up).
struct Dev {
    slot: u8,
    hid_v: *mut u8, hid_p: u64, hid_i: usize, hid_c: u8,
    hid_buf_v: *mut u8, hid_buf_p: u64,
}

unsafe fn queue_hid(db: *mut u8, dev: &mut Dev) {
    let c = dev.hid_c as u32;
    trb_w(dev.hid_v, dev.hid_i, [dev.hid_buf_p as u32, (dev.hid_buf_p >> 32) as u32, 8, TRB_NORMAL << 10 | 1 << 5 | c]);
    dev.hid_i += 1;
    if dev.hid_i >= RING_N - 1 {
        trb_w(dev.hid_v, dev.hid_i, [dev.hid_p as u32, (dev.hid_p >> 32) as u32, 0, TRB_LINK << 10 | (1 << 1) | c]);
        dev.hid_i = 0;
        dev.hid_c ^= 1;
    }
    (db.add(dev.slot as usize * 4) as *mut u32).write_volatile(3); // EP1 IN = DCI 3
}

unsafe fn dequeue(evt_v: *mut u8, evt_p: u64, rt: *mut u8, evt_i: &mut usize, evt_c: &mut u8) -> Option<[u32; 4]> {
    let trb = trb_r(evt_v, *evt_i);
    if (trb[3] & 1) != *evt_c as u32 { return None; }
    let erdp = evt_p + *evt_i as u64 * 16;
    w64(rt, 0x20 + IR0_ERDP, erdp | 8);
    *evt_i += 1;
    if *evt_i >= RING_N { *evt_i = 0; *evt_c ^= 1; }
    Some(trb)
}

/// Push one completed report into the mailbox's SPSC ring for the kernel to
/// drain. Drops the report (no retry, no error) if the ring is momentarily
/// full — the kernel drains it every `task_blink` iteration, far more often
/// than this driver's own ~10ms-rate-limited poll, so in practice the ring
/// never holds more than one or two entries; a full ring would mean the
/// kernel side has stopped polling entirely, at which point dropping is no
/// worse than any other failure mode.
unsafe fn push_report(mb: *mut Mailbox, kind: u32, data: &[u8]) {
    let head = core::ptr::read_volatile(&(*mb).head);
    let tail = core::ptr::read_volatile(&(*mb).tail);
    if head.wrapping_sub(tail) as usize >= REPORT_RING_N { return; }
    let idx = (head as usize) % REPORT_RING_N;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(data);
    (*mb).reports[idx] = Report { kind, data: buf };
    core::ptr::write_volatile(&mut (*mb).head, head.wrapping_add(1)); // release
}

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("xhcid: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("xhcid: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = mb_va as *mut Mailbox;

    let bar_va = hepos_rt::sys_mmap_mmio((*mb).bar_phys, 65536);
    let evt_va = hepos_rt::sys_mmap_mmio((*mb).evt_phys, 4096);
    if bar_va == 0 || evt_va == 0 {
        println!("xhcid: failed to map BAR/event ring — exiting");
        hepos_rt::sys_exit(1);
    }
    let op = (bar_va as *mut u8).add((*mb).cap_len as usize);
    let db = (bar_va as *mut u8).add((*mb).db_off as usize);
    let rt = (bar_va as *mut u8).add((*mb).rt_off as usize);
    let _ = op; // kept for parity with xhci.rs's layout; not read directly here
    let evt_v = evt_va as *mut u8;
    let evt_p = (*mb).evt_phys;
    let mut evt_i = (*mb).evt_i as usize;
    let mut evt_c = (*mb).evt_c as u8;

    let mut mouse = {
        let hid_va = hepos_rt::sys_mmap_mmio((*mb).mouse.hid_phys, 4096);
        let buf_va = hepos_rt::sys_mmap_mmio((*mb).mouse.hid_buf_phys, 4096);
        if hid_va == 0 || buf_va == 0 {
            println!("xhcid: failed to map mouse rings — exiting");
            hepos_rt::sys_exit(1);
        }
        Dev {
            slot: (*mb).mouse.slot as u8,
            hid_v: hid_va as *mut u8, hid_p: (*mb).mouse.hid_phys,
            hid_i: (*mb).mouse.hid_i as usize, hid_c: (*mb).mouse.hid_c as u8,
            hid_buf_v: buf_va as *mut u8, hid_buf_p: (*mb).mouse.hid_buf_phys,
        }
    };

    let mut kbd: Option<Dev> = if (*mb).kbd.present != 0 {
        let hid_va = hepos_rt::sys_mmap_mmio((*mb).kbd.hid_phys, 4096);
        let buf_va = hepos_rt::sys_mmap_mmio((*mb).kbd.hid_buf_phys, 4096);
        if hid_va == 0 || buf_va == 0 {
            println!("xhcid: failed to map keyboard rings — keyboard disabled");
            None
        } else {
            Some(Dev {
                slot: (*mb).kbd.slot as u8,
                hid_v: hid_va as *mut u8, hid_p: (*mb).kbd.hid_phys,
                hid_i: (*mb).kbd.hid_i as usize, hid_c: (*mb).kbd.hid_c as u8,
                hid_buf_v: buf_va as *mut u8, hid_buf_p: (*mb).kbd.hid_buf_phys,
            })
        }
    } else { None };

    println!("xhcid: ready (mouse slot {}, kbd {})", mouse.slot, kbd.is_some());

    loop {
        while let Some(t) = dequeue(evt_v, evt_p, rt, &mut evt_i, &mut evt_c) {
            let ty = (t[3] >> 10) & 0x3F;
            let cc = (t[2] >> 24) & 0xFF;
            if ty != TRB_EV_XFER || (cc != CC_SUCCESS && cc != CC_SHORT) { continue; }
            let slot = (t[3] >> 24) as u8;

            if slot == mouse.slot {
                let buf = core::slice::from_raw_parts(mouse.hid_buf_v, 8);
                push_report(mb, 1, buf);
                core::ptr::write_bytes(mouse.hid_buf_v, 0, 8);
                queue_hid(db, &mut mouse);
            } else if let Some(k) = kbd.as_mut() {
                if slot == k.slot {
                    let buf = core::slice::from_raw_parts(k.hid_buf_v, 8);
                    push_report(mb, 2, buf);
                    core::ptr::write_bytes(k.hid_buf_v, 0, 8);
                    queue_hid(db, k);
                }
            }
        }

        // Rate-limit to once per timer tick, same reasoning as every other
        // userspace driver in this kernel (see rtl8139d/hdad/ahcid) — a
        // never-yielding poll loop starves the host's own I/O handling
        // under this project's single-vCPU TCG emulation.
        hepos_rt::sys_wait_irq(0x20);
    }
}
