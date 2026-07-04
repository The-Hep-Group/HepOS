//! Intel High Definition Audio (HDA) driver.
//!
//! Detects the HDA controller (PCI class 0x04/0x03), maps its MMIO BAR,
//! resets the controller, configures QEMU's hda-duplex codec via the
//! Immediate Command interface, sets up a single output stream, and plays
//! PCM audio.
//!
//! Public API:
//!   `init(devs)`  — call once during boot; returns true if HDA found.
//!   `beep(hz, ms)` — play a square-wave tone (non-blocking if called from
//!                    a task; HDA DMA drives playback independently).

use spin::Mutex;
use crate::{paging, pci, pmm, serial, vmm};

// ── HDA global controller registers (byte offsets from BAR0) ─────────────────
const GCAP:     usize = 0x00; // u16 – Global Capabilities
const GCTL:     usize = 0x08; // u32 – Global Control
const STATESTS:  usize = 0x0E; // u16 – State Change Status (codec bitmask)
const IC:       usize = 0x60; // u32 – Immediate Command
const IR:       usize = 0x64; // u32 – Immediate Response
const IRS:      usize = 0x68; // u16 – Immediate Response Status

// ── Stream descriptor register offsets (relative to stream base) ─────────────
const SD_CTL:   usize = 0x00; // u32 – Control + Status (bytes 0-3)
const SD_CBL:   usize = 0x08; // u32 – Cyclic Buffer Length
const SD_LVI:   usize = 0x0C; // u16 – Last Valid Index (= BDL entries - 1)
const SD_FMT:   usize = 0x10; // u16 – Stream Format
const SD_BDPL:  usize = 0x18; // u32 – BDL Lower Base Address
const SD_BDPU:  usize = 0x1C; // u32 – BDL Upper Base Address

// ── Stream descriptor control bits (byte 0 of SD_CTL) ────────────────────────
const SD_CTL_SRST: u32 = 1 << 0; // stream reset
const SD_CTL_RUN:  u32 = 1 << 1; // stream run
const SD_CTL_IOCE: u32 = 1 << 2; // interrupt on completion enable
// BCIS = Buffer Completion Interrupt Status is byte 3 of the 4-byte block at
// SD_CTL; since we do 32-bit reads, it appears in bits[26] (bit 2 of byte 3).
// Byte 3 offset = SD_CTL + 3; bit 2 of that byte → bit 26 in the 32-bit word.
const SD_STS_BCIS: u32 = 1 << 26;

// ── BDL entry (Buffer Descriptor List) ───────────────────────────────────────
#[repr(C)]
struct BdlEntry {
    addr: u64, // physical address of PCM buffer
    len:  u32, // length in bytes
    ioc:  u32, // bit 0 = interrupt on completion
}

// ── Driver state ──────────────────────────────────────────────────────────────
struct Hda {
    mmio:   *mut u8, // virtual MMIO base
    sd_off: usize,   // byte offset of the output stream descriptor from mmio
}
unsafe impl Send for Hda {}

static HDA: Mutex<Option<Hda>> = Mutex::new(None);

pub fn is_available() -> bool { HDA.lock().is_some() }

// ── MMIO helpers ──────────────────────────────────────────────────────────────

#[inline(always)]
fn r8(base: *mut u8, off: usize) -> u8 {
    unsafe { (base.add(off) as *const u8).read_volatile() }
}
#[inline(always)]
fn r16(base: *mut u8, off: usize) -> u16 {
    unsafe { (base.add(off) as *const u16).read_volatile() }
}
#[inline(always)]
fn r32(base: *mut u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_volatile() }
}
#[inline(always)]
fn w16(base: *mut u8, off: usize, v: u16) {
    unsafe { (base.add(off) as *mut u16).write_volatile(v); }
}
#[inline(always)]
fn w32(base: *mut u8, off: usize, v: u32) {
    unsafe { (base.add(off) as *mut u32).write_volatile(v); }
}

fn spin(n: u32) {
    for _ in 0..n {
        core::hint::spin_loop();
    }
}

// TSC used purely for timeout, not for precise timing.
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem),
        );
    }
    ((hi as u64) << 32) | lo as u64
}

// ── Immediate Command interface ───────────────────────────────────────────────
//
// Sends one codec verb and returns the response word.
// codec address (cad), node ID (nid), 20-bit verb payload (verb).
fn send_verb(mmio: *mut u8, cad: u8, nid: u8, verb: u32) -> u32 {
    let cmd = ((cad as u32) << 28) | ((nid as u32) << 20) | (verb & 0x000F_FFFF);

    // Wait for ICB (bit 0 of IRS) to clear (previous command done)
    for _ in 0..100_000u32 {
        if r16(mmio, IRS) & 1 == 0 { break; }
        spin(10);
    }

    w32(mmio, IC, cmd);
    // Set ICB only (bit 0). Writing bit 1 would clear IRV since it's W1C,
    // but we don't want to touch it here — just trigger the command.
    w16(mmio, IRS, 0x01);

    // Wait for IRV (bit 1 set by hardware when response is ready)
    for _ in 0..200_000u32 {
        let irs = r16(mmio, IRS);
        if irs & 2 != 0 {
            w16(mmio, IRS, 0x02); // W1C: clear IRV
            return r32(mmio, IR);
        }
        spin(10);
    }
    0 // timeout — codec not responding
}

// Helpers for the two verb encodings used by HDA:
//   4-bit verb: opcode in bits[19:16], 16-bit data in bits[15:0]
//   12-bit verb: opcode in bits[19:8],  8-bit data in bits[7:0]
fn verb4(mmio: *mut u8, nid: u8, opcode: u32, data: u32) -> u32 {
    send_verb(mmio, 0, nid, (opcode << 16) | (data & 0xFFFF))
}
fn verb12(mmio: *mut u8, nid: u8, opcode: u32, data: u32) -> u32 {
    send_verb(mmio, 0, nid, (opcode << 8) | (data & 0xFF))
}

// ── Codec configuration ───────────────────────────────────────────────────────
//
// QEMU hda-duplex codec topology (hard-coded):
//   node 1 = AFG (Audio Function Group)
//   node 2 = Output DAC (converter)
//   node 3 = Input ADC
//   node 4 = Output Pin
//   node 5 = Input Pin
//
// stream_id: 1-indexed stream assigned to DAC (must match the SD stream number)
// fmt: 16-bit HDA format word
fn configure_codec(mmio: *mut u8, stream_id: u8, fmt: u16) {
    // Power on function group and DAC
    verb12(mmio, 1, 0x705, 0x00); // AFG: Set Power State D0
    verb12(mmio, 2, 0x705, 0x00); // DAC: Set Power State D0
    spin(5_000);                   // wait for D0 settle

    // Set converter PCM format (48 kHz, 16-bit, stereo = 0x0011)
    verb4(mmio, 2, 0x2, fmt as u32);

    // Assign stream and channel (stream_id, channel 0)
    verb12(mmio, 2, 0x706, (stream_id as u32) << 4);

    // Unmute + max gain on DAC output amplifier (both channels, output)
    // payload: bit 7=right, bit 6=left, bit 4=output, bits[5:0]=gain
    verb4(mmio, 2, 0x3, 0xD07F);

    // Enable output pin
    verb12(mmio, 4, 0x707, 0x40); // Pin Widget Control: HP-Out enable
}

// ── Public init ───────────────────────────────────────────────────────────────

pub fn init(devs: &[pci::PciDevice]) -> bool {
    let dev = match devs.iter().find(|d| d.class == 0x04 && d.subclass == 0x03) {
        Some(d) => d,
        None => {
            serial::print("HDA: no device found\n");
            return false;
        }
    };
    serial::print("HDA: found controller\n");

    // Enable Memory Space + Bus Mastering
    let cmd = pci::config_read16(dev.bus, dev.dev, dev.func, 0x04);
    pci::config_write32(dev.bus, dev.dev, dev.func, 0x04, (cmd | 0x06) as u32);

    // Read BAR0 (may be 64-bit)
    let bar0 = pci::config_read32(dev.bus, dev.dev, dev.func, 0x10) as u64;
    let bar1 = pci::config_read32(dev.bus, dev.dev, dev.func, 0x14) as u64;
    let bar_phys = if (bar0 & 0x6) == 0x4 {
        (bar1 << 32) | (bar0 & !0xF)
    } else {
        bar0 & !0xF
    };

    // Map 16 KB of MMIO (covers all registers and up to 8 stream descriptors)
    let mmio = paging::map_mmio(bar_phys, 16384);
    serial::print("HDA: MMIO mapped\n");

    // ── Controller reset ──────────────────────────────────────────────────────
    // Clear CRST bit, wait until hardware confirms (GCTL.CRST=0)
    w32(mmio, GCTL, 0x00);
    for _ in 0..100_000u32 {
        if r32(mmio, GCTL) & 1 == 0 { break; }
        spin(10);
    }
    // Set CRST to bring controller out of reset
    w32(mmio, GCTL, 0x01);
    for _ in 0..100_000u32 {
        if r32(mmio, GCTL) & 1 != 0 { break; }
        spin(10);
    }
    spin(50_000); // let codecs enumerate

    // Check codec presence
    let statests = r16(mmio, STATESTS);
    if statests == 0 {
        serial::print("HDA: no codec detected\n");
        return false;
    }
    serial::print("HDA: codec detected\n");

    // Read GCAP to find number of input streams (ISS)
    let gcap  = r16(mmio, GCAP);
    let iss   = ((gcap >> 8) & 0x0F) as usize; // input stream count
    let sd_off = 0x80 + iss * 0x20;             // output stream 0 descriptor offset

    *HDA.lock() = Some(Hda { mmio, sd_off });
    serial::print("HDA: init OK\n");
    true
}

// ── Beep ──────────────────────────────────────────────────────────────────────

/// Play a square-wave beep at `freq_hz` for `duration_ms` milliseconds.
/// Blocking: returns only after the audio DMA completes.
pub fn beep(freq_hz: u32, duration_ms: u32) {
    let guard = HDA.lock();
    let hda = match guard.as_ref() {
        Some(h) => h,
        None    => return,
    };
    let mmio   = hda.mmio;
    let sd_off = hda.sd_off;
    drop(guard);

    // Sample parameters: 48 kHz, 16-bit, stereo
    let sample_rate: u32 = 48_000;
    let bytes_per_sample: u32 = 4; // 2 ch × 2 bytes
    let total_samples = (sample_rate * duration_ms) / 1000;
    let buf_bytes = (total_samples * bytes_per_sample) as usize;

    // Clamp to 1 MB (≈5.5 s at 48 kHz stereo 16-bit)
    let buf_bytes = buf_bytes.min(1 << 20).max(4096);
    let buf_pages = (buf_bytes + 4095) / 4096;

    // Allocate physically contiguous PCM buffer
    let buf_phys = match pmm::alloc_contiguous(buf_pages) {
        Some(p) => p,
        None    => { serial::print("HDA: OOM for PCM buffer\n"); return; }
    };
    let buf_virt = vmm::phys_to_virt(buf_phys) as *mut i16;

    // Generate square wave: half-period at +0x7FFF, half at -0x7FFF
    let period_samples = if freq_hz > 0 { sample_rate / freq_hz } else { 0 };
    if period_samples > 0 {
        let half = period_samples / 2;
        for i in 0..(total_samples as usize).min(buf_bytes / bytes_per_sample as usize) {
            let phase = (i as u32) % period_samples;
            let val: i16 = if phase < half { 0x7FFF } else { -0x7FFF };
            unsafe {
                buf_virt.add(i * 2).write(val);       // left
                buf_virt.add(i * 2 + 1).write(val);   // right
            }
        }
    }

    // Allocate BDL page (128-byte alignment required; page-aligned satisfies this)
    let bdl_phys = match pmm::alloc_page() {
        Some(p) => p,
        None    => {
            for i in 0..buf_pages as u64 { pmm::free_page(buf_phys + i * 4096); }
            serial::print("HDA: OOM for BDL\n");
            return;
        }
    };
    let bdl_virt = vmm::phys_to_virt(bdl_phys) as *mut BdlEntry;
    unsafe {
        bdl_virt.write(BdlEntry {
            addr: buf_phys,
            len:  buf_bytes as u32,
            ioc:  1, // interrupt on completion
        });
    }

    // Stream number 1, format word: 48 kHz (base=0), ×1, /1, 16-bit (1), 2ch (1) = 0x0011
    let stream_id: u8 = 1;
    let fmt: u16 = 0x0011;

    configure_codec(mmio, stream_id, fmt);

    // ── Stream descriptor setup ───────────────────────────────────────────────
    // Reset stream
    w32(mmio, sd_off + SD_CTL, SD_CTL_SRST);
    for _ in 0..10_000u32 {
        if r32(mmio, sd_off + SD_CTL) & SD_CTL_SRST != 0 { break; }
        spin(10);
    }
    w32(mmio, sd_off + SD_CTL, 0);
    for _ in 0..10_000u32 {
        if r32(mmio, sd_off + SD_CTL) & SD_CTL_SRST == 0 { break; }
        spin(10);
    }

    // Clear any pending status bits in SDnSTS (W1C).
    // BCIS=bit26, FIFOE=bit27, DESE=bit28 in the 32-bit read of the 4-byte CTL+STS block.
    w32(mmio, sd_off + SD_CTL, 0x1C00_0000);

    // BDL address
    w32(mmio, sd_off + SD_BDPL, bdl_phys as u32);
    w32(mmio, sd_off + SD_BDPU, (bdl_phys >> 32) as u32);

    // Cyclic buffer length = total bytes in all BDL entries
    w32(mmio, sd_off + SD_CBL, buf_bytes as u32);

    // Last Valid Index = number of BDL entries - 1
    w16(mmio, sd_off + SD_LVI, 0);

    // Stream format
    w16(mmio, sd_off + SD_FMT, fmt);

    // Write stream number into CTL[23:20]; also enable IOCE
    let ctl_base = ((stream_id as u32) << 20) | SD_CTL_IOCE;
    w32(mmio, sd_off + SD_CTL, ctl_base);

    // Start stream
    w32(mmio, sd_off + SD_CTL, ctl_base | SD_CTL_RUN);

    // Wait for BCIS (DMA finished the BDL), with a hard TSC deadline as fallback.
    // TSC runs at ~CPU frequency; assume ≥1 GHz so 1M ticks ≥ 1ms.
    // We give the stream 3× its nominal duration before forcing a stop.
    let tsc_deadline = rdtsc() + 1_000_000u64 * (duration_ms as u64 * 3 + 500);
    loop {
        if r32(mmio, sd_off + SD_CTL) & SD_STS_BCIS != 0 { break; }
        if rdtsc() >= tsc_deadline { break; }
        spin(100);
    }

    // Stop stream, then clear status bits (W1C)
    w32(mmio, sd_off + SD_CTL, 0);
    w32(mmio, sd_off + SD_CTL, 0x1C00_0000);

    // Free buffers
    pmm::free_page(bdl_phys);
    for i in 0..buf_pages as u64 {
        pmm::free_page(buf_phys + i * 4096);
    }
}
