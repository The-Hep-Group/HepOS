#![no_std]
#![no_main]
#![feature(stmt_expr_attributes)]
extern crate alloc;

mod acpi;
mod apic;
mod editor;
mod e1000;
mod net;
mod rtc;
mod rtl8139;
mod virtio_net;
mod xhci;
mod desktop;
mod framebuffer;
mod gdt;
mod heap;
mod hepfs;
mod idt;
mod mouse;
mod nvme;
mod paging;
mod panic;
mod pci;
mod pmm;
mod ps2;
mod elf;
mod process;
mod scheduler;
mod serial;
mod syscall;
mod terminal;
mod vmm;

use framebuffer::Display;
use limine::request::{FramebufferRequest, HhdmRequest};
use limine::BaseRevision;
use spin::Mutex;

// Global display — used by exception handler and future modules
pub static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

// Focus: Some(id) = that window has keyboard focus; defaults to terminal (id=2)
pub static FOCUSED_WIN: Mutex<Option<usize>> = Mutex::new(None);
pub static PCI_DEVS: Mutex<alloc::vec::Vec<pci::PciDevice>> = Mutex::new(alloc::vec::Vec::new());

// Frame counter for uptime (~60 fps → divide by 60 for seconds)
static UPTIME_FRAMES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// HepFS navigator state (current directory + back/forward history)
struct HepfsNav {
    ino:  u32,
    path: alloc::string::String,
    back: alloc::vec::Vec<(u32, alloc::string::String)>,
    fwd:  alloc::vec::Vec<(u32, alloc::string::String)>,
}
static HEPFS_NAV: Mutex<Option<HepfsNav>> = Mutex::new(None);

#[used] static BASE_REVISION:       BaseRevision       = BaseRevision::new();
#[used] static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();
#[used] static HHDM_REQUEST:        HhdmRequest        = HhdmRequest::new();

#[no_mangle]
extern "C" fn kmain() -> ! {
    serial::init();
    serial::print("HepOS kmain\n");

    gdt::init();
    serial::print("GDT loaded\n");

    idt::init();
    serial::print("IDT loaded\n");

    let hhdm = HHDM_REQUEST.response().expect("no HHDM").offset;
    vmm::init(hhdm);
    pmm::init(hhdm);
    serial::print("PMM init\n");

    heap::HEAP.init();
    serial::print("Heap init\n");

    // smoke test: allocate and use a Vec
    {
        use alloc::vec::Vec;
        let mut v: Vec<u32> = Vec::new();
        for i in 0..16 { v.push(i); }
        serial::print("Heap smoke test OK\n");
        let _ = v;
    }

    syscall::init();

    let fb = FRAMEBUFFER_REQUEST
        .response()
        .and_then(|r| r.framebuffers().first().copied())
        .expect("no framebuffer");

    *DISPLAY.lock() = Some(Display::new(fb));

    {
        let mut guard = DISPLAY.lock();
        let display = guard.as_mut().unwrap();

        display.clear(framebuffer::Color::from_hex(0x0D0D0D));

        let accent = framebuffer::Color::from_hex(0x6C8EFF);
        let white  = framebuffer::Color::from_hex(0xE8E8E8);
        let dim    = framebuffer::Color::from_hex(0x555555);

        display.fill_rect(0, 0, display.width(), 2, accent);

        let x_mid = display.width() / 2;
        let y_mid = display.height() / 2;

        display.draw_text(x_mid - 72, y_mid - 24, "HepOS",               accent, 3);
        display.draw_text(x_mid - 88, y_mid + 16, "kernel alive",         white,  2);
        display.draw_text(x_mid - 96, y_mid + 48, "v0.1 | x86_64 exokernel", dim, 1);

        // show memory stats
        let free_mb  = pmm::free_pages()  * 4 / 1024;
        let total_mb = pmm::total_pages() * 4 / 1024;
        let mut buf = [0u8; 64];
        let mem_str = fmt_mem(free_mb, total_mb, &mut buf);
        display.draw_text(x_mid - (mem_str.len() * 9 / 2), y_mid + 72, mem_str, dim, 1);
    }

    // Allocate backbuffer so all rendering is atomic (no tearing / flicker)
    if let Some(display) = DISPLAY.lock().as_mut() {
        display.init_backbuf();
    }

    // Init desktop BEFORE enabling interrupts so task_blink sees it immediately
    {
        let fb = FRAMEBUFFER_REQUEST.response()
            .and_then(|r| r.framebuffers().first().copied())
            .expect("no framebuffer for desktop");
        let w = fb.width as usize;
        let h = fb.height as usize;
        let mut dt = desktop::Desktop::new(w, h);
        // Window positions chosen to fit common resolutions (640×480 min)
        dt.add_window("Welcome to HepOS", 20,  50,  300, 160);
        dt.add_window("HepFS",            340, 50,  260, 160);
        dt.add_window("Terminal",         20,  240, 580, 200);
        dt.set_terminal_window(2);
        // Editor window (id=3) — hidden until `edit` command opens a file
        dt.add_window("Editor",           60,  40,  580, 380);
        // Sysmon window (id=4) — hidden until opened from start menu
        dt.add_window("Sysmon",           80,  60,  340, 260);
        *desktop::DESKTOP.lock() = Some(dt);
    }

    // Init terminal NOW before sti so task_blink sees it immediately
    terminal::init();
    serial::print("Terminal init\n");

    // HepFS navigator starts at root
    *HEPFS_NAV.lock() = Some(HepfsNav {
        ino:  hepfs::ROOT_INO,
        path: alloc::string::String::from("/"),
        back: alloc::vec::Vec::new(),
        fwd:  alloc::vec::Vec::new(),
    });

    // Minimize editor and sysmon until explicitly opened; focus terminal (id=2)
    {
        let mut dt = desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            for id in [3usize, 4] {
                if let Some(w) = dt.windows.iter_mut().find(|w| w.id == id) {
                    w.minimized = true;
                }
            }
        }
    }
    *FOCUSED_WIN.lock() = Some(2);

    // PCI enumeration (interrupts still off — APIC not started yet)
    let pci_devices = pci::enumerate();
    // Store for lspci command
    *PCI_DEVS.lock() = pci_devices.clone();
    serial::print("PCI devices:\n");
    for d in &pci_devices {
        serial::print("  ");
        serial::print(pci::class_name(d.class, d.subclass));
        serial::print("\n");
    }

    // NVMe
    if let Some(mut ctrl) = nvme::init(&pci_devices) {
        serial::print("NVMe ready\n");
        let s = alloc::format!(
            "NVMe: {} MB  ({} byte blocks)\n",
            ctrl.lba_count * ctrl.lba_size as u64 / 1024 / 1024,
            ctrl.lba_size
        );
        serial::print(&s);
        // smoke test: write then read block 0
        let (phys, virt) = {
            let p = pmm::alloc_page().unwrap();
            (p, vmm::phys_to_virt(p))
        };
        unsafe { core::ptr::write_bytes(virt, 0xAB, 512); }
        ctrl.write_blocks(0, 1, phys);
        unsafe { core::ptr::write_bytes(virt, 0x00, 512); }
        ctrl.read_blocks(0, 1, phys);
        let ok = unsafe { *(virt as *const u8) } == 0xAB;
        serial::print(if ok { "NVMe R/W OK\n" } else { "NVMe R/W FAIL\n" });

        // HepFS
        if !hepfs::probe(&mut ctrl) {
            serial::print("Formatting HepFS...\n");
            hepfs::format(&mut ctrl);
            serial::print("HepFS formatted\n");
        } else {
            serial::print("HepFS found\n");
        }

        // Smoke test: create dirs + file, write, read back
        hepfs::create_dir(&mut ctrl, hepfs::ROOT_INO, "home");
        hepfs::create_dir(&mut ctrl, hepfs::ROOT_INO, "etc");
        let home = hepfs::lookup(&mut ctrl, "/home").unwrap();
        let fno  = hepfs::create_file(&mut ctrl, home, "hello.txt");
        hepfs::write_file(&mut ctrl, fno, b"Hello from HepOS!\n");
        let data = hepfs::read_file(&mut ctrl, fno);
        let s    = core::str::from_utf8(&data).unwrap_or("?");
        serial::print("Read back: ");
        serial::print(s);

        let entries = hepfs::list_dir(&mut ctrl, hepfs::ROOT_INO);
        serial::print("/ contents:\n");
        for (_, name) in &entries { serial::print("  "); serial::print(name); serial::print("\n"); }

        // Store controller globally so apps can use it
        *nvme::CONTROLLER.lock() = Some(ctrl);

        // Write kernel manifest to HepFS (skipped if already exists)
        {
            let mut c = nvme::CONTROLLER.lock();
            if let Some(ctrl) = c.as_mut() {
                if hepfs::lookup(ctrl, "/kernel.txt").is_none() {
                    let ino = hepfs::create_file(ctrl, hepfs::ROOT_INO, "kernel.txt");
                    let mut db = [0u8; 11];
                    let date = rtc::fmt_date(&mut db);
                    let content = alloc::format!(
                        "HepOS v0.1 | {} | x86_64 exokernel\n",
                        date
                    );
                    hepfs::write_file(ctrl, ino, content.as_bytes());
                }
            }
        }

    } else {
        serial::print("No NVMe device found\n");
    }

    // Networking — try RTL8139 first (simplest QEMU NIC), then e1000
    rtl8139::init(&pci_devices);
    if rtl8139::NIC.lock().is_none() { e1000::init(&pci_devices); }
    net::arp_announce();
    serial::print("Network init\n");

    // Input devices
    ps2::init();
    mouse::init();
    xhci::init(&pci_devices);
    serial::print("Input init\n");

    serial::print("Boot complete\n");

    // Register scheduler tasks and start APIC timer AFTER all init is stable.
    // First timer tick switches from kmain → task_blink; task_blink runs forever
    // (polling-based, doesn't need interrupts enabled).
    {
        let mut sched = scheduler::SCHEDULER.lock();
        sched.add(scheduler::Task::new(0, "idle",  task_idle));
        sched.add(scheduler::Task::new(1, "blink", task_blink));
        sched.tasks[0].state = scheduler::TaskState::Running;
    }
    idt::set_handler(apic::timer_vector(), idt::timer_stub as u64);
    apic::init();
    serial::print("APIC init\n");

    // Enable interrupts — first timer tick will switch to task_blink
    unsafe { core::arch::asm!("sti", options(nomem, nostack)); }

    loop { core::hint::spin_loop(); }
}

fn task_idle() -> ! {
    loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
}

fn task_blink() -> ! {
    let mut mx: i32 = 400;
    let mut my: i32 = 300;
    let mut btn: u8  = 0;
    let mut prev_btn: u8 = 0;
    // Track where the cursor was last painted so we can restore only those rows
    let mut prev_cursor_y: usize = 300;

    loop {
        ps2::poll();
        mouse::poll();

        // XHCI USB tablet — absolute coordinates (overrides PS/2 relative if available)
        {
            let (fw, fh) = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().map(|d| (d.fb_w as u32, d.fb_h as u32)).unwrap_or((640, 480))
            };
            xhci::poll_mouse(fw, fh);
        }

        // PS/2 or USB mouse updates
        {
            let m = mouse::MOUSE.lock();
            mx = m.x;
            my = m.y;
            btn = m.buttons;
        }

        // Keyboard routing: editor gets all keys when focused, otherwise terminal
        let mut ps2_had_input = false;
        while let Some(c) = ps2::read_char() {
            ps2_had_input = true;
            let focused = *FOCUSED_WIN.lock();

            if focused == Some(3) {
                // Editor has focus — route all keys
                let mut eg = editor::EDITOR.lock();
                if let Some(ed) = eg.as_mut() {
                    ed.on_key(c);
                    if !ed.open {
                        drop(eg);
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 3) {
                                w.minimized = true;
                            }
                            dt.dirty = true;
                        }
                        *FOCUSED_WIN.lock() = Some(2);
                    }
                }
            } else {
                let focused = *FOCUSED_WIN.lock();
                // Route to the focused extra terminal if one is focused
                let routed_extra = {
                    let mut et = terminal::EXTRA_TERMINALS.lock();
                    if let Some((_, t)) = et.iter_mut().find(|(wid, _)| Some(*wid) == focused) {
                        t.on_key(c);
                        true
                    } else { false }
                };
                if !routed_extra {
                    let mut tg = terminal::TERMINAL.lock();
                    if let Some(t) = tg.as_mut() { t.on_key(c); }
                }
            }
        }

        // Clamp and write back mouse state
        let (fb_w, fb_h) = {
            let dt = desktop::DESKTOP.lock();
            dt.as_ref().map(|d| (d.fb_w as i32, d.fb_h as i32)).unwrap_or((1280, 720))
        };
        mx = mx.clamp(0, fb_w - 1);
        my = my.clamp(0, fb_h - 1);
        {
            let mut m = mouse::MOUSE.lock();
            m.x = mx; m.y = my; m.buttons = btn;
        }

        // Update WM (update_mouse sets dirty flag if position changed)
        let fresh_click = btn & 1 != 0 && prev_btn & 1 == 0;
        prev_btn = btn;
        {
            let mut dt_guard = desktop::DESKTOP.lock();
            if let Some(dt) = dt_guard.as_mut() {
                dt.update_mouse(mx, my, btn);
            }
        }

        // Sync keyboard focus with visual focus whenever a mouse click brings a window forward.
        // This fixes the case where the user clicks a window in cursor mode and expects to type.
        if fresh_click {
            let clicked_focus = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().and_then(|d| d.focused)
            };
            if let Some(fid) = clicked_focus {
                *FOCUSED_WIN.lock() = Some(fid);
            }
        }

        // HepFS window: navigate directories and open files on click
        if fresh_click {
            // Determine where in the HepFS window was clicked
            let hepfs_click = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().and_then(|d| {
                    let win = d.windows.iter().find(|w| w.id == 1 && !w.minimized)?;
                    if mx < win.x || mx >= win.x + win.w as i32 { return None; }
                    if my < win.y || my >= win.y + win.h as i32  { return None; }
                    let rel_x = (mx - win.x) as usize;
                    let rel_y = my - win.y;
                    if rel_y < 22 {
                        // Nav bar: back=0, fwd=1, none=2
                        let zone = if rel_x < 22 { 0usize }
                                   else if rel_x < 44 { 1 }
                                   else { 2 };
                        Some((0i32, zone)) // rel_y sentinel 0 = nav bar
                    } else {
                        // File list (entries start at row y=23)
                        let entry_idx = (rel_y - 23).max(0) as usize / 14;
                        Some((1, entry_idx))
                    }
                })
            };

            match hepfs_click {
                Some((0, 0)) => {
                    // Back button
                    let mut nav = HEPFS_NAV.lock();
                    if let Some(nav) = nav.as_mut() {
                        if let Some((pino, ppath)) = nav.back.pop() {
                            let cur_ino  = nav.ino;
                            let cur_path = nav.path.clone();
                            nav.fwd.push((cur_ino, cur_path));
                            nav.ino  = pino;
                            nav.path = ppath;
                        }
                    }
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                }
                Some((0, 1)) => {
                    // Forward button
                    let mut nav = HEPFS_NAV.lock();
                    if let Some(nav) = nav.as_mut() {
                        if let Some((nino, npath)) = nav.fwd.pop() {
                            let cur_ino  = nav.ino;
                            let cur_path = nav.path.clone();
                            nav.back.push((cur_ino, cur_path));
                            nav.ino  = nino;
                            nav.path = npath;
                        }
                    }
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                }
                Some((1, idx)) => {
                    // File list entry click
                    let cur_ino = HEPFS_NAV.lock().as_ref().map(|n| n.ino).unwrap_or(hepfs::ROOT_INO);
                    let at_root = cur_ino == hepfs::ROOT_INO;

                    // ".." row (only shown when not at root)
                    if !at_root && idx == 0 {
                        // Navigate up: back button behaviour
                        let mut nav = HEPFS_NAV.lock();
                        if let Some(nav) = nav.as_mut() {
                            if let Some((pino, ppath)) = nav.back.pop() {
                                let ci = nav.ino;
                                let cp = nav.path.clone();
                                nav.fwd.push((ci, cp));
                                nav.ino  = pino;
                                nav.path = ppath;
                            }
                        }
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                    } else {
                        // Real entry index (subtract 1 if ".." row is shown)
                        let real_idx = if !at_root { idx.saturating_sub(1) } else { idx };
                        let entry = {
                            let mut ctrl = nvme::CONTROLLER.lock();
                            ctrl.as_mut().and_then(|ctrl| {
                                let entries = hepfs::list_dir(ctrl, cur_ino);
                                entries.get(real_idx).map(|(ino, name)| {
                                    let inode = hepfs::read_inode(ctrl, *ino);
                                    (*ino, name.clone(), inode.flags)
                                })
                            })
                        };
                        if let Some((ino, name, flags)) = entry {
                            if flags == hepfs::F_DIR {
                                // Navigate into directory
                                let mut nav = HEPFS_NAV.lock();
                                if let Some(nav) = nav.as_mut() {
                                    let cur_ino2 = nav.ino;
                                    let cur_path = nav.path.clone();
                                    nav.back.push((cur_ino2, cur_path));
                                    nav.fwd.clear();
                                    nav.ino = ino;
                                    nav.path = if nav.path == "/" {
                                        alloc::format!("/{}", name)
                                    } else {
                                        alloc::format!("{}/{}", nav.path, name)
                                    };
                                }
                                let mut dt = desktop::DESKTOP.lock();
                                if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                            } else {
                                // Open file in editor
                                let cur_path = HEPFS_NAV.lock().as_ref().map(|n| n.path.clone())
                                    .unwrap_or_else(|| alloc::string::String::from("/"));
                                let file_path = if cur_path == "/" {
                                    alloc::format!("/{}", name)
                                } else {
                                    alloc::format!("{}/{}", cur_path, name)
                                };
                                editor::open(&file_path);
                                {
                                    let mut dt = desktop::DESKTOP.lock();
                                    if let Some(dt) = dt.as_mut() {
                                        if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 3) {
                                            w.minimized = false;
                                        }
                                        dt.bring_to_front(3);
                                        dt.dirty = true;
                                    }
                                }
                                *FOCUSED_WIN.lock() = Some(3);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Also force dirty after keyboard input so terminal text appears
        if ps2_had_input {
            let mut dt = desktop::DESKTOP.lock();
            if let Some(dt) = dt.as_mut() { dt.dirty = true; }
            let mut tm = terminal::TERMINAL.lock();
            if let Some(tm) = tm.as_mut() { tm.dirty = true; }
        }

        // Two-tier rendering:
        //   content_dirty → full scene redraw + save_scene + cursor + full flush
        //   mouse_only    → restore scene rows near cursor + repaint cursor + partial flush
        // The partial path updates only ~20 rows (~100 KB) instead of 3.5 MB, so
        // cursor movement is extremely cheap and runs at the full polling rate.
        let content_dirty = {
            let dd = desktop::DESKTOP.lock().as_ref().map(|d| d.dirty).unwrap_or(false);
            let td = terminal::TERMINAL.lock().as_ref().map(|t| t.dirty).unwrap_or(false);
            let ed = terminal::EXTRA_TERMINALS.lock().iter().any(|(_, t)| t.dirty);
            dd || td || ed || ps2_had_input
        };
        let mouse_moved = {
            let md = desktop::DESKTOP.lock().as_ref().map(|d| d.mouse_dirty).unwrap_or(false);
            md
        };
        // Consume both flags before rendering
        if mouse_moved {
            let mut dt = desktop::DESKTOP.lock();
            if let Some(dt) = dt.as_mut() { dt.mouse_dirty = false; }
        }

        // ── Closure-like block for cursor drawing ─────────────────────────────
        // Shared between both render paths. Returns the cursor y-span (y0, rows).
        macro_rules! draw_cursor {
            ($display:expr, $cx:expr, $cy:expr) => {{
                let cx = $cx;
                let cy = $cy;
                let white = framebuffer::Color::from_hex(0xFFFFFF);
                let black = framebuffer::Color::from_hex(0x111111);
                let cursor_type = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().map(|d| d.cursor_type_at(mx, my))
                        .unwrap_or(desktop::CursorType::Normal)
                };
                match cursor_type {
                    // SE diagonal (↘)
                    desktop::CursorType::ResizeNWSE => {
                        for i in -4_i32..=4 {
                            $display.put_pixel_pub((cx as i32+i+1).max(0) as usize, (cy as i32+i+1).max(0) as usize, black);
                        }
                        $display.fill_rect(cx.saturating_sub(3), cy.saturating_sub(5)+1, 5, 1, black);
                        $display.fill_rect(cx.saturating_sub(5)+1, cy.saturating_sub(3), 1, 4, black);
                        $display.fill_rect(cx+1, cy+5, 5, 1, black);
                        $display.fill_rect(cx+5, cy+1, 1, 4, black);
                        for i in -4_i32..=4 {
                            $display.put_pixel_pub((cx as i32+i).max(0) as usize, (cy as i32+i).max(0) as usize, white);
                        }
                        $display.fill_rect(cx.saturating_sub(3), cy.saturating_sub(5), 5, 1, white);
                        $display.fill_rect(cx.saturating_sub(5), cy.saturating_sub(3), 1, 4, white);
                        $display.fill_rect(cx+1, cy+5, 4, 1, white);
                        $display.fill_rect(cx+5, cy+1, 1, 4, white);
                    }
                    // NE diagonal (↗)
                    desktop::CursorType::ResizeNESW => {
                        for i in -4_i32..=4 {
                            $display.put_pixel_pub((cx as i32+i+1).max(0) as usize, (cy as i32-i+1).max(0) as usize, black);
                        }
                        $display.fill_rect(cx.saturating_sub(3), cy+4, 5, 1, black);
                        $display.fill_rect(cx.saturating_sub(5)+1, cy.saturating_sub(3), 1, 4, black);
                        $display.fill_rect(cx+1, cy.saturating_sub(5)+1, 5, 1, black);
                        $display.fill_rect(cx+5, cy.saturating_sub(3), 1, 4, black);
                        for i in -4_i32..=4 {
                            $display.put_pixel_pub((cx as i32+i).max(0) as usize, (cy as i32-i).max(0) as usize, white);
                        }
                        $display.fill_rect(cx.saturating_sub(3), cy+4, 4, 1, white);
                        $display.fill_rect(cx.saturating_sub(5), cy.saturating_sub(3), 1, 4, white);
                        $display.fill_rect(cx+1, cy.saturating_sub(5), 5, 1, white);
                        $display.fill_rect(cx+5, cy.saturating_sub(3), 1, 4, white);
                    }
                    // Horizontal resize (↔)
                    desktop::CursorType::ResizeEW => {
                        // Black outline
                        $display.fill_rect(cx.saturating_sub(7), cy, 15, 1, black);
                        $display.fill_rect(cx.saturating_sub(7), cy.saturating_sub(3), 1, 7, black);
                        $display.fill_rect(cx+7, cy.saturating_sub(3), 1, 7, black);
                        // White fill
                        $display.fill_rect(cx.saturating_sub(6), cy, 13, 1, white);
                        $display.fill_rect(cx.saturating_sub(6), cy.saturating_sub(2), 1, 5, white);
                        $display.fill_rect(cx+6, cy.saturating_sub(2), 1, 5, white);
                    }
                    // Vertical resize (↕)
                    desktop::CursorType::ResizeNS => {
                        // Black outline
                        $display.fill_rect(cx, cy.saturating_sub(7), 1, 15, black);
                        $display.fill_rect(cx.saturating_sub(3), cy.saturating_sub(7), 7, 1, black);
                        $display.fill_rect(cx.saturating_sub(3), cy+7, 7, 1, black);
                        // White fill
                        $display.fill_rect(cx, cy.saturating_sub(6), 1, 13, white);
                        $display.fill_rect(cx.saturating_sub(2), cy.saturating_sub(6), 5, 1, white);
                        $display.fill_rect(cx.saturating_sub(2), cy+6, 5, 1, white);
                    }
                    // Normal crosshair
                    desktop::CursorType::Normal => {
                        $display.fill_rect(cx.saturating_sub(6), cy, 13, 1, white);
                        $display.fill_rect(cx, cy.saturating_sub(6), 1, 13, white);
                    }
                }
            }};
        }

        if content_dirty {
            if let Some(display) = DISPLAY.lock().as_mut() {
                // 1. Clear background
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.render(display, mx, my); } }
                { let mut dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_mut() { dt.dirty = false; } }

                // 2. Windows in z-order
                let win_order: alloc::vec::Vec<(usize, bool, i32, i32, usize, usize)> = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().map(|d| d.windows.iter()
                        .filter(|w| !w.minimized)
                        .map(|w| (w.id, d.focused == Some(w.id), w.x, w.y, w.w, w.h))
                        .collect()
                    ).unwrap_or_default()
                };
                for (id, focused, wx, wy, ww, wh) in &win_order {
                    { let dt = desktop::DESKTOP.lock();
                      if let Some(dt) = dt.as_ref() {
                          if let Some(win) = dt.windows.iter().find(|w| w.id == *id) {
                              dt.draw_window(display, win, *focused);
                          }
                      }
                    }
                    let wx = (*wx).max(0) as usize;
                    let wy = (*wy).max(0) as usize;
                    match id {
                        0 => render_welcome_window(display),
                        1 => render_hepfs_window(display),
                        2 => { let mut tg = terminal::TERMINAL.lock();
                               if let Some(t) = tg.as_mut() {
                                   t.render(display, wx, wy, *ww, *wh);
                                   t.dirty = false;
                               } }
                        3 => { let mut eg = editor::EDITOR.lock();
                               if let Some(ed) = eg.as_mut() {
                                   ed.render(display, wx, wy, *ww, *wh);
                               } }
                        4 => render_sysmon_window(display),
                        _ => {
                            // Extra terminals spawned via `newterm`
                            let mut et = terminal::EXTRA_TERMINALS.lock();
                            if let Some((_, t)) = et.iter_mut().find(|(wid, _)| *wid == *id) {
                                t.render(display, wx, wy, *ww, *wh);
                                t.dirty = false;
                            }
                        }
                    }
                }

                // 3. Overlays
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_start_menu(display); } }
                // 3a. Snap preview — outlined rect showing where window will snap
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() {
                      if let Some((px, py, pw, ph)) = dt.snap_preview_rect() {
                          let c = desktop::pal::ACCENT;
                          let px = px.max(0) as usize;
                          let py = py.max(0) as usize;
                          display.fill_rect(px, py, pw, 2, c);
                          if ph > 2 { display.fill_rect(px, py + ph - 2, pw, 2, c); }
                          display.fill_rect(px, py, 2, ph, c);
                          if pw > 2 { display.fill_rect(px + pw - 2, py, 2, ph, c); }
                      }
                  }
                }
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_taskbar(display); } }

                // 4. Save scene (no cursor yet) so cursor-only path can erase later
                display.save_scene();

                // 5. Cursor
                let cy = my as usize;
                draw_cursor!(display, mx as usize, cy);
                prev_cursor_y = cy;

                // 6. Full flush
                display.flush();
            }
        } else if mouse_moved {
            if let Some(display) = DISPLAY.lock().as_mut() {
                let cy    = my as usize;
                // Union of old and new cursor spans (crosshair is 13px tall, resize is 12px)
                let span  = 8usize;
                let y0    = prev_cursor_y.saturating_sub(span).min(cy.saturating_sub(span));
                let y1    = (prev_cursor_y + span).max(cy + span);
                let rows  = (y1 - y0 + 1).min(display.height().saturating_sub(y0));

                // Restore scene pixels (erases old cursor)
                display.restore_rows(y0, rows);

                // Paint cursor at new position
                draw_cursor!(display, mx as usize, cy);
                prev_cursor_y = cy;

                // Flush only the affected rows (~100 KB vs 3.5 MB full flush)
                display.flush_rows(y0, rows);
            }
        }

        UPTIME_FRAMES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // No sleep — dirty flags gate rendering; idle task uses hlt so CPU rests between quanta
    }
}

fn window_rect(title: &str) -> Option<(usize, usize, usize, usize)> {
    let dt = desktop::DESKTOP.lock();
    dt.as_ref().and_then(|d| {
        d.windows.iter()
            .find(|w| !w.minimized && w.title.as_str() == title)
            .map(|w| (w.x.max(0) as usize, w.y.max(0) as usize, w.w, w.h))
    })
}

fn render_hepfs_window(display: &mut framebuffer::Display) {
    let Some((wx, wy, ww, wh)) = window_rect("HepFS") else { return; };
    let bg   = framebuffer::Color::from_hex(0x0C0C0C);
    let acc  = framebuffer::Color::from_hex(0x6C8EFF);
    let text = framebuffer::Color::from_hex(0xE8E8E8);
    let dim  = framebuffer::Color::from_hex(0x888888);
    let nav_bg   = framebuffer::Color::from_hex(0x0F0F1A);
    let btn_bg   = framebuffer::Color::from_hex(0x1A1A30);
    let path_bg  = framebuffer::Color::from_hex(0x0D0D18);
    let dir_col  = framebuffer::Color::from_hex(0x88AAFF);

    // Background
    display.fill_rect(wx, wy, ww, wh, bg);

    // ── Nav bar (22px tall) ──────────────────────────────────────────────────
    let nav_h: usize = 22;
    display.fill_rect(wx, wy, ww, nav_h, nav_bg);

    let (has_back, has_fwd, cur_path, cur_ino) = {
        let nav = HEPFS_NAV.lock();
        if let Some(n) = nav.as_ref() {
            (!n.back.is_empty(), !n.fwd.is_empty(), n.path.clone(), n.ino)
        } else {
            (false, false, alloc::string::String::from("/"), hepfs::ROOT_INO)
        }
    };

    // Back button
    display.fill_rect(wx + 2, wy + 4, 18, 14, btn_bg);
    display.draw_text(wx + 6,  wy + 6, "<",
        if has_back { acc } else { dim }, 1);

    // Forward button
    display.fill_rect(wx + 22, wy + 4, 18, 14, btn_bg);
    display.draw_text(wx + 27, wy + 6, ">",
        if has_fwd { acc } else { dim }, 1);

    // Path bar
    let path_x = wx + 44;
    let path_w = ww.saturating_sub(46);
    display.fill_rect(path_x, wy + 4, path_w, 14, path_bg);
    // Trim path if too long for the bar
    let max_chars = path_w / 9;
    let display_path = if cur_path.len() > max_chars && max_chars > 0 {
        &cur_path[cur_path.len() - max_chars..]
    } else { &cur_path };
    display.draw_text(path_x + 2, wy + 6, display_path, text, 1);

    // Separator
    display.fill_rect(wx, wy + nav_h, ww, 1, acc);

    // ── File list ────────────────────────────────────────────────────────────
    let list_top = wy + nav_h + 1;
    let at_root  = cur_ino == hepfs::ROOT_INO;
    let mut y = list_top + 2;

    // ".." entry when not at root
    if !at_root {
        display.draw_text(wx + 4,  y, "d", dim, 1);
        display.draw_text(wx + 16, y, "..", dir_col, 1);
        y += 14;
    }

    let mut ctrl = nvme::CONTROLLER.lock();
    if let Some(ctrl) = ctrl.as_mut() {
        let entries = hepfs::list_dir(ctrl, cur_ino);
        for (ino, name) in &entries {
            if y + 12 > wy + wh { break; }
            let inode = hepfs::read_inode(ctrl, *ino);
            let is_dir = inode.flags == hepfs::F_DIR;
            let (pfx, col) = if is_dir { ("d", dir_col) } else { ("f", text) };
            display.draw_text(wx + 4,  y, pfx, dim, 1);
            display.draw_text(wx + 16, y, name, col, 1);
            // File size on right
            if !is_dir {
                let sz = fmt_size(inode.size);
                let chars = sz.iter().position(|&b| b == 0).unwrap_or(sz.len());
                let sx = wx + ww.saturating_sub(chars * 9 + 4);
                display.draw_text(sx, y, core::str::from_utf8(&sz[..chars]).unwrap_or(""), dim, 1);
            }
            y += 14;
        }
        if entries.is_empty() && at_root {
            display.draw_text(wx + 4, list_top + 4, "(empty)", dim, 1);
        }
    } else {
        display.draw_text(wx + 4, list_top + 4, "No NVMe", dim, 1);
    }
}

/// Format a byte count into a compact string (e.g. "1.2 KB").
fn fmt_size(bytes: u64) -> [u8; 12] {
    let mut buf = [0u8; 12];
    if bytes < 1024 {
        write_num(bytes, &mut buf, "B")
    } else if bytes < 1024 * 1024 {
        write_num(bytes / 1024, &mut buf, "KB")
    } else {
        write_num(bytes / 1024 / 1024, &mut buf, "MB")
    }
    buf
}

fn write_num(n: u64, buf: &mut [u8; 12], suffix: &str) {
    let mut tmp = [0u8; 8];
    let mut i = 8usize;
    let mut n = n;
    if n == 0 { tmp[7] = b'0'; i = 7; }
    while n > 0 { i -= 1; tmp[i] = b'0' + (n % 10) as u8; n /= 10; }
    let num_bytes = &tmp[i..];
    let mut pos = 0usize;
    for &b in num_bytes { if pos < 12 { buf[pos] = b; pos += 1; } }
    buf[pos] = b' '; pos += 1;
    for b in suffix.bytes() { if pos < 12 { buf[pos] = b; pos += 1; } }
}

fn render_sysmon_window(display: &mut framebuffer::Display) {
    let Some((wx, wy, ww, wh)) = window_rect("Sysmon") else { return; };
    let bg     = framebuffer::Color::from_hex(0x0C0C0C);
    let acc    = framebuffer::Color::from_hex(0x6C8EFF);
    let text   = framebuffer::Color::from_hex(0xE8E8E8);
    let dim    = framebuffer::Color::from_hex(0x666688);
    let ok     = framebuffer::Color::from_hex(0x6BFF8E);
    let warn   = framebuffer::Color::from_hex(0xFF9944);
    let red    = framebuffer::Color::from_hex(0xFF6B6B);
    let bar_bg = framebuffer::Color::from_hex(0x1A1A2E);

    display.fill_rect(wx, wy, ww, wh, bg);
    display.fill_rect(wx, wy, ww, 2, acc);

    let mut y = wy + 6;
    let x = wx + 4;

    // ── RAM bar ──────────────────────────────────────────────────────────────
    let free_mb  = pmm::free_pages() * 4 / 1024;
    let total_mb = pmm::total_pages() * 4 / 1024;
    let used_mb  = total_mb.saturating_sub(free_mb);
    display.draw_text(x, y, "RAM", acc, 1);
    let bar_x = x + 32;
    let bar_w = ww.saturating_sub(40).min(240);
    let bar_h = 10usize;
    display.fill_rect(bar_x, y, bar_w, bar_h, bar_bg);
    if total_mb > 0 {
        let pct  = used_mb * 100 / total_mb;
        let fill = (used_mb * bar_w as u64 / total_mb) as usize;
        let bar_col = if pct > 80 { red } else if pct > 60 { warn } else { ok };
        display.fill_rect(bar_x, y, fill, bar_h, bar_col);
    }
    y += bar_h + 2;
    let mem_line = alloc::format!("    {} MB used / {} MB total", used_mb, total_mb);
    display.draw_text(x, y, &mem_line, dim, 1);
    y += 13;

    // ── Uptime ───────────────────────────────────────────────────────────────
    display.fill_rect(x, y, ww.saturating_sub(8), 1, framebuffer::Color::from_hex(0x1A1A30));
    y += 4;
    let frames  = UPTIME_FRAMES.load(core::sync::atomic::Ordering::Relaxed);
    let secs    = frames / 60;
    let mins    = secs / 60;
    let hours   = mins / 60;
    let mut tbuf = [0u8; 32];
    let uptime_str = fmt_hms(hours, mins % 60, secs % 60, &mut tbuf);
    display.draw_text(x, y, "Uptime", acc, 1);
    display.draw_text(x + 56, y, uptime_str, text, 1);
    y += 13;

    // ── System info ──────────────────────────────────────────────────────────
    display.draw_text(x, y, "CPU    x86_64  APIC x2APIC", dim, 1);   y += 13;

    let has_nvme = nvme::CONTROLLER.lock().is_some();
    let nvme_str = if has_nvme { "NVMe OK" } else { "NVMe --" };
    let nvme_col = if has_nvme { ok } else { dim };
    display.draw_text(x, y, "Storage", acc, 1);
    display.draw_text(x + 60, y, nvme_str, nvme_col, 1);
    display.draw_text(x + 130, y, "HepFS OK", ok, 1);
    y += 13;

    let has_nic = rtl8139::NIC.lock().is_some() || e1000::NIC.lock().is_some();
    let net_str = if has_nic { "eth0 up  10.0.2.15" } else { "no NIC" };
    let net_col = if has_nic { ok } else { dim };
    display.draw_text(x, y, "Net", acc, 1);
    display.draw_text(x + 32, y, net_str, net_col, 1);
    y += 13;

    // ── PCI devices ──────────────────────────────────────────────────────────
    display.fill_rect(x, y, ww.saturating_sub(8), 1, framebuffer::Color::from_hex(0x1A1A30));
    y += 4;
    display.draw_text(x, y, "PCI", acc, 1);
    y += 12;
    let devs = PCI_DEVS.lock();
    for d in devs.iter() {
        if y + 10 > wy + wh { break; }
        let line = alloc::format!("{:02X}:{:02X}.{} {:04X}:{:04X} {}",
            d.bus, d.dev, d.func, d.vendor_id, d.device_id,
            pci::class_name(d.class, d.subclass));
        // Truncate to fit window
        let max_chars = (ww.saturating_sub(10)) / 9;
        let trimmed = if line.len() > max_chars { &line[..max_chars] } else { &line };
        display.draw_text(x, y, trimmed, dim, 1);
        y += 11;
    }
    if devs.is_empty() {
        display.draw_text(x, y, "(none)", dim, 1);
    }
}

fn fmt_hms<'a>(h: u64, m: u64, s: u64, buf: &'a mut [u8; 32]) -> &'a str {
    let digits = |n: u64, buf: &mut [u8], off: usize| {
        buf[off]     = b'0' + (n / 10) as u8;
        buf[off + 1] = b'0' + (n % 10) as u8;
    };
    digits(h, buf, 0); buf[2] = b':';
    digits(m, buf, 3); buf[5] = b':';
    digits(s, buf, 6);
    core::str::from_utf8(&buf[..8]).unwrap_or("00:00:00")
}

fn render_welcome_window(display: &mut framebuffer::Display) {
    let Some((wx, wy, ww, wh)) = window_rect("Welcome to HepOS") else { return; };
    let bg   = framebuffer::Color::from_hex(0x0C0C0C);
    let acc  = framebuffer::Color::from_hex(0x6C8EFF);
    let text = framebuffer::Color::from_hex(0xE8E8E8);
    let dim  = framebuffer::Color::from_hex(0x888888);
    let ok   = framebuffer::Color::from_hex(0x6BFF8E);

    display.fill_rect(wx, wy, ww, wh, bg);
    display.fill_rect(wx, wy, ww, 2, acc);

    let mut y = wy + 6;
    display.draw_text(wx + 4, y, "HepOS v0.1", acc, 1);   y += 16;
    display.draw_text(wx + 4, y, "x86_64 exokernel", dim, 1); y += 14;

    let free_mb  = pmm::free_pages() * 4 / 1024;
    let total_mb = pmm::total_pages() * 4 / 1024;
    let mut buf = [0u8; 64];
    let s = fmt_mem(free_mb, total_mb, &mut buf);
    display.draw_text(wx + 4, y, s, text, 1); y += 14;

    let has_nvme = nvme::CONTROLLER.lock().is_some();
    display.draw_text(wx + 4, y, if has_nvme { "NVMe: OK" } else { "NVMe: --" },
        if has_nvme { ok } else { dim }, 1); y += 14;

    display.draw_text(wx + 4, y, "HepFS: OK", ok, 1);
    let _ = (y, ww, wh);
}

fn fmt_mem<'a>(free_mb: u64, total_mb: u64, buf: &'a mut [u8; 64]) -> &'a str {
    let mut pos = 0usize;
    for b in b"RAM: "       { if pos < 64 { buf[pos] = *b; pos += 1; } }
    write_u64(free_mb,  &mut pos, buf);
    for b in b" MB free / " { if pos < 64 { buf[pos] = *b; pos += 1; } }
    write_u64(total_mb, &mut pos, buf);
    for b in b" MB total"   { if pos < 64 { buf[pos] = *b; pos += 1; } }
    core::str::from_utf8(&buf[..pos]).unwrap_or("")
}

fn write_u64(mut n: u64, pos: &mut usize, buf: &mut [u8; 64]) {
    if n == 0 { if *pos < 64 { buf[*pos] = b'0'; *pos += 1; } return; }
    let start = *pos;
    while n > 0 {
        if *pos < 64 { buf[*pos] = b'0' + (n % 10) as u8; *pos += 1; }
        n /= 10;
    }
    buf[start..*pos].reverse();
}
