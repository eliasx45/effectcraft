//! Fix 2: times getting hardware-decoded ProRes pictures into the GPU compositor's native YUV
//! decode: uploading the CPU planes (before) against reading VideoToolbox's surface (now), on the
//! same decoded frames, waiting for the GPU each time; at full depth, and at 8 bits (Decode
//! Footage at 8 Bits: the pool's rounding to 8 bits, the old per-sample one beside the new one,
//! then the 8-bit planes uploaded or the surface read). Also checks that the two give the same
//! picture on the first frame.
//!
//! `cargo run -p effectcraft-platform --example prores_surface_bench -- <file.mov> [frames]`
//! (`VERBOSE=1`: a line per frame)

#[cfg(target_os = "macos")]
fn main() {
    use effectcraft_platform::prores::VtProResDecoder;
    use effectcraft_platform::surfaces;
    use effectcraft_render::yuv::{ChromaSiting, Coefficients, Plane, Samples, View};
    use filmcraft_codecs::VideoDecoder;
    use filmcraft_frame::{Chroma, PixelData, VideoFrame};
    use rayon::prelude::*;
    use std::time::Instant;

    fn view(f: &VideoFrame) -> View<'_> {
        let (planes, chroma, bits, alpha) = match &f.data {
            PixelData::Yuv16 { planes, chroma, bits, alpha } => {
                (planes.each_ref().map(|p| Samples::U16(&p[..])), *chroma, *bits, alpha.as_ref().map(|a| Samples::U16(&a[..])))
            }
            PixelData::Yuv8 { planes, chroma, alpha } => {
                (planes.each_ref().map(|p| Samples::U8(&p[..])), *chroma, 8, alpha.as_ref().map(|a| Samples::U8(&a[..])))
            }
            _ => panic!("not planar Y'CbCr"),
        };
        let (sx, sy) = if chroma == Chroma::C444 { (0, 0) } else { (1, 0) };
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
        fn plane(samples: Samples<'_>, w: u32, h: u32) -> Plane<'_> {
            Plane { samples, width: w, height: h, stride: w as usize }
        }
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

    /// The media pool's rounding to 8 bits as it was before 1 Nov 2026 (a parallel iterator over
    /// samples through a `dyn Fn`; limited range), for comparison.
    fn old_to_8bit(f: &VideoFrame) -> usize {
        let PixelData::Yuv16 { planes, bits, alpha, .. } = &f.data else { return 0 };
        let max = (1u32 << bits) - 1;
        let shift = bits - 8;
        let half = (1u32 << shift) >> 1;
        let full = |v: u16| ((u32::from(v).min(max) * 255 + max / 2) / max).min(255) as u8;
        let limited = |v: u16| ((u32::from(v).min(max) + half) >> shift).min(255) as u8;
        let conv = |p: &[u16], f: &(dyn Fn(u16) -> u8 + Sync)| -> Vec<u8> { p.par_iter().map(|v| f(*v)).collect() };
        let out = [conv(&planes[0], &limited), conv(&planes[1], &limited), conv(&planes[2], &limited)];
        let a = alpha.as_ref().map(|a| conv(a, &full));
        std::hint::black_box(out.iter().map(Vec::len).sum::<usize>() + a.map_or(0, |a| a.len()))
    }

    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: prores_surface_bench <file.mov> [frames]");
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let verbose = std::env::var_os("VERBOSE").is_some();
    let file = std::fs::File::open(path).expect("open");
    let mp4 = filmcraft_isobmff::open(&file).expect("parse");
    let ti = mp4.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("video track");
    let track = &mp4.tracks[ti];
    let entry = &track.entries[0];
    let filmcraft_isobmff::CodecConfig::ProRes { fourcc } = &entry.codec else { panic!("not ProRes") };
    let v = entry.video.as_ref().expect("video params");
    let n = frames.min(track.samples.len());

    let gpu = effectcraft_gpu::Gpu::headless().expect("GPU");
    effectcraft_gpu::surface::set_surface_import(surfaces::import);
    effectcraft_media::hw::set_eight_bit_hook(surfaces::rounded_to_8bit);
    let probe = effectcraft_gpu::surface::SurfaceKey { luma: 1, width: 2, height: 2, bits: 12, subsampling: [0, 0], alpha: false };
    let _ = surfaces::import(gpu.device(), probe);
    let mut d = VtProResDecoder::new(fourcc.0, v.width.into(), v.height.into()).expect("hardware decoder");

    let names = [
        "hardware decode (incl. CPU copy)",
        "full depth: planes uploaded",
        "full depth: surface read",
        "8 bits: old CPU rounding",
        "8 bits: new CPU rounding",
        "8 bits: planes uploaded",
        "8 bits: surface read",
    ];
    let mut t: [Vec<f64>; 7] = Default::default();
    let (mut imported, mut imported8) = (0, 0);
    let ms = |s: Instant| s.elapsed().as_secs_f64() * 1e3;
    for i in 0..n {
        let sample = mp4.read_sample(&file, ti, i).expect("sample");
        let s = Instant::now();
        let frame = d.decode(&sample, i as i64).expect("decode").remove(0).frame;
        t[0].push(ms(s));
        let full = view(&frame);
        if i == 0 {
            let (_, a) = gpu.decode_yuv(full, true, true).expect("surface");
            let (_, b) = gpu.decode_yuv(full, false, true).expect("upload");
            let same = a.map(|a| a.data) == b.map(|b| b.data);
            println!("first frame: surface picture {} the uploaded one", if same { "identical to" } else { "DIFFERS from" });
        }
        // GPU decode of `v` uploaded and from the surface (the order alternates, so neither path
        // always runs on a warm GPU); whether the surface was read
        let pair = |v: View<'_>, up: usize, surf: usize, t: &mut [Vec<f64>; 7]| -> bool {
            let run = |surface: bool| {
                let s = Instant::now();
                let (from, _) = gpu.decode_yuv(v, surface, false).expect("decoded");
                (ms(s), from)
            };
            let (a, b) = if i % 2 == 0 {
                let a = run(false);
                (a, run(true))
            } else {
                let b = run(true);
                (run(false), b)
            };
            t[up].push(a.0);
            t[surf].push(b.0);
            b.1
        };
        imported += usize::from(pair(full, 1, 2, &mut t));
        let s = Instant::now();
        old_to_8bit(&frame);
        t[3].push(ms(s));
        let s = Instant::now();
        let eight = effectcraft_media::hw::round_to_8bit(&frame).expect("rounded to 8 bits");
        t[4].push(ms(s));
        imported8 += usize::from(pair(view(&eight), 5, 6, &mut t));
        if verbose {
            let row: Vec<String> = t.iter().map(|v| format!("{:.2}", v.last().copied().unwrap_or(f64::NAN))).collect();
            eprintln!("frame {i}: {} ms; kept {} ({} MB)", row.join(" / "), surfaces::kept(), surfaces::kept_bytes() >> 20);
        }
    }
    println!(
        "{} {}x{}, {n} frames ({imported} full-depth and {imported8} 8-bit read from the surface; kept {} surfaces, {:.0} MB)",
        String::from_utf8_lossy(&fourcc.0),
        v.width,
        v.height,
        surfaces::kept(),
        surfaces::kept_bytes() as f64 / 1e6
    );
    for (name, v) in names.iter().zip(t.iter_mut()) {
        let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
        v.sort_by(f64::total_cmp);
        let median = v.get(v.len() / 2).copied().unwrap_or(f64::NAN);
        println!("{name:34} median {median:6.2} ms  mean {mean:6.2} ms");
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("needs macOS");
}
