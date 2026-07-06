//! In-OS text editor.
//! Open with `edit <file>` in the terminal.
//! Controls: arrows=navigate, type=insert, backspace=delete,
//!            Ctrl+S=save, Ctrl+Q/ESC=close.

use alloc::{string::String, vec::Vec};
use spin::Mutex;
use crate::framebuffer::{Color, Display};

const SCALE:  usize = 1;
const CHAR_W: usize = 9;
const CHAR_H: usize = 12;

const BG:           Color = Color::from_hex(0x0A0A14);
const TEXT:         Color = Color::from_hex(0xE8E8E8);
const DIM:          Color = Color::from_hex(0x666688);
const CURSOR_C:     Color = Color::from_hex(0x6C8EFF);
const MODIFIED:     Color = Color::from_hex(0xFF9944);
const STATUS:       Color = Color::from_hex(0x1A1A2E);
const OK:           Color = Color::from_hex(0x6BFF8E);
const ERR:          Color = Color::from_hex(0xFF6B6B);
const LINE_COL:     Color = Color::from_hex(0x333355);
const FIND_HIT:     Color = Color::from_hex(0x2244AA); // other match background
const FIND_CURRENT: Color = Color::from_hex(0xFFDD44); // current match background
const SELECTION:    Color = Color::from_hex(0x264F78); // text selection background

// ── Syntax highlighting (basic — Rust/C, per-line, no lexer) ─────────────────
// Shared with terminal.rs, which reuses `highlight_line_kw` with its own
// command-name list instead of Rust/C keywords, for live input highlighting.
pub const KW_COLOR:  Color = Color::from_hex(0xC792EA); // keywords
pub const STR_COLOR: Color = Color::from_hex(0xC3E88D); // string/char literals
pub const NUM_COLOR: Color = Color::from_hex(0xF78C6C); // numeric literals
pub const CMT_COLOR: Color = Color::from_hex(0x546E7A); // line comments

const KEYWORDS: &[&str] = &[
    // Rust
    "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "use", "mod", "crate",
    "self", "Self", "if", "else", "match", "for", "while", "loop", "break", "continue",
    "return", "true", "false", "const", "static", "unsafe", "async", "await", "move",
    "ref", "in", "as", "dyn", "where", "type", "extern", "super", "box", "yield",
    // C / C++
    "int", "char", "float", "double", "void", "long", "short", "unsigned", "signed",
    "typedef", "sizeof", "goto", "switch", "case", "default", "do", "union", "volatile",
    "register", "auto", "include", "define", "ifdef", "ifndef", "endif", "class",
    "public", "private", "protected", "namespace", "template", "new", "delete",
    "virtual", "override", "nullptr", "NULL",
];

/// Only Rust/C-family files get highlighted — everything else renders plain.
fn supports_highlighting(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".rs", ".c", ".h", ".cpp", ".hpp", ".cc", ".cxx"]
        .iter().any(|ext| lower.ends_with(ext))
}

fn is_ident_start(b: u8) -> bool { b == b'_' || b.is_ascii_alphabetic() }
fn is_ident_cont(b: u8)  -> bool { b == b'_' || b.is_ascii_alphanumeric() }

/// Per-character colors for one line. Purely line-local (no cross-line block-
/// comment tracking) — enough for basic keyword/string/number/comment coloring
/// without a real lexer or parser.
fn highlight_line(line: &[u8], default_col: Color) -> Vec<Color> {
    highlight_line_kw(line, KEYWORDS, default_col)
}

/// Same tokenizer as `highlight_line`, but parametrized over the keyword list —
/// lets `terminal.rs` reuse it for live command-line input highlighting with
/// its own list of command names instead of Rust/C keywords.
pub fn highlight_line_kw(line: &[u8], keywords: &[&str], default_col: Color) -> Vec<Color> {
    let mut colors = alloc::vec![default_col; line.len()];
    let mut i = 0usize;
    while i < line.len() {
        let b = line[i];
        // Line comment — rest of the line
        if b == b'/' && i + 1 < line.len() && line[i + 1] == b'/' {
            for c in colors.iter_mut().skip(i) { *c = CMT_COLOR; }
            break;
        }
        // String literal
        if b == b'"' {
            let start = i;
            i += 1;
            while i < line.len() && line[i] != b'"' {
                i += if line[i] == b'\\' && i + 1 < line.len() { 2 } else { 1 };
            }
            i = (i + 1).min(line.len());
            for c in &mut colors[start..i] { *c = STR_COLOR; }
            continue;
        }
        // Char literal
        if b == b'\'' {
            let start = i;
            i += 1;
            while i < line.len() && line[i] != b'\'' {
                i += if line[i] == b'\\' && i + 1 < line.len() { 2 } else { 1 };
            }
            i = (i + 1).min(line.len());
            for c in &mut colors[start..i] { *c = STR_COLOR; }
            continue;
        }
        // Numeric literal
        if b.is_ascii_digit() {
            let start = i;
            while i < line.len() && (line[i].is_ascii_alphanumeric() || line[i] == b'.' || line[i] == b'_') { i += 1; }
            for c in &mut colors[start..i] { *c = NUM_COLOR; }
            continue;
        }
        // Identifier / keyword
        if is_ident_start(b) {
            let start = i;
            while i < line.len() && is_ident_cont(line[i]) { i += 1; }
            let word = core::str::from_utf8(&line[start..i]).unwrap_or("");
            if keywords.contains(&word) {
                for c in &mut colors[start..i] { *c = KW_COLOR; }
            }
            continue;
        }
        i += 1;
    }
    colors
}

pub struct Editor {
    pub path:    String,
    lines:       Vec<Vec<u8>>,
    cursor_row:  usize,
    cursor_col:  usize,
    scroll_row:  usize,
    pub modified:          bool,
    pub status_msg:        String,
    status_ok:             bool,
    pub open:              bool,
    pub visible_rows_hint: usize,
    highlight:             bool, // syntax highlighting on/off, set from file extension
    // Text selection — (row, col) where the selection started. `None` = no
    // selection. A real range only exists once this differs from the cursor
    // position (selection_range() returns None for anchor == cursor), so a
    // plain click or a non-extending keypress can safely leave this Some
    // without showing a stray highlight.
    select_anchor: Option<(usize, usize)>,
    // Find mode
    find_mode:    bool,
    find_query:   Vec<u8>,
    find_matches: Vec<(usize, usize)>, // (row, col) of each match start
    find_idx:     usize,               // index of the currently highlighted match
}

impl Editor {
    pub fn new(path: &str, content: &[u8]) -> Self {
        let mut lines: Vec<Vec<u8>> = content
            .split(|&b| b == b'\n')
            .map(|l| l.to_vec())
            .collect();
        if lines.is_empty() { lines.push(Vec::new()); }

        Editor {
            path: String::from(path),
            lines,
            cursor_row: 0, cursor_col: 0,
            scroll_row: 0,
            modified: false,
            status_msg: String::from("F2/Ctrl+S=save  F10=close  Ctrl+F=find"),
            status_ok: true,
            open: true,
            visible_rows_hint: 20,
            highlight: supports_highlighting(path),
            select_anchor: None,
            find_mode: false,
            find_query: Vec::new(),
            find_matches: Vec::new(),
            find_idx: 0,
        }
    }

    fn find_all(&mut self) {
        self.find_matches.clear();
        let qlen = self.find_query.len();
        if qlen == 0 { return; }
        for (r, line) in self.lines.iter().enumerate() {
            let mut c = 0;
            while c + qlen <= line.len() {
                if line[c..c + qlen] == self.find_query[..] {
                    self.find_matches.push((r, c));
                }
                c += 1;
            }
        }
    }

    fn find_jump_nearest(&mut self) {
        if self.find_matches.is_empty() { return; }
        let pos = self.find_matches.iter().position(|&(r, c)| {
            r > self.cursor_row || (r == self.cursor_row && c >= self.cursor_col)
        });
        self.find_idx = pos.unwrap_or(0);
        let (r, c) = self.find_matches[self.find_idx];
        self.cursor_row = r;
        self.cursor_col = c;
        self.ensure_visible();
    }

    fn find_next(&mut self) {
        if self.find_matches.is_empty() { return; }
        self.find_idx = (self.find_idx + 1) % self.find_matches.len();
        let (r, c) = self.find_matches[self.find_idx];
        self.cursor_row = r;
        self.cursor_col = c;
        self.ensure_visible();
    }

    // ── Selection ──────────────────────────────────────────────────────────

    /// Normalized (start, end) selection range, or `None` if there's no real
    /// selection (anchor equals cursor, or no anchor set at all).
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.select_anchor?;
        let cursor = (self.cursor_row, self.cursor_col);
        if anchor == cursor { return None; }
        Some(if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) })
    }

    /// Column range `[start, end)` selected on `line_idx`, if any.
    fn selection_cols_for_line(&self, line_idx: usize, line_len: usize) -> Option<(usize, usize)> {
        let (s, e) = self.selection_range()?;
        if line_idx < s.0 || line_idx > e.0 { return None; }
        let start = if line_idx == s.0 { s.1 } else { 0 };
        let end   = if line_idx == e.0 { e.1 } else { line_len };
        if start >= end { return None; }
        Some((start, end))
    }

    /// If a real selection exists, delete it and collapse the cursor to the
    /// selection start. Returns true if anything was deleted — callers use
    /// this to replace-selection-on-type/backspace/delete like a normal editor.
    fn delete_selection_if_any(&mut self) -> bool {
        let Some((s, e)) = self.selection_range() else { return false; };
        if s.0 == e.0 {
            let line = &mut self.lines[s.0];
            let end = e.1.min(line.len());
            line.drain(s.1.min(end)..end);
        } else {
            let e_row = e.0.min(self.lines.len() - 1);
            let e_col = e.1.min(self.lines[e_row].len());
            let s_col = s.1.min(self.lines[s.0].len());
            let tail = self.lines[e_row].split_off(e_col);
            self.lines[s.0].truncate(s_col);
            self.lines[s.0].extend_from_slice(&tail);
            self.lines.drain(s.0 + 1..=e_row);
        }
        self.cursor_row = s.0;
        self.cursor_col = s.1;
        self.select_anchor = None;
        self.modified = true;
        true
    }

    /// Called at the top of every cursor-movement key handler: extends the
    /// selection if Shift is held (anchoring it at the pre-move position the
    /// first time), or clears it otherwise — same behavior as any normal editor.
    fn update_selection_anchor(&mut self) {
        if crate::ps2::shift_held() {
            if self.select_anchor.is_none() {
                self.select_anchor = Some((self.cursor_row, self.cursor_col));
            }
        } else {
            self.select_anchor = None;
        }
    }

    /// Mouse button pressed at (row, col) in the text buffer — moves the
    /// cursor there and marks it as a potential selection anchor (only
    /// becomes a visible selection once the mouse actually moves elsewhere).
    pub fn mouse_down(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.lines.len().saturating_sub(1));
        self.cursor_col = col.min(self.lines[self.cursor_row].len());
        self.select_anchor = Some((self.cursor_row, self.cursor_col));
        self.ensure_visible();
    }

    /// Mouse dragged (button still held) to (row, col) — extends the
    /// selection from wherever `mouse_down` anchored it.
    pub fn mouse_drag(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.lines.len().saturating_sub(1));
        self.cursor_col = col.min(self.lines[self.cursor_row].len());
        self.ensure_visible();
    }

    /// Pixel coordinates (window-relative) → (row, col) in the text buffer.
    /// Mirrors `render()`'s exact layout math. Returns `None` for clicks
    /// outside the scrollable content area (status bar / bottom bar / gutter).
    pub fn hit_test(&self, wx: usize, wy: usize, ww: usize, wh: usize, mx: i32, my: i32) -> Option<(usize, usize)> {
        let status_h    = CHAR_H + 4;
        let content_y   = wy + status_h;
        let content_h   = wh.saturating_sub(status_h);
        let line_no_w   = 4 * CHAR_W;
        let text_x      = wx + line_no_w;
        let bottom_bar_y = wy + wh.saturating_sub(CHAR_H + 2);

        if mx < wx as i32 || mx >= (wx + ww) as i32 { return None; }
        if my < content_y as i32 || my >= bottom_bar_y as i32 { return None; }

        let visible_rows = (content_h / CHAR_H).max(1);
        let r = ((my - content_y as i32) as usize / CHAR_H).min(visible_rows.saturating_sub(1));
        let line_idx = (self.scroll_row + r).min(self.lines.len().saturating_sub(1));

        let rel_x = mx - (text_x as i32 + 2);
        let col = if rel_x < 0 { 0 } else { (rel_x as usize / CHAR_W).min(self.lines[line_idx].len()) };
        Some((line_idx, col))
    }

    // ── Clipboard ──────────────────────────────────────────────────────────

    /// Copies the current selection (if any) to the system clipboard. No-op
    /// if nothing is selected — matches every other editor's Ctrl+C behavior.
    fn copy_selection(&self) {
        let Some((s, e)) = self.selection_range() else { return; };
        let mut buf: Vec<u8> = Vec::new();
        if s.0 == e.0 {
            buf.extend_from_slice(&self.lines[s.0][s.1..e.1]);
        } else {
            buf.extend_from_slice(&self.lines[s.0][s.1..]);
            for row in &self.lines[s.0 + 1..e.0] {
                buf.push(b'\n');
                buf.extend_from_slice(row);
            }
            buf.push(b'\n');
            buf.extend_from_slice(&self.lines[e.0][..e.1]);
        }
        crate::clipboard::set(&buf);
    }

    /// Inserts raw text at the cursor, splitting into new lines on `\n` —
    /// shared by paste and (indirectly) by typed input.
    fn insert_text(&mut self, text: &[u8]) {
        for &b in text {
            if b == b'\n' {
                let rest = self.lines[self.cursor_row].split_off(self.cursor_col);
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, rest);
                self.cursor_col = 0;
            } else if b >= 32 && b < 128 {
                self.lines[self.cursor_row].insert(self.cursor_col, b);
                self.cursor_col += 1;
            }
        }
        self.ensure_visible();
        self.modified = true;
    }

    /// Replaces the selection (if any) with the clipboard's contents.
    fn paste_clipboard(&mut self) {
        self.delete_selection_if_any();
        let text = crate::clipboard::get();
        self.insert_text(&text);
    }

    /// Entry point for the right-click Copy/Paste context menu (main.rs
    /// doesn't have access to the private copy_selection/paste_clipboard).
    pub fn clipboard_action(&mut self, is_paste: bool) {
        if is_paste { self.paste_clipboard(); } else { self.copy_selection(); }
    }

    pub fn on_key(&mut self, c: char) {
        use crate::ps2;

        // Ctrl+F: enter find mode (or find next if already in find mode)
        if c as u8 == 0x06 {
            if !self.find_mode {
                self.find_mode = true;
                self.find_query.clear();
                self.find_matches.clear();
                self.find_idx = 0;
            } else {
                self.find_next();
            }
            return;
        }

        // In find mode: route keys to query building
        if self.find_mode {
            match c as u8 {
                0x1B => { // ESC: exit find mode (don't close editor)
                    self.find_mode = false;
                    self.find_query.clear();
                    self.find_matches.clear();
                    self.status_msg = String::from("F2/Ctrl+S=save  F10=close  Ctrl+F=find");
                    self.status_ok = true;
                }
                b'\n' | 0x07 => { self.find_next(); } // Enter or Ctrl+G: next match
                b'\x08' => {
                    self.find_query.pop();
                    self.find_all();
                    self.find_jump_nearest();
                }
                ch if ch >= 32 && ch < 128 => {
                    self.find_query.push(ch);
                    self.find_all();
                    self.find_jump_nearest();
                }
                _ => {}
            }
            return;
        }

        match c as u8 {
            // Ctrl+S or F2 → save
            0x13 => { self.save(); }
            b if b == ps2::KEY_F2 => { self.save(); }

            // Ctrl+C (0x03) or Ctrl+Shift+C ('C' — shift suppresses the ctrl
            // lowercase→control-code conversion in ps2.rs) → copy selection
            0x03 => { self.copy_selection(); }
            b'C' if ps2::ctrl_held() => { self.copy_selection(); }

            // Ctrl+V (0x16) or Ctrl+Shift+V ('V', same reasoning) → paste
            0x16 => { self.paste_clipboard(); }
            b'V' if ps2::ctrl_held() => { self.paste_clipboard(); }

            // Ctrl+Q or ESC or F10 → close
            0x11 | 0x1B => {
                if self.modified {
                    self.status_msg = String::from("Unsaved! Press again to force close");
                    self.status_ok = false;
                    self.modified = false;
                } else {
                    self.open = false;
                }
            }
            b if b == ps2::KEY_F10 => { self.open = false; }

            // Arrow keys — Shift extends the selection, otherwise it's cleared
            b if b == ps2::KEY_UP => {
                self.update_selection_anchor();
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.clamp_col();
                    self.ensure_visible();
                }
            }
            b if b == ps2::KEY_DOWN => {
                self.update_selection_anchor();
                if self.cursor_row + 1 < self.lines.len() {
                    self.cursor_row += 1;
                    self.clamp_col();
                    self.ensure_visible();
                }
            }
            b if b == ps2::KEY_LEFT => {
                self.update_selection_anchor();
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.lines[self.cursor_row].len();
                    self.ensure_visible();
                }
            }
            b if b == ps2::KEY_RIGHT => {
                self.update_selection_anchor();
                let len = self.lines[self.cursor_row].len();
                if self.cursor_col < len {
                    self.cursor_col += 1;
                } else if self.cursor_row + 1 < self.lines.len() {
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                    self.ensure_visible();
                }
            }
            b if b == ps2::KEY_HOME => {
                self.update_selection_anchor();
                if crate::ps2::ctrl_held() {
                    // Ctrl+Home → file start
                    self.cursor_row = 0; self.cursor_col = 0; self.scroll_row = 0;
                } else {
                    self.cursor_col = 0;
                }
            }
            b if b == ps2::KEY_END => {
                self.update_selection_anchor();
                if crate::ps2::ctrl_held() {
                    // Ctrl+End → file end
                    self.cursor_row = self.lines.len().saturating_sub(1);
                    self.cursor_col = self.lines[self.cursor_row].len();
                    self.ensure_visible();
                } else {
                    self.cursor_col = self.lines[self.cursor_row].len();
                }
            }
            b if b == ps2::KEY_PGUP => {
                self.update_selection_anchor();
                let page = self.visible_rows_hint.max(1);
                self.cursor_row = self.cursor_row.saturating_sub(page);
                self.clamp_col();
                self.ensure_visible();
            }
            b if b == ps2::KEY_PGDN => {
                self.update_selection_anchor();
                let page = self.visible_rows_hint.max(1);
                self.cursor_row = (self.cursor_row + page).min(self.lines.len().saturating_sub(1));
                self.clamp_col();
                self.ensure_visible();
            }

            // Enter → split line (replaces selection first, if any)
            b'\n' => {
                self.delete_selection_if_any();
                let rest = self.lines[self.cursor_row].split_off(self.cursor_col);
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, rest);
                self.cursor_col = 0;
                self.ensure_visible();
                self.modified = true;
            }

            // Backspace — deletes the selection instead of one char, if any
            b'\x08' => {
                if self.delete_selection_if_any() {
                    self.ensure_visible();
                } else if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    self.lines[self.cursor_row].remove(self.cursor_col);
                    self.modified = true;
                } else if self.cursor_row > 0 {
                    // Merge with previous line
                    let cur = self.lines.remove(self.cursor_row);
                    self.cursor_row -= 1;
                    self.cursor_col = self.lines[self.cursor_row].len();
                    self.lines[self.cursor_row].extend_from_slice(&cur);
                    self.ensure_visible();
                    self.modified = true;
                }
            }

            // Delete (DEL key) — deletes the selection instead of one char, if any
            b if b == ps2::KEY_DEL => {
                if self.delete_selection_if_any() {
                    self.ensure_visible();
                } else {
                    let len = self.lines[self.cursor_row].len();
                    if self.cursor_col < len {
                        self.lines[self.cursor_row].remove(self.cursor_col);
                        self.modified = true;
                    } else if self.cursor_row + 1 < self.lines.len() {
                        let next = self.lines.remove(self.cursor_row + 1);
                        self.lines[self.cursor_row].extend_from_slice(&next);
                        self.modified = true;
                    }
                }
            }

            // Tab → 4 spaces (replaces selection first, if any)
            b'\t' => {
                self.delete_selection_if_any();
                for _ in 0..4 {
                    self.lines[self.cursor_row].insert(self.cursor_col, b' ');
                    self.cursor_col += 1;
                }
                self.modified = true;
            }

            // Printable characters (replace selection first, if any)
            ch if ch >= 32 && ch < 128 => {
                self.delete_selection_if_any();
                self.lines[self.cursor_row].insert(self.cursor_col, ch);
                self.cursor_col += 1;
                self.modified = true;
            }

            _ => {}
        }
    }

    fn clamp_col(&mut self) {
        let max = self.lines[self.cursor_row].len();
        if self.cursor_col > max { self.cursor_col = max; }
    }

    fn ensure_visible(&mut self) {
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        }
    }

    fn save(&mut self) {
        let mut data: Vec<u8> = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            data.extend_from_slice(line);
            if i + 1 < self.lines.len() { data.push(b'\n'); }
        }

        let mut ctrl = crate::nvme::CONTROLLER.lock();
        if let Some(ctrl) = ctrl.as_mut() {
            // Find or create file
            let path = self.path.clone();
            let ino = crate::hepfs::lookup(ctrl, &path).unwrap_or_else(|| {
                // Create in root if simple name
                let name = path.trim_start_matches('/');
                crate::hepfs::create_file(ctrl, crate::hepfs::ROOT_INO, name)
            });
            crate::hepfs::write_file(ctrl, ino, &data);
            self.modified = false;
            self.status_msg = alloc::format!("Saved {} ({} bytes)", self.path, data.len());
            self.status_ok = true;
        } else {
            self.status_msg = String::from("ERROR: no NVMe controller");
            self.status_ok = false;
        }
    }

    pub fn render(&mut self, display: &mut Display, wx: usize, wy: usize, ww: usize, wh: usize) {
        // Status bar at top (1 row)
        let status_h = CHAR_H + 4;
        display.fill_rect(wx, wy, ww, status_h, STATUS);

        // File name + modified indicator
        let name = self.path.trim_start_matches('/');
        let indicator = if self.modified { " [modified]" } else { "" };
        let title = alloc::format!(" {} {}", name, indicator);
        display.draw_text(wx + 2, wy + 2, &title,
            if self.modified { MODIFIED } else { TEXT }, SCALE);

        // Status message (right side)
        let msg_x = wx + ww.saturating_sub(self.status_msg.len() * CHAR_W + 4);
        display.draw_text(msg_x, wy + 2, &self.status_msg,
            if self.status_ok { DIM } else { ERR }, SCALE);

        // Content area
        let content_y = wy + status_h;
        let content_h = wh.saturating_sub(status_h);
        display.fill_rect(wx, content_y, ww, content_h, BG);

        let line_no_w = 4 * CHAR_W; // "NNN " prefix
        let text_x = wx + line_no_w;
        let text_w = ww.saturating_sub(line_no_w);

        let visible_rows = content_h / CHAR_H;
        self.visible_rows_hint = visible_rows.max(1);

        // Adjust scroll so cursor is visible
        if self.cursor_row >= self.scroll_row + visible_rows {
            self.scroll_row = self.cursor_row + 1 - visible_rows;
        }
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        }

        for r in 0..visible_rows {
            let line_idx = self.scroll_row + r;
            let py = content_y + r * CHAR_H;

            // Line number gutter
            display.fill_rect(wx, py, line_no_w - 2, CHAR_H, LINE_COL);
            if line_idx < self.lines.len() {
                let n = alloc::format!("{:3}", line_idx + 1);
                display.draw_text(wx + 2, py + 1, &n, DIM, SCALE);
            }

            if line_idx >= self.lines.len() { continue; }
            let line = &self.lines[line_idx];

            // Draw cursor row highlight
            if line_idx == self.cursor_row {
                display.fill_rect(text_x, py, text_w, CHAR_H, Color::from_hex(0x12122A));
            }

            // Draw text-selection highlight (drag / Shift+movement)
            if let Some((sel_start, sel_end)) = self.selection_cols_for_line(line_idx, line.len()) {
                let sel_x = text_x + 2 + sel_start * CHAR_W;
                let sel_w = (sel_end - sel_start) * CHAR_W;
                if sel_x < wx + ww {
                    let sel_w = sel_w.min(wx + ww - sel_x);
                    display.fill_rect(sel_x, py, sel_w, CHAR_H, SELECTION);
                }
            }

            // Per-character syntax colors (plain default color if this file
            // type isn't one we highlight — see supports_highlighting()).
            let default_col = if line_idx == self.cursor_row { TEXT } else { Color::from_hex(0xCCCCCC) };
            let line_colors = if self.highlight {
                highlight_line(line, default_col)
            } else {
                alloc::vec![default_col; line.len()]
            };

            // Draw text
            let mut px = text_x + 2;
            let qlen = self.find_query.len();
            for (ci, &ch) in line.iter().enumerate() {
                if px + CHAR_W > wx + ww { break; }
                // Find-mode highlight
                let is_current_match = self.find_mode && qlen > 0 && {
                    self.find_idx < self.find_matches.len() && {
                        let (mr, mc) = self.find_matches[self.find_idx];
                        mr == line_idx && ci >= mc && ci < mc + qlen
                    }
                };
                let is_other_match = !is_current_match && self.find_mode && qlen > 0 && {
                    self.find_matches.iter().any(|&(mr, mc)| {
                        mr == line_idx && ci >= mc && ci < mc + qlen
                    })
                };
                if is_current_match {
                    display.fill_rect(px, py, CHAR_W, CHAR_H, FIND_CURRENT);
                } else if is_other_match {
                    display.fill_rect(px, py, CHAR_W, CHAR_H, FIND_HIT);
                }
                if ch > b' ' {
                    let s = core::str::from_utf8(core::slice::from_ref(&ch)).unwrap_or("?");
                    let col = if is_current_match { BG } else { line_colors[ci] };
                    display.draw_text(px, py + 1, s, col, SCALE);
                }
                // Draw cursor underline
                if line_idx == self.cursor_row && ci == self.cursor_col {
                    display.fill_rect(px, py + CHAR_H - 2, CHAR_W - 1, 2, CURSOR_C);
                }
                px += CHAR_W;
            }
            // Cursor at end of line
            if line_idx == self.cursor_row && self.cursor_col == line.len() {
                let end_x = text_x + 2 + line.len() * CHAR_W;
                if end_x + CHAR_W <= wx + ww {
                    display.fill_rect(end_x, py + CHAR_H - 2, CHAR_W - 1, 2, CURSOR_C);
                }
            }
        }

        // Bottom bar: find mode or position info
        let py = wy + wh.saturating_sub(CHAR_H + 2);
        display.fill_rect(wx, py, ww, CHAR_H + 2, STATUS);
        if self.find_mode {
            let q = core::str::from_utf8(&self.find_query).unwrap_or("?");
            let n = self.find_matches.len();
            let i = if n > 0 { self.find_idx + 1 } else { 0 };
            let bar = alloc::format!("Find: {}  [{}/{}]  Enter/Ctrl+G=next  ESC=close", q, i, n);
            let bar_color = if n == 0 && !self.find_query.is_empty() { ERR } else { OK };
            display.draw_text(wx + 4, py + 1, &bar, bar_color, SCALE);
        } else {
            let pos = alloc::format!("{}:{}", self.cursor_row + 1, self.cursor_col + 1);
            let px = wx + ww.saturating_sub(pos.len() * CHAR_W + 4);
            display.draw_text(px, py + 1, &pos, DIM, SCALE);
            let nlines = alloc::format!("{} lines", self.lines.len());
            let lines_str = if self.lines.len() == 1 { "1 line" } else { &nlines };
            display.draw_text(wx + 4, py + 1, lines_str, DIM, SCALE);
        }
    }
}

pub static EDITOR: Mutex<Option<Editor>> = Mutex::new(None);

/// Additional editor instances keyed by their window ID — lets multiple
/// files be open for editing simultaneously (window IDs assigned by the
/// desktop when spawned), the same pattern as `terminal::EXTRA_TERMINALS`.
pub static EXTRA_EDITORS: Mutex<alloc::vec::Vec<(usize, Editor)>> = Mutex::new(alloc::vec::Vec::new());

fn read_content(path: &str) -> Vec<u8> {
    let mut ctrl = crate::nvme::CONTROLLER.lock();
    if let Some(ctrl) = ctrl.as_mut() {
        match crate::hepfs::lookup(ctrl, path) {
            Some(ino) => crate::hepfs::read_file(ctrl, ino),
            None      => alloc::vec![],
        }
    } else { alloc::vec![] }
}

/// Open a file in the main Editor window (id=3) — called from terminal
/// `edit` command / HepFS click when the main editor is free to reuse.
pub fn open(path: &str) {
    let content = read_content(path);
    *EDITOR.lock() = Some(Editor::new(path, &content));
}

/// Open a file in a brand-new floating editor window (used when the main
/// Editor window is already showing something else). Returns the new window ID.
pub fn spawn_editor(path: &str) -> usize {
    let content = read_content(path);
    let count = EXTRA_EDITORS.lock().len();
    let off   = (count + 1) as i32 * 24;
    let win_id = {
        let mut dt = crate::desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            let id = dt.add_window(crate::desktop::AppKind::Editor,
                &alloc::format!("Editor: {}", path.trim_start_matches('/')),
                140 + off, 60 + off, 580, 380);
            dt.dirty = true;
            id
        } else {
            return usize::MAX;
        }
    };
    EXTRA_EDITORS.lock().push((win_id, Editor::new(path, &content)));
    win_id
}

/// Spawn a brand-new blank/untitled editor window — used by the taskbar/start-menu
/// "New Window" context-menu action, which has no specific file to open.
pub fn spawn_editor_blank() -> usize {
    let count = EXTRA_EDITORS.lock().len();
    let off   = (count + 1) as i32 * 24;
    let win_id = {
        let mut dt = crate::desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            let id = dt.add_window(crate::desktop::AppKind::Editor, "Editor",
                140 + off, 60 + off, 580, 380);
            dt.dirty = true;
            id
        } else {
            return usize::MAX;
        }
    };
    EXTRA_EDITORS.lock().push((win_id, Editor::new("/untitled.txt", &[])));
    win_id
}

/// Open `path` for editing: reuses the main Editor window (id=3) if it's
/// currently minimized (free), otherwise spawns a brand-new editor window —
/// this is how multiple files end up open for editing at once. Un-minimizes,
/// focuses, and brings the target window to front. Returns the window ID used.
pub fn open_smart(path: &str) -> usize {
    let main_free = {
        let dt = crate::desktop::DESKTOP.lock();
        dt.as_ref()
            .and_then(|d| d.windows.iter().find(|w| w.id == 3))
            .map(|w| w.minimized)
            .unwrap_or(true)
    };
    let win_id = if main_free {
        open(path);
        3
    } else {
        spawn_editor(path)
    };
    {
        let mut dt = crate::desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == win_id) {
                w.minimized = false;
            }
            dt.bring_to_front(win_id);
            dt.dirty = true;
        }
    }
    *crate::FOCUSED_WIN.lock() = Some(win_id);
    win_id
}
