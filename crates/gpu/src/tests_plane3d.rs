//! Classic 3D shape planes made on the GPU (`plane3d`) vs the CPU compositor: a Continuously
//! Rasterized 3D shape scaled up (its effect canvas at a raster scale over 1), with blurs, CC
//! Radial Fast Blur and Glows, then Bevel and Emboss and a Gradient Overlay (P01 Clock's
//! circle), at 8 and 32 bpc, Full and Half, with motion blur; the plane must be made on the GPU.

use std::sync::atomic::Ordering;

use effectcraft_keyframe::{Gradient, Keyframe, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{BitDepth, LayerSource};
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;

use crate::tests::{Scene, check, compare_at, n, opts, set};

fn circle(depth: BitDepth, styled: bool) -> Scene {
    circle_at(depth, styled, 160.0, 9.0, true)
}

/// [`circle`] scaled `scale` % with a `bevel`-pixel Bevel and Emboss, its Gradient Overlay
/// aligned with the layer or not.
fn circle_at(depth: BitDepth, styled: bool, scale: f64, bevel: f64, align: bool) -> Scene {
    let mut s = Scene::new(depth);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = build::layer(&mut s.p, &s.comp, "Circle", LayerSource::Shape, (97, 61), None);
    let mut next = s.p.next_id;
    let mut ids = Ids(&mut next);
    let rect = build::shape_rect(&mut ids, [30.0, 22.0], [0.0, 0.0], 6.0);
    let fill = build::shape_fill(&mut ids, [0.9, 0.3, 0.1, 1.0]);
    let g = build::shape_group(&mut ids, "Rectangle 1", vec![rect, fill]);
    s.p.next_id = next;
    l.props.sub_mut("contents").unwrap().children.push(g.into());
    l.switches.three_d = true;
    l.switches.collapse = true;
    l.switches.motion_blur = true;
    set(&mut l, "transform/scale", Value::Vec3([scale, scale, 100.0]));
    set(&mut l, "transform/rotationY", Value::Scalar(18.0));
    s.effect(&mut l, "ec.blur.gaussian", &[("blurriness", n(1.5)), ("repeatEdge", Value::Bool(true))]);
    s.effect(&mut l, "ec.blur.ccradialfast", &[("center", Value::Vec2([40.0, 30.0])), ("amount", n(40.0))]);
    s.effect(&mut l, "ec.stylize.glow", &[("radius", n(6.0)), ("threshold", n(50.0)), ("operation", Value::Enum(1))]);
    // Animated: the layer cache would otherwise hold the plane.
    l.props.prop_mut("effects/#3/radius").unwrap().keys = vec![Keyframe::new(Tick::ZERO, n(6.0)), Keyframe::new(Tick::from_seconds_f64(1.0), n(9.0))];
    if styled {
        let mut next = s.p.next_id;
        for id in ["bevelEmboss", "gradientOverlay"] {
            st::add_style(&mut l, &mut Ids(&mut next), id, &s.comp.global_light, true).unwrap();
        }
        s.p.next_id = next;
        set(&mut l, &format!("{GROUP}/bevelEmboss/size"), n(bevel));
        set(&mut l, &format!("{GROUP}/bevelEmboss/depth"), n(40.0));
        set(&mut l, &format!("{GROUP}/gradientOverlay/alignWithLayer"), Value::Bool(align));
        set(
            &mut l,
            &format!("{GROUP}/gradientOverlay/colors"),
            Value::Gradient(Box::new(Gradient {
                colors: vec![(0.0, [0.28, 0.41, 1.0, 1.0]), (1.0, [0.04, 0.9, 0.5, 1.0])],
                color_midpoints: vec![0.5],
                opacities: vec![(0.0, 1.0), (1.0, 1.0)],
                opacity_midpoints: vec![0.5],
            })),
        );
    }
    s.push(l);
    s
}

#[test]
fn a_3d_shape_with_effects_and_styles_is_made_on_the_gpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for styled in [false, true] {
            for scale in [1.0, 0.5] {
                let s = circle(depth, styled);
                let before = crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed);
                let d = compare_at(&s, RenderOpts { scale, ..opts() }, Tick::from_seconds_f64(0.5));
                if d.is_some() {
                    assert!(
                        crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed) > before,
                        "styled {styled} {depth:?} at {scale}: the plane was made on the CPU"
                    );
                }
                check(&format!("3D circle, styled {styled}, {depth:?} at {scale}"), d, 0.002);
            }
        }
    }
}

/// A held 3D plane (nothing on it changes) is made on the GPU too (the CPU would style it once,
/// seconds for a large Bevel), and later frames upload nothing and read back only its alpha
/// bounds (a few bytes per row): the source buffer comes from the layer cache, its upload is
/// indexed by it.
#[test]
fn a_held_3d_styled_plane_is_made_on_the_gpu_without_uploads() {
    use effectcraft_render::{Backend, LayerCache, Renderer};
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        g.ctx.clear_uploads();
        let mut s = circle(depth, true);
        for l in &mut s.p.comp_mut(s.cid).unwrap().layers {
            if let Some(p) = l.props.prop_mut("effects/#3/radius") {
                p.keys.clear();
            }
        }
        let cache = LayerCache::default();
        let o = RenderOpts { backend: Backend::Gpu, ..opts() };
        let mut r = Renderer::new(&s.p, &crate::tests::Pattern, o);
        r.accel = Some(g);
        r.cache = Some(&cache);
        g.within_memory(|| g.render_display(&r, s.cid, Tick::from_seconds_f64(0.2))).unwrap().unwrap();
        let before = g.ctx.transfer_stats();
        let made = crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed);
        g.within_memory(|| g.render_display(&r, s.cid, Tick::from_seconds_f64(0.6))).unwrap().unwrap();
        let after = g.ctx.transfer_stats();
        assert_eq!(after.uploads, before.uploads, "{depth:?}: the held plane uploads nothing");
        assert!(after.readback_bytes - before.readback_bytes < 64 << 10, "{depth:?}: the held plane reads back only its bounds");
        assert!(crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed) > made, "{depth:?}: the held plane was made on the CPU");
    }
}

/// A 3D styled plane scaled up far past the frame (16×: most of it is never seen) styles only
/// the part the frame shows, plus the styles' reach, and the frame is unchanged: the styles'
/// images stay small (P01 Clock's 39-megapixel circle ran an 8 GB card out of memory).
#[test]
fn a_3d_styled_plane_far_past_the_frame_styles_only_what_is_seen() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for align in [false, true] {
            let s = circle_at(depth, true, 1600.0, 2.0, align);
            let (made, cut) = (crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed), crate::plane3d::CUT_TO_VIEW.load(Ordering::Relaxed));
            let d = compare_at(&s, opts(), Tick::from_seconds_f64(0.5));
            if d.is_some() {
                assert!(crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed) > made, "{depth:?}: the plane was made on the CPU");
                // A gradient aligned with the layer spans all of it: kept whole.
                assert_eq!(crate::plane3d::CUT_TO_VIEW.load(Ordering::Relaxed) > cut, !align, "{depth:?}, aligned {align}: cut to the view");
            }
            check(&format!("3D circle at 1600%, {depth:?}, aligned gradient {align}"), d, 0.002);
        }
    }
}
