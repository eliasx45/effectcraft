//! Playback-stream chunks (`plan/cloud-render.md` CR6): a run of rendered viewer frames encoded
//! as one self-contained H.264 stream (FilmCraft's pure-Rust `h264enc`), and decoded back.
//!
//! A chunk is one closed GOP: Annex-B NAL units with SPS/PPS and an IDR first, P frames after it,
//! no B frames (no reordering delay, so the first frame is out as soon as it is encoded). Any chunk
//! decodes on its own, so chunks can be encoded in parallel, sent in any order and dropped
//! independently.
//!
//! Frames come in as the viewer's premultiplied RGBA8. H.264 has no alpha, so the frames are
//! composited over a background colour (the comp's) before the BT.709 limited-range 4:2:0
//! conversion; decoded frames are opaque.

use filmcraft_h264enc::{EncoderConfig, PacketFormat, Preset, Profile, RateControl, YuvFrame};
use rayon::prelude::*;

/// How to encode a chunk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkSpec {
    /// Encoded size (even; [`even`] makes it so).
    pub width: u32,
    pub height: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    /// Constant quality, roughly the P-frame QP (0–51, lower is better). 23 is visually clean for
    /// motion graphics at 1080p.
    pub crf: f32,
    /// Encoder worker threads for this chunk (1: on the calling thread; slices follow it).
    pub threads: usize,
    /// `h264enc`'s speed/quality trade-off.
    pub preset: ChunkPreset,
}

/// [`ChunkSpec::preset`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChunkPreset {
    #[default]
    Speed,
    Balanced,
}

impl ChunkSpec {
    /// A 1-thread, CRF 23, speed-preset chunk of `width`×`height` at `rate_num/rate_den` fps.
    pub fn new(width: u32, height: u32, rate_num: u32, rate_den: u32) -> ChunkSpec {
        ChunkSpec { width, height, rate_num, rate_den, crf: 23.0, threads: 1, preset: ChunkPreset::Speed }
    }
}

/// `n` rounded down to an even size of at least 2 (H.264 4:2:0 needs even sizes).
pub fn even(n: u32) -> u32 {
    (n & !1).max(2)
}

/// One decoded frame: opaque RGBA8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Premultiplied RGBA8 (`w`×`h`, rows `w * 4` bytes) over `bg` (linear 0–1 sRGB-encoded values,
/// as the comp's background colour) → BT.709 limited-range 4:2:0 planes of `ew`×`eh` (the even
/// encoded size: the last odd row or column is dropped). Rows are converted in parallel.
pub fn rgba_to_yuv420(rgba: &[u8], w: u32, h: u32, bg: [f32; 3], ew: u32, eh: u32) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let (w, h, ew, eh) = (w as usize, h as usize, ew as usize, eh as usize);
    let need = w.checked_mul(h).and_then(|n| n.checked_mul(4)).ok_or("frame too large")?;
    if rgba.len() < need || ew > w || eh > h || ew % 2 != 0 || eh % 2 != 0 || ew == 0 || eh == 0 {
        return Err(format!("a {w}×{h} frame of {} bytes can't be encoded at {ew}×{eh}", rgba.len()));
    }
    let (cw, ch) = (ew / 2, eh / 2);
    let mut y = vec![0u8; ew * eh];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    let bg = bg.map(|c| c.clamp(0.0, 1.0) * 255.0);
    let stride = w * 4;
    y.par_chunks_mut(ew * 2).zip(u.par_chunks_mut(cw).zip(v.par_chunks_mut(cw))).enumerate().for_each(|(cy, (yr, (ur, vr)))| {
        let mut acc = vec![[0f32; 2]; cw];
        for dy in 0..2 {
            let Some(src) = rgba.get((cy * 2 + dy) * stride..).and_then(|s| s.get(..ew * 4)) else { continue };
            let Some(row) = yr.get_mut(dy * ew..(dy + 1) * ew) else { continue };
            for (x, (px, out)) in src.as_chunks::<4>().0.iter().zip(row.iter_mut()).enumerate() {
                let rest = 1.0 - px[3] as f32 / 255.0;
                let r = (px[0] as f32 + bg[0] * rest) / 255.0;
                let g = (px[1] as f32 + bg[1] * rest) / 255.0;
                let b = (px[2] as f32 + bg[2] * rest) / 255.0;
                let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                *out = (16.0 + 219.0 * l).round().clamp(1.0, 254.0) as u8;
                if let Some(a) = acc.get_mut(x / 2) {
                    a[0] += (b - l) / 1.8556;
                    a[1] += (r - l) / 1.5748;
                }
            }
        }
        for ((a, uo), vo) in acc.iter().zip(ur.iter_mut()).zip(vr.iter_mut()) {
            *uo = (128.0 + 224.0 * a[0] / 4.0).round().clamp(1.0, 254.0) as u8;
            *vo = (128.0 + 224.0 * a[1] / 4.0).round().clamp(1.0, 254.0) as u8;
        }
    });
    Ok((y, u, v))
}

/// Encode `frames` (premultiplied RGBA8 of `src_w`×`src_h`, at least one) as one chunk.
pub fn encode_chunk(frames: &[&[u8]], src_w: u32, src_h: u32, bg: [f32; 3], spec: &ChunkSpec) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("a chunk needs a frame".into());
    }
    let mut cfg = EncoderConfig::new(spec.width, spec.height, spec.rate_num, spec.rate_den);
    cfg.profile = Profile::High;
    cfg.preset = match spec.preset {
        ChunkPreset::Speed => Preset::Speed,
        ChunkPreset::Balanced => Preset::Balanced,
    };
    cfg.rate = RateControl::Crf(spec.crf.clamp(0.0, 51.0));
    // One GOP per chunk: the IDR first and no other.
    cfg.keyint = u32::try_from(frames.len()).unwrap_or(u32::MAX).saturating_add(1);
    cfg.bframes = 0;
    cfg.scenecut = false;
    cfg.threads = spec.threads.max(1);
    cfg.format = PacketFormat::AnnexB;
    cfg.aud = false;
    let mut enc = filmcraft_h264enc::Encoder::new(cfg).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let (y, u, v) = rgba_to_yuv420(f, src_w, src_h, bg, spec.width, spec.height)?;
        let frame = YuvFrame { y: &y, u: &u, v: &v, y_stride: spec.width as usize, uv_stride: (spec.width / 2) as usize };
        for p in enc.try_encode(&frame, i as i64).map_err(|e| e.to_string())? {
            out.extend_from_slice(&p.data);
        }
    }
    for p in enc.flush() {
        out.extend_from_slice(&p.data);
    }
    Ok(out)
}

/// Decode a chunk into opaque RGBA8 frames, in display order. `threads`: decoder threads (1 =
/// the calling thread).
pub fn decode_chunk(bytes: &[u8], threads: usize) -> Result<Vec<ChunkFrame>, String> {
    let mut dec = filmcraft_h264::Decoder::with_threads(threads.max(1));
    let mut pics = dec.decode(bytes, 0).map_err(|e| format!("H.264 chunk: {e}"))?;
    pics.extend(dec.flush());
    pics.sort_by_key(|p| p.poc);
    Ok(pics.par_iter().map(yuv420_to_rgba).collect())
}

/// BT.709 limited-range 4:2:0 → opaque RGBA8 (nearest chroma).
fn yuv420_to_rgba(p: &filmcraft_h264::Picture) -> ChunkFrame {
    let (w, h) = (p.width as usize, p.height as usize);
    let mut rgba = vec![255u8; w * h * 4];
    rgba.par_chunks_mut(w * 4).enumerate().for_each(|(row, out)| {
        let ys = p.y.get(row * p.y_stride..).unwrap_or(&[]);
        let cr = row / 2 * p.uv_stride;
        let (us, vs) = (p.u.get(cr..).unwrap_or(&[]), p.v.get(cr..).unwrap_or(&[]));
        for (x, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let yv = (*ys.get(x).unwrap_or(&16) as f32 - 16.0) / 219.0;
            let cb = (*us.get(x / 2).unwrap_or(&128) as f32 - 128.0) / 224.0;
            let crv = (*vs.get(x / 2).unwrap_or(&128) as f32 - 128.0) / 224.0;
            let r = yv + 1.5748 * crv;
            let b = yv + 1.8556 * cb;
            let g = (yv - 0.2126 * r - 0.0722 * b) / 0.7152;
            let q = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            px[0] = q(r);
            px[1] = q(g);
            px[2] = q(b);
        }
    });
    ChunkFrame { width: p.width, height: p.height, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moving gradient with a half-transparent square: something like a motion graphic.
    fn frame(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let inside = (x + t * 3) % w < w / 3 && y > h / 4 && y < h * 3 / 4;
                let (r, g, b) = ((x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y + t * 5) % 256) as u8);
                if inside {
                    // 50 % white, premultiplied.
                    v.extend_from_slice(&[128, 128, 128, 128]);
                } else {
                    v.extend_from_slice(&[r, g, b, 255]);
                }
            }
        }
        v
    }

    #[test]
    fn a_chunk_decodes_alone_with_every_frame_close_to_its_source() {
        let (w, h) = (128u32, 72u32);
        let frames: Vec<Vec<u8>> = (0..6).map(|t| frame(w, h, t)).collect();
        let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        let spec = ChunkSpec { crf: 18.0, ..ChunkSpec::new(w, h, 30000, 1001) };
        let bytes = encode_chunk(&refs, w, h, [0.0, 0.0, 0.0], &spec).unwrap();
        // Annex B with SPS (7), PPS (8) and an IDR (5) before any other slice (an SEI, 6, may sit
        // between them).
        let nal_types: Vec<u8> = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, s)| s[..3] == [0, 0, 1])
            .filter_map(|(i, _)| bytes.get(i + 3).map(|b| b & 31))
            .filter(|t| *t != 6)
            .collect();
        assert_eq!(&nal_types[..3], &[7, 8, 5], "{nal_types:?}");
        assert_eq!(nal_types.iter().filter(|t| **t == 5).count(), 1, "one IDR per chunk");
        let back = decode_chunk(&bytes, 1).unwrap();
        assert_eq!(back.len(), 6);
        for (src, got) in frames.iter().zip(&back) {
            assert_eq!((got.width, got.height), (w, h));
            let diff: f64 = src
                .as_chunks::<4>()
                .0
                .iter()
                .zip(got.rgba.as_chunks::<4>().0.iter())
                .map(|(a, b)| (0..3).map(|c| (a[c] as f64 - b[c] as f64).abs()).sum::<f64>())
                .sum();
            let mean = diff / (w * h * 3) as f64;
            assert!(mean < 6.0, "mean error {mean}");
            assert!(got.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "opaque");
        }
    }

    #[test]
    fn transparency_takes_the_background_and_odd_sizes_drop_an_edge() {
        let (w, h) = (33u32, 19u32);
        let clear = vec![0u8; (w * h * 4) as usize];
        let spec = ChunkSpec { crf: 10.0, ..ChunkSpec::new(even(w), even(h), 30, 1) };
        let bytes = encode_chunk(&[&clear], w, h, [1.0, 0.0, 0.0], &spec).unwrap();
        let back = decode_chunk(&bytes, 1).unwrap();
        assert_eq!((back[0].width, back[0].height), (32, 18));
        let p = &back[0].rgba[(9 * 32 + 16) * 4..][..3];
        assert!(p[0] > 230 && p[1] < 25 && p[2] < 25, "{p:?}");
    }

    #[test]
    fn bad_input_is_an_error_not_a_crash() {
        let spec = ChunkSpec::new(16, 16, 30, 1);
        assert!(encode_chunk(&[], 16, 16, [0.0; 3], &spec).is_err());
        assert!(encode_chunk(&[&[0u8; 10]], 16, 16, [0.0; 3], &spec).is_err());
        assert!(encode_chunk(&[&[0u8; 16 * 16 * 4]], 16, 16, [0.0; 3], &ChunkSpec::new(18, 16, 30, 1)).is_err());
        assert!(decode_chunk(&[0, 0, 1, 0x65, 1, 2, 3], 1).is_ok_and(|f| f.is_empty()) || decode_chunk(&[0, 0, 1, 0x65, 1, 2, 3], 1).is_err());
        assert!(decode_chunk(&[], 1).is_ok_and(|f| f.is_empty()));
    }
}
