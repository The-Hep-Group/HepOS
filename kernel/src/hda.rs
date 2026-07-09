//! Intel High Definition Audio (HDA) driver.
//!
//! Detects the HDA controller (PCI class 0x04/0x03), maps its MMIO BAR,
//! resets the controller, and hands off to a persistent userspace process
//! (`userspace/hdad`) that configures QEMU's hda-duplex codec via the
//! Immediate Command interface, sets up the single output stream, and plays
//! PCM audio — HepOS's second real "driver moved to userspace" migration
//! (PLAN.md's "move drivers to userspace libOS" item), following the exact
//! pattern RTL8139's did: one-time hardware bring-up stays in the kernel
//! (needs `pmm`/PCI access no ring-3 process has), the ongoing hot-path
//! (here: codec verbs + DMA position tracking, there: TX/RX polling) moves
//! to a persistent userspace process talking to the kernel through a shared
//! `Mailbox` page. Every caller of this module's public API
//! (`beep`/`play_pcm`/`poll`/`is_playing`/`progress_ms`/`set_volume`/
//! `get_volume`/`is_available`) needed zero changes — same signatures,
//! reimplemented underneath to write/read the mailbox instead of touching
//! HDA MMIO directly.
//!
//! Public API:
//!   `init(devs)`  — call once during boot; returns true if HDA found.
//!   `beep(hz, ms)` — play a square-wave tone (non-blocking if called from
//!                    a task; HDA DMA drives playback independently).

use spin::Mutex;
use crate::{paging, pci, pmm, process, serial, syscall, vmm};

// ── HDA global controller registers (byte offsets from BAR0) ─────────────────
// Only needed here for the one-time kernel-side reset/codec-detection dance;
// `userspace/hdad` has its own copy for the ongoing verb/stream-descriptor work.
const GCTL:     usize = 0x08; // u32 – Global Control
const STATESTS: usize = 0x0E; // u16 – State Change Status (codec bitmask)

fn spin(n: u32) {
    for _ in 0..n {
        core::hint::spin_loop();
    }
}

#[inline(always)]
fn r16(base: *mut u8, off: usize) -> u16 {
    unsafe { (base.add(off) as *const u16).read_volatile() }
}
#[inline(always)]
fn w32(base: *mut u8, off: usize, v: u32) {
    unsafe { (base.add(off) as *mut u32).write_volatile(v); }
}

// ── TSC + PIT-calibrated timing ──────────────────────────────────────────────
//
// `hdad` needs wall-time delays (its zero-buffer→drain→stop sequence, and
// playback-position tracking) without relying on `scheduler::TICK_COUNT` or
// any kernel state — TSC works the same in ring 3 as ring 0 (unrestricted,
// CR4.TSD is never set anywhere in this kernel), so it reads it directly via
// its own `rdtsc` (no syscall needed), the same way the kernel used to.
// Calibrated once here (still needs PIT port I/O, ring-0 only) and handed to
// the driver via the mailbox.
pub static TSC_PER_MS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(1_000_000); // sane default (1 GHz TSC)

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
    for _ in 0..100_000_000u32 {
        if pit_in(0x61) & 0x20 != 0 { break; }
        core::hint::spin_loop();
    }
    let t1 = rdtsc();

    pit_out(0x61, old61); // restore

    let elapsed = t1.wrapping_sub(t0);
    if elapsed > 0 {
        TSC_PER_MS.store(elapsed / 10, core::sync::atomic::Ordering::Relaxed);
        serial::print("HDA: TSC calibrated\n");
    }
}

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

// ── Shared memory mailbox ─────────────────────────────────────────────────────
//
// One physical page, mapped into both the kernel (`vmm::phys_to_virt`) and
// the `hdad` userspace process (`SYS_MMAP_MMIO`, using the physical address
// handed to it as its one launch argument). **Layout must stay byte-for-byte
// identical to the copy in `userspace/hdad/src/main.rs`** — no shared crate
// between them to enforce that (userspace can't depend on kernel code).
#[repr(C)]
struct Mailbox {
    mmio_phys:  u64,
    buf_phys:   u64,
    bdl_phys:   u64,
    sd_off:     u32,
    tsc_per_ms: u32,
    /// Nonzero = "please play this many i16 words (stereo-interleaved) —
    /// they're already sitting in the shared PCM buffer." Kernel sets it
    /// after writing samples; the driver clears it back to 0 once it's
    /// started the DMA stream.
    play_request: u32,
    /// Current volume (0-100) — kernel updates any time via `set_volume()`;
    /// the driver re-sends the amp/gain verb whenever it notices a change
    /// from its own last-applied value.
    volume:      u32,
    is_playing:  u32,
    elapsed_ms:  u32,
    total_ms:    u32,
    /// 0 = keep running. Kernel writes 1 to request a cooperative shutdown
    /// (`service stop hdad` / `kill <pid>`); see `stop_service()`'s doc
    /// comment for why this is cooperative rather than a true forced kill.
    stop: u32,
}

/// Max PCM buffer size (1 MB, matching the original in-kernel driver's cap —
/// ~5.4s of 48kHz stereo audio) — pre-allocated once at `init()` and reused
/// for every `beep()`/`play_pcm()` call, same "fixed reusable DMA buffer"
/// approach RTL8139's TX slots use, rather than allocating fresh per call
/// (which would need a fresh allowlist grant every time).
const PCM_MAX_BYTES: usize = 1 << 20;
const PCM_MAX_PAGES: usize = PCM_MAX_BYTES / 4096;

struct Hda {
    mailbox_virt: *mut Mailbox,
    mailbox_phys: u64,
    /// Kernel's own mapping of the shared PCM buffer — `beep()`/`play_pcm()`
    /// write samples here directly (same physical page `hdad` separately
    /// maps via `SYS_MMAP_MMIO` to feed the hardware).
    buf_virt: *mut i16,
}
unsafe impl Send for Hda {}

static HDA: Mutex<Option<Hda>> = Mutex::new(None);

pub fn is_available() -> bool { HDA.lock().is_some() }

// ── Volume ────────────────────────────────────────────────────────────────────
static VOLUME: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(100);

pub fn get_volume() -> u8 {
    VOLUME.load(core::sync::atomic::Ordering::Relaxed)
}

/// Set output volume (0-100, clamped). Takes effect the moment `hdad` next
/// notices the mailbox's `volume` field changed — within one of its
/// ~10ms-rate-limited loop iterations, same as the old in-kernel driver's
/// "immediately" was really "next time anything touches the codec".
pub fn set_volume(vol: u8) {
    let vol = vol.min(100);
    VOLUME.store(vol, core::sync::atomic::Ordering::Relaxed);
    if let Some(hda) = HDA.lock().as_ref() {
        unsafe { core::ptr::write_volatile(&mut (*hda.mailbox_virt).volume, vol as u32); }
    }
}

// ── Public init ───────────────────────────────────────────────────────────────

/// Mailbox physical address waiting to be handed to a freshly-spawned
/// `hdad` task, set by `init()` and consumed once by `spawn_pending_driver()`.
/// Can't spawn directly from `init()` — that runs during early hardware
/// bring-up, before `kmain` registers the scheduler's idle/blink tasks;
/// `scheduler::spawn()` that early corrupts the "task 0 becomes kmain's own
/// execution context" bootstrap trick (see `rtl8139.rs`'s identical fix,
/// found the hard way there first).
static PENDING_DRIVER_MAILBOX: Mutex<Option<u64>> = Mutex::new(None);

/// Launches the queued `hdad` driver process, if `init()` found the
/// hardware and queued one. Must be called only after the scheduler's
/// idle/blink tasks are registered and the timer is running (i.e. from
/// within `task_blink`'s own loop). A no-op on every call after the first.
pub fn spawn_pending_driver() {
    let Some(mailbox_phys) = PENDING_DRIVER_MAILBOX.lock().take() else { return };
    match process::exec_async_with_arg(usize::MAX, "<hdad>", HDAD_ELF, mailbox_phys) {
        Ok(()) => serial::print("HDA: hdad launched\n"),
        Err(_) => serial::print("HDA: hdad launch failed\n"),
    }
}

// ── Service management (`service`/`kill` terminal commands) ──────────────────
// Same shape as rtl8139.rs's — see that module's comments for the full
// reasoning (cooperative stop via a mailbox flag, not a true forced kill;
// enable/disable is in-memory only for this session).
static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub const SERVICE_NAME: &str = "<hdad>";
const STOP_WAIT_SPINS: u32 = 500_000_000;

pub fn is_enabled() -> bool { ENABLED.load(core::sync::atomic::Ordering::Relaxed) }
pub fn set_enabled(v: bool) { ENABLED.store(v, core::sync::atomic::Ordering::Relaxed); }
pub fn is_running() -> bool { process::is_process_running(SERVICE_NAME) }

pub fn stop_service() -> Result<(), &'static str> {
    if !is_running() { return Err("not running"); }
    let mb = { let g = HDA.lock(); g.as_ref().map(|h| h.mailbox_virt) };
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
            let g = HDA.lock();
            match g.as_ref() { Some(h) => (h.mailbox_virt, h.mailbox_phys), None => return Err("driver not initialized") }
        };
        if HDAD_ELF.is_empty() { return Err("hdad ELF not built"); }
        unsafe { core::ptr::write_volatile(&mut (*mb).stop, 0); }
        process::exec_async_with_arg(usize::MAX, SERVICE_NAME, HDAD_ELF, mailbox_phys)?;
        for _ in 0..STOP_WAIT_SPINS {
            if is_running() { return Ok(()); }
            core::hint::spin_loop();
        }
        Err("timeout waiting for driver to start")
    })();
    STARTING.store(false, Ordering::Release);
    result
}

// Baked-in hdad ELF (generated by build.rs from userspace/target/.../hdad).
include!(concat!(env!("OUT_DIR"), "/hdad_elf.rs"));

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

    // Map 16 KB of MMIO — one-time kernel-side use, just for reset + codec
    // detection below; `hdad` maps the same physical region separately for
    // its own ongoing use.
    let mmio = paging::map_mmio(bar_phys, 16384);
    serial::print("HDA: MMIO mapped\n");

    // ── Controller reset ──────────────────────────────────────────────────────
    w32(mmio, GCTL, 0x00);
    for _ in 0..100_000u32 {
        if r32_pub(mmio, GCTL) & 1 == 0 { break; }
        spin(10);
    }
    w32(mmio, GCTL, 0x01);
    for _ in 0..100_000u32 {
        if r32_pub(mmio, GCTL) & 1 != 0 { break; }
        spin(10);
    }
    spin(50_000); // let codecs enumerate

    let statests = r16(mmio, STATESTS);
    if statests == 0 {
        serial::print("HDA: no codec detected\n");
        return false;
    }
    serial::print("HDA: codec detected\n");

    let gcap   = r16(mmio, 0x00);
    let iss    = ((gcap >> 8) & 0x0F) as usize;
    let sd_off = 0x80 + iss * 0x20; // first output stream descriptor

    // Pre-allocate the reusable PCM buffer + BDL page + mailbox page.
    let buf_phys = match pmm::alloc_contiguous(PCM_MAX_PAGES) {
        Some(p) => p,
        None => { serial::print("HDA: OOM for PCM buffer\n"); return false; }
    };
    unsafe { core::ptr::write_bytes(vmm::phys_to_virt(buf_phys), 0, PCM_MAX_BYTES); }
    let buf_virt = vmm::phys_to_virt(buf_phys) as *mut i16;

    let bdl_phys = match pmm::alloc_page() {
        Some(p) => p,
        None => { serial::print("HDA: OOM for BDL\n"); return false; }
    };

    let mailbox_phys = match pmm::alloc_page() {
        Some(p) => p,
        None => { serial::print("HDA: OOM for mailbox\n"); return false; }
    };
    let mailbox_virt = vmm::phys_to_virt(mailbox_phys) as *mut Mailbox;
    unsafe {
        core::ptr::write_bytes(mailbox_virt as *mut u8, 0, 4096);
        (*mailbox_virt).mmio_phys  = bar_phys;
        (*mailbox_virt).buf_phys   = buf_phys;
        (*mailbox_virt).bdl_phys   = bdl_phys;
        (*mailbox_virt).sd_off     = sd_off as u32;
        (*mailbox_virt).tsc_per_ms = TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed) as u32;
        (*mailbox_virt).volume     = VOLUME.load(core::sync::atomic::Ordering::Relaxed) as u32;
    }

    // Grant the runtime-discovered ranges hdad needs — no fixed compile-time
    // allowlist entry could cover a PCI BAR or a pmm-allocated buffer's
    // address. HDA is MMIO-only (no port I/O), so no port grant needed.
    syscall::grant_mmio_range(bar_phys, 16384);
    syscall::grant_mmio_range(buf_phys, PCM_MAX_PAGES as u64 * 4096);
    syscall::grant_mmio_range(bdl_phys, 4096);
    syscall::grant_mmio_range(mailbox_phys, 4096);

    *PENDING_DRIVER_MAILBOX.lock() = Some(mailbox_phys);
    *HDA.lock() = Some(Hda { mailbox_virt, mailbox_phys, buf_virt });
    serial::print("HDA: init OK (hdad queued to launch once the scheduler is up)\n");
    true
}

/// Public-in-crate 32-bit MMIO read, reused by `init()`'s reset-wait loops
/// above (the private `r32`/`w32`/`r16` pair below stays crate-private).
fn r32_pub(base: *mut u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_volatile() }
}

// ── Beep ──────────────────────────────────────────────────────────────────────

/// Play a square-wave beep at `freq_hz` for `duration_ms` milliseconds.
/// Non-blocking — generates the tone into the shared PCM buffer and hands it
/// to `play_pcm()`.
pub fn beep(freq_hz: u32, duration_ms: u32) {
    let sample_rate: u32 = 48_000;
    let total_samples = ((sample_rate * duration_ms) / 1_000) as usize;
    let n = total_samples.min(PCM_MAX_BYTES / 4); // cap to the shared buffer

    let mut samples = alloc::vec![0i16; n * 2];
    let period_samp = if freq_hz > 0 { sample_rate / freq_hz } else { 0 };
    if period_samp > 0 {
        let half = period_samp / 2;
        for i in 0..n {
            let val: i16 = if (i as u32) % period_samp < half { 0x7FFF } else { -0x7FFF };
            samples[i*2] = val;
            samples[i*2 + 1] = val;
        }
    }

    play_pcm(&samples);
}

/// Start playing raw interleaved 16-bit stereo PCM at 48 kHz — returns
/// immediately; playback continues in the background, entirely driven by
/// `hdad` (see that process — `poll()` below is now a no-op). Truncates to
/// whatever fits in the shared 1 MB DMA buffer (~5.4s).
///
/// Returns `(samples_played, truncated)` — `samples_played` counts i16 words
/// (so stereo pairs = samples_played / 2).
pub fn play_pcm(samples_stereo: &[i16]) -> (usize, bool) {
    let guard = HDA.lock();
    let hda = match guard.as_ref() { Some(h) => h, None => return (0, false) };

    let max_words = PCM_MAX_BYTES / 2;
    let n = samples_stereo.len().min(max_words) & !1; // keep an even stereo-pair count
    let truncated = n < samples_stereo.len();
    if n == 0 { return (0, truncated); }

    // Wait (bounded) for `hdad` to have picked up any previous request —
    // same reasoning as RTL8139's TX mailbox wait: writing new samples into
    // the shared buffer before the driver's finished reading the old ones
    // would corrupt whatever it's mid-copying.
    for _ in 0..1_000_000u32 {
        if unsafe { core::ptr::read_volatile(&(*hda.mailbox_virt).play_request) } == 0 { break; }
        core::hint::spin_loop();
    }

    unsafe { core::ptr::copy_nonoverlapping(samples_stereo.as_ptr(), hda.buf_virt, n); }
    unsafe { core::ptr::write_volatile(&mut (*hda.mailbox_virt).play_request, n as u32); }

    (n, truncated)
}

/// Whether `hdad` currently has an active clip playing.
pub fn is_playing() -> bool {
    let guard = HDA.lock();
    match guard.as_ref() {
        Some(h) => unsafe { core::ptr::read_volatile(&(*h.mailbox_virt).is_playing) != 0 },
        None => false,
    }
}

/// (elapsed_ms, total_ms) of the currently active clip, if any — read
/// straight from the mailbox, which `hdad` updates every loop iteration.
pub fn progress_ms() -> Option<(u64, u64)> {
    let guard = HDA.lock();
    let hda = guard.as_ref()?;
    if unsafe { core::ptr::read_volatile(&(*hda.mailbox_virt).is_playing) } == 0 { return None; }
    let elapsed = unsafe { core::ptr::read_volatile(&(*hda.mailbox_virt).elapsed_ms) } as u64;
    let total   = unsafe { core::ptr::read_volatile(&(*hda.mailbox_virt).total_ms) } as u64;
    Some((elapsed, total))
}

/// Used to advance the playback state machine (zero-buffer → drain → stop)
/// when that lived in the kernel — `hdad` does all of that now, entirely on
/// its own, so this is a no-op. Kept (rather than removing and updating
/// every call site) since `main.rs` still calls it once per frame alongside
/// `net::poll()`.
pub fn poll() {}
