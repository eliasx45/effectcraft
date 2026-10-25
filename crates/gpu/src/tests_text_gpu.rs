//! Text layers rasterised on the GPU (`shapes_gpu` with `effectcraft_render::text::gpu_plan`)
//! against the CPU reference: each layer's resident source (glyph runs filled and stroked with
//! tiny-skia's coverage, sample for sample) and whole frames through the GPU walk.

use effectcraft_keyframe::{Keyframe, ShapePath, TextDoc, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Layer, LayerSource, MaskMode};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::context::Enc;
use crate::tests::{Pattern, Scene, check, diff, gpu, tolerance};

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

fn set(l: &mut Layer, path: &str, v: Value) {
    l.props.prop_mut(path).unwrap_or_else(|| panic!("no {path}")).value = v;
}

fn keys(l: &mut Layer, path: &str, a: Value, b: Value) {
    l.props.prop_mut(path).unwrap_or_else(|| panic!("no {path}")).keys = vec![Keyframe::new(Tick::ZERO, a), Keyframe::new(s(1.0), b)];
}

/// An animator with `kinds` and its range selector, at index `n` (1-based).
fn animator(sc: &mut Scene, l: &mut Layer, kinds: &[&str]) {
    let mut next = sc.p.next_id;
    let mut ids = Ids(&mut next);
    let props = kinds.iter().flat_map(|k| build::text_anim_props(&mut ids, k, false)).collect();
    let g = build::text_animator(&mut ids, "Animator", props);
    sc.p.next_id = next;
    l.props.group_mut("text/animators").unwrap().children.push(g.into());
}

fn text(sc: &mut Scene, name: &str, doc: TextDoc) -> Layer {
    let mut l = build::layer(&mut sc.p, &sc.comp, name, LayerSource::Text, (97, 61), None);
    set(&mut l, "text/sourceText", Value::Text(Box::new(doc)));
    l
}

/// Over a footage backdrop: a faux-bold, stroked title whose characters move, turn, fade and
/// change colour under a range selector sweeping across them; a faux-italic line drawn all
/// strokes over all fills with a wider stroke and characters that scale; plain opaque text
/// whose touching glyphs are drawn as one run; and a large word drifting past the comp's edge
/// (kept windowed).
fn scene(depth: BitDepth) -> Scene {
    let mut sc = Scene::new(depth);
    let bg = sc.footage(97, 61);
    sc.push(bg);

    let mut big = text(&mut sc, "Big", TextDoc { text: "Wide".into(), size: 30.0, fill: [0.9, 0.8, 0.2, 1.0], ..Default::default() });
    set(&mut big, "transform/scale", Value::Vec3([320.0, 320.0, 100.0]));
    keys(&mut big, "transform/position", Value::Vec3([60.0, 70.0, 0.0]), Value::Vec3([20.3, 66.6, 0.0]));
    sc.push(big);

    let plain = TextDoc { text: "ffiAWAV".into(), size: 15.0, fill: [1.0, 1.0, 1.0, 1.0], tracking: -40.0, ..Default::default() };
    let mut plain = text(&mut sc, "Plain", plain);
    set(&mut plain, "transform/position", Value::Vec3([8.4, 55.2, 0.0]));
    set(&mut plain, "transform/rotation", Value::Scalar(-6.0));
    sc.push(plain);

    let line = TextDoc {
        text: "Wavy line".into(),
        size: 13.0,
        fill: [0.2, 0.8, 0.5, 0.85],
        stroke: [0.9, 0.2, 0.3, 1.0],
        stroke_width: 3.0,
        apply_stroke: true,
        faux_italic: true,
        ..Default::default()
    };
    let mut line = text(&mut sc, "Line", line);
    set(&mut line, "transform/position", Value::Vec3([5.5, 36.3, 0.0]));
    set(&mut line, "text/moreOptions/fillStroke", Value::Enum(1));
    animator(&mut sc, &mut line, &["scale", "strokeWidth"]);
    set(&mut line, "text/animators/#1/properties/scale", Value::Vec3([140.0, 70.0, 100.0]));
    set(&mut line, "text/animators/#1/properties/strokeWidth", Value::Scalar(1.5));
    keys(&mut line, "text/animators/#1/selectors/#1/end", Value::Scalar(20.0), Value::Scalar(90.0));
    sc.push(line);

    let title = TextDoc {
        text: "Spotlight".into(),
        size: 19.0,
        fill: [0.95, 0.9, 0.8, 1.0],
        stroke: [0.1, 0.2, 0.7, 1.0],
        stroke_width: 2.0,
        apply_stroke: true,
        faux_bold: true,
        ..Default::default()
    };
    let mut title = text(&mut sc, "Title", title);
    keys(&mut title, "transform/position", Value::Vec3([6.0, 18.0, 0.0]), Value::Vec3([9.7, 21.4, 0.0]));
    set(&mut title, "transform/rotation", Value::Scalar(4.0));
    animator(&mut sc, &mut title, &["position", "rotation", "opacity", "fillColor"]);
    set(&mut title, "text/animators/#1/properties/position", Value::Vec3([0.0, -9.0, 0.0]));
    set(&mut title, "text/animators/#1/properties/rotation", Value::Scalar(30.0));
    set(&mut title, "text/animators/#1/properties/opacity", Value::Scalar(35.0));
    set(&mut title, "text/animators/#1/properties/fillColor", Value::Color([0.9, 0.3, 0.1, 1.0]));
    set(&mut title, "text/animators/#1/selectors/#1/end", Value::Scalar(40.0));
    keys(&mut title, "text/animators/#1/selectors/#1/offset", Value::Scalar(-40.0), Value::Scalar(100.0));
    sc.push(title);
    sc
}

/// Every text layer's resident source made on the GPU equals the CPU's: same size, offset and
/// scale, and the same pixels, at 8, 16 and 32 bpc, Full and Half.
#[test]
fn text_layer_sources_on_the_gpu_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = scene(depth);
        for scale in [1.0, 0.5] {
            for t in [0.0, 0.37, 0.8] {
                let cache = LayerCache::default();
                let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, scale, ..Default::default() });
                r.accel = Some(g);
                r.cache = Some(&cache);
                let ctx = r.eval_ctx(sc.cid, s(t)).unwrap();
                for layer in ctx.comp.layers.iter().filter(|l| matches!(l.source, LayerSource::Text)) {
                    let label = format!("{} {depth:?} scale {scale} t {t}", layer.name);
                    assert!(r.text_raster_eligible(&ctx, layer), "{label}: eligible");
                    let (_, clip) = r.resident_content(&ctx, layer).expect("keyed");
                    let cpu = r.resident_source(&ctx, layer, clip).expect("the CPU renders it");
                    let mut e = Enc::new(&g.ctx);
                    let (img, offset, bs) = crate::shapes_gpu::resident_source(&mut e, &r, &ctx, layer, clip).expect("the GPU rasterises it");
                    let got = e.download(&img).expect("readback");
                    assert_eq!((got.width, got.height), (cpu.img.width, cpu.img.height), "{label}: size");
                    assert!(
                        (offset[0] - cpu.offset[0]).abs() < 1e-9 && (offset[1] - cpu.offset[1]).abs() < 1e-9,
                        "{label}: offset {offset:?} vs {:?}",
                        cpu.offset
                    );
                    assert_eq!(bs, cpu.scale, "{label}: scale");
                    let mut d = diff(&cpu.img, &got, tolerance(depth));
                    d.quantized = depth != BitDepth::Bpc32;
                    check(&label, Some(d), 0.0);
                }
            }
        }
    }
}

/// Whole frames through the GPU walk (layer cache on: the resident path) match the CPU oracle,
/// and the text did rasterise on the GPU.
#[test]
fn frames_with_gpu_rasterised_text_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = scene(depth);
        let cache = LayerCache::default();
        for t in [0.0, 0.5, 0.9] {
            let before = crate::shapes_gpu::RASTERISED_TEXT.load(std::sync::atomic::Ordering::Relaxed);
            let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let img = g.render(&r, sc.cid, s(t)).expect("the GPU renders the frame");
            let made = crate::shapes_gpu::RASTERISED_TEXT.load(std::sync::atomic::Ordering::Relaxed) - before;
            let cpu = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame_cpu(sc.cid, s(t));
            let mut d = diff(&cpu, &img, tolerance(depth));
            d.quantized = depth != BitDepth::Bpc32;
            check(&format!("text frame {depth:?} t {t}"), Some(d), 0.0);
            // The first frame rasterises every text layer; later ones the animated ones at least.
            assert!(made >= 2, "{depth:?} t {t}: {made} text layers rasterised on the GPU");
        }
    }
}

/// What the plan can't reproduce stays on the CPU: Blur and Gradient Fill animators, other
/// Inter-Character Blending modes (masks no longer: MASK-GPU2).
#[test]
fn text_the_gpu_cannot_match_stays_on_the_cpu() {
    let mut sc = scene(BitDepth::Bpc8);
    let mut next = sc.p.next_id;
    let mut ids = Ids(&mut next);
    let blur_props = build::text_anim_props(&mut ids, "blur", false);
    let blur = build::text_animator(&mut ids, "Animator", blur_props);
    let mask = build::mask(&mut ids, "Mask 1", ShapePath::rect([0.0, 0.0], 30.0, 20.0), MaskMode::Add, [255, 255, 0]);
    sc.p.next_id = next;
    let multiply = build::INTER_CHAR_BLEND_MODES.iter().position(|n| *n == "Multiply").expect("Multiply") as u32;
    let comp = sc.p.comp_mut(sc.cid).unwrap();
    for l in comp.layers.iter_mut() {
        match l.name.as_str() {
            "Title" => l.props.group_mut("text/animators").unwrap().children.push(blur.clone().into()),
            "Line" => set(l, "text/moreOptions/interCharBlend", Value::Enum(multiply)),
            "Plain" => l.props.sub_mut("masks").unwrap().children.push(mask.clone().into()),
            _ => {}
        }
    }
    let cache = LayerCache::default();
    let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts::default());
    r.cache = Some(&cache);
    let ctx = r.eval_ctx(sc.cid, s(0.2)).unwrap();
    let layer = |name: &str| ctx.comp.layers.iter().find(|l| l.name == name).unwrap();
    // A plain mask is drawn on the GPU too (MASK-GPU2, `tests_masks_gpu`).
    assert!(r.text_raster_eligible(&ctx, layer("Plain")), "masked Plain");
    for name in ["Title", "Line"] {
        assert!(!r.text_raster_eligible(&ctx, layer(name)), "{name}");
        let (_, clip) = r.resident_content(&ctx, layer(name)).expect("keyed");
        assert!(r.text_raster_plan(&ctx, layer(name), clip).is_none(), "{name}");
    }
    let big = layer("Big");
    let (_, clip) = r.resident_content(&ctx, big).expect("keyed");
    let plan = r.text_raster_plan(&ctx, big, clip).expect("plain text plans");
    assert!(!plan.draws.is_empty(), "the big word draws");
}

/// A motion-blurred title whose characters animate within the shutter (TEXT-GPU2): its source is
/// the average of the sub-samples (`blurred_source`), one group of draws each, rasterised in one
/// pass and matching the CPU's.
#[test]
fn motion_blurred_text_sources_on_the_gpu_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut sc = scene(depth);
        let comp = sc.p.comp_mut(sc.cid).unwrap();
        comp.enable_motion_blur = true;
        comp.motion_blur_samples = 8;
        for l in comp.layers.iter_mut().filter(|l| l.name == "Title" || l.name == "Line") {
            l.switches.motion_blur = true;
        }
        for t in [0.3, 0.6] {
            let cache = LayerCache::default();
            let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let ctx = r.eval_ctx(sc.cid, s(t)).unwrap();
            for layer in ctx.comp.layers.iter().filter(|l| l.switches.motion_blur) {
                let label = format!("{} {depth:?} t {t}", layer.name);
                let (_, clip) = r.resident_content(&ctx, layer).expect("keyed");
                let cpu = r.resident_source(&ctx, layer, clip).expect("the CPU renders it");
                let plan = r.text_raster_plan(&ctx, layer, clip).expect("plans");
                assert!(plan.draws.iter().any(|d| d.group > 0), "{label}: sub-samples");
                let mut e = Enc::new(&g.ctx);
                let (img, offset, bs) = crate::shapes_gpu::resident_source(&mut e, &r, &ctx, layer, clip).expect("the GPU rasterises it");
                let got = e.download(&img).expect("readback");
                assert_eq!((got.width, got.height, offset, bs), (cpu.img.width, cpu.img.height, cpu.offset, cpu.scale), "{label}");
                let mut d = diff(&cpu.img, &got, tolerance(depth));
                d.quantized = depth != BitDepth::Bpc32;
                check(&label, Some(d), 0.0);
            }
        }
    }
}
