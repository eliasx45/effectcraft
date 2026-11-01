//! Fix 3: per-frame CPU time and wall time of a preview read of hardware-decoded ProRes footage
//! into the GPU compositor's native YUV decode, through the media pool as the app reads it
//! (decode-ahead off, so all the work is on the reading thread and the threads it starts):
//! frames copied out of VideoToolbox at decode (before: the copy, at 8 bits also the CPU rounding,
//! then the GPU reads Fix 2's kept surface) against frames left in the surface (now). Full depth
//! and Decode Footage at 8 Bits, runs alternated; frames left in the surface read directly by the
//! decode (Fix 3c) or split into planes first. CPU time is the whole process's (`ps`), wall time
//! includes waiting for the GPU.
//!
//! `cargo run -p effectcraft-platform --example resident_bench -- <file.mov> [frames] [rounds]`

#[cfg(target_os = "macos")]
fn main() {
    use effectcraft_media::MediaPool;
    use effectcraft_project::{EightBit, ItemId};
    use effectcraft_render::FootageSource;
    use effectcraft_time::Tick;
    use std::time::Instant;

    /// This process's CPU time so far (user + system, all threads), seconds.
    fn cpu_seconds() -> f64 {
        let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output().expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        text.trim().split(':').fold(0.0, |acc, part| acc * 60.0 + part.trim().parse::<f64>().unwrap_or(0.0))
    }

    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: resident_bench <file.mov> [frames] [rounds]");
    let frames: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let rounds: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    assert!(matches!(effectcraft_platform::register(), effectcraft_platform::Availability::Available(_)), "no hardware decoder");
    let gpu = effectcraft_gpu::Gpu::headless().expect("GPU");
    let probe = effectcraft_gpu::surface::SurfaceKey { luma: 1, width: 2, height: 2, bits: 12, subsampling: [0, 0], alpha: false };
    let _ = effectcraft_platform::surfaces::import(gpu.device(), probe);
    let footage = effectcraft_media::probe(path).expect("probe");
    let n = frames.min(u32::try_from(footage.frame_rate.frame_at(footage.duration)).unwrap_or(u32::MAX)).max(2);
    let at = |i: u32| footage.frame_rate.tick_of(i64::from(i)) + Tick(footage.frame_rate.frame_duration().0 / 2);
    println!("{} {}x{} ({}), {n} frames a run, {rounds} rounds", path, footage.width, footage.height, footage.codec);

    // (mode name, eight-bit, resident, direct read) -> (per-frame wall ms, per-frame CPU ms, frames from the surface)
    let modes = [
        ("full depth, copied at decode (before)", EightBit::Off, false, false),
        ("full depth, kept on the GPU, split pass", EightBit::Off, true, false),
        ("full depth, kept on the GPU, read directly", EightBit::Off, true, true),
        ("8 bits, copied + rounded on the CPU (before)", EightBit::All, false, false),
        ("8 bits, kept on the GPU, split pass", EightBit::All, true, false),
        ("8 bits, kept on the GPU, read directly", EightBit::All, true, true),
    ];
    let mut wall: Vec<Vec<f64>> = vec![Vec::new(); modes.len()];
    let mut cpu: Vec<Vec<f64>> = vec![Vec::new(); modes.len()];
    let mut read: Vec<usize> = vec![0; modes.len()];
    for _ in 0..rounds {
        for (m, (_, eight, resident, direct)) in modes.iter().enumerate() {
            effectcraft_gpu::surface::set_direct(*direct);
            let pool = MediaPool::new();
            pool.set_hardware_decoding(true);
            pool.set_eight_bit(*eight);
            pool.set_read_ahead(false);
            pool.set_resident_frames(*resident);
            // (opened and the session made before timing)
            let _ = pool.frame_pixels(ItemId(1), &footage, at(0));
            let c0 = cpu_seconds();
            for i in 1..n {
                let s = Instant::now();
                let px = pool.frame_pixels(ItemId(1), &footage, at(i)).expect("frame");
                let view = px.gpu_yuv().expect("native view");
                let (from_surface, _) = gpu.decode_yuv(view, true, false).expect("GPU decode");
                read[m] += usize::from(from_surface);
                wall[m].push(s.elapsed().as_secs_f64() * 1e3);
            }
            cpu[m].push((cpu_seconds() - c0) * 1e3 / f64::from(n - 1));
            let st = pool.stats();
            assert!(!*resident || st.copies == 0, "a GPU-only read made {} CPU copies", st.copies);
        }
    }
    effectcraft_gpu::surface::set_direct(true);
    for (m, (name, ..)) in modes.iter().enumerate() {
        let w = &mut wall[m];
        let mean = w.iter().sum::<f64>() / w.len().max(1) as f64;
        w.sort_by(f64::total_cmp);
        let median = w.get(w.len() / 2).copied().unwrap_or(f64::NAN);
        let c = cpu[m].iter().sum::<f64>() / cpu[m].len().max(1) as f64;
        println!("{name:46} wall median {median:6.2} ms  mean {mean:6.2} ms   CPU {c:6.2} ms/frame   ({} of {} read from the surface)", read[m], w.len());
    }

    // The GPU decode alone (Fix 3c): one frame kept on the GPU, read from its surface again and
    // again, directly or through the split pass (wall time, waiting for the GPU each time).
    for eight in [EightBit::Off, EightBit::All] {
        let pool = MediaPool::new();
        pool.set_hardware_decoding(true);
        pool.set_eight_bit(eight);
        pool.set_read_ahead(false);
        let px = pool.frame_pixels(ItemId(1), &footage, at(1)).expect("frame");
        let Some(view) = px.gpu_yuv().filter(|v| v.surface.is_some()) else { continue };
        let mut t: [Vec<f64>; 2] = Default::default();
        for _ in 0..rounds.max(1) * 40 {
            for (k, direct) in [false, true].into_iter().enumerate() {
                effectcraft_gpu::surface::set_direct(direct);
                let s = Instant::now();
                let (from_surface, _) = gpu.decode_yuv(view, true, false).expect("GPU decode");
                t[k].push(s.elapsed().as_secs_f64() * 1e3);
                assert!(from_surface);
            }
        }
        effectcraft_gpu::surface::set_direct(true);
        for (k, name) in ["split pass", "read directly"].iter().enumerate() {
            let v = &mut t[k];
            v.sort_by(f64::total_cmp);
            let median = v.get(v.len() / 2).copied().unwrap_or(f64::NAN);
            let p10 = v.get(v.len() / 10).copied().unwrap_or(f64::NAN);
            println!("GPU decode of a kept frame ({eight:?}), {name:14} median {median:6.2} ms  p10 {p10:6.2} ms  ({} runs)", v.len());
        }
    }

    // CPU copies made on demand (a CPU reader of a frame kept on the GPU): into fresh pages (every
    // copy kept, as nothing is dropped) against recycled buffers (each copy dropped before the
    // next: the cache's steady state). A pool per frame: a CPU read makes a movie's next frames
    // copy at decode for a while.
    for eight in [EightBit::Off, EightBit::All] {
        let mut t: [Vec<f64>; 2] = Default::default();
        for _ in 0..rounds {
            for (k, recycle) in [false, true].into_iter().enumerate() {
                let mut held = Vec::new();
                for i in 1..n.min(40) {
                    let pool = MediaPool::new();
                    pool.set_hardware_decoding(true);
                    pool.set_eight_bit(eight);
                    pool.set_read_ahead(false);
                    let px = pool.frame_pixels(ItemId(1), &footage, at(i)).expect("frame");
                    if px.gpu_yuv().is_none_or(|v| v.surface.is_none()) {
                        continue;
                    }
                    let s = Instant::now();
                    let v = px.yuv().map(|v| v.planes[0].samples.len());
                    t[k].push(s.elapsed().as_secs_f64() * 1e3);
                    assert!(v.is_some(), "copied");
                    if !recycle {
                        held.push((px, pool));
                    }
                }
            }
        }
        for (k, name) in ["into fresh pages", "into recycled buffers"].iter().enumerate() {
            let v = &mut t[k];
            v.sort_by(f64::total_cmp);
            let median = v.get(v.len() / 2).copied().unwrap_or(f64::NAN);
            println!("CPU copy on demand ({eight:?}), {name:22} median {median:6.2} ms  ({} copies)", v.len());
        }
    }
    let (kept, copied) = effectcraft_platform::resident::counters();
    println!("pictures kept on the GPU at decode: {kept}, copied out on demand: {copied}");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("needs macOS");
}
