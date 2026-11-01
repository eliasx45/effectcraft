//! Fix 2: the GPU reads hardware-decoded ProRes pictures (4:2:2 `x422`, 4:4:4 + alpha `s4as`)
//! straight from VideoToolbox's surface, and decodes them to exactly the picture it decodes from
//! the uploaded CPU planes (bit-identical floats), with alpha; also once the media pool rounded
//! the frame to 8 bits (Decode Footage at 8 Bits), the surface going with the 8-bit frame. Kept surfaces stay within their
//! byte budget, and none is kept (all go upload) with a budget of 0. macOS (wgpu runs on Metal
//! there) with a hardware ProRes decoder; elsewhere the test notes why and passes. Its own test binary: the
//! surface registry is process-wide.
//!
//! Clips: small ffmpeg (`prores_ks`) movies generated into `target/fixtures/platform` (skipped
//! without ffmpeg), plus any movies listed in `EFFECTCRAFT_PRORES_SURFACE_CLIPS` (`:`-separated
//! paths; e.g. 4K ProRes 4444 + alpha and 422 HQ clips).

#![cfg(target_os = "macos")]

use effectcraft_platform::prores::VtProResDecoder;
use effectcraft_platform::surfaces;
use effectcraft_render::yuv::{ChromaSiting, Coefficients, Plane, Samples, View};
use filmcraft_codecs::VideoDecoder;
use filmcraft_frame::{Chroma, PixelData, VideoFrame};

/// An ffmpeg ProRes movie (`profile`, `pix_fmt`) generated once; `None` without ffmpeg.
fn ffmpeg_clip(name: &str, profile: &str, pix_fmt: &str, alpha: bool) -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fixtures/platform");
    let path = dir.join(name);
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join(format!("tmp-{}-{name}", std::process::id()));
        let src = if alpha {
            "testsrc2=size=352x198:rate=24:duration=0.25,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='255*X/W'"
        } else {
            "testsrc2=size=352x198:rate=24:duration=0.25"
        };
        let st = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", src, "-c:v", "prores_ks", "-profile:v", profile, "-pix_fmt", pix_fmt])
            .arg(&tmp)
            .status()
            .ok()?;
        assert!(st.success(), "ffmpeg failed");
        std::fs::rename(&tmp, &path).expect("rename");
    }
    Some(path)
}

/// The fourcc, size and first `n` samples of a ProRes movie.
fn samples(path: &std::path::Path, n: usize) -> ([u8; 4], u32, u32, Vec<Vec<u8>>) {
    let file = std::fs::File::open(path).expect("open clip");
    let mp4 = filmcraft_isobmff::open(&file).expect("parse clip");
    let ti = mp4.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("video track");
    let track = &mp4.tracks[ti];
    let entry = &track.entries[0];
    let filmcraft_isobmff::CodecConfig::ProRes { fourcc } = &entry.codec else { panic!("{}: not ProRes", path.display()) };
    let v = entry.video.as_ref().expect("video params");
    let n = n.min(track.samples.len());
    (fourcc.0, v.width.into(), v.height.into(), (0..n).map(|i| mp4.read_sample(&file, ti, i).expect("sample")).collect())
}

/// The native YUV view of a decoded frame (as the media pool builds it), video-range Rec. 709;
/// straight alpha.
fn view(f: &VideoFrame) -> View<'_> {
    let (planes, chroma, bits, alpha) = match &f.data {
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            (planes.each_ref().map(|p| Samples::U16(&p[..])), *chroma, *bits, alpha.as_ref().map(|a| Samples::U16(&a[..])))
        }
        PixelData::Yuv8 { planes, chroma, alpha } => (planes.each_ref().map(|p| Samples::U8(&p[..])), *chroma, 8, alpha.as_ref().map(|a| Samples::U8(&a[..]))),
        _ => panic!("not planar Y'CbCr"),
    };
    let (sx, sy) = match chroma {
        Chroma::C444 => (0, 0),
        Chroma::C422 => (1, 0),
        other => panic!("unexpected chroma {other:?}"),
    };
    let (w, h) = (f.width, f.height);
    let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
    let s = (1u32 << (bits - 8)) as f32;
    View {
        width: w,
        height: h,
        planes: [plane(planes[0], w, h), plane(planes[1], cw, ch), plane(planes[2], cw, ch)],
        alpha: alpha.map(|a| plane(a, w, h)),
        bits,
        subsampling: [sx, sy],
        siting: ChromaSiting::Centered,
        coefficients: Coefficients {
            luma: [16.0 * s, 1.0 / (219.0 * s)],
            chroma: [128.0 * s, 1.0 / (224.0 * s)],
            rgb: [1.5748, 0.46812427, 0.18732427, 1.8556],
        },
        alpha_mode: effectcraft_project::AlphaMode::Straight,
        matte: [0.0; 3],
        surface: None,
    }
}

#[test]
fn prores_pictures_read_from_the_decoder_surface_equal_the_uploaded_ones() {
    let Some(gpu) = effectcraft_gpu::Gpu::headless() else {
        eprintln!("no GPU: skipping");
        return;
    };
    effectcraft_gpu::surface::set_surface_import(surfaces::import);
    effectcraft_media::hw::set_eight_bit_hook(surfaces::rounded_to_8bit);
    // the first ask turns keeping surfaces on (nothing is kept before)
    let first = effectcraft_gpu::surface::SurfaceKey { luma: 1, width: 2, height: 2, bits: 12, subsampling: [0, 0], alpha: false };
    assert!(surfaces::import(gpu.device(), first).is_none());

    let mut clips: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (name, profile, pix_fmt, alpha) in
        [("prores_ks_4444_alpha_352x198.mov", "4444", "yuva444p10le", true), ("prores_ks_422hq_352x198.mov", "3", "yuv422p10le", false)]
    {
        match ffmpeg_clip(name, profile, pix_fmt, alpha) {
            Some(p) => clips.push((name.into(), p)),
            None => eprintln!("{name}: no ffmpeg: skipping"),
        }
    }
    for p in std::env::var("EFFECTCRAFT_PRORES_SURFACE_CLIPS").unwrap_or_default().split(':').filter(|p| !p.is_empty()) {
        clips.push((p.into(), p.into()));
    }
    for (name, path) in clips {
        let (fourcc, w, h, samples) = samples(&path, 6);
        let mut d = match VtProResDecoder::new(fourcc, w, h) {
            Ok(d) => d,
            Err(why) => {
                eprintln!("{name}: no hardware ProRes decoder ({why}): skipping");
                continue;
            }
        };
        let four = matches!(&fourcc, b"ap4h" | b"ap4x");
        let mut imported = 0;
        for (i, s) in samples.iter().enumerate() {
            let before = effectcraft_platform::stats();
            let frame = d.decode(s, i as i64).expect("decode").remove(0).frame;
            if effectcraft_platform::stats().frames == before.frames {
                // (the media engines are shared with other processes: a frame may fall back)
                eprintln!("{name}: frame {i} was decoded in software: not compared");
                continue;
            }
            let v = view(&frame);
            assert_eq!(v.alpha.is_some(), four, "{name}: alpha plane");
            assert!(surfaces::kept_bytes() <= surfaces::budget(), "{name}: kept surfaces within the budget");
            let direct = effectcraft_gpu::surface::direct_frames();
            let (from_surface, a) = gpu.decode_yuv(v, true, true).expect("decoded from the surface");
            let (uploaded, b) = gpu.decode_yuv(v, false, true).expect("decoded from uploaded planes");
            assert!(from_surface, "{name}: frame {i} read from the decoder surface");
            assert!(effectcraft_gpu::surface::direct_frames() > direct, "{name}: frame {i}: the decode read the surface directly (Fix 3c)");
            assert!(!uploaded);
            let (a, b) = (a.expect("read back"), b.expect("read back"));
            assert_eq!((a.width, a.height), (w, h));
            assert_same(&a.data, &b.data, &format!("{name}: frame {i}"));
            // the split pass (before Fix 3c, and the fallback) gives the same picture
            effectcraft_gpu::surface::set_direct(false);
            let split = gpu.decode_yuv(v, true, true);
            effectcraft_gpu::surface::set_direct(true);
            let (from_surface, c) = split.expect("decoded from split planes");
            assert!(from_surface, "{name}: frame {i}: split from the surface");
            assert_same(&c.expect("read back").data, &b.data, &format!("{name}: frame {i}, split"));
            if four && name.starts_with("prores_ks") {
                assert!(a.data.iter().any(|p| p[3] < 1.0) && a.data.iter().any(|p| p[3] > 0.99), "{name}: frame {i}: the alpha ramp came through");
            }
            // Decode Footage at 8 Bits: the pool keeps the frame rounded to 8 bits; the surface
            // goes with it, and the GPU rounds it exactly as the pool's CPU rounding does
            let eight = effectcraft_media::hw::round_to_8bit(&frame).expect("rounded to 8 bits");
            let e = view(&eight);
            let (from_surface, a8) = gpu.decode_yuv(e, true, true).expect("8-bit frame decoded from the surface");
            let (_, b8) = gpu.decode_yuv(e, false, true).expect("8-bit frame decoded from uploaded planes");
            assert!(from_surface, "{name}: frame {i}: the 8-bit frame read from the decoder surface");
            let b8 = b8.expect("read back");
            assert_same(&a8.expect("read back").data, &b8.data, &format!("{name}: frame {i} at 8 bits"));
            effectcraft_gpu::surface::set_direct(false);
            let split = gpu.decode_yuv(e, true, true);
            effectcraft_gpu::surface::set_direct(true);
            assert_same(&split.and_then(|(_, c)| c).expect("8-bit frame split").data, &b8.data, &format!("{name}: frame {i} at 8 bits, split"));
            let (full_again, _) = gpu.decode_yuv(v, true, false).expect("full frame uploaded");
            assert!(!full_again, "{name}: the surface went to the 8-bit frame");
            imported += 1;
        }
        assert!(imported * 2 > samples.len(), "{name}: most frames read from the decoder surface ({imported} of {})", samples.len());
        println!("{name}: {imported} frames read from the decoder surface (full depth and 8-bit), identical to the upload");
    }

    // no budget: nothing is kept, every frame is uploaded as before
    let saved = surfaces::budget();
    surfaces::set_budget(0);
    if let Some(path) = ffmpeg_clip("prores_ks_422hq_352x198.mov", "3", "yuv422p10le", false) {
        let (fourcc, w, h, samples) = samples(&path, 2);
        if let Ok(mut d) = VtProResDecoder::new(fourcc, w, h) {
            for (i, s) in samples.iter().enumerate() {
                let frame = d.decode(s, i as i64).expect("decode").remove(0).frame;
                let (from_surface, _) = gpu.decode_yuv(view(&frame), true, false).expect("uploaded");
                assert!(!from_surface, "a budget of 0 keeps no surface");
            }
        }
    }
    surfaces::set_budget(saved);
}

/// A plane of `w`×`h` samples, rows packed.
fn plane(samples: Samples<'_>, w: u32, h: u32) -> Plane<'_> {
    Plane { samples, width: w, height: h, stride: w as usize }
}

/// The two pictures are the same floats; the message says where not.
fn assert_same(a: &[[f32; 4]], b: &[[f32; 4]], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}");
    let differ = a.iter().zip(b).filter(|(x, y)| x != y).count();
    let first = a.iter().zip(b).position(|(x, y)| x != y);
    assert_eq!(differ, 0, "{what}: {differ} pixels differ, first at {first:?}: {:?} vs {:?}", first.map(|p| a[p]), first.map(|p| b[p]));
}
