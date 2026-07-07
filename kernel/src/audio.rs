//! Minimal audio player — decodes uncompressed PCM WAV (16-bit, 48kHz,
//! mono or stereo) and plays it via the Intel HDA DMA stream.
//! Open with `play <file>` in the terminal, or click a `.wav` in HepFS.

use alloc::{string::String, vec::Vec};
use spin::Mutex;
use crate::framebuffer::{Color, Display};

/// Parse a RIFF/WAVE PCM file. Returns (sample_rate, channels, bits_per_sample, pcm_bytes).
/// Only PCM (audioFormat=1) is supported — no compressed WAV variants.
fn decode_wav(data: &[u8]) -> Option<(u32, u16, u16, &[u8])> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" { return None; }

    let mut pos = 12usize;
    let mut fmt: Option<(u32, u16, u16)> = None; // (sample_rate, channels, bits)
    let mut pcm: Option<&[u8]> = None;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body_start = pos + 8;
        if body_start + chunk_size > data.len() { break; }
        let body = &data[body_start..body_start + chunk_size];

        if chunk_id == b"fmt " {
            if body.len() < 16 { return None; }
            let audio_format = u16::from_le_bytes(body[0..2].try_into().ok()?);
            if audio_format != 1 { return None; } // only uncompressed PCM
            let channels    = u16::from_le_bytes(body[2..4].try_into().ok()?);
            let sample_rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
            let bits        = u16::from_le_bytes(body[14..16].try_into().ok()?);
            fmt = Some((sample_rate, channels, bits));
        } else if chunk_id == b"data" {
            pcm = Some(body);
        }

        // Chunks are padded to an even byte boundary.
        pos = body_start + chunk_size + (chunk_size & 1);
    }

    let (sample_rate, channels, bits) = fmt?;
    let pcm = pcm?;
    Some((sample_rate, channels, bits, pcm))
}

pub struct PlayResult {
    pub error:            Option<String>,
    pub duration_ms:      u64,
    pub truncated:        bool,
}

/// State shown by the Audio Player window. `hda::play_pcm()` is non-blocking —
/// it starts the DMA stream and returns immediately, so live progress comes
/// from `hda::progress_ms()`/`hda::is_playing()` each frame; this struct just
/// records which file/format is (or was) playing.
pub struct NowPlaying {
    pub path:        String,
    pub sample_rate:  u32,
    pub channels:     u16,
    pub duration_ms:  u64,
    pub truncated:    bool,
    pub error:        Option<String>,
}

pub static NOW_PLAYING: Mutex<Option<NowPlaying>> = Mutex::new(None);

/// Decode and start playing `path` from HepFS — returns as soon as the DMA
/// stream is started (playback continues in the background; see hda::poll()).
/// Only 16-bit PCM WAV at 48 kHz (mono or stereo) is supported — no resampler.
/// Updates `NOW_PLAYING` so the Audio Player window can show the result.
pub fn play(path: &str) -> PlayResult {
    let result = play_inner(path);
    *NOW_PLAYING.lock() = Some(NowPlaying {
        path:        String::from(path),
        sample_rate: 48_000,
        channels:    2,
        duration_ms: result.duration_ms,
        truncated:   result.truncated,
        error:       result.error.clone(),
    });
    result
}

fn play_inner(path: &str) -> PlayResult {
    let content = {
        let mut ctrl = crate::nvme::CONTROLLER.lock();
        if let Some(ctrl) = ctrl.as_mut() {
            let ctrl = &mut crate::hepfs::BlockDev::Nvme(ctrl);
            match crate::hepfs::lookup(ctrl, path) {
                Some(ino) => crate::hepfs::read_file(ctrl, ino),
                None => return PlayResult {
                    error: Some(String::from("file not found")), duration_ms: 0, truncated: false,
                },
            }
        } else {
            return PlayResult {
                error: Some(String::from("no storage controller")), duration_ms: 0, truncated: false,
            };
        }
    };

    let Some((sample_rate, channels, bits, pcm)) = decode_wav(&content) else {
        return PlayResult {
            error: Some(String::from("not a PCM WAV file")), duration_ms: 0, truncated: false,
        };
    };
    if bits != 16 {
        return PlayResult {
            error: Some(String::from("only 16-bit PCM WAV supported")), duration_ms: 0, truncated: false,
        };
    }
    if sample_rate != 48_000 {
        return PlayResult {
            error: Some(String::from("only 48kHz WAV supported (no resampler yet)")),
            duration_ms: 0, truncated: false,
        };
    }
    if channels != 1 && channels != 2 {
        return PlayResult {
            error: Some(String::from("only mono or stereo WAV supported")), duration_ms: 0, truncated: false,
        };
    }

    // Decode PCM bytes -> i16 samples, upmixing mono to stereo (HDA stream is fixed stereo).
    let mono_or_stereo: Vec<i16> = pcm.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let stereo: Vec<i16> = if channels == 2 {
        mono_or_stereo
    } else {
        let mut v = Vec::with_capacity(mono_or_stereo.len() * 2);
        for s in mono_or_stereo { v.push(s); v.push(s); }
        v
    };

    let (played, truncated) = crate::hda::play_pcm(&stereo);
    let duration_ms = (played as u64 / 2) * 1000 / 48_000;

    PlayResult { error: None, duration_ms, truncated }
}

/// Render the Audio Player window: last-played file, format, duration, or error.
pub fn render(display: &mut Display, wx: usize, wy: usize, ww: usize, wh: usize) {
    const BG:   Color = Color::from_hex(0x0A0A14);
    const ACC:  Color = Color::from_hex(0x6C8EFF);
    const TEXT: Color = Color::from_hex(0xE8E8E8);
    const DIM:  Color = Color::from_hex(0x888888);
    const OK:   Color = Color::from_hex(0x6BFF8E);
    const ERR:  Color = Color::from_hex(0xFF6B6B);

    display.fill_rect(wx, wy, ww, wh, BG);

    // Speaker icon (simple triangle-ish box, drawn with rects — no font glyph needed)
    let icon_x = wx + 16;
    let icon_y = wy + 16;
    display.fill_rect(icon_x, icon_y + 8, 10, 16, ACC);
    display.fill_rect(icon_x + 10, icon_y, 14, 32, ACC);

    let now = NOW_PLAYING.lock();
    let text_x = icon_x + 40;
    let mut y = wy + 16;
    match now.as_ref() {
        None => {
            display.draw_text(text_x, y, "No audio played yet", DIM, 1);
            y += 16;
            display.draw_text(wx + 8, y + 4, "Try: play /demo.wav", DIM, 1);
        }
        Some(np) => {
            display.draw_text(text_x, y, &np.path, TEXT, 1); y += 16;
            match &np.error {
                Some(e) => {
                    display.draw_text(text_x, y, "Error:", ERR, 1); y += 14;
                    display.draw_text(text_x, y, e, ERR, 1);
                }
                None => {
                    display.draw_text(text_x, y, &alloc::format!(
                        "{} Hz, {}-ch PCM16", np.sample_rate,
                        if np.channels == 1 { "1" } else { "2" }
                    ), DIM, 1);
                    y += 14;
                    if let Some((elapsed_ms, total_ms)) = crate::hda::progress_ms() {
                        display.draw_text(text_x, y, &alloc::format!(
                            "Playing... {}.{}s / {}.{}s",
                            elapsed_ms / 1000, (elapsed_ms % 1000) / 100,
                            total_ms / 1000,   (total_ms % 1000) / 100,
                        ), ACC, 1);
                        y += 14;
                        // Progress bar
                        let bar_w = ww.saturating_sub(text_x - wx + 8).max(20);
                        let fill_w = if total_ms > 0 { (bar_w * elapsed_ms as usize / total_ms as usize).min(bar_w) } else { 0 };
                        display.fill_rect(text_x, y, bar_w, 6, Color::from_hex(0x222244));
                        display.fill_rect(text_x, y, fill_w, 6, ACC);
                    } else {
                        display.draw_text(text_x, y, &alloc::format!(
                            "Played {} ms{}", np.duration_ms,
                            if np.truncated { " (truncated)" } else { "" }
                        ), OK, 1);
                    }
                }
            }
        }
    }
    drop(now);

    if wh > 60 {
        display.draw_text(wx + 8, wy + wh.saturating_sub(16),
            "play <file.wav> in the terminal to load another track", DIM, 1);
    }
}
