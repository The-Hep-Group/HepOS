//! Small pixel-art glyphs for app/file icons — desktop, taskbar, Start Menu,
//! and the file manager all draw through here so they never drift apart.
//!
//! The framebuffer API is `fill_rect`/`draw_text`/`put_pixel` only (no bitmap
//! loading, no line/circle primitives — see `framebuffer.rs`), so these are
//! blocky Win95-era pixel icons built from a handful of rects per glyph, not
//! true bitmaps. That's a real, deliberate ceiling: a proper icon set would
//! need a bitmap/PNG asset pipeline this project doesn't have (only a
//! hand-rolled BMP decoder for user *content*, not UI chrome).
//!
//! Every glyph is authored on a 16×16 unit grid and scaled to whatever `size`
//! the caller asks for via `u()` — same shapes at 48px (desktop) and 12px
//! (taskbar/Start Menu/file list rows), just coarser at the small end.

use crate::framebuffer::{Color, Display};
use crate::desktop::AppKind;

/// Scale a 0..16 unit coordinate to a pixel offset within a `size`×`size` box.
fn u(v: i32, size: usize) -> usize {
    ((v.max(0) as i64 * size as i64) / 16) as usize
}

/// Fill an upward-pointing triangle (point at top, base at bottom), used for
/// roofs/mountains/speaker cones. `cx`/`top` are in the *destination* pixel
/// space already (post-scaling), not unit-grid.
fn fill_tri_down(display: &mut Display, cx: usize, top: usize, half_w: usize, h: usize, color: Color) {
    for row in 0..h.max(1) {
        let w = (half_w * (row + 1)) / h.max(1);
        if w == 0 { continue; }
        display.fill_rect(cx.saturating_sub(w), top + row, w * 2, 1, color);
    }
}

/// Base color for an app's icon — same palette `desktop::icon_color()` used
/// before this existed, kept here since file-type glyphs need it too.
pub fn app_base_color(kind: AppKind) -> Color {
    match kind {
        AppKind::Welcome     => Color::from_hex(0xE8A020),
        AppKind::Files       => Color::from_hex(0x3A8FD4),
        AppKind::Terminal    => Color::from_hex(0x1A1A1A),
        AppKind::Editor      => Color::from_hex(0x9B5CE5),
        AppKind::Sysmon      => Color::from_hex(0x20B8B0),
        AppKind::Settings    => Color::from_hex(0x707070),
        AppKind::ImageViewer => Color::from_hex(0x4CAF50),
        AppKind::AudioPlayer => Color::from_hex(0xD46A8F),
    }
}

/// Draw an app's icon face into a `size`×`size` box at (x, y) — base color
/// fill plus a small recognizable glyph on top. Does NOT draw the outer
/// border/title-strip chrome desktop icons use — callers add that themselves
/// (this is just the "face").
pub fn draw_app_icon(display: &mut Display, x: usize, y: usize, size: usize, kind: AppKind) {
    let base = app_base_color(kind);
    display.fill_rect(x, y, size, size, base);
    let white = Color::from_hex(0xF0F0F0);
    let dark  = Color::from_hex(0x101010);

    match kind {
        AppKind::Welcome => {
            // House: white triangle roof + a dark door.
            fill_tri_down(display, x + u(8, size), y + u(2, size), u(6, size).max(1), u(5, size).max(1), white);
            display.fill_rect(x + u(6, size), y + u(9, size), u(4, size).max(1), u(6, size).max(1), dark);
        }
        AppKind::Files => {
            // Folder: lighter body + a small tab.
            let light = Color::from_hex(0x8FC4F0);
            display.fill_rect(x + u(2, size), y + u(5, size), u(6, size).max(1), u(3, size).max(1), light);
            display.fill_rect(x + u(2, size), y + u(7, size), u(12, size).max(1), u(6, size).max(1), light);
        }
        AppKind::Terminal => {
            // Screen + a green ">_" prompt mark.
            let green = Color::from_hex(0x30E060);
            display.fill_rect(x + u(2, size), y + u(3, size), u(12, size).max(1), u(10, size).max(1), Color::from_hex(0x0A0A0A));
            display.fill_rect(x + u(4, size), y + u(6, size), u(2, size).max(1), u(2, size).max(1), green);
            display.fill_rect(x + u(4, size), y + u(9, size), u(2, size).max(1), u(2, size).max(1), green);
            display.fill_rect(x + u(7, size), y + u(9, size), u(4, size).max(1), u(1, size).max(1), green);
        }
        AppKind::Editor => {
            // Page + a few text-line rects.
            display.fill_rect(x + u(3, size), y + u(2, size), u(10, size).max(1), u(12, size).max(1), white);
            let line = Color::from_hex(0x666666);
            for row in [5, 8, 11] {
                display.fill_rect(x + u(5, size), y + u(row, size), u(6, size).max(1), 1, line);
            }
        }
        AppKind::Sysmon => {
            // Bar chart, increasing height.
            let bar = Color::from_hex(0xA0F0E8);
            let base_y = y + u(13, size);
            for (i, h) in [4i32, 7, 10].iter().enumerate() {
                let bx = x + u(3 + i as i32 * 4, size);
                display.fill_rect(bx, base_y.saturating_sub(u(*h, size)), u(3, size).max(1), u(*h, size).max(1), bar);
            }
        }
        AppKind::Settings => {
            // Gear: center square + 4 corner nubs.
            let gear = Color::from_hex(0xD0D0D0);
            display.fill_rect(x + u(5, size), y + u(5, size), u(6, size).max(1), u(6, size).max(1), gear);
            display.fill_rect(x + u(7, size), y + u(1, size), u(2, size).max(1), u(3, size).max(1), gear);
            display.fill_rect(x + u(7, size), y + u(12, size), u(2, size).max(1), u(3, size).max(1), gear);
            display.fill_rect(x + u(1, size), y + u(7, size), u(3, size).max(1), u(2, size).max(1), gear);
            display.fill_rect(x + u(12, size), y + u(7, size), u(3, size).max(1), u(2, size).max(1), gear);
        }
        AppKind::ImageViewer => {
            // Photo frame + mountain + sun.
            display.fill_rect(x + u(2, size), y + u(3, size), u(12, size).max(1), u(10, size).max(1), white);
            display.fill_rect(x + u(10, size), y + u(4, size), u(2, size).max(1), u(2, size).max(1), Color::from_hex(0xE8C020));
            fill_tri_down(display, x + u(6, size), y + u(7, size), u(4, size).max(1), u(5, size).max(1), Color::from_hex(0x2E8C4A));
        }
        AppKind::AudioPlayer => {
            // Speaker body + cone + two sound-wave bars.
            let spk = white;
            display.fill_rect(x + u(3, size), y + u(6, size), u(3, size).max(1), u(4, size).max(1), spk);
            fill_tri_down(display, x + u(8, size), y + u(3, size), u(3, size).max(1), u(10, size).max(1), spk);
            let wave = Color::from_hex(0xF0D0DC);
            display.fill_rect(x + u(12, size), y + u(6, size), 1, u(4, size).max(1), wave);
            display.fill_rect(x + u(14, size), y + u(5, size), 1, u(6, size).max(1), wave);
        }
    }
}

/// Draw a file/directory glyph (file manager rows, desktop `FsEntry` icons)
/// into a `size`×`size` box at (x, y). Directories get a folder; files are
/// classified by extension where a matching app icon already exists
/// (`.bmp`→image glyph, `.wav`→audio glyph), otherwise a generic page.
pub fn draw_file_icon(display: &mut Display, x: usize, y: usize, size: usize, is_dir: bool, name: &str) {
    if is_dir {
        let folder = Color::from_hex(0xE8C64A);
        let light  = Color::from_hex(0xF5DC8A);
        display.fill_rect(x + u(1, size), y + u(4, size), u(6, size).max(1), u(3, size).max(1), light);
        display.fill_rect(x + u(1, size), y + u(6, size), u(14, size).max(1), u(8, size).max(1), folder);
        return;
    }
    let lower = name.to_lowercase();
    if lower.ends_with(".bmp") {
        draw_app_icon(display, x, y, size, AppKind::ImageViewer);
        return;
    }
    if lower.ends_with(".wav") {
        draw_app_icon(display, x, y, size, AppKind::AudioPlayer);
        return;
    }
    // Generic file: white page with a folded top-right corner + text lines.
    let white = Color::from_hex(0xE8E8E8);
    let fold  = Color::from_hex(0xB0B0B0);
    display.fill_rect(x + u(2, size), y + u(1, size), u(10, size).max(1), u(14, size).max(1), white);
    display.fill_rect(x + u(9, size), y + u(1, size), u(3, size).max(1), u(3, size).max(1), fold);
    let line = Color::from_hex(0x808080);
    for row in [6, 8, 10, 12] {
        display.fill_rect(x + u(4, size), y + u(row, size), u(6, size).max(1), 1, line);
    }
}
