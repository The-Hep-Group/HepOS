//! Minimal audio player — decodes uncompressed PCM WAV (16-bit, 48kHz,
//! mono or stereo) and plays it via the Intel HDA DMA stream.
//! Open with `play <file>` in the terminal.

use alloc::{string::String, vec::Vec};

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

/// Decode and play `path` from HepFS. Blocks until playback (+ drain) finishes.
/// Only 16-bit PCM WAV at 48 kHz (mono or stereo) is supported — no resampler.
pub fn play(path: &str) -> PlayResult {
    let content = {
        let mut ctrl = crate::nvme::CONTROLLER.lock();
        if let Some(ctrl) = ctrl.as_mut() {
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
