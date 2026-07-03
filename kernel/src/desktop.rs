//! Desktop WM — compositor, window manager, taskbar.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use crate::framebuffer::{Color, Display};

// ── Palette ─────────────────────────────────────────────────────────────────
pub mod pal {
    use crate::framebuffer::Color;
    pub const BG:          Color = Color::from_hex(0x0D0D0D);
    pub const ACCENT:      Color = Color::from_hex(0x6C8EFF);
    pub const WIN_BG:      Color = Color::from_hex(0x141414);
    pub const WIN_TITLE:   Color = Color::from_hex(0x1A1A2E);
    pub const WIN_TITLE_A: Color = Color::from_hex(0x252550);
    pub const TEXT:        Color = Color::from_hex(0xE8E8E8);
    pub const TEXT_DIM:    Color = Color::from_hex(0x888888);
    pub const TASKBAR:     Color = Color::from_hex(0x0A0A14);
    pub const TASKBAR_BTN: Color = Color::from_hex(0x1A1A30);
    pub const TASKBAR_ACT: Color = Color::from_hex(0x2A2A50);
    pub const CLOSE_BTN:   Color = Color::from_hex(0x8B1A1A);
    pub const CURSOR:      Color = Color::from_hex(0xFFFFFF);
    pub const BORDER:      Color = Color::from_hex(0x333355);
    pub const BORDER_ACT:  Color = Color::from_hex(0x6C8EFF);
    pub const START_BTN:   Color = Color::from_hex(0x203060);
    pub const MENU_BG:     Color = Color::from_hex(0x12122A);
    pub const MENU_HOVER:  Color = Color::from_hex(0x1E1E40);
    pub const MENU_BORDER: Color = Color::from_hex(0x4040A0);
}

pub const TITLE_H:    usize = 22;
pub const TASKBAR_H:  usize = 32;

#[derive(Clone, Copy, PartialEq)]
pub enum SnapZone { Left, Right, Top }

#[derive(Clone, Copy, PartialEq)]
pub enum CursorType { Normal, ResizeEW, ResizeNS, ResizeNWSE, ResizeNESW }
pub const BORDER_W:   usize = 1;
const START_W:        usize = 68; // width of the "HepOS" start button
const TASK_BTN_W:     usize = 120;
const MENU_ENTRY_H:   usize = 26;
const MENU_W:         usize = 160;

// ── Window ───────────────────────────────────────────────────────────────────
pub struct Window {
    pub id:          usize,
    pub title:       String,
    pub x:           i32,
    pub y:           i32,
    pub w:           usize,
    pub h:           usize,
    pub minimized:   bool,
    pub maximized:   bool,
    pub is_terminal: bool,
    saved_x:         i32,
    saved_y:         i32,
    saved_w:         usize,
    saved_h:         usize,
    drag_off_x:      i32,
    drag_off_y:      i32,
    pub dragging:    bool,
    pub resizing:    bool,
    resize_edge_flags: u8,  // bits: 0=right, 1=bottom, 2=left
    resize_orig_x:   i32,
    resize_orig_y:   i32,
    resize_orig_w:   usize,
    resize_orig_h:   usize,
    resize_orig_mx:  i32,
    resize_orig_my:  i32,
}

impl Window {
    pub fn new(id: usize, title: &str, x: i32, y: i32, w: usize, h: usize) -> Self {
        Window {
            id, title: String::from(title),
            x, y, w, h, minimized: false, maximized: false, is_terminal: false,
            saved_x: x, saved_y: y, saved_w: w, saved_h: h,
            drag_off_x: 0, drag_off_y: 0, dragging: false,
            resizing: false, resize_edge_flags: 0,
            resize_orig_x: x, resize_orig_y: y,
            resize_orig_w: w, resize_orig_h: h,
            resize_orig_mx: 0, resize_orig_my: 0,
        }
    }

    pub fn toggle_maximize(&mut self, sw: usize, sh: usize) {
        if self.maximized {
            self.x = self.saved_x; self.y = self.saved_y;
            self.w = self.saved_w; self.h = self.saved_h;
            self.maximized = false;
        } else {
            self.saved_x = self.x; self.saved_y = self.y;
            self.saved_w = self.w; self.saved_h = self.h;
            self.x = BORDER_W as i32;
            self.y = TITLE_H as i32 + BORDER_W as i32;
            self.w = sw.saturating_sub(BORDER_W * 2);
            self.h = sh.saturating_sub(TITLE_H + TASKBAR_H + BORDER_W * 2);
            self.maximized = true;
        }
    }

    pub fn maximize_hit(&self, mx: i32, my: i32) -> bool {
        let tx = self.x.max(0);
        let ty = self.outer_y() + BORDER_W as i32;
        let bx = tx + self.w as i32 - 34;
        let by = ty + 4;
        mx >= bx && mx < bx + 14 && my >= by && my < by + 14
    }

    pub fn outer_x(&self) -> i32 { self.x - BORDER_W as i32 }
    pub fn outer_y(&self) -> i32 { self.y - TITLE_H as i32 - BORDER_W as i32 }
    pub fn outer_w(&self) -> usize { self.w + BORDER_W * 2 }
    pub fn outer_h(&self) -> usize { self.h + TITLE_H + BORDER_W * 2 }

    pub fn title_hit(&self, mx: i32, my: i32) -> bool {
        mx >= self.outer_x() && mx < self.outer_x() + self.outer_w() as i32
            && my >= self.outer_y() && my < self.y
    }
    pub fn close_hit(&self, mx: i32, my: i32) -> bool {
        let cx = self.outer_x() + self.outer_w() as i32 - 18;
        let cy = self.outer_y() + 4;
        mx >= cx && mx < cx + 14 && my >= cy && my < cy + 14
    }
    pub fn content_hit(&self, mx: i32, my: i32) -> bool {
        mx >= self.x && mx < self.x + self.w as i32
            && my >= self.y && my < self.y + self.h as i32
    }
    /// Returns edge-resize flags for the cursor position.
    /// Bit 0 = right, bit 1 = bottom, bit 2 = left.  0 = not on any resize zone.
    pub fn resize_edges_at(&self, mx: i32, my: i32) -> u8 {
        if self.maximized { return 0; }
        const M: i32 = 6;
        let cx0 = self.x;
        let cx1 = self.x + self.w as i32;
        let cy0 = self.y;
        let cy1 = self.y + self.h as i32;
        let ox  = self.outer_x();
        let ow  = self.outer_w() as i32;
        if mx < ox - M || mx >= ox + ow + M { return 0; }
        if my < cy0 || my >= cy1 + M        { return 0; }
        let mut f = 0u8;
        if mx >= cx1 - M && mx < cx1 + M && my >= cy0 { f |= 1; } // right
        if my >= cy1 - M && my < cy1 + M                { f |= 2; } // bottom
        if mx >= ox  - M && mx < ox  + M && my >= cy0  { f |= 4; } // left
        f
    }

    /// Kept for compatibility — calls resize_edges_at and checks any edge.
    pub fn resize_hit(&self, mx: i32, my: i32) -> bool { self.resize_edges_at(mx, my) != 0 }

    /// Hit-test for the "+" spawn-terminal button on the left of the title bar.
    /// Only returns true if is_terminal == true.
    pub fn newterm_hit(&self, mx: i32, my: i32) -> bool {
        if !self.is_terminal { return false; }
        let tx = self.x.max(0);
        let ty = self.outer_y() + BORDER_W as i32;
        mx >= tx + 4 && mx < tx + 18 && my >= ty + 4 && my < ty + 18
    }
}

// ── Desktop ──────────────────────────────────────────────────────────────────
pub struct Desktop {
    pub windows:          Vec<Window>,
    pub focused:          Option<usize>,
    next_id:              usize,
    pub fb_w:             usize,
    pub fb_h:             usize,
    prev_btn:             u8,
    pub dirty:            bool,
    pub mouse_dirty:      bool,
    pub prev_cx:          i32,
    pub prev_cy:          i32,
    pub start_menu_open:  bool,
    dbl_click_pending:    Option<usize>,  // Some(id) = waiting for 2nd click on same title bar
    pub snap_zone:        Option<SnapZone>, // active snap target while dragging
}

impl Desktop {
    pub fn new(fb_w: usize, fb_h: usize) -> Self {
        Desktop {
            windows: Vec::new(), focused: None, next_id: 0,
            fb_w, fb_h, prev_btn: 0, dirty: true, mouse_dirty: false,
            prev_cx: 0, prev_cy: 0, start_menu_open: false,
            dbl_click_pending: None, snap_zone: None,
        }
    }

    pub fn add_window(&mut self, title: &str, x: i32, y: i32, w: usize, h: usize) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window::new(id, title, x, y, w, h));
        self.focused = Some(id);
        id
    }

    pub fn set_terminal_window(&mut self, id: usize) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.is_terminal = true;
        }
    }

    /// Which cursor shape to show at (mx, my).
    pub fn cursor_type_at(&self, mx: i32, my: i32) -> CursorType {
        // If a window is currently being resized, keep that cursor
        for w in &self.windows {
            if w.resizing {
                return Self::cursor_for_flags(w.resize_edge_flags);
            }
        }
        // Check hover over resize edges (top-to-bottom z-order, so last = topmost)
        for w in self.windows.iter().rev() {
            if w.minimized || w.maximized { continue; }
            let f = w.resize_edges_at(mx, my);
            if f != 0 { return Self::cursor_for_flags(f); }
        }
        CursorType::Normal
    }

    fn cursor_for_flags(f: u8) -> CursorType {
        match f {
            1 | 4 => CursorType::ResizeEW,    // right or left only
            2     => CursorType::ResizeNS,     // bottom only
            3     => CursorType::ResizeNWSE,   // bottom-right corner
            6     => CursorType::ResizeNESW,   // bottom-left corner
            _     => CursorType::ResizeNS,
        }
    }

    pub fn bring_to_front(&mut self, id: usize) {
        if let Some(pos) = self.windows.iter().position(|w| w.id == id) {
            let win = self.windows.remove(pos);
            self.windows.push(win);
            self.focused = Some(id);
        }
    }

    pub fn update_mouse(&mut self, mx: i32, my: i32, buttons: u8) {
        if mx != self.prev_cx || my != self.prev_cy {
            self.mouse_dirty = true;  // cursor moved — don't force a full scene redraw
            self.prev_cx = mx;
            self.prev_cy = my;
        }
        let clicked  = buttons & 0x01 != 0 && self.prev_btn & 0x01 == 0;
        let released = buttons & 0x01 == 0 && self.prev_btn & 0x01 != 0;
        let held     = buttons & 0x01 != 0;
        self.prev_btn = buttons;

        // Drag or resize
        if held {
            if let Some(fid) = self.focused {
                if let Some(win) = self.windows.iter_mut().find(|w| w.id == fid) {
                    if win.resizing {
                        let dmx = mx - win.resize_orig_mx;
                        let dmy = my - win.resize_orig_my;
                        if win.resize_edge_flags & 1 != 0 { // right
                            win.w = (win.resize_orig_w as i32 + dmx).max(120) as usize;
                        }
                        if win.resize_edge_flags & 2 != 0 { // bottom
                            win.h = (win.resize_orig_h as i32 + dmy).max(60) as usize;
                        }
                        if win.resize_edge_flags & 4 != 0 { // left
                            let new_w = (win.resize_orig_w as i32 - dmx).max(120);
                            win.x = win.resize_orig_x + (win.resize_orig_w as i32 - new_w);
                            win.w = new_w as usize;
                        }
                        self.dirty = true;
                        return;
                    }
                    if win.dragging && !win.maximized {
                        win.x = (mx - win.drag_off_x).max(0).min(self.fb_w as i32 - win.w as i32);
                        win.y = (my - win.drag_off_y).max(TITLE_H as i32).min(self.fb_h as i32 - TASKBAR_H as i32 - 1);
                        // Track snap zone based on cursor position
                        self.snap_zone = if mx <= 4 { Some(SnapZone::Left) }
                                         else if mx >= self.fb_w as i32 - 4 { Some(SnapZone::Right) }
                                         else if my <= 4 { Some(SnapZone::Top) }
                                         else { None };
                        self.dirty = true;
                        return;
                    }
                }
            }
        }
        if released {
            // Apply snap if a window was being dragged into a snap zone
            if let Some(zone) = self.snap_zone.take() {
                if let Some(fid) = self.focused {
                    if let Some(win) = self.windows.iter_mut().find(|w| w.id == fid && w.dragging) {
                        let sw = self.fb_w; let sh = self.fb_h;
                        let bx = BORDER_W as i32;
                        let by = TITLE_H as i32 + BORDER_W as i32;
                        let hw = sw / 2;
                        let fh = sh.saturating_sub(TITLE_H + TASKBAR_H + BORDER_W * 2);
                        match zone {
                            SnapZone::Left  => { win.x = bx; win.y = by;
                                                 win.w = hw.saturating_sub(BORDER_W * 2); win.h = fh; }
                            SnapZone::Right => { win.x = bx + hw as i32; win.y = by;
                                                 win.w = hw.saturating_sub(BORDER_W * 2); win.h = fh; }
                            SnapZone::Top   => { win.toggle_maximize(sw, sh); }
                        }
                        self.dirty = true;
                    }
                }
            }
            for win in &mut self.windows {
                win.dragging = false;
                win.resizing = false;
            }
        }

        if !clicked { return; }

        // Double-click tracking: clear pending unless we land on the same title bar again
        let prev_dbl = self.dbl_click_pending.take();

        let in_taskbar = my >= self.fb_h as i32 - TASKBAR_H as i32;
        let menu_h     = self.windows.len() * MENU_ENTRY_H + 8;
        let menu_top   = (self.fb_h - TASKBAR_H).saturating_sub(menu_h);
        let in_menu    = self.start_menu_open
            && mx >= 0 && (mx as usize) < MENU_W
            && my >= menu_top as i32 && my < self.fb_h as i32 - TASKBAR_H as i32;

        // Start-menu item click
        if in_menu {
            let entry = (my as usize - menu_top) / MENU_ENTRY_H;
            if entry < self.windows.len() {
                let id = self.windows[entry].id;
                if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
                    w.minimized = false;
                }
                self.bring_to_front(id);
                self.start_menu_open = false;
                self.dirty = true;
            }
            return;
        }

        // Taskbar click
        if in_taskbar {
            if (mx as usize) < START_W {
                // Start button
                self.start_menu_open = !self.start_menu_open;
                self.dirty = true;
            } else {
                // Window button — find the N-th visible (non-minimized) window
                self.start_menu_open = false;
                let btn_x = mx as usize - START_W;
                let slot  = btn_x / TASK_BTN_W;
                let ids: Vec<usize> = self.windows.iter()
                    .filter(|w| !w.minimized)
                    .map(|w| w.id)
                    .collect();
                if let Some(&wid) = ids.get(slot) {
                    if self.focused == Some(wid) {
                        // Click focused window button = minimize it
                        if let Some(w) = self.windows.iter_mut().find(|w| w.id == wid) {
                            w.minimized = true;
                        }
                        self.focused = None;
                    } else {
                        self.bring_to_front(wid);
                    }
                    self.dirty = true;
                }
            }
            return;
        }

        // Desktop / window click — close start menu, hit-test windows top-to-bottom
        self.start_menu_open = false;
        let mut hit_id = None;
        for win in self.windows.iter().rev() {
            if win.minimized { continue; }
            if win.close_hit(mx, my) || win.maximize_hit(mx, my) || win.newterm_hit(mx, my)
                || win.title_hit(mx, my) || win.resize_hit(mx, my) || win.content_hit(mx, my)
            {
                hit_id = Some(win.id);
                break;
            }
        }
        if let Some(id) = hit_id {
            self.bring_to_front(id);
            self.dirty = true;
            let win = self.windows.iter_mut().find(|w| w.id == id).unwrap();
            if win.close_hit(mx, my) {
                win.minimized = true;
                self.focused  = None;
            } else if win.newterm_hit(mx, my) {
                crate::terminal::spawn_terminal();
            } else if win.maximize_hit(mx, my) {
                let sw = self.fb_w; let sh = self.fb_h;
                win.toggle_maximize(sw, sh);
            } else if !win.maximized {
                let edges = win.resize_edges_at(mx, my);
                if edges != 0 {
                    win.resizing          = true;
                    win.resize_edge_flags = edges;
                    win.resize_orig_x     = win.x;
                    win.resize_orig_y     = win.y;
                    win.resize_orig_w     = win.w;
                    win.resize_orig_h     = win.h;
                    win.resize_orig_mx    = mx;
                    win.resize_orig_my    = my;
                } else if win.title_hit(mx, my) {
                    if prev_dbl == Some(id) {
                        // Double-click on title bar → toggle maximize
                        let sw = self.fb_w; let sh = self.fb_h;
                        win.toggle_maximize(sw, sh);
                    } else {
                        self.dbl_click_pending = Some(id);
                        win.dragging   = true;
                        win.drag_off_x = mx - win.x;
                        win.drag_off_y = my - win.y;
                    }
                }
            }
        }
    }

    /// Returns the bounding rect of the snap-target zone, for drawing a preview overlay.
    pub fn snap_preview_rect(&self) -> Option<(i32, i32, usize, usize)> {
        let zone = self.snap_zone?;
        let sw = self.fb_w; let sh = self.fb_h;
        let bx = BORDER_W as i32;
        let by = TITLE_H as i32 + BORDER_W as i32;
        let hw = sw / 2;
        let fh = sh.saturating_sub(TITLE_H + TASKBAR_H + BORDER_W * 2);
        Some(match zone {
            SnapZone::Left  => (bx,             by, hw.saturating_sub(BORDER_W * 2), fh),
            SnapZone::Right => (bx + hw as i32, by, hw.saturating_sub(BORDER_W * 2), fh),
            SnapZone::Top   => (bx,             by, sw.saturating_sub(BORDER_W * 2), fh),
        })
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    /// Clear the desktop background. Windows and taskbar are rendered by the
    /// task_blink loop so that chrome + content render in correct z-order.
    pub fn render(&self, display: &mut Display, _cx: i32, _cy: i32) {
        display.clear(pal::BG);
    }

    /// Draw a single window's chrome (title bar, border, content-area background).
    /// Called per-window from the task_blink render loop in z-order.
    pub fn draw_window(&self, display: &mut Display, win: &Window, focused: bool) {
        let ox = win.outer_x();
        let oy = win.outer_y();
        let ow = win.outer_w();
        let oh = win.outer_h();

        // Border
        let border_col = if focused { pal::BORDER_ACT } else { pal::BORDER };
        if ox >= 0 && oy >= 0 {
            display.fill_rect(ox as usize, oy as usize, ow, oh, border_col);
        }

        // Title bar
        let title_col = if focused { pal::WIN_TITLE_A } else { pal::WIN_TITLE };
        let tx = (win.x as usize).max(0);
        let ty = (win.outer_y() + BORDER_W as i32).max(0) as usize;
        display.fill_rect(tx, ty, win.w, TITLE_H, title_col);

        // "+" new-terminal button (left side of title bar, terminal windows only)
        if win.is_terminal {
            display.fill_rect(tx + 4, ty + 4, 14, 14, pal::TASKBAR_BTN);
            display.draw_text(tx + 8, ty + 5, "+", pal::TEXT, 1);
            display.draw_text(tx + 22, ty + 5, &win.title, pal::TEXT, 1);
        } else {
            display.draw_text(tx + 6, ty + 5, &win.title, pal::TEXT, 1);
        }

        // Close button
        let close_x = tx + win.w.saturating_sub(18);
        display.fill_rect(close_x, ty + 4, 14, 14, pal::CLOSE_BTN);
        display.draw_text(close_x + 4, ty + 5, "x", pal::TEXT, 1);

        // Maximize / restore button (14×14, just left of close button)
        let max_x = close_x.saturating_sub(16);
        let max_bg = if win.maximized { pal::ACCENT } else { pal::TASKBAR_BTN };
        display.fill_rect(max_x, ty + 4, 14, 14, max_bg);
        // Hollow square icon inside the button
        display.fill_rect(max_x + 2, ty + 6, 10, 1, pal::TEXT);     // top
        display.fill_rect(max_x + 2, ty + 15, 10, 1, pal::TEXT);    // bottom
        display.fill_rect(max_x + 2, ty + 6, 1, 10, pal::TEXT);     // left
        display.fill_rect(max_x + 11, ty + 6, 1, 10, pal::TEXT);    // right

        // Content background
        display.fill_rect(win.x.max(0) as usize, win.y.max(0) as usize, win.w, win.h, pal::WIN_BG);
        display.fill_rect(win.x.max(0) as usize, win.y.max(0) as usize, win.w, 1, pal::BORDER);

        // Resize handle — only visible when not maximized
        if !win.maximized {
            let rx = (win.x + win.w as i32 - 2).max(0) as usize;
            let by = (win.y + win.h as i32 - 2).max(0) as usize;
            let hc = if focused { pal::BORDER_ACT } else { pal::BORDER };
            for d in 0..3usize {
                if rx >= d * 3 + 1 && by >= d + 1 {
                    display.fill_rect(rx - d * 3, by - d, 2, 1, hc);
                }
            }
        }
    }

    /// Draw the taskbar. Call this after all window content so it stays on top.
    pub fn draw_taskbar(&self, display: &mut Display) {
        let ty = self.fb_h - TASKBAR_H;
        display.fill_rect(0, ty, self.fb_w, TASKBAR_H, pal::TASKBAR);
        display.fill_rect(0, ty, self.fb_w, 1, pal::ACCENT);

        // Start button
        let start_active = self.start_menu_open;
        let sc = if start_active { pal::TASKBAR_ACT } else { pal::START_BTN };
        display.fill_rect(4, ty + 4, START_W - 8, TASKBAR_H - 8, sc);
        display.draw_text(10, ty + 10, "HepOS", pal::ACCENT, 1);

        // Separator
        display.fill_rect(START_W, ty + 4, 1, TASKBAR_H - 8, pal::BORDER);

        // Open (non-minimized) window buttons
        let mut bx = START_W + 4;
        for win in self.windows.iter().filter(|w| !w.minimized) {
            if bx + TASK_BTN_W > self.fb_w - 80 { break; }
            let active = self.focused == Some(win.id);
            let bc = if active { pal::TASKBAR_ACT } else { pal::TASKBAR_BTN };
            display.fill_rect(bx, ty + 4, TASK_BTN_W - 4, TASKBAR_H - 8, bc);
            // Active indicator bar
            if active {
                display.fill_rect(bx, ty + TASKBAR_H - 5, TASK_BTN_W - 4, 2, pal::ACCENT);
            }
            let label = if win.title.len() > 13 { &win.title[..13] } else { &win.title };
            display.draw_text(bx + 6, ty + 10, label, pal::TEXT, 1);
            bx += TASK_BTN_W;
        }

        // Clock (right-aligned)
        let mut tbuf = [0u8; 6];
        let time = crate::rtc::fmt_time(&mut tbuf);
        let tw   = time.len() * 9;
        display.draw_text(self.fb_w.saturating_sub(tw + 8), ty + 10, time, pal::TEXT, 1);
    }

    /// Draw the start menu popup. Call BEFORE draw_taskbar so taskbar renders on top.
    pub fn draw_start_menu(&self, display: &mut Display) {
        if !self.start_menu_open { return; }

        let menu_h  = self.windows.len() * MENU_ENTRY_H + 8;
        let menu_y  = (self.fb_h - TASKBAR_H).saturating_sub(menu_h);

        // Background + border
        display.fill_rect(0, menu_y, MENU_W, menu_h, pal::MENU_BG);
        display.fill_rect(0, menu_y, MENU_W, 1, pal::MENU_BORDER);
        display.fill_rect(MENU_W - 1, menu_y, 1, menu_h, pal::MENU_BORDER);

        // Header
        display.draw_text(8, menu_y + 4, "Programs", pal::ACCENT, 1);
        display.fill_rect(0, menu_y + 18, MENU_W, 1, pal::MENU_BORDER);

        // Entries — all windows, regardless of minimized state
        for (i, win) in self.windows.iter().enumerate() {
            let ey = menu_y + 8 + i * MENU_ENTRY_H;
            let active = self.focused == Some(win.id) && !win.minimized;
            if active {
                display.fill_rect(2, ey, MENU_W - 4, MENU_ENTRY_H - 2, pal::MENU_HOVER);
            }
            let label = if win.title.len() > 18 { &win.title[..18] } else { &win.title };
            let col = if active { pal::ACCENT } else { pal::TEXT };
            display.draw_text(10, ey + 6, label, col, 1);
            // Minimized badge
            if win.minimized {
                display.draw_text(MENU_W - 24, ey + 6, "--", pal::TEXT_DIM, 1);
            }
        }
    }
}

pub static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);
