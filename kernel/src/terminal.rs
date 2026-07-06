//! VT100-subset terminal emulator.
//! Renders text into a window content area on the HepOS desktop.

use alloc::{vec::Vec, string::String};
use spin::Mutex;
use crate::{framebuffer::{Color, Display}, ps2};

// Terminal dimensions (in character cells) — scale 2 (18×18 px per cell)
const SCALE:        usize = 2;
const MAX_COLS:     usize = 120;  // maximum cols (cell array width)
const DEFAULT_COLS: usize = 30;   // initial cols before first render
const SCROLLBACK:   usize = 200;
const CHAR_W:       usize = 9 * SCALE + 1;   // 19 px per column
const CHAR_H:       usize = 8 * SCALE + 2;   // 18 px per row

// Palette
const TEXT:   Color = Color::from_hex(0xE8E8E8);
const DIM:    Color = Color::from_hex(0x888888);
const BG:     Color = Color::from_hex(0x0C0C0C);
const CURSOR: Color = Color::from_hex(0x6C8EFF);
const ERR:    Color = Color::from_hex(0xFF6B6B);
const OK:     Color = Color::from_hex(0x6BFF8E);
const SELECTION: Color = Color::from_hex(0x264F78); // text selection background

// Known command verbs — shared by tab-completion and live input highlighting
// (reuses editor.rs's generic tokenizer with these as the "keyword" list).
const COMMAND_NAMES: &[&str] = &[
    "help", "clear", "pwd", "ls", "cd", "cat", "mkdir", "touch",
    "rm", "cp", "mv", "write", "edit", "uname", "mem", "date",
    "history", "lspci", "netdiag", "netstart", "netpoll", "ifconfig",
    "ping", "wget", "udp", "view", "play", "shutdown", "reboot", "echo",
    "sysinfo", "syscallinfo", "runtest", "runhello", "exec", "ps", "newterm", "beep",
];

#[derive(Clone, Copy)]
struct Cell {
    ch:    u8,
    color: Color,
}

impl Cell {
    const fn blank() -> Self { Self { ch: b' ', color: TEXT } }
}

pub struct Terminal {
    lines:       Vec<[Cell; MAX_COLS]>,
    cols:        usize,   // current usable column count (≤ MAX_COLS), updated by render()
    col:         usize,
    row:         usize,
    pub dirty:   bool,
    cmd_buf:     String,
    cmd_cursor:  usize,   // position within cmd_buf (0 = before first char)
    prompt_row:  usize,
    prompt_col:  usize,   // column where user input starts (after the prompt text)
    // Shell state
    cwd_ino:     u32,
    cwd_path:    String,
    // History
    history:     Vec<String>,
    history_idx: Option<usize>,
    // Text selection — (absolute line idx, col). `select_head` is the other
    // end (updated by mouse drag or Shift+Left/Right); a real selection only
    // exists once the two differ (see `selection_range()`).
    select_anchor: Option<(usize, usize)>,
    select_head:   Option<(usize, usize)>,
    // Set every render() call — lets hit_test() replicate the same
    // start-row/visible-rows math used to lay out the last frame.
    render_start: usize,
    render_rows:  usize,
}

impl Terminal {
    pub fn new() -> Self {
        let mut lines = Vec::new();
        for _ in 0..SCROLLBACK {
            lines.push([Cell::blank(); MAX_COLS]);
        }
        let mut t = Terminal {
            lines,
            cols: DEFAULT_COLS,
            col: 0, row: 0, dirty: true,
            cmd_buf: String::new(), cmd_cursor: 0,
            prompt_row: 0, prompt_col: 0,
            cwd_ino: crate::hepfs::ROOT_INO,
            cwd_path: String::from("/"),
            history: Vec::new(),
            history_idx: None,
            select_anchor: None,
            select_head: None,
            render_start: 0,
            render_rows: 1,
        };
        t.print_colored("HepOS Terminal v0.1\n", OK);
        t.print_colored("Type 'help' for commands\n\n", DIM);
        t.show_prompt();
        t
    }

    fn show_prompt(&mut self) {
        self.print_colored(&alloc::format!("{} $ ", self.cwd_path), CURSOR);
        self.prompt_row = self.row;
        self.prompt_col = self.col;
        self.cmd_cursor = 0;
    }

    fn print_colored(&mut self, s: &str, color: Color) {
        for ch in s.chars() {
            self.put_char(ch as u8, color);
        }
        self.dirty = true;
    }

    fn print(&mut self, s: &str) {
        self.print_colored(s, TEXT);
    }

    /// Drain the process output capture buffer and print it to this terminal.
    fn flush_proc_output(&mut self) {
        let bytes = crate::process::take_proc_output();
        if bytes.is_empty() { return; }
        if let Ok(s) = core::str::from_utf8(&bytes) {
            self.print(s);
        } else {
            // Non-UTF-8: print printable ASCII, replace others with '?'
            for &b in &bytes {
                self.put_char(if b.is_ascii_graphic() || b == b'\n' || b == b' ' { b } else { b'?' }, TEXT);
            }
        }
    }

    fn put_char(&mut self, ch: u8, color: Color) {
        match ch {
            b'\n' => {
                self.col = 0;
                self.advance_row();
            }
            b'\r' => {
                self.col = 0;
            }
            _ if ch >= 32 => {
                if self.row < self.lines.len() {
                    self.lines[self.row][self.col] = Cell { ch, color };
                }
                self.col += 1;
                if self.col >= self.cols {
                    self.col = 0;
                    self.advance_row();
                }
            }
            _ => {}
        }
    }

    fn advance_row(&mut self) {
        self.row += 1;
        if self.row >= SCROLLBACK {
            self.lines.remove(0);
            self.lines.push([Cell::blank(); MAX_COLS]);
            self.row = SCROLLBACK - 1;
        }
    }

    // ── Selection ──────────────────────────────────────────────────────────

    /// Normalized (start, end) selection range, or `None` if there's no real
    /// selection (anchor equals head, or either endpoint isn't set).
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.select_anchor?;
        let h = self.select_head?;
        if a == h { return None; }
        Some(if a <= h { (a, h) } else { (h, a) })
    }

    /// Column range `[start, end)` selected on absolute line `line_idx`, if any.
    fn selection_cols_for_line(&self, line_idx: usize) -> Option<(usize, usize)> {
        let (s, e) = self.selection_range()?;
        if line_idx < s.0 || line_idx > e.0 { return None; }
        let start = if line_idx == s.0 { s.1 } else { 0 };
        let end   = if line_idx == e.0 { e.1 } else { self.cols };
        if start >= end { return None; }
        Some((start, end))
    }

    /// Called before Left/Right move the input cursor: extends the selection
    /// if Shift is held (anchoring at the pre-move position the first time),
    /// or clears it otherwise.
    fn update_selection_anchor(&mut self) {
        if ps2::shift_held() {
            if self.select_anchor.is_none() {
                self.select_anchor = Some((self.row, self.col));
            }
        } else {
            self.select_anchor = None;
            self.select_head = None;
        }
    }

    /// Called after Left/Right actually move the input cursor — extends the
    /// selection head to the new position, if a selection is in progress.
    fn sync_selection_head(&mut self) {
        if self.select_anchor.is_some() {
            self.select_head = Some((self.row, self.col));
        }
    }

    /// Mouse button pressed at (row, col) — anchors a potential drag selection
    /// (only becomes visible once the mouse actually moves elsewhere).
    pub fn mouse_down(&mut self, row: usize, col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let col = col.min(self.cols.saturating_sub(1));
        self.select_anchor = Some((row, col));
        self.select_head = Some((row, col));
    }

    /// Mouse dragged (button still held) to (row, col) — extends the
    /// selection from wherever `mouse_down` anchored it.
    pub fn mouse_drag(&mut self, row: usize, col: usize) {
        if self.select_anchor.is_some() {
            let row = row.min(self.lines.len().saturating_sub(1));
            let col = col.min(self.cols.saturating_sub(1));
            self.select_head = Some((row, col));
        }
    }

    /// Pixel coordinates (window-relative) → (absolute line idx, col).
    /// Mirrors `render()`'s exact layout math via the hints it stores.
    /// Returns `None` for clicks outside the content area.
    pub fn hit_test(&self, wx: usize, wy: usize, ww: usize, wh: usize, mx: i32, my: i32) -> Option<(usize, usize)> {
        if mx < wx as i32 || mx >= (wx + ww) as i32 { return None; }
        if my < wy as i32 || my >= (wy + wh) as i32 { return None; }

        let visible_rows = self.render_rows.max(1);
        let r = (((my - wy as i32 - 4).max(0)) as usize / CHAR_H).min(visible_rows.saturating_sub(1));
        let line_idx = (self.render_start + r).min(self.lines.len().saturating_sub(1));

        let c = ((mx - wx as i32 - 4).max(0)) as usize / CHAR_W;
        let col = c.min(self.cols.saturating_sub(1));
        Some((line_idx, col))
    }

    /// Selects the entire line at absolute index `row`, trimmed of trailing
    /// blank cells — double-click convenience. A wholly-blank line selects
    /// nothing (start == end collapses to no selection, same as elsewhere).
    pub fn select_line(&mut self, row: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let mut end = self.cols;
        while end > 0 && self.lines[row][end - 1].ch == b' ' { end -= 1; }
        self.select_anchor = Some((row, 0));
        self.select_head = Some((row, end));
    }

    // ── Clipboard ──────────────────────────────────────────────────────────

    /// Copies the current selection (if any) to the system clipboard.
    pub fn copy_selection(&self) {
        let Some((s, e)) = self.selection_range() else { return; };
        let mut buf: Vec<u8> = Vec::new();
        for line_idx in s.0..=e.0 {
            if line_idx >= self.lines.len() { break; }
            let (cs, ce) = if line_idx == s.0 && line_idx == e.0 { (s.1, e.1) }
                else if line_idx == s.0 { (s.1, self.cols) }
                else if line_idx == e.0 { (0, e.1) }
                else { (0, self.cols) };
            let line = &self.lines[line_idx];
            for col in cs..ce.min(self.cols) { buf.push(line[col].ch); }
            while buf.last() == Some(&b' ') { buf.pop(); }
            if line_idx != e.0 { buf.push(b'\n'); }
        }
        crate::clipboard::set(&buf);
    }

    /// Inserts the clipboard's contents into the current input line at the
    /// cursor (newlines are dropped — terminal input is always single-line).
    pub fn paste_clipboard(&mut self) {
        for b in crate::clipboard::get() {
            if b == b'\n' || b == b'\r' { continue; }
            if b >= 32 && b < 128 && self.cmd_buf.len() < self.cols.saturating_sub(2) {
                if self.cmd_cursor == self.cmd_buf.len() {
                    self.cmd_buf.push(b as char);
                    self.put_char(b, TEXT);
                    self.cmd_cursor += 1;
                } else {
                    self.cmd_buf.insert(self.cmd_cursor, b as char);
                    let row = self.row;
                    let c = self.col;
                    for i in (c..self.cols - 1).rev() { self.lines[row][i + 1] = self.lines[row][i]; }
                    self.lines[row][c] = Cell { ch: b, color: TEXT };
                    self.col += 1;
                    self.cmd_cursor += 1;
                }
            }
        }
        self.recolor_input();
        self.dirty = true;
    }

    /// Handle a keypress from PS/2.
    pub fn on_key(&mut self, c: char) {
        // Any key other than Left/Right (which manage selection themselves
        // via update_selection_anchor/sync_selection_head) clears an
        // in-progress selection — matches normal editor/terminal behavior.
        // Copy (Ctrl+Shift+C) is exempt so the selection survives to be read
        // below, and stays visible afterward like any other editor's Ctrl+C.
        let is_copy_combo = c == 'C' && ps2::ctrl_held();
        if c as u8 != ps2::KEY_LEFT && c as u8 != ps2::KEY_RIGHT && !is_copy_combo {
            self.select_anchor = None;
            self.select_head = None;
        }
        match c as u8 {
            b'\n' => {
                self.put_char(b'\n', TEXT);
                let cmd = alloc::string::String::from(self.cmd_buf.trim());
                self.cmd_buf.clear();
                self.cmd_cursor = 0;
                self.history_idx = None;
                if !cmd.is_empty() {
                    self.history.push(cmd.clone());
                    if self.history.len() > 50 { self.history.remove(0); }
                }
                self.execute(&cmd);
                self.show_prompt();
            }
            b'\x08' => { // backspace — delete char before cursor
                if self.cmd_cursor > 0 {
                    self.cmd_buf.remove(self.cmd_cursor - 1);
                    self.cmd_cursor -= 1;
                    if self.col > 0 { self.col -= 1; }
                    // Shift all cells from col leftward to close the gap
                    let row = self.row;
                    let c = self.col;
                    for i in c..self.cols - 1 { self.lines[row][i] = self.lines[row][i + 1]; }
                    self.lines[row][self.cols - 1] = Cell::blank();
                }
            }
            b'\x01' => { // Ctrl+A — jump to start of input
                self.col = self.prompt_col;
                self.cmd_cursor = 0;
            }
            b'\x05' => { // Ctrl+E — jump to end of input
                self.cmd_cursor = self.cmd_buf.len();
                self.col = self.prompt_col + self.cmd_cursor;
            }
            b'\x03' => { // Ctrl+C — cancel current input
                self.cmd_buf.clear();
                self.print_colored("^C\n", ERR);
                self.show_prompt();
            }
            // Ctrl+Shift+C ('C' — shift suppresses the ctrl lowercase→control-code
            // conversion in ps2.rs, so this is distinct from plain Ctrl+C above) → copy
            b'C' if ps2::ctrl_held() => { self.copy_selection(); }
            // Ctrl+Shift+V (same reasoning) → paste. Ctrl+V alone (0x16) isn't
            // otherwise bound in the terminal, so it pastes too.
            b'V' if ps2::ctrl_held() => { self.paste_clipboard(); }
            0x16 => { self.paste_clipboard(); } // Ctrl+V
            b'\x0C' | b'\x0B' => { // Ctrl+L or Ctrl+K = clear
                for line in &mut self.lines { *line = [Cell::blank(); MAX_COLS]; }
                self.col = 0; self.row = 0;
                self.show_prompt();
            }
            b'\t' => { self.tab_complete(); }

            ps2::KEY_UP | 0x10 => { // UP arrow or Ctrl+P — history previous
                if self.history.is_empty() { self.dirty = true; return; }
                let new_idx = match self.history_idx {
                    None    => self.history.len() - 1,
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                self.history_idx = Some(new_idx);
                let entry = self.history[new_idx].clone();
                self.replace_input(&entry);
            }
            ps2::KEY_DOWN | 0x0E => { // DOWN arrow or Ctrl+N — history next
                let action = match self.history_idx {
                    None => None,
                    Some(i) if i + 1 >= self.history.len() => Some(None),
                    Some(i) => Some(Some(i + 1)),
                };
                match action {
                    None => {}
                    Some(None) => { self.history_idx = None; self.replace_input(""); }
                    Some(Some(i)) => {
                        self.history_idx = Some(i);
                        let entry = self.history[i].clone();
                        self.replace_input(&entry);
                    }
                }
            }
            ps2::KEY_LEFT => { // move cursor left within input (Shift extends selection)
                self.update_selection_anchor();
                if self.cmd_cursor > 0 {
                    self.cmd_cursor -= 1;
                    self.col -= 1;
                }
                self.sync_selection_head();
            }
            ps2::KEY_RIGHT => { // move cursor right within input (Shift extends selection)
                self.update_selection_anchor();
                if self.cmd_cursor < self.cmd_buf.len() {
                    self.cmd_cursor += 1;
                    self.col += 1;
                }
                self.sync_selection_head();
            }
            ch if ch >= 32 => {
                if self.cmd_buf.len() < self.cols.saturating_sub(2) {
                    if self.cmd_cursor == self.cmd_buf.len() {
                        // At end — just append
                        self.cmd_buf.push(ch as char);
                        self.put_char(ch, TEXT);
                        self.cmd_cursor += 1;
                    } else {
                        // Mid-line — insert and shift cells right
                        self.cmd_buf.insert(self.cmd_cursor, ch as char);
                        let row = self.row;
                        let c = self.col;
                        for i in (c..self.cols - 1).rev() { self.lines[row][i + 1] = self.lines[row][i]; }
                        self.lines[row][c] = Cell { ch, color: TEXT };
                        self.col += 1;
                        self.cmd_cursor += 1;
                    }
                }
            }
            _ => {}
        }
        // Re-tokenize and recolor the input line on every keystroke that could
        // have changed it — cheap (short line, no allocation beyond one Vec).
        self.recolor_input();
        self.dirty = true;
    }

    /// Recolor the current input line in place, live as the user types —
    /// command name / known verbs, quoted strings, and numbers get distinct
    /// colors (reuses editor.rs's generic per-line tokenizer with
    /// `COMMAND_NAMES` standing in for language keywords).
    fn recolor_input(&mut self) {
        let colors = crate::editor::highlight_line_kw(self.cmd_buf.as_bytes(), COMMAND_NAMES, TEXT);
        let row = self.prompt_row;
        for (i, color) in colors.iter().enumerate() {
            let col = self.prompt_col + i;
            if col >= self.cols || row >= self.lines.len() { break; }
            self.lines[row][col].color = *color;
        }
    }

    fn replace_input(&mut self, new: &str) {
        // Erase entire current input by blanking the cells from prompt_col
        let row = self.prompt_row;
        let end = self.prompt_col + self.cmd_buf.len();
        for c in self.prompt_col..end.min(self.cols) { self.lines[row][c] = Cell::blank(); }
        self.col = self.prompt_col;
        self.cmd_buf.clear();
        self.cmd_cursor = 0;
        for ch in new.bytes() {
            self.cmd_buf.push(ch as char);
            self.put_char(ch, TEXT);
            self.cmd_cursor += 1;
        }
    }

    fn tab_complete(&mut self) {
        let partial = self.cmd_buf.clone();

        // Split into the word-before-last-space and the word being completed
        let (prefix, word) = match partial.rfind(' ') {
            None       => ("", partial.as_str()),   // completing the command verb
            Some(pos)  => (&partial[..=pos], &partial[pos + 1..]),
        };

        let matches: alloc::vec::Vec<alloc::string::String> = if prefix.is_empty() {
            // Completing command name
            COMMAND_NAMES.iter()
                .filter(|c| c.starts_with(word))
                .map(|c| alloc::string::String::from(*c))
                .collect()
        } else {
            // Completing file/directory name
            let cwd = self.cwd_ino;
            self.with_ctrl(|ctrl| {
                crate::hepfs::list_dir(ctrl, cwd)
                    .into_iter()
                    .filter(|(_, n)| n.starts_with(word))
                    .map(|(_, n)| n)
                    .collect()
            })
        };

        match matches.len() {
            0 => { /* no match — do nothing */ }
            1 => {
                // Single match: complete in-place with a trailing space
                let completed = alloc::format!("{}{} ", prefix, matches[0]);
                self.replace_input(&completed);
            }
            _ => {
                // Multiple matches: show them on a new line, then re-display prompt + partial
                self.print_colored("\n", DIM);
                for m in &matches {
                    self.print_colored(m, OK);
                    self.print_colored("  ", DIM);
                }
                self.print_colored("\n", DIM);
                self.cmd_buf.clear();
                self.cmd_cursor = 0;
                self.show_prompt();
                // Re-show what the user had typed
                let p = partial.clone();
                for ch in p.bytes() {
                    self.cmd_buf.push(ch as char);
                    self.put_char(ch, TEXT);
                    self.cmd_cursor += 1;
                }
            }
        }
    }

    fn execute(&mut self, cmd: &str) {
        let parts: alloc::vec::Vec<&str> = cmd.splitn(3, ' ').collect();
        let verb = parts.first().copied().unwrap_or("");
        let arg1 = parts.get(1).copied().unwrap_or("");
        let arg2 = parts.get(2).copied().unwrap_or("");

        match verb {
            "" => {}

            "help" => {
                self.print_colored("Commands:\n", OK);
                let cmds = [
                    ("help",           "this message"),
                    ("clear",          "clear screen"),
                    ("pwd",            "print working directory"),
                    ("ls [path]",      "list directory"),
                    ("cd <dir>",       "change directory"),
                    ("cat <file>",     "print file contents"),
                    ("mkdir <name>",   "create directory"),
                    ("touch <name>",   "create empty file"),
                    ("rm <name>",      "remove file or empty dir"),
                    ("cp <src> <dst>", "copy file"),
                    ("mv <src> <dst>", "move/rename file"),
                    ("write <f> <txt>","write text to file"),
                    ("uname",          "system info"),
                    ("mem",            "memory usage"),
                    ("shutdown",       "power off (ACPI)"),
                    ("reboot",         "reboot"),
                    ("echo <text>",    "print text"),
                    ("beep [hz] [ms]", "play tone via HDA audio"),
                    ("wget <host>[:<port>] [path]","HTTP GET, resolves DNS (default port 80)"),
                    ("udp <host>:<port> <msg>","send a UDP datagram, print any reply (3s timeout)"),
                    ("view <file.bmp>", "open an uncompressed BMP in the image viewer"),
                    ("play <file.wav>", "play a 16-bit PCM WAV (48kHz, mono/stereo) via HDA"),
                ];
                for (name, desc) in &cmds {
                    self.print_colored("  ", DIM);
                    self.print_colored(name, TEXT);
                    self.print_colored(" - ", DIM);
                    self.print(desc);
                    self.print("\n");
                }
            }

            "clear" => {
                for line in &mut self.lines { *line = [Cell::blank(); MAX_COLS]; }
                self.col = 0; self.row = 0;
            }

            "pwd" => {
                self.print_colored(&self.cwd_path.clone(), OK);
                self.print("\n");
            }

            "ls" => {
                let target_ino = if arg1.is_empty() {
                    Some(self.cwd_ino)
                } else {
                    self.resolve(arg1)
                };
                match target_ino {
                    None => { self.print_colored("ls: not found\n", ERR); }
                    Some(ino) => {
                        let entries = self.with_ctrl(|ctrl| crate::hepfs::list_dir(ctrl, ino));
                        if entries.is_empty() {
                            self.print_colored("(empty)\n", DIM);
                        }
                        for (child_ino, name) in entries {
                            let (is_dir, sz) = self.with_ctrl(|ctrl| crate::hepfs::stat(ctrl, child_ino));
                            if is_dir {
                                self.print_colored(&alloc::format!("{}/\n", name), CURSOR);
                            } else {
                                self.print_colored(&name, TEXT);
                                self.print_colored(&alloc::format!("  ({} B)\n", sz), DIM);
                            }
                        }
                    }
                }
            }

            "cd" => {
                if arg1 == "/" {
                    self.cwd_ino  = crate::hepfs::ROOT_INO;
                    self.cwd_path = String::from("/");
                } else if arg1 == ".." {
                    // Go up — re-resolve parent from path
                    let parent_path = {
                        let p = self.cwd_path.trim_end_matches('/');
                        match p.rfind('/') {
                            Some(0) | None => String::from("/"),
                            Some(i)        => String::from(&p[..i]),
                        }
                    };
                    let new_ino = self.with_ctrl(|ctrl|
                        crate::hepfs::lookup(ctrl, &parent_path)
                    ).unwrap_or(crate::hepfs::ROOT_INO);
                    self.cwd_ino  = new_ino;
                    self.cwd_path = parent_path;
                } else {
                    match self.resolve(arg1) {
                        None => { self.print_colored("cd: not found\n", ERR); }
                        Some(ino) => {
                            let (is_dir, _) = self.with_ctrl(|ctrl| crate::hepfs::stat(ctrl, ino));
                            if !is_dir {
                                self.print_colored("cd: not a directory\n", ERR);
                            } else {
                                self.cwd_ino = ino;
                                self.cwd_path = if self.cwd_path == "/" {
                                    alloc::format!("/{}", arg1)
                                } else {
                                    alloc::format!("{}/{}", self.cwd_path, arg1)
                                };
                            }
                        }
                    }
                }
            }

            "cat" => {
                if arg1.is_empty() { self.print_colored("usage: cat <file>\n", ERR); return; }
                match self.resolve(arg1) {
                    None => { self.print_colored("cat: not found\n", ERR); }
                    Some(ino) => {
                        let data = self.with_ctrl(|ctrl| crate::hepfs::read_file(ctrl, ino));
                        let s = alloc::string::String::from_utf8_lossy(&data);
                        self.print(&s);
                        if !s.ends_with('\n') { self.print("\n"); }
                    }
                }
            }

            "mkdir" => {
                if arg1.is_empty() { self.print_colored("usage: mkdir <name>\n", ERR); return; }
                let cwd = self.cwd_ino;
                self.with_ctrl(|ctrl| { crate::hepfs::create_dir(ctrl, cwd, arg1); });
                self.print_colored("created\n", OK);
            }

            "touch" => {
                if arg1.is_empty() { self.print_colored("usage: touch <name>\n", ERR); return; }
                let cwd = self.cwd_ino;
                self.with_ctrl(|ctrl| { crate::hepfs::create_file(ctrl, cwd, arg1); });
                self.print_colored("created\n", OK);
            }

            "rm" => {
                if arg1.is_empty() { self.print_colored("usage: rm <name>\n", ERR); return; }
                let cwd = self.cwd_ino;
                let ok = self.with_ctrl(|ctrl| crate::hepfs::remove(ctrl, cwd, arg1));
                if ok { self.print_colored("removed\n", OK); }
                else  { self.print_colored("rm: failed (not found or dir not empty)\n", ERR); }
            }

            "cp" => {
                if arg1.is_empty() || arg2.is_empty() {
                    self.print_colored("usage: cp <src> <dst>\n", ERR); return;
                }
                let src_ino = self.resolve(arg1);
                match src_ino {
                    None => { self.print_colored("cp: source not found\n", ERR); }
                    Some(ino) => {
                        let bytes = self.with_ctrl(|ctrl| crate::hepfs::read_file(ctrl, ino));
                        let dst_ino = self.resolve(arg2);
                        let cwd = self.cwd_ino;
                        let dst_name = arg2.trim_start_matches('/');
                        self.with_ctrl(|ctrl| {
                            let di = dst_ino.unwrap_or_else(|| crate::hepfs::create_file(ctrl, cwd, dst_name));
                            crate::hepfs::write_file(ctrl, di, &bytes);
                        });
                        self.print_colored("copied\n", OK);
                    }
                }
            }

            "mv" => {
                if arg1.is_empty() || arg2.is_empty() {
                    self.print_colored("usage: mv <src> <dst>\n", ERR); return;
                }
                let src_ino = self.resolve(arg1);
                match src_ino {
                    None => { self.print_colored("mv: source not found\n", ERR); }
                    Some(ino) => {
                        let bytes = self.with_ctrl(|ctrl| crate::hepfs::read_file(ctrl, ino));
                        let src_name = arg1.trim_start_matches('/');
                        let cwd = self.cwd_ino;
                        let removed = self.with_ctrl(|ctrl| crate::hepfs::remove(ctrl, cwd, src_name));
                        if removed {
                            let dst_ino = self.resolve(arg2);
                            let dst_name = arg2.trim_start_matches('/');
                            self.with_ctrl(|ctrl| {
                                let di = dst_ino.unwrap_or_else(|| crate::hepfs::create_file(ctrl, cwd, dst_name));
                                crate::hepfs::write_file(ctrl, di, &bytes);
                            });
                            self.print_colored("moved\n", OK);
                        } else {
                            self.print_colored("mv: remove failed\n", ERR);
                        }
                    }
                }
            }

            "write" => {
                if arg1.is_empty() || arg2.is_empty() {
                    self.print_colored("usage: write <file> <content>\n", ERR); return;
                }
                let cwd = self.cwd_ino;
                let ino = match self.resolve(arg1) {
                    Some(i) => i,
                    None    => self.with_ctrl(|ctrl| crate::hepfs::create_file(ctrl, cwd, arg1)),
                };
                let data = arg2.as_bytes();
                self.with_ctrl(|ctrl| crate::hepfs::write_file(ctrl, ino, data));
                self.print_colored("written\n", OK);
            }

            "history" => {
                let hist: alloc::vec::Vec<String> = self.history.clone();
                for (i, h) in hist.iter().enumerate() {
                    self.print_colored(&alloc::format!("{:3}  ", i + 1), DIM);
                    self.print(h);
                    self.print("\n");
                }
            }

            "date" => {
                let mut tb = [0u8; 6];
                let mut db = [0u8; 11];
                let time = crate::rtc::fmt_time(&mut tb);
                let date = crate::rtc::fmt_date(&mut db);
                self.print_colored(date, TEXT);
                self.print("  ");
                self.print_colored(time, CURSOR);
                self.print("\n");
            }

            "sysinfo" => {
                self.print_colored("=== HepOS Kernel Info ===\n", OK);
                self.print_colored("Kernel:    ", DIM); self.print("HepOS v0.1\n");
                self.print_colored("Arch:      ", DIM); self.print("x86_64\n");
                self.print_colored("Type:      ", DIM); self.print("Exokernel (Rust)\n");
                self.print_colored("Boot:      ", DIM); self.print("HepBL v0.1 (UEFI)\n");
                self.print_colored("Language:  ", DIM); self.print("Rust (no_std + alloc)\n");
                self.print_colored("Heap:      ", DIM); self.print("Bump allocator 1MB\n");
                self.print_colored("Sched:     ", DIM); self.print("Preemptive round-robin\n");
                self.print_colored("APIC:      ", DIM); self.print("x2APIC (MSR-mode)\n");
                self.print_colored("FS:        ", DIM); self.print("HepFS (flat inode, 4KB blocks)\n");
                self.print_colored("Display:   ", DIM); self.print("GOP framebuffer, software render\n");
                self.print_colored("Storage:   ", DIM); self.print("NVMe (custom driver)\n");
                let free_mb  = crate::pmm::free_pages() * 4 / 1024;
                let total_mb = crate::pmm::total_pages() * 4 / 1024;
                self.print_colored("RAM:       ", DIM);
                self.print_u64(free_mb); self.print(" MB free / ");
                self.print_u64(total_mb); self.print(" MB total\n");
                self.print_colored("=========================\n", DIM);
                self.print_colored("Source: github.com/The-Hep-Group/HepOS\n", DIM);
            }

            "uname" => {
                self.print_colored("HepOS", CURSOR);
                self.print(" v0.1  x86_64 exokernel  HepFS\n");
            }

            "mem" => {
                let free  = crate::pmm::free_pages() * 4;
                let total = crate::pmm::total_pages() * 4;
                self.print_colored("RAM: ", DIM);
                self.print_u64(free);
                self.print(" KB free / ");
                self.print_u64(total);
                self.print(" KB total\n");
            }

            "netpoll" => {
                // Directly scan all 8 RX descriptors and show status
                self.print_colored("Scanning RX descriptors...\n", DIM);
                {
                    let nic_guard = crate::e1000::NIC.lock();
                    if let Some(nic) = nic_guard.as_ref() {
                        for i in 0..8usize {
                            let st  = nic.rx_status(i);
                            let len = nic.rx_len(i);
                            self.print(&alloc::format!("  desc[{}]: status={:02X} len={}\n",
                                i, st, len));
                        }
                    } else {
                        self.print_colored("NIC is None - run netstart first\n", ERR);
                    }
                }
            }

            "netstart" => {
                // Force-initialize e1000 from terminal (bar at 0xFEBC0000 from lspci)
                self.print_colored("Starting e1000...\n", DIM);
                crate::e1000::force_init(0xFEBC_0000);
                if crate::e1000::NIC.lock().is_some() {
                    self.print_colored("NIC initialized! Try ping 10.0.2.2\n", OK);
                    crate::net::arp_announce();
                } else {
                    self.print_colored("NIC still None - check serial\n", ERR);
                }
            }

            "netdiag" => {
                // Read e1000 PCI config and registers directly (bus=0,dev=3,func=0)
                let bus = 0u8; let dev = 3u8; let func = 0u8;
                let vid = crate::pci::config_read16(bus, dev, func, 0x00);
                let did = crate::pci::config_read16(bus, dev, func, 0x02);
                let cmd = crate::pci::config_read16(bus, dev, func, 0x04);
                let bar0 = crate::pci::config_read32(bus, dev, func, 0x10);
                let bar1 = crate::pci::config_read32(bus, dev, func, 0x14);
                self.print(&alloc::format!("00:03.0  VID:{:04X} DID:{:04X}\n", vid, did));
                self.print(&alloc::format!("CMD: {:04X}\n", cmd));
                self.print(&alloc::format!("BAR0: {:08X}  BAR1: {:08X}\n", bar0, bar1));

                // Compute BAR physical address
                let bar_phys = if (bar0 & 6) == 4 {
                    ((bar1 as u64) << 32) | ((bar0 & !0xF) as u64)
                } else {
                    (bar0 & !0xF) as u64
                };
                self.print(&alloc::format!("BAR phys: {:016X}\n", bar_phys));

                if bar_phys != 0 {
                    // Map and read CTRL + STATUS
                    let regs = crate::paging::map_mmio(bar_phys, 131072);
                    let ctrl   = unsafe { (regs as *const u32).read_volatile() };
                    let status = unsafe { (regs.add(8) as *const u32).read_volatile() };
                    let ral    = unsafe { (regs.add(0x5400) as *const u32).read_volatile() };
                    let rah    = unsafe { (regs.add(0x5404) as *const u32).read_volatile() };
                    self.print(&alloc::format!("CTRL:   {:08X}\n", ctrl));
                    self.print(&alloc::format!("STATUS: {:08X}\n", status));
                    self.print(&alloc::format!("RAL:    {:08X}  RAH: {:08X}\n", ral, rah));
                    let mac = [
                        (ral & 0xFF) as u8, (ral >> 8 & 0xFF) as u8,
                        (ral >> 16 & 0xFF) as u8, (ral >> 24) as u8,
                        (rah & 0xFF) as u8, (rah >> 8 & 0xFF) as u8,
                    ];
                    self.print(&alloc::format!(
                        "MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}\n",
                        mac[0],mac[1],mac[2],mac[3],mac[4],mac[5]));
                    self.print(&alloc::format!("e1000::NIC is {}\n",
                        if crate::e1000::NIC.lock().is_some() { "SOME" } else { "NONE" }));
                    // TX registers
                    let tctl  = unsafe { (regs.add(0x400) as *const u32).read_volatile() };
                    let tdbal = unsafe { (regs.add(0x3800) as *const u32).read_volatile() };
                    let tdlen = unsafe { (regs.add(0x3808) as *const u32).read_volatile() };
                    let tdh   = unsafe { (regs.add(0x3810) as *const u32).read_volatile() };
                    let tdt   = unsafe { (regs.add(0x3818) as *const u32).read_volatile() };
                    self.print(&alloc::format!("TX: TCTL:{:08X} TDBAL:{:08X} TDLEN:{} TDH:{} TDT:{}\n",
                        tctl, tdbal, tdlen, tdh, tdt));
                    // RX registers
                    let rctl  = unsafe { (regs.add(0x100) as *const u32).read_volatile() };
                    let rdbal = unsafe { (regs.add(0x2800) as *const u32).read_volatile() };
                    let rdlen = unsafe { (regs.add(0x2808) as *const u32).read_volatile() };
                    let rdh   = unsafe { (regs.add(0x2810) as *const u32).read_volatile() };
                    let rdt   = unsafe { (regs.add(0x2818) as *const u32).read_volatile() };
                    self.print(&alloc::format!("RX: RCTL:{:08X} RDBAL:{:08X} RDLEN:{} RDH:{} RDT:{}\n",
                        rctl, rdbal, rdlen, rdh, rdt));
                } else {
                    self.print_colored("BAR phys = 0, device not initialized by BIOS\n", ERR);
                }
            }

            "runtest" => {
                self.print_colored("Launching ring-3 test process...\n", OK);
                let code = crate::process::run_test();
                self.flush_proc_output();
                self.print(&alloc::format!("Process exited: {}\n", code));
            }

            "runhello" => {
                self.print_colored("Launching hello (hepos-std demo)...\n", OK);
                match crate::process::run_hello() {
                    Ok(code) => {
                        self.flush_proc_output();
                        self.print(&alloc::format!("hello exited: {}\n", code));
                    }
                    Err(e) => self.print_colored(&alloc::format!("runhello: {}\n", e), ERR),
                }
            }

            "beep" => {
                // beep [freq_hz] [duration_ms]
                let freq: u32 = arg1.parse().unwrap_or(440);
                let ms:   u32 = arg2.parse().unwrap_or(200);
                if !crate::hda::is_available() {
                    self.print_colored("beep: HDA not available (launch QEMU with -device intel-hda -device hda-duplex)\n", ERR);
                } else {
                    self.print(&alloc::format!("beep: {} Hz for {} ms\n", freq, ms));
                    crate::hda::beep(freq, ms);
                }
            }

            "exec" => {
                if arg1.is_empty() { self.print_colored("usage: exec <file>\n", ERR); return; }
                match self.resolve(arg1) {
                    None => { self.print_colored("exec: file not found\n", ERR); }
                    Some(ino) => {
                        let (is_dir, _) = self.with_ctrl(|ctrl| crate::hepfs::stat(ctrl, ino));
                        if is_dir { self.print_colored("exec: is a directory\n", ERR); return; }
                        let data = self.with_ctrl(|ctrl| crate::hepfs::read_file(ctrl, ino));
                        self.print_colored("Executing ELF...\n", OK);
                        match crate::process::exec(arg1, &data) {
                            Ok(code) => {
                                self.flush_proc_output();
                                self.print(&alloc::format!("Process exited: {}\n", code));
                            }
                            Err(e) => self.print_colored(&alloc::format!("exec: {}\n", e), ERR),
                        }
                    }
                }
            }

            "ps" => {
                self.print("  PID  STATE        NAME\n");
                self.print("  ---  -----------  ----\n");
                let mut any = false;
                crate::process::for_each_proc(|pid, name, running, code| {
                    any = true;
                    let state = if running {
                        alloc::format!("running    ")
                    } else {
                        alloc::format!("exited({:<3})", code)
                    };
                    self.print(&alloc::format!("  {:>3}  {}  {}\n", pid, state, name));
                });
                if !any { self.print("  (no processes)\n"); }
            }

            "newterm" => {
                let id = crate::terminal::spawn_terminal();
                if id == usize::MAX {
                    self.print_colored("newterm: desktop not ready\n", ERR);
                } else {
                    self.print_colored(
                        &alloc::format!("Spawned terminal (window {}). Click it to focus.\n", id),
                        OK,
                    );
                }
            }

            "syscallinfo" => {
                // Read back the MSRs we programmed in syscall::init() and show them.
                // Values should match exactly what was written:
                //   EFER  bit 0 (SCE) = 1
                //   STAR  = 0x0010_0008_0000_0000
                //   LSTAR = address of syscall_entry (non-zero, in kernel text)
                //   SFMASK = 0x300 (clears IF + TF on SYSCALL)
                let (efer, star, lstar, sfmask) = unsafe {
                    let efer  = crate::syscall::rdmsr_pub(0xC000_0080);
                    let star  = crate::syscall::rdmsr_pub(0xC000_0081);
                    let lstar = crate::syscall::rdmsr_pub(0xC000_0082);
                    let sfmask= crate::syscall::rdmsr_pub(0xC000_0084);
                    (efer, star, lstar, sfmask)
                };
                let sce = if efer & 1 != 0 { "YES" } else { "NO (broken!)" };
                self.print_colored("Syscall gate MSR readback\n", OK);
                self.print(&alloc::format!("  EFER  SCE = {}\n", sce));
                self.print(&alloc::format!("  STAR  = 0x{:016X}\n", star));
                self.print(&alloc::format!("  LSTAR = 0x{:016X}  (syscall entry RIP)\n", lstar));
                self.print(&alloc::format!("  SFMASK= 0x{:016X}  (expect 0x300)\n", sfmask));
                let kgs: u64 = unsafe { crate::syscall::rdmsr_pub(0xC000_0102) };
                self.print(&alloc::format!("  KGSBASE = 0x{:016X}  (per-CPU struct)\n", kgs));
                if lstar != 0 && efer & 1 != 0 {
                    self.print_colored("Gate is live. Ring-3 SYSCALL will land at LSTAR.\n", OK);
                } else {
                    self.print_colored("Gate NOT active — check syscall::init() order.\n", ERR);
                }
            }

            "lspci" => {
                let devs = crate::PCI_DEVS.lock();
                for d in devs.iter() {
                    self.print(&alloc::format!(
                        "{:02X}:{:02X}.{} {:04X}:{:04X} {}\n",
                        d.bus, d.dev, d.func,
                        d.vendor_id, d.device_id,
                        crate::pci::class_name(d.class, d.subclass)
                    ));
                }
            }

            "ifconfig" => {
                let has_nic = crate::rtl8139::NIC.lock().is_some() || crate::e1000::NIC.lock().is_some();
                let mac = crate::net::my_mac_pub();
                if !has_nic {
                    self.print_colored("eth0: NIC not initialized\n", ERR);
                    self.print_colored("      (check serial for e1000 init messages)\n", DIM);
                }
                let ip = crate::net::MY_IP;
                let gw = crate::net::GW_IP;
                self.print_colored("eth0\n", OK);
                self.print_colored("  MAC: ", DIM);
                self.print(&alloc::format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}\n",
                    mac[0],mac[1],mac[2],mac[3],mac[4],mac[5]));
                self.print_colored("  IP:  ", DIM);
                self.print(&alloc::format!("{}.{}.{}.{}\n", ip[0],ip[1],ip[2],ip[3]));
                self.print_colored("  GW:  ", DIM);
                self.print(&alloc::format!("{}.{}.{}.{}\n", gw[0],gw[1],gw[2],gw[3]));
                self.print_colored("  Net: ", DIM);
                self.print("255.255.255.0\n");
            }

            "ping" => {
                if arg1.is_empty() {
                    self.print_colored("usage: ping <ip>\n", ERR);
                } else {
                    let parts: alloc::vec::Vec<&str> = arg1.split('.').collect();
                    if parts.len() == 4 {
                        let ip: [u8; 4] = [
                            parts[0].parse().unwrap_or(0),
                            parts[1].parse().unwrap_or(0),
                            parts[2].parse().unwrap_or(0),
                            parts[3].parse().unwrap_or(0),
                        ];
                        let on_subnet = ip[0] == 10 && ip[1] == 0 && ip[2] == 2;
                        let route_note = if on_subnet {
                            if ip == crate::net::GW_IP {
                                " (gateway)"
                            } else {
                                " (local subnet — SLiRP may not reply)"
                            }
                        } else {
                            " (via gateway 10.0.2.2)"
                        };
                        self.print_colored(
                            &alloc::format!("PING {}{}\n", arg1, route_note), DIM
                        );
                        let result = crate::net::ping(ip);
                        self.print(&result);
                        self.print("\n");
                    } else {
                        self.print_colored("ping: invalid IP address\n", ERR);
                    }
                }
            }

            "wget" => {
                if arg1.is_empty() {
                    self.print_colored("usage: wget <ip|host>[:<port>] [/path]\n", ERR);
                } else {
                    // Split optional ":port" suffix
                    let (host_str, port) = if let Some(colon) = arg1.rfind(':') {
                        let p = arg1[colon+1..].parse::<u16>().unwrap_or(80);
                        (&arg1[..colon], p)
                    } else {
                        (arg1, 80u16)
                    };
                    // Resolve: bare IP first, then DNS
                    let resolved = if let Some(ip) = crate::net::parse_ip(host_str) {
                        Some(ip)
                    } else {
                        self.print(&alloc::format!("Resolving {}…\n", host_str));
                        crate::net::dns_resolve(host_str)
                    };
                    match resolved {
                        None => self.print_colored("wget: could not resolve host\n", ERR),
                        Some(ip) => {
                            let path = if arg2.is_empty() { "/" } else { arg2 };
                            let req  = alloc::format!(
                                "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
                                path, host_str
                            );
                            self.print(&alloc::format!("Connecting to {}:{}…\n", host_str, port));
                            match crate::net::tcp_get(ip, port, req.as_bytes()) {
                                Err(e) => self.print_colored(
                                    &alloc::format!("wget: {}\n", e), ERR
                                ),
                                Ok(data) => {
                                    let limit = data.len().min(4096);
                                    let s = core::str::from_utf8(&data[..limit])
                                        .unwrap_or("(non-UTF-8 response)");
                                    self.print(s);
                                    if data.len() > limit {
                                        self.print_colored(
                                            &alloc::format!("\n[… {} bytes truncated]\n",
                                                data.len() - limit), DIM
                                        );
                                    }
                                    self.print("\n");
                                }
                            }
                        }
                    }
                }
            }

            "udp" => {
                if arg1.is_empty() || arg2.is_empty() {
                    self.print_colored("usage: udp <ip|host>[:<port>] <message>\n", ERR);
                } else {
                    let (host_str, port) = if let Some(colon) = arg1.rfind(':') {
                        let p = arg1[colon+1..].parse::<u16>().unwrap_or(0);
                        (&arg1[..colon], p)
                    } else {
                        (arg1, 0u16)
                    };
                    if port == 0 {
                        self.print_colored("udp: missing or invalid port (use ip:port)\n", ERR);
                    } else {
                        let resolved = if let Some(ip) = crate::net::parse_ip(host_str) {
                            Some(ip)
                        } else {
                            self.print(&alloc::format!("Resolving {}…\n", host_str));
                            crate::net::dns_resolve(host_str)
                        };
                        match resolved {
                            None => self.print_colored("udp: could not resolve host\n", ERR),
                            Some(ip) => {
                                let src_port = 51000u16 + (crate::hda::rdtsc() as u16 % 1000);
                                self.print(&alloc::format!(
                                    "Sending {} bytes to {}:{}…\n", arg2.len(), host_str, port
                                ));
                                match crate::net::udp_send_recv(
                                    ip, crate::net::GW_MAC, src_port, port,
                                    arg2.as_bytes(), 3_000,
                                ) {
                                    None => self.print_colored(
                                        "udp: no reply within 3s (datagram sent either way)\n", DIM
                                    ),
                                    Some((data, rip, rport)) => {
                                        self.print_colored(&alloc::format!(
                                            "Reply from {}.{}.{}.{}:{} ({} bytes):\n",
                                            rip[0], rip[1], rip[2], rip[3], rport, data.len()
                                        ), OK);
                                        let limit = data.len().min(4096);
                                        let s = core::str::from_utf8(&data[..limit])
                                            .unwrap_or("(non-UTF-8 reply)");
                                        self.print(s);
                                        self.print("\n");
                                    }
                                }
                            }
                        }
                    }
                }
            }

            "edit" => {
                if arg1.is_empty() {
                    self.print_colored("usage: edit <file>\n", ERR);
                } else {
                    let full = if arg1.starts_with('/') {
                        String::from(arg1)
                    } else if self.cwd_path == "/" {
                        alloc::format!("/{}", arg1)
                    } else {
                        alloc::format!("{}/{}", self.cwd_path, arg1)
                    };
                    let win_id = crate::editor::open_smart(&full);
                    // Only the main editor window (id=3) has a fixed generic title to update —
                    // spawned instances already get "Editor: <name>" titles from open_smart.
                    if win_id == 3 {
                        let mut dt = crate::desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 3) {
                                w.title = alloc::format!("Editor: {}", arg1);
                            }
                        }
                    }
                    self.print_colored("Editor opened  Ctrl+S=save  Ctrl+Q=close\n", OK);
                }
            }

            "view" => {
                if arg1.is_empty() {
                    self.print_colored("usage: view <file.bmp>\n", ERR);
                } else {
                    let full = if arg1.starts_with('/') {
                        String::from(arg1)
                    } else if self.cwd_path == "/" {
                        alloc::format!("/{}", arg1)
                    } else {
                        alloc::format!("{}/{}", self.cwd_path, arg1)
                    };
                    // Reuses the main viewer window if free, else spawns a new one —
                    // lets several images be open side by side.
                    let win_id = crate::image::open_smart(&full);
                    let has_err = if win_id == 6 {
                        crate::image::VIEWER.lock().as_ref().map(|v| v.error.is_some()).unwrap_or(false)
                    } else {
                        crate::image::EXTRA_VIEWERS.lock().iter()
                            .find(|(wid, _)| *wid == win_id)
                            .map(|(_, v)| v.error.is_some())
                            .unwrap_or(false)
                    };
                    if has_err {
                        self.print_colored("view: unsupported or unreadable BMP\n", ERR);
                    } else {
                        self.print_colored("Image viewer opened\n", OK);
                    }
                }
            }

            "play" => {
                if arg1.is_empty() {
                    self.print_colored("usage: play <file.wav>\n", ERR);
                } else {
                    let full = if arg1.starts_with('/') {
                        String::from(arg1)
                    } else if self.cwd_path == "/" {
                        alloc::format!("/{}", arg1)
                    } else {
                        alloc::format!("{}/{}", self.cwd_path, arg1)
                    };
                    self.print_colored(&alloc::format!("Playing {}…\n", arg1), DIM);
                    let result = crate::audio::play(&full);
                    // Un-minimize Audio Player window (id=7), bring to front, focus it
                    {
                        let mut dt = crate::desktop::DESKTOP.lock();
                        if let Some(dt) = dt.as_mut() {
                            if let Some(w) = dt.windows.iter_mut().find(|w| w.id == 7) {
                                w.minimized = false;
                            }
                            dt.bring_to_front(7);
                            dt.dirty = true;
                        }
                    }
                    *crate::FOCUSED_WIN.lock() = Some(7);
                    match result.error {
                        Some(e) => self.print_colored(&alloc::format!("play: {}\n", e), ERR),
                        None => {
                            // Playback runs in the background now — the terminal
                            // doesn't block, so report the target duration, not a
                            // completed one; the Audio Player window shows live progress.
                            self.print_colored(&alloc::format!(
                                "Playing ({} ms){}\n", result.duration_ms,
                                if result.truncated { " (truncated to ~5.4s buffer)" } else { "" }
                            ), OK);
                        }
                    }
                }
            }

            "shutdown" => { self.print_colored("Shutting down...\n", ERR); crate::acpi::shutdown(); }
            "reboot"   => { self.print_colored("Rebooting...\n",    ERR); crate::acpi::reboot(); }

            s if s.starts_with("echo") => {
                self.print(if arg1.is_empty() { "\n" } else { &cmd[5..] });
                self.print("\n");
            }

            other => {
                self.print_colored(other, ERR);
                self.print_colored(": command not found  (try 'help')\n", DIM);
            }
        }
    }

    /// Resolve a name relative to cwd (or absolute path).
    fn resolve(&mut self, name: &str) -> Option<u32> {
        if name.starts_with('/') {
            self.with_ctrl(|ctrl| crate::hepfs::lookup(ctrl, name))
        } else {
            let cwd = self.cwd_ino;
            self.with_ctrl(|ctrl| {
                crate::hepfs::list_dir(ctrl, cwd)
                    .into_iter()
                    .find(|(_, n)| n.as_str() == name)
                    .map(|(ino, _)| ino)
            })
        }
    }

    /// Run a closure with the global NVMe controller locked.
    fn with_ctrl<T, F: FnOnce(&mut crate::nvme::NvmeController) -> T>(&self, f: F) -> T {
        let mut guard = crate::nvme::CONTROLLER.lock();
        f(guard.as_mut().expect("no NVMe controller"))
    }

    fn print_u64(&mut self, mut n: u64) {
        if n == 0 { self.put_char(b'0', TEXT); return; }
        let mut buf = [0u8; 20];
        let mut i = 20usize;
        while n > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        for &b in &buf[i..] {
            self.put_char(b, TEXT);
        }
    }

    /// Render terminal content into the window content area.
    pub fn render(&mut self, display: &mut Display, wx: usize, wy: usize, ww: usize, wh: usize) {
        // Update column count to match window width
        let new_cols = ((ww.saturating_sub(8)) / CHAR_W).max(10).min(MAX_COLS);
        if new_cols != self.cols {
            self.cols = new_cols;
            // Clamp cursor to new width so it doesn't go out of bounds
            if self.col >= self.cols { self.col = self.cols - 1; }
            if self.prompt_col >= self.cols { self.prompt_col = 0; }
        }

        // Fill background
        display.fill_rect(wx, wy, ww, wh, BG);

        // Accent line at top
        display.fill_rect(wx, wy, ww, 2, CURSOR);

        let visible_rows = (wh.saturating_sub(6)) / CHAR_H;
        let start = if self.row + 1 >= visible_rows {
            self.row + 1 - visible_rows
        } else {
            0
        };
        self.render_start = start;
        self.render_rows  = visible_rows.max(1);

        for r in 0..visible_rows {
            let line_idx = start + r;
            if line_idx >= self.lines.len() { break; }
            let line = &self.lines[line_idx];
            let py = wy + 4 + r * CHAR_H;
            if py + CHAR_H > wy + wh { break; }

            if let Some((sel_start, sel_end)) = self.selection_cols_for_line(line_idx) {
                let sel_x = wx + 4 + sel_start * CHAR_W;
                let sel_w = (sel_end - sel_start) * CHAR_W;
                if sel_x < wx + ww {
                    let sel_w = sel_w.min(wx + ww - sel_x);
                    display.fill_rect(sel_x, py, sel_w, CHAR_H, SELECTION);
                }
            }

            for (ci, cell) in line[..self.cols].iter().enumerate() {
                let px = wx + 4 + ci * CHAR_W;
                if px + CHAR_W > wx + ww { break; }
                if cell.ch > b' ' {
                    let s = core::str::from_utf8(
                        core::slice::from_ref(&cell.ch)
                    ).unwrap_or("?");
                    display.draw_text(px, py, s, cell.color, SCALE);
                }
            }
        }

        // Cursor underline
        let vis_row = self.row.saturating_sub(start).min(visible_rows.saturating_sub(1));
        let cx = wx + 4 + self.col * CHAR_W;
        let cy = wy + 4 + vis_row * CHAR_H + CHAR_H - 2;
        if cx + 16 <= wx + ww && cy + 2 <= wy + wh {
            display.fill_rect(cx, cy, 16, 2, CURSOR);
        }
    }
}

pub static TERMINAL: Mutex<Option<Terminal>> = Mutex::new(None);

/// Additional terminal instances keyed by their window ID.
/// Window IDs for extra terminals are assigned by the desktop when spawned.
pub static EXTRA_TERMINALS: Mutex<alloc::vec::Vec<(usize, Terminal)>> = Mutex::new(alloc::vec::Vec::new());

pub fn init() {
    *TERMINAL.lock() = Some(Terminal::new());
    // Trigger desktop re-render so terminal content appears immediately
    if let Some(dt) = crate::desktop::DESKTOP.lock().as_mut() {
        dt.dirty = true;
    }
}

/// Spawn a new floating terminal window. Returns the new window ID.
pub fn spawn_terminal() -> usize {
    let count = EXTRA_TERMINALS.lock().len();
    let off   = (count + 1) as i32 * 24;
    let win_id = {
        let mut dt = crate::desktop::DESKTOP.lock();
        if let Some(dt) = dt.as_mut() {
            let id = dt.add_window(crate::desktop::AppKind::Terminal, "Terminal", 80 + off, 80 + off, 520, 320);
            dt.dirty = true;
            id
        } else {
            return usize::MAX;
        }
    };
    EXTRA_TERMINALS.lock().push((win_id, Terminal::new()));
    win_id
}
