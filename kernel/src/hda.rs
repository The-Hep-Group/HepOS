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
const SD_CTL:  usize = 0x00;
const SD_CBL:  usize = 0x08;
const SD_LVI:  usize = 0x0C;
const SD_FMT:  usize = 0x10;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN:  u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;

#[repr(C)]
struct BdlEntry { addr: u64, len: u32, ioc: u32 }

// ── Driver state ──────────────────────────────────────────────────────────────
struct Hda {
    mmio:   *mut u8,
    sd_off: usize,
}
unsafe impl Send for Hda {}

static HDA: Mutex<Option<Hda>> = Mutex::new(None);

pub fn is_available() -> bool { HDA.lock().is_some() }

// ── MMIO helpers ──────────────────────────────────────────────────────────────

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

// ── TSC + PIT-calibrated timing ──────────────────────────────────────────────
//
// beep() must wait `duration_ms` without reading HDA MMIO (QEMU's HDA MMIO
// emulation is extremely slow while DMA is active).  TICK_COUNT from the
// scheduler is not usable because tasks run with IF=0 after the first
// context-switch, so the APIC timer ISR never fires again.
//
// Solution: calibrate TSC against PIT timer 2 once at init time, then use
// `rdtsc()` for all delays.  TSC advances regardless of IF flag.

pub static TSC_PER_MS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(1_000_000); // sane default (1 GHz TSC)

#[inline(always)]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | lo as u64
}

#[inline(always)]
fn pit_in(port: u16) -> u8 {
    let v: u8;
    unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)); }
    v
}
#[inline(always)]
fn pit_out(port: u16, v: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)); }
}

/// Measure TSC frequency using PIT timer 2 as a ~10 ms reference.
/// Must be called with interrupts disabled (as is the case during init).
fn calibrate_tsc() {
    let old61 = pit_in(0x61);

    // Program PIT timer 2: mode 0 (terminal count), LSB+MSB, binary
    pit_out(0x43, 0xB0);
    // 11932 counts at 1,193,182 Hz ≈ 10 ms
    pit_out(0x42, (11932u16 & 0xFF) as u8);
    pit_out(0x42, (11932u16 >> 8)   as u8);

    // Gate timer 2 (bit 0 = 1), speaker off (bit 1 = 0)
    pit_out(0x61, (old61 | 0x01) & !0x02);

    let t0 = rdtsc();
    // Poll OUT pin (bit 5 of port 0x61); bound the wait so we can't hang here.
    // At the 1GHz TSC default, 100M iterations easily covers 50ms.
    for _ in 0..100_000_000u32 {
        if pit_in(0x61) & 0x20 != 0 { break; }
        core::hint::spin_loop();
    }
    let t1 = rdtsc();

    pit_out(0x61, old61); // restore

    // 10ms reference → divide by 10 for cycles/ms
    let elapsed = t1.wrapping_sub(t0);
    if elapsed > 0 {
        TSC_PER_MS.store(elapsed / 10, core::sync::atomic::Ordering::Relaxed);
        serial::print("HDA: TSC calibrated\n");
    }
}

// ── Immediate Command interface ───────────────────────────────────────────────
//
// Sends one codec verb and returns the response word.
// codec address (cad), node ID (nid), 20-bit verb payload (verb).
fn send_verb(mmio: *mut u8, cad: u8, nid: u8, verb: u32) -> u32 {
    let cmd = ((cad as u32) << 28) | ((nid as u32) << 20) | (verb & 0x000F_FFFF);

    // Wait for ICB (bit 0) to clear — 1 000 iterations max (~10 µs).
    // QEMU processes IC commands synchronously so this normally exits on iter 1.
    for _ in 0..1_000u32 {
        if r16(mmio, IRS) & 1 == 0 { break; }
        spin(10);
    }

    w32(mmio, IC, cmd);
    w16(mmio, IRS, 0x01); // set ICB to trigger command

    // Wait for IRV (bit 1) — bounded to 1 000 iterations for the same reason.
    for _ in 0..1_000u32 {
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
    // Calibrate TSC now, while interrupts are still off and before HDA DMA starts.
    calibrate_tsc();

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

    let gcap   = r16(mmio, GCAP);
    let iss    = ((gcap >> 8) & 0x0F) as usize;
    let sd_off = 0x80 + iss * 0x20; // first output stream descriptor

    *HDA.lock() = Some(Hda { mmio, sd_off });
    serial::print("HDA: init OK\n");
    true
}

// ── Beep ──────────────────────────────────────────────────────────────────────

/// Play a square-wave beep at `freq_hz` for `duration_ms` milliseconds.
///
/// Stop strategy: after the tone duration, zero the DMA buffer IN-PLACE while
/// the stream is still running.  QEMU's HDA timer reads guest memory each
/// ~21 ms period, so the next read delivers silence to SDL.  We then wait
/// 200 ms for SDL to drain before stopping the stream.  This adds 200 ms of
/// inaudible tail but guarantees the tone stops cleanly regardless of SDL's
/// internal buffer size.
///
/// The stop write preserves the stream-number field (bits [23:20]) so QEMU
/// can match the running stream; writing 0 to those bits previously caused
/// QEMU to look for "stream 0" (not running) and leave the stream active.
pub fn beep(freq_hz: u32, duration_ms: u32) {
    let guard  = HDA.lock();
    let hda    = match guard.as_ref() { Some(h) => h, None => return };
    let mmio   = hda.mmio;
    let sd_off = hda.sd_off;
    drop(guard);

    let sample_rate:    u32 = 48_000;
    let bytes_per_samp: u32 = 4; // stereo 16-bit
    let total_samples = (sample_rate * duration_ms) / 1_000;
    let buf_bytes = ((total_samples * bytes_per_samp) as usize).min(1 << 20).max(4096);
    let buf_pages = (buf_bytes + 4095) / 4096;

    let buf_phys = match pmm::alloc_contiguous(buf_pages) {
        Some(p) => p,
        None    => { serial::print("HDA: OOM\n"); return; }
    };
    let buf_virt = vmm::phys_to_virt(buf_phys) as *mut i16;

    // Pre-zero so any unwritten tail is silent.
    unsafe { core::ptr::write_bytes(buf_virt as *mut u8, 0, buf_bytes); }

    // Generate square wave.
    let period_samp = if freq_hz > 0 { sample_rate / freq_hz } else { 0 };
    if period_samp > 0 {
        let half = period_samp / 2;
        let n    = (total_samples as usize).min(buf_bytes / 4);
        for i in 0..n {
            let val: i16 = if (i as u32) % period_samp < half { 0x7FFF } else { -0x7FFF };
            unsafe { buf_virt.add(i*2).write(val); buf_virt.add(i*2+1).write(val); }
        }
    }

    let bdl_phys = match pmm::alloc_page() {
        Some(p) => p,
        None    => { for i in 0..buf_pages as u64 { pmm::free_page(buf_phys + i*4096); } return; }
    };
    unsafe { (vmm::phys_to_virt(bdl_phys) as *mut BdlEntry).write(BdlEntry { addr: buf_phys, len: buf_bytes as u32, ioc: 1 }); }

    let stream_id: u8 = 1;
    let fmt: u16      = 0x0011; // 48 kHz, 16-bit, stereo
    configure_codec(mmio, stream_id, fmt);

    // Reset stream.
    w32(mmio, sd_off + SD_CTL, SD_CTL_SRST); spin(5_000);
    w32(mmio, sd_off + SD_CTL, 0);           spin(5_000);

    // Program stream descriptor.
    w32(mmio, sd_off + SD_BDPL, bdl_phys as u32);
    w32(mmio, sd_off + SD_BDPU, (bdl_phys >> 32) as u32);
    w32(mmio, sd_off + SD_CBL,  buf_bytes as u32);
    w16(mmio, sd_off + SD_LVI,  0);
    w16(mmio, sd_off + SD_FMT,  fmt);

    // ctl_base keeps stream_id in bits [23:20] — must be preserved in every
    // subsequent SD_CTL write so QEMU can identify the running stream.
    let ctl_base = ((stream_id as u32) << 20) | SD_CTL_IOCE;
    w32(mmio, sd_off + SD_CTL, ctl_base | SD_CTL_RUN);

    let tsc_per_ms = TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed);

    // Wait for the tone to play.
    let tone_end = rdtsc().wrapping_add(tsc_per_ms.saturating_mul(duration_ms as u64));
    while rdtsc().wrapping_sub(tone_end) > u64::MAX / 2 { core::hint::spin_loop(); }

    // Zero the DMA buffer while the stream is still running.
    // QEMU's HDA timer will read zeros on the next ~21 ms tick → SDL receives
    // silence.  The stream keeps running (reading zeros) during the drain wait.
    unsafe { core::ptr::write_bytes(buf_virt as *mut u8, 0, buf_bytes); }

    // Wait 200 ms for SDL to drain its internal queue to silence.
    let drain_end = rdtsc().wrapping_add(tsc_per_ms.saturating_mul(200));
    while rdtsc().wrapping_sub(drain_end) > u64::MAX / 2 { core::hint::spin_loop(); }

    // Stop stream — preserve stream_id so QEMU matches the running stream.
    w32(mmio, sd_off + SD_CTL, ctl_base);                spin(50_000); // RUN=0
    w32(mmio, sd_off + SD_CTL, ctl_base | SD_CTL_SRST);  spin(50_000); // SRST=1
    w32(mmio, sd_off + SD_CTL, ctl_base);                spin(50_000); // SRST=0

    pmm::free_page(bdl_phys);
    for i in 0..buf_pages as u64 { pmm::free_page(buf_phys + i*4096); }
}
