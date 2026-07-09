//! RTL8139 NIC driver for QEMU.
//! PCI vendor 0x10EC, device 0x8139.
//! Uses I/O BAR0. TX: 4 flat buffers. RX: 64KB ring buffer.
//!
//! This is HepOS's first real "driver moved to userspace" migration
//! (PLAN.md's "move drivers to userspace libOS" item): the delicate
//! one-time hardware bring-up (PCI enable, BAR discovery, reset, DMA buffer
//! allocation, initial register programming) stays here in the kernel — it
//! needs `pmm`/PCI access no ring-3 process has — but the *ongoing*
//! per-packet TX/RX polling that used to run in-kernel now runs as a
//! persistent userspace process (`userspace/rtl8139d`), talking to this
//! module through a shared-memory `Mailbox` page. `send()`/`recv()` below
//! just write/read that mailbox; the driver process does the actual
//! `in`/`out` register pokes via `SYS_PORT_IN`/`SYS_PORT_OUT` and reads the
//! hardware's RX ring via `SYS_MMAP_MMIO`, exactly the primitives
//! `userspace/hwtest` already proved out. `net.rs` needed zero changes —
//! `Rtl8139::send()`/`recv()`/`.mac` keep their exact old signatures.

use core::arch::asm;
use crate::{pmm, process, syscall, vmm};

fn inb(p: u16) -> u8  { let v: u8;  unsafe { asm!("in al,dx",  out("al") v,  in("dx") p, options(nomem,nostack)); } v }
fn outb(p: u16, v: u8)  { unsafe { asm!("out dx,al",  in("dx") p, in("al") v,  options(nomem,nostack)); } }
fn outw(p: u16, v: u16) { unsafe { asm!("out dx,ax",  in("dx") p, in("ax") v,  options(nomem,nostack)); } }
fn outl(p: u16, v: u32) { unsafe { asm!("out dx,eax", in("dx") p, in("eax") v, options(nomem,nostack)); } }

// Register offsets (from I/O base)
const MAC0:    u16 = 0x00; // 6 bytes
const TXSTAT0: u16 = 0x10; // TX status 0-3     (4 × 4B)
const RXBUF:   u16 = 0x30; // RX buffer start (4B)
const CMD:     u16 = 0x37; // command register (1B)
const CAPR:    u16 = 0x38; // current address of packet read (2B)
const IMR:     u16 = 0x3C; // interrupt mask register (2B)
const TXCFG:   u16 = 0x40; // TX configuration (4B)
const RXCFG:   u16 = 0x44; // RX configuration (4B)
const CONFIG1: u16 = 0x52; // config register 1 (1B)

// CMD bits
const CMD_TE: u8 = 1 << 2; // TX enable
const CMD_RE: u8 = 1 << 3; // RX enable
const CMD_RST: u8 = 1 << 4;

// TXSTAT bits — only the kernel's own one-time slot-priming needs this now
// (marking every slot "ours" at boot); the per-packet OWN-bit wait moved to
// `userspace/rtl8139d`.
const TXSTAT_OWN: u32 = 1 << 13;

// RXCFG: accept all, 64KB ring, no threshold
const RX_ACCEPT_ALL: u32 = 0xF;   // AB|AM|APM|AAP
const RX_WRAP:        u32 = 1 << 7; // WRAP=1 (safer, simpler)
const RX_RBLEN_64K:   u32 = 0b11 << 11;

const TX_SLOTS: usize = 4;
/// One full 4 KB page per TX slot (only ~1.8 KB ever used) — wasteful but
/// simple, and it means `mailbox.tx_phys + slot * TX_SLOT_STRIDE` is all the
/// address math either side needs, no packed-2KB-slot arithmetic to keep in
/// sync between two separate crates.
const TX_SLOT_STRIDE: u64 = 4096;
const RX_BUF_LEN: usize = 65536 + 16; // 64KB + 16 for overruns

/// Shared memory mailbox — one physical page, mapped into both the kernel
/// (via `vmm::phys_to_virt`, same as any other kernel memory) and the
/// `rtl8139d` userspace process (via `SYS_MMAP_MMIO`, using the physical
/// address handed to it as its one launch argument — see `enter_ring3`'s doc
/// comment in `process.rs`). **Layout must stay byte-for-byte identical to
/// the copy in `userspace/rtl8139d/src/main.rs`** — there's no shared crate
/// between the two to enforce that, since userspace crates can't depend on
/// kernel code (different target, different address space, no `std`).
#[repr(C)]
struct Mailbox {
    io_base: u32,
    tx_phys: u32,
    rx_phys: u32,
    _pad0:   u32,
    /// 0 = empty/idle. Kernel writes a nonzero length + fills `tx_buf` to
    /// request a send; the driver process clears it back to 0 once the
    /// packet has actually been handed to the hardware.
    tx_len:  u32,
    tx_buf:  [u8; 1792],
    /// 0 = empty. Driver process sets this (with `rx_len`/`rx_buf` filled)
    /// when a real packet arrives; kernel clears it back to 0 once it's
    /// copied the packet out.
    rx_ready: u32,
    rx_len:   u32,
    rx_buf:   [u8; 1600],
    /// 0 = keep running. Kernel writes 1 to request a cooperative shutdown
    /// (`service stop rtl8139d` / `kill <pid>`); the driver checks this once
    /// per loop iteration and calls `sys_exit(0)` if set. Cooperative rather
    /// than a true forced kill because there's no safe way to preempt a
    /// process that might currently be holding a kernel lock (e.g. mid
    /// register I/O) from outside its own context in this scheduler — see
    /// `stop_service()`'s doc comment.
    stop: u32,
}

pub struct Rtl8139 {
    pub mac: [u8; 6],
    mailbox_virt: *mut Mailbox,
    mailbox_phys: u64,
}
unsafe impl Send for Rtl8139 {}

impl Rtl8139 {
    /// Queue `data` for the driver process to send. Non-blocking in the
    /// hardware sense (no more waiting on the actual TX-OWN register bit —
    /// that's `rtl8139d`'s job now) — just a short bounded wait for the
    /// *mailbox slot itself* to be free if a previous send hasn't been
    /// picked up yet.
    pub fn send(&mut self, data: &[u8]) {
        if data.len() > 1792 || data.is_empty() { return; }
        let mb = unsafe { &mut *self.mailbox_virt };
        for _ in 0..1_000_000u32 {
            if unsafe { core::ptr::read_volatile(&mb.tx_len) } == 0 { break; }
            core::hint::spin_loop();
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), mb.tx_buf.as_mut_ptr(), data.len());
            core::ptr::write_volatile(&mut mb.tx_len, data.len() as u32);
        }
    }

    /// Pick up one received packet the driver process already copied into
    /// the mailbox, if any.
    pub fn recv(&mut self) -> Option<alloc::vec::Vec<u8>> {
        let mb = unsafe { &mut *self.mailbox_virt };
        if unsafe { core::ptr::read_volatile(&mb.rx_ready) } == 0 { return None; }
        let len = (unsafe { core::ptr::read_volatile(&mb.rx_len) } as usize).min(mb.rx_buf.len());
        let mut packet = alloc::vec::Vec::with_capacity(len);
        packet.extend_from_slice(&mb.rx_buf[..len]);
        unsafe { core::ptr::write_volatile(&mut mb.rx_ready, 0); }
        Some(packet)
    }
}

pub static NIC: spin::Mutex<Option<Rtl8139>> = spin::Mutex::new(None);

/// Mailbox physical address waiting to be handed to a freshly-spawned
/// `rtl8139d` task, set by `init()` and consumed once by
/// `spawn_pending_driver()`. See that fn's doc comment for why the spawn
/// can't happen inside `init()` itself.
static PENDING_DRIVER_MAILBOX: spin::Mutex<Option<u64>> = spin::Mutex::new(None);

/// Launches the queued `rtl8139d` driver process, if `init()` found the
/// hardware and queued one. Must be called only after the scheduler's
/// idle/blink tasks are registered and the timer is running (i.e. from
/// within `task_blink`'s own loop) — see `init()`'s doc comment on why
/// spawning any earlier corrupts the scheduler's task-0 bootstrap. A no-op
/// on every call after the first (or if there was never anything queued).
pub fn spawn_pending_driver() {
    let Some(mailbox_phys) = PENDING_DRIVER_MAILBOX.lock().take() else { return };
    match process::exec_async_with_arg(usize::MAX, "<rtl8139d>", RTL8139D_ELF, mailbox_phys) {
        Ok(()) => crate::serial::print("rtl8139: rtl8139d launched\n"),
        Err(_) => crate::serial::print("rtl8139: rtl8139d launch failed\n"),
    }
}

// ── Service management (`service`/`kill` terminal commands) ──────────────────
//
// Whether a future `start_service()` call is allowed to actually launch the
// driver — `service enable`/`disable` toggle this. In-memory only, doesn't
// persist across a reboot (this session's scope): matches real `systemctl`
// semantics for the flag's *meaning* (governs future starts, doesn't touch
// an already-running instance), just not its durability.
static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub const SERVICE_NAME: &str = "<rtl8139d>";

pub fn is_enabled() -> bool { ENABLED.load(core::sync::atomic::Ordering::Relaxed) }
pub fn set_enabled(v: bool) { ENABLED.store(v, core::sync::atomic::Ordering::Relaxed); }
pub fn is_running() -> bool { process::is_process_running(SERVICE_NAME) }

/// Number of spin iterations to wait for `rtl8139d` to notice `stop` and
/// exit. Generous for the same reason `ahci.rs`'s `MAILBOX_WAIT_SPINS` is —
/// the driver only checks once per ~10ms timer tick.
const STOP_WAIT_SPINS: u32 = 500_000_000;

/// Request a cooperative shutdown of the running `rtl8139d` and wait for it
/// to actually exit. Not a true forced kill: this scheduler has no safe way
/// to preempt an arbitrary task from outside its own context (it might be
/// holding `NIC`'s own lock, mid register I/O, etc. — see
/// `scheduler::block_on_irq()`'s doc comments for the kinds of subtle bugs
/// that already came from *less* invasive cross-task interactions). Instead
/// this sets a flag the driver's own loop checks once per iteration and
/// exits itself via `sys_exit(0)` — safe, since it only ever happens at a
/// point the driver chose, never mid-operation.
pub fn stop_service() -> Result<(), &'static str> {
    if !is_running() { return Err("not running"); }
    let mb = { let g = NIC.lock(); g.as_ref().map(|n| n.mailbox_virt) };
    let Some(mb) = mb else { return Err("driver not initialized") };
    unsafe { core::ptr::write_volatile(&mut (*mb).stop, 1); }
    for _ in 0..STOP_WAIT_SPINS {
        if !is_running() { return Ok(()); }
        core::hint::spin_loop();
    }
    Err("timeout waiting for driver to stop")
}

/// Guards against two concurrent `start_service()` calls both slipping past
/// the `is_running()` check below — necessary because `process::
/// exec_async_with_arg()` only *queues* the launch; the spawned task needs
/// its own scheduling turn before it actually registers in the process
/// table, so `is_running()` can still read false for a little while after a
/// successful `start_service()` call returns. A real bug this exact race
/// caused during testing: two rapid `start_service()` calls (one checking
/// "already running" *before* the first call's spawn had registered) both
/// launched their own `rtl8139d` instance, leaving two running concurrently.
static STARTING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Launch a fresh `rtl8139d` instance on the same already-initialized
/// hardware/mailbox — the one-time PCI/DMA bring-up in `init()` never needs
/// to be redone, only the ring-3 process itself. Blocks (bounded spin) until
/// the new instance is actually confirmed running before returning `Ok`, so
/// callers never see a false "success" for a launch that hasn't landed yet
/// — see `STARTING`'s doc comment for the double-launch bug this closes.
pub fn start_service() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;
    if !is_enabled() { return Err("disabled"); }
    if is_running() { return Err("already running"); }
    if STARTING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("start already in progress");
    }
    let result = (|| {
        let (mb, mailbox_phys) = {
            let g = NIC.lock();
            match g.as_ref() { Some(n) => (n.mailbox_virt, n.mailbox_phys), None => return Err("driver not initialized") }
        };
        if RTL8139D_ELF.is_empty() { return Err("rtl8139d ELF not built"); }
        unsafe { core::ptr::write_volatile(&mut (*mb).stop, 0); }
        process::exec_async_with_arg(usize::MAX, SERVICE_NAME, RTL8139D_ELF, mailbox_phys)?;
        for _ in 0..STOP_WAIT_SPINS {
            if is_running() { return Ok(()); }
            core::hint::spin_loop();
        }
        Err("timeout waiting for driver to start")
    })();
    STARTING.store(false, Ordering::Release);
    result
}

// Baked-in rtl8139d ELF (generated by build.rs from
// userspace/target/.../rtl8139d). Empty slice if userspace hasn't been
// rebuilt since this driver was added.
include!(concat!(env!("OUT_DIR"), "/rtl8139d_elf.rs"));

pub fn init(devices: &[crate::pci::PciDevice]) {
    use crate::serial;
    for d in devices {
        if d.vendor_id != 0x10EC || d.device_id != 0x8139 { continue; }
        serial::print("rtl8139: found\n");

        // Enable I/O space + bus master
        let cmd = crate::pci::config_read16(d.bus, d.dev, d.func, 0x04);
        crate::pci::config_write32(d.bus, d.dev, d.func, 0x04, (cmd | 0x05) as u32);

        // I/O BAR at BAR0 (bit 0 = 1 = I/O)
        let bar0 = crate::pci::config_read32(d.bus, d.dev, d.func, 0x10);
        if bar0 & 1 == 0 { serial::print("rtl8139: BAR0 not I/O\n"); continue; }
        let io = (bar0 & !3) as u16;
        serial::print_hex("rtl8139: io_base", io as u64);

        // Power on
        outb(io + CONFIG1, 0x00);

        // Software reset
        outb(io + CMD, CMD_RST);
        for _ in 0..100_000u32 {
            if inb(io + CMD) & CMD_RST == 0 { break; }
            core::hint::spin_loop();
        }
        serial::print("rtl8139: reset done\n");

        // Read MAC (one-time, kernel-side — no ongoing access needed after this)
        let mut mac = [0u8; 6];
        for i in 0..6 { mac[i] = inb(io + MAC0 + i as u16); }
        serial::print("rtl8139: MAC=");
        for (i,&b) in mac.iter().enumerate() {
            if i>0 { serial::print(":"); }
            serial::print_hex("", b as u64);
        }
        serial::print("\n");

        // TX buffers: 4 contiguous pages (one per slot — see TX_SLOT_STRIDE)
        // so the driver process's mapping is a single MMIO grant instead of four.
        let tx_phys = match pmm::alloc_contiguous(TX_SLOTS) {
            Some(p) => p,
            None => { serial::print("rtl8139: tx OOM\n"); continue; }
        };
        unsafe { core::ptr::write_bytes(vmm::phys_to_virt(tx_phys), 0, TX_SLOTS * 4096); }
        for i in 0..TX_SLOTS {
            // Mark every slot as OWN (bit 13 = 1 means we own it) — the
            // same one-time priming the old in-kernel driver did.
            outl(io + TXSTAT0 + i as u16 * 4, TXSTAT_OWN);
        }

        // RX buffer (64KB + 16 = 17 pages)
        let rx_pages = RX_BUF_LEN.div_ceil(4096);
        let rx_phys = match pmm::alloc_contiguous(rx_pages) {
            Some(p) => p,
            None => { serial::print("rtl8139: rx OOM\n"); continue; }
        };
        unsafe { core::ptr::write_bytes(vmm::phys_to_virt(rx_phys), 0, rx_pages * 4096); }
        serial::print_hex("rtl8139: rx_phys", rx_phys);

        // Shared mailbox page — the only thing the kernel and the
        // `rtl8139d` userspace process both touch going forward.
        let mailbox_phys = match pmm::alloc_page() {
            Some(p) => p,
            None => { serial::print("rtl8139: mailbox OOM\n"); continue; }
        };
        let mailbox_virt = vmm::phys_to_virt(mailbox_phys) as *mut Mailbox;
        unsafe {
            core::ptr::write_bytes(mailbox_virt as *mut u8, 0, 4096);
            (*mailbox_virt).io_base = io as u32;
            (*mailbox_virt).tx_phys = tx_phys as u32;
            (*mailbox_virt).rx_phys = rx_phys as u32;
        }

        // Setup RX buffer
        outl(io + RXBUF, rx_phys as u32);

        // Disable interrupts — rtl8139d polls; this device has no real IRQ
        // for a `SYS_WAIT_IRQ`-style wait to attach to.
        outw(io + IMR, 0);

        // Enable TX+RX
        outb(io + CMD, CMD_TE | CMD_RE);

        // RX config: accept all frames, 64KB buffer, WRAP=1
        outl(io + RXCFG, RX_ACCEPT_ALL | RX_WRAP | RX_RBLEN_64K);

        // TX config: standard gap
        outl(io + TXCFG, 0x03000600);

        // Init CAPR to -16 (wrap trick)
        outw(io + CAPR, 0xFFF0u16);

        // Grant the runtime-discovered ranges rtl8139d needs — the kernel's
        // fixed compile-time allowlist (RTC/APIC) obviously can't cover a
        // PCI BAR or a pmm-allocated DMA buffer's address.
        syscall::grant_port_range(io, io + 0x5F);
        syscall::grant_mmio_range(mailbox_phys, 4096);
        syscall::grant_mmio_range(tx_phys, TX_SLOTS as u64 * TX_SLOT_STRIDE);
        syscall::grant_mmio_range(rx_phys, rx_pages as u64 * 4096);

        if RTL8139D_ELF.is_empty() {
            serial::print("rtl8139: rtl8139d ELF not built (run `cargo build --release` in userspace/) — NIC will not send/receive\n");
        } else {
            // Don't spawn the driver task here — `init()` runs during early
            // hardware bring-up, *before* `kmain` registers the scheduler's
            // idle/blink tasks and starts the timer. `scheduler::spawn()`
            // that early collides with the "task 0 becomes kmain's own
            // execution context" bootstrap trick (see `main.rs`'s scheduler
            // setup comment): the driver's freshly-built task slot would
          // land at index 0, get silently overwritten by kmain's own live
            // context on the very first preemption, and the driver would
            // never run — exactly what happened the first time this was
            // tried (confirmed by its startup print never appearing in the
            // serial log even after many seconds of uptime). Instead, stash
            // the mailbox address and let `spawn_pending_driver()` (called
            // once `task_blink` is actually looping, i.e. the scheduler is
            // fully live) do the real spawn.
            *PENDING_DRIVER_MAILBOX.lock() = Some(mailbox_phys);
            serial::print("rtl8139: rtl8139d queued to launch once the scheduler is up\n");
        }

        serial::print("rtl8139: ready\n");

        *NIC.lock() = Some(Rtl8139 { mac, mailbox_virt, mailbox_phys });
        return;
    }
    serial::print("rtl8139: not found\n");
}
