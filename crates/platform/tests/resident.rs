//! Fix 3: hardware-decoded ProRes frames stay in VideoToolbox's surface, the CPU copy made only
//! when something asks for CPU pixels. Through the media pool, as the app reads footage:
//!
//! - a GPU read (the native YUV decode, as a footage layer's GPU path does it) reads the surface
//!   and never makes a CPU copy, at full depth and at 8 bits (Decode Footage at 8 Bits), and
//!   gives the same picture as a frame copied at decode and uploaded;
//! - a CPU reader (a CPU render, Save Frame, a CPU-only layer step; `frame()` / `yuv()`) gets the
//!   frame copied out exactly once, the pixels of a frame copied at decode;
//! - the cache counts the surface, then the copy; exports (the exact view) never see one;
//! - past the held-pictures budget, or after a hardware failure, frames are copied at decode as
//!   before; pictures leave with their frames;
//! - pictures only a decoder's cache of recent frames holds (spare) are capped per decoder,
//!   counted in the pool's budget and let go first; a frame whose picture went is decoded again
//!   with the same pixels.
//!
//! macOS with a hardware ProRes decoder and a GPU; elsewhere it notes why and passes. Its own
//! test binary: surfaces, held pictures and their counters are process-wide.
//!
//! Clips: small ffmpeg (`prores_ks`) movies generated into `target/fixtures/platform` (skipped
//! without ffmpeg), plus any movies listed in `EFFECTCRAFT_PRORES_SURFACE_CLIPS` (`:`-separated).

#![cfg(target_os = "macos")]

use effectcraft_media::MediaPool;
use effectcraft_platform::{resident, surfaces};
use effectcraft_project::{EightBit, Footage, ItemId};
use effectcraft_render::FootageSource;
use effectcraft_time::Tick;

/// An ffmpeg ProRes movie (`profile`, `pix_fmt`, 0.25 s) generated once; `None` without ffmpeg.
fn ffmpeg_clip(name: &str, profile: &str, pix_fmt: &str, alpha: bool) -> Option<std::path::PathBuf> {
    ffmpeg_clip_of(name, profile, pix_fmt, alpha, "0.25")
}

/// [`ffmpeg_clip`], `seconds` long.
fn ffmpeg_clip_of(name: &str, profile: &str, pix_fmt: &str, alpha: bool, seconds: &str) -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fixtures/platform");
    let path = dir.join(name);
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join(format!("tmp-{}-{name}", std::process::id()));
        let src = if alpha {
            format!("testsrc2=size=352x198:rate=24:duration={seconds},format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='255*X/W'")
        } else {
            format!("testsrc2=size=352x198:rate=24:duration={seconds}")
        };
        let st = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &src, "-c:v", "prores_ks", "-profile:v", profile, "-pix_fmt", pix_fmt])
            .arg(&tmp)
            .status()
            .ok()?;
        assert!(st.success(), "ffmpeg failed");
        std::fs::rename(&tmp, &path).expect("rename");
    }
    Some(path)
}

/// A pool reading previews on the hardware decoder, as the app's (`eight`: Decode Footage at 8
/// Bits), frames kept on the GPU or not.
fn pool(eight: EightBit, resident_frames: bool) -> MediaPool {
    let p = MediaPool::new();
    p.set_hardware_decoding(true);
    p.set_eight_bit(eight);
    p.set_read_ahead(false);
    p.set_resident_frames(resident_frames);
    p
}

/// The middle of frame `i`.
fn at(f: &Footage, i: u32) -> Tick {
    f.frame_rate.tick_of(i64::from(i)) + Tick(f.frame_rate.frame_duration().0 / 2)
}

/// The two pictures are the same floats; the message says where not.
fn assert_same(a: &[[f32; 4]], b: &[[f32; 4]], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}");
    let differ = a.iter().zip(b).filter(|(x, y)| x != y).count();
    let first = a.iter().zip(b).position(|(x, y)| x != y);
    assert_eq!(differ, 0, "{what}: {differ} pixels differ, first at {first:?}: {:?} vs {:?}", first.map(|p| a[p]), first.map(|p| b[p]));
}

#[test]
fn prores_frames_stay_on_the_gpu_until_a_cpu_reader_asks() {
    if !matches!(effectcraft_platform::register(), effectcraft_platform::Availability::Available(_)) || !effectcraft_platform::prores::hardware_available() {
        eprintln!("no hardware ProRes decoder: skipping");
        return;
    }
    let Some(gpu) = effectcraft_gpu::Gpu::headless() else {
        eprintln!("no GPU: skipping");
        return;
    };
    // the first ask turns sharing surfaces on (nothing is kept before)
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
    let mut checked = 0;
    for (name, path) in &clips {
        let footage = effectcraft_media::probe(path).expect("probe");
        for eight in [EightBit::Off, EightBit::All] {
            for i in 0..3 {
                checked += usize::from(check(&gpu, &format!("{name} {eight:?} frame {i}"), &footage, eight, i));
            }
        }
    }
    assert!(clips.is_empty() || checked > 0, "no frame was kept on the GPU");
    println!("{checked} frames kept on the GPU checked");

    if let Some((name, path)) = clips.first() {
        bounds(&gpu, name, &effectcraft_media::probe(path).expect("probe"));
    }
    if let Some(path) = ffmpeg_clip_of("prores_ks_422hq_352x198_1s.mov", "3", "yuv422p10le", false, "1") {
        spare(&effectcraft_media::probe(&path).expect("probe"));
    }
}

/// Spare pictures (only the decoder's cache of recent frames holds them; FilmCraft keeps 64):
/// at most [`resident::SPARE_PER_DECODER`] per decoder, counted in the pool's budget and let go
/// first when it needs room; a frame whose picture went is decoded again, the same pixels.
fn spare(footage: &Footage) {
    let name = "1 s 422 HQ";
    let n = 16;
    let p = pool(EightBit::Off, true);
    let reference = pool(EightBit::Off, false);
    let kept =
        (0..n).filter(|i| p.frame_pixels(ItemId(1), footage, at(footage, *i)).is_some_and(|px| px.gpu_yuv().is_some_and(|v| v.surface.is_some()))).count();
    if kept < n as usize {
        eprintln!("{name}: {kept} of {n} frames kept on the GPU: spare pictures not checked");
        return;
    }
    assert!(resident::usage().0 >= n as usize, "{name}: the pool holds all {n}");
    // the pool lets its frames go; the decoder's cache still holds them, as spare pictures
    p.clear_frames();
    let shed = resident::shed_count();
    let _ = p.frame_pixels(ItemId(1), footage, at(footage, n));
    let (held, _) = resident::usage();
    // (the pool's newest frame, and the decoder's newest spare ones)
    assert!(held <= 1 + resident::SPARE_PER_DECODER, "{name}: {held} pictures held after the pool let go");
    assert!(resident::shed_count() >= shed + (n as u64 - resident::SPARE_PER_DECODER as u64), "{name}: the older spare pictures went");
    let spare = resident::spare_bytes();
    assert!(spare > 0, "{name}: spare pictures are counted");
    assert!(p.stats().bytes >= spare, "{name}: in the pool's budget ({} < {spare})", p.stats().bytes);

    // a frame whose picture went (frame 0: the decoder's cache still has its stand-in) is decoded
    // again: the pixels of a frame copied at decode
    let t = at(footage, 0);
    let img = p.frame(ItemId(1), footage, t).expect("frame decoded again");
    let want = reference.frame(ItemId(1), footage, t).expect("reference");
    assert_same(&img.data, &want.data, &format!("{name}: frame 0 decoded again"));
    let started = std::time::Instant::now();
    p.clear_frames();
    let _ = p.frame_pixels(ItemId(1), footage, at(footage, 1)).expect("frame 1 decoded again");
    eprintln!("{name}: a frame whose picture went decoded again in {:.2} ms", started.elapsed().as_secs_f64() * 1e3);

    // the budget needs room: spare pictures go first
    let before = resident::spare_bytes();
    let small = MediaPool::with_budget(1);
    small.set_hardware_decoding(true);
    small.set_read_ahead(false);
    let _ = small.frame_pixels(ItemId(1), footage, at(footage, 2));
    assert!(resident::spare_bytes() < before.max(1), "{name}: spare pictures shed for the budget ({before} -> {})", resident::spare_bytes());
    drop((small, p, reference));
}

/// One frame through a pool that keeps it on the GPU, against one that copies it at decode.
/// Whether it was kept on the GPU (a frame the hardware didn't decode is not compared).
fn check(gpu: &effectcraft_gpu::Gpu, what: &str, footage: &Footage, eight: EightBit, i: u32) -> bool {
    let t = at(footage, i);
    // fresh pools: a CPU read makes a movie's next frames copy at decode for a while
    let (p, reference) = (pool(eight, true), pool(eight, false));
    let fallbacks = effectcraft_platform::stats().fallbacks;
    let px = p.frame_pixels(ItemId(1), footage, t).expect("frame");
    let view = px.gpu_yuv().expect("a native view");
    if view.surface.is_none() || effectcraft_platform::stats().fallbacks != fallbacks {
        eprintln!(
            "{what}: decoded in software or copied at decode: not compared (held {:?} of {} bytes, fallbacks {} -> {})",
            resident::usage(),
            resident::budget(),
            fallbacks,
            effectcraft_platform::stats().fallbacks
        );
        return false;
    }
    let copies = |p: &MediaPool| p.stats().copies;
    let (_, copied_before) = resident::counters();
    if eight == EightBit::All {
        assert_eq!(view.bits, 8, "{what}: 8-bit view");
    }
    assert!(view.planes.iter().all(|p| p.samples.is_empty()), "{what}: no CPU samples");

    // the GPU reads the surface: no CPU copy
    let (from_surface, a) = gpu.decode_yuv(view, true, true).expect("decoded from the surface");
    assert!(from_surface, "{what}: read from the surface");
    assert_eq!(copies(&p), 0, "{what}: a GPU read made no CPU copy");
    assert_eq!(resident::counters().1, copied_before, "{what}: nothing copied out");
    // without the surface there is nothing to upload: the GPU asks the frame for its planes
    assert!(gpu.decode_yuv(view, false, false).is_none(), "{what}: no samples to upload");

    // the same picture as a frame copied at decode, uploaded
    let r = reference.frame_pixels(ItemId(1), footage, t).expect("reference frame");
    if effectcraft_platform::stats().fallbacks != fallbacks {
        eprintln!("{what}: the reference was decoded in software: not compared");
        return false;
    }
    let rv = r.yuv().expect("reference view");
    assert!(rv.surface.is_none(), "{what}: the reference holds CPU samples");
    let (_, b) = gpu.decode_yuv(rv, false, true).expect("reference uploaded");
    let b = b.expect("read back");
    assert_same(&a.expect("read back").data, &b.data, &format!("{what}: GPU picture"));
    // read directly (Fix 3c) or split into planes first: the same picture, still no copy
    effectcraft_gpu::surface::set_direct(false);
    let split = gpu.decode_yuv(view, true, true);
    effectcraft_gpu::surface::set_direct(true);
    assert_same(&split.and_then(|(_, c)| c).expect("split").data, &b.data, &format!("{what}: split from the surface"));
    assert_eq!(copies(&p), 0, "{what}: the split made no CPU copy either");

    // memory: the cache counts the surface, then the copy
    // (the pool's own frames: spare pictures of earlier pools' decoders count in its budget too)
    let own = |p: &MediaPool| p.stats().bytes.saturating_sub(resident::spare_bytes());
    let held = own(&p);
    let pixels = (footage.width as usize) * (footage.height as usize);
    assert!(held >= pixels * 2, "{what}: the surface is counted ({held} bytes)");
    let usage = p.cache_usage();
    assert!(usage.surface_frames >= 1 && usage.surface_bytes > 0, "{what}: held pictures reported: {usage:?}");

    // a CPU reader: copied out once, the pixels of the frame copied at decode
    let img = p.frame(ItemId(1), footage, t).expect("CPU frame");
    let again = p.frame(ItemId(1), footage, t).expect("CPU frame again");
    let cpu_view = px.yuv().expect("CPU view");
    assert!(cpu_view.surface.is_none() && cpu_view.validate().is_ok(), "{what}: the CPU view holds samples");
    assert_eq!(copies(&p), 1, "{what}: copied exactly once");
    assert_eq!(resident::counters().1, copied_before + 1, "{what}: one copy out of the surface");
    let want = reference.frame(ItemId(1), footage, t).expect("reference CPU frame");
    assert_same(&img.data, &want.data, &format!("{what}: CPU pixels"));
    assert_same(&again.data, &want.data, &format!("{what}: CPU pixels again"));
    // the CPU view uploaded is the same picture too (what a GPU that can't read the surface does)
    let (_, c) = gpu.decode_yuv(cpu_view, false, true).expect("CPU planes uploaded");
    assert_same(&c.expect("read back").data, &b_again(gpu, &reference, footage, t), &format!("{what}: uploaded copy"));
    assert!(own(&p) > held, "{what}: the copy is counted ({} -> {})", held, own(&p));

    // exports read the exact view: never a frame kept on the GPU
    let exact = p.exact().frame_pixels(ItemId(1), footage, t).expect("exact frame");
    assert!(exact.gpu_yuv().is_some_and(|v| v.surface.is_none()), "{what}: exports read CPU samples");
    true
}

/// The reference frame's uploaded picture.
fn b_again(gpu: &effectcraft_gpu::Gpu, reference: &MediaPool, footage: &Footage, t: Tick) -> Vec<[f32; 4]> {
    let r = reference.frame_pixels(ItemId(1), footage, t).expect("reference frame");
    gpu.decode_yuv(r.yuv().expect("view"), false, true).and_then(|(_, img)| img).expect("uploaded").data
}

/// Past the held-pictures budget or after a hardware failure frames are copied at decode (as
/// before Fix 3, the GPU still reads Fix 2's kept surface); held pictures leave with their frames.
fn bounds(gpu: &effectcraft_gpu::Gpu, name: &str, footage: &Footage) {
    let kept_at_decode = |p: &MediaPool, i| p.frame_pixels(ItemId(1), footage, at(footage, i)).and_then(|px| px.gpu_yuv().map(|v| v.surface.is_some()));

    let saved = resident::budget();
    resident::set_budget(0);
    let p = pool(EightBit::Off, true);
    assert_eq!(kept_at_decode(&p, 0), Some(false), "{name}: a budget of 0 holds no picture");
    let px = p.frame_pixels(ItemId(1), footage, at(footage, 0)).expect("frame");
    let v = px.gpu_yuv().expect("view");
    let (from_surface, _) = gpu.decode_yuv(v, true, false).expect("decoded");
    eprintln!("{name}: copied at decode; GPU read from the kept surface: {from_surface}");
    resident::set_budget(saved);
    drop((px, p));

    // a CPU read makes the movie's next frames copy at decode for a while
    let p = pool(EightBit::Off, true);
    if kept_at_decode(&p, 0) == Some(true) {
        assert!(p.frame(ItemId(1), footage, at(footage, 0)).is_some());
        assert_eq!(kept_at_decode(&p, 1), Some(false), "{name}: after a CPU read the next frame is copied at decode");
    }
    drop(p);

    // pictures leave with their frames: a 1-byte cache keeps only the newest frame, and the
    // decoder's cache of recent frames goes with the pool
    let p = MediaPool::with_budget(1);
    p.set_hardware_decoding(true);
    p.set_read_ahead(false);
    for i in 0..3 {
        let _ = p.frame_pixels(ItemId(1), footage, at(footage, i));
    }
    assert!(p.stats().frames <= 1, "the newest frame stays");
    p.clear();
    drop(p);
    let (held, bytes) = resident::usage();
    assert_eq!((held, bytes), (0, 0), "{name}: no picture held once every frame is gone");
}
