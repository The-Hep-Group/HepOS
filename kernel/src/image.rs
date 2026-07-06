//! Minimal image viewer — decodes uncompressed BMP (24/32-bit, BI_RGB) and
//! renders it into a window. Open with `view <file>` or by clicking a
//! `.bmp` file in the HepFS file manager.

use alloc::{string::String, vec::Vec};
use spin::Mutex;
use crate::framebuffer::{Color, Display};

pub struct ImageViewer {
    pub path:   String,
    pub open:   bool,
    width:      usize,
    height:     usize,
    pixels:     Vec<Color>, // row-major, top-down
    pub error:  Option<String>,
}

/// Parse a BMP file (BITMAPFILEHEADER + BITMAPINFOHEADER, BI_RGB, 24 or 32 bpp).
/// Returns None if the format isn't one we support.
fn decode_bmp(data: &[u8]) -> Option<(usize, usize, Vec<Color>)> {
    if data.len() < 54 || data[0] != b'B' || data[1] != b'M' { return None; }

    let pixel_offset = u32::from_le_bytes(data[10..14].try_into().ok()?) as usize;
    let dib_size      = u32::from_le_bytes(data[14..18].try_into().ok()?) as usize;
    if dib_size < 40 { return None; } // need at least BITMAPINFOHEADER

    let width_raw  = i32::from_le_bytes(data[18..22].try_into().ok()?);
    let height_raw = i32::from_le_bytes(data[22..26].try_into().ok()?);
    let bpp        = u16::from_le_bytes(data[28..30].try_into().ok()?);
    let compression = u32::from_le_bytes(data[30..34].try_into().ok()?);

    if compression != 0 { return None; } // only BI_RGB (uncompressed)
    if bpp != 24 && bpp != 32 { return None; }

    let width  = width_raw.unsigned_abs() as usize;
    let top_down = height_raw < 0;
    let height = height_raw.unsigned_abs() as usize;
    if width == 0 || height == 0 || width > 4096 || height > 4096 { return None; }

    let bytes_per_px = (bpp / 8) as usize;
    let row_stride   = ((width * bytes_per_px + 3) / 4) * 4; // rows padded to 4 bytes

    let mut pixels = alloc::vec![Color { r: 0, g: 0, b: 0 }; width * height];

    for row in 0..height {
        // BMP rows are stored bottom-up unless height is negative (top-down).
        let src_row = if top_down { row } else { height - 1 - row };
        let row_start = pixel_offset + src_row * row_stride;
        if row_start + width * bytes_per_px > data.len() { return None; }
        for col in 0..width {
            let px = row_start + col * bytes_per_px;
            // BMP stores BGR(A)
            let b = data[px];
            let g = data[px + 1];
            let r = data[px + 2];
            pixels[row * width + col] = Color { r, g, b };
        }
    }

    Some((width, height, pixels))
}

impl ImageViewer {
    pub fn from_bytes(path: &str, data: &[u8]) -> Self {
        match decode_bmp(data) {
            Some((width, height, pixels)) => ImageViewer {
                path: String::from(path), open: true, width, height, pixels, error: None,
            },
            None => ImageViewer {
                path: String::from(path), open: true, width: 0, height: 0,
                pixels: Vec::new(),
                error: Some(String::from("Unsupported format — only uncompressed 24/32-bit BMP")),
            },
        }
    }

    pub fn render(&self, display: &mut Display, wx: usize, wy: usize, ww: usize, wh: usize) {
        const BG: Color   = Color::from_hex(0x0A0A14);
        const DIM: Color  = Color::from_hex(0x888888);
        const ERR: Color  = Color::from_hex(0xFF6B6B);

        display.fill_rect(wx, wy, ww, wh, BG);

        if let Some(err) = &self.error {
            display.draw_text(wx + 8, wy + 8, err, ERR, 1);
            return;
        }
        if self.width == 0 || self.height == 0 {
            display.draw_text(wx + 8, wy + 8, "(empty image)", DIM, 1);
            return;
        }

        // Draw clipped to the window content area — no scaling for now.
        let draw_w = self.width.min(ww);
        let draw_h = self.height.min(wh);
        for y in 0..draw_h {
            for x in 0..draw_w {
                display.put_pixel_pub(wx + x, wy + y, self.pixels[y * self.width + x]);
            }
        }

        let info = alloc::format!("{}x{} BMP", self.width, self.height);
        if draw_h + 12 <= wh {
            display.draw_text(wx + 4, wy + draw_h + 2, &info, DIM, 1);
        }
    }
}

pub static VIEWER: Mutex<Option<ImageViewer>> = Mutex::new(None);

/// Open `path` for viewing (called from the terminal `view` command or a
/// HepFS `.bmp` click). Reads the file from HepFS itself, mirroring `editor::open`.
pub fn open(path: &str) {
    let content = {
        let mut ctrl = crate::nvme::CONTROLLER.lock();
        if let Some(ctrl) = ctrl.as_mut() {
            match crate::hepfs::lookup(ctrl, path) {
                Some(ino) => crate::hepfs::read_file(ctrl, ino),
                None      => alloc::vec![],
            }
        } else { alloc::vec![] }
    };
    *VIEWER.lock() = Some(ImageViewer::from_bytes(path, &content));
}
