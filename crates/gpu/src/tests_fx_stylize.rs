//! GPU stylize and distort family effects vs the CPU effects (the oracle): direct on a buffer at full and half
//! resolution, as adjustment, and composited at 8 and 32 bpc.

use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::BitDepth;
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;

use crate::tests::{Scene, c, check, compare_at, effect_case, n, opts, set, v3};

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
fn posterize_thresholds_strobe_vignette() {
    effect_case("ec.stylize.posterize", &[("level", n(4.0))]);
    effect_case("ec.stylize.posterize", &[("level", n(9.0))]);
    effect_case("ec.stylize.threshold", &[("level", n(100.0))]);
    effect_case("ec.stylize.threshold", &[("level", n(181.0))]);
    effect_case("ec.stylize.ccthreshold", &[]);
    effect_case("ec.stylize.ccthreshold", &[("channel", e(1)), ("invert", on()), ("threshold", n(90.0))]);
    effect_case("ec.stylize.ccthreshold", &[("channel", e(2)), ("blend", n(30.0))]);
    effect_case("ec.stylize.ccthreshold", &[("channel", e(3)), ("threshold", n(60.0))]);
    effect_case(
        "ec.stylize.ccthresholdrgb",
        &[("redThreshold", n(70.0)), ("greenThreshold", n(150.0)), ("blueThreshold", n(110.0)), ("invertGreen", on()), ("blend", n(25.0))],
    );
    effect_case(
        "ec.stylize.strobe",
        &[("strobeDuration", n(0.5)), ("strobeOperator", e(1)), ("strobeColor", c(0.2, 0.5, 0.9)), ("blendWithOriginal", n(30.0))],
    );
    effect_case("ec.stylize.strobe", &[("strobeDuration", n(0.5)), ("strobeOperator", e(4)), ("strobeColor", c(0.6, 0.1, 0.3))]);
    effect_case("ec.stylize.strobe", &[("strobeDuration", n(0.5)), ("strobe", e(1)), ("blendWithOriginal", n(40.0))]);
    effect_case("ec.stylize.strobe", &[("strobeDuration", n(0.1)), ("strobePeriod", n(0.2)), ("randomStrobeProbability", n(50.0))]);
    effect_case("ec.stylize.ccvignette", &[("amount", n(80.0))]);
    effect_case("ec.stylize.ccvignette", &[("amount", n(-60.0)), ("pinHighlights", n(50.0)), ("angleOfView", n(120.0)), ("center", pt(20.0, 15.0))]);
}

#[test]
fn scatter_brush_strokes() {
    effect_case("ec.stylize.scatter", &[("amount", n(5.0))]);
    effect_case("ec.stylize.scatter", &[("amount", n(3.0)), ("grain", e(1))]);
    effect_case("ec.stylize.scatter", &[("amount", n(7.0)), ("grain", e(2)), ("randomizeEveryFrame", on())]);
    effect_case("ec.stylize.brushstrokes", &[]);
    effect_case(
        "ec.stylize.brushstrokes",
        &[("strokeAngle", n(40.0)), ("brushSize", n(3.0)), ("strokeLength", n(10.0)), ("paintSurface", e(2)), ("blendWithOriginal", n(20.0))],
    );
    effect_case("ec.stylize.brushstrokes", &[("paintSurface", e(1)), ("strokeDensity", n(0.6)), ("strokeRandomness", n(2.0)), ("brushSize", n(1.3))]);
    effect_case("ec.stylize.brushstrokes", &[("paintSurface", e(3)), ("strokeLength", n(25.0))]);
}

#[test]
fn roughen_edges() {
    effect_case("ec.stylize.roughenedges", &[]);
    effect_case("ec.stylize.roughenedges", &[("edgeType", e(1)), ("border", n(4.0)), ("edgeColor", c(0.1, 0.6, 0.9))]);
    effect_case(
        "ec.stylize.roughenedges",
        &[
            ("edgeType", e(5)),
            ("evolutionOptions/cycleEvolution", on()),
            ("evolutionOptions/cycle", n(2.0)),
            ("evolution", n(500.0)),
            ("stretch", n(2.0)),
            ("complexity", n(3.0)),
        ],
    );
    effect_case("ec.stylize.roughenedges", &[("edgeType", e(7)), ("border", n(0.0)), ("scale", n(60.0)), ("evolutionOptions/randomSeed", n(9.0))]);
}

#[test]
fn tiles_kaleida_repetile() {
    effect_case("ec.stylize.motiontile", &[("tileWidth", n(50.0)), ("tileHeight", n(40.0))]);
    effect_case(
        "ec.stylize.motiontile",
        &[("outputWidth", n(150.0)), ("outputHeight", n(130.0)), ("mirrorEdges", on()), ("phase", n(70.0)), ("tileWidth", n(45.0))],
    );
    effect_case(
        "ec.stylize.motiontile",
        &[("horizontalPhaseShift", on()), ("phase", n(33.0)), ("tileCenter", pt(30.0, 17.0)), ("tileHeight", n(55.0)), ("outputWidth", n(80.0))],
    );
    effect_case("ec.stylize.cckaleida", &[]);
    effect_case("ec.stylize.cckaleida", &[("mirroring", e(2)), ("rotation", n(20.0)), ("size", n(150.0))]);
    effect_case("ec.stylize.ccrepetile", &[("expandRight", n(10.0)), ("expandUp", n(4.0))]);
    effect_case("ec.stylize.ccrepetile", &[("expandLeft", n(12.0)), ("expandDown", n(9.0)), ("tiling", e(1))]);
    effect_case("ec.stylize.ccrepetile", &[("expandLeft", n(30.0)), ("expandRight", n(30.0)), ("expandUp", n(20.0)), ("tiling", e(4))]);
    effect_case("ec.distort.cctiler", &[]);
    effect_case("ec.distort.cctiler", &[("scale", n(70.0)), ("blendWithOriginal", n(30.0)), ("center", pt(30.0, 18.0))]);
    effect_case("ec.distort.ccgriddler", &[("rotation", n(20.0))]);
    effect_case("ec.distort.ccgriddler", &[("cutTiles", on()), ("horizontalScale", n(120.0)), ("verticalScale", n(80.0)), ("tileSize", n(17.0))]);
}

#[test]
fn mirror_offset_polar_spherize_corner_pin() {
    effect_case("ec.distort.mirror", &[("angle", n(30.0))]);
    effect_case("ec.distort.mirror", &[("angle", n(200.0)), ("center", pt(27.0, 19.0))]);
    effect_case("ec.distort.offset", &[("shift", pt(10.0, 5.0))]);
    effect_case("ec.distort.offset", &[("shift", pt(50.3, 20.7)), ("blend", n(40.0))]);
    effect_case("ec.distort.polar", &[("interpolation", n(100.0))]);
    effect_case("ec.distort.polar", &[("interpolation", n(60.0)), ("conversion", e(1))]);
    effect_case("ec.distort.spherize", &[("radius", n(20.0))]);
    effect_case("ec.distort.spherize", &[("radius", n(35.0)), ("center", pt(30.0, 25.0))]);
    effect_case("ec.distort.cornerpin", &[("ul", pt(5.0, 3.0)), ("ur", pt(60.0, 8.0)), ("lr", pt(66.0, 40.0)), ("ll", pt(2.0, 44.0))]);
    effect_case("ec.distort.cornerpin", &[("ul", pt(-8.0, 6.0)), ("ur", pt(75.0, -4.0)), ("lr", pt(50.0, 30.0)), ("ll", pt(12.0, 50.0))]);
}

#[test]
fn optics_magnify() {
    effect_case("ec.distort.opticscompensation", &[("fieldOfView", n(60.0))]);
    effect_case("ec.distort.opticscompensation", &[("fieldOfView", n(90.0)), ("reverseLensDistortion", on()), ("fovOrientation", e(2))]);
    effect_case(
        "ec.distort.opticscompensation",
        &[("fieldOfView", n(110.0)), ("reverseLensDistortion", on()), ("resize", e(1)), ("viewCenter", pt(30.0, 20.0))],
    );
    effect_case("ec.distort.opticscompensation", &[("fieldOfView", n(120.0)), ("optimalPixels", on()), ("fovOrientation", e(1))]);
    effect_case("ec.distort.magnify", &[("size", n(15.0))]);
    effect_case(
        "ec.distort.magnify",
        &[("shape", e(1)), ("feather", n(6.0)), ("size", n(18.0)), ("scaling", e(1)), ("blendingMode", e(3)), ("magnification", n(270.0))],
    );
    effect_case("ec.distort.magnify", &[("scaling", e(2)), ("blendingMode", e(0)), ("size", n(20.0)), ("opacity", n(70.0))]);
    effect_case("ec.distort.magnify", &[("size", n(25.0)), ("center", pt(60.0, 10.0)), ("resizeLayer", on()), ("blendingMode", e(5))]);
}

#[test]
fn cc_slant_smear_split() {
    effect_case("ec.distort.ccslant", &[("slant", n(30.0))]);
    effect_case("ec.distort.ccslant", &[("slant", n(-20.0)), ("stretching", on()), ("height", n(120.0))]);
    effect_case("ec.distort.ccsmear", &[]);
    effect_case("ec.distort.ccsmear", &[("reach", n(-40.0)), ("radius", n(20.0)), ("to", pt(40.0, 10.0))]);
    for reach in [-1000.0, -150.0, 100.0, 150.0, 200.0, 1000.0] {
        effect_case("ec.distort.ccsmear", &[("reach", n(reach)), ("radius", n(20.0)), ("from", pt(20.0, 20.0)), ("to", pt(40.0, 10.0))]);
    }
    effect_case("ec.distort.ccsplit", &[]);
    effect_case("ec.distort.ccsplit2", &[("split1", n(30.0)), ("split2", n(80.0)), ("pointB", pt(60.0, 30.0))]);
}

#[test]
fn twirl_legacy_ripple_pulse_power_pin() {
    effect_case("ec.distort.twirllegacy", &[("angle", n(90.0))]);
    effect_case("ec.distort.twirllegacy", &[("angle", n(-250.0)), ("radius", n(80.0)), ("center", pt(30.0, 18.0))]);
    effect_case("ec.distort.ccripplepulse", &[("pulseLevel", n(80.0)), ("timeSpan", n(0.5))]);
    effect_case("ec.distort.ccripplepulse", &[("pulseLevel", n(-60.0)), ("amplitude", n(150.0)), ("renderBump", on()), ("center", pt(25.0, 20.0))]);
    effect_case("ec.distort.ccpowerpin", &[("topLeft", pt(5.0, 3.0)), ("bottomRight", pt(62.0, 38.0))]);
    effect_case("ec.distort.ccpowerpin", &[("topRight", pt(80.0, -6.0)), ("bottomLeft", pt(-4.0, 50.0)), ("expandLeft", n(20.0)), ("expandBottom", n(-10.0))]);
    // Perspective below 100 %: blended toward the bilinear inverse.
    effect_case("ec.distort.ccpowerpin", &[("topRight", pt(80.0, -6.0)), ("bottomLeft", pt(-4.0, 50.0)), ("perspective", n(0.0))]);
    effect_case("ec.distort.ccpowerpin", &[("topLeft", pt(12.0, 6.0)), ("bottomRight", pt(75.0, 30.0)), ("perspective", n(40.0)), ("expandTop", n(15.0))]);
}

#[test]
fn flo_motion() {
    effect_case("ec.distort.ccflomotion", &[]);
    effect_case("ec.distort.ccflomotion", &[("antialiasing", e(2)), ("tileEdges", on()), ("amount1", n(-150.0)), ("falloff", n(0.7))]);
    effect_case("ec.distort.ccflomotion", &[("antialiasing", e(0)), ("amount2", n(0.0)), ("knot1", pt(20.0, 30.0))]);
}

#[test]
fn liquify_render() {
    let mesh = "warp 20 50 0 0 0 10,10 20,15 30,20\ntwirlClockwise 24 80 0 0 0 40,25 42,27";
    effect_case("ec.distort.liquify", &[("distortionMesh", Value::Str(mesh.into()))]);
    effect_case(
        "ec.distort.liquify",
        &[("distortionMesh", Value::Str(mesh.into())), ("distortionPercentage", n(60.0)), ("distortionMeshOffset", pt(3.0, -2.0))],
    );
}

#[test]
fn glow_operations_and_arbitrary_map() {
    effect_case("ec.stylize.glow", &[("glowOperation", e(2)), ("threshold", n(30.0)), ("radius", n(8.0))]);
    effect_case("ec.stylize.glow", &[("glowOperation", e(3)), ("colors", e(1)), ("intensity", n(2.0))]);
    effect_case("ec.stylize.glow", &[("colors", e(2)), ("arbitraryMap", Value::Str("0,0 0.5,0.8 1,1|0,0.2 1,0.6|".into())), ("threshold", n(20.0))]);
    effect_case("ec.stylize.glow", &[("colors", e(2)), ("arbitraryMap", Value::Str("0,1 1,0||0,0 1,1".into())), ("operation", e(1)), ("glowOperation", e(4))]);
}

/// Texturize with a texture layer (Tile / Center / Stretch), composited.
#[test]
fn texturize_layer() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for placement in 0..3 {
            let mut s = Scene::new(depth);
            let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
            s.push(bg);
            let mut tex = s.footage(50, 36);
            tex.switches.video = false;
            let tid = s.push(tex);
            let mut l = s.footage(70, 44);
            s.effect(
                &mut l,
                "ec.stylize.texturize",
                &[("textureLayer", Value::Layer(Some(tid.0))), ("texturePlacement", e(placement)), ("textureContrast", n(1.5)), ("lightDirection", n(60.0))],
            );
            set(&mut l, "transform/rotation", Value::Scalar(8.0));
            s.push(l);
            let label = format!("texturize {placement} {depth:?}");
            check(&label, compare_at(&s, opts(), Tick::ZERO), 0.0);
            check(&format!("{label} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
        }
    }
}

/// Transform's own shutter angle: the effect's rotation and position animate.
#[test]
fn transform_motion_blur() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut s = Scene::new(depth);
        let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
        s.push(bg);
        let mut l = s.footage(70, 44);
        s.effect(&mut l, "ec.distort.transform", &[("useCompositionShutterAngle", Value::Bool(false)), ("shutterAngle", n(180.0))]);
        let rot = l.props.prop_mut("effects/#1/rotation").expect("transform rotation");
        rot.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(0.0)), Keyframe::new(Tick::from_seconds_f64(1.0), Value::Scalar(120.0))];
        let pos = l.props.prop_mut("effects/#1/position").expect("transform position");
        pos.keys = vec![Keyframe::new(Tick::ZERO, pt(35.0, 22.0)), Keyframe::new(Tick::from_seconds_f64(1.0), pt(95.0, 40.0))];
        s.push(l);
        let label = format!("transform motion blur {depth:?}");
        check(&label, compare_at(&s, opts(), Tick::from_seconds_f64(0.3)), 0.0);
        check(&format!("{label} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::from_seconds_f64(0.3)), 0.0);
    }
}

/// A fast move that carries the layer past its own buffer (a 720° shutter across the frame):
/// the buffer grows to hold every sample on the GPU too, as on the CPU.
#[test]
fn transform_motion_blur_past_the_buffer() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut s = Scene::new(depth);
        let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
        s.push(bg);
        let mut l = s.footage(70, 44);
        s.effect(&mut l, "ec.distort.transform", &[("useCompositionShutterAngle", Value::Bool(false)), ("shutterAngle", n(720.0))]);
        let pos = l.props.prop_mut("effects/#1/position").expect("transform position");
        pos.keys = vec![Keyframe::new(Tick::ZERO, pt(-30.0, 22.0)), Keyframe::new(Tick::from_seconds_f64(0.5), pt(120.0, 30.0))];
        set(&mut l, "transform/position", v3(48.0, 30.0));
        s.push(l);
        let label = format!("transform motion blur past the buffer {depth:?}");
        for t in [0.1, 0.25] {
            check(&label, compare_at(&s, opts(), Tick::from_seconds_f64(t)), 0.0);
            check(&format!("{label} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::from_seconds_f64(t)), 0.0);
        }
    }
}

#[test]
fn text_distortions_use_expanded_canvas() {
    use effectcraft_keyframe::TextDoc;
    use effectcraft_project::{LayerSource, build};
    for id in ["ec.distort.cctiler", "ec.distort.ccsmear"] {
        let mut s = Scene::new(BitDepth::Bpc32);
        let mut l = build::layer(&mut s.p, &s.comp, "Text", LayerSource::Text, (97, 61), None);
        set(&mut l, "text/sourceText", Value::Text(Box::new(TextDoc { text: "H".into(), size: 14.0, ..Default::default() })));
        let values = if id.ends_with("cctiler") {
            vec![("scale", n(50.0))]
        } else {
            vec![("from", pt(52.0, 25.0)), ("to", pt(75.0, 25.0)), ("radius", n(20.0)), ("reach", n(150.0))]
        };
        s.effect(&mut l, id, &values);
        s.push(l);
        for scale in [1.0, 0.5] {
            check(id, compare_at(&s, RenderOpts { scale, ..opts() }, Tick::ZERO), 0.0);
        }
    }
}

/// A comp-sized layer with Deep Glow (a padded buffer) whose Transform effect flies it across
/// the frame with a 720° shutter: the frame times on the CPU and the GPU.
#[test]
#[ignore = "perf: cargo test -p effectcraft-gpu --lib transform_motion_blur_perf -- --ignored --nocapture"]
fn transform_motion_blur_perf() {
    use effectcraft_color::Label;
    use effectcraft_project::{Comp, ItemKind, Project};
    use effectcraft_render::{Backend, Renderer};
    use effectcraft_time::FrameRate;
    let Some(g) = crate::tests::gpu() else { return };
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc8;
    let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let mut s = Scene { p, cid, comp };
    let mut l = s.solid([0.25, 0.55, 1.0], 1920, 1080);
    s.effect(&mut l, "ec.stylize.deepglow", &[("PEDG-0001", n(295.0))]);
    s.effect(&mut l, "ec.distort.transform", &[("useCompositionShutterAngle", Value::Bool(false)), ("shutterAngle", n(720.0))]);
    let pos = l.props.prop_mut("effects/#2/position").expect("transform position");
    pos.keys = vec![Keyframe::new(Tick::ZERO, pt(-800.0, 540.0)), Keyframe::new(Tick::from_seconds_f64(0.5), pt(960.0, 540.0))];
    s.push(l);
    let t = Tick::from_seconds_f64(0.2);
    let opts = opts();
    let t0 = std::time::Instant::now();
    let _ = Renderer::new(&s.p, &crate::tests::Pattern, effectcraft_render::RenderOpts { backend: Backend::Cpu, ..opts }).comp_frame_cpu(s.cid, t);
    let cpu_ms = t0.elapsed().as_secs_f64() * 1e3;
    // Consecutive frames with a layer cache (as the viewer renders them): the static Deep Glow
    // is kept on the first and restored on the next.
    let cache = effectcraft_render::cache::LayerCache::new(2 << 30);
    let mut gpu_ms = Vec::new();
    for k in 0..6 {
        let mut r = Renderer::new(&s.p, &crate::tests::Pattern, effectcraft_render::RenderOpts { backend: Backend::Gpu, ..opts });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let t0 = std::time::Instant::now();
        let _ = g.render(&r, s.cid, t + Tick::from_seconds_f64(k as f64 / 30.0)).expect("the GPU renders the frame");
        gpu_ms.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    #[cfg(feature = "profile")]
    {
        let mut r = Renderer::new(&s.p, &crate::tests::Pattern, effectcraft_render::RenderOpts { backend: Backend::Gpu, ..opts });
        r.accel = Some(g);
        r.cache = Some(&cache);
        effectcraft_render::profile::start();
        let _ = g.render(&r, s.cid, t + Tick::from_seconds_f64(0.2));
        let (events, _) = effectcraft_render::profile::finish();
        let mut by: std::collections::BTreeMap<(String, String), (usize, f64)> = Default::default();
        for e in &events {
            let s = by.entry((e.cat.to_string(), e.name.to_string())).or_default();
            s.0 += 1;
            s.1 += e.dur / 1e3;
        }
        for ((c, n), (k, d)) in by {
            eprintln!("    {c} {n} x{k} {d:.2} ms");
        }
    }
    eprintln!("transform motion blur 1920x1080 + deep glow: cpu {cpu_ms:.1} ms, gpu frames (incl. readback) {gpu_ms:.1?} ms");
}

/// A static Deep Glow under an animated Transform effect is rendered once and restored on the
/// next frames (`Renderer::static_prefix`): with a layer cache every frame is the same, bit for
/// bit, as without one, on the GPU and the CPU, and also when the glow animates (no reuse).
#[test]
fn static_first_effects_are_reused_across_frames() {
    use effectcraft_render::{Backend, Renderer};
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for animated_glow in [false, true] {
            let mut s = Scene::new(depth);
            let bg = s.solid([0.15, 0.1, 0.2], 97, 61);
            s.push(bg);
            let mut l = s.footage(40, 24);
            s.effect(&mut l, "ec.stylize.deepglow", &[("PEDG-0001", n(12.0)), ("glowExposure", n(0.5))]);
            s.effect(&mut l, "ec.distort.transform", &[("useCompositionShutterAngle", Value::Bool(false)), ("shutterAngle", n(360.0))]);
            let pos = l.props.prop_mut("effects/#2/position").expect("transform position");
            pos.keys = vec![Keyframe::new(Tick::ZERO, pt(-10.0, 12.0)), Keyframe::new(Tick::from_seconds_f64(0.5), pt(60.0, 20.0))];
            if animated_glow {
                let r = l.props.prop_mut("effects/#1/PEDG-0001").expect("glow radius");
                r.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(6.0)), Keyframe::new(Tick::from_seconds_f64(0.5), Value::Scalar(16.0))];
            }
            set(&mut l, "transform/position", v3(48.0, 30.0));
            s.push(l);
            let label = format!("static first effects {depth:?} animated glow {animated_glow}");
            let cache = effectcraft_render::cache::LayerCache::new(256 << 20);
            let cpu_cache = effectcraft_render::cache::LayerCache::new(256 << 20);
            for k in 0..4 {
                let t = Tick::from_seconds_f64(0.1 + k as f64 / 30.0);
                let mut plain = Renderer::new(&s.p, &crate::tests::Pattern, effectcraft_render::RenderOpts { backend: Backend::Cpu, ..opts() });
                let cpu = plain.comp_frame_cpu(s.cid, t);
                plain.cache = Some(&cpu_cache);
                assert_eq!(plain.comp_frame_cpu(s.cid, t).data, cpu.data, "{label}: CPU frame {k} with a cache");
                let Some(g) = crate::tests::gpu() else { continue };
                let mut r = Renderer::new(&s.p, &crate::tests::Pattern, effectcraft_render::RenderOpts { backend: Backend::Gpu, ..opts() });
                r.accel = Some(g);
                let uncached = g.render(&r, s.cid, t).expect("the GPU renders the frame");
                r.cache = Some(&cache);
                let cached = g.render(&r, s.cid, t).expect("the GPU renders the frame");
                assert_eq!(cached.data, uncached.data, "{label}: GPU frame {k} with a cache");
                check(&format!("{label} frame {k}"), compare_at(&s, opts(), t), 0.0);
            }
        }
    }
}
