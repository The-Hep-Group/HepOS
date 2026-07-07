//! virtio-gpu driver (2D mode only — no 3D/virgl).
//!
//! First step toward real GPU-assisted display per PLAN.md's original
//! design goal ("path to real GPU acceleration" instead of software-only
//! pixel pushing) — this driver detects the device, brings up the modern
//! virtio-pci transport from scratch (virtio-gpu has no legacy/transitional
//! ID, unlike e.g. virtio-net, so there's no simpler I/O-port fallback
//! available), creates a 2D resource, and can push pixel data to a scanout.
//!
//! **Not wired into the real display path yet** — HepBL's GOP framebuffer
//! (set up before ExitBootServices) remains what the kernel actually draws
//! to. Swapping the *boot* display over to virtio-gpu is a materially
//! bigger, riskier follow-up (touches the critical boot sequence); this is
//! scoped to proving the driver itself works, run as an independent PCI
//! device alongside the existing `-vga std` display.
//!
//! Every wait loop here is bounded and fails soft (logs + returns
//! false/None) instead of panicking or spinning forever — same lesson as
//! the NVMe/AHCI drivers this session: an under-budgeted or unbounded
//! polling loop during boot-time driver init is exactly the bug class that
//! freezes the whole OS before a single frame ever renders.

use spin::Mutex;
use crate::{paging, pci, pmm, serial, vmm};

// ── virtio-pci modern transport ───────────────────────────────────────────────

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE:        usize = 0x04;
const COMMON_GUEST_FEATURE_SELECT:  usize = 0x08;
const COMMON_GUEST_FEATURE:         usize = 0x0C;
const COMMON_NUM_QUEUES:            usize = 0x12;
const COMMON_DEVICE_STATUS:         usize = 0x14;
const COMMON_QUEUE_SELECT:          usize = 0x16;
const COMMON_QUEUE_SIZE:            usize = 0x18;
const COMMON_QUEUE_ENABLE:          usize = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF:      usize = 0x1E;
const COMMON_QUEUE_DESC:            usize = 0x20;
const COMMON_QUEUE_DRIVER:          usize = 0x28; // "avail" ring
const COMMON_QUEUE_DEVICE:          usize = 0x30; // "used" ring

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER:      u8 = 2;
const STATUS_DRIVER_OK:   u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

const FEATURE_VERSION_1: u32 = 1 << 0; // bit 32 overall; word index 1, bit 0

const DESC_F_NEXT:  u16 = 1;
const DESC_F_WRITE: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Desc { addr: u64, len: u32, flags: u16, next: u16 }

fn r8(base: *mut u8, off: usize) -> u8 { unsafe { (base.add(off) as *const u8).read_volatile() } }
fn r16(base: *mut u8, off: usize) -> u16 { unsafe { (base.add(off) as *const u16).read_volatile() } }
fn r32(base: *mut u8, off: usize) -> u32 { unsafe { (base.add(off) as *const u32).read_volatile() } }
fn w8(base: *mut u8, off: usize, v: u8) { unsafe { (base.add(off) as *mut u8).write_volatile(v) } }
fn w16(base: *mut u8, off: usize, v: u16) { unsafe { (base.add(off) as *mut u16).write_volatile(v) } }
fn w32(base: *mut u8, off: usize, v: u32) { unsafe { (base.add(off) as *mut u32).write_volatile(v) } }
fn w64(base: *mut u8, off: usize, v: u64) { unsafe { (base.add(off) as *mut u64).write_volatile(v) } }

struct BarCap { bar: u8, offset: u32, length: u32 }

/// Reads a device's raw BAR value and resolves it to a physical address,
/// handling the 64-bit-pair case (bits[2:1] of the low dword == 0b10).
fn bar_phys(dev: &pci::PciDevice, bar_idx: u8) -> u64 {
    let lo = dev.bar(bar_idx);
    if lo & 0x6 == 0x4 {
        let hi = dev.bar(bar_idx + 1);
        ((hi as u64) << 32) | (lo & !0xF) as u64
    } else {
        (lo & !0xF) as u64
    }
}

/// Walks the PCI capability list looking for the three virtio-pci
/// structures we need (common config, notify config, device config).
/// Returns `(common, (notify, notify_off_multiplier), device_cfg)`.
fn find_virtio_caps(dev: &pci::PciDevice) -> (Option<BarCap>, Option<(BarCap, u32)>, Option<BarCap>) {
    let mut common = None;
    let mut notify = None;
    let mut device_cfg = None;

    let mut ptr = pci::config_read8(dev.bus, dev.dev, dev.func, 0x34) & 0xFC;
    let mut guard = 0; // bounded — a malformed/looping capability list must never hang boot
    while ptr != 0 && guard < 64 {
        guard += 1;
        let cap_vndr = pci::config_read8(dev.bus, dev.dev, dev.func, ptr);
        let cap_next = pci::config_read8(dev.bus, dev.dev, dev.func, ptr + 1);
        if cap_vndr == 0x09 { // vendor-specific capability
            let cfg_type = pci::config_read8(dev.bus, dev.dev, dev.func, ptr + 3);
            let bar      = pci::config_read8(dev.bus, dev.dev, dev.func, ptr + 4);
            let offset   = pci::config_read32(dev.bus, dev.dev, dev.func, ptr + 8);
            let length   = pci::config_read32(dev.bus, dev.dev, dev.func, ptr + 12);
            let bc = BarCap { bar, offset, length };
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => common = Some(bc),
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    let mult = pci::config_read32(dev.bus, dev.dev, dev.func, ptr + 16);
                    notify = Some((bc, mult));
                }
                VIRTIO_PCI_CAP_DEVICE_CFG => device_cfg = Some(bc),
                _ => {}
            }
        }
        ptr = cap_next & 0xFC;
    }
    (common, notify, device_cfg)
}

fn map_cap(dev: &pci::PciDevice, cap: &BarCap) -> *mut u8 {
    let base = bar_phys(dev, cap.bar);
    let map_len = (cap.offset as u64 + cap.length as u64).max(4096) as usize;
    let mapped = paging::map_mmio(base, map_len);
    unsafe { mapped.add(cap.offset as usize) }
}

// ── virtio-gpu protocol ────────────────────────────────────────────────────────

const CMD_GET_DISPLAY_INFO:        u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D:      u32 = 0x0101;
const CMD_SET_SCANOUT:             u32 = 0x0103;
const CMD_RESOURCE_FLUSH:          u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D:     u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

const RESP_OK_NODATA:       u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

const FORMAT_B8G8R8X8_UNORM: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct CtrlHdr { cmd_type: u32, flags: u32, fence_id: u64, ctx_id: u32, padding: u32 } // 24 bytes

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect { x: u32, y: u32, w: u32, h: u32 } // 16 bytes

struct VirtioGpuInner {
    common: *mut u8,
    notify_base: *mut u8,
    notify_off_multiplier: u32,
    queue_notify_off: u16,
    qsize: u16,
    desc_virt:  *mut Desc,
    avail_virt: *mut u8,
    used_virt:  *mut u8,
    avail_idx:  u16, // next avail slot we'll fill (host-independent, ours to track)
    used_seen:  u16, // last used.idx we've consumed
    req_phys: u64, req_virt: *mut u8,
    resp_phys: u64, resp_virt: *mut u8,
}
unsafe impl Send for VirtioGpuInner {}

pub static GPU: Mutex<Option<VirtioGpuInner>> = Mutex::new(None);

pub fn is_available() -> bool { GPU.lock().is_some() }

pub fn init(devs: &[pci::PciDevice]) -> bool {
    let dev = match devs.iter().find(|d| d.vendor_id == 0x1AF4 && d.device_id == 0x1050) {
        Some(d) => d,
        None => { serial::print("virtio-gpu: not found\n"); return false; }
    };
    serial::print("virtio-gpu: found device\n");

    let cmd = pci::config_read16(dev.bus, dev.dev, dev.func, 0x04);
    pci::config_write32(dev.bus, dev.dev, dev.func, 0x04, (cmd | 0x06) as u32);

    let (common_cap, notify_cap, _device_cfg_cap) = find_virtio_caps(dev);
    let (Some(common_cap), Some((notify_cap, notify_mult))) = (common_cap, notify_cap) else {
        serial::print("virtio-gpu: missing common/notify capability\n");
        return false;
    };
    let common = map_cap(dev, &common_cap);
    let notify_base = map_cap(dev, &notify_cap);
    serial::print("virtio-gpu: config regions mapped\n");

    // Reset, then the standard virtio init handshake.
    w8(common, COMMON_DEVICE_STATUS, 0);
    w8(common, COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
    w8(common, COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Only ever ack VIRTIO_F_VERSION_1 — we don't need any optional features
    // (EDID, multiple scanouts, resource UUIDs, etc.) for this proof-of-driver.
    w32(common, COMMON_DEVICE_FEATURE_SELECT, 1);
    let hi_features = r32(common, COMMON_DEVICE_FEATURE);
    if hi_features & FEATURE_VERSION_1 == 0 {
        serial::print("virtio-gpu: device doesn't offer VIRTIO_F_VERSION_1\n");
        return false;
    }
    w32(common, COMMON_GUEST_FEATURE_SELECT, 0);
    w32(common, COMMON_GUEST_FEATURE, 0);
    w32(common, COMMON_GUEST_FEATURE_SELECT, 1);
    w32(common, COMMON_GUEST_FEATURE, FEATURE_VERSION_1);

    w8(common, COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    let status = r8(common, COMMON_DEVICE_STATUS);
    if status & STATUS_FEATURES_OK == 0 {
        serial::print("virtio-gpu: device rejected feature set\n");
        return false;
    }

    // Set up controlq (queue index 0) — the only queue this driver uses;
    // cursorq (index 1) is left disabled since we never move a HW cursor.
    w16(common, COMMON_QUEUE_SELECT, 0);
    let qsize = r16(common, COMMON_QUEUE_SIZE);
    if qsize == 0 { serial::print("virtio-gpu: controlq size 0\n"); return false; }
    let qsize = qsize.min(128); // plenty for one-command-at-a-time use

    let desc_phys  = match pmm::alloc_page() { Some(p) => p, None => return false };
    let avail_phys = match pmm::alloc_page() { Some(p) => p, None => return false };
    let used_phys  = match pmm::alloc_page() { Some(p) => p, None => return false };
    let req_phys   = match pmm::alloc_page() { Some(p) => p, None => return false };
    let resp_phys  = match pmm::alloc_page() { Some(p) => p, None => return false };
    unsafe {
        core::ptr::write_bytes(vmm::phys_to_virt(desc_phys), 0, 4096);
        core::ptr::write_bytes(vmm::phys_to_virt(avail_phys), 0, 4096);
        core::ptr::write_bytes(vmm::phys_to_virt(used_phys), 0, 4096);
    }

    w64(common, COMMON_QUEUE_DESC, desc_phys);
    w64(common, COMMON_QUEUE_DRIVER, avail_phys);
    w64(common, COMMON_QUEUE_DEVICE, used_phys);
    let queue_notify_off = r16(common, COMMON_QUEUE_NOTIFY_OFF);
    w16(common, COMMON_QUEUE_ENABLE, 1);

    w8(common, COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    serial::print("virtio-gpu: controlq ready, DRIVER_OK set\n");

    let inner = VirtioGpuInner {
        common, notify_base, notify_off_multiplier: notify_mult, queue_notify_off, qsize,
        desc_virt: vmm::phys_to_virt(desc_phys) as *mut Desc,
        avail_virt: vmm::phys_to_virt(avail_phys),
        used_virt: vmm::phys_to_virt(used_phys),
        avail_idx: 0, used_seen: 0,
        req_phys, req_virt: vmm::phys_to_virt(req_phys),
        resp_phys, resp_virt: vmm::phys_to_virt(resp_phys),
    };
    *GPU.lock() = Some(inner);
    serial::print("virtio-gpu: init OK\n");
    true
}

/// Submits one command: copies `req` into the scratch request buffer, chains
/// a device-readable descriptor (the request) to a device-writable one (a
/// `resp_len`-byte response buffer), notifies the device, and polls the used
/// ring for completion. Returns a slice over the response bytes, or `None`
/// on a bounded timeout.
fn submit(gpu: &mut VirtioGpuInner, req: &[u8], resp_len: usize) -> Option<&'static [u8]> {
    unsafe {
        core::ptr::copy_nonoverlapping(req.as_ptr(), gpu.req_virt, req.len());
        core::ptr::write_bytes(gpu.resp_virt, 0, resp_len);

        // Two descriptors: [0] = request (device reads), chained to
        // [1] = response (device writes). Always the same two slots — this
        // driver only ever has one command in flight at a time (fully
        // synchronous: submit, then poll the used ring to completion before
        // returning), so there's no risk of a reused descriptor racing a
        // not-yet-processed previous command.
        gpu.desc_virt.add(0).write_volatile(Desc {
            addr: gpu.req_phys, len: req.len() as u32, flags: DESC_F_NEXT, next: 1,
        });
        gpu.desc_virt.add(1).write_volatile(Desc {
            addr: gpu.resp_phys, len: resp_len as u32, flags: DESC_F_WRITE, next: 0,
        });

        // avail.ring[idx % qsize] = 0; avail.idx += 1 (avail layout: flags:u16, idx:u16, ring:[u16])
        let head = (gpu.avail_idx % gpu.qsize) as usize;
        let ring_ptr = gpu.avail_virt.add(4) as *mut u16;
        ring_ptr.add(head).write_volatile(0u16);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let idx_ptr = gpu.avail_virt.add(2) as *mut u16;
        gpu.avail_idx = gpu.avail_idx.wrapping_add(1);
        idx_ptr.write_volatile(gpu.avail_idx);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // Notify: write the queue index to notify_base + queue_notify_off*multiplier.
        let notify_addr = gpu.notify_base.add(gpu.queue_notify_off as usize * gpu.notify_off_multiplier as usize);
        (notify_addr as *mut u16).write_volatile(0);

        // used layout: flags:u16, idx:u16, ring:[{id:u32,len:u32}]
        let used_idx_ptr = gpu.used_virt.add(2) as *const u16;
        for _ in 0..200_000_000u32 {
            if used_idx_ptr.read_volatile() != gpu.used_seen { break; }
            core::hint::spin_loop();
        }
        if used_idx_ptr.read_volatile() == gpu.used_seen {
            serial::print("virtio-gpu: command timeout\n");
            return None;
        }
        gpu.used_seen = gpu.used_seen.wrapping_add(1);

        Some(core::slice::from_raw_parts(gpu.resp_virt, resp_len))
    }
}

fn ctrl_hdr(cmd_type: u32) -> CtrlHdr {
    CtrlHdr { cmd_type, flags: 0, fence_id: 0, ctx_id: 0, padding: 0 }
}

fn as_bytes<T>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// GET_DISPLAY_INFO — the simplest possible round trip, good for verifying
/// the whole transport (capability walk, feature negotiation, virtqueue
/// mechanics, notify, used-ring polling) actually works end to end.
/// Returns (width, height) of scanout 0 if it's enabled.
pub fn get_display_info() -> Option<(u32, u32)> {
    let mut guard = GPU.lock();
    let gpu = guard.as_mut()?;
    let resp = submit(gpu, as_bytes(&ctrl_hdr(CMD_GET_DISPLAY_INFO)), 24 + 16 * 24)?;
    let resp_type = u32::from_ne_bytes(resp[0..4].try_into().unwrap());
    if resp_type != RESP_OK_DISPLAY_INFO {
        serial::print("virtio-gpu: GET_DISPLAY_INFO bad response\n");
        return None;
    }
    // pmodes[0]: rect (x,y,w,h) then enabled:u32, flags:u32 — starts right after the 24-byte header.
    let w = u32::from_ne_bytes(resp[32..36].try_into().unwrap());
    let h = u32::from_ne_bytes(resp[36..40].try_into().unwrap());
    let enabled = u32::from_ne_bytes(resp[40..44].try_into().unwrap());
    if enabled == 0 { return None; }
    Some((w, h))
}

/// Creates a 2D resource, attaches a backing buffer, sets it as scanout 0,
/// then transfers + flushes it to the display — a full round trip proving
/// resource creation and pixel push work, not just command/response plumbing.
/// `pixels_phys` must point at `width*height*4` bytes of BGRX8888 data.
pub fn show_resource(resource_id: u32, width: u32, height: u32, pixels_phys: u64) -> bool {
    let mut guard = GPU.lock();
    let Some(gpu) = guard.as_mut() else { return false; };

    #[repr(C)]
    struct Create2D { hdr: CtrlHdr, resource_id: u32, format: u32, width: u32, height: u32 }
    let create = Create2D { hdr: ctrl_hdr(CMD_RESOURCE_CREATE_2D), resource_id, format: FORMAT_B8G8R8X8_UNORM, width, height };
    let Some(resp) = submit(gpu, as_bytes(&create), 24) else { return false; };
    if u32::from_ne_bytes(resp[0..4].try_into().unwrap()) != RESP_OK_NODATA {
        serial::print("virtio-gpu: RESOURCE_CREATE_2D failed\n"); return false;
    }

    #[repr(C)]
    struct AttachBacking { hdr: CtrlHdr, resource_id: u32, nr_entries: u32, addr: u64, length: u32, padding: u32 }
    let attach = AttachBacking {
        hdr: ctrl_hdr(CMD_RESOURCE_ATTACH_BACKING), resource_id, nr_entries: 1,
        addr: pixels_phys, length: width * height * 4, padding: 0,
    };
    let Some(resp) = submit(gpu, as_bytes(&attach), 24) else { return false; };
    if u32::from_ne_bytes(resp[0..4].try_into().unwrap()) != RESP_OK_NODATA {
        serial::print("virtio-gpu: RESOURCE_ATTACH_BACKING failed\n"); return false;
    }

    #[repr(C)]
    struct SetScanout { hdr: CtrlHdr, r: Rect, scanout_id: u32, resource_id: u32 }
    let scanout = SetScanout { hdr: ctrl_hdr(CMD_SET_SCANOUT), r: Rect { x: 0, y: 0, w: width, h: height }, scanout_id: 0, resource_id };
    let Some(resp) = submit(gpu, as_bytes(&scanout), 24) else { return false; };
    if u32::from_ne_bytes(resp[0..4].try_into().unwrap()) != RESP_OK_NODATA {
        serial::print("virtio-gpu: SET_SCANOUT failed\n"); return false;
    }

    #[repr(C)]
    struct Transfer2D { hdr: CtrlHdr, r: Rect, offset: u64, resource_id: u32, padding: u32 }
    let xfer = Transfer2D { hdr: ctrl_hdr(CMD_TRANSFER_TO_HOST_2D), r: Rect { x: 0, y: 0, w: width, h: height }, offset: 0, resource_id, padding: 0 };
    let Some(resp) = submit(gpu, as_bytes(&xfer), 24) else { return false; };
    if u32::from_ne_bytes(resp[0..4].try_into().unwrap()) != RESP_OK_NODATA {
        serial::print("virtio-gpu: TRANSFER_TO_HOST_2D failed\n"); return false;
    }

    #[repr(C)]
    struct Flush { hdr: CtrlHdr, r: Rect, resource_id: u32, padding: u32 }
    let flush = Flush { hdr: ctrl_hdr(CMD_RESOURCE_FLUSH), r: Rect { x: 0, y: 0, w: width, h: height }, resource_id, padding: 0 };
    let Some(resp) = submit(gpu, as_bytes(&flush), 24) else { return false; };
    if u32::from_ne_bytes(resp[0..4].try_into().unwrap()) != RESP_OK_NODATA {
        serial::print("virtio-gpu: RESOURCE_FLUSH failed\n"); return false;
    }

    true
}
