//! GPU Deep Glow (`fx_deepglow`) vs the CPU effect (the oracle): threshold, radius (including
//! the reduced working resolution past 512 px), exposure, tint, chromatic aberration, the
//! display and linear models, Glow Input view, and the Mask Layer, at 8 and 32 bpc.

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

#[test]
fn deep_glow_matches_cpu() {
    // Linear model (Auto Detect Gamma), Screen, Unmult, Dither, Downsample 80 %.
    effect_case(ID, &[("PEDG-0001", n(20.0)), ("PEDG-0004", n(30.0)), ("PEDG-0005", n(10.0))]);
    // Display model, Add, exposure, Overlay tint, chromatic aberration, aspect, no Unmult.
    effect_case(
        ID,
        &[
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
    effect_case(ID, &[("PEDG-0001", n(1100.0)), ("PEDG-0055", n(0.0)), ("PEDG-0013", b(true))]);
    // A glow much narrower than its Radius pad (the linear model): the blurs and sums run on the
    // layer's rectangle grown by their reach, not the whole padded frame (Comp 28's blobs).
    effect_case(ID, &[("PEDG-0001", n(200.0)), ("PEDG-0019", n(25.0))]);
    effect_case(
        ID,
        &[("PEDG-0001", n(160.0)), ("PEDG-0013", b(true)), ("PEDG-0015", n(0.5)), ("PEDG-0010", n(0.6)), ("PEDG-0025", b(false)), ("PEDG-0084", e(1))],
    );
    // Glow Input view.
    effect_case(ID, &[("PEDG-0001", n(16.0)), ("PEDG-0004", n(40.0)), ("PEDG-0023", e(0))]);
}

/// The Mask Layer (fetched through the effect host, fitted on the CPU), alpha and inverted luma.
#[test]
fn deep_glow_mask_layer_matches_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for (luma, invert) in [(0, false), (1, true)] {
            let mut s = Scene::new(depth);
            let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
            s.push(bg);
            let mask = s.footage(40, 30);
            let mid = s.push(mask);
            let mut l = s.footage(70, 44);
            s.effect(
                &mut l,
                ID,
                &[("PEDG-0001", n(18.0)), ("PEDG-0004", n(20.0)), ("PEDG-0056", Value::Layer(Some(mid.0))), ("PEDG-0057", e(luma)), ("PEDG-0059", b(invert))],
            );
            set(&mut l, "transform/rotation", Value::Scalar(8.0));
            s.push(l);
            let label = format!("deep glow mask {luma} {invert} {depth:?}");
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
