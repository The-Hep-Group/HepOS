#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Persistent userspace GOP-flush driver — the first slice of PLAN.md's GOP
// Phase 2 (moving the desktop compositor toward userspace) actually moved.
//
// GOP has no ongoing hardware polling loop the way a NIC/disk/USB controller
// does — the display mode is set once by HepBL before the kernel even
// starts, and after that it's just a block of physical memory the CPU
// writes into. The compositor (desktop.rs/terminal.rs/editor.rs/etc, ~6,400
// lines, ~263 call sites into kernel-internal state) stays entirely
// in-kernel for now — moving *that* is the much bigger remaining task this
// doesn't attempt. What this DOES move is the one thing that actually is a
// repeated "hot path" analogous to a NIC's RX ring or a disk's I/O queue:
// copying a rendered frame out to the real GOP framebuffer every frame
// (`kernel/src/framebuffer.rs`'s old `Display::flush()`/`flush_rows()`).
//
// **Reads from a dedicated `publish` buffer, not the kernel's live
// `backbuf` — a real bug found the hard way, not a design choice made up
// front.** The first version pointed this process straight at `backbuf`
// and just copied whatever was there when `req` changed. Since `backbuf`
// keeps getting overwritten by the kernel's *next* rendered frame the
// instant `flush()`/`flush_rows()` returns (they never waited on anything),
// this process could end up mid-copy of one frame while the kernel was
// already rendering the next one into the same memory — real tearing,
// visible as flicker especially during window drags. Fixed with a
// request/ack handshake: the kernel only writes a new snapshot into
// `publish_phys` while `ack == req` (i.e. this process has fully finished
// the *previous* one), and this process sets `ack = req` only after
// finishing its copy out to the real framebuffer — see
// `kernel/src/framebuffer.rs`'s `GopMailbox` doc comment for the complete
// reasoning on why that's race-free without a lock.
//
// **Mailbox layout must stay byte-for-byte identical to the kernel's copy**
// (`kernel/src/framebuffer.rs`'s `GopMailbox` struct) — there's no shared
// crate between the two to enforce that, since userspace crates can't
// depend on kernel code at all (different target, no `std`, different
// address space).

use hepos_std::println;

#[repr(C)]
struct Mailbox {
    fb_phys:      u64,
    publish_phys: u64,
    fb_pitch:     u32,
    width:        u32,
    height:       u32,
    dirty_y:      u32,
    dirty_count:  u32,
    req:          u32,
    ack:          u32,
    stop:         u32,
}

#[no_mangle]
pub unsafe extern "C" fn _start(mailbox_phys: u64) -> ! {
    println!("gopd: starting (mailbox phys {:#x})", mailbox_phys);

    let mb_va = hepos_rt::sys_mmap_mmio(mailbox_phys, core::mem::size_of::<Mailbox>() as u64);
    if mb_va == 0 {
        println!("gopd: failed to map mailbox — exiting");
        hepos_rt::sys_exit(1);
    }
    let mb = &mut *(mb_va as *mut Mailbox);

    let fb_phys      = core::ptr::read_volatile(&mb.fb_phys);
    let publish_phys = core::ptr::read_volatile(&mb.publish_phys);
    let fb_pitch     = core::ptr::read_volatile(&mb.fb_pitch) as usize;
    let width        = core::ptr::read_volatile(&mb.width) as usize;
    let height       = core::ptr::read_volatile(&mb.height) as usize;

    let fb_va      = hepos_rt::sys_mmap_mmio(fb_phys, (height * fb_pitch) as u64);
    let publish_va = hepos_rt::sys_mmap_mmio(publish_phys, (width * height * 4) as u64);
    if fb_va == 0 || publish_va == 0 {
        println!("gopd: failed to map framebuffer/publish buffer — exiting");
        hepos_rt::sys_exit(1);
    }

    println!("gopd: ready ({}x{}, pitch {})", width, height, fb_pitch);

    let fb      = fb_va as *mut u32;
    let publish = publish_va as *mut u32;
    let pitch_u32 = fb_pitch / 4;

    // Starts at 0, matching the kernel's own zeroed `req` at handoff time
    // (see `spawn_gopd()`'s doc comment) — both sides agree on a clean
    // initial state, including across a `service stop`/`start` restart.
    let mut last_req: u32 = 0;

    loop {
        if core::ptr::read_volatile(&mb.stop) != 0 {
            println!("gopd: stop requested, exiting");
            hepos_rt::sys_exit(0);
        }

        let req = core::ptr::read_volatile(&mb.req);
        if req != last_req {
            last_req = req;
            let dirty_y     = (core::ptr::read_volatile(&mb.dirty_y) as usize).min(height);
            let dirty_count = core::ptr::read_volatile(&mb.dirty_count) as usize;
            let y1 = (dirty_y + dirty_count).min(height);
            // Safe to read `publish` here: the kernel guarantees it won't
            // write a new snapshot into it until it sees `ack == req`, which
            // we haven't posted yet.
            for row in dirty_y..y1 {
                core::ptr::copy_nonoverlapping(
                    publish.add(row * width),
                    fb.add(row * pitch_u32),
                    width,
                );
            }
            // Signal completion — only now may the kernel reuse `publish`
            // for the next snapshot.
            core::ptr::write_volatile(&mut mb.ack, req);
        }

        // Rate-limit to once per timer tick instead of spinning at 100% CPU
        // — same reasoning as every other userspace driver in this kernel
        // (see rtl8139d/hdad/ahcid/xhcid/nvmed): a never-yielding poll loop
        // starves the host's own I/O handling under this project's
        // single-vCPU TCG emulation.
        hepos_rt::sys_wait_irq(0x20);
    }
}
