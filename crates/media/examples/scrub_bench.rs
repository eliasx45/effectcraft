//! `cargo run --release -p effectcraft-media --example scrub_bench -- MOVIE [--clicks N]
//! [--gap MS] [--layers K] [--rounds R] [--play N] [--workers W] [--only-playing]`: scrubs
//! MOVIE the way the viewer reads it while nothing plays (a random frame, then the two frames
//! after it prefetched on two threads, then `--gap` ms until the next click), at 8 bits, with K
//! footage layers (K paths of the same file: the file and hard links to it in a temporary
//! folder; each layer at its own random frame). Every round runs the scrub twice in the same
//! process: once with the pool told a preview plays (decode-ahead as before Q2: any read within
//! 32 frames of the last one decodes 32 frames ahead) and once told nothing plays (Q2). Prints
//! CPU seconds used, frames decoded ahead, and the click's latency (p50 / p90). `--play N` then
//! plays N frames on W frame workers (4), playing and paused (as Cache Frames When Idle reads;
//! not with `--only-playing`), and prints frames per second.

use std::time::{Duration, Instant};

use effectcraft_media::MediaPool;
use effectcraft_project::{EightBit, Footage, ItemId};
use effectcraft_render::FootageSource;
use effectcraft_time::Tick;

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
}

/// CPU seconds this process used so far (`ps`: user + system).
fn cpu_seconds() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output();
    let Ok(out) = out else { return 0.0 };
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().split(':').fold(0.0, |acc, part| acc * 60.0 + part.trim().parse::<f64>().unwrap_or(0.0))
}

/// A small deterministic random number generator (xorshift).
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: i64) -> i64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as i64
    }
}

fn at(f: &Footage, i: i64) -> Tick {
    f.frame_rate.tick_of(i) + Tick(f.frame_rate.frame_duration().0 / 2)
}

fn read(pool: &MediaPool, f: &Footage, i: i64) -> bool {
    pool.frame_pixels(ItemId(1), f, at(f, i)).is_some()
}

/// Waits until the pool decodes nothing more (its decode-ahead runners finished).
fn settle(pool: &MediaPool) {
    let mut last = u64::MAX;
    loop {
        std::thread::sleep(Duration::from_millis(150));
        let p = pool.stats().prefetched;
        if p == last {
            return;
        }
        last = p;
    }
}

struct Scrub {
    cpu: f64,
    ahead: u64,
    decoded: u64,
    p50: f64,
    p90: f64,
}

fn scrub(pool: &MediaPool, layers: &[Footage], clicks: usize, gap: Duration, seed: u64) -> Scrub {
    pool.purge();
    let before = pool.stats();
    let cpu0 = cpu_seconds();
    let mut rng = Rng(seed);
    let mut ms = Vec::with_capacity(clicks);
    let frames = layers.first().map_or(1, |f| f.frame_rate.frame_at(f.duration)).max(3);
    for _ in 0..clicks {
        let xs: Vec<i64> = layers.iter().map(|_| rng.below(frames - 2)).collect();
        // the viewer's frame: every layer's footage, on the frame workers
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for (f, &x) in layers.iter().zip(&xs) {
                s.spawn(move || read(pool, f, x));
            }
        });
        ms.push(t0.elapsed().as_secs_f64() * 1e3);
        // then the two frames after it, prefetched
        std::thread::scope(|s| {
            for k in 1..=2 {
                let xs = &xs;
                s.spawn(move || {
                    for (f, &x) in layers.iter().zip(xs) {
                        read(pool, f, x + k);
                    }
                });
            }
        });
        std::thread::sleep(gap);
    }
    settle(pool);
    let after = pool.stats();
    Scrub {
        cpu: cpu_seconds() - cpu0,
        ahead: after.prefetched - before.prefetched,
        decoded: (after.misses - before.misses) + (after.prefetched - before.prefetched),
        p50: pct(&mut ms.clone(), 0.5),
        p90: pct(&mut ms, 0.9),
    }
}

/// Plays `n` frames from frame 0 as fast as `workers` frame workers read them; frames per second.
fn play(pool: &MediaPool, layers: &[Footage], n: i64, workers: usize) -> f64 {
    pool.purge();
    let t0 = Instant::now();
    let next = std::sync::atomic::AtomicI64::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= n {
                        return;
                    }
                    for f in layers {
                        read(pool, f, i);
                    }
                }
            });
        }
    });
    n as f64 / t0.elapsed().as_secs_f64()
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let Some(movie) = args.get(1) else {
        return Err("usage: scrub_bench MOVIE [--clicks N] [--gap MS] [--layers K] [--rounds R] [--play N] [--workers W] [--only-playing]".into());
    };
    let clicks = arg(&args, "--clicks").and_then(|v| v.parse().ok()).unwrap_or(60);
    let gap = Duration::from_millis(arg(&args, "--gap").and_then(|v| v.parse().ok()).unwrap_or(60));
    let k: usize = arg(&args, "--layers").and_then(|v| v.parse().ok()).unwrap_or(1);
    let rounds: u64 = arg(&args, "--rounds").and_then(|v| v.parse().ok()).unwrap_or(2);
    let play_n: i64 = arg(&args, "--play").and_then(|v| v.parse().ok()).unwrap_or(0);
    let workers: usize = arg(&args, "--workers").and_then(|v| v.parse().ok()).unwrap_or(4);
    let only_playing = args.iter().any(|a| a == "--only-playing");

    // K paths of the same file: K movies to the pool
    let dir = std::env::temp_dir().join(format!("ec-scrub-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let src = std::fs::canonicalize(movie).map_err(|e| e.to_string())?;
    let mut layers = vec![];
    for i in 0..k {
        let path = match i {
            0 => src.clone(),
            _ => {
                let link = dir.join(format!("layer{i}.mov"));
                let _ = std::fs::remove_file(&link);
                std::fs::hard_link(&src, &link).map_err(|e| format!("{}: {e}", link.display()))?;
                link
            }
        };
        layers.push(effectcraft_media::probe(&path).map_err(|e| e.to_string())?);
    }
    let pool = MediaPool::new();
    pool.set_eight_bit(EightBit::All);
    let f = layers.first().ok_or("no layers")?;
    println!("{} ({}x{} {}), {k} layer(s), {clicks} clicks {} ms apart", movie, f.width, f.height, f.codec, gap.as_millis());
    for r in 0..rounds {
        for (name, playing) in [("before (decode-ahead as playing)", true), ("after  (Q2, nothing plays)    ", false)] {
            pool.set_playing(playing);
            let s = scrub(&pool, &layers, clicks, gap, 0x9e37_79b9 ^ r);
            println!("round {r} {name}: cpu {:6.2} s  decoded {:5} (ahead {:5})  click p50 {:6.1} ms  p90 {:6.1} ms", s.cpu, s.decoded, s.ahead, s.p50, s.p90);
        }
    }
    if play_n > 0 {
        for (name, playing) in [("playing", true), ("paused (idle cache)", false)] {
            pool.set_playing(playing);
            let ahead0 = pool.stats().prefetched;
            if only_playing && !playing {
                continue;
            }
            let fps = play(&pool, &layers, play_n, workers);
            println!("play {name}: {fps:.1} frames/s, decoded ahead {}", pool.stats().prefetched - ahead0);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
