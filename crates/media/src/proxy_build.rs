//! Preview proxies built from a movie (the cloud server's automatic proxies of big clips): every
//! frame decoded, reduced by a whole factor with [`filmcraft_frame::VideoFrame::box_decimated`]
//! (each sample the mean of an n×n block, in the file's own Y'CbCr code values: no colour
//! conversion, alpha kept) and written as ProRes (4444 with alpha, else 422 HQ) in a QuickTime
//! movie at the source's bit depth, frame rate and colour tags. ProRes is intra-only, so a
//! proxy frame decodes on its own, in about 1/n² of the original's time.
//!
//! Only ProRes sources for now (4:2:2 or 4:4:4 planes the encoder takes as they are).

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use filmcraft_frame::{Chroma, PixelData, VideoFrame};
use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
use filmcraft_media::{FrameRequest, SharedSource};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use rayon::prelude::*;

use crate::{MediaError, Result};

/// A finished proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyBuilt {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub bytes: u64,
}

/// Frames decoded and encoded at once (each a few MB of planes).
const BATCH: usize = 16;

/// Whether [`build`] can make a proxy of a movie with this codec (as probed, e.g. `ProRes 4444`).
pub fn supports(codec: &str) -> bool {
    codec.to_ascii_lowercase().contains("prores")
}

/// The proxy's size for a `w`×`h` source reduced by `divisor` (rounded up, as box decimation
/// rounds).
pub fn proxy_size(w: u32, h: u32, divisor: u32) -> (u32, u32) {
    let d = divisor.max(1);
    (w.div_ceil(d), h.div_ceil(d))
}

/// Least coded bits per macroblock a proxy gets (flat areas still need their DC terms).
const MIN_BITS_PER_MB: u32 = 64;

/// Coded bits per 16×16 macroblock of a movie of `bytes` over `frames` frames of `w`×`h`.
fn source_bits_per_mb(bytes: Option<u64>, frames: u64, w: u32, h: u32) -> Option<u32> {
    let mbs = u64::from(w.div_ceil(16)).checked_mul(u64::from(h.div_ceil(16)))?.checked_mul(frames)?;
    let bits = bytes?.checked_mul(8)?.checked_div(mbs)?;
    u32::try_from(bits).ok().filter(|b| *b > 0)
}

/// Frames of a movie of `duration` at `rate` (rounded to the nearest frame).
pub(crate) fn frame_count(rate: FrameRate, duration: Tick) -> Option<u64> {
    if rate.num <= 0 || rate.den <= 0 || duration.0 <= 0 {
        return None;
    }
    let den = (TICKS_PER_SECOND as i128).checked_mul(rate.den as i128)?;
    let n = (duration.0 as i128).checked_mul(rate.num as i128)?.checked_add(den / 2)? / den;
    u64::try_from(n).ok().filter(|n| *n > 0)
}

/// ISO/IEC 23091-2 codes of FilmCraft's colour description (primaries, transfer, matrix).
pub(crate) fn color_codes(c: &filmcraft_color::ColorInfo) -> (u8, u8, u8) {
    use filmcraft_color::{Matrix, Primaries, Transfer};
    let p = match c.primaries {
        Primaries::Bt709 => 1,
        Primaries::Bt601_625 => 5,
        Primaries::Bt601_525 => 6,
        Primaries::Bt2020 => 9,
        Primaries::P3D65 => 12,
    };
    let t = match c.transfer {
        Transfer::Bt709 => 1,
        Transfer::Srgb => 13,
        Transfer::Linear => 8,
        Transfer::Pq => 16,
        Transfer::Hlg => 18,
    };
    let m = match c.matrix {
        Matrix::Bt709 => 1,
        Matrix::Bt601 => 6,
        Matrix::Bt2020Ncl => 9,
    };
    (p, t, m)
}

/// One reduced frame as ProRes input.
fn prores_frame(f: &VideoFrame, color: filmcraft_prores::ColorInfo) -> Result<filmcraft_prores::Frame> {
    let PixelData::Yuv16 { planes, chroma, bits, alpha } = &f.data else {
        return Err(MediaError::Unsupported("proxy: the decoder gave no 10–16-bit Y'CbCr planes".into()));
    };
    let chroma = match chroma {
        Chroma::C444 => filmcraft_prores::ChromaFormat::Yuv444,
        Chroma::C422 => filmcraft_prores::ChromaFormat::Yuv422,
        c => return Err(MediaError::Unsupported(format!("proxy: {} chroma can't be ProRes", c.label()))),
    };
    let depth = u8::try_from(*bits).ok().filter(|b| (8..=16).contains(b)).ok_or_else(|| MediaError::Unsupported(format!("proxy: {bits}-bit samples")))?;
    let mut out = filmcraft_prores::Frame::new(f.width, f.height, chroma, depth, false);
    out.y = planes[0].to_vec();
    out.cb = planes[1].to_vec();
    out.cr = planes[2].to_vec();
    out.alpha = alpha.as_ref().map(|a| a.to_vec());
    out.color = color;
    Ok(out)
}

fn io(path: &Path, e: impl std::fmt::Display) -> MediaError {
    MediaError::Io(format!("{}: {e}", path.display()))
}

/// The movie at `path` opened as frames read it (for benchmarks and checks).
pub fn open_movie(path: &str) -> Result<SharedSource> {
    crate::open::open_source(path, None).map(|(s, _)| s)
}

/// Build the proxy of the movie at `src` reduced by `divisor` (2 = Half) into `out`. The movie is
/// written next to it first and moved into place when complete, so `out` is never half written.
/// `cancel` stops it between batches (an error then, and no file); `progress(done, total)`
/// after each batch. Runs on the current rayon pool (install a small one to keep a busy
/// machine responsive).
pub fn build(src: &str, out: &Path, divisor: u32, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<ProxyBuilt> {
    let (source, _) = crate::open::open_source(src, None)?;
    build_from(&source, src, out, divisor, cancel, progress)
}

fn build_from(source: &SharedSource, src: &str, out: &Path, divisor: u32, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<ProxyBuilt> {
    let info = source.info();
    let v = info.video.as_ref().ok_or_else(|| MediaError::Unsupported(format!("{src}: no video")))?;
    if !supports(&v.codec) {
        return Err(MediaError::Unsupported(format!("{src}: proxies are made of ProRes movies only, not {}", v.codec)));
    }
    let n = usize::try_from(divisor).ok().filter(|d| (2..=8).contains(d)).ok_or_else(|| MediaError::Unsupported(format!("proxy divisor {divisor}")))?;
    let rate = v.frame_rate;
    let total = frame_count(rate, info.duration).ok_or_else(|| MediaError::Decode(format!("{src}: no frames")))?;
    let timescale = u32::try_from(rate.num).map_err(|_| MediaError::Unsupported(format!("{src}: frame rate {}/{}", rate.num, rate.den)))?;
    let sample_duration = u32::try_from(rate.den).map_err(|_| MediaError::Unsupported(format!("{src}: frame rate {}/{}", rate.num, rate.den)))?;
    let (w, h) = proxy_size(v.width, v.height, divisor);
    let (wq, hq) = (
        u16::try_from(w).map_err(|_| MediaError::Unsupported("proxy too wide".into()))?,
        u16::try_from(h).map_err(|_| MediaError::Unsupported("proxy too tall".into()))?,
    );
    let (p, t, m) = color_codes(&v.color);
    let color = filmcraft_prores::ColorInfo { primaries: p, transfer: t, matrix: m };
    // 4444 keeps alpha (and 4:4:4 chroma); a 4:2:2 source without alpha gets 422 HQ.
    let first = source.video_frame(FrameRequest::full(Tick::ZERO)).map_err(MediaError::from)?;
    let (keep444, has_alpha) = match &first.data {
        PixelData::Yuv16 { chroma, alpha, .. } => (*chroma == Chroma::C444 || alpha.is_some(), alpha.is_some()),
        _ => return Err(MediaError::Unsupported(format!("{src}: the decoder gave no 10–16-bit Y'CbCr planes"))),
    };
    if keep444 && !matches!(&first.data, PixelData::Yuv16 { chroma: Chroma::C444, .. }) {
        return Err(MediaError::Unsupported(format!("{src}: 4:2:2 with alpha")));
    }
    let profile = if keep444 { filmcraft_prores::Profile::P4444 } else { filmcraft_prores::Profile::Hq };
    let mut cfg = filmcraft_prores::EncoderConfig::new(profile, w, h);
    // The source's own density (a keyed clip is mostly empty: Elias's runs at ~360 bits per
    // macroblock, a third of 4444's nominal rate), not the profile's: decode time follows the
    // coded bytes, so a proxy at the nominal rate would decode barely faster.
    if let Some(bits) = source_bits_per_mb(info.file_size.or_else(|| std::fs::metadata(src).ok().map(|m| m.len())), total, v.width, v.height) {
        cfg.bits_per_mb = Some(bits.clamp(MIN_BITS_PER_MB, profile.nominal_bits_per_mb()));
    }
    cfg.color = color;
    cfg.encode_alpha = has_alpha;
    let enc = filmcraft_prores::Encoder::with_config(cfg);

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
        let mut mux = Mp4Writer::new(BufWriter::with_capacity(4 << 20, file), WriterOptions::new(Brand::Mov)).map_err(|e| io(&tmp, e))?;
        let mut entry = SampleEntry::prores(FourCc(profile.fourcc()), wq, hq);
        if let Some(vp) = entry.video.as_mut() {
            vp.color = Some(match v.color.range {
                filmcraft_color::Range::Full => {
                    filmcraft_isobmff::ColorInfo::Nclx { primaries: p.into(), transfer: t.into(), matrix: m.into(), full_range: true }
                }
                filmcraft_color::Range::Limited => filmcraft_isobmff::ColorInfo::Nclc { primaries: p.into(), transfer: t.into(), matrix: m.into() },
            });
        }
        let track = mux.add_track(TrackConfig::new(entry, timescale)).map_err(|e| io(&tmp, e))?;
        let mut done = 0u64;
        while done < total {
            if cancel.load(Ordering::Relaxed) {
                return Err(MediaError::Cancelled);
            }
            let end = done.saturating_add(BATCH as u64).min(total);
            let batch: Vec<Result<Option<Vec<u8>>>> = (done..end)
                .into_par_iter()
                .map(|i| {
                    let at = rate.tick_of(i64::try_from(i).unwrap_or(i64::MAX));
                    let frame: Arc<VideoFrame> = match source.video_frame(FrameRequest::full(at)) {
                        Ok(f) => f,
                        // The last frames of a movie whose duration rounds up: none to write.
                        Err(_) if i + 2 >= total && i > 0 => return Ok(None),
                        Err(e) => return Err(MediaError::from(e)),
                    };
                    let small = frame.box_decimated(n).ok_or_else(|| MediaError::Unsupported(format!("{src}: frame {i} can't be reduced")))?;
                    if (small.width, small.height) != (w, h) {
                        return Err(MediaError::Decode(format!("{src}: frame {i} is {}×{}, not {}×{}", frame.width, frame.height, v.width, v.height)));
                    }
                    let pf = prores_frame(&small, color)?;
                    enc.clone().encode(&pf).map(Some).map_err(|e| MediaError::Decode(format!("{src}: proxy frame {i}: {e}")))
                })
                .collect();
            for data in batch {
                if let Some(data) = data? {
                    mux.write_sample(track, WriteSample { data: &data, duration: sample_duration, composition_offset: 0, is_sync: true })
                        .map_err(|e| io(&tmp, e))?;
                }
            }
            done = end;
            progress(done, total);
        }
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
    Ok(ProxyBuilt { width: w, height: h, frames, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4444 + alpha movie of `n` frames at 12 bits, each a different ramp.
    fn movie_4444(path: &Path, w: u32, h: u32, n: u32) {
        use filmcraft_prores::{ChromaFormat, Encoder, Frame, Profile};
        let mut enc = Encoder::new(Profile::P4444, w, h);
        let file = std::fs::File::create(path).unwrap();
        let mut mux = Mp4Writer::new(BufWriter::new(file), WriterOptions::new(Brand::Mov)).unwrap();
        let vt = mux.add_track(TrackConfig::new(SampleEntry::prores(FourCc(*b"ap4h"), w as u16, h as u16), 30000)).unwrap();
        for i in 0..n {
            let mut fr = Frame::new(w, h, ChromaFormat::Yuv444, 12, true);
            for (k, y) in fr.y.iter_mut().enumerate() {
                *y = 256 + ((k as u32 % w) * 40 + i * 300) as u16 % 3000;
            }
            for (k, a) in fr.alpha.as_mut().unwrap().iter_mut().enumerate() {
                *a = if (k as u32 / w) < h / 2 { 4095 } else { 0 };
            }
            let data = enc.encode(&fr).unwrap();
            mux.write_sample(vt, WriteSample { data: &data, duration: 1001, composition_offset: 0, is_sync: true }).unwrap();
        }
        mux.finish().unwrap().flush().unwrap();
    }

    #[test]
    fn half_proxy_keeps_frames_rate_alpha_and_values() {
        let dir = std::env::temp_dir().join(format!("ec-proxy-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mov");
        let out = dir.join("sub/proxy.mov");
        movie_4444(&src, 64, 32, 5);
        let stop = AtomicBool::new(false);
        let mut seen = vec![];
        let built = build(src.to_str().unwrap(), &out, 2, &stop, &mut |d, t| seen.push((d, t))).unwrap();
        assert_eq!((built.width, built.height, built.frames), (32, 16, 5));
        assert_eq!(seen.last(), Some(&(5, 5)));
        assert!(!dir.join("sub/proxy.mov.part").exists());

        let (orig, _) = crate::open::open_source(src.to_str().unwrap(), None).unwrap();
        let (px, _) = crate::open::open_source(out.to_str().unwrap(), None).unwrap();
        let (vi, pi) = (orig.info().video.clone().unwrap(), px.info().video.clone().unwrap());
        assert_eq!((pi.width, pi.height, pi.frame_rate), (32, 16, vi.frame_rate));
        assert!(pi.has_alpha && pi.codec.to_ascii_lowercase().contains("prores"));
        assert_eq!(px.info().duration, orig.info().duration);
        for i in [0i64, 2, 4] {
            let t = vi.frame_rate.tick_of(i);
            let a = orig.video_frame(FrameRequest::full(t)).unwrap().box_decimated(2).unwrap();
            let b = px.video_frame(FrameRequest::full(t)).unwrap();
            let (PixelData::Yuv16 { planes: pa, alpha: aa, bits: ba, .. }, PixelData::Yuv16 { planes: pb, alpha: ab, bits: bb, .. }) = (&a.data, &b.data)
            else {
                panic!("planes")
            };
            assert_eq!(ba, bb);
            // ProRes is lossy: the re-encoded ramp stays within a few 12-bit codes.
            let worst = pa[0].iter().zip(pb[0].iter()).map(|(x, y)| (*x as i32 - *y as i32).abs()).max().unwrap();
            assert!(worst <= 24, "luma differs by {worst} codes at frame {i}");
            let (aa, ab) = (aa.as_ref().unwrap(), ab.as_ref().unwrap());
            assert_eq!(aa[0], ab[0]);
            assert_eq!(aa[aa.len() - 1], ab[ab.len() - 1]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("ec-proxy-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mov");
        let out = dir.join("proxy.mov");
        movie_4444(&src, 32, 16, 3);
        let stop = AtomicBool::new(true);
        assert!(matches!(build(src.to_str().unwrap(), &out, 2, &stop, &mut |_, _| {}), Err(MediaError::Cancelled)));
        assert!(!out.exists() && !dir.join("proxy.mov.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_counts_and_sizes() {
        let r = FrameRate::new(30000, 1001);
        assert_eq!(frame_count(r, r.tick_of(4500)), Some(4500));
        assert_eq!(frame_count(r, Tick(0)), None);
        assert_eq!(frame_count(FrameRate { num: 0, den: 1 }, Tick(5)), None);
        assert_eq!(proxy_size(1920, 1080, 2), (960, 540));
        assert_eq!(proxy_size(1921, 1081, 2), (961, 541));
        assert!(supports("Apple ProRes 4444") && !supports("H.264"));
        // 6.32 GB over 17 257 frames of 1080p: ~359 bits per macroblock.
        assert_eq!(source_bits_per_mb(Some(6_323_492_113), 17_257, 1920, 1080), Some(359));
        assert_eq!(source_bits_per_mb(None, 10, 16, 16), None);
        assert_eq!(source_bits_per_mb(Some(5), 0, 16, 16), None);
    }
}
