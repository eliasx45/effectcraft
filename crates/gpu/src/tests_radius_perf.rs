//! Radius-scaling timings: effects whose per-pixel cost grew with their radius / spread /
//! samples (Median, Minimax, Simple Choker, Camera Lens Blur, 3D bokeh, Shadow Studio,
//! Bilateral and Smart Blur), on a 1920 × 1080 layer, CPU and GPU.
//!
//! `cargo test -p effectcraft-gpu --profile local --lib radius_perf -- --ignored --nocapture`
//! `RADIUS_PERF_OUT=dir` saves every output (raw f32); `RADIUS_PERF_REF=dir` compares against a
//! saved run (max difference and pixels over 1/255), so a faster algorithm can be checked
//! against the one it replaced.

use effectcraft_effects::{Buf, EffectCtx, Params};
use effectcraft_keyframe::Value;
use effectcraft_raster::Image;

use crate::tests::n;

const W: u32 = 1920;
const H: u32 = 1080;

/// Noise colours inside a soft-edged 1400 × 800 rounded rectangle, transparent outside.
fn input() -> Image {
    let mut img = Image::new(W, H);
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (((x as f64 + 0.5) - 960.0).abs() - 620.0, ((y as f64 + 0.5) - 540.0).abs() - 320.0);
            let d = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - 80.0;
            let a = (0.5 - d / 3.0).clamp(0.0, 1.0) as f32;
            let v = effectcraft_raster::hash_noise(x, y, 7);
            let u = effectcraft_raster::hash_noise(x / 9, y / 9, 3);
            let c = [v * 0.6 + u * 0.4, u, (v + u) * 0.5];
            img.set(x, y, [c[0] * a, c[1] * a, c[2] * a, a]);
        }
    }
    img
}

fn params(spec: &effectcraft_effects::EffectSpec, vals: &[(&str, Value)]) -> Params {
    let size = [W as f64, H as f64];
    let mut p = Params { values: spec.params.iter().map(|ps| (ps.id.to_string(), effectcraft_effects::default_value(ps, size))).collect() };
    for (k, v) in vals {
        p.values.insert(k.to_string(), v.clone());
    }
    p
}

/// Best of up to three runs (one when a run takes over two seconds), in ms.
fn time<T>(mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::INFINITY;
    let mut out = None;
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let o = f();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        best = best.min(ms);
        out = Some(o);
        if ms > 2000.0 {
            break;
        }
    }
    (best, out.expect("ran"))
}

fn raw(img: &Image) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + img.data.len() * 16);
    v.extend_from_slice(&img.width.to_le_bytes());
    v.extend_from_slice(&img.height.to_le_bytes());
    for p in &img.data {
        for c in p {
            v.extend_from_slice(&c.to_le_bytes());
        }
    }
    v
}

fn unraw(b: &[u8]) -> Option<Image> {
    let w = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?);
    let h = u32::from_le_bytes(b.get(4..8)?.try_into().ok()?);
    let mut img = Image::new(w, h);
    for (i, p) in img.data.iter_mut().enumerate() {
        for (c, v) in p.iter_mut().enumerate() {
            let o = 8 + (i * 4 + c) * 4;
            *v = f32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?);
        }
    }
    Some(img)
}

/// Save `img` as `name` and compare it with the reference run's, as a short note.
fn record(name: &str, img: &Image) -> String {
    if let Ok(dir) = std::env::var("RADIUS_PERF_OUT") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{name}.f32")), raw(img));
    }
    let Ok(dir) = std::env::var("RADIUS_PERF_REF") else { return String::new() };
    let Some(r) = std::fs::read(std::path::Path::new(&dir).join(format!("{name}.f32"))).ok().and_then(|b| unraw(&b)) else {
        return " ref: missing".into();
    };
    if (r.width, r.height) != (img.width, img.height) {
        return format!(" ref: size {}x{} vs {}x{}", r.width, r.height, img.width, img.height);
    }
    let d = crate::tests::diff(&r, img, 1.0 / 255.0);
    format!(" ref: max {:.5}, over 1/255 {} of {}", d.max, d.over, d.total)
}

fn case(id: &str, label: &str, vals: &[(&str, Value)], src: &Image) {
    let spec = effectcraft_effects::find(id).expect("effect");
    let p = params(spec, vals);
    let ctx = || EffectCtx { params: &p, time: 0.0, layer_size: [W as f64, H as f64], seed: 5, adjustment: false, env: Default::default() };
    let buf = Buf { img: src.clone(), offset: [0.0, 0.0], scale: 1.0 };
    let (cpu_ms, cpu) = time(|| (spec.render)(&ctx(), buf.clone()));
    let cpu_note = record(&format!("{label}-cpu"), &cpu.img);
    let gpu = crate::tests::gpu().filter(|_| spec.gpu).and_then(|g| {
        // Warm-up (pipeline compilation), then timed (wall clock: upload, kernels, readback).
        effectcraft_render::Accelerator::effects(g, &[effectcraft_render::FxStep { spec, ctx: ctx() }], &buf, None)?;
        let (ms, out) = time(|| effectcraft_render::Accelerator::effects(g, &[effectcraft_render::FxStep { spec, ctx: ctx() }], &buf, None));
        Some((ms, out?))
    });
    let gpu_note = match &gpu {
        Some((ms, out)) => format!("gpu {ms:9.1} ms{}", record(&format!("{label}-gpu"), &out.img)),
        None => "gpu       n/a".into(),
    };
    eprintln!("RADIUS_PERF {label:28} cpu {cpu_ms:9.1} ms{cpu_note} | {gpu_note}");
}

#[test]
#[ignore = "perf: cargo test -p effectcraft-gpu --profile local --lib radius_perf -- --ignored --nocapture"]
fn radius_perf() {
    let src = input();
    let e = Value::Enum;
    for r in [10.0, 50.0, 150.0] {
        case("ec.noise.median", &format!("median-r{r}"), &[("radius", n(r))], &src);
    }
    for r in [20.0, 100.0, 400.0] {
        case("ec.channel.minimax", &format!("minimax-r{r}"), &[("radius", n(r))], &src);
    }
    for c in [20.0, 80.0, -80.0] {
        case("ec.matte.simplechoker", &format!("simplechoker-{c}"), &[("chokeMatte", n(c))], &src);
    }
    for r in [30.0, 100.0, 300.0] {
        // Hexagon iris (Iris Shape index 3).
        case("ec.blur.cameralens", &format!("cameralens-r{r}"), &[("blurRadius", n(r)), ("irisProperties/irisShape", e(3))], &src);
    }
    case(
        "ec.blur.cameralens",
        "cameralens-fringe-r100",
        &[("blurRadius", n(100.0)), ("irisProperties/irisShape", e(3)), ("irisProperties/diffractionFringe", n(100.0))],
        &src,
    );
    for s in [36.0, 512.0] {
        case("ec.perspective.shadowstudio2", &format!("shadowstudio-s{s}"), &[("PESS2-0004", n(600.0)), ("PESS2-0022", n(s))], &src);
    }
    for r in [5.0, 30.0, 150.0] {
        case("ec.blur.bilateral", &format!("bilateral-r{r}"), &[("radius", n(r))], &src);
    }
    for r in [5.0, 30.0, 90.0] {
        case("ec.blur.smart", &format!("smartblur-r{r}"), &[("radius", n(r))], &src);
    }
    // Classic 3D depth of field (hexagon iris), CPU.
    let iris = effectcraft_render::three_d::bokeh::Iris { sides: 6, ..Default::default() };
    for r in [30.0, 100.0, 300.0] {
        let (ms, out) = time(|| effectcraft_render::three_d::bokeh::bokeh_blur(&src, &iris, r));
        let note = record(&format!("bokeh3d-r{r}-cpu"), &out);
        eprintln!("RADIUS_PERF {:28} cpu {ms:9.1} ms{note} | gpu       n/a", format!("bokeh3d-r{r}"));
    }
}
