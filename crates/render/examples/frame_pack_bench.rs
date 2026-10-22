//! `cargo run --release -p effectcraft-render --example frame_pack_bench -- [--from <disk cache folder>]
//! [--frames N] [--reps N]`
//!
//! Compressed RAM preview frames benchmark (`plan/memory/compressed-cache.md`): compresses
//! 1080p frames and 4K frames (the same pixels tiled 2 × 2, as in
//! `plan/memory/cache-prototypes.md`) into strips ([`PackedFrame`]) and prints per size the
//! median encode time on one core, the median decode time on 1, 4 and 16 threads, the
//! compressed size and the frames 1 GB holds compressed and uncompressed.
//!
//! `--from` reads real frames from a disk cache folder (on macOS
//! `~/Library/Caches/EffectCraft/Disk Cache`; any frame size, the 1080p ones are used, older and
//! newer layouts alike). Without it, or when it holds no 1080p frames, synthetic frames with
//! flat areas, gradients and noise stand in (they compress differently from real ones).

use std::path::{Path, PathBuf};
use std::time::Instant;

use effectcraft_render::disk_cache::{self, Frame8};
use effectcraft_render::frame_pack::PackedFrame;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// Every `.ecc` file under `dir` (the disk cache's `v1/frames/<2 hex>/` folders).
fn entries(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            entries(&p, out);
        } else if p.extension().is_some_and(|x| x == "ecc") {
            out.push(p);
        }
    }
}

/// Up to `n` real 1080p frames from a disk cache folder.
fn real_frames(folder: &Path, n: usize) -> Vec<Frame8> {
    let mut files = vec![];
    entries(&folder.join("v1").join("frames"), &mut files);
    files.sort();
    let mut out = vec![];
    for f in files {
        if out.len() >= n {
            break;
        }
        let Ok(bytes) = std::fs::read(&f) else { continue };
        if let Some(frame) = disk_cache::read_frame_entry(&bytes)
            && frame.width == 1920
            && frame.height == 1080
        {
            out.push(frame);
        }
    }
    out
}

/// A synthetic 1080p frame: a gradient background, flat cards, a soft edge and some grain.
fn synthetic(seed: u32) -> Frame8 {
    let (w, h) = (1920u32, 1080u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let card = (x / 240 + y / 180 + seed) % 5 == 0;
            let grain = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed.wrapping_mul(83_492_791)) >> 29;
            let (r, g, b) = if card { (236, 240, 248) } else { ((x / 8) as u8, (y / 5) as u8, (60 + grain) as u8) };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Frame8 { width: w, height: h, rgba }
}

/// The 1080p frame tiled 2 × 2 into a 4K frame.
fn tile_4k(f: &Frame8) -> Frame8 {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut rgba = Vec::with_capacity(w * h * 16);
    for _ in 0..2 {
        for y in 0..h {
            let row = &f.rgba[y * w * 4..(y + 1) * w * 4];
            rgba.extend_from_slice(row);
            rgba.extend_from_slice(row);
        }
    }
    Frame8 { width: f.width * 2, height: f.height * 2, rgba }
}

fn bench(label: &str, frames: &[Frame8], reps: usize) {
    let mut enc = vec![];
    let mut dec: [Vec<f64>; 3] = [vec![], vec![], vec![]];
    let (mut packed_bytes, mut raw_bytes) = (0usize, 0usize);
    for f in frames {
        let mut packed = None;
        for _ in 0..reps {
            let t = Instant::now();
            packed = PackedFrame::encode(f.width, f.height, &f.rgba);
            enc.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let Some(p) = packed else {
            eprintln!("{label}: a frame did not compress");
            continue;
        };
        packed_bytes += p.bytes();
        raw_bytes += f.rgba.len();
        let mut out = vec![0u8; p.raw_len()];
        for (i, threads) in [1usize, 4, 16].into_iter().enumerate() {
            for _ in 0..reps {
                let t = Instant::now();
                let ok = p.decode_into(&mut out, threads);
                dec[i].push(t.elapsed().as_secs_f64() * 1000.0);
                if !ok || out != f.rgba {
                    eprintln!("{label}: a frame did not decode exactly");
                }
            }
        }
    }
    let n = frames.len().max(1);
    let per = packed_bytes as f64 / n as f64;
    let raw = raw_bytes as f64 / n as f64;
    let gb = (1u64 << 30) as f64;
    let [d1, d4, d16] = dec;
    println!(
        "{label:>6}: {:>6.2} MB/frame (raw {:.2} MB, {:.2}x) | encode {:>6.2} ms | decode {:>6.2} / {:>5.2} / {:>5.2} ms (1/4/16 threads) | per GB: {:>4.0} frames compressed, {:>4.0} raw",
        per / 1e6,
        raw / 1e6,
        raw / per.max(1.0),
        median(enc),
        median(d1),
        median(d4),
        median(d16),
        gb / per.max(1.0),
        gb / raw.max(1.0),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let n: usize = arg("--frames").and_then(|v| v.parse().ok()).unwrap_or(20);
    let reps: usize = arg("--reps").and_then(|v| v.parse().ok()).unwrap_or(3).max(1);
    let mut frames = arg("--from").map(|d| real_frames(Path::new(&d), n)).unwrap_or_default();
    let source = if frames.is_empty() {
        frames = (0..n.min(8) as u32).map(synthetic).collect();
        "synthetic"
    } else {
        "real"
    };
    println!("{} {source} 1080p frames, {reps} reps, {} strips per frame", frames.len(), effectcraft_render::frame_pack::STRIPS);
    bench("1080p", &frames, reps);
    let big: Vec<Frame8> = frames.iter().take(8).map(tile_4k).collect();
    bench("4K", &big, reps);
}
