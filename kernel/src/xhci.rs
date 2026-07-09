//! Minimal XHCI host controller driver — QEMU `qemu-xhci` + `usb-tablet` (+
//! `usb-kbd`, since this session). Poll-based (no interrupts).
//!
//! Scoped simplification, clearly documented rather than hidden: this driver
//! doesn't parse HID report/interface descriptors to classify device type
//! (a real classifier would read bInterfaceProtocol — 1=keyboard, 2=mouse —
//! off the configuration descriptor, which needs control-IN data-stage
//! transfers this driver doesn't implement). Instead it assumes **the first
//! connected port is the mouse and the second is the keyboard** — true as
//! long as `-device usb-tablet,bus=xhci.0` is listed before
//! `-device usb-kbd,bus=xhci.0` in the QEMU command line (see build.sh/
//! build.ps1), since QEMU attaches devices to ports in command-line order.
//! Good enough for this dev environment; would need real descriptor parsing
//! to be robust on arbitrary hardware/port orderings.
//!
//! Keyboard input is translated from USB HID boot-protocol keycodes to PS/2
//! Set-1 scancodes and fed through `ps2::handle_scancode()` — so it reuses
//! 100% of the existing shift/caps/ctrl state machine and special-key
//! mapping (arrows, F-keys, etc.) instead of duplicating any of it. The rest
//! of the OS (terminal, editor, ...) needs zero changes to accept USB
//! keyboard input alongside PS/2.
//!
//! **Fourth driver migrated to userspace**, reusing RTL8139's async
//! fire-and-forget pattern (not AHCI's synchronous request/response one):
//! the one-time bring-up above (HC reset, port power/reset, and the
//! Enable-Slot/Address-Device/Configure-Endpoint command sequence for each
//! HID device) stays in the kernel — it needs `pmm`/PCI access no ring-3
//! process has, and every step there is a synchronous command/wait-for-
//! completion exchange only ever run once per device at boot. The *ongoing*
//! work — draining the event ring for completed HID interrupt-IN transfers
//! and re-queuing the next one — now runs in a persistent userspace
//! process, `userspace/xhcid`, which copies each raw 8-byte HID report into
//! a small ring in a shared `Mailbox` page. The actual translation logic
//! (`handle_mouse_report()`/`handle_kbd_report()` below) stays in the
//! kernel completely unchanged, just driven by mailbox reports instead of
//! direct hardware polling — `poll_mouse()` needed zero signature changes,
//! so `main.rs`'s only caller needed zero changes either.

use crate::{pmm, process, syscall, vmm, serial};
use spin::Mutex;

// ─── MMIO helpers ──────────────────────────────────────────────────────────────
unsafe fn r8 (b: *const u8, o: usize) -> u8  { b.add(o).read_volatile() }
unsafe fn r32(b: *const u8, o: usize) -> u32 { (b.add(o) as *const u32).read_volatile() }
unsafe fn w32(b: *mut   u8, o: usize, v: u32){ (b.add(o) as *mut   u32).write_volatile(v) }
unsafe fn w64(b: *mut   u8, o: usize, v: u64){ (b.add(o) as *mut   u64).write_volatile(v) }

// ─── Capability register offsets ───────────────────────────────────────────────
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_DBOFF:      usize = 0x14;
const CAP_RTSOFF:     usize = 0x18;

// ─── Operational register offsets (cap_base + CAPLENGTH) ───────────────────────
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_DNCTRL: usize = 0x14;
const OP_CRCR:   usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;

const CMD_RUN:   u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH:   u32 = 1 << 0;
const STS_CNR:   u32 = 1 << 11;

// Port registers (op_base + 0x400 + port_idx * 0x10)
const PORT_CCS:  u32 = 1 << 0;
const PORT_PED:  u32 = 1 << 1;
const PORT_PR:   u32 = 1 << 4;
const PORT_PP:   u32 = 1 << 9;
const PORT_PRC:  u32 = 1 << 21;
// Bits 17-23 are write-1-to-clear status change bits
const PORT_W1C:  u32 = 0x00FE_0000;

// ─── Runtime interrupter 0 offsets (rt_base + 0x20) ───────────────────────────
const IR0_ERSTSZ: usize = 0x08;
const IR0_ERSTBA: usize = 0x10;
const IR0_ERDP:   usize = 0x18;

// ─── TRB types ─────────────────────────────────────────────────────────────────
const TRB_NORMAL:      u32 = 1;
const TRB_SETUP:       u32 = 2;
const TRB_STATUS:      u32 = 4;
const TRB_LINK:        u32 = 6;
const TRB_CMD_EN_SLOT: u32 = 9;
const TRB_CMD_ADDR:    u32 = 11;
const TRB_CMD_CFG_EP:  u32 = 12;
const TRB_EV_XFER:     u32 = 32;
const TRB_EV_CMD:      u32 = 33;

const CC_SUCCESS: u32 = 1;
const CC_SHORT:   u32 = 13;

const RING_N: usize = 64;

fn dma() -> (u64, *mut u8) {
    let p = pmm::alloc_page().expect("xhci: DMA OOM");
    let v = vmm::phys_to_virt(p);
    unsafe { core::ptr::write_bytes(v, 0, 4096); }
    (p, v)
}

unsafe fn trb_w(base: *mut u8, idx: usize, w: [u32; 4]) {
    let p = base.add(idx * 16) as *mut u32;
    for i in 0..4 { p.add(i).write_volatile(w[i]); }
}
unsafe fn trb_r(base: *const u8, idx: usize) -> [u32; 4] {
    let p = base.add(idx * 16) as *const u32;
    [p.read_volatile(), p.add(1).read_volatile(),
     p.add(2).read_volatile(), p.add(3).read_volatile()]
}

/// One HID interrupt-IN endpoint's ring state — every USB HID device this
/// driver drives (mouse, optionally keyboard) gets one of these.
struct HidEp {
    slot: u8,
    ep0_v: *mut u8, ep0_p: u64, ep0_i: usize, ep0_c: u8,
    hid_v: *mut u8, hid_p: u64, hid_i: usize, hid_c: u8,
    hid_buf_v: *mut u8, hid_buf_p: u64,
}

/// Edge-tracked state for translating boot-protocol keyboard reports into
/// `ps2::handle_scancode()` calls — only *changes* since the last report
/// produce output (new key press, modifier press/release), matching how a
/// real PS/2 controller only ever sends one make/break event per transition.
struct KbdState {
    prev_mods: u8,
    prev_keys: [u8; 6],
}

pub struct Xhci {
    _cap: *mut u8, _op: *mut u8, rt: *mut u8, db: *mut u8,

    cmd_v: *mut u8, cmd_p: u64, cmd_i: usize, cmd_c: u8,
    evt_v: *mut u8, evt_p: u64, evt_i: usize, evt_c: u8,

    _erst_v: *mut u8,
    _dcbaa_v: *mut u8,

    mouse: HidEp,
    kbd: Option<(HidEp, KbdState)>,
}

unsafe impl Send for Xhci {}

impl Xhci {
    unsafe fn ring_cmd(&self) { (self.db as *mut u32).write_volatile(0); }
    unsafe fn ring_ep(&self, slot: u8, dci: u8) {
        (self.db.add(slot as usize * 4) as *mut u32).write_volatile(dci as u32);
    }

    unsafe fn push_cmd(&mut self, mut w: [u32; 4]) {
        w[3] = (w[3] & !1) | self.cmd_c as u32;
        trb_w(self.cmd_v, self.cmd_i, w);
        self.cmd_i += 1;
        if self.cmd_i >= RING_N - 1 {
            let tc = if self.cmd_c == 1 { 1u32 << 1 } else { 0 };
            trb_w(self.cmd_v, self.cmd_i, [
                self.cmd_p as u32, (self.cmd_p >> 32) as u32, 0,
                TRB_LINK << 10 | tc | self.cmd_c as u32,
            ]);
            self.cmd_i = 0;
            self.cmd_c ^= 1;
        }
    }

    unsafe fn push_ep0(ep: &mut HidEp, mut w: [u32; 4]) {
        w[3] = (w[3] & !1) | ep.ep0_c as u32;
        trb_w(ep.ep0_v, ep.ep0_i, w);
        ep.ep0_i += 1;
        if ep.ep0_i >= RING_N - 1 {
            let tc = if ep.ep0_c == 1 { 1u32 << 1 } else { 0 };
            trb_w(ep.ep0_v, ep.ep0_i, [
                ep.ep0_p as u32, (ep.ep0_p >> 32) as u32, 0,
                TRB_LINK << 10 | tc | ep.ep0_c as u32,
            ]);
            ep.ep0_i = 0;
            ep.ep0_c ^= 1;
        }
    }

    unsafe fn dequeue(&mut self) -> Option<[u32; 4]> {
        let trb = trb_r(self.evt_v, self.evt_i);
        if (trb[3] & 1) != self.evt_c as u32 { return None; }
        let erdp = self.evt_p + self.evt_i as u64 * 16;
        w64(self.rt, 0x20 + IR0_ERDP, erdp | 8);
        self.evt_i += 1;
        if self.evt_i >= RING_N { self.evt_i = 0; self.evt_c ^= 1; }
        Some(trb)
    }

    // Wait for Command Completion Event; return (completion_code, slot_id)
    unsafe fn wait_cmd(&mut self) -> (u32, u8) {
        for _ in 0..8_000_000u32 {
            if let Some(t) = self.dequeue() {
                let ty = (t[3] >> 10) & 0x3F;
                if ty == TRB_EV_CMD {
                    let cc   = (t[2] >> 24) & 0xFF;
                    let slot = (t[3] >> 24) as u8;
                    return (cc, slot);
                }
                // Eat port-status-change events silently
                continue;
            }
            core::hint::spin_loop();
        }
        serial::print("xhci: wait_cmd timeout!\n");
        (0xFF, 0)
    }

    // Wait for one Transfer Event
    unsafe fn wait_xfer(&mut self) {
        for _ in 0..8_000_000u32 {
            if let Some(t) = self.dequeue() {
                if (t[3] >> 10) & 0x3F == TRB_EV_XFER { return; }
                continue;
            }
            core::hint::spin_loop();
        }
        serial::print("xhci: wait_xfer timeout!\n");
    }

    // No-data OUT control transfer (e.g. SET_CONFIGURATION)
    unsafe fn ctrl_nodata(&mut self, ep: &mut HidEp, bm: u8, req: u8, val: u16, idx: u16) {
        let w0 = bm as u32 | ((req as u32) << 8) | ((val as u32) << 16);
        let w1 = idx as u32;
        // Setup TRB: IDT=bit6, IOC=0, TRT=0 (no data stage)
        Self::push_ep0(ep, [w0, w1, 8, TRB_SETUP << 10 | 1 << 6]);
        // Status TRB: DIR=IN (bit16), IOC=bit5
        Self::push_ep0(ep, [0, 0, 0, TRB_STATUS << 10 | 1 << 16 | 1 << 5]);
        self.ring_ep(ep.slot, 1);
        self.wait_xfer();
    }

    unsafe fn queue_hid(&mut self, which: usize) {
        let ep = if which == 0 { &mut self.mouse } else { &mut self.kbd.as_mut().unwrap().0 };
        let c = ep.hid_c as u32;
        trb_w(ep.hid_v, ep.hid_i, [
            ep.hid_buf_p as u32, (ep.hid_buf_p >> 32) as u32,
            8, TRB_NORMAL << 10 | 1 << 5 | c,
        ]);
        ep.hid_i += 1;
        if ep.hid_i >= RING_N - 1 {
            // TC=1 (Toggle Cycle) ALWAYS so the XHC toggles PCS on every wrap.
            // cycle bit = c (current cycle, so XHC processes this link TRB now).
            trb_w(ep.hid_v, ep.hid_i, [
                ep.hid_p as u32, (ep.hid_p >> 32) as u32,
                0, TRB_LINK << 10 | (1 << 1) | c,
            ]);
            ep.hid_i = 0;
            ep.hid_c ^= 1;
        }
        let slot = ep.slot;
        self.ring_ep(slot, 3); // EP1 IN = DCI 3
    }

    fn handle_mouse_report(buf: &[u8], fb_w: u32, fb_h: u32) {
        let buttons = buf[0] & 0x07;
        let abs_x   = u16::from_le_bytes([buf[1], buf[2]]) as u32;
        let abs_y   = u16::from_le_bytes([buf[3], buf[4]]) as u32;
        // Ignore (0,0) with no buttons — tablet sends this as initial report
        // before the host cursor enters the QEMU window.
        if abs_x != 0 || abs_y != 0 || buttons != 0 {
            let sx = (abs_x.saturating_mul(fb_w)) / 32768;
            let sy = (abs_y.saturating_mul(fb_h)) / 32768;
            let mut m = crate::mouse::MOUSE.lock();
            m.x = sx as i32;
            m.y = sy as i32;
            m.buttons = buttons;
        }
    }

    fn handle_kbd_report(buf: &[u8], st: &mut KbdState) {
        let mods = buf[0];
        let mut keys = [0u8; 6];
        keys.copy_from_slice(&buf[2..8]);

        // Modifiers: PS/2's SHIFT/CTRL tracking needs both press *and*
        // release edges (unlike regular keys, where ps2.rs ignores releases
        // outright), so react to every bit that flipped either way.
        let changed = st.prev_mods ^ mods;
        for bit in 0..8u8 {
            if changed & (1 << bit) == 0 { continue; }
            let pressed = mods & (1 << bit) != 0;
            let Some(ps2_sc) = modifier_to_ps2(bit) else { continue };
            crate::ps2::handle_scancode(if pressed { ps2_sc } else { ps2_sc | 0x80 });
        }
        st.prev_mods = mods;

        // Regular keys: only react to newly-pressed keycodes (not already in
        // the previous report) — boot-protocol reports list every
        // currently-held key on every report, so this is how "new key" edges
        // are detected. Releases don't need an event: ps2.rs already ignores
        // non-modifier key releases.
        for &kc in &keys {
            if kc == 0 || kc < 4 { continue; } // 0=none, 1-3=error/rollover codes
            if st.prev_keys.contains(&kc) { continue; }
            if let Some((ps2_sc, extended)) = keycode_to_ps2(kc) {
                if extended { crate::ps2::handle_scancode(0xE0); }
                crate::ps2::handle_scancode(ps2_sc);
            }
        }
        st.prev_keys = keys;
    }

}

/// USB HID modifier bit (0-7, matching the boot-report byte0 layout: LCtrl,
/// LShift, LAlt, LGui, RCtrl, RShift, RAlt, RGui) → PS/2 Set-1 make code.
/// `None` for modifiers PS/2 has no equivalent state for (Gui/Win key).
fn modifier_to_ps2(bit: u8) -> Option<u8> {
    match bit {
        0 => Some(0x1D), // LCtrl
        1 => Some(0x2A), // LShift
        2 => Some(0x38), // LAlt (unmapped in ps2.rs's tables, but harmless)
        4 => Some(0x1D), // RCtrl — ps2.rs's Ctrl match doesn't distinguish L/R
        5 => Some(0x36), // RShift
        6 => Some(0x38), // RAlt
        _ => None,       // LGui(3)/RGui(7) — no PS/2 equivalent tracked
    }
}

/// USB HID Usage ID (boot-protocol keycode) → (PS/2 Set-1 make code, is_extended).
/// `is_extended` means `ps2::handle_scancode(0xE0)` must be sent first, same
/// as a real PS/2 keyboard would for that key (arrows, Home/End/PgUp/PgDn, Delete).
fn keycode_to_ps2(kc: u8) -> Option<(u8, bool)> {
    let sc = match kc {
        0x04..=0x1D => { // a-z
            const MAP: [u8; 26] = [
                0x1E,0x30,0x2E,0x20,0x12,0x21,0x22,0x23,0x17,0x24, // a-j
                0x25,0x26,0x32,0x31,0x18,0x19,0x10,0x13,0x1F,0x14, // k-t
                0x16,0x2F,0x11,0x2D,0x15,0x2C,                     // u-z
            ];
            MAP[(kc - 0x04) as usize]
        }
        0x1E..=0x26 => 0x02 + (kc - 0x1E), // 1-9
        0x27 => 0x0B,                       // 0
        0x28 => 0x1C, // Enter
        0x29 => 0x01, // Escape
        0x2A => 0x0E, // Backspace
        0x2B => 0x0F, // Tab
        0x2C => 0x39, // Space
        0x2D => 0x0C, // -
        0x2E => 0x0D, // =
        0x2F => 0x1A, // [
        0x30 => 0x1B, // ]
        0x31 => 0x2B, // backslash
        0x33 => 0x27, // ;
        0x34 => 0x28, // '
        0x35 => 0x29, // `
        0x36 => 0x33, // ,
        0x37 => 0x34, // .
        0x38 => 0x35, // /
        0x39 => 0x3A, // CapsLock
        0x3A..=0x43 => 0x3B + (kc - 0x3A), // F1-F10
        0x44 => 0x57, // F11
        0x45 => 0x58, // F12
        0x4A => return Some((0x47, true)), // Home
        0x4B => return Some((0x49, true)), // PageUp
        0x4C => return Some((0x53, true)), // Delete
        0x4D => return Some((0x4F, true)), // End
        0x4E => return Some((0x51, true)), // PageDown
        0x4F => return Some((0x4D, true)), // Right
        0x50 => return Some((0x4B, true)), // Left
        0x51 => return Some((0x50, true)), // Down
        0x52 => return Some((0x48, true)), // Up
        _ => return None,
    };
    Some((sc, false))
}

// ── Post-bring-up: mailbox handoff to `xhcid` ─────────────────────────────────
//
// Everything above stays in the kernel (one-time, needs `pmm`/PCI access).
// From here down, ongoing hardware polling moves to `userspace/xhcid`; the
// kernel side only drains a small ring of raw HID reports and still owns
// `handle_mouse_report()`/`handle_kbd_report()` — completely unchanged by
// this migration, just called with mailbox-sourced bytes instead of bytes
// read directly out of `hid_buf_v`.

/// One device's ring/slot info as handed to `xhcid` at launch — mirrors the
/// subset of `HidEp` the driver process needs to keep polling and
/// re-queuing transfers on its own, plus the ring-position state
/// (`hid_i`/`hid_c`) the kernel already advanced by one queued transfer
/// during bring-up (`queue_hid()` below), which `xhcid` must continue from
/// rather than re-initializing to 0/1.
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

const REPORT_RING_N: usize = 32;

/// Shared memory mailbox — one physical page, mapped into both the kernel
/// (via `vmm::phys_to_virt`) and the `xhcid` userspace process (via
/// `SYS_MMAP_MMIO`, using the physical address handed to it as its one
/// launch argument). **Layout must stay byte-for-byte identical to the copy
/// in `userspace/xhcid/src/main.rs`.**
#[repr(C)]
struct Mailbox {
    bar_phys: u64,
    evt_phys: u64,
    cap_len:  u32,
    db_off:   u32,
    rt_off:   u32,
    /// Event-ring consumer position the kernel's own bring-up (Enable
    /// Slot/Address Device/Configure Endpoint command completions) already
    /// advanced past — `xhcid` must continue from here, not reset to 0/1.
    evt_i: u32,
    evt_c: u32,
    _pad0: u32,
    mouse: DeviceInfo,
    kbd:   DeviceInfo,
    /// SPSC ring: `xhcid` writes a report then advances `head` (release);
    /// the kernel reads at `tail` then advances `tail` once consumed. Same
    /// convention as RTL8139's `rx_ready` handoff, just ring-shaped instead
    /// of single-slot — a mouse/keyboard losing an in-flight report to a
    /// races would be a real regression (dropped key edges), unlike a
    /// dropped network packet.
    head: u32,
    tail: u32,
    reports: [Report; REPORT_RING_N],
    /// 0 = keep running. Kernel writes 1 to request a cooperative shutdown
    /// (`service stop xhcid` / `kill <pid>`); see `stop_service()`'s doc
    /// comment for why this is cooperative rather than a true forced kill.
    stop: u32,
}

struct XhciHandle {
    mailbox:      *mut Mailbox,
    mailbox_phys: u64,
    kbd_present:  bool,
    kbd_state:    KbdState,
}
unsafe impl Send for XhciHandle {}

pub static XHCI: Mutex<Option<XhciHandle>> = Mutex::new(None);

/// Mailbox physical address waiting to be handed to a freshly-spawned
/// `xhcid` task, set by `init()` and consumed once by
/// `spawn_pending_driver()` — same deferred-spawn pattern as every other
/// driver migrated so far (`init()` runs too early, before the scheduler's
/// idle/blink tasks are registered, for `scheduler::spawn()` to be safe to
/// call directly).
static PENDING_DRIVER_MAILBOX: Mutex<Option<u64>> = Mutex::new(None);

/// Launches the queued `xhcid` driver process, if `init()` found a mouse (and
/// optional keyboard) and queued one. Must be called only after the
/// scheduler's idle/blink tasks are registered and the timer is running
/// (i.e. from within `task_blink`'s own loop). A no-op on every call after
/// the first (or if nothing was ever queued).
pub fn spawn_pending_driver() {
    let Some(mailbox_phys) = PENDING_DRIVER_MAILBOX.lock().take() else { return };
    match process::exec_async_with_arg(usize::MAX, "<xhcid>", XHCID_ELF, mailbox_phys) {
        Ok(()) => serial::print("xhci: xhcid launched\n"),
        Err(_) => serial::print("xhci: xhcid launch failed\n"),
    }
}

// Baked-in xhcid ELF (generated by build.rs from userspace/target/.../xhcid).
// Empty slice if userspace hasn't been rebuilt since this driver was added.
include!(concat!(env!("OUT_DIR"), "/xhcid_elf.rs"));

// ── Service management (`service`/`kill` terminal commands) ──────────────────
// Same shape as rtl8139.rs's — see that module's comments for the full
// reasoning (cooperative stop via a mailbox flag, not a true forced kill;
// enable/disable is in-memory only for this session). Note: while `xhcid`
// is stopped, mouse/keyboard input simply stops being delivered — `poll_mouse()`
// still runs fine (it only ever reads the mailbox's report ring, which just
// stays empty), so no other code needs to know the service is down.
static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub const SERVICE_NAME: &str = "<xhcid>";
const STOP_WAIT_SPINS: u32 = 500_000_000;

pub fn is_enabled() -> bool { ENABLED.load(core::sync::atomic::Ordering::Relaxed) }
pub fn set_enabled(v: bool) { ENABLED.store(v, core::sync::atomic::Ordering::Relaxed); }
pub fn is_running() -> bool { process::is_process_running(SERVICE_NAME) }

pub fn stop_service() -> Result<(), &'static str> {
    if !is_running() { return Err("not running"); }
    let mb = { let g = XHCI.lock(); g.as_ref().map(|h| h.mailbox) };
    let Some(mb) = mb else { return Err("driver not initialized") };
    unsafe { core::ptr::write_volatile(&mut (*mb).stop, 1); }
    for _ in 0..STOP_WAIT_SPINS {
        if !is_running() { return Ok(()); }
        core::hint::spin_loop();
    }
    Err("timeout waiting for driver to stop")
}

/// Guards against two concurrent `start_service()` calls racing past the
/// `is_running()` check below — see `rtl8139.rs`'s identical `STARTING`
/// for the double-launch bug this closes (`exec_async_with_arg()` only
/// *queues* the launch; the spawned task needs its own scheduling turn
/// before `is_running()` sees it).
static STARTING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn start_service() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;
    if !is_enabled() { return Err("disabled"); }
    if is_running() { return Err("already running"); }
    if STARTING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("start already in progress");
    }
    let result = (|| {
        let (mb, mailbox_phys) = {
            let g = XHCI.lock();
            match g.as_ref() { Some(h) => (h.mailbox, h.mailbox_phys), None => return Err("driver not initialized") }
        };
        if XHCID_ELF.is_empty() { return Err("xhcid ELF not built"); }
        unsafe { core::ptr::write_volatile(&mut (*mb).stop, 0); }
        process::exec_async_with_arg(usize::MAX, SERVICE_NAME, XHCID_ELF, mailbox_phys)?;
        for _ in 0..STOP_WAIT_SPINS {
            if is_running() { return Ok(()); }
            core::hint::spin_loop();
        }
        Err("timeout waiting for driver to start")
    })();
    STARTING.store(false, Ordering::Release);
    result
}

/// Drain every raw HID report `xhcid` has queued since the last call and
/// translate it via the same `handle_mouse_report()`/`handle_kbd_report()`
/// logic the kernel always used, just fed from the mailbox instead of
/// hardware. Same call site/signature as before this migration — `main.rs`
/// needed zero changes.
pub fn poll_mouse(fb_w: u32, fb_h: u32) {
    let mut guard = XHCI.lock();
    let Some(h) = guard.as_mut() else { return };
    let mb = h.mailbox;
    unsafe {
        loop {
            let head = core::ptr::read_volatile(&(*mb).head);
            let tail = core::ptr::read_volatile(&(*mb).tail);
            if head == tail { break; }
            let idx = (tail as usize) % REPORT_RING_N;
            let report = (*mb).reports[idx];
            match report.kind {
                1 => Xhci::handle_mouse_report(&report.data, fb_w, fb_h),
                2 => Xhci::handle_kbd_report(&report.data, &mut h.kbd_state),
                _ => {}
            }
            core::ptr::write_volatile(&mut (*mb).tail, tail.wrapping_add(1));
        }
    }
}

/// Returns true if XHCI is initialized and `xhcid` has been queued/launched.
pub fn is_ready() -> bool { XHCI.lock().is_some() }

/// Brings up one USB HID device already reset on `port` (1-based), returning
/// its `HidEp` state with the interrupt-IN endpoint queued and ready. Shared
/// by both the mouse and the (optional) keyboard in `init()` below — the
/// per-device sequence (Enable Slot → Address Device → SET_CONFIGURATION →
/// Configure Endpoint) is identical for either.
unsafe fn bring_up_hid_device(x: &mut Xhci, dcbaa_v: *mut u8, port: u8, port_speed: u8, label: &str) -> Option<HidEp> {
    serial::print("xhci: sending Enable Slot (");
    serial::print(label);
    serial::print(")...\n");
    x.push_cmd([0, 0, 0, TRB_CMD_EN_SLOT << 10]);
    x.ring_cmd();
    let (cc, slot) = x.wait_cmd();
    serial::print_hex("xhci: Enable Slot CC=", cc as u64);
    if cc != CC_SUCCESS { return None; }
    serial::print_hex("xhci: slot_id=", slot as u64);

    let (dev_ctx_p, dev_ctx_v) = dma();
    let (in_ctx_p, in_ctx_v)   = dma();
    let (ep0_p, ep0_v)         = dma();
    let (hid_p, hid_v)         = dma();
    let (hid_buf_p, hid_buf_v) = dma();
    let _ = dev_ctx_v; // kept alive via DCBAAP entry below; never read directly

    trb_w(ep0_v, RING_N - 1, [ep0_p as u32, (ep0_p >> 32) as u32, 0, TRB_LINK << 10 | 1 << 1 | 1]);
    trb_w(hid_v, RING_N - 1, [hid_p as u32, (hid_p >> 32) as u32, 0, TRB_LINK << 10 | 1 << 1 | 1]);

    (dcbaa_v as *mut u64).add(slot as usize).write_volatile(dev_ctx_p);

    let ic = in_ctx_v;
    (ic.add(0x04) as *mut u32).write_volatile(0x3); // Add A0(slot)+A1(EP0)
    (ic.add(0x20) as *mut u32).write_volatile(1 << 27 | (port_speed as u32) << 20);
    (ic.add(0x24) as *mut u32).write_volatile((port as u32) << 16);
    (ic.add(0x44) as *mut u32).write_volatile(3 << 1 | 4 << 3 | 64 << 16); // EP0: Cerr=3, Control, MPS=64
    (ic.add(0x48) as *mut u64).write_volatile(ep0_p | 1);
    (ic.add(0x50) as *mut u32).write_volatile(8);

    serial::print("xhci: sending Address Device...\n");
    x.push_cmd([in_ctx_p as u32 & !0xF, (in_ctx_p >> 32) as u32, 0, TRB_CMD_ADDR << 10 | (slot as u32) << 24]);
    x.ring_cmd();
    let (cc, _) = x.wait_cmd();
    serial::print_hex("xhci: Address Device CC=", cc as u64);
    if cc != CC_SUCCESS { return None; }
    serial::print("xhci: device addressed\n");

    let mut ep = HidEp { slot, ep0_v, ep0_p, ep0_i: 0, ep0_c: 1, hid_v, hid_p, hid_i: 0, hid_c: 1, hid_buf_v, hid_buf_p };

    serial::print("xhci: sending SET_CONFIGURATION...\n");
    x.ctrl_nodata(&mut ep, 0x00, 0x09, 1, 0);
    serial::print("xhci: SET_CONFIGURATION done\n");

    // Configure Endpoint — add EP1 IN (DCI 3), same input context reused in place.
    (ic.add(0x00) as *mut u32).write_volatile(0);
    (ic.add(0x04) as *mut u32).write_volatile(1 | 1 << 3); // Add A0(slot)+A3(EP1 IN)
    (ic.add(0x20) as *mut u32).write_volatile(3 << 27 | (port_speed as u32) << 20); // Context Entries=3
    (ic.add(0x80) as *mut u32).write_volatile(3 << 16); // Interval=3 (1ms @ HS)
    (ic.add(0x84) as *mut u32).write_volatile(3 << 1 | 7 << 3 | 8 << 16); // Cerr=3, Interrupt IN, MPS=8
    (ic.add(0x88) as *mut u64).write_volatile(hid_p | 1);
    (ic.add(0x90) as *mut u32).write_volatile(8);

    serial::print("xhci: sending Configure Endpoint...\n");
    x.push_cmd([in_ctx_p as u32 & !0xF, (in_ctx_p >> 32) as u32, 0, TRB_CMD_CFG_EP << 10 | (slot as u32) << 24]);
    x.ring_cmd();
    let (cc, _) = x.wait_cmd();
    serial::print_hex("xhci: Configure EP CC=", cc as u64);
    if cc != CC_SUCCESS { return None; }
    serial::print("xhci: EP1 IN ready (");
    serial::print(label);
    serial::print(")\n");

    Some(ep)
}

pub fn init(devices: &[crate::pci::PciDevice]) {
    let d = match devices.iter().find(|d|
        d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30)
    {
        Some(d) => d,
        None => { serial::print("xhci: no XHCI controller\n"); return; }
    };
    serial::print("xhci: found controller\n");
    serial::print_hex("  vendor:dev=", ((d.vendor_id as u64) << 16) | d.device_id as u64);

    // Enable MMIO + bus master
    let pci_cmd = crate::pci::config_read16(d.bus, d.dev, d.func, 0x04);
    crate::pci::config_write32(d.bus, d.dev, d.func, 0x04, (pci_cmd | 0x06) as u32);

    // BAR0 (64-bit MMIO for qemu-xhci)
    let bar0 = d.bar(0);
    let bar1 = d.bar(1);
    let bar_phys = if (bar0 & 0x06) == 0x04 {
        (bar0 as u64 & !0xF) | ((bar1 as u64) << 32)
    } else {
        bar0 as u64 & !0xF
    };
    serial::print_hex("xhci: BAR=", bar_phys);
    if bar_phys == 0 { serial::print("xhci: BAR is 0, aborting\n"); return; }

    let cap = crate::paging::map_mmio(bar_phys, 65536);

    unsafe {
        let cap_len  = r8(cap as *const u8, 0) as usize;
        let op       = cap.add(cap_len);
        let db_off   = (r32(cap as *const u8, CAP_DBOFF)  & !3)  as usize;
        let rt_off   = (r32(cap as *const u8, CAP_RTSOFF) & !31) as usize;
        let db       = cap.add(db_off);
        let rt       = cap.add(rt_off);

        let hcsp1    = r32(cap as *const u8, CAP_HCSPARAMS1);
        let max_slots= (hcsp1 & 0xFF) as usize;
        let max_ports= (hcsp1 >> 24) as usize;
        serial::print_hex("xhci: cap_len=", cap_len as u64);
        serial::print_hex("xhci: max_ports=", max_ports as u64);

        // Wait until controller is ready
        for _ in 0..4_000_000u32 {
            if r32(op as *const u8, OP_USBSTS) & STS_CNR == 0 { break; }
            core::hint::spin_loop();
        }
        let sts = r32(op as *const u8, OP_USBSTS);
        serial::print_hex("xhci: USBSTS before reset=", sts as u64);

        // Stop if running
        if r32(op as *const u8, OP_USBCMD) & CMD_RUN != 0 {
            w32(op, OP_USBCMD, r32(op as *const u8, OP_USBCMD) & !CMD_RUN);
            for _ in 0..4_000_000u32 {
                if r32(op as *const u8, OP_USBSTS) & STS_HCH != 0 { break; }
                core::hint::spin_loop();
            }
        }

        // HC reset
        w32(op, OP_USBCMD, CMD_HCRST);
        for _ in 0..4_000_000u32 {
            if r32(op as *const u8, OP_USBCMD) & CMD_HCRST == 0 { break; }
            core::hint::spin_loop();
        }
        for _ in 0..4_000_000u32 {
            if r32(op as *const u8, OP_USBSTS) & STS_CNR == 0 { break; }
            core::hint::spin_loop();
        }
        serial::print("xhci: reset done\n");

        // Allocate DMA pages (all zeroed by dma())
        let (cmd_p, cmd_v)         = dma();
        let (evt_p, evt_v)         = dma();
        let (erst_p, erst_v)       = dma();
        let (dcbaa_p, dcbaa_v)     = dma();

        // Place link TRB at end of the command ring
        trb_w(cmd_v, RING_N - 1, [cmd_p as u32, (cmd_p >> 32) as u32, 0, TRB_LINK << 10 | 1 << 1 | 1]);

        // ERST: 1 segment pointing to event ring
        (erst_v as *mut u64).write_volatile(evt_p);
        (erst_v.add(8) as *mut u32).write_volatile(RING_N as u32);

        // Interrupter 0
        let ir0 = rt.add(0x20);
        w32(ir0, IR0_ERSTSZ, 1);
        w64(ir0, IR0_ERSTBA, erst_p);
        w64(ir0, IR0_ERDP, evt_p);

        // Operational setup
        w64(op, OP_DCBAAP, dcbaa_p);
        w32(op, OP_CONFIG, max_slots.min(255) as u32);
        w32(op, OP_DNCTRL, 0xFFFF);
        w64(op, OP_CRCR, cmd_p | 1); // CCS=1

        // Start HC
        w32(op, OP_USBCMD, CMD_RUN | 1 << 2 | 1 << 3);
        for _ in 0..4_000_000u32 {
            if r32(op as *const u8, OP_USBSTS) & STS_HCH == 0 { break; }
            core::hint::spin_loop();
        }
        serial::print_hex("xhci: USBSTS after start=", r32(op as *const u8, OP_USBSTS) as u64);
        serial::print("xhci: HC running\n");

        // Small delay for USB devices to appear on ports
        for _ in 0..1_000_000u32 { core::hint::spin_loop(); }

        // Scan ports — print PORTSC for each, reset every connected one and
        // collect them in order. Per the module doc: port order determines
        // mouse-vs-keyboard, since this driver doesn't classify by descriptor.
        let mut connected: [(u8, u8); 8] = [(0, 0); 8]; // (port 1-based, speed)
        let mut n_connected = 0usize;
        for pi in 0..max_ports {
            let pb = op.add(0x400 + pi * 0x10);
            let sc = r32(pb as *const u8, 0);
            serial::print_hex("xhci: port PORTSC=", sc as u64);

            // Power on if needed
            if sc & PORT_PP == 0 {
                w32(pb, 0, (sc & !PORT_W1C) | PORT_PP);
                for _ in 0..500_000u32 { core::hint::spin_loop(); }
            }

            let sc = r32(pb as *const u8, 0);
            if sc & PORT_CCS != 0 && n_connected < connected.len() {
                serial::print_hex("xhci: device on port (1-based)=", (pi + 1) as u64);

                // Reset port: clear W1C bits, set PR
                w32(pb, 0, (sc & !PORT_W1C) | PORT_PR);
                for _ in 0..4_000_000u32 {
                    if r32(pb as *const u8, 0) & PORT_PR == 0 { break; }
                    core::hint::spin_loop();
                }

                // Read port speed AFTER reset (bits[13:10])
                let sc2 = r32(pb as *const u8, 0);
                let mut speed = ((sc2 >> 10) & 0xF) as u8;
                if speed == 0 { speed = 3; } // default to HS if unknown
                serial::print_hex("xhci: port speed=", speed as u64);
                serial::print_hex("xhci: PORTSC after reset=", sc2 as u64);

                // Clear PRC change bit
                w32(pb, 0, (sc2 & !PORT_W1C) | PORT_PRC);
                serial::print("xhci: port reset done\n");

                connected[n_connected] = ((pi + 1) as u8, speed);
                n_connected += 1;
            }
        }

        if n_connected == 0 {
            serial::print("xhci: no device found on any port!\n");
            return;
        }

        // Small delay before sending commands
        for _ in 0..200_000u32 { core::hint::spin_loop(); }

        let mut x = Xhci {
            _cap: cap, _op: op, rt, db,
            cmd_v, cmd_p, cmd_i: 0, cmd_c: 1,
            evt_v, evt_p, evt_i: 0, evt_c: 1,
            _erst_v: erst_v,
            _dcbaa_v: dcbaa_v,
            mouse: HidEp { slot: 0, ep0_v: core::ptr::null_mut(), ep0_p: 0, ep0_i: 0, ep0_c: 1,
                           hid_v: core::ptr::null_mut(), hid_p: 0, hid_i: 0, hid_c: 1,
                           hid_buf_v: core::ptr::null_mut(), hid_buf_p: 0 },
            kbd: None,
        };

        let (mouse_port, mouse_speed) = connected[0];
        let Some(mouse_ep) = bring_up_hid_device(&mut x, dcbaa_v, mouse_port, mouse_speed, "mouse") else {
            serial::print("xhci: mouse bring-up failed\n");
            return;
        };
        x.mouse = mouse_ep;
        x.queue_hid(0);
        serial::print("xhci: mouse ready!\n");

        if n_connected > 1 {
            let (kbd_port, kbd_speed) = connected[1];
            match bring_up_hid_device(&mut x, dcbaa_v, kbd_port, kbd_speed, "keyboard") {
                Some(kbd_ep) => {
                    x.kbd = Some((kbd_ep, KbdState { prev_mods: 0, prev_keys: [0; 6] }));
                    x.queue_hid(1);
                    serial::print("xhci: keyboard ready!\n");
                }
                None => serial::print("xhci: keyboard bring-up failed (mouse still works)\n"),
            }
        }

        // Shared mailbox — the only thing the kernel and the `xhcid`
        // userspace process both touch going forward. Its size (well under
        // one page) fits a single `pmm::alloc_page()`, unlike AHCI's.
        let Some(mailbox_phys) = pmm::alloc_page() else {
            serial::print("xhci: mailbox OOM — mouse/keyboard will not work\n");
            return;
        };
        let mailbox_virt = vmm::phys_to_virt(mailbox_phys) as *mut Mailbox;
        core::ptr::write_bytes(mailbox_virt as *mut u8, 0, 4096);
        (*mailbox_virt).bar_phys = bar_phys;
        (*mailbox_virt).evt_phys = x.evt_p;
        (*mailbox_virt).cap_len  = cap_len as u32;
        (*mailbox_virt).db_off   = db_off as u32;
        (*mailbox_virt).rt_off   = rt_off as u32;
        (*mailbox_virt).evt_i    = x.evt_i as u32;
        (*mailbox_virt).evt_c    = x.evt_c as u32;
        (*mailbox_virt).mouse = DeviceInfo {
            present: 1, slot: x.mouse.slot as u32,
            hid_i: x.mouse.hid_i as u32, hid_c: x.mouse.hid_c as u32,
            hid_phys: x.mouse.hid_p, hid_buf_phys: x.mouse.hid_buf_p,
        };

        syscall::grant_mmio_range(bar_phys, 65536);
        syscall::grant_mmio_range(x.evt_p, 4096);
        syscall::grant_mmio_range(x.mouse.hid_p, 4096);
        syscall::grant_mmio_range(x.mouse.hid_buf_p, 4096);

        let kbd_present = x.kbd.is_some();
        let kbd_state = if let Some((ep, st)) = &x.kbd {
            (*mailbox_virt).kbd = DeviceInfo {
                present: 1, slot: ep.slot as u32,
                hid_i: ep.hid_i as u32, hid_c: ep.hid_c as u32,
                hid_phys: ep.hid_p, hid_buf_phys: ep.hid_buf_p,
            };
            syscall::grant_mmio_range(ep.hid_p, 4096);
            syscall::grant_mmio_range(ep.hid_buf_p, 4096);
            KbdState { prev_mods: st.prev_mods, prev_keys: st.prev_keys }
        } else {
            KbdState { prev_mods: 0, prev_keys: [0; 6] }
        };
        syscall::grant_mmio_range(mailbox_phys, 4096);

        *XHCI.lock() = Some(XhciHandle { mailbox: mailbox_virt, mailbox_phys, kbd_present, kbd_state });

        if XHCID_ELF.is_empty() {
            serial::print("xhci: xhcid ELF not built (run `cargo build --release` in userspace/) — mouse/keyboard will not work\n");
        } else {
            // Don't spawn the driver task here — `init()` runs during early
            // hardware bring-up, before `kmain` registers the scheduler's
            // idle/blink tasks (see `spawn_pending_driver()`'s doc comment).
            *PENDING_DRIVER_MAILBOX.lock() = Some(mailbox_phys);
            serial::print("xhci: xhcid queued to launch once the scheduler is up\n");
        }
    }
}
