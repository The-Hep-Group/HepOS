#![no_std]
#![no_main]
#![feature(stmt_expr_attributes)]
extern crate alloc;

mod acpi;
mod ahci;
mod apic;
mod audio;
mod bootinfo;
mod clipboard;
mod editor;
mod e1000;
mod net;
mod rtc;
mod rtl8139;
mod virtio_net;
mod virtio_gpu;
mod xhci;
mod desktop;
mod framebuffer;
mod gdt;
mod hda;
mod heap;
mod hepfs;
mod icons;
mod idt;
mod image;
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
use spin::Mutex;

// Global display — used by exception handler and future modules
pub static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

/// Physical address of the ACPI RSDP, as found by HepBL (0 if not found) —
/// set once early in `kmain` from `BootInfo::acpi_rsdp`, read by
/// `acpi::shutdown()` to attempt a real ACPI shutdown before falling back to
/// the hardcoded QEMU/Bochs/VirtualBox ports.
pub static BOOTINFO_ACPI_RSDP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Focus: Some(id) = that window has keyboard focus; defaults to terminal (id=2)
pub static FOCUSED_WIN: Mutex<Option<usize>> = Mutex::new(None);

/// Window ID currently being drag-selected in an editor (mouse button held
/// since the click landed in that editor's content area). `None` when no
/// drag-selection is in progress.
static TEXT_DRAG_WIN: Mutex<Option<usize>> = Mutex::new(None);

/// True while the Settings window's volume slider is being dragged (mouse
/// button held since a fresh click landed on the slider) — lets the drag
/// keep scrubbing volume even if the cursor moves off the slider's y-range.
static VOLUME_DRAG: Mutex<bool> = Mutex::new(false);

/// A file/folder row's drag-and-drop between (or within) Files windows —
/// AND the row's own click action (open/navigate/rename), deferred here from
/// mousedown to release exactly like `desktop.rs`'s taskbar buttons: arming
/// a drag must NOT also fire the row's click action, or holding down to drag
/// a row would navigate into it / open it / start a rename before the drag
/// even got anywhere. `pending_action` is fixed at mousedown time (it depends
/// on the click's own timing, not on when it's resolved); it only actually
/// runs on release, and only if the row was never dragged.
struct FileDrag {
    from_parent: u32,
    /// Path of `from_parent`, captured at drag-arm time (HepFS inodes have
    /// no reverse pointer, so this can't be reconstructed later) — needed
    /// only if the drop lands on the pin dock, which stores pinned items by
    /// path rather than just ino for the same reason (see `PINNED_FILES`).
    from_path: alloc::string::String,
    ino:  u32,
    name: alloc::string::String,
    start_x: i32,
    start_y: i32,
    moved: bool,
    win_id: usize,
    is_dir: bool,
    pending_action: FileRowAction,
    /// Every *other* row that was part of the same multi-selection when the
    /// drag started (`HepfsNav::range_selected`) — empty if the dragged row
    /// wasn't part of one. Dropping moves the primary row *and* all of these
    /// together, same "drag any selected item moves the whole group" model
    /// the desktop's own icon multi-select uses.
    extra: alloc::vec::Vec<(u32, alloc::string::String, bool)>,
}
#[derive(Clone, Copy, PartialEq)]
enum FileRowAction { None, Open, Rename }
static FILE_DRAG: Mutex<Option<FileDrag>> = Mutex::new(None);

/// Armed on mousedown over a Files window's icon-grid scrollbar track —
/// `(win_id, list_top (screen y), list_h, max_scroll (rows))` — and read
/// every held frame after to convert the cursor's y position into a row
/// offset, same direct-manipulation-track model the Settings volume slider
/// already uses (`VOLUME_DRAG`), just living in `main.rs` instead of
/// `desktop.rs` since it needs `hepfs::list_dir()` to know the content's
/// row count (`desktop.rs` doesn't touch the block device).
static FS_SCROLL_DRAG: Mutex<Option<(usize, i32, usize, usize)>> = Mutex::new(None);

/// (window id, row, col, TSC timestamp) of the last fresh click in an
/// editor/terminal content area — used to detect a double-click (same cell,
/// within ~400ms) so it can select the whole line instead of just placing
/// the cursor. Timed via TSC rather than `scheduler::TICK_COUNT`: the APIC
/// timer only actually fires once in this build (see PLAN.md Known Issues —
/// TICK_COUNT freezes right after the kmain->task_blink bootstrap switch),
/// so anything gated on it would never expire.
static LAST_CLICK: Mutex<Option<(usize, usize, usize, u64)>> = Mutex::new(None);

/// Consumes/updates `LAST_CLICK` and reports whether this click is a double-click
/// on the same (window, row, col) within the double-click time window.
fn is_double_click(win_id: usize, row: usize, col: usize) -> bool {
    let now = hda::rdtsc();
    let tsc_per_ms = hda::TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed);
    let window = tsc_per_ms.saturating_mul(400);
    let mut last = LAST_CLICK.lock();
    let double = matches!(*last, Some((w, r, c, t))
        if w == win_id && r == row && c.abs_diff(col) <= 1 && now.wrapping_sub(t) <= window);
    if double {
        *last = None; // consumed — the next click starts a fresh pair
    } else {
        *last = Some((win_id, row, col, now));
    }
    double
}
pub static PCI_DEVS: Mutex<alloc::vec::Vec<pci::PciDevice>> = Mutex::new(alloc::vec::Vec::new());

// Frame counter for uptime (~60 fps → divide by 60 for seconds)
static UPTIME_FRAMES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// HepFS navigator state (current directory + back/forward history). One
// shared nav per Files window — both panes (see render_hepfs_window) show
// this same current directory, just filtered differently: a directories-only
// list on the left (a lightweight one-level "tree"), the full listing
// (dirs + files) on the right. Not two independent browsers.
struct HepfsNav {
    ino:  u32,
    path: alloc::string::String,
    back: alloc::vec::Vec<(u32, alloc::string::String)>,
    fwd:  alloc::vec::Vec<(u32, alloc::string::String)>,
    /// Selected row, as (pane, row_index) — pane 3 = directory tree, pane 4 =
    /// full listing. Click semantics on the selected row mirror the desktop's
    /// icons: a fast second click opens/navigates, a slower one renames (see
    /// `desktop::ICON_DBLCLICK_TICKS`/`ICON_RENAME_TICKS`, reused as-is so both
    /// feel identical).
    selected:    Option<(u8, usize)>,
    selected_at: u64,
    /// Shift+click range-select: every row index (within `selected`'s pane)
    /// highlighted between the anchor (`selected`) and the shift-clicked row.
    /// Highlight-only — doesn't change what open/rename/drag act on (that's
    /// still just `selected`); file-manager multi-*move* isn't implemented,
    /// unlike the desktop's Shift+click/marquee (see PLAN.md).
    range_selected: alloc::vec::Vec<usize>,
    /// Vertical scroll offset, in whole *grid rows* (not pixels — keeps every
    /// visible row fully drawn, no partial-row clipping math needed) for the
    /// right pane's icon grid. The left tree pane doesn't scroll (it's
    /// always short: one-level directory names only). Reset to 0 on every
    /// navigation (see the `..`/back/forward/into-directory sites) so a new
    /// directory never opens already scrolled from wherever the previous
    /// one was left.
    scroll: usize,
}

// One entry per Files window, keyed by window id — same pattern as
// `editor::EXTRA_EDITORS` — so multiple Files windows browse independently.
static HEPFS_NAVS: Mutex<alloc::vec::Vec<(usize, HepfsNav)>> = Mutex::new(alloc::vec::Vec::new());

fn hepfs_nav_new() -> HepfsNav {
    HepfsNav {
        ino:  hepfs::ROOT_INO,
        path: alloc::string::String::from("/"),
        back: alloc::vec::Vec::new(),
        fwd:  alloc::vec::Vec::new(),
        selected: None, selected_at: 0, range_selected: alloc::vec::Vec::new(), scroll: 0,
    }
}

/// Spawn a brand-new Files window with its own independent navigator (starts at root).
fn spawn_files() -> usize {
    let win_id = {
        let mut dt = desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            let id = dt.add_window(desktop::AppKind::Files, "HepFS", 160, 90, 260, 160);
            dt.dirty = true;
            id
        } else {
            return usize::MAX;
        }
    };
    HEPFS_NAVS.lock().push((win_id, hepfs_nav_new()));
    win_id
}

/// Resolve `nav.selected` (pane, row) back into (parent_ino, ino, name,
/// is_dir) — same traversal the click handler uses to turn a click into an
/// entry, just driven by the *last selected* row instead of a live click.
fn selected_fs_entry(win_id: usize) -> Option<(u32, u32, alloc::string::String, bool)> {
    let (cur_ino, pane, idx) = {
        let navs = HEPFS_NAVS.lock();
        let (_, nav) = navs.iter().find(|(id, _)| *id == win_id)?;
        let (pane, idx) = nav.selected?;
        (nav.ino, pane, idx)
    };
    let (ino, name, is_dir) = resolve_fs_row(win_id, pane, idx)?;
    Some((cur_ino, ino, name, is_dir))
}

/// Resolve a specific (pane, row_idx) in `win_id`'s currently-browsed
/// directory into (ino, name, is_dir) — the row-lookup half of
/// `selected_fs_entry`, factored out so multi-selection resolution
/// (`selected_fs_entries`) can reuse it per-row. Returns `None` for the
/// ".." row or an out-of-range index.
fn resolve_fs_row(win_id: usize, pane: u8, idx: usize) -> Option<(u32, alloc::string::String, bool)> {
    let cur_ino = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == win_id).map(|(_, n)| n.ino)?;
    let at_root = cur_ino == hepfs::ROOT_INO;
    if !at_root && idx == 0 { return None; } // the ".." row isn't a real entry
    let real_idx = if !at_root { idx - 1 } else { idx };

    let mut ctrl = nvme::CONTROLLER.lock();
    let ctrl = ctrl.as_mut()?;
    let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
    let entries = hepfs::list_dir(ctrl, cur_ino);
    if pane == 3 {
        let (ino, name) = entries.iter()
            .filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
            .nth(real_idx)?;
        Some((*ino, name.clone(), true))
    } else {
        let (ino, name) = entries.get(real_idx)?;
        let is_dir = hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR;
        Some((*ino, name.clone(), is_dir))
    }
}

/// Every entry the "current selection" covers — the whole multi-selection
/// (`HepfsNav::range_selected`) if there is one (more than one row), else
/// just the single `selected` row. Used anywhere an action (Copy, drag)
/// should act on the whole group when one exists, not just whichever row
/// happened to be clicked/dragged.
fn selected_fs_entries(win_id: usize) -> alloc::vec::Vec<(u32, u32, alloc::string::String, bool)> {
    let (cur_ino, pane, sel_idx, range) = {
        let navs = HEPFS_NAVS.lock();
        let Some((_, nav)) = navs.iter().find(|(id, _)| *id == win_id) else { return alloc::vec::Vec::new() };
        let Some((pane, idx)) = nav.selected else { return alloc::vec::Vec::new() };
        (nav.ino, pane, idx, nav.range_selected.clone())
    };
    let rows: alloc::vec::Vec<usize> = if range.len() > 1 { range } else { alloc::vec![sel_idx] };
    rows.into_iter()
        .filter_map(|idx| resolve_fs_row(win_id, pane, idx).map(|(ino, name, is_dir)| (cur_ino, ino, name, is_dir)))
        .collect()
}

/// Lists `/home/desktop` and pushes the result into the desktop's icon set
/// (`Desktop::sync_fs_icons`). No-op if HepFS isn't mounted yet or
/// `/home/desktop` doesn't exist (shouldn't happen post-boot, but this runs
/// from the same early-boot code that creates it, so it's checked). Called
/// once after HepFS init and again after any op that changes that directory
/// (new file/folder, rename) — see the `prompt_result` handling below.
pub(crate) fn refresh_desktop_icons() {
    let entries = {
        let mut ctrl = nvme::CONTROLLER.lock();
        let Some(ctrl) = ctrl.as_mut() else { return };
        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
        let Some(dir_ino) = hepfs::lookup(ctrl, "/home/desktop") else { return };
        hepfs::list_dir(ctrl, dir_ino).into_iter()
            .map(|(ino, name)| { let is_dir = hepfs::stat(ctrl, ino).0; (ino, name, is_dir) })
            .collect::<alloc::vec::Vec<_>>()
    };
    let mut dt = desktop::DESKTOP.lock();
    if let Some(dt) = dt.as_mut() {
        dt.sync_fs_icons(&entries);
    }
}

/// The id of whichever window is actually topmost at (mx, my) — chrome
/// (close/maximize/minimize/newterm/title bar/resize edge) or content, same
/// predicate `Desktop::update_mouse()`'s own hit-testing uses, checked
/// topmost-first (`.rev()`, since z-order is back-to-front in `windows`).
///
/// Per-app click routing below (HepFS, Settings, editor/terminal text-drag)
/// used to hit-test only *their own* window kind, ignoring whether some
/// other window was actually stacked on top at that point — so e.g. a Files
/// window's content area is quite large, and clicking the close button of an
/// unrelated window that happened to visually overlap it would both close
/// that window (correctly, via Desktop::update_mouse()) *and* leak through
/// as a stray Files-window navigation click. Each such block now checks its
/// candidate window's id against this before acting.
fn topmost_window_id_at(d: &desktop::Desktop, mx: i32, my: i32) -> Option<usize> {
    d.windows.iter().rev().find(|w| {
        !w.minimized && (
            w.close_hit(mx, my) || w.maximize_hit(mx, my) || w.minimize_hit(mx, my)
                || w.newterm_hit(mx, my) || w.title_hit(mx, my)
                || w.resize_hit(mx, my) || w.content_hit(mx, my)
        )
    }).map(|w| w.id)
}

/// Resolves a completed file/folder drag: finds the topmost Files window
/// under (mx, my) and moves the dragged entry into whichever directory it
/// should land in — if the drop point is over a *specific directory row* in
/// that window's list, into that subdirectory (the natural first thing
/// anyone would try — drag a file onto a folder); otherwise into whatever
/// directory the window is currently browsing. Returns true if a valid drop
/// target was found and a move was attempted. Factored out of the main loop
/// so it's directly testable.
fn resolve_file_drop(fd: &FileDrag, mx: i32, my: i32) -> bool {
    let win_rect = {
        let dt = desktop::DESKTOP.lock();
        dt.as_ref().and_then(|d| {
            let win = d.windows.iter().rev().find(|w| {
                w.app_kind == desktop::AppKind::Files && !w.minimized
                    && mx >= w.x && mx < w.x + w.w as i32
                    && my >= w.y && my < w.y + w.h as i32
                    && topmost_window_id_at(d, mx, my) == Some(w.id)
            })?;
            Some((win.id, win.x, win.y, win.w))
        })
    };
    let Some((target_win, wx, wy, ww)) = win_rect else {
        return resolve_file_drop_outside_windows(fd, mx, my);
    };
    let Some(browsing_ino) = HEPFS_NAVS.lock().iter()
        .find(|(id, _)| *id == target_win).map(|(_, n)| n.ino) else { return false };

    // Did the drop land on a specific directory row (or the ".." row)? Same
    // nav-bar/pane/row geometry the click handler itself uses.
    let rel_x = (mx - wx) as usize;
    let rel_y = my - wy;
    let at_root = browsing_ino == hepfs::ROOT_INO;
    let left_w = (ww * HEPFS_TREE_W_NUM) / HEPFS_TREE_W_DEN;
    let on_tree_pane = rel_x <= left_w;
    let target_ino = if rel_y >= 22 {
        let entry_idx = if on_tree_pane {
            ((rel_y - 22) / 14).max(0) as usize
        } else {
            let right_w = ww.saturating_sub(left_w + 1);
            let scroll = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == target_win)
                .map(|(_, n)| n.scroll).unwrap_or(0);
            grid_idx_at(rel_x as i32 - left_w as i32 - 1, rel_y - 22, right_w, scroll)
        };
        if !at_root && entry_idx == 0 {
            // Dropped on the ".." row — move up into the parent directory,
            // same one clicking ".." itself would navigate to.
            HEPFS_NAVS.lock().iter().find(|(id, _)| *id == target_win)
                .and_then(|(_, n)| n.back.last().map(|(ino, _)| *ino))
                .unwrap_or(browsing_ino)
        } else {
            let real_idx = if !at_root { entry_idx - 1 } else { entry_idx };
            let mut ctrl = nvme::CONTROLLER.lock();
            ctrl.as_mut().and_then(|ctrl| {
                let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
                let entries = hepfs::list_dir(ctrl, browsing_ino);
                let row_ino = if on_tree_pane {
                    entries.iter().filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
                        .nth(real_idx).map(|(ino, _)| *ino)
                } else {
                    entries.get(real_idx).filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
                        .map(|(ino, _)| *ino)
                };
                // Dropping a directory onto itself isn't a real move.
                row_ino.filter(|&ino| ino != fd.ino)
            }).unwrap_or(browsing_ino)
        }
    } else {
        browsing_ino
    };

    let mut ctrl = nvme::CONTROLLER.lock();
    if let Some(ctrl) = ctrl.as_mut() {
        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
        // Move the dragged row, plus every other row that was part of the
        // same multi-selection when the drag started (see FileDrag::extra).
        hepfs::move_entry(ctrl, fd.from_parent, target_ino, &fd.name);
        for (_, name, _) in &fd.extra {
            hepfs::move_entry(ctrl, fd.from_parent, target_ino, name);
        }
    }
    drop(ctrl);
    refresh_desktop_icons();
    let mut dt = desktop::DESKTOP.lock();
    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
    true
}

/// Resolves a file/folder drag dropped somewhere that ISN'T a Files window —
/// called by `resolve_file_drop()` once it's ruled that out. Two valid
/// targets: the left pin dock (pins the dragged entry/entries in place,
/// without moving them — same as the right-click "Pin" menu item) or open
/// desktop background (moves them into `/home/desktop`, same as dragging a
/// file onto the desktop in a conventional OS). Dropping on the taskbar or
/// any other window is a no-op.
fn resolve_file_drop_outside_windows(fd: &FileDrag, mx: i32, my: i32) -> bool {
    let (on_dock, on_window, in_taskbar) = {
        let dt = desktop::DESKTOP.lock();
        let Some(d) = dt.as_ref() else { return false };
        let on_dock = desktop::in_pin_dock(mx, my, d.fb_h);
        let in_taskbar = my >= d.fb_h as i32 - desktop::TASKBAR_H as i32;
        let on_window = d.windows.iter().rev().any(|w| {
            !w.minimized && (w.close_hit(mx, my) || w.maximize_hit(mx, my) || w.minimize_hit(mx, my)
                || w.newterm_hit(mx, my) || w.title_hit(mx, my)
                || w.resize_hit(mx, my) || w.content_hit(mx, my))
        });
        (on_dock, on_window, in_taskbar)
    };

    if on_dock {
        desktop::pin_file(fd.ino, fd.from_path.clone(), fd.name.clone(), fd.is_dir);
        for (ino, name, is_dir) in &fd.extra {
            desktop::pin_file(*ino, fd.from_path.clone(), name.clone(), *is_dir);
        }
        if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
        return true;
    }

    if on_window || in_taskbar {
        return false;
    }

    // Open desktop background — move into /home/desktop, same directory
    // desktop icons already mirror (see `sync_fs_icons()`).
    let mut ctrl = nvme::CONTROLLER.lock();
    if let Some(ctrl) = ctrl.as_mut() {
        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
        if let Some(desktop_ino) = hepfs::lookup(ctrl, "/home/desktop") {
            if desktop_ino != fd.from_parent {
                hepfs::move_entry(ctrl, fd.from_parent, desktop_ino, &fd.name);
                for (_, name, _) in &fd.extra {
                    hepfs::move_entry(ctrl, fd.from_parent, desktop_ino, name);
                }
            }
        }
    }
    drop(ctrl);
    refresh_desktop_icons();
    if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
    true
}

/// Floating drag ghost for an in-progress file/folder drag — the file
/// manager equivalent of the taskbar's dragged-button ghost (`desktop.rs`'s
/// `dragged_ghost`). Before this, a file drag gave no visual feedback at all
/// until you dropped it; the row didn't even look picked up. Draws the same
/// icon+label a Files window row would, floating near the cursor, whenever
/// `FILE_DRAG` is armed and has actually moved (not just clicked).
fn draw_file_drag_ghost(display: &mut framebuffer::Display, mx: i32, my: i32) {
    let Some(fd) = FILE_DRAG.lock().as_ref()
        .map(|fd| (fd.moved, fd.is_dir, fd.name.clone(), fd.extra.len())) else { return };
    let (moved, is_dir, name, extra_count) = fd;
    if !moved { return; }

    // Dragging a multi-selection shows "N items" instead of a specific name
    // (and a generic folder-ish icon, since the group can mix files/dirs).
    let multi = extra_count > 0;
    let label = if multi {
        alloc::format!("{} items", extra_count + 1)
    } else if name.len() > 20 {
        alloc::format!("{}…", &name[..20])
    } else {
        name.clone()
    };
    let w = label.len() * 9 + 22;
    let h = 18usize;
    let gx = (mx + 12).max(0) as usize;
    let gy = (my + 12).max(0) as usize;
    display.fill_rect(gx + 2, gy + 2, w, h, framebuffer::Color::from_hex(0x000000));
    display.fill_rect(gx, gy, w, h, framebuffer::Color::from_hex(0x1E1E40));
    display.fill_rect(gx, gy, w, 1, framebuffer::Color::from_hex(0x6C8EFF));
    icons::draw_file_icon(display, gx + 3, gy + 3, 12, is_dir || multi, &name);
    display.draw_text(gx + 20, gy + 5, &label, framebuffer::Color::from_hex(0xE8E8E8), 1);
}

#[no_mangle]
extern "C" fn kmain(bi_ptr: *const bootinfo::BootInfo) -> ! {
    serial::init();
    serial::print("HepOS kmain (HepBL boot)\n");

    let bi = unsafe { &*bi_ptr };
    if bi.magic != bootinfo::BOOTINFO_MAGIC {
        serial::print("FATAL: bad BootInfo magic\n");
        loop { unsafe { core::arch::asm!("hlt"); } }
    }
    BOOTINFO_ACPI_RSDP.store(bi.acpi_rsdp, core::sync::atomic::Ordering::Relaxed);

    gdt::init();
    serial::print("GDT loaded\n");

    idt::init();
    serial::print("IDT loaded\n");

    let hhdm = bi.hhdm_offset;
    vmm::init(hhdm);
    pmm::init(&bi.memmap[..bi.memmap_count as usize]);
    serial::print("PMM init\n");

    // Drop HepBL's transitional identity map (PML4[0]) — nothing in the kernel
    // uses low-half virtual addresses, and user PML4s must not inherit it.
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        (vmm::phys_to_virt(cr3 & !0xFFF) as *mut u64).write_volatile(0);
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nomem, nostack)); // TLB flush
    }

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

    *DISPLAY.lock() = Some(Display::new(bi));

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
        let w = bi.fb_width as usize;
        let h = bi.fb_height as usize;
        let mut dt = desktop::Desktop::new(w, h);
        // Window positions chosen to fit common resolutions (640×480 min)
        use desktop::AppKind;
        dt.add_window(AppKind::Welcome, "Welcome to HepOS", 20,  50,  300, 160);
        dt.add_window(AppKind::Files,   "HepFS",            340, 50,  260, 160);
        dt.add_window(AppKind::Terminal,"Terminal",         20,  240, 580, 200);
        dt.set_terminal_window(2);
        // Editor window (id=3) — hidden until `edit` command opens a file
        dt.add_window(AppKind::Editor, "Editor",           60,  40,  580, 380);
        // Sysmon window (id=4) — hidden until opened from start menu
        dt.add_window(AppKind::Sysmon, "Sysmon",           80,  60,  340, 260);
        // Settings window (id=5) — hidden until opened from icon or right-click menu
        dt.add_window(AppKind::Settings, "Settings",         120, 80,  480, 320);
        // Image Viewer window (id=6) — hidden until a .bmp is opened
        dt.add_window(AppKind::ImageViewer, "Image Viewer",     100, 60,  420, 340);
        // Audio Player window (id=7) — hidden until a .wav is played
        dt.add_window(AppKind::AudioPlayer, "Audio Player",     140, 100, 380, 160);
        *desktop::DESKTOP.lock() = Some(dt);
    }

    // Init terminal NOW before sti so task_blink sees it immediately
    terminal::init();
    serial::print("Terminal init\n");

    // Main Files window (id=1) — both panes start at root
    HEPFS_NAVS.lock().push((1, hepfs_nav_new()));

    // Minimize editor, sysmon, settings, and image viewer until explicitly opened; focus terminal (id=2)
    {
        let mut dt = desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            for id in [3usize, 4, 5, 6, 7] {
                if let Some(w) = dt.windows.iter_mut().find(|w| w.id == id) {
                    w.hide_instant(); // starting hidden — never shown, nothing to animate
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
        let mut dev = hepfs::BlockDev::Nvme(&mut ctrl);
        if !hepfs::probe(&mut dev) {
            serial::print("Formatting HepFS...\n");
            hepfs::format(&mut dev);
            serial::print("HepFS formatted\n");
        } else {
            serial::print("HepFS found\n");
        }

        // Smoke test: create dirs + file, write, read back
        hepfs::create_dir(&mut dev, hepfs::ROOT_INO, "home");
        hepfs::create_dir(&mut dev, hepfs::ROOT_INO, "etc");
        let home = hepfs::lookup(&mut dev, "/home").unwrap();
        // Desktop icons for real files live under /home/desktop (see desktop.rs's
        // fs_icons()) — created once, guarded by lookup() since create_dir() has
        // no built-in dedup and this whole block reruns every boot.
        if hepfs::lookup(&mut dev, "/home/desktop").is_none() {
            hepfs::create_dir(&mut dev, home, "desktop");
        }
        let fno  = hepfs::create_file(&mut dev, home, "hello.txt");
        hepfs::write_file(&mut dev, fno, b"Hello from HepOS!\n");
        let data = hepfs::read_file(&mut dev, fno);
        let s    = core::str::from_utf8(&data).unwrap_or("?");
        serial::print("Read back: ");
        serial::print(s);

        let entries = hepfs::list_dir(&mut dev, hepfs::ROOT_INO);
        serial::print("/ contents:\n");
        for (_, name) in &entries { serial::print("  "); serial::print(name); serial::print("\n"); }

        // Store controller globally so apps can use it
        *nvme::CONTROLLER.lock() = Some(ctrl);

        // Write kernel manifest to HepFS (skipped if already exists)
        {
            let mut c = nvme::CONTROLLER.lock();
            if let Some(ctrl) = c.as_mut() {
                let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
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
                // Demo BMP so `view /demo.bmp` has something to show out of the box
                if hepfs::lookup(ctrl, "/demo.bmp").is_none() {
                    let ino = hepfs::create_file(ctrl, hepfs::ROOT_INO, "demo.bmp");
                    hepfs::write_file(ctrl, ino, &make_demo_bmp());
                }
                // Demo WAV so `play /demo.wav` has something to play out of the box
                if hepfs::lookup(ctrl, "/demo.wav").is_none() {
                    let ino = hepfs::create_file(ctrl, hepfs::ROOT_INO, "demo.wav");
                    hepfs::write_file(ctrl, ino, &make_demo_wav());
                }
            }
        }

        // Populate desktop icons for whatever's already in /home/desktop.
        refresh_desktop_icons();

    } else {
        serial::print("No NVMe device found\n");
    }

    // AHCI/SATA — not yet mounted by HepFS (NVMe-only today), but the driver
    // itself (detect, port init, IDENTIFY, read/write) is up and usable.
    ahci::init(&pci_devices);

    // virtio-gpu — runs alongside the real GOP boot display, not instead of
    // it (HepBL's GOP framebuffer stays the safety net if virtio-gpu isn't
    // present or the pixel format doesn't match). When available, mirror the
    // exact same backbuffer memory to it — zero-copy, no synthetic test
    // pattern — so the real desktop actually renders through it live.
    if virtio_gpu::init(&pci_devices) {
        let mirrored = DISPLAY.lock().as_ref()
            .and_then(|d| d.backbuf_info())
            .map(|(phys, w, h, is_bgrx8888)| {
                is_bgrx8888 && virtio_gpu::mirror_display(w as u32, h as u32, phys)
            })
            .unwrap_or(false);
        serial::print(if mirrored { "virtio-gpu: mirroring real backbuffer\n" }
                      else { "virtio-gpu: backbuffer format unsupported, not mirroring\n" });
    }

    // Intel HDA audio
    hda::init(&pci_devices);

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

/// TEMPORARY boot-time self-test for the async ping job — exercised via
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

            // Desktop "New File"/"New Folder"/rename text prompt takes priority
            // over everything else while it's open.
            {
                let mut dt = desktop::DESKTOP.lock();
                if let Some(dt) = dt.as_mut() {
                    if dt.text_prompt.is_some() {
                        dt.prompt_on_key(c);
                        continue;
                    }
                }
            }

            let focused = *FOCUSED_WIN.lock();

            if focused == Some(3) {
                // Main Editor window has focus — route all keys
                let mut eg = editor::EDITOR.lock();
                if let Some(ed) = eg.as_mut() {
                    ed.on_key(c);
                    if !ed.open {
                        drop(eg);
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 3) {
                                w.close();
                            }
                            dt.dirty = true;
                        }
                        *FOCUSED_WIN.lock() = Some(2);
                    }
                }
            } else {
                let focused = *FOCUSED_WIN.lock();
                // Route to a focused extra editor window, if one is focused
                let routed_editor = {
                    let (matched, should_close, target_wid) = {
                        let mut ee = editor::EXTRA_EDITORS.lock();
                        match ee.iter_mut().find(|(wid, _)| Some(*wid) == focused) {
                            Some((wid, ed)) => {
                                let wid = *wid;
                                ed.on_key(c);
                                (true, !ed.open, wid)
                            }
                            None => (false, false, 0),
                        }
                    }; // extra-editors lock released here
                    if matched && should_close {
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == target_wid) {
                                w.close();
                            }
                            dt.dirty = true;
                        }
                        *FOCUSED_WIN.lock() = Some(2);
                    }
                    matched
                };
                // Route to the focused extra terminal if one is focused
                let routed_extra = routed_editor || {
                    let mut et = terminal::EXTRA_TERMINALS.lock();
                    if let Some((_, t)) = et.iter_mut().find(|(wid, _)| Some(*wid) == focused) {
                        t.on_key(c);
                        true
                    } else { false }
                };
                // Ctrl+C/Ctrl+V in a focused Files window copies/pastes
                // whichever row is selected (files-and-directories clipboard —
                // see desktop::FS_CLIPBOARD; separate from the plain-text
                // clipboard editor/terminal Ctrl+C/V uses).
                let routed_files = routed_extra || {
                    let is_files_win = focused.map(|f| HEPFS_NAVS.lock().iter().any(|(id, _)| *id == f)).unwrap_or(false);
                    if is_files_win {
                        let win_id = focused.unwrap();
                        // Plain Ctrl+C on a lowercase 'c' is turned into the
                        // control code 0x03 by ps2.rs's own ctrl-modifier
                        // handling *before* it ever gets here — it never
                        // arrives as the literal char 'C'. That's why this
                        // silently did nothing before: only Ctrl+Shift+C
                        // (shift suppresses that conversion) was handled.
                        // Files windows have no competing meaning for plain
                        // Ctrl+C (unlike the terminal's Ctrl+C=cancel), so
                        // treat both as Copy here.
                        if c as u8 == 0x03 || (c == 'C' && ps2::ctrl_held()) {
                            // Copies the whole multi-selection if the selected
                            // row is part of one, else just that one row.
                            *desktop::FS_CLIPBOARD.lock() = selected_fs_entries(win_id);
                            true
                        } else if c as u8 == 0x16 || (c == 'V' && ps2::ctrl_held()) {
                            let clip = desktop::FS_CLIPBOARD.lock().clone();
                            if !clip.is_empty() {
                                let target_ino = HEPFS_NAVS.lock().iter()
                                    .find(|(id, _)| *id == win_id).map(|(_, n)| n.ino);
                                if let Some(target_ino) = target_ino {
                                    let mut ctrl = nvme::CONTROLLER.lock();
                                    if let Some(ctrl) = ctrl.as_mut() {
                                        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
                                        for (from_parent, _ino, name, _is_dir) in &clip {
                                            hepfs::copy_entry_unique(ctrl, *from_parent, target_ino, name);
                                        }
                                    }
                                    drop(ctrl);
                                    refresh_desktop_icons();
                                    let mut dt = desktop::DESKTOP.lock();
                                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                                }
                            }
                            true
                        } else { false }
                    } else { false }
                };
                // Only the main terminal (window id 2) — NOT whatever's
                // actually focused, e.g. Settings/Sysmon/Welcome/an
                // ImageViewer/AudioPlayer window — used to be a real bug:
                // every key that didn't match the editor/extra-terminal
                // cases above fell through here unconditionally, so e.g.
                // Ctrl+C typed while some other window had focus still
                // cancelled the main terminal's input line, even though it
                // wasn't the window the user was looking at.
                if !routed_files && focused == Some(2) {
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
        let spawn_terminal = {
            let mut dt_guard = desktop::DESKTOP.lock();
            dt_guard.as_mut().map(|dt| dt.update_mouse(mx, my, btn)).unwrap_or(false)
        };
        if spawn_terminal { crate::terminal::spawn_terminal(); }

        // Text selection: drag-to-select in editor/terminal windows. `fresh_click`
        // anchors the selection (mouse_down); subsequent frames with the
        // button still held extend it (mouse_drag) against the same window
        // even if the cursor later leaves its bounds.
        {
            let btn_held = btn & 1 != 0;
            if !btn_held {
                *TEXT_DRAG_WIN.lock() = None;
            } else {
                let win_info: Option<(usize, desktop::AppKind, i32, i32, usize, usize)> = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().and_then(|d| {
                        if fresh_click {
                            d.windows.iter().rev()
                                .find(|w| !w.minimized
                                    && matches!(w.app_kind, desktop::AppKind::Editor | desktop::AppKind::Terminal)
                                    && w.content_hit(mx, my)
                                    && topmost_window_id_at(d, mx, my) == Some(w.id))
                                .map(|w| (w.id, w.app_kind, w.x, w.y, w.w, w.h))
                        } else {
                            let wid = *TEXT_DRAG_WIN.lock();
                            wid.and_then(|id| d.windows.iter().find(|w| w.id == id)
                                .map(|w| (w.id, w.app_kind, w.x, w.y, w.w, w.h)))
                        }
                    })
                };
                if let Some((id, kind, wwx, wwy, www, wwh)) = win_info {
                    if fresh_click { *TEXT_DRAG_WIN.lock() = Some(id); }
                    let wxu = wwx.max(0) as usize;
                    let wyu = wwy.max(0) as usize;
                    let hit = match kind {
                        desktop::AppKind::Editor => {
                            if id == 3 {
                                let mut eg = editor::EDITOR.lock();
                                eg.as_mut().and_then(|ed| {
                                    let h = ed.hit_test(wxu, wyu, www, wwh, mx, my);
                                    if let Some((row, col)) = h {
                                        if fresh_click {
                                            if is_double_click(id, row, col) { ed.select_line(row); } else { ed.mouse_down(row, col); }
                                        } else { ed.mouse_drag(row, col); }
                                    }
                                    h
                                })
                            } else {
                                let mut ee = editor::EXTRA_EDITORS.lock();
                                ee.iter_mut().find(|(wid, _)| *wid == id).and_then(|(_, ed)| {
                                    let h = ed.hit_test(wxu, wyu, www, wwh, mx, my);
                                    if let Some((row, col)) = h {
                                        if fresh_click {
                                            if is_double_click(id, row, col) { ed.select_line(row); } else { ed.mouse_down(row, col); }
                                        } else { ed.mouse_drag(row, col); }
                                    }
                                    h
                                })
                            }
                        }
                        desktop::AppKind::Terminal => {
                            if id == 2 {
                                let mut tg = terminal::TERMINAL.lock();
                                tg.as_mut().and_then(|t| {
                                    let h = t.hit_test(wxu, wyu, www, wwh, mx, my);
                                    if let Some((row, col)) = h {
                                        if fresh_click {
                                            if is_double_click(id, row, col) { t.select_line(row); } else { t.mouse_down(row, col); }
                                        } else { t.mouse_drag(row, col); }
                                    }
                                    h
                                })
                            } else {
                                let mut et = terminal::EXTRA_TERMINALS.lock();
                                et.iter_mut().find(|(wid, _)| *wid == id).and_then(|(_, t)| {
                                    let h = t.hit_test(wxu, wyu, www, wwh, mx, my);
                                    if let Some((row, col)) = h {
                                        if fresh_click {
                                            if is_double_click(id, row, col) { t.select_line(row); } else { t.mouse_down(row, col); }
                                        } else { t.mouse_drag(row, col); }
                                    }
                                    h
                                })
                            }
                        }
                        _ => None,
                    };
                    if hit.is_some() {
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                        if kind == desktop::AppKind::Terminal {
                            if id == 2 {
                                if let Some(t) = terminal::TERMINAL.lock().as_mut() { t.dirty = true; }
                            } else if let Some((_, t)) = terminal::EXTRA_TERMINALS.lock().iter_mut().find(|(wid, _)| *wid == id) {
                                t.dirty = true;
                            }
                        }
                    }
                }
            }
        }

        // Handle clipboard_action_requested — Copy/Paste from an editor or
        // terminal's right-click context menu.
        {
            let action = {
                let mut dt = desktop::DESKTOP.lock();
                dt.as_mut().and_then(|d| d.clipboard_action_requested.take())
            };
            if let Some((win_id, is_paste)) = action {
                let kind = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().and_then(|d| d.windows.iter().find(|w| w.id == win_id).map(|w| w.app_kind))
                };
                match kind {
                    Some(desktop::AppKind::Editor) => {
                        if win_id == 3 {
                            if let Some(ed) = editor::EDITOR.lock().as_mut() { ed.clipboard_action(is_paste); }
                        } else if let Some((_, ed)) = editor::EXTRA_EDITORS.lock().iter_mut().find(|(wid, _)| *wid == win_id) {
                            ed.clipboard_action(is_paste);
                        }
                    }
                    Some(desktop::AppKind::Terminal) => {
                        if win_id == 2 {
                            if let Some(t) = terminal::TERMINAL.lock().as_mut() {
                                if is_paste { t.paste_clipboard(); } else { t.copy_selection(); }
                                t.dirty = true;
                            }
                        } else if let Some((_, t)) = terminal::EXTRA_TERMINALS.lock().iter_mut().find(|(wid, _)| *wid == win_id) {
                            if is_paste { t.paste_clipboard(); } else { t.copy_selection(); }
                            t.dirty = true;
                        }
                    }
                    _ => {}
                }
                if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
            }
        }

        // Handle open_settings_requested from right-click context menu
        {
            let requested = {
                let mut dt = desktop::DESKTOP.lock();
                if let Some(dt) = dt.as_mut() {
                    let r = dt.open_settings_requested;
                    dt.open_settings_requested = false;
                    r
                } else { false }
            };
            if requested {
                let mut dt = desktop::DESKTOP.lock();
                if let Some(dt) = dt.as_mut() {
                    if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 5) {
                        w.show();
                    }
                    dt.bring_to_front(5);
                    *FOCUSED_WIN.lock() = Some(5);
                    dt.dirty = true;
                }
            }
        }

        // Handle new_window_requested — "New Window" from a taskbar/start-menu right-click
        {
            let requested = {
                let mut dt = desktop::DESKTOP.lock();
                if let Some(dt) = dt.as_mut() {
                    let r = dt.new_window_requested;
                    dt.new_window_requested = None;
                    r
                } else { None }
            };
            if let Some(kind) = requested {
                let win_id = match kind {
                    desktop::AppKind::Terminal    => terminal::spawn_terminal(),
                    desktop::AppKind::Editor      => editor::spawn_editor_blank(),
                    desktop::AppKind::ImageViewer => image::spawn_viewer_blank(),
                    // Welcome/Sysmon/Settings/AudioPlayer show pure global state — a
                    // duplicate window just needs to exist, no per-instance data.
                    desktop::AppKind::Welcome     => spawn_stateless_window(desktop::AppKind::Welcome, "Welcome to HepOS", 300, 160),
                    desktop::AppKind::Sysmon      => spawn_stateless_window(desktop::AppKind::Sysmon, "Sysmon", 340, 260),
                    desktop::AppKind::Settings    => spawn_stateless_window(desktop::AppKind::Settings, "Settings", 480, 320),
                    desktop::AppKind::AudioPlayer => spawn_stateless_window(desktop::AppKind::AudioPlayer, "Audio Player", 380, 160),
                    desktop::AppKind::Files       => spawn_files(),
                };
                if win_id != usize::MAX {
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() {
                        if let Some(w) = dt.windows.iter_mut().find(|w| w.id == win_id) {
                            w.show();
                        }
                        dt.bring_to_front(win_id);
                        dt.dirty = true;
                    }
                    *FOCUSED_WIN.lock() = Some(win_id);
                }
            }
        }

        // Handle a double-clicked FsEntry desktop icon — desktop.rs has no
        // access to the block device or to editor/image/audio state, so it
        // just records what was opened and main.rs does the actual work,
        // same pattern as new_window_requested/open_settings_requested above.
        {
            let opened = {
                let mut dt = desktop::DESKTOP.lock();
                dt.as_mut().and_then(|dt| dt.open_fs_entry_requested.take())
            };
            if let Some((ino, is_dir, name, parent_path)) = opened {
                let entry_path = if parent_path == "/" {
                    alloc::format!("/{}", name)
                } else {
                    alloc::format!("{}/{}", parent_path, name)
                };
                if is_dir {
                    let win_id = spawn_files();
                    if win_id != usize::MAX {
                        let mut navs = HEPFS_NAVS.lock();
                        if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == win_id) {
                            nav.ino  = ino;
                            nav.path = entry_path;
                        }
                        drop(navs);
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == win_id) { w.show(); }
                            dt.bring_to_front(win_id);
                            dt.dirty = true;
                        }
                        *FOCUSED_WIN.lock() = Some(win_id);
                    }
                } else {
                    let file_path = entry_path;
                    let lower = name.to_lowercase();
                    if lower.ends_with(".bmp") {
                        image::open_smart(&file_path);
                    } else if lower.ends_with(".wav") {
                        audio::play(&file_path);
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 7) { w.show(); }
                            dt.bring_to_front(7);
                            dt.dirty = true;
                        }
                        drop(dt);
                        *FOCUSED_WIN.lock() = Some(7);
                    } else {
                        editor::open_smart(&file_path);
                    }
                }
            }
        }

        // Handle a confirmed desktop New File/New Folder/rename prompt — the
        // actual HepFS op (desktop.rs has no access to the block device).
        {
            let result = {
                let mut dt = desktop::DESKTOP.lock();
                dt.as_mut().and_then(|dt| dt.take_prompt_result())
            };
            if let Some(outcome) = result {
                let mut rename_failed: Option<alloc::string::String> = None;
                {
                    let mut ctrl = nvme::CONTROLLER.lock();
                    if let Some(ctrl) = ctrl.as_mut() {
                        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
                        match outcome {
                            desktop::PromptOutcome::CreateFile(name) => {
                                if let Some(desktop_ino) = hepfs::lookup(ctrl, "/home/desktop") {
                                    hepfs::create_file(ctrl, desktop_ino, &name);
                                }
                            }
                            desktop::PromptOutcome::CreateFolder(name) => {
                                if let Some(desktop_ino) = hepfs::lookup(ctrl, "/home/desktop") {
                                    hepfs::create_dir(ctrl, desktop_ino, &name);
                                }
                            }
                            desktop::PromptOutcome::Rename { old_name, new_name } => {
                                if let Some(desktop_ino) = hepfs::lookup(ctrl, "/home/desktop") {
                                    if !hepfs::rename(ctrl, desktop_ino, &old_name, &new_name) {
                                        rename_failed = Some(new_name);
                                    }
                                }
                            }
                            desktop::PromptOutcome::RenameFsPane { parent_ino, old_name, new_name, .. } => {
                                if !hepfs::rename(ctrl, parent_ino, &old_name, &new_name) {
                                    rename_failed = Some(new_name);
                                }
                            }
                        }
                    }
                }
                // Any of these can touch /home/desktop's contents (directly, or
                // indirectly if the file manager happened to be browsing it) —
                // cheap enough to just always re-sync rather than track which.
                refresh_desktop_icons();
                let mut dt = desktop::DESKTOP.lock();
                if let Some(dt) = dt.as_mut() {
                    dt.dirty = true;
                    // `rename()` only fails on a name collision (or a missing
                    // source, which can't happen here) — say so instead of
                    // just silently leaving the old name in place.
                    if let Some(new_name) = rename_failed {
                        dt.show_message(alloc::format!("'{}' already exists", new_name));
                    }
                }
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

        // Icon-grid scrollbar drag for a Files window's right pane — checked
        // before the ordinary row/nav click resolution below so a click
        // landing on the thin scrollbar strip doesn't also get misread as
        // a click on whatever grid cell happens to sit under that same x
        // column (the scrollbar occupies the pane's rightmost few pixels,
        // still inside its content-hit rect).
        let on_fs_scrollbar = if fresh_click {
            let dt = desktop::DESKTOP.lock();
            dt.as_ref().and_then(|d| {
                let win = d.windows.iter().rev().find(|w| {
                    w.app_kind == desktop::AppKind::Files && !w.minimized
                        && mx >= w.x && mx < w.x + w.w as i32 && my >= w.y && my < w.y + w.h as i32
                        && topmost_window_id_at(d, mx, my) == Some(w.id)
                })?;
                let left_w = (win.w * HEPFS_TREE_W_NUM) / HEPFS_TREE_W_DEN;
                let right_w = win.w.saturating_sub(left_w + 1);
                let list_top = win.y + 23;
                let list_h = (win.h as i32 - 23).max(0) as usize;
                let track_x = win.x + left_w as i32 + 1 + right_w.saturating_sub(HEPFS_SCROLLBAR_W) as i32;
                if mx >= track_x && mx < track_x + HEPFS_SCROLLBAR_W as i32 && my >= list_top as i32 {
                    Some((win.id, right_w, list_top, list_h))
                } else { None }
            })
        } else { None };
        if let Some((win_id, right_w, list_top, list_h)) = on_fs_scrollbar {
            if let Some(max_scroll) = hepfs_scroll_max(win_id, right_w, list_h) {
                *FS_SCROLL_DRAG.lock() = Some((win_id, list_top, list_h, max_scroll));
            }
        }
        {
            let btn_held = btn & 1 != 0;
            if btn_held {
                if let Some((win_id, list_top, list_h, max_scroll)) = *FS_SCROLL_DRAG.lock() {
                    let rel = (my - list_top).max(0) as i64;
                    let row = ((rel * max_scroll as i64) / list_h.max(1) as i64) as usize;
                    if let Some((_, nav)) = HEPFS_NAVS.lock().iter_mut().find(|(id, _)| *id == win_id) {
                        nav.scroll = row.min(max_scroll);
                    }
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                }
            } else {
                *FS_SCROLL_DRAG.lock() = None;
            }
        }

        // HepFS window: navigate directories and open files on click.
        // Any Files window can be clicked (not just id=1) — each has its own
        // independent navigator entry in HEPFS_NAVS, shared by both panes
        // (see render_hepfs_window: left = directories-only, right = full
        // listing — both views of the *same* current directory).
        //
        // Z-order note: this only fires if the topmost thing under the
        // cursor is actually this Files window's own chrome/content — a
        // window of any *other* kind stacked on top (e.g. its close button
        // visually overlapping the Files window underneath) must win the
        // click instead of leaking through to Files-window navigation. See
        // `topmost_window_id_at()`.
        if fresh_click && on_fs_scrollbar.is_none() {
            let hepfs_click = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().and_then(|d| {
                    let win = d.windows.iter().find(|w| {
                        w.app_kind == desktop::AppKind::Files && !w.minimized
                            && mx >= w.x && mx < w.x + w.w as i32
                            && my >= w.y && my < w.y + w.h as i32
                    })?;
                    if topmost_window_id_at(d, mx, my) != Some(win.id) { return None; }
                    let win_id = win.id;
                    let rel_x = (mx - win.x) as usize;
                    let rel_y = my - win.y;
                    if rel_y < 22 {
                        // Nav bar: back=0, fwd=1, other=2
                        let zone = if rel_x < 22 { 0u8 } else if rel_x < 44 { 1 } else { 2 };
                        Some((win_id, zone, 0usize))
                    } else {
                        let left_w = (win.w * HEPFS_TREE_W_NUM) / HEPFS_TREE_W_DEN;
                        if rel_x <= left_w {
                            let entry_idx = (rel_y - 23).max(0) as usize / 14;
                            Some((win_id, 3u8, entry_idx)) // left pane: directories only
                        } else {
                            let right_w = win.w.saturating_sub(left_w + 1);
                            let scroll = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == win_id)
                                .map(|(_, n)| n.scroll).unwrap_or(0);
                            let idx = grid_idx_at(rel_x as i32 - left_w as i32 - 1, rel_y - 23, right_w, scroll);
                            Some((win_id, 4u8, idx)) // right pane: full listing, icon grid
                        }
                    }
                })
            };

            match hepfs_click {
                Some((win_id, 0, _)) => {
                    // Back button
                    let mut navs = HEPFS_NAVS.lock();
                    if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == win_id) {
                        if let Some((pino, ppath)) = nav.back.pop() {
                            let cur_ino  = nav.ino;
                            let cur_path = nav.path.clone();
                            nav.fwd.push((cur_ino, cur_path));
                            nav.ino  = pino;
                            nav.path = ppath;
                            nav.range_selected.clear();
                            nav.scroll = 0;
                        }
                    }
                    drop(navs);
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                }
                Some((win_id, 1, _)) => {
                    // Forward button
                    let mut navs = HEPFS_NAVS.lock();
                    if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == win_id) {
                        if let Some((nino, npath)) = nav.fwd.pop() {
                            let cur_ino  = nav.ino;
                            let cur_path = nav.path.clone();
                            nav.back.push((cur_ino, cur_path));
                            nav.ino  = nino;
                            nav.path = npath;
                            nav.range_selected.clear();
                            nav.scroll = 0;
                        }
                    }
                    drop(navs);
                    let mut dt = desktop::DESKTOP.lock();
                    if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                }
                Some((win_id, pane @ (3 | 4), idx)) => {
                    // Directory-tree (pane==3) or full-listing (pane==4) entry click.
                    let (cur_ino, cur_path) = HEPFS_NAVS.lock().iter()
                        .find(|(id, _)| *id == win_id).map(|(_, n)| (n.ino, n.path.clone()))
                        .unwrap_or((hepfs::ROOT_INO, alloc::string::String::from("/")));
                    let at_root = cur_ino == hepfs::ROOT_INO;

                    // ".." row (only shown when not at root) — same on both panes
                    if !at_root && idx == 0 {
                        let mut navs = HEPFS_NAVS.lock();
                        if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == win_id) {
                            if let Some((pino, ppath)) = nav.back.pop() {
                                let ci = nav.ino;
                                let cp = nav.path.clone();
                                nav.fwd.push((ci, cp));
                                nav.ino  = pino;
                                nav.path = ppath;
                                nav.range_selected.clear();
                                nav.scroll = 0;
                            }
                        }
                        drop(navs);
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                    } else {
                        let real_idx = if !at_root { idx.saturating_sub(1) } else { idx };
                        // Left pane (3) only lists directories; right pane (4) lists everything.
                        let entry = {
                            let mut ctrl = nvme::CONTROLLER.lock();
                            ctrl.as_mut().and_then(|ctrl| {
                                let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
                                let entries = hepfs::list_dir(ctrl, cur_ino);
                                if pane == 3 {
                                    entries.iter()
                                        .filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
                                        .nth(real_idx)
                                        .map(|(ino, name)| (*ino, name.clone(), hepfs::F_DIR))
                                } else {
                                    entries.get(real_idx).map(|(ino, name)| {
                                        let inode = hepfs::read_inode(ctrl, *ino);
                                        (*ino, name.clone(), inode.flags)
                                    })
                                }
                            })
                        };
                        if let Some((ino, name, flags)) = entry {
                            // Shift+click range-selects every row between the
                            // current anchor (nav.selected) and this one, in
                            // the same pane — highlight only, doesn't move the
                            // anchor and doesn't arm a drag/open/rename (a
                            // Shift+click is purely a selection modifier, same
                            // as the desktop's own Shift+click).
                            if ps2::shift_held() {
                                let mut navs = HEPFS_NAVS.lock();
                                if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == win_id) {
                                    if let Some((anchor_pane, anchor_idx)) = nav.selected {
                                        if anchor_pane == pane {
                                            let (lo, hi) = (anchor_idx.min(idx), anchor_idx.max(idx));
                                            nav.range_selected = (lo..=hi).collect();
                                        }
                                    }
                                }
                                drop(navs);
                                let mut dt = desktop::DESKTOP.lock();
                                if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                                continue;
                            }

                            // Select-then-click-again semantics, same thresholds
                            // the desktop's own icons use (ICON_DBLCLICK_TICKS/
                            // ICON_RENAME_TICKS) so both feel identical: a fast
                            // second click on the already-selected row opens it,
                            // a slower one renames it, anything else just selects.
                            // The select itself (highlight) applies immediately —
                            // only the open/navigate/rename *action* is deferred
                            // to release (see FileDrag::pending_action).
                            let now = scheduler::TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                            let click_state = {
                                let mut navs = HEPFS_NAVS.lock();
                                navs.iter_mut().find(|(id, _)| *id == win_id).map(|(_, nav)| {
                                    let was_selected = nav.selected == Some((pane, idx));
                                    let elapsed = now.saturating_sub(nav.selected_at);
                                    // A plain click on a row that's already part of
                                    // the current multi-selection (range_selected)
                                    // keeps the whole group selected, so it can be
                                    // dragged together below — same "click an
                                    // unselected item narrows to just it, click an
                                    // already-selected one keeps the group" rule the
                                    // desktop's own icon multi-select uses. Only a
                                    // click *outside* the group resets to single-select.
                                    let in_group = nav.range_selected.contains(&idx);
                                    if !in_group {
                                        nav.selected = Some((pane, idx));
                                        nav.range_selected.clear();
                                    }
                                    nav.selected_at = now;
                                    (was_selected, elapsed, nav.range_selected.clone())
                                })
                            };
                            let Some((was_selected, elapsed, range)) = click_state else { continue; };
                            let action = if was_selected && elapsed <= desktop::ICON_DBLCLICK_TICKS {
                                FileRowAction::Open
                            } else if was_selected && elapsed <= desktop::ICON_RENAME_TICKS {
                                FileRowAction::Rename
                            } else {
                                FileRowAction::None
                            };
                            let mut dt = desktop::DESKTOP.lock();
                            if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                            drop(dt);

                            let extra = if range.len() > 1 {
                                range.into_iter().filter(|&r| r != idx)
                                    .filter_map(|r| resolve_fs_row(win_id, pane, r))
                                    .collect()
                            } else {
                                alloc::vec::Vec::new()
                            };
                            *FILE_DRAG.lock() = Some(FileDrag {
                                from_parent: cur_ino, from_path: cur_path.clone(), ino, name: name.clone(),
                                start_x: mx, start_y: my, moved: false,
                                win_id, is_dir: flags == hepfs::F_DIR,
                                pending_action: action, extra,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        // Resolve a pending Files-window right-click (desktop.rs detected
        // *that* one happened but can't work out which row — needs the block
        // device) into the actual context menu: a specific row (Open/Pin/
        // Copy) or empty space (Paste). Same pane/row geometry the left-click
        // handler above uses.
        {
            let pending = { let mut dt = desktop::DESKTOP.lock(); dt.as_mut().and_then(|d| d.fs_context_menu_pending.take()) };
            if let Some((win_id, rx, ry)) = pending {
                let win_rect = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().and_then(|d| d.windows.iter().find(|w| w.id == win_id).map(|w| (w.x, w.y, w.w)))
                };
                if let Some((wx, wy, ww)) = win_rect {
                    let (cur_ino, cur_path) = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == win_id)
                        .map(|(_, n)| (n.ino, n.path.clone())).unwrap_or((hepfs::ROOT_INO, alloc::string::String::from("/")));
                    let at_root = cur_ino == hepfs::ROOT_INO;
                    let rel_x = (rx - wx) as usize;
                    let rel_y = ry - wy;
                    let kind = if rel_y < 22 {
                        None // nav bar — no context menu there
                    } else {
                        let left_w = (ww * HEPFS_TREE_W_NUM) / HEPFS_TREE_W_DEN;
                        let pane: u8 = if rel_x <= left_w { 3 } else { 4 };
                        let entry_idx = if pane == 3 {
                            ((rel_y - 22) / 14).max(0) as usize
                        } else {
                            let right_w = ww.saturating_sub(left_w + 1);
                            let scroll = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == win_id)
                                .map(|(_, n)| n.scroll).unwrap_or(0);
                            grid_idx_at(rel_x as i32 - left_w as i32 - 1, rel_y - 22, right_w, scroll)
                        };
                        if !at_root && entry_idx == 0 {
                            None // ".." row — nothing sensible to Open/Pin/Copy there
                        } else {
                            let real_idx = if !at_root { entry_idx - 1 } else { entry_idx };
                            let entry = {
                                let mut ctrl = nvme::CONTROLLER.lock();
                                ctrl.as_mut().and_then(|ctrl| {
                                    let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
                                    let entries = hepfs::list_dir(ctrl, cur_ino);
                                    if pane == 3 {
                                        entries.iter().filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
                                            .nth(real_idx).map(|(ino, name)| (*ino, name.clone(), true))
                                    } else {
                                        entries.get(real_idx).map(|(ino, name)| {
                                            let is_dir = hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR;
                                            (*ino, name.clone(), is_dir)
                                        })
                                    }
                                })
                            };
                            match entry {
                                Some((ino, name, is_dir)) => {
                                    // If this row is part of a multi-selection,
                                    // "Copy" should stage the whole group —
                                    // resolve every *other* selected row too.
                                    let (is_selected_row, range) = {
                                        let navs = HEPFS_NAVS.lock();
                                        navs.iter().find(|(id, _)| *id == win_id)
                                            .map(|(_, n)| (n.selected == Some((pane, entry_idx)), n.range_selected.clone()))
                                            .unwrap_or((false, alloc::vec::Vec::new()))
                                    };
                                    let extra = if is_selected_row && range.len() > 1 {
                                        range.into_iter().filter(|&r| r != entry_idx)
                                            .filter_map(|r| resolve_fs_row(win_id, pane, r))
                                            .collect()
                                    } else {
                                        alloc::vec::Vec::new()
                                    };
                                    Some(desktop::ContextMenuKind::FsRow {
                                        win_id, parent_ino: cur_ino, parent_path: cur_path, ino, name, is_dir,
                                        is_pinned: desktop::is_pinned_file(ino), extra,
                                    })
                                }
                                None => Some(desktop::ContextMenuKind::FsPane { win_id, parent_ino: cur_ino, parent_path: cur_path }),
                            }
                        }
                    };
                    if let Some(kind) = kind {
                        let mut dt = desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            dt.context_menu = Some((rx, ry));
                            dt.context_menu_kind = kind;
                            dt.dirty = true;
                        }
                    }
                }
            }
        }

        // Settings window: sidebar page switch, wallpaper thumbnail click, and
        // the Sound page's volume slider (click-to-set, and drag-to-scrub
        // while the button stays held — same held/fresh_click pattern as the
        // editor/terminal text-selection drag above).
        {
            enum SettingsAction { Page(u8), Wallpaper(u8), Volume(u8) }
            let btn_held = btn & 1 != 0;
            let win_rel = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().and_then(|d| {
                    let win = d.windows.iter().find(|w| w.id == 5 && !w.minimized)?;
                    if mx < win.x || mx >= win.x + win.w as i32 { return None; }
                    if my < win.y || my >= win.y + win.h as i32  { return None; }
                    Some(((mx - win.x) as usize, (my - win.y) as usize))
                })
            };
            // Only act if the Settings window is actually topmost at (mx, my)
            // — otherwise a click meant for some other window stacked on top
            // (e.g. its close button visually overlapping Settings) would
            // leak through as a stray sidebar/slider click underneath.
            let settings_is_topmost = {
                let dt = desktop::DESKTOP.lock();
                dt.as_ref().map(|d| topmost_window_id_at(d, mx, my) == Some(5)).unwrap_or(false)
            };
            let action = if fresh_click && settings_is_topmost {
                win_rel.and_then(|(rel_x, rel_y)| {
                    if rel_x < 110 {
                        let row = (rel_y.saturating_sub(SETTINGS_SIDEBAR_TOP)) / SETTINGS_SIDEBAR_ROW_H;
                        return SETTINGS_SIDEBAR.get(row).map(|&(_, id)| SettingsAction::Page(id));
                    }
                    let page = desktop::SETTINGS_PAGE.load(core::sync::atomic::Ordering::Relaxed);
                    if page == desktop::SETTINGS_PAGE_SOUND {
                        let px = rel_x.saturating_sub(110 + 1);
                        let py = rel_y;
                        if py >= VOL_SLIDER_Y.saturating_sub(4) && py < VOL_SLIDER_Y + VOL_SLIDER_H + 4 {
                            *VOLUME_DRAG.lock() = true;
                            let frac = px.saturating_sub(VOL_SLIDER_X) as f32 / VOL_SLIDER_W as f32;
                            return Some(SettingsAction::Volume((frac.clamp(0.0, 1.0) * 100.0) as u8));
                        }
                        return None;
                    }
                    // Background page: thumbnail click
                    let tx0 = 120usize; let ty0 = 50usize;
                    let tw = 120usize;  let th = 80usize; let tgap = 16usize;
                    for i in 0..2usize {
                        let tleft = tx0 + i * (tw + tgap);
                        if rel_x >= tleft && rel_x < tleft + tw
                            && rel_y >= ty0 && rel_y < ty0 + th + 14
                        {
                            return Some(SettingsAction::Wallpaper(i as u8));
                        }
                    }
                    None
                })
            } else if btn_held && *VOLUME_DRAG.lock() {
                win_rel.map(|(rel_x, _)| {
                    let px = rel_x.saturating_sub(110 + 1);
                    let frac = (px.saturating_sub(VOL_SLIDER_X)) as f32 / VOL_SLIDER_W as f32;
                    SettingsAction::Volume((frac.clamp(0.0, 1.0) * 100.0) as u8)
                })
            } else {
                None
            };
            if !btn_held { *VOLUME_DRAG.lock() = false; }

            match action {
                Some(SettingsAction::Page(id)) => {
                    desktop::SETTINGS_PAGE.store(id, core::sync::atomic::Ordering::Relaxed);
                    if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
                }
                Some(SettingsAction::Wallpaper(wp)) => {
                    desktop::WALLPAPER.store(wp, core::sync::atomic::Ordering::Relaxed);
                    if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
                }
                Some(SettingsAction::Volume(v)) => {
                    hda::set_volume(v);
                    if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
                }
                None => {}
            }
        }

        // File/folder drag-and-drop between (or within) Files windows —
        // resolves the drag armed above. `btn_held` here reuses the same
        // "still down" signal the text-selection/volume drags above check.
        {
            let btn_held = btn & 1 != 0;
            if btn_held {
                let mut fd = FILE_DRAG.lock();
                if let Some(fd) = fd.as_mut() {
                    if !fd.moved && ((mx - fd.start_x).abs() > 8 || (my - fd.start_y).abs() > 8) {
                        fd.moved = true;
                    }
                }
            } else {
                let drag = FILE_DRAG.lock().take();
                if let Some(fd) = drag {
                    if fd.moved {
                        resolve_file_drop(&fd, mx, my);
                    } else {
                        // Not dragged — run whatever action this click resolved
                        // to at mousedown (see FileDrag::pending_action).
                        match fd.pending_action {
                            FileRowAction::None => {}
                            FileRowAction::Open if fd.is_dir => {
                                let mut navs = HEPFS_NAVS.lock();
                                if let Some((_, nav)) = navs.iter_mut().find(|(id, _)| *id == fd.win_id) {
                                    let cur_ino2 = nav.ino;
                                    let cur_path = nav.path.clone();
                                    nav.back.push((cur_ino2, cur_path));
                                    nav.fwd.clear();
                                    nav.ino = fd.ino;
                                    nav.path = if nav.path == "/" {
                                        alloc::format!("/{}", fd.name)
                                    } else {
                                        alloc::format!("{}/{}", nav.path, fd.name)
                                    };
                                    nav.selected = None;
                                    nav.range_selected.clear();
                                    nav.scroll = 0;
                                }
                                drop(navs);
                                let mut dt = desktop::DESKTOP.lock();
                                if let Some(dt) = dt.as_mut() { dt.dirty = true; }
                            }
                            FileRowAction::Open => {
                                // Open file in editor, or in the image viewer for
                                // .bmp / audio player for .wav.
                                let cur_path = HEPFS_NAVS.lock().iter()
                                    .find(|(id, _)| *id == fd.win_id).map(|(_, n)| n.path.clone())
                                    .unwrap_or_else(|| alloc::string::String::from("/"));
                                let file_path = if cur_path == "/" {
                                    alloc::format!("/{}", fd.name)
                                } else {
                                    alloc::format!("{}/{}", cur_path, fd.name)
                                };
                                let lower = fd.name.to_lowercase();
                                if lower.ends_with(".bmp") {
                                    image::open_smart(&file_path);
                                } else if lower.ends_with(".wav") {
                                    // All Audio Player windows show the same global
                                    // "now playing" state, so just bring the main
                                    // one (id=7) forward.
                                    audio::play(&file_path);
                                    let mut dt = desktop::DESKTOP.lock();
                                    if let Some(dt) = dt.as_mut() {
                                        if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 7) { w.show(); }
                                        dt.bring_to_front(7);
                                        dt.dirty = true;
                                    }
                                    drop(dt);
                                    *FOCUSED_WIN.lock() = Some(7);
                                } else {
                                    editor::open_smart(&file_path);
                                }
                            }
                            FileRowAction::Rename => {
                                let mut dt = desktop::DESKTOP.lock();
                                if let Some(dt) = dt.as_mut() {
                                    dt.begin_rename_fs_pane(fd.win_id, fd.from_parent, fd.name);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Also force dirty after keyboard input so terminal text appears
        if ps2_had_input {
            let mut dt = desktop::DESKTOP.lock();
            if let Some(dt) = dt.as_mut() { dt.dirty = true; }
            let mut tm = terminal::TERMINAL.lock();
            if let Some(tm) = tm.as_mut() { tm.dirty = true; }
        }

        // Advance any in-progress async audio playback (zero-buffer/drain/stop
        // state machine — see hda::play_pcm()/poll() docs).
        hda::poll();

        // Expire the desktop's transient "Programs can't be renamed" toast, if any.
        { let mut dt = desktop::DESKTOP.lock(); if let Some(dt) = dt.as_mut() { dt.tick_icon_message(); } }

        // Advance any in-progress async network job (ping/wget/udp) — see
        // net::poll() docs. Delivers the result to whichever terminal window
        // issued the command once it finishes (success, error, or timeout).
        if let Some((win_id, msg)) = net::poll() {
            if win_id == 2 {
                if let Some(t) = terminal::TERMINAL.lock().as_mut() { t.print_async(&msg); }
            } else if let Some((_, t)) = terminal::EXTRA_TERMINALS.lock().iter_mut().find(|(wid, _)| *wid == win_id) {
                t.print_async(&msg);
            }
            if let Some(dt) = desktop::DESKTOP.lock().as_mut() { dt.dirty = true; }
        }

        // Two-tier rendering:
        //   content_dirty → full scene redraw + save_scene + cursor + full flush
        //   mouse_only    → restore scene rows near cursor + repaint cursor + partial flush
        // The partial path updates only ~20 rows (~100 KB) instead of 3.5 MB, so
        // cursor movement is extremely cheap and runs at the full polling rate.
        // Advance window open/close animations (cheap no-op when none are
        // active); force a redraw every frame while any is in progress, since
        // nothing else would otherwise mark the scene dirty mid-animation.
        // Must also force it on the exact frame an animation *finishes* —
        // tick_anims() clears the anim before any_animating() would see it,
        // so relying on any_animating() alone skipped that final frame and
        // left a just-closed window's last (shrunk) frame on screen
        // indefinitely, until some unrelated input forced a redraw.
        let animating = {
            let mut dt = desktop::DESKTOP.lock();
            dt.as_mut().map(|d| {
                let just_finished = d.tick_anims();
                just_finished || d.any_animating() || d.start_menu_animating()
            }).unwrap_or(false)
        };

        // A file/folder being actively dragged needs a full redraw every
        // frame too, same reason the taskbar's drag ghost does (see
        // desktop.rs) — it has to visibly track the cursor, and nothing else
        // here would otherwise mark the scene dirty from mouse movement alone.
        let file_dragging = FILE_DRAG.lock().as_ref().map(|fd| fd.moved).unwrap_or(false);
        let content_dirty = {
            let dd = desktop::DESKTOP.lock().as_ref().map(|d| d.dirty).unwrap_or(false);
            let td = terminal::TERMINAL.lock().as_ref().map(|t| t.dirty).unwrap_or(false);
            let ed = terminal::EXTRA_TERMINALS.lock().iter().any(|(_, t)| t.dirty);
            dd || td || ed || ps2_had_input || hda::is_playing() || animating || file_dragging
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

                // 2. Windows in z-order — includes windows still mid-close-animation
                // (not yet actually `minimized`), rendered at their eased (shrinking
                // or growing) rect instead of their real target geometry.
                let win_order: alloc::vec::Vec<(usize, desktop::AppKind, bool, i32, i32, usize, usize)> = {
                    let dt = desktop::DESKTOP.lock();
                    dt.as_ref().map(|d| d.windows.iter()
                        .filter(|w| !w.minimized || w.is_closing())
                        .map(|w| {
                            let (ex, ey, ew, eh) = w.eased_rect();
                            (w.id, w.app_kind, d.focused == Some(w.id), ex, ey, ew, eh)
                        })
                        .collect()
                    ).unwrap_or_default()
                };
                for (id, kind, focused, wx, wy, ww, wh) in &win_order {
                    { let dt = desktop::DESKTOP.lock();
                      if let Some(dt) = dt.as_ref() {
                          if let Some(win) = dt.windows.iter().find(|w| w.id == *id) {
                              dt.draw_window(display, win, *focused, (*wx, *wy, *ww, *wh));
                          }
                      }
                    }
                    let wx = (*wx).max(0) as usize;
                    let wy = (*wy).max(0) as usize;
                    // Dispatch by app kind, not raw id — the "main" window of each
                    // kind (fixed id 2/3/6) reads its dedicated static; any other
                    // window of that kind is looked up in its EXTRA_* list. Kinds
                    // with no per-instance state (Welcome/Files/Sysmon/Settings/
                    // AudioPlayer) render identically regardless of which window
                    // (or how many) are open.
                    match kind {
                        desktop::AppKind::Welcome => render_welcome_window(display, wx, wy, *ww, *wh),
                        desktop::AppKind::Files   => render_hepfs_window(display, wx, wy, *ww, *wh, *id),
                        desktop::AppKind::Terminal => {
                            if *id == 2 {
                                let mut tg = terminal::TERMINAL.lock();
                                if let Some(t) = tg.as_mut() {
                                    t.render(display, wx, wy, *ww, *wh);
                                    t.dirty = false;
                                }
                            } else {
                                let mut et = terminal::EXTRA_TERMINALS.lock();
                                if let Some((_, t)) = et.iter_mut().find(|(wid, _)| *wid == *id) {
                                    t.render(display, wx, wy, *ww, *wh);
                                    t.dirty = false;
                                }
                            }
                        }
                        desktop::AppKind::Editor => {
                            if *id == 3 {
                                let mut eg = editor::EDITOR.lock();
                                if let Some(ed) = eg.as_mut() {
                                    ed.render(display, wx, wy, *ww, *wh);
                                }
                            } else {
                                let mut ee = editor::EXTRA_EDITORS.lock();
                                if let Some((_, ed)) = ee.iter_mut().find(|(wid, _)| *wid == *id) {
                                    ed.render(display, wx, wy, *ww, *wh);
                                }
                            }
                        }
                        desktop::AppKind::Sysmon   => render_sysmon_window(display, wx, wy, *ww, *wh),
                        desktop::AppKind::Settings => render_settings_window(display, wx, wy, *ww, *wh),
                        desktop::AppKind::ImageViewer => {
                            let no_image = |display: &mut framebuffer::Display| {
                                display.fill_rect(wx, wy, *ww, *wh, framebuffer::Color::from_hex(0x0A0A14));
                                display.draw_text(wx + 8, wy + 8, "No image open - try `view <file>.bmp`",
                                    framebuffer::Color::from_hex(0x888888), 1);
                            };
                            if *id == 6 {
                                let vg = image::VIEWER.lock();
                                match vg.as_ref() {
                                    Some(v) => v.render(display, wx, wy, *ww, *wh),
                                    None => no_image(display),
                                }
                            } else {
                                let ve = image::EXTRA_VIEWERS.lock();
                                match ve.iter().find(|(wid, _)| *wid == *id) {
                                    Some((_, v)) => v.render(display, wx, wy, *ww, *wh),
                                    None => no_image(display),
                                }
                            }
                        }
                        desktop::AppKind::AudioPlayer => audio::render(display, wx, wy, *ww, *wh),
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
                // 3b. Taskbar jump list + context menu — drawn last so they float above
                // the taskbar (both can now be triggered by a taskbar right-click).
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_taskbar_jumplist(display); } }
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_volume_popup(display); } }
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_context_menu(display); } }
                { let dt = desktop::DESKTOP.lock();
                  if let Some(dt) = dt.as_ref() { dt.draw_new_entry_prompt(display); } }
                draw_file_drag_ghost(display, mx, my);

                // 4. Save scene (no cursor yet) so cursor-only path can erase later
                display.save_scene();

                // 5. Cursor
                let cy = my as usize;
                draw_cursor!(display, mx as usize, cy);
                prev_cursor_y = cy;

                // 6. Full flush
                display.flush();
                virtio_gpu::present_mirror();
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

                // Flush only the affected rows (~100 KB vs 3.5 MB full flush).
                // Deliberately NOT mirrored to virtio-gpu — present_mirror()
                // always transfers the whole frame (no partial-rect support
                // yet), and this path runs on every mouse-move event, so
                // mirroring it too would turn the cheap ~100KB cursor-only
                // path into a full-frame transfer on every mouse tick. The
                // virtio-gpu output simply lags cursor-only moves until the
                // next full (content-dirty) redraw — an acceptable tradeoff
                // for a first cut, not a bug.
                display.flush_rows(y0, rows);
            }
        }

        UPTIME_FRAMES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // No sleep — dirty flags gate rendering; idle task uses hlt so CPU rests between quanta
    }
}

/// Spawn a new window for an app kind whose render function reads pure global
/// state (Welcome/Sysmon/Settings/AudioPlayer) — no per-instance data needed,
/// the new window just needs to exist; the render dispatch already handles any
/// window of that kind identically regardless of id.
fn spawn_stateless_window(kind: desktop::AppKind, title: &str, w: usize, h: usize) -> usize {
    let mut dt = desktop::DESKTOP.lock();
    if let Some(dt) = dt.as_mut() {
        let count = dt.windows.iter().filter(|win| win.app_kind == kind).count();
        let off = (count as i32) * 24;
        dt.add_window(kind, title, 100 + off, 60 + off, w, h)
    } else {
        usize::MAX
    }
}

/// Two-pane file manager (Norton-Commander-style): splits the window into a
/// left and right half, each an independent `HepfsNav`, separated by a 1px
/// divider. The active pane (last one clicked) gets a highlighted top border.
/// Left-pane width as a fraction of the window — narrower than the right,
/// since it only ever shows directory names (no sizes).
const HEPFS_TREE_W_NUM: usize = 35;
const HEPFS_TREE_W_DEN: usize = 100;

// ── Right-pane icon grid (the "large icon based file manager" ask) ─────────
// The left tree pane stays a plain directory-name list (it's always short —
// one level of subdirectories); only the right (full-listing) pane became a
// wrapped icon grid. Cell = icon + centered label underneath, same glyphs
// `icons::draw_file_icon` already draws for the old list rows, just bigger.
const GRID_ICON:     usize = 36;
const GRID_CELL_W:   usize = 72;
const GRID_CELL_H:   usize = 60;
/// Longest a filename label can be drawn at without overflowing
/// `GRID_CELL_W` (9px/char at scale 1) and running into the next cell — one
/// char short of the exact fit (`GRID_CELL_W / 9`) so there's still a
/// sliver of margin between adjacent cells' labels.
const GRID_LABEL_MAX_CHARS: usize = GRID_CELL_W / 9 - 1;
/// Width, in pixels, of the vertical scrollbar track drawn along the right
/// pane's right edge when its content overflows the window.
const HEPFS_SCROLLBAR_W: usize = 8;

/// Columns the right pane's icon grid fits at a given pane width — always at
/// least 1, so a very narrow window degrades to a single column instead of
/// dividing by a `cols` of 0.
fn grid_cols(right_w: usize) -> usize {
    (right_w.saturating_sub(HEPFS_SCROLLBAR_W) / GRID_CELL_W).max(1)
}

/// Maps a click/drop point *relative to the right pane's top-left content
/// origin* (i.e. already offset past the tree pane and the nav bar) into a
/// flat row index — the same `idx` numbering the old row-list used (0 = the
/// ".." row when not at root, entries following in `hepfs::list_dir()`
/// order), so every existing consumer of `(pane, idx)` — `resolve_fs_row()`,
/// the click handler, drag/drop, the right-click context menu — needed no
/// changes at all once this replaced their old `/14`-row-height math for
/// pane 4. `scroll_rows` is the pane's current scroll offset in whole grid
/// rows (`HepfsNav::scroll`), added before flattening to an index so a
/// scrolled view still hit-tests against what's actually on screen.
fn grid_idx_at(rel_x_in_pane: i32, rel_y_in_pane: i32, right_w: usize, scroll_rows: usize) -> usize {
    let cols = grid_cols(right_w);
    let col = (rel_x_in_pane.max(0) as usize / GRID_CELL_W).min(cols - 1);
    let visual_row = rel_y_in_pane.max(0) as usize / GRID_CELL_H;
    (visual_row + scroll_rows) * cols + col
}

/// Two-pane file manager: a single shared nav bar (back/forward/path) across
/// the top, then a directories-only list on the left (a lightweight
/// current-directory "tree") and the full listing (directories + files) on
/// the right — both views of the *same* current directory, not independent
/// browsers. Clicking a directory in either pane navigates both.
fn render_hepfs_window(display: &mut framebuffer::Display, wx: usize, wy: usize, ww: usize, wh: usize, win_id: usize) {
    let bg       = framebuffer::Color::from_hex(0x0C0C0C);
    let text     = framebuffer::Color::from_hex(0xE8E8E8);
    let dim      = framebuffer::Color::from_hex(0x888888);
    let acc      = framebuffer::Color::from_hex(0x6C8EFF);
    let nav_bg   = framebuffer::Color::from_hex(0x0F0F1A);
    let btn_bg   = framebuffer::Color::from_hex(0x1A1A30);
    let path_bg  = framebuffer::Color::from_hex(0x0D0D18);
    let dir_col  = framebuffer::Color::from_hex(0x88AAFF);

    display.fill_rect(wx, wy, ww, wh, bg);

    // ── Nav bar (22px tall, full width, shared by both panes) ────────────────
    let nav_h: usize = 22;
    display.fill_rect(wx, wy, ww, nav_h, nav_bg);

    let (has_back, has_fwd, cur_path, cur_ino, selected, range_selected) = {
        let navs = HEPFS_NAVS.lock();
        match navs.iter().find(|(id, _)| *id == win_id) {
            Some((_, n)) => (!n.back.is_empty(), !n.fwd.is_empty(), n.path.clone(), n.ino, n.selected, n.range_selected.clone()),
            None => (false, false, alloc::string::String::from("/"), hepfs::ROOT_INO, None, alloc::vec::Vec::new()),
        }
    };
    // A row is highlighted if it's the click-anchor OR part of a Shift+click
    // range — the range only applies within `selected`'s own pane (a range in
    // one pane shouldn't bleed into the other).
    let is_highlighted = |pane: u8, row: usize| {
        selected == Some((pane, row))
            || (selected.map(|(p, _)| p) == Some(pane) && range_selected.contains(&row))
    };

    // Back button
    display.fill_rect(wx + 2, wy + 4, 18, 14, btn_bg);
    display.draw_text(wx + 6,  wy + 6, "<", if has_back { acc } else { dim }, 1);

    // Forward button
    display.fill_rect(wx + 22, wy + 4, 18, 14, btn_bg);
    display.draw_text(wx + 27, wy + 6, ">", if has_fwd { acc } else { dim }, 1);

    // Path bar
    let path_x = wx + 44;
    let path_w = ww.saturating_sub(46);
    display.fill_rect(path_x, wy + 4, path_w, 14, path_bg);
    let max_chars = path_w / 9;
    let display_path = if cur_path.len() > max_chars && max_chars > 0 {
        &cur_path[cur_path.len() - max_chars..]
    } else { &cur_path };
    display.draw_text(path_x + 2, wy + 6, display_path, text, 1);

    // Separator under the nav bar
    display.fill_rect(wx, wy + nav_h, ww, 1, acc);

    // ── Two panes below the nav bar ───────────────────────────────────────────
    let list_top = wy + nav_h + 1;
    let list_h   = wh.saturating_sub(nav_h + 1);
    let left_w   = (ww * HEPFS_TREE_W_NUM) / HEPFS_TREE_W_DEN;
    let right_w  = ww.saturating_sub(left_w + 1);
    let right_x  = wx + left_w + 1;
    display.fill_rect(wx + left_w, list_top, 1, list_h, acc);

    let at_root = cur_ino == hepfs::ROOT_INO;
    let mut ctrl = nvme::CONTROLLER.lock();
    if let Some(ctrl) = ctrl.as_mut() {
        let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
        let entries = hepfs::list_dir(ctrl, cur_ino);
        let dirs: alloc::vec::Vec<_> = entries.iter()
            .filter(|(ino, _)| hepfs::read_inode(ctrl, *ino).flags == hepfs::F_DIR)
            .collect();

        let hover = framebuffer::Color::from_hex(0x1E1E40);

        // Left pane: directories only (current directory's one-level "tree")
        let mut y = list_top + 2;
        let mut row = 0usize;
        if !at_root {
            if is_highlighted(3, row) { display.fill_rect(wx, y.saturating_sub(1), left_w, 13, hover); }
            icons::draw_file_icon(display, wx + 3, y.saturating_sub(1), 12, true, "..");
            display.draw_text(wx + 18, y, "..", dir_col, 1);
            y += 14; row += 1;
        }
        for (_, name) in &dirs {
            if y + 12 > list_top + list_h { break; }
            if is_highlighted(3, row) { display.fill_rect(wx, y.saturating_sub(1), left_w, 13, hover); }
            icons::draw_file_icon(display, wx + 3, y.saturating_sub(1), 12, true, name);
            display.draw_text(wx + 18, y, name, dir_col, 1);
            y += 14; row += 1;
        }
        if dirs.is_empty() && at_root {
            display.draw_text(wx + 4, list_top + 4, "(no folders)", dim, 1);
        }

        // Right pane: full listing (directories + files) as a wrapped
        // large-icon grid — item 9's "large icon based file manager" ask.
        // Hit-testing (clicks/drops/right-clicks) lives in `grid_idx_at()`,
        // kept in lockstep with this exact layout (same `GRID_CELL_W/H`,
        // same idx numbering: 0 = ".." when not at root, then `entries` in
        // `hepfs::list_dir()` order) — this function only needs to *draw*.
        let grid_w    = right_w.saturating_sub(HEPFS_SCROLLBAR_W);
        let cols      = grid_cols(right_w);
        let total     = entries.len() + if at_root { 0 } else { 1 };
        let total_rows  = total.div_ceil(cols);
        let visible_rows = list_h.div_ceil(GRID_CELL_H).max(1);
        let max_scroll  = total_rows.saturating_sub(visible_rows);
        let scroll = {
            let mut navs = HEPFS_NAVS.lock();
            match navs.iter_mut().find(|(id, _)| *id == win_id) {
                Some((_, n)) => { n.scroll = n.scroll.min(max_scroll); n.scroll }
                None => 0,
            }
        };

        let mut idx = 0usize;
        let mut draw_grid_cell = |display: &mut framebuffer::Display, is_dir: bool, name: &str, size: Option<u64>| {
            let row = idx / cols;
            // Skip rows scrolled above the visible window entirely.
            if row < scroll { idx += 1; return; }
            let visual_row = row - scroll;
            let col = idx % cols;
            let cy = list_top + visual_row * GRID_CELL_H;
            if cy + GRID_CELL_H <= list_top + list_h {
                let cx = right_x + col * GRID_CELL_W;
                if is_highlighted(4, idx) {
                    display.fill_rect(cx + 2, cy + 1, GRID_CELL_W.saturating_sub(4), GRID_CELL_H - 3, hover);
                }
                let icon_x = cx + (GRID_CELL_W.saturating_sub(GRID_ICON)) / 2;
                icons::draw_file_icon(display, icon_x, cy + 4, GRID_ICON, is_dir, name);
                // Truncate to what actually fits in the cell (9px/char at
                // scale 1) — the previous cap (9 chars, 81px) overflowed
                // `GRID_CELL_W` (72px) and ran into the neighboring cell's
                // label. `GRID_LABEL_MAX_CHARS - 1` leaves room for the "…".
                let label = if name.chars().count() > GRID_LABEL_MAX_CHARS {
                    let head: alloc::string::String = name.chars().take(GRID_LABEL_MAX_CHARS - 1).collect();
                    alloc::format!("{}…", head)
                } else {
                    alloc::string::String::from(name)
                };
                let label_x = cx + (GRID_CELL_W.saturating_sub(label.chars().count() * 9)) / 2;
                let col_c = if is_dir { dir_col } else { text };
                display.draw_text(label_x, cy + GRID_ICON + 6, &label, col_c, 1);
                if let Some(sz) = size {
                    let s = fmt_size(sz);
                    let chars = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                    let sx = cx + (GRID_CELL_W.saturating_sub(chars * 9)) / 2;
                    display.draw_text(sx, cy + GRID_ICON + 16, core::str::from_utf8(&s[..chars]).unwrap_or(""), dim, 1);
                }
            }
            idx += 1;
        };
        if !at_root {
            draw_grid_cell(display, true, "..", None);
        }
        for (ino, name) in &entries {
            let inode = hepfs::read_inode(ctrl, *ino);
            let is_dir = inode.flags == hepfs::F_DIR;
            draw_grid_cell(display, is_dir, name, if is_dir { None } else { Some(inode.size) });
        }
        if entries.is_empty() && at_root {
            display.draw_text(right_x + 4, list_top + 4, "(empty)", dim, 1);
        }

        // Scrollbar: only drawn when content overflows the visible area —
        // a thin track along the pane's right edge with a thumb sized to
        // the visible/total ratio (`HepfsScrollDrag` below reads back this
        // same geometry to convert a drag position into a row offset).
        if max_scroll > 0 {
            let track_x = right_x + grid_w;
            display.fill_rect(track_x, list_top, HEPFS_SCROLLBAR_W, list_h, framebuffer::Color::from_hex(0x151525));
            let thumb_h = ((list_h * visible_rows) / total_rows).max(10).min(list_h);
            let thumb_y = list_top + ((list_h.saturating_sub(thumb_h)) * scroll) / max_scroll.max(1);
            display.fill_rect(track_x + 1, thumb_y, HEPFS_SCROLLBAR_W.saturating_sub(2), thumb_h, acc);
        }
    } else {
        display.draw_text(wx + 4, list_top + 4, "No NVMe", dim, 1);
    }
}

/// Everything `render_hepfs_window()`'s scrollbar needs to convert a click/
/// drag position back into a row offset — kept alongside `grid_idx_at()` so
/// the two stay consistent (same `total_rows`/`visible_rows` derivation).
/// Returns `None` if the pane doesn't currently need to scroll at all.
fn hepfs_scroll_max(win_id: usize, right_w: usize, list_h: usize) -> Option<usize> {
    let cur_ino = HEPFS_NAVS.lock().iter().find(|(id, _)| *id == win_id).map(|(_, n)| n.ino)?;
    let at_root = cur_ino == hepfs::ROOT_INO;
    let mut ctrl = nvme::CONTROLLER.lock();
    let ctrl = ctrl.as_mut()?;
    let ctrl = &mut hepfs::BlockDev::Nvme(ctrl);
    let total = hepfs::list_dir(ctrl, cur_ino).len() + if at_root { 0 } else { 1 };
    let cols = grid_cols(right_w);
    let total_rows   = total.div_ceil(cols);
    let visible_rows = list_h.div_ceil(GRID_CELL_H).max(1);
    let max_scroll = total_rows.saturating_sub(visible_rows);
    if max_scroll == 0 { None } else { Some(max_scroll) }
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

fn render_sysmon_window(display: &mut framebuffer::Display, wx: usize, wy: usize, ww: usize, wh: usize) {
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

fn render_welcome_window(display: &mut framebuffer::Display, wx: usize, wy: usize, ww: usize, wh: usize) {
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

/// Generate a small uncompressed 24-bit BMP (checkerboard, HepOS accent colors)
/// so `view /demo.bmp` has something to show without needing a way to import
/// host image files into HepFS.
fn make_demo_bmp() -> alloc::vec::Vec<u8> {
    const W: usize = 64;
    const H: usize = 64;
    const SQ: usize = 8; // checker square size
    let row_stride = W * 3; // 192, already a multiple of 4 — no row padding needed
    let pixel_offset = 54usize;
    let file_size = pixel_offset + row_stride * H;

    let mut buf = alloc::vec![0u8; file_size];
    // BITMAPFILEHEADER
    buf[0] = b'B'; buf[1] = b'M';
    buf[2..6].copy_from_slice(&(file_size as u32).to_le_bytes());
    buf[10..14].copy_from_slice(&(pixel_offset as u32).to_le_bytes());
    // BITMAPINFOHEADER
    buf[14..18].copy_from_slice(&40u32.to_le_bytes());       // header size
    buf[18..22].copy_from_slice(&(W as i32).to_le_bytes());
    buf[22..26].copy_from_slice(&(H as i32).to_le_bytes());  // positive = bottom-up
    buf[26..28].copy_from_slice(&1u16.to_le_bytes());        // planes
    buf[28..30].copy_from_slice(&24u16.to_le_bytes());       // bpp
    // compression (30..34) = 0 = BI_RGB — already zeroed

    let (ar, ag, ab) = (0x6Cu8, 0x8Eu8, 0xFFu8); // accent blue
    let (br, bg, bb) = (0x14u8, 0x14u8, 0x1Eu8); // near-black

    for row in 0..H {
        for col in 0..W {
            let checker = ((row / SQ) + (col / SQ)) % 2 == 0;
            let (r, g, b) = if checker { (ar, ag, ab) } else { (br, bg, bb) };
            let px = pixel_offset + row * row_stride + col * 3;
            buf[px]     = b; // BMP stores BGR
            buf[px + 1] = g;
            buf[px + 2] = r;
        }
    }
    buf
}

/// Generate a short 16-bit PCM WAV (440Hz square wave, 0.5s, 48kHz stereo)
/// so `play /demo.wav` has something to play without needing a way to
/// import host audio files into HepFS.
fn make_demo_wav() -> alloc::vec::Vec<u8> {
    const SAMPLE_RATE: u32 = 48_000;
    const FREQ_HZ:     u32 = 440;
    const DURATION_MS: u32 = 500;

    let total_samples = (SAMPLE_RATE * DURATION_MS / 1000) as usize; // per channel
    let data_bytes = total_samples * 2 /* channels */ * 2 /* bytes/sample */;
    let file_size  = 36 + data_bytes; // RIFF size field = everything after itself

    let mut buf = alloc::vec![0u8; 44 + data_bytes];
    buf[0..4].copy_from_slice(b"RIFF");
    buf[4..8].copy_from_slice(&(file_size as u32).to_le_bytes());
    buf[8..12].copy_from_slice(b"WAVE");
    buf[12..16].copy_from_slice(b"fmt ");
    buf[16..20].copy_from_slice(&16u32.to_le_bytes());   // fmt chunk size
    buf[20..22].copy_from_slice(&1u16.to_le_bytes());    // PCM
    buf[22..24].copy_from_slice(&2u16.to_le_bytes());    // channels
    buf[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    buf[28..32].copy_from_slice(&(SAMPLE_RATE * 4).to_le_bytes()); // byte rate
    buf[32..34].copy_from_slice(&4u16.to_le_bytes());    // block align (stereo * 2 bytes)
    buf[34..36].copy_from_slice(&16u16.to_le_bytes());   // bits per sample
    buf[36..40].copy_from_slice(b"data");
    buf[40..44].copy_from_slice(&(data_bytes as u32).to_le_bytes());

    let period_samp = SAMPLE_RATE / FREQ_HZ;
    let half = period_samp / 2;
    for i in 0..total_samples {
        let val: i16 = if (i as u32) % period_samp < half { 0x4000 } else { -0x4000 };
        let off = 44 + i * 4;
        buf[off..off+2].copy_from_slice(&val.to_le_bytes());   // left
        buf[off+2..off+4].copy_from_slice(&val.to_le_bytes()); // right
    }
    buf
}

/// Sidebar rows, top to bottom — shared between rendering and click hit-testing
/// in main.rs's Settings click handler so the two never drift out of sync.
const SETTINGS_SIDEBAR: &[(&str, u8)] = &[
    ("Background", desktop::SETTINGS_PAGE_BACKGROUND),
    ("Sound",      desktop::SETTINGS_PAGE_SOUND),
];
const SETTINGS_SIDEBAR_ROW_H: usize = 22;
const SETTINGS_SIDEBAR_TOP:   usize = 30;

// Volume slider geometry, relative to the Sound panel's origin (px, wy) —
// shared between rendering and the click/drag hit-test in main.rs's mouse loop.
const VOL_SLIDER_X: usize = 12;
const VOL_SLIDER_Y: usize = 56;
const VOL_SLIDER_W: usize = 220;
const VOL_SLIDER_H: usize = 10;

fn render_settings_sound(
    display: &mut framebuffer::Display, px: usize, wy: usize, pw: usize,
    acc: framebuffer::Color, text: framebuffer::Color, dim: framebuffer::Color,
) {
    use framebuffer::Color;
    display.draw_text(px + 12, wy + 10, "Sound", acc, 1);
    display.draw_text(px + 12, wy + 24, "Adjust output volume", dim, 1);
    display.fill_rect(px, wy + 38, pw, 1, Color::from_hex(0x2A2A50));

    let vol = if hda::is_available() { hda::get_volume() } else { 0 };
    let label = alloc::format!("Volume: {}{}", vol, if hda::is_available() { "" } else { " (HDA unavailable)" });
    display.draw_text(px + VOL_SLIDER_X, wy + VOL_SLIDER_Y - 14, &label, text, 1);

    let sx = px + VOL_SLIDER_X;
    let sy = wy + VOL_SLIDER_Y;
    display.fill_rect(sx, sy, VOL_SLIDER_W, VOL_SLIDER_H, Color::from_hex(0x222244));
    let fill_w = (VOL_SLIDER_W * vol as usize) / 100;
    if fill_w > 0 { display.fill_rect(sx, sy, fill_w, VOL_SLIDER_H, acc); }
    // Handle
    let hx = sx + fill_w.min(VOL_SLIDER_W.saturating_sub(4));
    display.fill_rect(hx, sy.saturating_sub(2), 4, VOL_SLIDER_H + 4, Color::from_hex(0xFFFFFF));
}

fn render_settings_window(display: &mut framebuffer::Display, wx: usize, wy: usize, ww: usize, wh: usize) {
    use framebuffer::Color;
    let bg       = Color::from_hex(0x0C0C0C);
    let sidebar  = Color::from_hex(0x111122);
    let acc      = Color::from_hex(0x6C8EFF);
    let text     = Color::from_hex(0xE8E8E8);
    let dim      = Color::from_hex(0x666688);
    let sel_bg   = Color::from_hex(0x1E1E40);
    let cur_wp   = desktop::WALLPAPER.load(core::sync::atomic::Ordering::Relaxed);
    let page     = desktop::SETTINGS_PAGE.load(core::sync::atomic::Ordering::Relaxed);

    // Content background
    display.fill_rect(wx, wy, ww, wh, bg);

    // ── Left sidebar ─────────────────────────────────────────────────────────
    const SB_W: usize = 110;
    display.fill_rect(wx, wy, SB_W, wh, sidebar);
    display.fill_rect(wx + SB_W, wy, 1, wh, Color::from_hex(0x2A2A50)); // divider

    // "Settings" header in sidebar
    display.draw_text(wx + 8, wy + 10, "Settings", acc, 1);
    display.fill_rect(wx, wy + 24, SB_W, 1, Color::from_hex(0x2A2A50));

    for (i, &(label, id)) in SETTINGS_SIDEBAR.iter().enumerate() {
        let ry = wy + SETTINGS_SIDEBAR_TOP + i * SETTINGS_SIDEBAR_ROW_H;
        if id == page {
            display.fill_rect(wx + 2, ry, SB_W - 4, SETTINGS_SIDEBAR_ROW_H, sel_bg);
            display.fill_rect(wx, ry, 3, SETTINGS_SIDEBAR_ROW_H, acc); // accent left bar
        }
        display.draw_text(wx + 10, ry + 7, label, if id == page { text } else { dim }, 1);
    }

    // ── Right panel ──────────────────────────────────────────────────────────
    let px = wx + SB_W + 1; // panel left edge
    let pw = ww.saturating_sub(SB_W + 1);

    if page == desktop::SETTINGS_PAGE_SOUND {
        render_settings_sound(display, px, wy, pw, acc, text, dim);
        return;
    }

    display.draw_text(px + 12, wy + 10, "Background", acc, 1);
    display.draw_text(px + 12, wy + 24, "Choose your desktop background", dim, 1);
    display.fill_rect(px, wy + 38, pw, 1, Color::from_hex(0x2A2A50));

    // Wallpaper thumbnails — two side by side
    const TW: usize = 120;  // thumb width
    const TH: usize = 80;   // thumb height
    const TGAP: usize = 16;
    let ty0 = wy + 50;

    let thumbs: &[(&str, u8)] = &[
        ("Dark Space", desktop::WP_DARK),
        ("Bliss",      desktop::WP_BLISS),
    ];

    for (i, &(label, wp_id)) in thumbs.iter().enumerate() {
        let tx = px + 12 + i * (TW + TGAP);

        // Thumb border — accent if selected
        let border = if cur_wp == wp_id { acc } else { Color::from_hex(0x333355) };
        display.fill_rect(tx.saturating_sub(2), ty0.saturating_sub(2), TW + 4, TH + 4, border);

        // Thumbnail preview
        if wp_id == desktop::WP_DARK {
            // Mini dark gradient + a couple of stars
            for dy in 0..TH {
                let t = dy as i32; let n = TH as i32;
                let r = ((0x0D*(n-t) + 0x07*t) / n) as u8;
                let g = ((0x1F*(n-t) + 0x07*t) / n) as u8;
                let b = ((0x40*(n-t) + 0x10*t) / n) as u8;
                display.fill_rect(tx, ty0 + dy, TW, 1, Color { r, g, b });
            }
            // Dot stars
            for &(sx, sy) in &[(10usize,8usize),(40,15),(80,5),(105,22),(55,35),(20,50),(90,60)] {
                if sx < TW && sy < TH {
                    display.put_pixel_pub(tx + sx, ty0 + sy, Color { r:180, g:180, b:220 });
                }
            }
        } else {
            // Mini Bliss: blue sky top half, green bottom half
            let sky_h = TH * 55 / 100;
            let (str_, stg, stb) = (0x27_i32, 0x76_i32, 0xD0_i32);
            let (shr, shg, shb) = (0x8B_i32, 0xCE_i32, 0xF4_i32);
            for dy in 0..sky_h {
                let t = dy as i32; let n = sky_h as i32;
                let r = ((str_*(n-t) + shr*t) / n.max(1)) as u8;
                let g = ((stg*(n-t)  + shg*t) / n.max(1)) as u8;
                let b = ((stb*(n-t)  + shb*t) / n.max(1)) as u8;
                display.fill_rect(tx, ty0 + dy, TW, 1, Color { r, g, b });
            }
            // Green hills at bottom
            display.fill_rect(tx, ty0 + sky_h, TW, TH - sky_h, Color::from_hex(0x5A8C32));
            // Far hill bump
            let bx = tx + TW / 4; let by_h = sky_h.saturating_sub(6);
            display.fill_rect(bx, ty0 + by_h, TW / 2, 7, Color::from_hex(0x6BA239));
        }

        // Label below thumbnail
        let lx = tx + TW / 2 - label.len() * 4;
        display.draw_text(lx, ty0 + TH + 6, label, if cur_wp == wp_id { acc } else { dim }, 1);
    }
}

