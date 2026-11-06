//! GPU Deep Glow (`fx_deepglow`) vs the CPU effect (the oracle), both Glow Models. Deep Glow 2
//! (the default): radius, every Quality Preset and Custom, threshold, exposure, Add, gamma,
//! tint, chromatic aberration, aspect, Unmult off, Glow Input view, the Mask Layer. Legacy:
//! threshold, radius (including the reduced working resolution past 512 px), exposure, tint,
//! chromatic aberration, the display and linear models, Glow Input view. At 8 and 32 bpc.

use effectcraft_keyframe::Value;
use effectcraft_project::BitDepth;
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;

use crate::tests::{Scene, c, check, compare_at, effect_case, n, opts, set};

fn e(v: u32) -> Value {
    Value::Enum(v)
}

fn b(v: bool) -> Value {
    Value::Bool(v)
}

const ID: &str = "ec.stylize.deepglow";

/// Legacy (the earlier Glow Model).
const LEGACY: (&str, Value) = ("glowModel", Value::Enum(1));

#[test]
fn deep_glow_2_matches_cpu() {
    // Defaults (Medium, Screen, Unmult, Auto Detect Gamma) at a few radii.
    for r in [3.0, 20.0, 60.0, 150.0] {
        effect_case(ID, &[("PEDG-0001", n(r)), ("glowExposure", n(0.5))]);
    }
    // Every Quality Preset, and Custom (Resolution, Steps Multiplier).
    for q in 0..6 {
        effect_case(ID, &[("PEDG-0001", n(40.0)), ("PEDG2-0048", e(q))]);
    }
    effect_case(ID, &[("PEDG-0001", n(40.0)), ("PEDG2-0048", e(6)), ("PEDG2-0049", n(40.0)), ("PEDG2-0050", n(1.3))]);
    // Threshold, Add, exposure, manual gamma, Overlay tint, chromatic aberration, aspect, no Unmult.
    effect_case(
        ID,
        &[
            ("PEDG-0001", n(30.0)),
            ("PEDG-0004", n(40.0)),
            ("PEDG-0005", n(15.0)),
            ("PEDG-0008", e(1)),
            ("glowExposure", n(3.0)),
            ("PEDG-0043", b(false)),
            ("PEDG2-0154", n(60.0)),
            ("PEDG-0037", b(true)),
            ("PEDG-0039", e(1)),
            ("PEDG-0041", n(70.0)),
            ("PEDG-0013", b(true)),
            ("PEDG-0015", n(0.8)),
            ("PEDG-0010", n(0.6)),
            ("PEDG-0025", b(false)),
        ],
    );
    // Soft Light tint, source opacity, no edge expansion, gamma off.
    effect_case(
        ID,
        &[
            ("PEDG-0001", n(24.0)),
            ("PEDG-0037", b(true)),
            ("PEDG-0038", c(0.8, 0.3, 0.6)),
            ("PEDG-0039", e(2)),
            ("PEDG-0024", n(60.0)),
            ("PEDG-0055", n(0.0)),
            ("PEDG-0043", b(false)),
            ("PEDG2-0154", n(0.0)),
        ],
    );
    // Glow Input view.
    effect_case(ID, &[("PEDG-0001", n(16.0)), ("PEDG-0004", n(40.0)), ("PEDG-0023", e(0))]);
}

#[test]
fn deep_glow_matches_cpu() {
    // Linear model (Auto Detect Gamma), Screen, Unmult, Dither, Downsample 80 %.
    effect_case(ID, &[LEGACY, ("PEDG-0001", n(20.0)), ("PEDG-0004", n(30.0)), ("PEDG-0005", n(10.0))]);
    // Display model, Add, exposure, Overlay tint, chromatic aberration, aspect, no Unmult.
    effect_case(
        ID,
        &[
            LEGACY,
            ("PEDG-0001", n(14.0)),
            ("PEDG-0043", b(false)),
            ("PEDG-0008", e(1)),
            ("glowExposure", n(5.0)),
            ("PEDG-0037", b(true)),
            ("PEDG-0039", e(1)),
            ("PEDG-0041", n(70.0)),
            ("PEDG-0013", b(true)),
            ("PEDG-0015", n(0.8)),
            ("PEDG-0010", n(0.6)),
            ("PEDG-0025", b(false)),
            ("PEDG-0031", b(false)),
        ],
    );
    // Soft Light tint, spread, strong downsample, source opacity, monochrome dither, expansion.
    effect_case(
        ID,
        &[
            LEGACY,
            ("PEDG-0001", n(24.0)),
            ("PEDG-0004", n(55.0)),
            ("PEDG-0037", b(true)),
            ("PEDG-0038", c(0.8, 0.3, 0.6)),
            ("PEDG-0039", e(2)),
            ("PEDG-0011", n(70.0)),
            ("PEDG-0019", n(30.0)),
            ("PEDG-0024", n(60.0)),
            ("PEDG-0032", b(true)),
            ("PEDG-0055", n(2.0)),
            ("PEDG-0084", e(1)),
        ],
    );
    // Past 512 px: the reduced working resolution (no edge expansion, to stay small).
    effect_case(ID, &[LEGACY, ("PEDG-0001", n(1100.0)), ("PEDG-0055", n(0.0)), ("PEDG-0013", b(true))]);
    // A glow much narrower than its Radius pad (the linear model): the blurs and sums run on the
    // layer's rectangle grown by their reach, not the whole padded frame (Comp 28's blobs).
    effect_case(ID, &[LEGACY, ("PEDG-0001", n(200.0)), ("PEDG-0019", n(25.0))]);
    effect_case(
        ID,
        &[LEGACY, ("PEDG-0001", n(160.0)), ("PEDG-0013", b(true)), ("PEDG-0015", n(0.5)), ("PEDG-0010", n(0.6)), ("PEDG-0025", b(false)), ("PEDG-0084", e(1))],
    );
    // Glow Input view.
    effect_case(ID, &[LEGACY, ("PEDG-0001", n(16.0)), ("PEDG-0004", n(40.0)), ("PEDG-0023", e(0))]);
}

/// The Mask Layer (fetched through the effect host, fitted on the CPU), alpha and inverted luma,
/// both Glow Models.
#[test]
fn deep_glow_mask_layer_matches_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for (luma, invert, model) in [(0, false, 0), (1, true, 0), (0, false, 1), (1, true, 1)] {
            let mut s = Scene::new(depth);
            let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
            s.push(bg);
            let mask = s.footage(40, 30);
            let mid = s.push(mask);
            let mut l = s.footage(70, 44);
            s.effect(
                &mut l,
                ID,
                &[
                    ("glowModel", e(model)),
                    ("PEDG-0001", n(18.0)),
                    ("PEDG-0004", n(20.0)),
                    ("PEDG-0056", Value::Layer(Some(mid.0))),
                    ("PEDG-0057", e(luma)),
                    ("PEDG-0059", b(invert)),
                ],
            );
            set(&mut l, "transform/rotation", Value::Scalar(8.0));
            s.push(l);
            let label = format!("deep glow mask {luma} {invert} model {model} {depth:?}");
            check(&label, compare_at(&s, opts(), Tick::ZERO), 0.0);
            check(&format!("{label} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
        }
    }
}

/// The Comp 28 background blob's Deep Glow input: a comp-sized (1920 × 1080) shape layer with a
/// 1200 × 800 ellipse off centre, after Gaussian Blur 300, at layer `scale` (1 = Full, 0.5 = Half).
fn blob_input(scale: f64) -> (effectcraft_effects::Buf, [f64; 2]) {
    let size = [1920.0, 1080.0];
    let (w, h) = ((size[0] * scale) as u32, (size[1] * scale) as u32);
    let mut img = effectcraft_raster::Image::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (((x as f64 + 0.5) / scale - 570.0) / 1200.0, ((y as f64 + 0.5) / scale - 610.0) / 800.0);
            if u * u + v * v <= 0.25 {
                img.set(x, y, [0.0235 * 0.8, 0.1255 * 0.8, 0.2275 * 0.8, 0.8]);
            }
        }
    }
    let spec = effectcraft_effects::find("ec.blur.gaussian").unwrap();
    let params = params_for(spec, size, &[("blurriness", n(300.0))]);
    let ctx = effectcraft_effects::EffectCtx { params: &params, time: 0.0, layer_size: size, seed: 1, adjustment: false, env: Default::default() };
    ((spec.render)(&ctx, effectcraft_effects::Buf { img, offset: [0.0, 0.0], scale }), size)
}

fn params_for(spec: &effectcraft_effects::EffectSpec, size: [f64; 2], vals: &[(&str, Value)]) -> effectcraft_effects::Params {
    let mut params =
        effectcraft_effects::Params { values: spec.params.iter().map(|ps| (ps.id.to_string(), effectcraft_effects::default_value(ps, size))).collect() };
    for (k, v) in vals {
        params.values.insert(k.to_string(), v.clone());
    }
    params
}

/// The blob's Deep Glow settings (Radius 300, linear model, Unmult, Dither, Downsample 25 %).
fn blob_params(size: [f64; 2]) -> effectcraft_effects::Params {
    let spec = effectcraft_effects::find(ID).unwrap();
    params_for(spec, size, &[("PEDG-0001", n(300.0)), ("PEDG-0019", n(25.0)), ("PEDG-0011", n(33.0)), ("PEDG-0055", n(1.0))])
}

#[test]
#[ignore = "perf: cargo test -p effectcraft-gpu --lib deep_glow_blob_perf -- --ignored --nocapture"]
fn deep_glow_blob_perf() {
    let Some(g) = crate::tests::gpu() else { return };
    let spec = effectcraft_effects::find(ID).unwrap();
    for scale in [1.0, 0.5] {
        let (buf, size) = blob_input(scale);
        let params = blob_params(size);
        let ctx = || effectcraft_effects::EffectCtx { params: &params, time: 0.0, layer_size: size, seed: 11, adjustment: false, env: Default::default() };
        let t = std::time::Instant::now();
        let cpu = (spec.render)(&ctx(), buf.clone());
        let cpu_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut gpu_ms = Vec::new();
        let mut out = None;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            out = effectcraft_render::Accelerator::effects(g, &[effectcraft_render::FxStep { spec, ctx: ctx() }], &buf, None);
            gpu_ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let out = out.expect("GPU Deep Glow");
        // GPU kernel time (feature `profile`: timestamps, so other work on the machine's CPUs
        // doesn't count), median of 7 runs, without the tests' NaN fills of new scratch images.
        #[cfg(feature = "profile")]
        {
            let mut totals = Vec::new();
            for _ in 0..7 {
                effectcraft_render::profile::start();
                let _ = effectcraft_render::Accelerator::effects(g, &[effectcraft_render::FxStep { spec, ctx: ctx() }], &buf, None);
                let (events, _) = effectcraft_render::profile::finish();
                totals.push(events.iter().filter(|e| e.cat == "gpu" && e.name != "fill").map(|e| e.dur / 1e3).sum::<f64>());
            }
            totals.sort_by(f64::total_cmp);
            eprintln!("  gpu kernels (median of 7) {:.2} ms, all {totals:.2?}", totals[3]);
        }
        let d = crate::tests::diff(&cpu.img, &out.img, 1.0 / 255.0);
        eprintln!(
            "deep glow blob scale {scale}: in {}x{} out {}x{}; cpu {cpu_ms:.1} ms, gpu (incl. up/readback) {gpu_ms:.1?} ms; gpu vs cpu over 1/255: {} (max {})",
            buf.img.width, buf.img.height, out.img.width, out.img.height, d.over, d.max
        );
    }
}

/// A text-like layer: rows of glyph-sized white blocks in a `w × h` layer, filling the box
/// `[x0, x1) × [y0, y1)` (the sambranding-roi "$10,000 / $500/day" text: [620, 1296) × [446, 711)
/// of a comp-sized precomp).
#[cfg(feature = "profile")]
fn text_layer(w: u32, h: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> effectcraft_raster::Image {
    let mut img = effectcraft_raster::Image::new(w, h);
    for y in y0..y1.min(h) {
        for x in x0..x1.min(w) {
            let (gx, gy) = ((x - x0) % 96, (y - y0) % 140);
            if gx < 70 && gy < 110 && !(20..50).contains(&gx) || gy < 20 {
                img.set(x, y, [1.0; 4]);
            }
        }
    }
    img
}

/// GPU kernel time of Deep Glow, Legacy vs Deep Glow 2, f32 and half working images, ABBA
/// order, medians of `DG_PERF_RUNS` (24) runs each (feature `profile`: GPU timestamps), and
/// the half path's and the CPU's largest difference from the GPU f32 result (/255):
/// `cargo test -p effectcraft-gpu --features profile --lib deep_glow_model_perf -- --ignored --nocapture`.
#[test]
#[cfg(feature = "profile")]
#[ignore = "perf: needs --features profile"]
fn deep_glow_model_perf() {
    let Some(g) = crate::tests::gpu() else { return };
    let spec = effectcraft_effects::find(ID).unwrap();
    let runs: usize = std::env::var("DG_PERF_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(24);
    // (label, layer, radius, exposure)
    let cases: Vec<(&str, effectcraft_raster::Image, f64, f64)> = vec![
        ("sambranding text precomp 1920x1080 R400 E0.08", text_layer(1920, 1080, 620, 446, 1296, 711), 400.0, 0.08),
        ("text layer 1434x394 R116", text_layer(1434, 394, 0, 0, 1434, 394), 116.0, 0.04),
        ("text layer 1434x394 R150", text_layer(1434, 394, 0, 0, 1434, 394), 150.0, 0.04),
        ("text layer 1434x394 R282", text_layer(1434, 394, 0, 0, 1434, 394), 282.0, 0.04),
        ("full-frame layer 1920x1080 R116", text_layer(1920, 1080, 240, 300, 1680, 780), 116.0, 0.04),
        ("full-frame layer 1920x1080 R150", text_layer(1920, 1080, 240, 300, 1680, 780), 150.0, 0.04),
        ("full-frame layer 1920x1080 R282", text_layer(1920, 1080, 240, 300, 1680, 780), 282.0, 0.04),
    ];
    let run_once = |buf: &effectcraft_effects::Buf, params: &effectcraft_effects::Params, half: bool, size: [f64; 2]| -> (f64, effectcraft_raster::Image) {
        let ctx = effectcraft_effects::EffectCtx { params, time: 0.0, layer_size: size, seed: 11, adjustment: false, env: Default::default() };
        effectcraft_render::profile::start();
        let mut e = crate::context::Enc::new(&g.ctx);
        e.half = half;
        let out = crate::effects::run_chain(&mut e, &[effectcraft_render::FxStep { spec, ctx }], buf, None).expect("GPU Deep Glow").0;
        drop(e);
        let (events, _) = effectcraft_render::profile::finish();
        let ms = events.iter().filter(|e| e.cat == "gpu" && e.name != "fill").map(|e| e.dur / 1e3).sum::<f64>();
        if std::env::var("DG_PERF_BREAKDOWN").is_ok() {
            let mut by: std::collections::BTreeMap<String, (f64, usize)> = Default::default();
            for e in events.iter().filter(|e| e.cat == "gpu") {
                let v = by.entry(e.name.clone()).or_default();
                v.0 += e.dur / 1e3;
                v.1 += 1;
            }
            eprintln!("BREAKDOWN {by:.3?}");
        }
        (ms, out.img)
    };
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let d255 = |a: &effectcraft_raster::Image, b: &effectcraft_raster::Image| -> f32 {
        a.data.iter().zip(&b.data).flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs() / q[c].abs().max(1.0) * 255.0)).fold(0.0, f32::max)
    };
    for (label, img, radius, exposure) in cases {
        let size = [img.width as f64, img.height as f64];
        let buf = effectcraft_effects::Buf { img, offset: [0.0, 0.0], scale: 1.0 };
        let params = |model: u32| params_for(spec, size, &[("glowModel", e(model)), ("PEDG-0001", n(radius)), ("glowExposure", n(exposure))]);
        let (old, new) = (params(1), params(0));
        for half in [false, true] {
            let (mut t_old, mut t_new) = (Vec::new(), Vec::new());
            // Warm-up (pipelines, pools).
            let _ = run_once(&buf, &old, half, size);
            let _ = run_once(&buf, &new, half, size);
            for i in 0..runs {
                // ABBA: old, new, new, old, ...
                if i % 2 == 0 {
                    t_old.push(run_once(&buf, &old, half, size).0);
                    t_new.push(run_once(&buf, &new, half, size).0);
                } else {
                    t_new.push(run_once(&buf, &new, half, size).0);
                    t_old.push(run_once(&buf, &old, half, size).0);
                }
            }
            let (mo, mn) = (median(&mut t_old), median(&mut t_new));
            let f32_new = run_once(&buf, &new, false, size).1;
            let this_new = run_once(&buf, &new, half, size).1;
            eprintln!(
                "PERF {label} {}: legacy {mo:.3} ms, dg2 {mn:.3} ms (x{:.2} of legacy), dg2 vs dg2 f32 max {:.3}/255",
                if half { "half" } else { "f32" },
                mn / mo,
                d255(&this_new, &f32_new)
            );
        }
        let ctx = effectcraft_effects::EffectCtx { params: &new, time: 0.0, layer_size: size, seed: 11, adjustment: false, env: Default::default() };
        let t = std::time::Instant::now();
        let cpu = (spec.render)(&ctx, buf.clone());
        let cpu_ms = t.elapsed().as_secs_f64() * 1e3;
        let gpu = run_once(&buf, &new, false, size).1;
        eprintln!("PERF {label} cpu dg2 {cpu_ms:.1} ms, cpu vs gpu f32 max {:.4}/255", d255(&cpu.img, &gpu));
    }
}
