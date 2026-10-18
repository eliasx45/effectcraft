//! Effects reading 8 bpc codes (`effects::apply_px`): an effect layer whose source is codes (a
//! precomp) and an adjustment layer over a codes canvas render bit for bit as with f32 working
//! images, without decoding the codes for the effects.

use effectcraft_color::Label;
use effectcraft_keyframe::Value;
use effectcraft_project::build;
use effectcraft_project::{BitDepth, Comp, ItemKind, LayerSource};
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::Gpu;
use crate::context::GpuContext;
use crate::tests::{Pattern, Scene, c, check, compare_at, n, opts, set};

type Stack<'a> = &'a [(&'a str, &'a [(&'a str, Value)])];

/// A precomp layer (its nested comp ends as codes) carrying `stack` over a background; or an
/// adjustment layer carrying it over footage (the canvas below is codes).
fn scene(stack: Stack, adjustment: bool) -> Scene {
    let mut s = Scene::new(BitDepth::Bpc8);
    let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
    s.push(bg);
    if adjustment {
        let mut l = s.footage(60, 40);
        set(&mut l, "transform/rotation", Value::Scalar(12.0));
        s.push(l);
        let mut adj = s.solid([1.0, 1.0, 1.0], 80, 50);
        adj.switches.adjustment = true;
        set(&mut adj, "transform/opacity", Value::Scalar(80.0));
        for (id, vals) in stack {
            s.effect(&mut adj, id, vals);
        }
        s.push(adj);
        return s;
    }
    let inner = Comp::new(60, 40, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let inner_id = s.p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
    let mut a = s.footage(44, 30);
    set(&mut a, "transform/rotation", Value::Scalar(9.0));
    s.p.comp_mut(inner_id).unwrap().layers = vec![a].into();
    let mut pre = build::layer(&mut s.p, &s.comp, "Inner", LayerSource::Comp { item: inner_id }, (60, 40), None);
    for (id, vals) in stack {
        s.effect(&mut pre, id, vals);
    }
    set(&mut pre, "transform/rotation", Value::Scalar(7.0));
    s.push(pre);
    s
}

fn context() -> Option<Gpu> {
    crate::tests::gpu()?;
    pollster::block_on(GpuContext::request()).ok().map(Gpu::from_context)
}

/// Decodes of codes for effects (and passes without a codes kernel) since `before`.
fn effect_decodes(g: &Gpu, before: &[(String, u64)]) -> Vec<(String, u64)> {
    g.codes_sites()
        .into_iter()
        .filter(|(site, _)| site.starts_with("effects.rs") || site.starts_with("fx_tone.rs") || site.starts_with("no variant"))
        .filter_map(|(site, k)| {
            let was = before.iter().find(|(s, _)| *s == site).map_or(0, |(_, k)| *k);
            (k > was).then(|| (site, k - was))
        })
        .collect()
}

#[test]
fn effects_read_codes_bit_for_bit() {
    let Some(g) = context() else { return };
    if g.ctx.codes.is_none() {
        return;
    }
    let ranges: &[(&str, Value)] = &[("redsHue", n(30.0)), ("redsSaturation", n(-20.0)), ("hue", n(15.0))];
    let cases: &[(&str, Stack)] = &[
        ("gaussian", &[("ec.blur.gaussian", &[("blurriness", n(6.0))])]),
        ("gaussian repeat", &[("ec.blur.gaussian", &[("blurriness", n(4.0)), ("repeatEdge", Value::Bool(true))])]),
        ("gaussian horizontal", &[("ec.blur.gaussian", &[("blurriness", n(5.0)), ("dimensions", Value::Enum(1))])]),
        ("gaussian vertical", &[("ec.blur.gaussian", &[("blurriness", n(5.0)), ("dimensions", Value::Enum(2))])]),
        ("fast box", &[("ec.blur.fastbox", &[("radius", n(4.0)), ("iterations", n(2.0))])]),
        ("fast box vertical", &[("ec.blur.fastbox", &[("radius", n(3.0)), ("dimensions", Value::Enum(2))])]),
        ("drop shadow", &[("ec.perspective.dropshadow", &[("distance", n(6.0)), ("softness", n(5.0))])]),
        ("drop shadow hard", &[("ec.perspective.dropshadow", &[("distance", n(4.0)), ("softness", n(0.0))])]),
        ("drop shadow only", &[("ec.perspective.dropshadow", &[("shadowOnly", Value::Bool(true))])]),
        ("fill", &[("ec.generate.fill", &[("color", c(0.2, 0.6, 1.0)), ("opacity", n(70.0))])]),
        ("fill invert", &[("ec.generate.fill", &[("invert", Value::Bool(true))])]),
        ("ramp", &[("ec.generate.gradientramp", &[("start", Value::Vec2([5.0, 3.0])), ("end", Value::Vec2([60.0, 40.0])), ("blend", n(25.0))])]),
        ("ramp radial", &[("ec.generate.gradientramp", &[("shape", Value::Enum(1)), ("scatter", n(40.0))])]),
        ("hue/sat", &[("ec.color.huesaturation", &[("hue", n(40.0)), ("saturation", n(20.0))])]),
        ("hue/sat colorize", &[("ec.color.huesaturation", &[("colorize", Value::Bool(true)), ("colorizeHue", n(200.0))])]),
        ("hue/sat ranges", &[("ec.color.huesaturation", ranges)]),
        // A zero blur leaves the codes for the next effect; a second effect reads floats.
        ("no blur, fill", &[("ec.blur.gaussian", &[("blurriness", n(0.0))]), ("ec.generate.fill", &[("color", c(0.9, 0.3, 0.1))])]),
        ("blur, hue/sat", &[("ec.blur.gaussian", &[("blurriness", n(3.0))]), ("ec.color.huesaturation", &[("hue", n(90.0))])]),
        // An effect without a codes kernel first: the codes are decoded once.
        ("invert, blur", &[("ec.channel.invert", &[]), ("ec.blur.gaussian", &[("blurriness", n(3.0))])]),
    ];
    for (label, stack) in cases {
        for adjustment in [false, true] {
            let s = scene(stack, adjustment);
            let what = format!("{label} (adjustment {adjustment})");
            // Bit for bit against f32 working images (`compare_at`), and the CPU (Gradient Ramp
            // on an adjustment layer differs from the CPU with f32 working images too: not
            // checked here).
            let cpu = !(adjustment && label.starts_with("ramp"));
            for o in [opts(), RenderOpts { scale: 0.5, ..opts() }] {
                let d = compare_at(&s, o, Tick::ZERO);
                if cpu {
                    check(&format!("{what} scale {}", o.scale), d, 0.0);
                }
            }
            let before = g.codes_sites();
            let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
            r.accel = Some(&g);
            g.render(&r, s.cid, Tick::ZERO).expect("the GPU renders the frame");
            let decodes = effect_decodes(&g, &before);
            if label.starts_with("invert") {
                assert!(!decodes.is_empty(), "{what}: Invert reads floats, so the codes are decoded");
            } else {
                assert!(decodes.is_empty(), "{what}: codes decoded for the effects: {decodes:?}");
            }
        }
    }
}
