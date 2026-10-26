//! Mask feathers on the GPU (MASK-GPU1) against the CPU: the blur of a coverage plane alone
//! (Gaussian and Linear falloffs, crops of a padded plane) and whole frames whose layers carry
//! feathered masks in every mode, inverted, with opacity and expansion.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, ShapePath, TextDoc, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, FeatherFalloff, GroupKind, ItemKind, Layer, LayerSource, MaskMode, MaskMotionBlur, PropGroup};
use effectcraft_render::masks::FeatherBlur;
use effectcraft_render::{Accelerator, Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::context::Enc;
use crate::tests::{self, Scene, check, compare_at, opts, set};

/// An anti-aliased disc plus a hard-edged bar: the shapes a mask's coverage has.
fn plane(w: usize, h: usize) -> Vec<f32> {
    let (cx, cy, r) = (w as f32 * 0.45, h as f32 * 0.5, w.min(h) as f32 * 0.3);
    let mut out = vec![0.0; w * h];
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let disc = (r - d + 0.5).clamp(0.0, 1.0);
            let bar = if x > w * 3 / 4 && y > h / 5 { 1.0 } else { 0.0 };
            out[y * w + x] = f32::max(disc, bar);
        }
    }
    out
}

#[test]
fn feather_blur_matches_the_cpu() {
    let Some(g) = tests::gpu() else { return };
    for (w, h) in [(300usize, 260usize), (613, 131)] {
        let data = plane(w, h);
        let blurs = [
            FeatherBlur::Gaussian { sx: 0.5, sy: 0.5 },
            FeatherBlur::Gaussian { sx: 12.0, sy: 3.0 },
            FeatherBlur::Gaussian { sx: 88.5, sy: 88.5 },
            FeatherBlur::Gaussian { sx: 40.0, sy: 0.0 },
            FeatherBlur::Box { rx: 1, ry: 1 },
            FeatherBlur::Box { rx: 15, ry: 40 },
            FeatherBlur::Box { rx: 0, ry: 9 },
        ];
        let crops = [[0, 0, w as u32, h as u32], [17, 9, w as u32 - 40, h as u32 - 21], [5, 3, 1, 1], [2, 0, 7, h as u32]];
        for blur in blurs {
            let cpu = blur.cpu(&data, w, h);
            for crop in crops {
                let gpu = g.mask_feather(&data, w as u32, h as u32, blur, crop).expect("the GPU blurs the plane");
                let [x, y, cw, ch] = crop.map(|v| v as usize);
                assert_eq!(gpu.len(), cw * ch, "{blur:?} {crop:?}");
                let mut max = 0.0f32;
                for row in 0..ch {
                    for col in 0..cw {
                        let a = cpu[(y + row) * w + x + col];
                        let b = gpu[row * cw + col];
                        max = max.max((a - b).abs());
                    }
                }
                assert!(max < 2e-5, "{w}×{h} {blur:?} crop {crop:?}: max difference {max}");
            }
        }
    }
}

#[test]
fn feather_declines_bad_input() {
    let Some(g) = tests::gpu() else { return };
    let data = plane(40, 30);
    let blur = FeatherBlur::Gaussian { sx: 3.0, sy: 3.0 };
    assert!(g.mask_feather(&data, 40, 31, blur, [0, 0, 40, 30]).is_none(), "plane size");
    assert!(g.mask_feather(&data, 40, 30, blur, [1, 0, 40, 30]).is_none(), "crop past the plane");
    assert!(g.mask_feather(&data, 40, 30, blur, [0, 0, 0, 30]).is_none(), "empty crop");
    assert!(g.mask_feather(&data, 40, 30, blur, [u32::MAX, 0, 2, 2]).is_none(), "overflowing crop");
}

/// A mask with these settings (feather in px; `linear` = Linear falloff).
#[allow(clippy::too_many_arguments)]
fn mask(s: &mut Scene, path: ShapePath, mode: MaskMode, feather: [f64; 2], linear: bool, inverted: bool, opacity: f64, expansion: f64) -> PropGroup {
    let mut next = s.p.next_id;
    let mut m = build::mask(&mut Ids(&mut next), "Mask", path, mode, [255, 255, 0]);
    s.p.next_id = next;
    if let GroupKind::Mask { inverted: inv, feather_falloff, .. } = &mut m.kind {
        *inv = inverted;
        if linear {
            *feather_falloff = FeatherFalloff::Linear;
        }
    }
    m.get_mut("feather").unwrap().value = Value::Vec2(feather);
    m.get_mut("opacity").unwrap().value = Value::Scalar(opacity);
    m.get_mut("expansion").unwrap().value = Value::Scalar(expansion);
    m
}

fn push_masks(l: &mut Layer, masks: Vec<PropGroup>) {
    let group = l.props.sub_mut("masks").unwrap();
    for m in masks {
        group.children.push(m.into());
    }
}

/// A comp big enough that its mask planes blur on the GPU ([`effectcraft_render::masks::GPU_FEATHER_MIN_PIXELS`]).
fn scene(depth: BitDepth) -> Scene {
    let mut s = Scene::new(depth);
    let comp = Comp::new(420, 300, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    s.cid = s.p.add_item("Big", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    s.comp = comp;
    s
}

#[test]
fn feathered_masks_render_like_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut s = scene(depth);
        let bg = s.solid([0.1, 0.15, 0.2], 420, 300);
        s.push(bg);
        // The spotlight: one ellipse, a large Gaussian feather, the layer as an alpha matte's source.
        let mut spot = s.footage(420, 300);
        let m = mask(&mut s, ShapePath::ellipse([200.0, 150.0], 220.0, 160.0), MaskMode::Add, [177.0, 177.0], false, false, 100.0, 0.0);
        push_masks(&mut spot, vec![m]);
        s.push(spot);
        check(&format!("spotlight {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);

        // Every mode over a first mask, Linear and Gaussian, inverted, opacity, expansion, and
        // a mask running past the layer's edge.
        for (i, mode) in
            [MaskMode::Add, MaskMode::Subtract, MaskMode::Intersect, MaskMode::Lighten, MaskMode::Darken, MaskMode::Difference].into_iter().enumerate()
        {
            let mut s = scene(depth);
            let bg = s.solid([0.2, 0.1, 0.05], 420, 300);
            s.push(bg);
            let mut l = s.footage(400, 280);
            let first = mask(&mut s, ShapePath::rect([150.0, 140.0], 220.0, 300.0), MaskMode::Add, [30.0, 12.0], i % 2 == 1, false, 100.0, 6.0);
            let second = mask(&mut s, ShapePath::ellipse([260.0, 120.0], 180.0, 140.0), mode, [24.0, 40.0], i % 2 == 0, i % 3 == 0, 70.0, -4.0);
            push_masks(&mut l, vec![first, second]);
            s.push(l);
            check(&format!("{mode:?} {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
        }
    }
}

// ---------------------------------------------------------------- MASK-GPU2: masks drawn on the GPU

fn shape_layer(s: &mut Scene, name: &str, size: [f64; 2], color: [f64; 4]) -> Layer {
    let mut l = build::layer(&mut s.p, &s.comp, name, LayerSource::Shape, (420, 300), None);
    let mut next = s.p.next_id;
    let mut ids = Ids(&mut next);
    let items = vec![build::shape_rect(&mut ids, size, [0.0, 0.0], 12.0), build::shape_fill(&mut ids, color)];
    l.props.sub_mut("contents").unwrap().children.push(build::shape_group(&mut ids, "Group", items).into());
    s.p.next_id = next;
    l
}

/// Over a footage backdrop: the spotlight (a shape whose ellipse mask has a 177 px Gaussian
/// feather and a small expansion), a shape with three masks (Linear-feathered Add, inverted
/// Subtract at 60 %, Intersect), a text layer with a feathered mask, and a shape whose shrinking
/// mask the GPU can't draw (it renders on the CPU).
fn masked_scene(depth: BitDepth) -> Scene {
    let mut s = scene(depth);
    let bg = s.footage(420, 300);
    s.push(bg);

    let mut spot = shape_layer(&mut s, "Spotlight", [380.0, 260.0], [1.0, 0.95, 0.8, 1.0]);
    let m = mask(&mut s, ShapePath::ellipse([10.0, -5.0], 180.0, 120.0), MaskMode::Add, [177.0, 177.0], false, false, 100.0, 3.0);
    push_masks(&mut spot, vec![m]);
    s.push(spot);

    let mut multi = shape_layer(&mut s, "Multi", [200.0, 150.0], [0.2, 0.5, 0.9, 0.9]);
    set(&mut multi, "transform/rotation", Value::Scalar(9.0));
    let a = mask(&mut s, ShapePath::rect([-20.0, 0.0], 150.0, 130.0), MaskMode::Add, [16.0, 6.0], true, false, 100.0, 0.0);
    let b = mask(&mut s, ShapePath::ellipse([40.0, 20.0], 90.0, 70.0), MaskMode::Subtract, [10.0, 10.0], false, true, 60.0, 0.0);
    let c = mask(&mut s, ShapePath::rect([0.0, 0.0], 400.0, 90.0), MaskMode::Intersect, [0.0, 0.0], false, false, 85.0, 0.0);
    push_masks(&mut multi, vec![a, b, c]);
    s.push(multi);

    let mut text = build::layer(&mut s.p, &s.comp, "Text", LayerSource::Text, (420, 300), None);
    let doc = TextDoc { text: "Masked words".into(), size: 40.0, fill: [1.0, 1.0, 1.0, 1.0], ..Default::default() };
    set(&mut text, "text/sourceText", Value::Text(Box::new(doc)));
    set(&mut text, "transform/position", Value::Vec3([60.0, 250.0, 0.0]));
    let m = mask(&mut s, ShapePath::rect([110.0, -14.0], 160.0, 34.0), MaskMode::Add, [12.0, 12.0], false, false, 100.0, 0.0);
    push_masks(&mut text, vec![m]);
    s.push(text);

    let mut shrink = shape_layer(&mut s, "Shrink", [120.0, 80.0], [0.9, 0.2, 0.3, 1.0]);
    set(&mut shrink, "transform/position", Value::Vec3([330.0, 70.0, 0.0]));
    let m = mask(&mut s, ShapePath::ellipse([0.0, 0.0], 110.0, 70.0), MaskMode::Add, [8.0, 8.0], false, false, 100.0, -6.0);
    push_masks(&mut shrink, vec![m]);
    s.push(shrink);
    s
}

/// Each masked shape and text layer's resident source made on the GPU (rasterised and masked
/// there) equals the CPU's; the shrinking mask declines.
#[test]
fn masked_layer_sources_on_the_gpu_match_the_cpu() {
    let Some(g) = tests::gpu() else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let sc = masked_scene(depth);
        for scale in [1.0, 0.5] {
            let (gc, cc) = (LayerCache::default(), LayerCache::default());
            let mut r = Renderer::new(&sc.p, &tests::Pattern, RenderOpts { backend: Backend::Gpu, scale, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&gc);
            let mut cpu_r = Renderer::new(&sc.p, &tests::Pattern, RenderOpts { backend: Backend::Cpu, scale, ..Default::default() });
            cpu_r.cache = Some(&cc);
            let ctx = r.eval_ctx(sc.cid, Tick::ZERO).unwrap();
            let cpu_ctx = cpu_r.eval_ctx(sc.cid, Tick::ZERO).unwrap();
            for layer in ctx.comp.layers.iter().filter(|l| matches!(l.source, LayerSource::Shape | LayerSource::Text)) {
                let label = format!("{} {depth:?} scale {scale}", layer.name);
                let (_, clip) = r.resident_content(&ctx, layer).expect("keyed");
                let mut e = Enc::new(&g.ctx);
                let made = crate::shapes_gpu::resident_source(&mut e, &r, &ctx, layer, clip);
                if layer.name == "Shrink" {
                    assert!(made.is_none(), "{label}: a shrinking mask renders on the CPU");
                    continue;
                }
                let (img, offset, bs) = made.expect("the GPU rasterises and masks it");
                let (_, cpu_clip) = cpu_r.resident_content(&cpu_ctx, layer).expect("keyed");
                let cpu = cpu_r.resident_source(&cpu_ctx, layer, cpu_clip).expect("the CPU renders it");
                let got = e.download(&img).expect("readback");
                assert_eq!((got.width, got.height), (cpu.img.width, cpu.img.height), "{label}: size");
                assert!((offset[0] - cpu.offset[0]).abs() < 1e-9 && (offset[1] - cpu.offset[1]).abs() < 1e-9, "{label}: offset");
                assert_eq!(bs, cpu.scale, "{label}: scale");
                assert!(cpu.img.data.iter().any(|p| p[3] > 0.2), "{label}: something shows");
                assert!(cpu.img.data.iter().any(|p| p[3] > 0.01 && p[3] < 0.99), "{label}: the masks soften it");
                let mut d = tests::diff(&cpu.img, &got, tests::tolerance(depth));
                d.quantized = depth != BitDepth::Bpc32;
                check(&label, Some(d), 0.0);
            }
        }
    }
}

/// Whole frames through the GPU walk (layer cache on: the resident path) match the CPU oracle,
/// and the masks were applied on the GPU.
#[test]
fn frames_with_gpu_masks_match_the_cpu() {
    let Some(g) = tests::gpu() else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let sc = masked_scene(depth);
        let cache = LayerCache::default();
        let before = crate::masks_gpu::APPLIED.load(std::sync::atomic::Ordering::Relaxed);
        let mut r = Renderer::new(&sc.p, &tests::Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let img = g.render(&r, sc.cid, Tick::ZERO).expect("the GPU renders the frame");
        let applied = crate::masks_gpu::APPLIED.load(std::sync::atomic::Ordering::Relaxed) - before;
        let cpu = Renderer::new(&sc.p, &tests::Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame_cpu(sc.cid, Tick::ZERO);
        let mut d = tests::diff(&cpu, &img, tests::tolerance(depth));
        d.quantized = depth != BitDepth::Bpc32;
        check(&format!("masked frame {depth:?}"), Some(d), 0.0);
        assert!(applied >= 3, "{depth:?}: {applied} layers masked on the GPU");
    }
}

/// The plan declines what the GPU can't draw: a variable-width feather, mask motion blur on an
/// animated path, a shrinking expansion.
#[test]
fn masks_the_gpu_cannot_draw_decline() {
    let mut s = scene(BitDepth::Bpc8);
    let mut variable = shape_layer(&mut s, "Variable", [100.0, 100.0], [1.0; 4]);
    let mut m = mask(&mut s, ShapePath::rect([0.0, 0.0], 80.0, 80.0), MaskMode::Add, [0.0, 0.0], false, false, 100.0, 0.0);
    if let Value::Path(sp) = &mut m.get_mut("path").unwrap().value {
        sp.feather.push(Default::default());
    }
    push_masks(&mut variable, vec![m]);
    assert!(!effectcraft_render::masks::gpu_plan::gpu_eligible(&variable), "variable-width feather");

    let mut moving = shape_layer(&mut s, "Moving", [100.0, 100.0], [1.0; 4]);
    let mut m = mask(&mut s, ShapePath::rect([0.0, 0.0], 80.0, 80.0), MaskMode::Add, [0.0, 0.0], false, false, 100.0, 0.0);
    if let GroupKind::Mask { motion_blur, .. } = &mut m.kind {
        *motion_blur = MaskMotionBlur::On;
    }
    m.get_mut("path").unwrap().keys = vec![
        Keyframe::new(Tick::ZERO, Value::path(ShapePath::rect([0.0, 0.0], 80.0, 80.0))),
        Keyframe::new(Tick::from_seconds_f64(1.0), Value::path(ShapePath::rect([30.0, 0.0], 80.0, 80.0))),
    ];
    push_masks(&mut moving, vec![m]);
    assert!(!effectcraft_render::masks::gpu_plan::gpu_eligible(&moving), "motion-blurred animated mask");

    let plain = shape_layer(&mut s, "Plain", [100.0, 100.0], [1.0; 4]);
    assert!(effectcraft_render::masks::gpu_plan::gpu_eligible(&plain), "no masks");
}

/// RI1: a precomp layer the GPU walk hands to the CPU path (a masked precomp) is tagged "CPU"
/// on the frame its nested comp renders there, and not on the next frame, which the layer cache
/// serves (before, it was tagged whenever the walk handed it over, cached or not).
#[test]
fn a_precomp_is_tagged_cpu_only_when_its_nested_comp_renders_there() {
    let Some(g) = tests::gpu() else { return };
    let mut s = scene(BitDepth::Bpc8);
    let inner = Comp::new(120, 80, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let inner_id = s.p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    let solid = s.solid([0.3, 0.6, 0.9], 120, 80);
    s.p.comp_mut(inner_id).unwrap().layers = vec![solid].into();
    let mut pre = build::layer(&mut s.p, &s.comp, "Inner", LayerSource::Comp { item: inner_id }, (120, 80), None);
    let m = mask(&mut s, ShapePath::rect([0.0, 0.0], 100.0, 60.0), MaskMode::Add, [0.0, 0.0], false, false, 100.0, 0.0);
    push_masks(&mut pre, vec![m]);
    let pid = pre.id;
    s.push(pre);
    let cache = LayerCache::new(256 << 20);
    let tagged = |t: f64| {
        let times = effectcraft_render::LayerTimes::default();
        let mut r = Renderer::new(&s.p, &tests::Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
        r.accel = Some(g as &dyn Accelerator);
        r.cache = Some(&cache);
        r.times = Some(&times);
        g.render(&r, s.cid, Tick::from_seconds_f64(t)).expect("the GPU renders the frame");
        times.cpu_reason(s.cid, pid).is_some()
    };
    assert!(tagged(0.1), "the nested comp rendered on the CPU: tagged");
    assert!(!tagged(0.2), "the cache served the static precomp: not tagged");
}
