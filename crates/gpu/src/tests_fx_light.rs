//! GPU CC light effects (CC Light Rays, CC Light Burst 2.5, CC Light Sweep, CC Light Wipe) vs
//! the CPU effects (the oracle): direct on a buffer at full and half resolution, as
//! adjustment, and composited at 8 and 32 bpc.

use effectcraft_keyframe::Value;

use crate::tests::{c, effect_case, n};

fn e(v: u32) -> Value {
    Value::Enum(v)
}

fn on() -> Value {
    Value::Bool(true)
}

fn pt(x: f64, y: f64) -> Value {
    Value::Vec2([x, y])
}

#[test]
fn light_rays() {
    effect_case("ec.generate.cclightrays", &[("center", pt(30.0, 20.0))]);
    effect_case(
        "ec.generate.cclightrays",
        &[("center", pt(40.0, 18.0)), ("intensity", n(250.0)), ("radius", n(15.0)), ("shape", e(1)), ("transferMode", e(1))],
    );
    effect_case(
        "ec.generate.cclightrays",
        &[("intensity", n(80.0)), ("colorFromSource", Value::Bool(false)), ("color", c(1.0, 0.6, 0.2)), ("transferMode", e(2)), ("warpSoftness", n(120.0))],
    );
    effect_case("ec.generate.cclightrays", &[("intensity", n(150.0)), ("transferMode", e(3)), ("radius", n(25.0))]);
}

#[test]
fn light_burst() {
    effect_case("ec.generate.cclightburst", &[("center", pt(30.0, 20.0)), ("burst", e(1))]);
    effect_case("ec.generate.cclightburst", &[("rayLength", n(80.0)), ("burst", e(2)), ("intensity", n(150.0))]);
    effect_case("ec.generate.cclightburst", &[("rayLength", n(30.0)), ("setColor", on()), ("color", c(0.3, 0.7, 1.0)), ("burst", e(1))]);
    effect_case("ec.generate.cclightburst", &[("rayLength", n(40.0))]);
}

#[test]
fn light_sweep() {
    effect_case("ec.generate.cclightsweep", &[("center", pt(30.0, 20.0))]);
    effect_case("ec.generate.cclightsweep", &[("shape", e(0)), ("width", n(30.0)), ("edgeThickness", n(4.0)), ("lightReceptionMode", e(1))]);
    effect_case("ec.generate.cclightsweep", &[("shape", e(2)), ("direction", n(60.0)), ("lightReceptionMode", e(2)), ("lightColor", c(1.0, 0.8, 0.4))]);
    effect_case("ec.generate.cclightsweep", &[("edgeThickness", n(0.0)), ("sweepIntensity", n(120.0))]);
}

#[test]
fn light_wipe() {
    effect_case("ec.transition.cclightwipe", &[("completion", n(40.0)), ("center", pt(30.0, 20.0))]);
    effect_case("ec.transition.cclightwipe", &[("completion", n(25.0)), ("shape", e(0)), ("direction", n(30.0)), ("colorFromSource", on())]);
    effect_case("ec.transition.cclightwipe", &[("completion", n(60.0)), ("shape", e(2)), ("reverse", on()), ("color", c(0.2, 0.9, 0.5))]);
}

/// CC Light Sweep on a comp-sized (1920 × 1080) card with 10-10 Edit's settings (Width 40,
/// Thickness 1): GPU kernel time (feature `profile`) vs the CPU effect (`cargo test -p
/// effectcraft-gpu --features profile --lib light_sweep_comp_perf -- --ignored --nocapture`).
#[test]
#[ignore = "perf"]
fn light_sweep_comp_perf() {
    let Some(g) = crate::tests::gpu() else { return };
    let spec = effectcraft_effects::find("ec.generate.cclightsweep").unwrap();
    let size = [1920.0, 1080.0];
    let mut params =
        effectcraft_effects::Params { values: spec.params.iter().map(|ps| (ps.id.to_string(), effectcraft_effects::default_value(ps, size))).collect() };
    for (k, v) in [("center", pt(960.0, 540.0)), ("width", n(40.0)), ("sweepIntensity", n(70.0)), ("edgeIntensity", n(60.0))] {
        params.values.insert(k.to_string(), v);
    }
    let mut img = effectcraft_raster::Image::new(1920, 1080);
    for y in 200..880 {
        for x in 300..1620 {
            img.set(x, y, [0.2, 0.3, 0.4, 1.0]);
        }
    }
    let buf = effectcraft_effects::Buf { img, offset: [0.0; 2], scale: 1.0 };
    let ctx = || effectcraft_effects::EffectCtx { params: &params, time: 0.0, layer_size: size, seed: 1, adjustment: false, env: Default::default() };
    let t = std::time::Instant::now();
    let cpu = (spec.render)(&ctx(), buf.clone());
    let cpu_ms = t.elapsed().as_secs_f64() * 1e3;
    let out = effectcraft_render::Accelerator::effects(g, &[effectcraft_render::FxStep { spec, ctx: ctx() }], &buf, None).expect("GPU Light Sweep");
    let d = crate::tests::diff(&cpu.img, &out.img, 1.0 / 255.0);
    assert_eq!(d.over, 0, "GPU vs CPU over 1/255 (max {})", d.max);
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
        eprintln!("CC Light Sweep 1920x1080: GPU kernels (median of 7) {:.2} ms, CPU {cpu_ms:.1} ms", totals[3]);
    }
    let _ = cpu_ms;
}
