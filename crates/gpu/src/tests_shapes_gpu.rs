//! Shape layers rasterised on the GPU (`shapes_gpu`) against the CPU reference: each layer's
//! resident source (the buffer `Renderer::resident_source` makes: the coverage masks are
//! tiny-skia's, sample for sample) and whole frames through the GPU walk.

use effectcraft_keyframe::{Gradient, Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, LayerSource, MaskMode};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::context::Enc;
use crate::tests::{Pattern, Scene, check, diff, gpu, tolerance};

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

fn gradient() -> Gradient {
    Gradient {
        colors: vec![(0.0, [1.0, 0.2, 0.1, 1.0]), (0.6, [0.1, 0.9, 0.3, 1.0]), (1.0, [0.2, 0.1, 0.9, 1.0])],
        opacities: vec![(0.0, 1.0), (1.0, 0.4)],
        color_midpoints: vec![0.3, 0.5],
        opacity_midpoints: vec![],
    }
}

/// Over a footage backdrop: a rotated, scaled, moving card (rounded rectangle + ellipse, fill and
/// a dashed round-joined stroke), a star with an even-odd radial gradient fill (highlight) and a
/// linear gradient stroke, a trimmed ellipse stroke with round caps, and a repeated rectangle.
fn scene(depth: BitDepth) -> Scene {
    let mut sc = Scene::new(depth);
    let bg = sc.footage(97, 61);
    sc.push(bg);
    let mut layers: Vec<_> = (0..5).map(|i| build::layer(&mut sc.p, &sc.comp, &format!("Shape {i}"), LayerSource::Shape, (97, 61), None)).collect();
    let mut next = sc.p.next_id;
    let mut ids = Ids(&mut next);

    let mut stroke = build::shape_stroke(&mut ids, [1.0, 1.0, 1.0, 1.0], 3.0);
    stroke.prop_mut("join").unwrap().value = Value::Enum(1);
    stroke.prop_mut("cap").unwrap().value = Value::Enum(1);
    stroke.prop_mut("dashes/dash").unwrap().value = Value::Scalar(6.0);
    stroke.prop_mut("dashes/gap").unwrap().value = Value::Scalar(3.0);
    // Content animates too (rectangle size, highlight angle): moving alone keeps the resident
    // pixels, these are rasterised again every frame.
    let mut rect = build::shape_rect(&mut ids, [50.0, 30.0], [-10.0, 0.0], 8.0);
    rect.prop_mut("size").unwrap().keys = vec![Keyframe::new(Tick::ZERO, Value::Vec2([50.0, 30.0])), Keyframe::new(s(1.0), Value::Vec2([37.5, 41.3]))];
    let card = vec![rect, build::shape_ellipse(&mut ids, [30.0, 30.0], [15.0, 5.0]), stroke, build::shape_fill(&mut ids, [0.2, 0.6, 0.9, 0.8])];

    let mut gfill = build::shape_gradient_fill(&mut ids, true, [0.0, 0.0], [18.0, 4.0], gradient());
    gfill.prop_mut("rule").unwrap().value = Value::Enum(1);
    gfill.prop_mut("highlightLength").unwrap().value = Value::Scalar(40.0);
    gfill.prop_mut("highlightAngle").unwrap().keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(30.0)), Keyframe::new(s(1.0), Value::Scalar(160.0))];
    let star = vec![
        build::shape_star(&mut ids, true, 5.0, [0.0, 0.0], 22.0, 9.0),
        build::shape_gradient_stroke(&mut ids, false, [-20.0, 0.0], [20.0, 0.0], gradient(), 4.0),
        gfill,
    ];

    let mut cap = build::shape_stroke(&mut ids, [0.9, 0.8, 0.1, 1.0], 5.0);
    cap.prop_mut("cap").unwrap().value = Value::Enum(1);
    let trimmed = vec![build::shape_ellipse(&mut ids, [40.0, 26.0], [0.0, 0.0]), build::shape_trim(&mut ids, 10.0, 80.0, 25.0), cap];

    let mut fill = build::shape_fill(&mut ids, [0.9, 0.3, 0.5, 1.0]);
    fill.prop_mut("opacity").unwrap().value = Value::Scalar(60.0);
    let repeated = vec![build::shape_rect(&mut ids, [9.0, 13.0], [0.0, 0.0], 2.0), fill, build::shape_repeater(&mut ids, 4.0, [12.5, 1.5])];

    // Reaching far past the comp's corner: its edges are clipped to the window first.
    let mut rim = build::shape_stroke(&mut ids, [0.3, 0.9, 0.4, 1.0], 7.0);
    rim.prop_mut("cap").unwrap().value = Value::Enum(1);
    let off = vec![build::shape_ellipse(&mut ids, [90.0, 70.0], [0.0, 0.0]), rim, build::shape_fill(&mut ids, [0.8, 0.5, 0.2, 0.9])];

    for (l, items) in layers.iter_mut().zip([card, star, trimmed, repeated, off]) {
        l.props.sub_mut("contents").unwrap().children.push(build::shape_group(&mut ids, "Group", items).into());
    }
    sc.p.next_id = next;
    let key = |a: Value, b: Value| vec![Keyframe::new(Tick::ZERO, a), Keyframe::new(s(1.0), b)];
    let set = |l: &mut effectcraft_project::Layer, path: &str, v: Value| l.props.prop_mut(path).unwrap().value = v;
    layers[0].props.prop_mut("transform/position").unwrap().keys = key(Value::Vec3([30.0, 25.0, 0.0]), Value::Vec3([62.3, 33.7, 0.0]));
    set(&mut layers[0], "transform/rotation", Value::Scalar(12.0));
    set(&mut layers[0], "transform/scale", Value::Vec3([90.0, 110.0, 100.0]));
    set(&mut layers[1], "transform/position", Value::Vec3([70.4, 20.6, 0.0]));
    layers[1].props.prop_mut("transform/rotation").unwrap().keys = key(Value::Scalar(0.0), Value::Scalar(40.0));
    set(&mut layers[2], "transform/position", Value::Vec3([28.0, 44.0, 0.0]));
    set(&mut layers[3], "transform/position", Value::Vec3([50.0, 50.0, 0.0]));
    set(&mut layers[4], "transform/position", Value::Vec3([88.0, 6.0, 0.0]));
    set(&mut layers[4], "transform/rotation", Value::Scalar(-17.0));
    set(&mut layers[4], "transform/scale", Value::Vec3([180.0, 150.0, 100.0]));
    // Below the others, so their indices stay put.
    let off = layers.pop().unwrap();
    sc.push(off);
    for l in layers {
        sc.push(l);
    }
    sc
}

/// Every shape layer's resident source made on the GPU equals the CPU's: same size, offset and
/// scale, and the same pixels (the coverage is tiny-skia's to the sample; colour and gradients
/// in f32 on the GPU), at 8, 16 and 32 bpc, Full and Half.
#[test]
fn shape_layer_sources_on_the_gpu_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = scene(depth);
        for scale in [1.0, 0.5] {
            for t in [0.0, 0.43] {
                let cache = LayerCache::default();
                let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, scale, ..Default::default() });
                r.accel = Some(g);
                r.cache = Some(&cache);
                let ctx = r.eval_ctx(sc.cid, s(t)).unwrap();
                for layer in ctx.comp.layers.iter().filter(|l| matches!(l.source, LayerSource::Shape)) {
                    let label = format!("{} {depth:?} scale {scale} t {t}", layer.name);
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
                    assert!(cpu.img.data.iter().any(|p| p[3] > 0.5), "{label}: the shape shows");
                    let mut d = diff(&cpu.img, &got, tolerance(depth));
                    d.quantized = depth != BitDepth::Bpc32;
                    check(&label, Some(d), 0.0);
                }
            }
        }
    }
}

/// Whole frames through the GPU walk (layer cache on: the resident path that rasterises shapes
/// here) match the CPU oracle, and the shapes did rasterise on the GPU.
#[test]
fn frames_with_gpu_rasterised_shapes_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = scene(depth);
        let cache = LayerCache::default();
        for t in [0.0, 0.5, 0.9] {
            let before = crate::shapes_gpu::RASTERISED.load(std::sync::atomic::Ordering::Relaxed);
            let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let img = g.render(&r, sc.cid, s(t)).expect("the GPU renders the frame");
            let made = crate::shapes_gpu::RASTERISED.load(std::sync::atomic::Ordering::Relaxed) - before;
            let cpu = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame_cpu(sc.cid, s(t));
            let mut d = diff(&cpu, &img, tolerance(depth));
            d.quantized = depth != BitDepth::Bpc32;
            check(&format!("shape frame {depth:?} t {t}"), Some(d), 0.0);
            // The first frame rasterises every shape; later ones the animated two at least.
            assert!(made >= 2, "{depth:?} t {t}: {made} shape layers rasterised on the GPU");
        }
    }
}

/// What the plan can't reproduce stays on the CPU: masks, non-Normal draws, Taper strokes.
#[test]
fn shapes_the_gpu_cannot_match_stay_on_the_cpu() {
    let mut sc = scene(BitDepth::Bpc8);
    let mut next = sc.p.next_id;
    let mut ids = Ids(&mut next);
    let comp = sc.p.comp_mut(sc.cid).unwrap();
    // `Scene::push` puts each layer on top: [repeated, trimmed, star, card, off-comp, backdrop].
    // The card gets a mask.
    let m = build::mask(&mut ids, "Mask 1", ShapePath::rect([10.0, 10.0], 40.0, 30.0), MaskMode::Add, [255, 255, 0]);
    comp.layers[3].props.sub_mut("masks").unwrap().children.push(m.into());
    // The star's gradient fill multiplies.
    let fill = comp.layers[2].props.prop_mut("contents/#1/contents/#3/blend").map(|p| p.value = Value::Enum(3));
    assert!(fill.is_some(), "the star's gradient fill has a blend mode");
    // The trimmed ellipse's stroke tapers.
    let stroke = comp.layers[1].props.prop_mut("contents/#1/contents/#3/taper/startLength").map(|p| p.value = Value::Scalar(20.0));
    assert!(stroke.is_some(), "the trimmed stroke has a taper");
    comp.layers[1].props.prop_mut("contents/#1/contents/#3/taper/startWidth").unwrap().value = Value::Scalar(0.0);
    sc.p.next_id = next;
    let cache = LayerCache::default();
    let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts::default());
    r.cache = Some(&cache);
    let ctx = r.eval_ctx(sc.cid, s(0.2)).unwrap();
    let shapes: Vec<_> = ctx.comp.layers.iter().filter(|l| matches!(l.source, LayerSource::Shape)).collect();
    let plan = |i: usize| {
        let l = shapes[i];
        let (_, clip) = r.resident_content(&ctx, l).expect("keyed");
        r.shape_raster_plan(&ctx, l, clip)
    };
    assert!(!r.shape_raster_eligible(&ctx, shapes[3]), "masked");
    assert!(plan(3).is_none(), "masked");
    assert!(plan(2).is_none(), "Multiply fill");
    assert!(plan(1).is_none(), "tapered stroke");
    let repeated = plan(0).expect("plain fills plan");
    assert_eq!(repeated.draws.len(), 4, "one draw per repeated copy");
}
