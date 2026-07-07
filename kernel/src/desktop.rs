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

// ── Wallpaper ─────────────────────────────────────────────────────────────────
pub const WP_DARK:  u8 = 0;
pub const WP_BLISS: u8 = 1;
pub static WALLPAPER: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(WP_DARK);

// ── Settings sidebar page ─────────────────────────────────────────────────────
pub const SETTINGS_PAGE_BACKGROUND: u8 = 0;
pub const SETTINGS_PAGE_SOUND:      u8 = 1;
pub static SETTINGS_PAGE: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(SETTINGS_PAGE_BACKGROUND);

// ── Desktop icons ─────────────────────────────────────────────────────────────
// Each icon: 48×48 box + 8px gap + 8px text label = 64px slot, 80px stride.
const ICON_SIZE:   usize = 48;
const ICON_STRIDE: usize = 80; // vertical spacing between icon tops
const ICON_X:      usize = 16; // left edge

// ── App kinds — used to group taskbar/start-menu entries and to know what
// "spawn another instance" means for a given program. ──────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppKind { Welcome, Files, Terminal, Editor, Sysmon, Settings, ImageViewer, AudioPlayer }

pub fn app_label(kind: AppKind) -> &'static str {
    match kind {
        AppKind::Welcome     => "Welcome",
        AppKind::Files       => "Files",
        AppKind::Terminal    => "Terminal",
        AppKind::Editor      => "Editor",
        AppKind::Sysmon      => "Sysmon",
        AppKind::Settings    => "Settings",
        AppKind::ImageViewer => "Image Viewer",
        AppKind::AudioPlayer => "Audio Player",
    }
}

/// All app kinds support spawning another independent instance. Files/HepFS
/// used to be excluded (its nav state was a single global), but each Files
/// window now gets its own navigator entry, so it works like everything else.
pub fn app_supports_new_window(_kind: AppKind) -> bool { true }

struct IconDef { win_id: usize, label: &'static str, color: Color }
const ICONS: &[IconDef] = &[
    IconDef { win_id: 0, label: "Welcome",  color: Color::from_hex(0xE8A020) },
    IconDef { win_id: 1, label: "Files",    color: Color::from_hex(0x3A8FD4) },
    IconDef { win_id: 2, label: "Terminal", color: Color::from_hex(0x2EA84A) },
    IconDef { win_id: 3, label: "Editor",   color: Color::from_hex(0x9B5CE5) },
    IconDef { win_id: 4, label: "Sysmon",   color: Color::from_hex(0x20B8B0) },
    IconDef { win_id: 5, label: "Settings", color: Color::from_hex(0x888888) },
];

fn icon_rect(slot: usize) -> (i32, i32, usize, usize) {
    (ICON_X as i32, (16 + slot * ICON_STRIDE) as i32, ICON_SIZE, ICON_SIZE)
}

#[derive(Clone, Copy, PartialEq)]
pub enum SnapZone { Left, Right, Top }

#[derive(Clone, Copy, PartialEq)]
pub enum CursorType { Normal, ResizeEW, ResizeNS, ResizeNWSE, ResizeNESW }
pub const BORDER_W:   usize = 1;
const START_W:        usize = 68; // width of the "HepOS" start button
const TASK_BTN_W:     usize = 120;
const MENU_ENTRY_H:   usize = 26;
const MENU_W:         usize = 160;

// ── Window open/close animation ───────────────────────────────────────────────
// 180ms ease-out scale, centered on the window's own position. `Opening` runs
// on creation and on every unminimize; `Closing` runs on minimize/close and
// keeps the window rendering (at a shrinking scale) until it finishes, at
// which point `Desktop::tick_anims()` actually sets `minimized = true`.
//
// Timed via TSC rather than `scheduler::TICK_COUNT` (frozen after the first
// tick — see PLAN.md Known Issues). TSC_PER_MS is calibrated fairly early in
// boot (during `hda::init()`), but windows 0-7 are created just *before*
// that — their very first Opening anim will see `TSC_PER_MS == 0`, which
// `.max(1)` turns into a huge elapsed-ms value, so `progress()` reads as
// "already done" and they simply appear at full size with no animation.
// Harmless: only the boot-created windows are affected, and every animation
// after boot (new windows, unminimize, close) times correctly.
const ANIM_MS: u64 = 180;
const MENU_ANIM_MS: u64 = 150;

#[derive(Clone, Copy, PartialEq)]
enum AnimKind {
    Opening,
    Closing,
    /// Maximize/restore/snap — lerps from a stored old rect to the window's
    /// current real (x, y, w, h), rather than scaling from/to a phantom
    /// smaller rect centered on the window (that's Opening/Closing's job).
    Transition { from: (i32, i32, usize, usize) },
}

#[derive(Clone, Copy)]
struct WindowAnim { kind: AnimKind, start_tsc: u64 }

impl WindowAnim {
    fn progress(&self) -> f32 {
        let tsc_per_ms = crate::hda::TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed).max(1);
        let elapsed_ms = crate::hda::rdtsc().wrapping_sub(self.start_tsc) / tsc_per_ms;
        (elapsed_ms as f32 / ANIM_MS as f32).min(1.0)
    }
}

// ── Window ───────────────────────────────────────────────────────────────────
pub struct Window {
    pub id:          usize,
    pub app_kind:    AppKind,
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
    anim:            Option<WindowAnim>,
    /// True once this window has actually been shown to the user (created
    /// visible, or unminimized at least once) — distinguishes "minimized
    /// after being open" (still shows a taskbar button, like a real OS) from
    /// "never opened yet" (boot-time windows created pre-hidden via
    /// `hide_instant()` — no taskbar clutter for programs nobody's touched).
    ever_shown:      bool,
}

impl Window {
    pub fn new(id: usize, app_kind: AppKind, title: &str, x: i32, y: i32, w: usize, h: usize) -> Self {
        Window {
            id, app_kind, title: String::from(title),
            x, y, w, h, minimized: false, maximized: false, is_terminal: false,
            saved_x: x, saved_y: y, saved_w: w, saved_h: h,
            drag_off_x: 0, drag_off_y: 0, dragging: false,
            resizing: false, resize_edge_flags: 0,
            ever_shown: true,
            resize_orig_x: x, resize_orig_y: y,
            resize_orig_w: w, resize_orig_h: h,
            resize_orig_mx: 0, resize_orig_my: 0,
            anim: Some(WindowAnim { kind: AnimKind::Opening, start_tsc: crate::hda::rdtsc() }),
        }
    }

    /// Unminimize (or reveal a just-created window) with an ease-out open animation.
    pub fn show(&mut self) {
        self.ever_shown = true;
        if self.minimized {
            self.minimized = false;
            self.anim = Some(WindowAnim { kind: AnimKind::Opening, start_tsc: crate::hda::rdtsc() });
        }
    }

    /// Minimize/close with an ease-out shrink animation — `minimized` isn't
    /// set until the animation finishes (see `Desktop::tick_anims()`), so the
    /// window keeps rendering (at a shrinking scale) for the transition.
    pub fn hide(&mut self) {
        if !self.minimized {
            self.anim = Some(WindowAnim { kind: AnimKind::Closing, start_tsc: crate::hda::rdtsc() });
        }
    }

    /// Minimize with no animation — for boot-time setup (windows that start
    /// minimized were never shown to the user, so there's nothing to animate).
    pub fn hide_instant(&mut self) {
        self.minimized = true;
        self.anim = None;
        self.ever_shown = false;
    }

    /// True while a close animation is in progress — the render loop needs to
    /// keep drawing these even though they're not yet `minimized`.
    pub fn is_closing(&self) -> bool {
        matches!(self.anim, Some(WindowAnim { kind: AnimKind::Closing, .. }))
    }

    /// Current on-screen geometry, eased toward/away from the real (x, y, w, h)
    /// while `anim` is active. Content renderers already take explicit
    /// (wx, wy, ww, wh) and adapt to whatever size they're given (same as
    /// live window resize), so passing a scaled rect "just works" without any
    /// content-side changes.
    pub fn eased_rect(&self) -> (i32, i32, usize, usize) {
        let Some(anim) = &self.anim else { return (self.x, self.y, self.w, self.h); };
        let t = anim.progress();
        let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t); // ease-out cubic
        match anim.kind {
            AnimKind::Opening | AnimKind::Closing => {
                // Shrinks/grows toward the taskbar (slides down + shrinks a lot,
                // not just a small in-place scale) so minimizing/restoring reads
                // as "going to/from the taskbar" rather than a static fade.
                // slide_frac/scale both run 0->1 over the animation for Opening
                // (rising up + growing from near nothing) and 1->0 for Closing
                // (sinking down + shrinking away), via `eased` vs `1-eased`.
                let (scale, slide_frac) = match anim.kind {
                    AnimKind::Opening => (0.1 + 0.9 * eased, 1.0 - eased),
                    AnimKind::Closing => (1.0 - 0.9 * eased, eased),
                    AnimKind::Transition { .. } => unreachable!(),
                };
                let cx = self.x + self.w as i32 / 2;
                let cy = self.y + self.h as i32 / 2 + (self.h as f32 * slide_frac) as i32;
                let nw = ((self.w as f32) * scale).max(1.0) as usize;
                let nh = ((self.h as f32) * scale).max(1.0) as usize;
                let nx = cx - nw as i32 / 2;
                let ny = cy - nh as i32 / 2;
                (nx, ny, nw, nh)
            }
            AnimKind::Transition { from: (fx, fy, fw, fh) } => {
                let lerp = |a: f32, b: f32| a + (b - a) * eased;
                let nx = lerp(fx as f32, self.x as f32) as i32;
                let ny = lerp(fy as f32, self.y as f32) as i32;
                let nw = lerp(fw as f32, self.w as f32).max(0.0) as usize;
                let nh = lerp(fh as f32, self.h as f32).max(0.0) as usize;
                (nx, ny, nw, nh)
            }
        }
    }

    /// Start a Transition anim from `from` (the window's geometry *before* the
    /// caller changes x/y/w/h) to whatever x/y/w/h are set to next — used by
    /// maximize/restore and edge-drag snap.
    fn start_transition(&mut self, from: (i32, i32, usize, usize)) {
        self.anim = Some(WindowAnim { kind: AnimKind::Transition { from }, start_tsc: crate::hda::rdtsc() });
    }

    pub fn toggle_maximize(&mut self, sw: usize, sh: usize) {
        let from = (self.x, self.y, self.w, self.h);
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
        self.start_transition(from);
    }

    pub fn maximize_hit(&self, mx: i32, my: i32) -> bool {
        let tx = self.x.max(0);
        let ty = self.outer_y() + BORDER_W as i32;
        let bx = tx + self.w as i32 - 34;
        let by = ty + 4;
        mx >= bx && mx < bx + 14 && my >= by && my < by + 14
    }

    /// "_" minimize button — just left of the maximize button.
    pub fn minimize_hit(&self, mx: i32, my: i32) -> bool {
        let tx = self.x.max(0);
        let ty = self.outer_y() + BORDER_W as i32;
        let bx = tx + self.w as i32 - 50;
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
    /// TSC timestamp of the most recent Start Menu open — lets `draw_start_menu()`
    /// slide the popup up from behind the taskbar over `MENU_ANIM_MS` instead of
    /// snapping open instantly. `None` means "not mid-open-animation" (fully open
    /// or closed — closing is still instant, only the open transition animates).
    start_menu_anim_tsc:  Option<u64>,
    dbl_click_pending:    Option<usize>,
    pub snap_zone:        Option<SnapZone>,
    pub context_menu:      Option<(i32, i32)>, // Some(x,y) = right-click menu position
    pub context_menu_kind: ContextMenuKind,
    pub open_settings_requested: bool,
    pub new_window_requested:   Option<AppKind>,
    pub taskbar_jumplist:  Option<(AppKind, usize)>, // (kind, button left-x) — open when count>1
    /// Set when the user picks Copy/Paste from an editor/terminal's
    /// right-click menu — (window id, is_paste). Consumed by main.rs, which
    /// routes it to the right Editor/Terminal instance.
    pub clipboard_action_requested: Option<(usize, bool)>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ContextMenuKind { Background, App(AppKind), EditText(usize) }

impl Desktop {
    pub fn new(fb_w: usize, fb_h: usize) -> Self {
        Desktop {
            windows: Vec::new(), focused: None, next_id: 0,
            fb_w, fb_h, prev_btn: 0, dirty: true, mouse_dirty: false,
            prev_cx: 0, prev_cy: 0, start_menu_open: false,
            start_menu_anim_tsc: None,
            dbl_click_pending: None, snap_zone: None,
            context_menu: None, context_menu_kind: ContextMenuKind::Background,
            open_settings_requested: false, new_window_requested: None,
            taskbar_jumplist: None,
            clipboard_action_requested: None,
        }
    }

    pub fn add_window(&mut self, app_kind: AppKind, title: &str, x: i32, y: i32, w: usize, h: usize) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window::new(id, app_kind, title, x, y, w, h));
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

    /// Group windows that have ever been shown to the user by app kind, in
    /// order of first appearance — includes currently-minimized ones (a real
    /// "minimize to taskbar", not a disappear) but excludes windows that
    /// started pre-hidden at boot and were never actually opened.
    /// Returns (kind, ids-of-that-kind) — ids in the same z-order as `self.windows`.
    pub fn grouped_taskbar_entries(&self) -> Vec<(AppKind, Vec<usize>)> {
        Self::group_by_kind(self.windows.iter().filter(|w| w.ever_shown))
    }

    /// Group ALL windows (regardless of minimized state) by app kind — used by
    /// the Start Menu, which lists every program, not just open ones.
    pub fn grouped_all_entries(&self) -> Vec<(AppKind, Vec<usize>)> {
        Self::group_by_kind(self.windows.iter())
    }

    fn group_by_kind<'a>(iter: impl Iterator<Item = &'a Window>) -> Vec<(AppKind, Vec<usize>)> {
        let mut groups: Vec<(AppKind, Vec<usize>)> = Vec::new();
        for w in iter {
            if let Some(g) = groups.iter_mut().find(|(k, _)| *k == w.app_kind) {
                g.1.push(w.id);
            } else {
                groups.push((w.app_kind, alloc::vec![w.id]));
            }
        }
        groups
    }

    /// Display label for one window inside a jump-list / grouped context — if the
    /// window's title is just the generic app label (e.g. every Terminal window is
    /// literally titled "Terminal"), number it ("Terminal 1", "Terminal 2", ...);
    /// otherwise the title already differentiates instances (e.g. "Editor: foo.txt").
    fn instance_label(&self, kind: AppKind, ids: &[usize], id: usize) -> String {
        let win = match self.windows.iter().find(|w| w.id == id) { Some(w) => w, None => return String::new() };
        if win.title == app_label(kind) {
            let n = ids.iter().position(|&i| i == id).unwrap_or(0) + 1;
            alloc::format!("{} {}", app_label(kind), n)
        } else {
            win.title.clone()
        }
    }

    pub fn bring_to_front(&mut self, id: usize) {
        if let Some(pos) = self.windows.iter().position(|w| w.id == id) {
            let win = self.windows.remove(pos);
            self.windows.push(win);
            self.focused = Some(id);
        }
    }

    /// True while any window has an open/close animation in progress — the
    /// caller should force a redraw every frame while this holds, since
    /// nothing else would otherwise mark the scene dirty mid-animation.
    pub fn any_animating(&self) -> bool {
        self.windows.iter().any(|w| w.anim.is_some())
    }

    /// 0.0-1.0 progress through the Start Menu's open-slide animation (always
    /// 1.0 once it's finished, or if the menu's never been animated-open yet).
    fn start_menu_progress(&self) -> f32 {
        let Some(start) = self.start_menu_anim_tsc else { return 1.0; };
        let tsc_per_ms = crate::hda::TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed).max(1);
        let elapsed_ms = crate::hda::rdtsc().wrapping_sub(start) / tsc_per_ms;
        (elapsed_ms as f32 / MENU_ANIM_MS as f32).min(1.0)
    }

    /// True while the Start Menu's open-slide animation is still in progress —
    /// same "force a redraw" role as `any_animating()`.
    pub fn start_menu_animating(&self) -> bool {
        self.start_menu_open && self.start_menu_progress() < 1.0
    }

    /// Advance window animations — call once per frame. Finishes any anim
    /// whose 180ms has elapsed: a completed `Closing` anim actually sets
    /// `minimized = true` here (that's what the render loop was waiting on to
    /// stop drawing it); a completed `Opening` anim just clears itself.
    /// Returns true if anything changed (caller should mark the scene dirty).
    pub fn tick_anims(&mut self) -> bool {
        let mut changed = false;
        for w in &mut self.windows {
            if let Some(anim) = &w.anim {
                if anim.progress() >= 1.0 {
                    if anim.kind == AnimKind::Closing { w.minimized = true; }
                    w.anim = None;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Returns true if a new terminal window should be spawned (caller must do it after dropping the DESKTOP lock).
    pub fn update_mouse(&mut self, mx: i32, my: i32, buttons: u8) -> bool {
        let mut spawn_terminal = false;
        if mx != self.prev_cx || my != self.prev_cy {
            self.mouse_dirty = true;  // cursor moved — don't force a full scene redraw
            self.prev_cx = mx;
            self.prev_cy = my;
        }
        let right_clicked = buttons & 0x02 != 0 && self.prev_btn & 0x02 == 0;
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
                        return false;
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
                        return false;
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
                        let from = (win.x, win.y, win.w, win.h);
                        match zone {
                            SnapZone::Left  => { win.x = bx; win.y = by;
                                                 win.w = hw.saturating_sub(BORDER_W * 2); win.h = fh;
                                                 win.start_transition(from); }
                            SnapZone::Right => { win.x = bx + hw as i32; win.y = by;
                                                 win.w = hw.saturating_sub(BORDER_W * 2); win.h = fh;
                                                 win.start_transition(from); }
                            SnapZone::Top   => { win.toggle_maximize(sw, sh); } // toggle_maximize starts its own transition
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

        // Right-click: taskbar button → New Window; start-menu row → New Window;
        // empty desktop → Change background. Priority in that order.
        if right_clicked {
            let in_taskbar = my >= self.fb_h as i32 - TASKBAR_H as i32;

            let taskbar_kind = if in_taskbar && mx as usize >= START_W {
                let groups = self.grouped_taskbar_entries();
                let slot   = (mx as usize - START_W) / TASK_BTN_W;
                groups.get(slot).map(|(k, _)| *k)
            } else { None };

            let start_groups = self.grouped_all_entries();
            let menu_h   = start_groups.len() * MENU_ENTRY_H + 8;
            let menu_top = (self.fb_h - TASKBAR_H).saturating_sub(menu_h);
            let in_menu  = self.start_menu_open
                && mx >= 0 && (mx as usize) < MENU_W
                && my >= menu_top as i32 && my < self.fb_h as i32 - TASKBAR_H as i32;
            let startmenu_kind = if in_menu {
                let entry = (my as usize - menu_top) / MENU_ENTRY_H;
                start_groups.get(entry).map(|(k, _)| *k)
            } else { None };

            if let Some(kind) = taskbar_kind.or(startmenu_kind) {
                if app_supports_new_window(kind) {
                    self.context_menu      = Some((mx, my));
                    self.context_menu_kind = ContextMenuKind::App(kind);
                    self.taskbar_jumplist   = None;
                    self.dirty              = true;
                }
            } else if !in_taskbar {
                // Right-clicking inside an Editor/Terminal window's text area
                // shows Copy/Paste instead of the generic New-Window menu.
                let text_win = self.windows.iter().rev().find(|w| {
                    !w.minimized && w.content_hit(mx, my)
                        && matches!(w.app_kind, AppKind::Editor | AppKind::Terminal)
                });
                if let Some(w) = text_win {
                    self.context_menu      = Some((mx, my));
                    self.context_menu_kind = ContextMenuKind::EditText(w.id);
                    self.taskbar_jumplist   = None;
                    self.dirty              = true;
                } else {
                    let on_window = self.windows.iter().rev().any(|w| {
                        !w.minimized && (w.close_hit(mx, my) || w.maximize_hit(mx, my) || w.minimize_hit(mx, my)
                            || w.newterm_hit(mx, my) || w.title_hit(mx, my)
                            || w.resize_hit(mx, my) || w.content_hit(mx, my))
                    });
                    if !on_window {
                        self.context_menu      = Some((mx, my));
                        self.context_menu_kind = ContextMenuKind::Background;
                        self.start_menu_open    = false;
                        self.taskbar_jumplist   = None;
                        self.dirty               = true;
                    }
                }
            }
        }

        if !clicked { return false; }

        // Left-click dismisses context menu; handle the item first if clicked on it.
        if self.context_menu.is_some() {
            let item = self.context_menu_item_at(mx, my);
            let kind = self.context_menu_kind;
            self.context_menu = None;
            self.dirty = true;
            match (kind, item) {
                (ContextMenuKind::Background, Some(0)) => self.open_settings_requested = true,
                (ContextMenuKind::App(k),     Some(0)) => self.new_window_requested = Some(k),
                (ContextMenuKind::EditText(id), Some(0)) => self.clipboard_action_requested = Some((id, false)), // Copy
                (ContextMenuKind::EditText(id), Some(1)) => self.clipboard_action_requested = Some((id, true)),  // Paste
                _ => {}
            }
            return false;
        }

        // Left-click while a taskbar jump list is open: pick an entry, or dismiss it.
        // Falls through (doesn't return) so the same click can still register normally
        // — e.g. clicking a different taskbar button both closes this popup and works.
        let mut jumplist_just_closed_for: Option<AppKind> = None;
        if let Some((jl_kind, jl_x)) = self.taskbar_jumplist {
            let ids = self.grouped_all_entries().into_iter()
                .find(|(k, _)| *k == jl_kind).map(|(_, ids)| ids).unwrap_or_default();
            let hit = self.jumplist_item_at(jl_x, &ids, mx, my);
            self.taskbar_jumplist = None;
            self.dirty = true;
            if let Some(id) = hit {
                if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) { w.show(); }
                self.bring_to_front(id);
                return false;
            }
            jumplist_just_closed_for = Some(jl_kind);
        }

        // Double-click tracking: clear pending unless we land on the same title bar again
        let prev_dbl = self.dbl_click_pending.take();

        let in_taskbar   = my >= self.fb_h as i32 - TASKBAR_H as i32;
        let start_groups = self.grouped_all_entries();
        let menu_h       = start_groups.len() * MENU_ENTRY_H + 8;
        let menu_top     = (self.fb_h - TASKBAR_H).saturating_sub(menu_h);
        let in_menu      = self.start_menu_open
            && mx >= 0 && (mx as usize) < MENU_W
            && my >= menu_top as i32 && my < self.fb_h as i32 - TASKBAR_H as i32;

        // Start-menu item click — one program per row; >1 open instance opens
        // a jump list (same picker the taskbar uses) instead of guessing which to focus.
        if in_menu {
            let entry = (my as usize - menu_top) / MENU_ENTRY_H;
            if let Some((kind, ids)) = start_groups.get(entry) {
                self.start_menu_open = false;
                if ids.len() == 1 {
                    let id = ids[0];
                    if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
                        w.show();
                    }
                    self.bring_to_front(id);
                } else {
                    self.taskbar_jumplist = Some((*kind, 0));
                }
                self.dirty = true;
            }
            return false;
        }

        // Taskbar click
        if in_taskbar {
            if (mx as usize) < START_W {
                // Start button
                self.start_menu_open = !self.start_menu_open;
                if self.start_menu_open { self.start_menu_anim_tsc = Some(crate::hda::rdtsc()); }
                self.dirty = true;
            } else {
                // Window button — grouped by app kind; a group with >1 window opens
                // a jump list instead of directly toggling a single window.
                self.start_menu_open = false;
                let groups = self.grouped_taskbar_entries();
                let btn_x  = mx as usize - START_W;
                let slot   = btn_x / TASK_BTN_W;
                if let Some((kind, ids)) = groups.get(slot) {
                    if ids.len() == 1 {
                        let wid = ids[0];
                        let is_minimized = self.windows.iter().find(|w| w.id == wid).map(|w| w.minimized).unwrap_or(false);
                        if is_minimized {
                            // Minimized (but shown before, hence its taskbar button
                            // still exists) — restore it, same as clicking it in the
                            // Start Menu or a jump list would.
                            if let Some(w) = self.windows.iter_mut().find(|w| w.id == wid) { w.show(); }
                            self.bring_to_front(wid);
                        } else if self.focused == Some(wid) {
                            // Click focused window button = minimize it
                            if let Some(w) = self.windows.iter_mut().find(|w| w.id == wid) {
                                w.hide();
                            }
                            self.focused = None;
                        } else {
                            self.bring_to_front(wid);
                        }
                    } else if jumplist_just_closed_for == Some(*kind) {
                        // This click just dismissed this exact jump list — leave it closed.
                    } else {
                        let btn_left = START_W + slot * TASK_BTN_W;
                        self.taskbar_jumplist = Some((*kind, btn_left));
                    }
                    self.dirty = true;
                }
            }
            return false;
        }

        // Desktop / window click — close start menu, hit-test windows top-to-bottom
        self.start_menu_open = false;
        let mut hit_id = None;
        for win in self.windows.iter().rev() {
            if win.minimized { continue; }
            if win.close_hit(mx, my) || win.maximize_hit(mx, my) || win.minimize_hit(mx, my) || win.newterm_hit(mx, my)
                || win.title_hit(mx, my) || win.resize_hit(mx, my) || win.content_hit(mx, my)
            {
                hit_id = Some(win.id);
                break;
            }
        }
        // No window hit — check desktop icon click
        if hit_id.is_none() {
            for (slot, icon) in ICONS.iter().enumerate() {
                let (ix, iy, iw, ih) = icon_rect(slot);
                if mx >= ix && mx < ix + iw as i32
                    && my >= iy && my < iy + ih as i32 + 12 // include label row
                {
                    if let Some(w) = self.windows.iter_mut().find(|w| w.id == icon.win_id) {
                        w.show();
                    }
                    self.bring_to_front(icon.win_id);
                    self.dirty = true;
                    return false;
                }
                let _ = slot; // suppress unused warning if loop body always returns
            }
        }

        if let Some(id) = hit_id {
            self.bring_to_front(id);
            self.dirty = true;
            let win = self.windows.iter_mut().find(|w| w.id == id).unwrap();
            if win.close_hit(mx, my) {
                win.hide();
                self.focused  = None;
            } else if win.minimize_hit(mx, my) {
                win.hide();
                self.focused  = None;
            } else if win.newterm_hit(mx, my) {
                spawn_terminal = true;
            } else if win.maximize_hit(mx, my) {
                let sw = self.fb_w; let sh = self.fb_h;
                win.toggle_maximize(sw, sh);
            } else if win.maximized {
                // Dragging a maximized window's title bar restores it first
                // (like any normal OS), then keeps dragging so the same motion
                // can carry it into a snap zone (e.g. back to the top to
                // re-maximize) — this used to do nothing at all, since the
                // drag-start logic below only ran for `!win.maximized` windows.
                if win.title_hit(mx, my) {
                    // Grab position as a fraction along the (wide) maximized
                    // title bar, so restoring keeps the window roughly under
                    // the cursor instead of snapping to a fixed spot.
                    let old_outer_x = win.outer_x();
                    let old_outer_w = win.outer_w().max(1);
                    let frac = ((mx - old_outer_x) as f32 / old_outer_w as f32).clamp(0.0, 1.0);
                    let sw = self.fb_w; let sh = self.fb_h;
                    win.toggle_maximize(sw, sh); // restores to saved_x/y/w/h
                    win.x = mx - (frac * win.w as f32) as i32;
                    win.y = my - (TITLE_H as i32 / 2);
                    win.dragging   = true;
                    win.drag_off_x = mx - win.x;
                    win.drag_off_y = my - win.y;
                }
            } else {
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
        spawn_terminal
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

    fn draw_wallpaper(&self, display: &mut Display) {
        match WALLPAPER.load(core::sync::atomic::Ordering::Relaxed) {
            WP_BLISS => self.draw_bliss(display),
            _        => self.draw_dark_gradient(display),
        }
    }

    /// Dark navy gradient + deterministic star field.
    fn draw_dark_gradient(&self, display: &mut Display) {
        let w = display.width();
        let h = display.height();
        let (tr, tg, tb) = (0x0D_i32, 0x1F_i32, 0x40_i32);
        let (br, bg, bb) = (0x07_i32, 0x07_i32, 0x10_i32);
        let n = h as i32;
        for y in 0..h {
            let t = y as i32;
            let r = ((tr * (n-t) + br * t) / n) as u8;
            let g = ((tg * (n-t) + bg * t) / n) as u8;
            let b = ((tb * (n-t) + bb * t) / n) as u8;
            display.fill_rect(0, y, w, 1, Color { r, g, b });
        }
        let mut rng = 0x9E3779B9u32;
        let cols = w / 16; let rows = h / 16;
        for sy in 0..rows {
            for sx in 0..cols {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                if rng & 0x1F == 0 {
                    let px = sx * 16 + (rng >> 20 & 15) as usize;
                    let py = sy * 16 + (rng >> 16 & 15) as usize;
                    let bright = ((rng >> 24 & 0x5F) as u8).saturating_add(0x60);
                    if px < w && py < h {
                        display.put_pixel_pub(px, py, Color { r: bright, g: bright, b: bright });
                    }
                }
            }
        }
    }

    /// Parabolic sine approximation: ang in 0..256 → result in -1024..1024.
    fn isin(ang: i32) -> i32 {
        let neg = ang & 128 != 0;
        let a   = ang & 127;
        let v   = a * (128 - a) / 4; // peak at a=64: 64*64/4 = 1024
        if neg { -v } else { v }
    }

    /// Windows-XP-Bliss-style wallpaper: blue sky gradient + two green hill layers.
    fn draw_bliss(&self, display: &mut Display) {
        let w = display.width();
        let h = display.height();

        // Sky gradient: #2776D0 (top) → #8BCEF4 (horizon at 60%)
        let horizon = h * 60 / 100;
        let (str_, stg, stb) = (0x27_i32, 0x76_i32, 0xD0_i32);
        let (shr, shg, shb) = (0x8B_i32, 0xCE_i32, 0xF4_i32);
        for y in 0..h {
            let t  = y.min(horizon) as i32;
            let n  = horizon as i32;
            let r  = ((str_ * (n-t) + shr * t) / n.max(1)) as u8;
            let g  = ((stg  * (n-t) + shg * t) / n.max(1)) as u8;
            let b  = ((stb  * (n-t) + shb * t) / n.max(1)) as u8;
            display.fill_rect(0, y, w, 1, Color { r, g, b });
        }

        // Far hills: base at 57%, amplitude 9%, ~1.5 sine cycles
        let far_base = h * 57 / 100;
        let far_amp  = h * 9  / 100;
        // Near hills: base at 70%, amplitude 8%, ~1 cycle (offset phase)
        let near_base = h * 70 / 100;
        let near_amp  = h * 8  / 100;

        for x in 0..w {
            // Far hills
            let ang_f  = (x * 192 / w.max(1)) as i32;
            let hy_f   = (far_base as i32  + far_amp  as i32 * Self::isin(ang_f)  / 1024)
                            .max(0).min(h as i32 - 1) as usize;
            // Near hills (different frequency + phase shift)
            let ang_n  = (x * 128 / w.max(1) + 32) as i32;
            let hy_n   = (near_base as i32 + near_amp as i32 * Self::isin(ang_n) / 1024)
                            .max(0).min(h as i32 - 1) as usize;

            // Far grass column (from hy_f down to hy_n or bottom)
            let grass_far_bot = hy_n.min(h);
            if hy_f < grass_far_bot {
                display.fill_rect(x, hy_f, 1, grass_far_bot - hy_f,
                    Color::from_hex(0x6BA239));
            }
            // Near grass column (from hy_n to bottom)
            if hy_n < h {
                display.fill_rect(x, hy_n, 1, h - hy_n,
                    Color::from_hex(0x4E8C27));
            }
        }
    }

    /// Clear the desktop background and draw desktop icons.
    pub fn render(&self, display: &mut Display, _cx: i32, _cy: i32) {
        self.draw_wallpaper(display);
        for (slot, icon) in ICONS.iter().enumerate() {
            let (ix, iy, iw, ih) = icon_rect(slot);
            if iy < 0 { continue; }
            let ix = ix as usize; let iy = iy as usize;
            // Outer border (1px, slightly lighter than bg)
            display.fill_rect(ix, iy, iw, ih, Color::from_hex(0x2A2A3A));
            // Coloured icon face (inset 2px)
            display.fill_rect(ix + 2, iy + 2, iw - 4, ih - 4, icon.color);
            // Dark title-bar strip on the icon face
            display.fill_rect(ix + 2, iy + 2, iw - 4, 8, Color::from_hex(0x111111));
            // Label below (scale=1, centred under icon)
            let label_y = iy + ih + 4;
            let label_len = icon.label.len();
            let label_x = if label_len * 8 < iw {
                ix + (iw - label_len * 8) / 2
            } else { ix };
            display.draw_text(label_x, label_y, icon.label, pal::TEXT, 1);
        }
    }

    /// Draw a single window's chrome (title bar, border, content-area background).
    /// Called per-window from the task_blink render loop in z-order. `rect` is
    /// the window's on-screen (x, y, w, h) for this frame — normally just
    /// `(win.x, win.y, win.w, win.h)`, but a shrunk/grown value from
    /// `win.eased_rect()` while an open/close animation is in progress.
    pub fn draw_window(&self, display: &mut Display, win: &Window, focused: bool, rect: (i32, i32, usize, usize)) {
        let (wx, wy, ww, wh) = rect;
        let ox = wx - BORDER_W as i32;
        let oy = wy - TITLE_H as i32 - BORDER_W as i32;
        let ow = ww + BORDER_W * 2;
        let oh = wh + TITLE_H + BORDER_W * 2;

        // Border
        let border_col = if focused { pal::BORDER_ACT } else { pal::BORDER };
        if ox >= 0 && oy >= 0 {
            display.fill_rect(ox as usize, oy as usize, ow, oh, border_col);
        }

        // Title bar
        let title_col = if focused { pal::WIN_TITLE_A } else { pal::WIN_TITLE };
        let tx = (wx as usize).max(0);
        let ty = oy.max(0) as usize;
        display.fill_rect(tx, ty, ww, TITLE_H, title_col);

        // "+" new-terminal button (left side of title bar, terminal windows only)
        if win.is_terminal {
            display.fill_rect(tx + 4, ty + 4, 14, 14, pal::TASKBAR_BTN);
            display.draw_text(tx + 8, ty + 5, "+", pal::TEXT, 1);
            display.draw_text(tx + 22, ty + 5, &win.title, pal::TEXT, 1);
        } else {
            display.draw_text(tx + 6, ty + 5, &win.title, pal::TEXT, 1);
        }

        // Close button
        let close_x = tx + ww.saturating_sub(18);
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

        // Minimize button (14×14, just left of maximize button)
        let min_x = max_x.saturating_sub(16);
        display.fill_rect(min_x, ty + 4, 14, 14, pal::TASKBAR_BTN);
        display.fill_rect(min_x + 2, ty + 11, 10, 1, pal::TEXT); // "_" underline glyph

        // Content background
        display.fill_rect(wx.max(0) as usize, wy.max(0) as usize, ww, wh, pal::WIN_BG);
        display.fill_rect(wx.max(0) as usize, wy.max(0) as usize, ww, 1, pal::BORDER);

        // Resize handle — only visible when not maximized
        if !win.maximized {
            let rx = (wx + ww as i32 - 2).max(0) as usize;
            let by = (wy + wh as i32 - 2).max(0) as usize;
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

        // Window buttons — grouped by app kind, one per program that's ever been
        // opened (includes currently-minimized windows — a real "minimize to
        // taskbar", not a disappear). "(N)" suffix shows the currently-open
        // count when there's more than one instance; minimized-only groups are
        // dimmed since nothing of that kind is visible right now.
        let mut bx = START_W + 4;
        for (kind, ids) in self.grouped_taskbar_entries() {
            if bx + TASK_BTN_W > self.fb_w - 80 { break; }
            let open_count = ids.iter().filter(|&&id| self.windows.iter().any(|w| w.id == id && !w.minimized)).count();
            let active = open_count > 0 && (ids.len() == 1 && self.focused == Some(ids[0])
                || (ids.len() > 1 && ids.iter().any(|&i| self.focused == Some(i))));
            let bc = if active { pal::TASKBAR_ACT }
                     else if open_count == 0 { pal::TASKBAR } // dimmed — minimized, not currently visible
                     else { pal::TASKBAR_BTN };
            display.fill_rect(bx, ty + 4, TASK_BTN_W - 4, TASKBAR_H - 8, bc);
            if active {
                display.fill_rect(bx, ty + TASKBAR_H - 5, TASK_BTN_W - 4, 2, pal::ACCENT);
            }
            let full_label = if ids.len() > 1 {
                alloc::format!("{} ({})", app_label(kind), open_count)
            } else {
                let win = self.windows.iter().find(|w| w.id == ids[0]);
                win.map(|w| w.title.clone()).unwrap_or_else(|| String::from(app_label(kind)))
            };
            let label = if full_label.len() > 13 { &full_label[..13] } else { &full_label };
            let text_col = if open_count == 0 { pal::TEXT_DIM } else { pal::TEXT };
            display.draw_text(bx + 6, ty + 10, label, text_col, 1);
            bx += TASK_BTN_W;
        }

        // Clock (right-aligned)
        let mut tbuf = [0u8; 6];
        let time = crate::rtc::fmt_time(&mut tbuf);
        let tw   = time.len() * 9;
        display.draw_text(self.fb_w.saturating_sub(tw + 8), ty + 10, time, pal::TEXT, 1);
    }

    /// Draw the jump-list popup (opens above the taskbar when a grouped button
    /// with >1 window is clicked) — one row per instance of that app kind.
    pub fn draw_taskbar_jumplist(&self, display: &mut Display) {
        let Some((kind, btn_x)) = self.taskbar_jumplist else { return; };
        // Uses ALL instances (not just non-minimized) so a jump list opened from
        // the Start Menu still works when every instance happens to be minimized.
        let ids = match self.grouped_all_entries().into_iter().find(|(k, _)| *k == kind) {
            Some((_, ids)) => ids,
            None => return,
        };
        const JL_W: usize = 180;
        let item_h = MENU_ENTRY_H;
        let jl_h   = ids.len() * item_h + 8;
        let x = btn_x.min(self.fb_w.saturating_sub(JL_W));
        let y = (self.fb_h - TASKBAR_H).saturating_sub(jl_h);

        display.fill_rect(x, y, JL_W, jl_h, pal::MENU_BG);
        display.fill_rect(x, y, JL_W, 1, pal::MENU_BORDER);
        display.fill_rect(x, y, 1, jl_h, pal::MENU_BORDER);
        display.fill_rect(x + JL_W - 1, y, 1, jl_h, pal::MENU_BORDER);
        display.fill_rect(x, y + jl_h - 1, JL_W, 1, pal::MENU_BORDER);

        for (i, &id) in ids.iter().enumerate() {
            let ey = y + 4 + i * item_h;
            let active = self.focused == Some(id);
            if active {
                display.fill_rect(x + 2, ey, JL_W - 4, item_h - 2, pal::MENU_HOVER);
            }
            let label = self.instance_label(kind, &ids, id);
            let label = if label.len() > 22 { &label[..22] } else { &label };
            display.draw_text(x + 8, ey + 6, label, if active { pal::ACCENT } else { pal::TEXT }, 1);
        }
    }

    /// Hit-test for the jump-list popup. Returns the window id clicked, if any.
    pub fn jumplist_item_at(&self, btn_x: usize, ids: &[usize], mx: i32, my: i32) -> Option<usize> {
        const JL_W: usize = 180;
        let item_h = MENU_ENTRY_H;
        let jl_h   = ids.len() * item_h + 8;
        let x = btn_x.min(self.fb_w.saturating_sub(JL_W)) as i32;
        let y = (self.fb_h - TASKBAR_H).saturating_sub(jl_h) as i32;
        if mx < x || mx >= x + JL_W as i32 || my < y + 4 || my >= y + jl_h as i32 - 4 { return None; }
        let row = (my - y - 4) as usize / item_h;
        ids.get(row).copied()
    }

    /// Labels for the current context menu's items, top to bottom.
    fn context_menu_labels(&self) -> &'static [&'static str] {
        match self.context_menu_kind {
            ContextMenuKind::Background => &["Change background"],
            ContextMenuKind::App(_)     => &["New Window"],
            ContextMenuKind::EditText(_) => &["Copy", "Paste"],
        }
    }

    /// Draw right-click context menu if one is open.
    pub fn draw_context_menu(&self, display: &mut Display) {
        let (cx, cy) = match self.context_menu {
            Some(p) => p,
            None    => return,
        };
        const CM_W: usize = 168;
        const ITEM_H: usize = 26;
        let labels = self.context_menu_labels();
        let cm_h = labels.len() * ITEM_H + 8;
        // Clamp so it stays on screen
        let x = (cx as usize).min(self.fb_w.saturating_sub(CM_W));
        let y = (cy as usize).min(self.fb_h.saturating_sub(cm_h + TASKBAR_H));
        // Shadow
        display.fill_rect(x + 3, y + 3, CM_W, cm_h, Color::from_hex(0x000000));
        // Background + border
        display.fill_rect(x, y, CM_W, cm_h, pal::MENU_BG);
        display.fill_rect(x, y, CM_W, 1, pal::MENU_BORDER);
        display.fill_rect(x, y + cm_h - 1, CM_W, 1, pal::MENU_BORDER);
        display.fill_rect(x, y, 1, cm_h, pal::MENU_BORDER);
        display.fill_rect(x + CM_W - 1, y, 1, cm_h, pal::MENU_BORDER);
        for (i, label) in labels.iter().enumerate() {
            display.draw_text(x + 10, y + 8 + i * ITEM_H, label, pal::TEXT, 1);
        }
    }

    /// Context-menu item hit-test.  Returns Some(item_index) if (mx,my) is inside
    /// the menu and on a valid item.
    pub fn context_menu_item_at(&self, mx: i32, my: i32) -> Option<usize> {
        let (cx, cy) = self.context_menu?;
        const CM_W: i32 = 168;
        const ITEM_H: i32 = 26;
        let labels = self.context_menu_labels();
        let cm_h = (labels.len() * ITEM_H as usize + 8) as i32;
        let x = (cx as usize).min(self.fb_w.saturating_sub(CM_W as usize)) as i32;
        let y = (cy as usize).min(self.fb_h.saturating_sub((cm_h as usize) + TASKBAR_H)) as i32;
        if mx >= x && mx < x + CM_W && my >= y + 4 && my < y + 4 + cm_h - 8 {
            Some(((my - (y + 4)) / ITEM_H) as usize)
        } else {
            None
        }
    }

    /// Draw the start menu popup. Call BEFORE draw_taskbar so taskbar renders on top.
    /// Lists one row per *program* (grouped by app kind), not one per window —
    /// several open instances of the same app still show as a single entry.
    pub fn draw_start_menu(&self, display: &mut Display) {
        if !self.start_menu_open { return; }

        let groups = self.grouped_all_entries();
        let menu_h = groups.len() * MENU_ENTRY_H + 8;
        let target_menu_y = (self.fb_h - TASKBAR_H).saturating_sub(menu_h);
        // Slide up from behind the taskbar on open (150ms ease-out); closing
        // stays instant. Hit-testing (in update_mouse) always uses target_menu_y
        // directly, not this eased value — you can't click precisely during a
        // 150ms slide anyway, so there's no reason to chase a moving target.
        let t = self.start_menu_progress();
        let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
        let start_y = self.fb_h - TASKBAR_H;
        let menu_y = (start_y as f32 + (target_menu_y as f32 - start_y as f32) * eased) as usize;

        // Background + border
        display.fill_rect(0, menu_y, MENU_W, menu_h, pal::MENU_BG);
        display.fill_rect(0, menu_y, MENU_W, 1, pal::MENU_BORDER);
        display.fill_rect(MENU_W - 1, menu_y, 1, menu_h, pal::MENU_BORDER);

        // Header
        display.draw_text(8, menu_y + 4, "Programs", pal::ACCENT, 1);
        display.fill_rect(0, menu_y + 18, MENU_W, 1, pal::MENU_BORDER);

        for (i, (kind, ids)) in groups.iter().enumerate() {
            let ey = menu_y + 8 + i * MENU_ENTRY_H;
            // `ids` is every window of this kind ever created (grouped_all_entries()
            // deliberately includes minimized ones, so the jump list can still
            // restore a hidden instance) — but the displayed "(N)" badge should
            // track *currently open* windows, same as the taskbar, not the
            // ever-growing total (which never shrinks since windows are only
            // ever minimized, never destroyed).
            let open_count = ids.iter().filter(|&&id| self.windows.iter().any(|w| w.id == id && !w.minimized)).count();
            let any_open   = open_count > 0;
            let any_focused = ids.iter().any(|&id| self.focused == Some(id))
                && self.windows.iter().any(|w| ids.contains(&w.id) && self.focused == Some(w.id) && !w.minimized);
            if any_focused {
                display.fill_rect(2, ey, MENU_W - 4, MENU_ENTRY_H - 2, pal::MENU_HOVER);
            }
            let label = if open_count > 1 {
                alloc::format!("{} ({})", app_label(*kind), open_count)
            } else {
                String::from(app_label(*kind))
            };
            let label = if label.len() > 18 { &label[..18] } else { &label };
            let col = if any_focused { pal::ACCENT } else { pal::TEXT };
            display.draw_text(10, ey + 6, label, col, 1);
            // "--" badge when no instance of this program is currently open
            if !any_open {
                display.draw_text(MENU_W - 24, ey + 6, "--", pal::TEXT_DIM, 1);
            }
        }
    }
}

pub static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

