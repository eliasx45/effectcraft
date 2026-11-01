//! Track mattes built on the GPU (`walk::matte_image`, 1 Nov 2026): a shape, text or solid
//! matte's content is made resident there like a plain layer's (rasterised by `shapes_gpu`,
//! kept while unchanged) instead of drawn on the CPU (`Renderer::matte_buf`) and uploaded onto a
//! full-frame blank every frame. Elias's "K Step Stack" (glass and mosaic adjustment layers,
//! each matted by a hidden rounded rect) spent ~45 % of a frame uploading mattes and ~35 %
//! drawing them. Frames must match the CPU as before; footage and precomp mattes keep the CPU
//! path.

use std::sync::atomic::Ordering::Relaxed;

use effectcraft_color::BlendMode;
use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Layer, LayerSource, MaskMode, MatteKind, TrackMatte};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::tests::{Pattern, Scene, check, diff, gpu, n, opts, set, tolerance, v3};

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

/// A hidden (video off) shape matte: a rounded rect (or an ellipse) filled `fill`, at `at`.
fn shape_matte(sc: &mut Scene, name: &str, size: [f64; 2], at: [f64; 2], ellipse: bool, fill: [f64; 4]) -> Layer {
    let mut m = build::layer(&mut sc.p, &sc.comp, name, LayerSource::Shape, (97, 61), None);
    let mut next = sc.p.next_id;
    let mut ids = Ids(&mut next);
    let path = if ellipse { build::shape_ellipse(&mut ids, size, [0.0, 0.0]) } else { build::shape_rect(&mut ids, size, [0.0, 0.0], 5.0) };
    let fill = build::shape_fill(&mut ids, fill);
    let g = build::shape_group(&mut ids, "Group 1", vec![path, fill]);
    sc.p.next_id = next;
    m.props.sub_mut("contents").unwrap().children.push(g.into());
    m.switches.video = false;
    set(&mut m, "transform/position", v3(at[0], at[1]));
    m
}

fn matted(l: &mut Layer, m: &Layer, kind: MatteKind) {
    l.track_matte = Some(TrackMatte { layer: m.id, kind });
}

/// Over a footage backdrop: a footage layer under a moving, rotated rounded-rect Alpha matte at
/// 70 % opacity; a solid under an ellipse whose size animates (Alpha Inverted); two layers
/// sharing one grey Luma matte with a mask; a glass adjustment layer (Gaussian Blur) matted by
/// a rounded rect, as in "K Step Stack"; a visible shape that is also another layer's Luma
/// Inverted matte; and a footage matte (stays on the CPU path).
fn scene(depth: BitDepth) -> Scene {
    let mut sc = Scene::new(depth);
    let bg = sc.footage(97, 61);
    sc.push(bg);

    let mut a = sc.footage(80, 50);
    set(&mut a, "transform/position", v3(40.0, 30.0));
    let mut ma = shape_matte(&mut sc, "Matte A", [44.0, 22.0], [30.0, 20.0], false, [1.0, 1.0, 1.0, 1.0]);
    ma.props.prop_mut("transform/position").unwrap().keys = vec![Keyframe::new(Tick::ZERO, v3(30.0, 20.0)), Keyframe::new(s(1.0), v3(61.3, 37.6))];
    set(&mut ma, "transform/rotation", Value::Scalar(14.0));
    set(&mut ma, "transform/opacity", Value::Scalar(70.0));
    matted(&mut a, &ma, MatteKind::Alpha);
    sc.push(a);
    sc.push(ma);

    let mut b = sc.solid([0.9, 0.4, 0.1], 60, 40);
    set(&mut b, "transform/position", v3(60.0, 35.0));
    let mut mb = shape_matte(&mut sc, "Matte B", [30.0, 30.0], [60.0, 35.0], true, [1.0, 1.0, 1.0, 1.0]);
    mb.props.prop_mut("contents/#1/contents/#1/size").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec2([30.0, 30.0])), Keyframe::new(s(1.0), Value::Vec2([47.5, 21.3]))];
    matted(&mut b, &mb, MatteKind::AlphaInverted);
    sc.push(b);
    sc.push(mb);

    // One matte, two layers, a mask on the matte.
    let mut mc = shape_matte(&mut sc, "Matte C", [70.0, 40.0], [48.0, 30.0], false, [0.6, 0.6, 0.6, 1.0]);
    let mut next = sc.p.next_id;
    let mask = build::mask(&mut Ids(&mut next), "Mask 1", ShapePath::rect([20.0, 5.0], 50.0, 45.0), MaskMode::Add, [255, 255, 0]);
    sc.p.next_id = next;
    mc.props.sub_mut("masks").unwrap().children.push(mask.into());
    let mut c1 = sc.solid([0.2, 0.7, 0.9], 40, 30);
    set(&mut c1, "transform/position", v3(30.0, 40.0));
    let mut c2 = sc.footage(40, 30);
    set(&mut c2, "transform/position", v3(66.0, 22.0));
    c2.blend_mode = BlendMode::Screen;
    matted(&mut c1, &mc, MatteKind::Luma);
    matted(&mut c2, &mc, MatteKind::Luma);
    sc.push(c1);
    sc.push(c2);
    sc.push(mc);

    // The glass card.
    let mut glass = sc.solid([1.0, 1.0, 1.0], 97, 61);
    glass.switches.adjustment = true;
    set(&mut glass, "transform/opacity", Value::Scalar(80.0));
    sc.effect(&mut glass, "ec.blur.gaussian", &[("blurriness", n(6.0)), ("repeatEdge", Value::Bool(true))]);
    let mg = shape_matte(&mut sc, "Glass Matte", [52.0, 16.0], [50.0, 46.0], false, [1.0, 1.0, 1.0, 1.0]);
    matted(&mut glass, &mg, MatteKind::Alpha);
    sc.push(glass);
    sc.push(mg);

    // A visible shape that is also a matte.
    let mut d = sc.footage(50, 50);
    set(&mut d, "transform/position", v3(20.0, 15.0));
    let mut md = shape_matte(&mut sc, "Visible Matte", [24.0, 24.0], [22.0, 14.0], true, [0.8, 0.8, 0.8, 1.0]);
    md.switches.video = true;
    matted(&mut d, &md, MatteKind::LumaInverted);
    sc.push(d);
    sc.push(md);

    // A footage matte: the CPU's `matte_buf`, uploaded.
    let mut f = sc.solid([0.3, 0.9, 0.3], 30, 20);
    set(&mut f, "transform/position", v3(80.0, 50.0));
    let mut mf = sc.footage(40, 30);
    set(&mut mf, "transform/position", v3(82.0, 48.0));
    mf.switches.video = false;
    matted(&mut f, &mf, MatteKind::Alpha);
    sc.push(f);
    sc.push(mf);
    sc
}

/// Whole frames with the layer cache on (the walk's resident path: mattes rasterised on the GPU,
/// then kept) match the CPU oracle at 8, 16 and 32 bpc, Full and Half, also when a frame comes
/// back (the static mattes resident), and the shape mattes did come from the GPU.
#[test]
fn frames_with_gpu_track_mattes_match_the_cpu() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = scene(depth);
        for scale in [1.0, 0.5] {
            let cache = LayerCache::default();
            for t in [0.0, 0.5, 0.5, 0.9, 0.0] {
                let label = format!("track mattes {depth:?} scale {scale} t {t}");
                let before = crate::walk::MATTES_ON_GPU.load(Relaxed);
                let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, scale, ..opts() });
                r.accel = Some(g);
                r.cache = Some(&cache);
                let img = g.render(&r, sc.cid, s(t)).expect("the GPU renders the frame");
                let made = crate::walk::MATTES_ON_GPU.load(Relaxed) - before;
                let cpu = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Cpu, scale, ..opts() }).comp_frame_cpu(sc.cid, s(t));
                let mut d = diff(&cpu, &img, tolerance(depth));
                d.quantized = depth != BitDepth::Bpc32;
                check(&label, Some(d), 0.0);
                // Six shape-matted layers (the shared matte twice); the footage matte isn't one.
                assert!(made >= 6, "{label}: {made} track mattes from the GPU");
            }
        }
    }
}

/// The GPU-made matte equals the CPU's placed `matte_buf` pixel for pixel (within the
/// GPU-vs-CPU tolerances): a frame that shows only the matte, as the alpha of a white solid.
#[test]
fn a_gpu_matte_is_the_cpu_matte() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut sc = Scene::new(depth);
        let mut white = sc.solid([1.0, 1.0, 1.0], 97, 61);
        let mut m = shape_matte(&mut sc, "Matte", [61.0, 27.0], [47.3, 31.8], false, [1.0, 1.0, 1.0, 0.75]);
        set(&mut m, "transform/rotation", Value::Scalar(-23.0));
        set(&mut m, "transform/scale", Value::Vec3([87.0, 113.0, 100.0]));
        set(&mut m, "transform/opacity", Value::Scalar(55.0));
        matted(&mut white, &m, MatteKind::Alpha);
        sc.push(white);
        sc.push(m);
        let cache = LayerCache::default();
        for pass in 0..2 {
            let mut r = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let img = g.render(&r, sc.cid, Tick::ZERO).expect("the GPU renders the frame");
            let cpu = Renderer::new(&sc.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..opts() }).comp_frame_cpu(sc.cid, Tick::ZERO);
            assert!(cpu.data.iter().any(|p| p[3] > 0.3), "the matte shows");
            let mut d = diff(&cpu, &img, tolerance(depth));
            d.quantized = depth != BitDepth::Bpc32;
            check(&format!("matte only {depth:?} pass {pass}"), Some(d), 0.0);
        }
    }
}
