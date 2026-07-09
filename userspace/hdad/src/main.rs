#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace HDA audio driver — HepOS's second real "driver moved
// to userspace" migration (PLAN.md's "move drivers to userspace libOS"
// item), following the exact pattern `userspace/rtl8139d` established: the
// kernel (kernel/src/hda.rs) does the one-time PCI enable/reset/codec-
// detection/DMA-buffer-allocation dance (needs `pmm`/PCI access no ring-3
// process has) and then launches this process, handing it the physical
// address of a shared `Mailbox` page as its one launch argument. From then
// on this process runs forever, sending codec verbs via the Immediate
// Command interface and driving the single output stream directly — the
// kernel's `hda::beep()`/`play_pcm()`/`is_playing()`/`progress_ms()` just
// write/read the mailbox, never touching HDA MMIO again.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/hda.rs`'s `Mailbox` struct) — no shared crate between the two
// to enforce that (userspace can't depend on kernel code at all).

use hepos_std::println;

// ── HDA register offsets (byte offsets from the MMIO base) ─────────────────
const IC:  usize = 0x60; // u32 – Immediate Command
const IR:  usize = 0x64; // u32 – Immediate Response
const IRS: usize = 0x68; // u16 – Immediate Response Status

const SD_CTL:  usize = 0x00;
const SD_CBL:  usize = 0x08;
const SD_LVI:  usize = 0x0C;
const SD_FMT:  usize = 0x10;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN:  u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;

const MMIO_LEN: u64 = 16384;

#[repr(C)]
struct BdlEntry { addr: u64, len: u32, ioc: u32 }

#[repr(C)]
struct Mailbox {
    mmio_phys:  u64,
    buf_phys:   u64,
    bdl_phys:   u64,
    sd_off:     u32,
    tsc_per_ms: u32,
    play_request: u32,
    volume:      u32,
    is_playing:  u32,
    elapsed_ms:  u32,
    total_ms:    u32,
}

const PCM_MAX_BYTES: u64 = 1 << 20;

#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

fn spin(n: u32) { for _ in 0..n { core::hint::spin_loop(); } }

#[inline(always)]
fn r16(base: *mut u8, off: usize) -> u16 { unsafe { (base.add(off) as *const u16).read_volatile() } }
#[inline(always)]
fn r32(base: *mut u8, off: usize) -> u32 { unsafe { (base.add(off) as *const u32).read_volatile() } }
#[inline(always)]
fn w16(base: *mut u8, off: usize, v: u16) { unsafe { (base.add(off) as *mut u16).write_volatile(v); } }
#[inline(always)]
fn w32(base: *mut u8, off: usize, v: u32) { unsafe { (base.add(off) as *mut u32).write_volatile(v); } }

/// Sends one codec verb and returns the response word — same Immediate
/// Command protocol the old in-kernel driver used.
fn send_verb(mmio: *mut u8, cad: u8, nid: u8, verb: u32) -> u32 {
    let cmd = ((cad as u32) << 28) | ((nid as u32) << 20) | (verb & 0x000F_FFFF);
    for _ in 0..1_000u32 {
        if r16(mmio, IRS) & 1 == 0 { break; }
        spin(10);
    }
    w32(mmio, IC, cmd);
    w16(mmio, IRS, 0x01);
    for _ in 0..1_000u32 {
        let irs = r16(mmio, IRS);
        if irs & 2 != 0 {
            w16(mmio, IRS, 0x02);
            return r32(mmio, IR);
        }
        spin(10);
    }
    0
}
fn verb4(mmio: *mut u8, nid: u8, opcode: u32, data: u32) -> u32 {
    send_verb(mmio, 0, nid, (opcode << 16) | (data & 0xFFFF))
}
fn verb12(mmio: *mut u8, nid: u8, opcode: u32, data: u32) -> u32 {
    send_verb(mmio, 0, nid, (opcode << 8) | (data & 0xFF))
}
fn send_amp_gain_mute_verb(mmio: *mut u8, vol: u8) {
    let gain = (vol as u32 * 127) / 100;
    verb4(mmio, 2, 0x3, 0xD000 | gain);
}
/// QEMU hda-duplex codec topology (hard-coded, same as the old in-kernel
/// driver): node 1 = AFG, node 2 = Output DAC, node 4 = Output Pin.
fn configure_codec(mmio: *mut u8, stream_id: u8, fmt: u16, vol: u8) {
    verb12(mmio, 1, 0x705, 0x00); // AFG: Set Power State D0
    verb12(mmio, 2, 0x705, 0x00); // DAC: Set Power State D0
    spin(5_000);
    verb4(mmio, 2, 0x2, fmt as u32);
    verb12(mmio, 2, 0x706, (stream_id as u32) << 4);
    send_amp_gain_mute_verb(mmio, vol);
    verb12(mmio, 4, 0x707, 0x40); // Pin Widget Control: HP-Out enable
}

/// Wraparound-safe "has `now` passed `mark`" check, same idiom the old
/// in-kernel driver used for its TSC deadlines.
fn tsc_past(now: u64, mark: u64) -> bool {
    now.wrapping_sub(mark) < u64::MAX / 2
}

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("hdad: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("hdad: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = &mut *(mb_va as *mut Mailbox);

    let mmio_va = hepos_rt::sys_mmap_mmio(mb.mmio_phys, MMIO_LEN) as *mut u8;
    let buf_va  = hepos_rt::sys_mmap_mmio(mb.buf_phys, PCM_MAX_BYTES) as *mut i16;
    let bdl_va  = hepos_rt::sys_mmap_mmio(mb.bdl_phys, 4096) as *mut BdlEntry;
    if mmio_va.is_null() || buf_va.is_null() || bdl_va.is_null() {
        println!("hdad: failed to map MMIO/PCM/BDL — exiting");
        hepos_rt::sys_exit(1);
    }

    let sd_off     = mb.sd_off as usize;
    let tsc_per_ms = (mb.tsc_per_ms as u64).max(1);
    let buf_phys   = mb.buf_phys;

    println!("hdad: ready (mmio {:#x}, sd_off {:#x})", mb.mmio_phys, sd_off);

    let mut last_volume: u32 = core::ptr::read_volatile(&mb.volume);
    send_amp_gain_mute_verb(mmio_va, last_volume as u8);

    // Local playback-position state — not shared with the kernel except
    // through `mb.is_playing`/`elapsed_ms`/`total_ms`.
    let mut ctl_base: u32 = 0;
    let mut buf_bytes: usize = 0;
    let mut start_at: u64 = 0;
    let mut stop_at: u64 = 0;
    let mut drain_at: Option<u64> = None;

    loop {
        // ── Volume: re-apply whenever the kernel changes it ────────────────
        let vol = core::ptr::read_volatile(&mb.volume);
        if vol != last_volume {
            send_amp_gain_mute_verb(mmio_va, vol as u8);
            last_volume = vol;
        }

        // ── New play request ────────────────────────────────────────────────
        let req = core::ptr::read_volatile(&mb.play_request);
        if req != 0 {
            if core::ptr::read_volatile(&mb.is_playing) != 0 {
                // Stop whatever's playing cleanly first — same sequence the
                // old in-kernel stop_now() used (zero buffer, let SDL drain
                // ~200ms, then reset the stream).
                core::ptr::write_bytes(buf_va as *mut u8, 0, buf_bytes);
                let until = rdtsc().wrapping_add(tsc_per_ms.saturating_mul(200));
                while !tsc_past(rdtsc(), until) { core::hint::spin_loop(); }
                w32(mmio_va, sd_off + SD_CTL, ctl_base);               spin(50_000);
                w32(mmio_va, sd_off + SD_CTL, ctl_base | SD_CTL_SRST); spin(50_000);
                w32(mmio_va, sd_off + SD_CTL, ctl_base);               spin(50_000);
            }

            let n = req as usize; // i16 words (stereo-interleaved)
            buf_bytes = n * 2;
            bdl_va.write(BdlEntry { addr: buf_phys, len: buf_bytes as u32, ioc: 1 });

            let stream_id: u8 = 1;
            let fmt: u16 = 0x0011; // 48 kHz, 16-bit, stereo
            configure_codec(mmio_va, stream_id, fmt, vol as u8);

            w32(mmio_va, sd_off + SD_CTL, SD_CTL_SRST); spin(5_000);
            w32(mmio_va, sd_off + SD_CTL, 0);           spin(5_000);
            w32(mmio_va, sd_off + SD_BDPL, mb.bdl_phys as u32);
            w32(mmio_va, sd_off + SD_BDPU, (mb.bdl_phys >> 32) as u32);
            w32(mmio_va, sd_off + SD_CBL, buf_bytes as u32);
            w16(mmio_va, sd_off + SD_LVI, 0);
            w16(mmio_va, sd_off + SD_FMT, fmt);

            ctl_base = ((stream_id as u32) << 20) | SD_CTL_IOCE;
            w32(mmio_va, sd_off + SD_CTL, ctl_base | SD_CTL_RUN);

            let stereo_pairs = (n / 2) as u64;
            let duration_ms = (stereo_pairs * 1000 / 48_000).max(1);
            start_at = rdtsc();
            stop_at  = start_at.wrapping_add(tsc_per_ms.saturating_mul(duration_ms));
            drain_at = None;

            core::ptr::write_volatile(&mut mb.total_ms, duration_ms as u32);
            core::ptr::write_volatile(&mut mb.elapsed_ms, 0);
            core::ptr::write_volatile(&mut mb.is_playing, 1);
            core::ptr::write_volatile(&mut mb.play_request, 0);
        }

        // ── Advance the position/stop/drain state machine ──────────────────
        if core::ptr::read_volatile(&mb.is_playing) != 0 {
            let now = rdtsc();
            match drain_at {
                None => {
                    if !tsc_past(now, stop_at) {
                        let elapsed = now.wrapping_sub(start_at) / tsc_per_ms;
                        core::ptr::write_volatile(&mut mb.elapsed_ms, elapsed as u32);
                    } else {
                        // Clip finished — zero the buffer in-place while the
                        // stream keeps running (QEMU's next ~21ms read
                        // delivers silence to SDL), then start the drain timer.
                        core::ptr::write_bytes(buf_va as *mut u8, 0, buf_bytes);
                        drain_at = Some(now.wrapping_add(tsc_per_ms.saturating_mul(200)));
                    }
                }
                Some(d) => {
                    if tsc_past(now, d) {
                        w32(mmio_va, sd_off + SD_CTL, ctl_base);               spin(50_000);
                        w32(mmio_va, sd_off + SD_CTL, ctl_base | SD_CTL_SRST); spin(50_000);
                        w32(mmio_va, sd_off + SD_CTL, ctl_base);               spin(50_000);
                        core::ptr::write_volatile(&mut mb.is_playing, 0);
                        drain_at = None;
                    }
                }
            }
        }

        // Rate-limit to once per timer tick instead of spinning at 100% CPU
        // — see `userspace/rtl8139d`'s identical fix (and its doc comment on
        // why: a never-yielding driver task starves QEMU's own host-side
        // threads under this project's single-vCPU TCG emulation).
        hepos_rt::sys_wait_irq(0x20);
    }
}
