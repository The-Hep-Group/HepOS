use crate::bootinfo::BootInfo;
use crate::{pmm, process, syscall, vmm};

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn from_hex(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xFF) as u8,
            g: ((hex >> 8)  & 0xFF) as u8,
            b: (hex & 0xFF) as u8,
        }
    }
}

pub struct Display {
    addr:    *mut u8,
    width:   usize,
    height:  usize,
    pitch:   usize,
    bpp:     usize,
    r_shift: u8,
    g_shift: u8,
    b_shift: u8,
    // Double-buffer setup:
    //   backbuf  — what's currently being rendered/displayed (scene + cursor)
    //   scene_buf — scene without cursor; saved after each full render so the
    //               cursor can be erased cheaply without a full redraw
    backbuf:      *mut u32,
    scene_buf:    *mut u32,
    backbuf_len:  usize,     // width * height
    backbuf_phys: u64,       // physical address of `backbuf` — 0 if not allocated.
                              // Lets virtio_gpu.rs attach it directly as a resource's
                              // backing memory (zero-copy mirroring of the real
                              // desktop, not just a synthetic test pattern).
    /// Physical address of the real GOP framebuffer (`addr` is the HHDM
    /// *virtual* address BootInfo hands the kernel; this is that minus
    /// `vmm::hhdm_offset()`) — needed to grant `gopd` an MMIO mapping onto
    /// it, since `SYS_MMAP_MMIO` takes a physical address. Computed once in
    /// `new()`.
    fb_phys: u64,
    /// Null until `spawn_gopd()` successfully launches the `gopd` process —
    /// see that method's doc comment for the GOP-flush userspace migration
    /// this backs. `flush()`/`flush_rows()` fall back to the original direct
    /// MMIO copy whenever this is null, same "direct path until handoff"
    /// pattern `nvme.rs` uses for early-boot I/O before `nvmed` exists.
    mailbox_virt: *mut GopMailbox,
    mailbox_phys: u64,
    /// Dedicated snapshot buffer `gopd` exclusively reads — see
    /// `GopMailbox`'s doc comment for the tearing bug this fixes. Same
    /// layout as `backbuf` (tightly packed `width * height` `u32`s, no
    /// pitch padding). Allocated once in `spawn_gopd()`, null before that
    /// (and whenever `gopd` isn't running).
    publish_buf:  *mut u32,
    publish_phys: u64,
    // Staged locally and committed with the next accepted present request.
    // This prevents a later mouse move from changing the cursor attached to
    // a snapshot gopd is already presenting.
    cursor_x: i32,
    cursor_y: i32,
    cursor_kind: u32,
}

/// Shared memory mailbox — one physical page, mapped into both the kernel
/// (via `vmm::phys_to_virt`) and the `gopd` userspace process (via
/// `SYS_MMAP_MMIO`, using the physical address handed to it as its one
/// launch argument). **Layout must stay byte-for-byte identical to the copy
/// in `userspace/gopd/src/main.rs`** — no shared crate between them enforces
/// this (userspace crates can't depend on kernel code — different target, no
/// `std`, different address space).
///
/// **Not fire-and-forget like RTL8139/HDA's mailboxes — a real bug found the
/// hard way.** The first version pointed `gopd` straight at the live
/// `Display::backbuf` and never waited for anything, on the theory that
/// nothing needs the flush's *result* back. That's true, but irrelevant: the
/// actual problem is that `backbuf` is a **shared mutable buffer** —
/// `task_blink` keeps rendering the *next* frame into it the instant
/// `flush()`/`flush_rows()` returns, since neither ever blocked. `gopd` is a
/// genuinely separate, concurrently-scheduled task, so it can be mid-copy of
/// one frame's rows while `task_blink` (preempted back in) is already
/// overwriting `backbuf` with the *next* frame's content — real tearing,
/// visible as flicker especially during window drags, which redraw the full
/// screen every frame. The old, pre-migration `flush()` never had this
/// problem: it was the copy, running synchronously and atomically inside
/// `task_blink` itself, so nothing else could ever be mutating `backbuf`
/// while it ran.
///
/// Fixed with a request/ack handshake and a dedicated `publish_buf` (see
/// `Display`) that only `gopd` ever reads: `request_flush()` only copies
/// `backbuf`'s dirty rows into `publish_buf` (safe — `gopd` guarantees it
/// isn't touching `publish_buf` right now, see below) if `ack == req` (i.e.
/// `gopd` has fully finished the *previous* request); if `gopd` hasn't
/// caught up yet, the new flush is simply dropped rather than racing its
/// read of the buffer it might still be mid-copy from — an ordinary,
/// harmless frame-drop under backpressure, not corruption. `gopd` writes
/// `ack = req` only *after* finishing its copy out to the real framebuffer,
/// which is what makes it safe for the kernel to reuse `publish_buf` for the
/// next snapshot the moment it sees `ack == req` again. No lock needed: on
/// this single-core cooperative-preemptive scheduler, whichever side runs
/// between the other's check-then-act pair simply sees state that hasn't
/// changed yet, never state that's changing mid-read.
#[repr(C)]
struct GopMailbox {
    /// Physical address of the real GOP framebuffer — written once by the
    /// kernel; `gopd` never needs to know it again after its first map.
    fb_phys: u64,
    /// Physical address of `Display::publish_buf` — a dedicated snapshot
    /// buffer `gopd` exclusively reads, **not** the live `backbuf` (see this
    /// struct's own doc comment for why that distinction is the whole fix).
    publish_phys: u64,
    /// Bytes per scanline of the *real* framebuffer (may differ from
    /// `width * 4` due to hardware padding) — `publish_buf` itself has no
    /// such padding (tightly packed `width * height` `u32`s, mirroring
    /// `backbuf`'s own layout), so `gopd` copies `width` pixels per row but
    /// advances by `fb_pitch / 4` in the destination and by `width` in the
    /// source.
    fb_pitch: u32,
    width:  u32,
    height: u32,
    /// First dirty row (inclusive) and how many rows starting there need
    /// copying — set by the kernel before bumping `req`.
    dirty_y:     u32,
    dirty_count: u32,
    /// Bumped by the kernel every time a new flush is actually published
    /// (i.e. `ack == req` held at request time — see this struct's doc
    /// comment). `gopd` remembers the last value it serviced and only
    /// copies again once this changes.
    req: u32,
    /// Set by `gopd` to the `req` value it just finished copying out to the
    /// real framebuffer — the other half of the handshake. The kernel only
    /// writes new data into `publish_buf` while `ack == req` holds.
    ack: u32,
    cursor_x: i32,
    cursor_y: i32,
    cursor_kind: u32,
    /// 0 = keep running, 1 = kernel wants this process to exit (`service
    /// stop gopd` / `kill`) — same cooperative-shutdown convention every
    /// other migrated driver uses (see e.g. `rtl8139.rs`'s `stop_service()`
    /// doc comment for why it's cooperative, not a forced kill).
    stop: u32,
}

unsafe impl Send for Display {}

impl Display {
    pub fn new(bi: &BootInfo) -> Self {
        Self {
            addr:    bi.fb_addr as *mut u8,
            width:   bi.fb_width as usize,
            height:  bi.fb_height as usize,
            pitch:   bi.fb_pitch as usize,
            bpp:     bi.fb_bpp as usize / 8,
            r_shift: bi.fb_red_shift,
            g_shift: bi.fb_green_shift,
            b_shift: bi.fb_blue_shift,
            backbuf:      core::ptr::null_mut(),
            scene_buf:    core::ptr::null_mut(),
            backbuf_len:  0,
            backbuf_phys: 0,
            fb_phys:      (bi.fb_addr).wrapping_sub(vmm::hhdm_offset()),
            mailbox_virt: core::ptr::null_mut(),
            mailbox_phys: 0,
            publish_buf:  core::ptr::null_mut(),
            publish_phys: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_kind: 0,
        }
    }

    /// Allocate backbuf + scene_buf from PMM.  Falls back to direct FB writes
    /// if allocation fails (no tearing elimination, but still works).
    pub fn init_backbuf(&mut self) {
        if self.bpp != 4 { return; }
        let pixels = self.width * self.height;
        let bytes  = pixels * 4;
        let pages  = (bytes + 4095) / 4096;
        if let Some(p1) = crate::pmm::alloc_contiguous(pages) {
            if let Some(p2) = crate::pmm::alloc_contiguous(pages) {
                let v1 = crate::vmm::phys_to_virt(p1);
                let v2 = crate::vmm::phys_to_virt(p2);
                unsafe {
                    core::ptr::write_bytes(v1, 0, bytes);
                    core::ptr::write_bytes(v2, 0, bytes);
                }
                self.backbuf      = v1 as *mut u32;
                self.scene_buf    = v2 as *mut u32;
                self.backbuf_len  = pixels;
                self.backbuf_phys = p1;
            }
        }
    }

    /// (physical address, width, height, whether the pixel format is
    /// B8G8R8X8 — the only layout `virtio_gpu::mirror_display()` understands)
    /// of the backbuffer, for `virtio_gpu.rs` to attach directly as a
    /// resource's backing memory. `None` if the backbuffer wasn't allocated.
    pub fn backbuf_info(&self) -> Option<(u64, usize, usize, bool)> {
        if self.backbuf_phys == 0 { return None; }
        let is_bgrx8888 = self.r_shift == 16 && self.g_shift == 8 && self.b_shift == 0 && self.bpp == 4;
        Some((self.backbuf_phys, self.width, self.height, is_bgrx8888))
    }

    /// Save the current backbuf (scene without cursor) into scene_buf.
    /// Call this after rendering the scene and before painting the cursor.
    pub fn save_scene(&mut self) {
        if self.backbuf.is_null() || self.scene_buf.is_null() { return; }
        unsafe {
            core::ptr::copy_nonoverlapping(self.backbuf, self.scene_buf, self.backbuf_len);
        }
    }

    /// Restore rows `y .. y+count` of backbuf from scene_buf (erases cursor pixels).
    pub fn restore_rows(&mut self, y: usize, count: usize) {
        if self.backbuf.is_null() || self.scene_buf.is_null() { return; }
        let y0    = y.min(self.height);
        let y1    = (y + count).min(self.height);
        let n     = (y1 - y0) * self.width;
        if n == 0 { return; }
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.scene_buf.add(y0 * self.width),
                self.backbuf.add(y0 * self.width),
                n,
            );
        }
    }

    /// Flush the entire backbuf to the physical framebuffer.
    pub fn flush(&mut self) {
        self.request_flush(0, self.height);
    }

    /// Common body of `flush()`/`flush_rows()`: routes through `gopd`'s
    /// mailbox once it's up (see `spawn_gopd()`), falling back to the
    /// original direct MMIO copy before that — same "direct path until
    /// handoff" shape `nvme.rs`'s `read_blocks()` uses for early-boot I/O.
    ///
    /// See `GopMailbox`'s doc comment for the tearing bug this handshake
    /// fixes: only copies `backbuf`'s dirty rows into `publish_buf` (the
    /// buffer `gopd` actually reads) while `ack == req` — i.e. `gopd` has
    /// fully finished the previous request and is guaranteed not to be
    /// touching `publish_buf` right now. If `gopd` hasn't caught up yet,
    /// this flush is simply dropped (no spin-wait — that would reintroduce
    /// exactly the "task_blink stalls on a background task's scheduling
    /// turn" latency problem this session's earlier scheduler fixes solved)
    /// rather than racing a concurrent read of a buffer that's still in use.
    fn request_flush(&mut self, y: usize, count: usize) {
        if self.backbuf.is_null() || self.bpp != 4 { return; }
        if !self.mailbox_virt.is_null() && !self.publish_buf.is_null() {
            let y0 = y.min(self.height);
            let y1 = (y + count).min(self.height);
            if y0 >= y1 { return; }
            unsafe {
                let mb = self.mailbox_virt;
                let ack = core::ptr::read_volatile(&(*mb).ack);
                let req = core::ptr::read_volatile(&(*mb).req);
                if ack != req { return; } // gopd still catching up — drop this frame
                core::ptr::copy_nonoverlapping(
                    self.backbuf.add(y0 * self.width),
                    self.publish_buf.add(y0 * self.width),
                    (y1 - y0) * self.width,
                );
                core::ptr::write_volatile(&mut (*mb).dirty_y, y0 as u32);
                core::ptr::write_volatile(&mut (*mb).dirty_count, (y1 - y0) as u32);
                core::ptr::write_volatile(&mut (*mb).cursor_x, self.cursor_x);
                core::ptr::write_volatile(&mut (*mb).cursor_y, self.cursor_y);
                core::ptr::write_volatile(&mut (*mb).cursor_kind, self.cursor_kind);
                core::ptr::write_volatile(&mut (*mb).req, req.wrapping_add(1));
            }
            return;
        }
        let pitch_u32 = self.pitch / 4;
        let y0 = y.min(self.height);
        let y1 = (y + count).min(self.height);
        unsafe {
            for row in y0..y1 {
                core::ptr::copy_nonoverlapping(
                    self.backbuf.add(row * self.width),
                    (self.addr as *mut u32).add(row * pitch_u32),
                    self.width,
                );
            }
        }
    }

    /// Flush only rows `y .. y+count` of backbuf to the physical framebuffer.
    /// Used for cursor-only updates — far cheaper than a full flush.
    pub fn flush_rows(&mut self, y: usize, count: usize) {
        self.request_flush(y, count);
    }

    /// True when final composition is delegated to gopd.
    pub fn gopd_active(&self) -> bool { !self.mailbox_virt.is_null() }

    /// Wire enum: 0 normal, 1 EW, 2 NS, 3 NWSE, 4 NESW.
    pub fn set_gop_cursor(&mut self, x: i32, y: i32, kind: u32) {
        self.cursor_x = x;
        self.cursor_y = y;
        self.cursor_kind = kind;
    }

    /// One-time bring-up of `gopd`, the GOP-flush userspace driver — moves
    /// the actual backbuffer→real-framebuffer copy (`flush()`/`flush_rows()`)
    /// off the kernel, the closest thing GOP has to a "hot path" analogous to
    /// a NIC's RX ring or a disk's I/O queue (see PLAN.md's GOP Phase 2 write
    /// up for why the rest of the compositor — everything that draws *into*
    /// the backbuffer — isn't part of this migration). A no-op after the
    /// first successful call, or if the backbuffer isn't allocated yet
    /// (`init_backbuf()` must run first — called once at boot before
    /// `task_blink` starts, so by the time this runs from that loop it
    /// always has). Must be called from `task_blink`'s own loop, same
    /// "scheduler must already be live" constraint every other driver's
    /// `spawn_pending_driver()` has — see e.g. `rtl8139.rs`'s doc comment on
    /// why spawning any earlier corrupts the task-0 bootstrap.
    /// Idempotent across restarts, not just the first call: `mailbox_phys`/
    /// `publish_phys` are only ever allocated once (guarded by
    /// `self.mailbox_phys == 0`, which survives a `stop_service()` — only
    /// `mailbox_virt` gets nulled by that) and reused on every subsequent
    /// `start_service()` — same "one-time DMA-buffer allocation in `init()`,
    /// reused across restarts" pattern every other migrated driver uses
    /// (e.g. `rtl8139.rs`'s `tx_phys`/`rx_phys`). Allocating fresh buffers on
    /// every restart instead would leak the old ones every single
    /// `service stop`/`start gopd` cycle.
    pub fn spawn_gopd(&mut self) {
        if !self.mailbox_virt.is_null() || self.backbuf.is_null() { return; }

        let bytes = self.backbuf_len * 4;

        if self.mailbox_phys == 0 {
            // Dedicated snapshot buffer gopd exclusively reads — see
            // `GopMailbox`'s doc comment for why this can't just be
            // `backbuf` itself. Same size/layout as `backbuf`.
            let pages = (bytes + 4095) / 4096;
            let Some(publish_phys) = pmm::alloc_contiguous(pages) else {
                crate::serial::print("gopd: publish buffer OOM\n");
                return;
            };
            self.publish_phys = publish_phys;
            self.publish_buf  = vmm::phys_to_virt(publish_phys) as *mut u32;
            unsafe { core::ptr::write_bytes(self.publish_buf as *mut u8, 0, bytes); }

            let Some(mailbox_phys) = pmm::alloc_page() else {
                crate::serial::print("gopd: mailbox OOM\n");
                return;
            };
            self.mailbox_phys = mailbox_phys;

            syscall::grant_mmio_range(self.fb_phys, (self.height * self.pitch) as u64);
            syscall::grant_mmio_range(publish_phys, bytes as u64);
            syscall::grant_mmio_range(mailbox_phys, 4096);
        }

        let mailbox_virt = vmm::phys_to_virt(self.mailbox_phys) as *mut GopMailbox;
        unsafe {
            // Re-zero (not just on first alloc) so a restart starts the
            // ack/req handshake fresh at (0, 0) rather than resuming
            // whatever counters the previous `gopd` instance left behind —
            // `gopd` itself starts `last_req` at 0 too (see its own
            // `_start()`), so both sides agree on a clean initial state.
            core::ptr::write_bytes(mailbox_virt as *mut u8, 0, 4096);
            (*mailbox_virt).fb_phys      = self.fb_phys;
            (*mailbox_virt).publish_phys = self.publish_phys;
            (*mailbox_virt).fb_pitch     = self.pitch as u32;
            (*mailbox_virt).width        = self.width as u32;
            (*mailbox_virt).height       = self.height as u32;
        }

        match process::exec_async_with_arg(usize::MAX, SERVICE_NAME, GOPD_ELF, self.mailbox_phys) {
            Ok(()) => {
                self.mailbox_virt = mailbox_virt;
                crate::serial::print("gopd: launched\n");
            }
            Err(_) => crate::serial::print("gopd: launch failed\n"),
        }
    }

    pub fn width(&self)  -> usize { self.width  }
    pub fn height(&self) -> usize { self.height }

    #[inline]
    fn pack_color(&self, c: Color) -> u32 {
        ((c.r as u32) << self.r_shift)
        | ((c.g as u32) << self.g_shift)
        | ((c.b as u32) << self.b_shift)
    }

    #[inline]
    pub fn put_pixel_pub(&mut self, x: usize, y: usize, c: Color) {
        self.put_pixel(x, y, c);
    }

    #[inline]
    fn put_pixel(&mut self, x: usize, y: usize, c: Color) {
        if x >= self.width || y >= self.height { return; }
        let pixel = self.pack_color(c);
        if !self.backbuf.is_null() {
            unsafe { *self.backbuf.add(y * self.width + x) = pixel; }
        } else {
            let offset = y * self.pitch + x * self.bpp;
            unsafe { *(self.addr.add(offset) as *mut u32) = pixel; }
        }
    }

    pub fn clear(&mut self, c: Color) {
        let pixel = self.pack_color(c);
        if !self.backbuf.is_null() {
            // Fast path: single fill over the whole backbuffer
            unsafe {
                core::slice::from_raw_parts_mut(self.backbuf, self.backbuf_len)
                    .fill(pixel);
            }
        } else {
            for y in 0..self.height {
                for x in 0..self.width { self.put_pixel(x, y, c); }
            }
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        let pixel  = self.pack_color(c);
        let x_end  = (x + w).min(self.width);
        let y_end  = (y + h).min(self.height);
        if x >= x_end { return; }
        if !self.backbuf.is_null() {
            // Fast path: fill each row with a single slice::fill call
            let row_len = x_end - x;
            for row in y..y_end {
                unsafe {
                    let ptr = self.backbuf.add(row * self.width + x);
                    core::slice::from_raw_parts_mut(ptr, row_len).fill(pixel);
                }
            }
        } else {
            for row in y..y_end {
                for col in x..x_end { self.put_pixel(col, row, c); }
            }
        }
    }

    pub fn draw_text(&mut self, x: usize, y: usize, text: &str, c: Color, scale: usize) {
        let mut cx = x;
        for ch in text.chars() {
            let glyph = FONT.get(ch as usize).unwrap_or(&FONT[b'?' as usize]);
            for (row, &bits) in glyph.iter().enumerate() {
                for col in 0..8usize {
                    if bits & (1 << col) != 0 {
                        self.fill_rect(cx + col * scale, y + row * scale, scale, scale, c);
                    }
                }
            }
            cx += 9 * scale;
        }
    }
}

// ── gopd service management (`service`/`kill` terminal commands) ─────────────
//
// Same conventions every other migrated driver's service layer uses (see
// e.g. `rtl8139.rs`), just reading/writing `Display::mailbox_virt` via
// `crate::DISPLAY` (`main.rs`) instead of a dedicated per-driver static,
// since the mailbox pointer lives on `Display` itself here rather than a
// separate `NIC`/`CONTROLLER`-style struct.

static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub const SERVICE_NAME: &str = "<gopd>";

pub fn is_enabled() -> bool { ENABLED.load(core::sync::atomic::Ordering::Relaxed) }
pub fn set_enabled(v: bool) { ENABLED.store(v, core::sync::atomic::Ordering::Relaxed); }
pub fn is_running() -> bool { process::is_process_running(SERVICE_NAME) }

/// Number of spin iterations to wait for `gopd` to notice `stop` and exit,
/// or for a fresh launch to register as running — same generous budget
/// every other driver's identical constant uses (the driver only checks
/// once per ~10ms timer tick).
const STOP_WAIT_SPINS: u32 = 500_000_000;

static STARTING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Request a cooperative shutdown of the running `gopd` and wait for it to
/// actually exit. Once stopped, `flush()`/`flush_rows()` fall straight back
/// to the direct MMIO copy (see `request_flush()`) — the desktop keeps
/// rendering the entire time the service is down, only the userspace
/// process itself goes away.
pub fn stop_service() -> Result<(), &'static str> {
    if !is_running() { return Err("not running"); }
    let mb = { crate::DISPLAY.lock().as_ref().map(|d| d.mailbox_virt) };
    let Some(mb) = mb else { return Err("driver not initialized") };
    if mb.is_null() { return Err("driver not initialized"); }
    unsafe { core::ptr::write_volatile(&mut (*mb).stop, 1); }
    for _ in 0..STOP_WAIT_SPINS {
        if !is_running() {
            if let Some(d) = crate::DISPLAY.lock().as_mut() {
                d.mailbox_virt = core::ptr::null_mut();
            }
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("timeout waiting for driver to stop")
}

/// Launch a fresh `gopd` instance. Unlike the other 4 drivers, there's no
/// separate one-time hardware bring-up to reuse here — `spawn_gopd()` *is*
/// both the bring-up and the launch, guarded by `mailbox_virt` already being
/// null (cleared by `stop_service()` above) — so this just re-runs it.
pub fn start_service() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;
    if !is_enabled() { return Err("disabled"); }
    if is_running() { return Err("already running"); }
    if STARTING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("start already in progress");
    }
    let result = (|| {
        if GOPD_ELF.is_empty() { return Err("gopd ELF not built"); }
        {
            let mut guard = crate::DISPLAY.lock();
            let Some(d) = guard.as_mut() else { return Err("display not initialized"); };
            if !d.mailbox_virt.is_null() { return Err("already running"); }
            d.spawn_gopd();
            if d.mailbox_virt.is_null() { return Err("launch failed"); }
        }
        for _ in 0..STOP_WAIT_SPINS {
            if is_running() { return Ok(()); }
            core::hint::spin_loop();
        }
        Err("timeout waiting for driver to start")
    })();
    STARTING.store(false, Ordering::Release);
    result
}

// Baked-in gopd ELF (generated by build.rs from userspace/target/.../gopd).
// Empty slice if userspace hasn't been rebuilt since this driver was added.
include!(concat!(env!("OUT_DIR"), "/gopd_elf.rs"));

// 8x8 bitmap font — printable ASCII starting at 0x20
static FONT: [[u8; 8]; 128] = {
    let mut f = [[0u8; 8]; 128];

    // space
    f[0x20] = [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00];
    // !
    f[0x21] = [0x18,0x18,0x18,0x18,0x18,0x00,0x18,0x00];
    // "
    f[0x22] = [0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00];
    // #
    f[0x23] = [0x36,0x36,0x7F,0x36,0x7F,0x36,0x36,0x00];
    // $
    f[0x24] = [0x0C,0x3E,0x03,0x1E,0x30,0x1F,0x0C,0x00];
    // %
    f[0x25] = [0x00,0x63,0x33,0x18,0x0C,0x66,0x63,0x00];
    // &
    f[0x26] = [0x1C,0x36,0x1C,0x6E,0x3B,0x33,0x6E,0x00];
    // '
    f[0x27] = [0x06,0x06,0x03,0x00,0x00,0x00,0x00,0x00];
    // (
    f[0x28] = [0x18,0x0C,0x06,0x06,0x06,0x0C,0x18,0x00];
    // )
    f[0x29] = [0x06,0x0C,0x18,0x18,0x18,0x0C,0x06,0x00];
    // *
    f[0x2A] = [0x00,0x66,0x3C,0xFF,0x3C,0x66,0x00,0x00];
    // +
    f[0x2B] = [0x00,0x0C,0x0C,0x3F,0x0C,0x0C,0x00,0x00];
    // ,
    f[0x2C] = [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x06];
    // -
    f[0x2D] = [0x00,0x00,0x00,0x3F,0x00,0x00,0x00,0x00];
    // .
    f[0x2E] = [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x00];
    // /
    f[0x2F] = [0x60,0x30,0x18,0x0C,0x06,0x03,0x01,0x00];

    // 0–9
    f[0x30] = [0x3E,0x63,0x73,0x7B,0x6F,0x67,0x3E,0x00];
    f[0x31] = [0x0C,0x0E,0x0C,0x0C,0x0C,0x0C,0x3F,0x00];
    f[0x32] = [0x1E,0x33,0x30,0x1C,0x06,0x33,0x3F,0x00];
    f[0x33] = [0x1E,0x33,0x30,0x1C,0x30,0x33,0x1E,0x00];
    f[0x34] = [0x38,0x3C,0x36,0x33,0x7F,0x30,0x78,0x00];
    f[0x35] = [0x3F,0x03,0x1F,0x30,0x30,0x33,0x1E,0x00];
    f[0x36] = [0x1C,0x06,0x03,0x1F,0x33,0x33,0x1E,0x00];
    f[0x37] = [0x3F,0x33,0x30,0x18,0x0C,0x0C,0x0C,0x00];
    f[0x38] = [0x1E,0x33,0x33,0x1E,0x33,0x33,0x1E,0x00];
    f[0x39] = [0x1E,0x33,0x33,0x3E,0x30,0x18,0x0E,0x00];

    // :
    f[0x3A] = [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x00];
    // ;
    f[0x3B] = [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x06];
    // <
    f[0x3C] = [0x18,0x0C,0x06,0x03,0x06,0x0C,0x18,0x00];
    // =
    f[0x3D] = [0x00,0x00,0x3F,0x00,0x00,0x3F,0x00,0x00];
    // >
    f[0x3E] = [0x06,0x0C,0x18,0x30,0x18,0x0C,0x06,0x00];
    // ?
    f[0x3F] = [0x1E,0x33,0x30,0x18,0x0C,0x00,0x0C,0x00];
    // @
    f[0x40] = [0x3E,0x63,0x7B,0x7B,0x7B,0x03,0x1E,0x00];

    // A–Z
    f[0x41] = [0x0C,0x1E,0x33,0x33,0x3F,0x33,0x33,0x00];
    f[0x42] = [0x3F,0x66,0x66,0x3E,0x66,0x66,0x3F,0x00];
    f[0x43] = [0x3C,0x66,0x03,0x03,0x03,0x66,0x3C,0x00];
    f[0x44] = [0x1F,0x36,0x66,0x66,0x66,0x36,0x1F,0x00];
    f[0x45] = [0x7F,0x46,0x16,0x1E,0x16,0x46,0x7F,0x00];
    f[0x46] = [0x7F,0x46,0x16,0x1E,0x16,0x06,0x0F,0x00];
    f[0x47] = [0x3C,0x66,0x03,0x03,0x73,0x66,0x7C,0x00];
    f[0x48] = [0x33,0x33,0x33,0x3F,0x33,0x33,0x33,0x00];
    f[0x49] = [0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00];
    f[0x4A] = [0x78,0x30,0x30,0x30,0x33,0x33,0x1E,0x00];
    f[0x4B] = [0x67,0x66,0x36,0x1E,0x36,0x66,0x67,0x00];
    f[0x4C] = [0x0F,0x06,0x06,0x06,0x46,0x66,0x7F,0x00];
    f[0x4D] = [0x63,0x77,0x7F,0x7F,0x6B,0x63,0x63,0x00];
    f[0x4E] = [0x63,0x67,0x6F,0x7B,0x73,0x63,0x63,0x00];
    f[0x4F] = [0x1C,0x36,0x63,0x63,0x63,0x36,0x1C,0x00];
    f[0x50] = [0x3F,0x66,0x66,0x3E,0x06,0x06,0x0F,0x00];
    f[0x51] = [0x1E,0x33,0x33,0x33,0x3B,0x1E,0x38,0x00];
    f[0x52] = [0x3F,0x66,0x66,0x3E,0x36,0x66,0x67,0x00];
    f[0x53] = [0x1E,0x33,0x07,0x0E,0x38,0x33,0x1E,0x00];
    f[0x54] = [0x3F,0x2D,0x0C,0x0C,0x0C,0x0C,0x1E,0x00];
    f[0x55] = [0x33,0x33,0x33,0x33,0x33,0x33,0x3F,0x00];
    f[0x56] = [0x33,0x33,0x33,0x33,0x33,0x1E,0x0C,0x00];
    f[0x57] = [0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x00];
    f[0x58] = [0x63,0x63,0x36,0x1C,0x1C,0x36,0x63,0x00];
    f[0x59] = [0x33,0x33,0x33,0x1E,0x0C,0x0C,0x1E,0x00];
    f[0x5A] = [0x7F,0x63,0x31,0x18,0x4C,0x66,0x7F,0x00];

    // [
    f[0x5B] = [0x1E,0x06,0x06,0x06,0x06,0x06,0x1E,0x00];
    // backslash
    f[0x5C] = [0x03,0x06,0x0C,0x18,0x30,0x60,0x40,0x00];
    // ]
    f[0x5D] = [0x1E,0x18,0x18,0x18,0x18,0x18,0x1E,0x00];
    // ^
    f[0x5E] = [0x08,0x1C,0x36,0x63,0x00,0x00,0x00,0x00];
    // _
    f[0x5F] = [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFF];
    // `
    f[0x60] = [0x0C,0x0C,0x18,0x00,0x00,0x00,0x00,0x00];

    // a–z
    f[0x61] = [0x00,0x00,0x1E,0x30,0x3E,0x33,0x6E,0x00];
    f[0x62] = [0x07,0x06,0x06,0x3E,0x66,0x66,0x3B,0x00];
    f[0x63] = [0x00,0x00,0x1E,0x33,0x03,0x33,0x1E,0x00];
    f[0x64] = [0x38,0x30,0x30,0x3e,0x33,0x33,0x6E,0x00];
    f[0x65] = [0x00,0x00,0x1E,0x33,0x3f,0x03,0x1E,0x00];
    f[0x66] = [0x1C,0x36,0x06,0x0f,0x06,0x06,0x0F,0x00];
    f[0x67] = [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x1F];
    f[0x68] = [0x07,0x06,0x36,0x6E,0x66,0x66,0x67,0x00];
    f[0x69] = [0x0C,0x00,0x0E,0x0C,0x0C,0x0C,0x1E,0x00];
    f[0x6A] = [0x30,0x00,0x30,0x30,0x30,0x33,0x33,0x1E];
    f[0x6B] = [0x07,0x06,0x66,0x36,0x1E,0x36,0x67,0x00];
    f[0x6C] = [0x0E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00];
    f[0x6D] = [0x00,0x00,0x33,0x7F,0x7F,0x6B,0x63,0x00];
    f[0x6E] = [0x00,0x00,0x1F,0x33,0x33,0x33,0x33,0x00];
    f[0x6F] = [0x00,0x00,0x1E,0x33,0x33,0x33,0x1E,0x00];
    f[0x70] = [0x00,0x00,0x3B,0x66,0x66,0x3E,0x06,0x0F];
    f[0x71] = [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x78];
    f[0x72] = [0x00,0x00,0x3B,0x6E,0x66,0x06,0x0F,0x00];
    f[0x73] = [0x00,0x00,0x3E,0x03,0x1E,0x30,0x1F,0x00];
    f[0x74] = [0x08,0x0C,0x3E,0x0C,0x0C,0x2C,0x18,0x00];
    f[0x75] = [0x00,0x00,0x33,0x33,0x33,0x33,0x6E,0x00];
    f[0x76] = [0x00,0x00,0x33,0x33,0x33,0x1E,0x0C,0x00];
    f[0x77] = [0x00,0x00,0x63,0x6B,0x7F,0x7F,0x36,0x00];
    f[0x78] = [0x00,0x00,0x63,0x36,0x1C,0x36,0x63,0x00];
    f[0x79] = [0x00,0x00,0x33,0x33,0x33,0x3E,0x30,0x1F];
    f[0x7A] = [0x00,0x00,0x3F,0x19,0x0C,0x26,0x3F,0x00];

    f
};
