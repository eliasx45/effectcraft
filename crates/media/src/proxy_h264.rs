//! Automatic preview proxies (`proxy.autoCreate`): any movie the media layer decodes, every
//! frame resampled to a smaller size (area average of each plane, in the file's own Y'CbCr code
//! values when it has them) and written as 8-bit 4:2:0 H.264 in an MP4, with the source's frame
//! rate and colour tags. H.264 at proxy sizes is what the hardware decoder takes (unlike, say, a
//! 6K Level 6.0 stream), and short GOPs with no B-frames keep scrubbing cheap.
//!
//! The source is never written to; the proxy is written next to its final path first and moved
//! into place when complete. Alpha is not kept (callers only make proxies of opaque footage).

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use filmcraft_frame::{PixelData, VideoFrame};
use filmcraft_h264enc::{ColorConfig, EncoderConfig, PacketFormat, Preset, Profile, RateControl, YuvFrame};
use filmcraft_isobmff::{AvcConfig, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
use filmcraft_media::FrameRequest;
use rayon::prelude::*;

use crate::proxy_build::{ProxyBuilt, color_codes, frame_count};
use crate::{MediaError, Result};

/// Constant quality of the proxy (visually clean at proxy sizes, small files).
const CRF: f32 = 18.0;

/// The largest even size of `w`×`h`'s aspect that fits `max_w`×`max_h` and is no larger than the
/// source (at least 16×16).
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return (16, 16);
    }
    let s = (f64::from(max_w) / f64::from(w)).min(f64::from(max_h) / f64::from(h)).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(16);
    (even(f64::from(w) * s), even(f64::from(h) * s))
}

/// Source taps of one output sample along an axis: the first source index and the weights of
/// the source samples it covers (summing to 1).
fn axis(src: usize, dst: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src as f64 / dst.max(1) as f64;
    (0..dst)
        .map(|i| {
            let a = i as f64 * scale;
            let b = (a + scale).min(src as f64);
            let start = (a.floor() as usize).min(src.saturating_sub(1));
            let end = (b.ceil() as usize).clamp(start + 1, src.max(start + 1));
            let mut w: Vec<f32> = (start..end).map(|k| ((k as f64 + 1.0).min(b) - (k as f64).max(a)).max(0.0) as f32).collect();
            let sum: f32 = w.iter().sum();
            if sum > 0.0 {
                w.iter_mut().for_each(|x| *x /= sum);
            } else {
                w = vec![1.0];
            }
            (start, w)
        })
        .collect()
}

/// One plane of samples, read as 8-bit code values.
#[derive(Clone, Copy)]
enum Plane<'a> {
    U8(&'a [u8]),
    /// Samples of `bits` significant bits.
    U16(&'a [u16], u32),
}

impl Plane<'_> {
    fn len(&self) -> usize {
        match self {
            Plane::U8(p) => p.len(),
            Plane::U16(p, _) => p.len(),
        }
    }

    #[inline(always)]
    fn at(&self, i: usize) -> f32 {
        match self {
            Plane::U8(p) => f32::from(p[i]),
            Plane::U16(p, bits) => f32::from(p[i]) / (1u32 << bits.saturating_sub(8).min(8)) as f32,
        }
    }
}

/// `src` (`sw`×`sh`) area-resampled to `dw`×`dh` 8-bit samples.
fn resample(src: Plane, sw: usize, sh: usize, dw: usize, dh: usize) -> Result<Vec<u8>> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh {
        return Err(MediaError::Decode(format!("proxy: a {sw}×{sh} plane has {} samples", src.len())));
    }
    let hx = axis(sw, dw);
    let vy = axis(sh, dh);
    let mut tmp = vec![0f32; sh * dw];
    tmp.par_chunks_mut(dw).enumerate().for_each(|(y, row)| {
        let base = y * sw;
        for (x, (s, ws)) in hx.iter().enumerate() {
            row[x] = ws.iter().enumerate().map(|(k, w)| src.at(base + s + k) * w).sum();
        }
    });
    let mut out = vec![0u8; dw * dh];
    out.par_chunks_mut(dw).enumerate().for_each(|(y, row)| {
        let (s, ws) = &vy[y];
        for (x, o) in row.iter_mut().enumerate() {
            let v: f32 = ws.iter().enumerate().map(|(k, w)| tmp[(s + k) * dw + x] * w).sum();
            *o = v.round().clamp(0.0, 255.0) as u8;
        }
    });
    Ok(out)
}

/// 8-bit 4:2:0 planes of `f` at `w`×`h` (even), and whether they are full range.
fn yuv420(f: &VideoFrame, w: u32, h: u32) -> Result<([Vec<u8>; 3], bool)> {
    let (sw, sh) = (f.width as usize, f.height as usize);
    let (dw, dh) = (w as usize, h as usize);
    let full = f.color.range == filmcraft_color::Range::Full;
    let planes = |p: [Plane; 3], (sx, sy): (u32, u32)| -> Result<[Vec<u8>; 3]> {
        let (cw, ch) = (sw.div_ceil(1 << sx), sh.div_ceil(1 << sy));
        Ok([resample(p[0], sw, sh, dw, dh)?, resample(p[1], cw, ch, dw / 2, dh / 2)?, resample(p[2], cw, ch, dw / 2, dh / 2)?])
    };
    match &f.data {
        PixelData::Yuv8 { planes: p, chroma, .. } => Ok((planes([Plane::U8(&p[0]), Plane::U8(&p[1]), Plane::U8(&p[2])], chroma.shifts())?, full)),
        PixelData::Yuv16 { planes: p, chroma, bits, .. } => {
            Ok((planes([Plane::U16(&p[0], *bits), Plane::U16(&p[1], *bits), Plane::U16(&p[2], *bits)], chroma.shifts())?, full))
        }
        // RGB frames: BT.709 limited-range Y'CbCr at full size, then resampled.
        PixelData::Rgba8(_) | PixelData::RgbaF32(_) => {
            let rgba = f.to_rgba8();
            if rgba.len() < sw * sh * 4 {
                return Err(MediaError::Decode("proxy: short RGBA frame".into()));
            }
            let (mut y, mut cb, mut cr) = (vec![0u8; sw * sh], vec![0u8; sw * sh], vec![0u8; sw * sh]);
            for (i, px) in rgba.chunks_exact(4).take(sw * sh).enumerate() {
                let (r, g, b) = (f32::from(px[0]) / 255.0, f32::from(px[1]) / 255.0, f32::from(px[2]) / 255.0);
                let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                y[i] = (16.0 + 219.0 * l).round().clamp(0.0, 255.0) as u8;
                cb[i] = (128.0 + 224.0 * (b - l) / 1.8556).round().clamp(0.0, 255.0) as u8;
                cr[i] = (128.0 + 224.0 * (r - l) / 1.5748).round().clamp(0.0, 255.0) as u8;
            }
            Ok((planes([Plane::U8(&y), Plane::U8(&cb), Plane::U8(&cr)], (0, 0))?, false))
        }
    }
}

fn io(path: &Path, e: impl std::fmt::Display) -> MediaError {
    MediaError::Io(format!("{}: {e}", path.display()))
}

/// Build an H.264 proxy of the movie at `src` at `width`×`height` (even, no larger than the
/// source; see [`fit_size`]) into `out` (an `.mp4`). `cancel` stops it between frames (an error
/// then, and no file); `progress(done, total)` after each frame. Decodes may use the hardware
/// decoder (a proxy is a preview). Resampling and encoding use half the cores, so the editor
/// stays responsive.
pub fn build(src: &str, out: &Path, width: u32, height: u32, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<ProxyBuilt> {
    let threads = std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(2);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).thread_name(|i| format!("auto-proxy-{i}")).build().ok();
    let _hw = crate::hw::scope(true);
    let (source, _) = crate::open::open_source(src, None)?;
    let info = source.info();
    let v = info.video.as_ref().ok_or_else(|| MediaError::Unsupported(format!("{src}: no video")))?;
    if width < 16 || height < 16 || width % 2 != 0 || height % 2 != 0 || width > v.width.max(16) || height > v.height.max(16) {
        return Err(MediaError::Unsupported(format!("proxy size {width}×{height} for a {}×{} movie", v.width, v.height)));
    }
    let (wq, hq) = (
        u16::try_from(width).map_err(|_| MediaError::Unsupported("proxy too wide".into()))?,
        u16::try_from(height).map_err(|_| MediaError::Unsupported("proxy too tall".into()))?,
    );
    let rate = v.frame_rate;
    let total = frame_count(rate, info.duration).ok_or_else(|| MediaError::Decode(format!("{src}: no frames")))?;
    let bad_rate = || MediaError::Unsupported(format!("{src}: frame rate {}/{}", rate.num, rate.den));
    let (num, den) = (u32::try_from(rate.num).map_err(|_| bad_rate())?, u32::try_from(rate.den).map_err(|_| bad_rate())?);
    let (p, t, m) = color_codes(&v.color);

    let tmp: PathBuf = {
        let mut name = out.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".part");
        out.with_file_name(name)
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    let file = std::fs::File::create(&tmp).map_err(|e| io(&tmp, e))?;
    let result = (|| {
        let mut enc: Option<filmcraft_h264enc::Encoder> = None;
        let mut mux = Mp4Writer::new(BufWriter::with_capacity(4 << 20, file), WriterOptions::new(Brand::Mp4)).map_err(|e| io(&tmp, e))?;
        let mut track = None;
        let mut done = 0u64;
        let write = |mux: &mut Mp4Writer<BufWriter<std::fs::File>>, track: usize, ps: Vec<filmcraft_h264enc::Packet>| -> Result<()> {
            for pk in ps {
                let offset = i32::try_from((pk.pts - pk.dts).saturating_mul(i64::from(den))).unwrap_or(0);
                mux.write_sample(track, WriteSample { data: &pk.data, duration: den, composition_offset: offset, is_sync: pk.keyframe })
                    .map_err(|e| io(&tmp, e))?;
            }
            Ok(())
        };
        for i in 0..total {
            if cancel.load(Ordering::Relaxed) {
                return Err(MediaError::Cancelled);
            }
            let at = rate.tick_of(i64::try_from(i).unwrap_or(i64::MAX));
            let frame = match source.video_frame(FrameRequest::full(at)) {
                Ok(f) => f,
                // The last frames of a movie whose duration rounds up: none to write.
                Err(_) if i + 2 >= total && i > 0 => break,
                Err(e) => return Err(MediaError::from(e)),
            };
            let ([y, u, vv], full) = match &pool {
                Some(pool) => pool.install(|| yuv420(&frame, width, height))?,
                None => yuv420(&frame, width, height)?,
            };
            if enc.is_none() {
                let mut cfg = EncoderConfig::new(width, height, num, den);
                cfg.profile = Profile::High;
                cfg.preset = Preset::Speed;
                cfg.rate = RateControl::Crf(CRF);
                // A key frame every second, no B-frames: cheap seeks while scrubbing.
                cfg.keyint = (rate.as_f64().round() as u32).clamp(1, 120);
                cfg.bframes = 0;
                cfg.scenecut = false;
                cfg.threads = std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(2);
                cfg.format = PacketFormat::LengthPrefixed;
                cfg.aud = false;
                cfg.color = ColorConfig { primaries: p, transfer: t, matrix: m, full_range: full };
                let e = filmcraft_h264enc::Encoder::new(cfg).map_err(|e| MediaError::Unsupported(format!("proxy encoder: {e}")))?;
                let avc = AvcConfig::parse(&e.avcc()).map_err(|e| MediaError::Decode(format!("proxy encoder config: {e}")))?;
                let mut entry = SampleEntry::avc(avc, wq, hq);
                if let Some(vp) = entry.video.as_mut() {
                    vp.color = Some(filmcraft_isobmff::ColorInfo::Nclx { primaries: p.into(), transfer: t.into(), matrix: m.into(), full_range: full });
                }
                track = Some(mux.add_track(TrackConfig::new(entry, num)).map_err(|e| io(&tmp, e))?);
                enc = Some(e);
            }
            let (Some(e), Some(tr)) = (enc.as_mut(), track) else {
                return Err(MediaError::Decode("proxy encoder".into()));
            };
            let yf = YuvFrame { y: &y, u: &u, v: &vv, y_stride: width as usize, uv_stride: (width / 2) as usize };
            let packets = e.try_encode(&yf, i64::try_from(i).unwrap_or(i64::MAX)).map_err(|e| MediaError::Decode(format!("proxy frame {i}: {e}")))?;
            write(&mut mux, tr, packets)?;
            done = i + 1;
            progress(done, total);
        }
        let (Some(mut e), Some(tr)) = (enc, track) else {
            return Err(MediaError::Decode(format!("{src}: no frames decoded")));
        };
        write(&mut mux, tr, e.flush())?;
        let mut w = mux.finish().map_err(|e| io(&tmp, e))?;
        w.flush().map_err(|e| io(&tmp, e))?;
        let f = w.into_inner().map_err(|e| io(&tmp, e.error()))?;
        f.sync_all().map_err(|e| io(&tmp, e))?;
        Ok(done)
    })();
    let frames = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, out).map_err(|e| io(out, e))?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(ProxyBuilt { width, height, frames, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_even_and_never_grows() {
        assert_eq!(fit_size(6144, 3456, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(6144, 3456, 1080, 1920), (1080, 608));
        assert_eq!(fit_size(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_size(0, 10, 100, 100), (16, 16));
    }

    #[test]
    fn area_resample_averages_blocks_and_rejects_short_planes() {
        let src: Vec<u8> = (0..16).map(|i| if i % 4 < 2 { 0 } else { 200 }).collect();
        let out = resample(Plane::U8(&src), 4, 4, 2, 2).unwrap();
        assert_eq!(out, vec![0, 200, 0, 200]);
        let ten: Vec<u16> = vec![1023; 9];
        assert_eq!(resample(Plane::U16(&ten, 10), 3, 3, 2, 2).unwrap(), vec![255; 4]);
        assert!(resample(Plane::U8(&src), 5, 5, 2, 2).is_err());
    }

    /// A small H.264 movie made with the encoder, reduced to a proxy that decodes back at its
    /// size with every frame.
    #[test]
    fn builds_a_smaller_h264_movie_that_decodes() {
        let dir = std::env::temp_dir().join(format!("ec-proxy-h264-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let (w, h, n) = (128u32, 96u32, 6u32);
        {
            let mut cfg = EncoderConfig::new(w, h, 24, 1);
            cfg.format = PacketFormat::LengthPrefixed;
            cfg.aud = false;
            cfg.bframes = 0;
            let mut e = filmcraft_h264enc::Encoder::new(cfg).unwrap();
            let file = std::fs::File::create(&src).unwrap();
            let mut mux = Mp4Writer::new(BufWriter::new(file), WriterOptions::new(Brand::Mp4)).unwrap();
            let tr = mux.add_track(TrackConfig::new(SampleEntry::avc(AvcConfig::parse(&e.avcc()).unwrap(), w as u16, h as u16), 24)).unwrap();
            let mut packets = Vec::new();
            for i in 0..n {
                let y: Vec<u8> = (0..w * h).map(|k| ((k % w) * 2 + i * 10) as u8).collect();
                let c = vec![128u8; (w * h / 4) as usize];
                let f = YuvFrame { y: &y, u: &c, v: &c, y_stride: w as usize, uv_stride: (w / 2) as usize };
                packets.extend(e.try_encode(&f, i as i64).unwrap());
            }
            packets.extend(e.flush());
            for p in packets {
                mux.write_sample(tr, WriteSample { data: &p.data, duration: 1, composition_offset: 0, is_sync: p.keyframe }).unwrap();
            }
            let mut wr = mux.finish().unwrap();
            wr.flush().unwrap();
        }
        let out = dir.join("Proxies").join("src_64x48_proxy.mp4");
        let (pw, ph) = fit_size(w, h, 64, 64);
        let mut last = (0, 0);
        let built = build(src.to_str().unwrap(), &out, pw, ph, &AtomicBool::new(false), &mut |d, t| last = (d, t)).unwrap();
        assert_eq!((built.width, built.height), (64, 48));
        assert_eq!(built.frames, u64::from(n));
        assert_eq!(last, (u64::from(n), u64::from(n)));
        let probed = crate::probe(out.to_str().unwrap()).unwrap();
        assert_eq!((probed.width, probed.height), (64, 48));
        assert!(!dir.join("Proxies").join("src_64x48_proxy.mp4.part").exists());
        // Cancelled: no file.
        let gone = dir.join("Proxies").join("cancelled.mp4");
        assert!(build(src.to_str().unwrap(), &gone, pw, ph, &AtomicBool::new(true), &mut |_, _| {}).is_err());
        assert!(!gone.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
