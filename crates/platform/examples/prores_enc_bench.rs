//! Times the VideoToolbox ProRes encoder against FilmCraft's software encoder on a movie's
//! frames, writes both as movies, and compares the pictures they decode to (PSNR per plane,
//! alpha included). Optionally builds a half-size preview proxy both ways.
//!
//! `cargo run --release -p effectcraft-platform --example prores_enc_bench -- <file.mov> [frames] [threads] [out-dir] [proxy]`

#[cfg(target_os = "macos")]
fn main() {
    use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
    use filmcraft_prores::{EncoderConfig, Frame, Profile};
    use std::time::Instant;

    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: prores_enc_bench <file.mov> [frames] [threads] [out-dir] [proxy]");
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4).max(1);
    let out_dir = std::path::PathBuf::from(args.get(4).cloned().unwrap_or_else(|| ".".into()));
    let proxy = args.get(5).is_some_and(|s| s == "proxy");
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();

    let file = std::fs::File::open(path).expect("open");
    let mp4 = filmcraft_isobmff::open(&file).expect("parse");
    let ti = mp4.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("video track");
    let track = &mp4.tracks[ti];
    let entry = &track.entries[0];
    let filmcraft_isobmff::CodecConfig::ProRes { fourcc } = &entry.codec else { panic!("not ProRes") };
    let profile = Profile::from_fourcc(&fourcc.0).expect("ProRes profile");
    let n = frames.min(track.samples.len());
    let src: Vec<Frame> = (0..n).map(|i| filmcraft_prores::decode_frame(&mp4.read_sample(&file, ti, i).expect("sample")).expect("decode")).collect();
    let first = &src[0];
    let (w, h, depth, alpha) = (first.width, first.height, first.bit_depth, first.alpha.is_some());
    let mut cfg = EncoderConfig::new(profile, w, h);
    cfg.encode_alpha = alpha;
    cfg.color = first.color;
    println!(
        "{stem}: {} {w}x{h} {depth}-bit alpha {alpha} colour {}/{}/{}, {n} frames, {threads} threads",
        String::from_utf8_lossy(&fourcc.0),
        cfg.color.primaries,
        cfg.color.transfer,
        cfg.color.matrix
    );

    // the same frames on `threads` threads, each with its own encoder
    let run = |name: &str, threads: usize, enc: &(dyn Fn(&Frame) -> Vec<u8> + Sync)| -> Vec<Vec<u8>> {
        let t0 = Instant::now();
        let chunk = n.div_ceil(threads);
        let out: Vec<Vec<Vec<u8>>> = std::thread::scope(|s| {
            let hs: Vec<_> = src.chunks(chunk).map(|c| s.spawn(move || c.iter().map(enc).collect::<Vec<_>>())).collect();
            hs.into_iter().map(|h| h.join().expect("join")).collect()
        });
        let dt = t0.elapsed().as_secs_f64();
        let out: Vec<Vec<u8>> = out.into_iter().flatten().collect();
        let kb = out.iter().map(Vec::len).sum::<usize>() as f64 / n as f64 / 1024.0;
        println!("{name}: {n} frames in {dt:.3} s = {:.1} fps ({:.2} ms/frame), {kb:.0} KB/frame", n as f64 / dt, dt * 1e3 / n as f64);
        out
    };
    let sw_enc = |f: &Frame| filmcraft_prores::Encoder::with_config(cfg.clone()).encode(f).expect("software encode");
    let hw_enc = |f: &Frame| effectcraft_platform::prores_enc::encode(&cfg, f).expect("hardware encode");
    let _ = run("software, 1 thread (exports before Fix 4)", 1, &sw_enc);
    let sw = run(&format!("software, {threads} threads"), threads, &sw_enc);
    let _ = run("hardware, 1 thread", 1, &hw_enc);
    let hw = run(&format!("hardware, {threads} threads"), threads, &hw_enc);
    for (i, d) in hw.iter().enumerate() {
        effectcraft_media::prores_enc::consistent(&cfg, &src[i], d).expect("hardware frame header");
    }

    // both as movies for ffprobe / players
    let write = |tag: &str, samples: &[Vec<u8>]| {
        let p = out_dir.join(format!("{stem}_{tag}.mov"));
        let mut mux = Mp4Writer::new(std::io::BufWriter::new(std::fs::File::create(&p).expect("create")), WriterOptions::new(Brand::Mov)).expect("mux");
        let mut se = SampleEntry::prores(FourCc(profile.fourcc()), w as u16, h as u16);
        if let Some(vp) = se.video.as_mut() {
            vp.color = Some(filmcraft_isobmff::ColorInfo::Nclc {
                primaries: cfg.color.primaries.into(),
                transfer: cfg.color.transfer.into(),
                matrix: cfg.color.matrix.into(),
            });
        }
        let vt = mux.add_track(TrackConfig::new(se, 30000)).expect("track");
        for d in samples {
            mux.write_sample(vt, WriteSample { data: d, duration: 1001, composition_offset: 0, is_sync: true }).expect("sample");
        }
        std::io::Write::flush(&mut mux.finish().expect("finish")).expect("flush");
        println!("wrote {}", p.display());
    };
    write("sw", &sw);
    write("hw", &hw);

    // decoded back: PSNR per plane against the source frames, and hardware against software
    let opts = filmcraft_prores::DecodeOptions { bit_depth: Some(depth), threads: true };
    let back = |samples: &[Vec<u8>]| samples.iter().map(|d| filmcraft_prores::decode_frame_with(d, &opts).expect("decode back")).collect::<Vec<_>>();
    let (sb, hb) = (back(&sw), back(&hw));
    let peak = f64::from((1u32 << depth) - 1);
    let psnr = |a: &[Frame], b: &[Frame]| -> Vec<(String, f64, u32)> {
        let mut out = vec![];
        for (name, k) in [("Y", 0), ("Cb", 1), ("Cr", 2), ("A", 3)] {
            let mut se = 0f64;
            let mut cnt = 0usize;
            let mut worst = 0u32;
            for (x, y) in a.iter().zip(b) {
                let (p, q) = match k {
                    0 => (&x.y, &y.y),
                    1 => (&x.cb, &y.cb),
                    2 => (&x.cr, &y.cr),
                    _ => match (&x.alpha, &y.alpha) {
                        (Some(p), Some(q)) => (p, q),
                        (None, None) => continue,
                        _ => panic!("alpha presence differs"),
                    },
                };
                for (u, v) in p.iter().zip(q) {
                    let d = (*u as f64 - *v as f64).abs();
                    se += d * d;
                    worst = worst.max(d as u32);
                }
                cnt += p.len();
            }
            if cnt > 0 {
                let mse = se / cnt as f64;
                out.push((name.to_string(), if mse == 0.0 { f64::INFINITY } else { 10.0 * (peak * peak / mse).log10() }, worst));
            }
        }
        out
    };
    for (label, a, b) in [("software vs source", &src, &sb), ("hardware vs source", &src, &hb), ("hardware vs software", &sb, &hb)] {
        let s: Vec<String> = psnr(a, b).into_iter().map(|(p, db, m)| format!("{p} {db:.2} dB (max {m})")).collect();
        println!("PSNR {label}: {}", s.join(", "));
    }

    if proxy {
        effectcraft_media::prores_enc::register(effectcraft_platform::prores_enc::encode);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        for (tag, on) in [("sw", false), ("hw", true)] {
            effectcraft_media::prores_enc::set_enabled(on);
            let s0 = effectcraft_media::prores_enc::stats();
            let p = out_dir.join(format!("{stem}_proxy_{tag}.mov"));
            let t0 = Instant::now();
            let built = effectcraft_media::proxy_build::build(path, &p, 2, &cancel, &mut |_, _| {}).expect("proxy");
            let dt = t0.elapsed().as_secs_f64();
            let s1 = effectcraft_media::prores_enc::stats();
            println!(
                "proxy {tag}: {}x{} {} frames in {dt:.2} s = {:.1} fps, {:.0} KB/frame, hardware frames {}, fallbacks {} -> {}",
                built.width,
                built.height,
                built.frames,
                built.frames as f64 / dt,
                built.bytes as f64 / built.frames.max(1) as f64 / 1024.0,
                s1.hardware_frames - s0.hardware_frames,
                s1.fallbacks - s0.fallbacks,
                p.display()
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("VideoToolbox is macOS only");
}
