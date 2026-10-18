//! `cargo run --release -p effectcraft-media --example proxy_bench -- MOVIE PROXY.mov [--build]
//! [--frames N] [--at SECONDS] [--only original|proxy]`: builds the half-size proxy of MOVIE (with `--build`, or when
//! PROXY.mov is missing), then decodes N frames from `--at` of each, one after another as
//! playback reads them, and prints decode ms per frame (p50 / p90) and peak memory.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use filmcraft_media::FrameRequest;
use filmcraft_time::Tick;

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
}

fn decode(path: &str, at: f64, n: i64) -> Result<(f64, f64, u32, u32), String> {
    let src = effectcraft_media::proxy_build::open_movie(path).map_err(|e| e.to_string())?;
    let v = src.info().video.clone().ok_or("no video")?;
    let first = v.frame_rate.frame_at(Tick((at * filmcraft_time::TICKS_PER_SECOND as f64) as i64));
    let mut ms = vec![];
    for i in first..first + n {
        let t0 = Instant::now();
        let f = src.video_frame(FrameRequest::full(v.frame_rate.tick_of(i))).map_err(|e| e.to_string())?;
        ms.push(t0.elapsed().as_secs_f64() * 1e3);
        std::hint::black_box(f);
    }
    Ok((pct(&mut ms.clone(), 0.5), pct(&mut ms, 0.9), v.width, v.height))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(movie), Some(proxy)) = (args.get(1), args.get(2)) else {
        return Err("usage: proxy_bench MOVIE PROXY.mov [--build] [--frames N] [--at S]".into());
    };
    let n = arg(&args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(120);
    let at = arg(&args, "--at").and_then(|v| v.parse().ok()).unwrap_or(30.0);
    let out = std::path::Path::new(proxy);
    if args.iter().any(|a| a == "--build") || !out.exists() {
        let t0 = Instant::now();
        let stop = AtomicBool::new(false);
        let mut tenth = 0;
        let b = effectcraft_media::proxy_build::build(movie, out, 2, &stop, &mut |d, t| {
            if d * 10 / t.max(1) > tenth {
                tenth = d * 10 / t.max(1);
                eprintln!("{}% ({d}/{t}) {:.0} fps", tenth * 10, d as f64 / t0.elapsed().as_secs_f64());
            }
        })
        .map_err(|e| e.to_string())?;
        println!("built {}x{} {} frames, {:.2} GB in {:.0} s", b.width, b.height, b.frames, b.bytes as f64 / 1e9, t0.elapsed().as_secs_f64());
    }
    let only = arg(&args, "--only");
    for (name, path) in [("original", movie.as_str()), ("proxy", proxy.as_str())] {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let (p50, p90, w, h) = decode(path, at, n)?;
        println!("{name:8} {w}x{h}: decode p50 {p50:.1} ms, p90 {p90:.1} ms per frame ({n} frames from {at} s)");
    }
    Ok(())
}
