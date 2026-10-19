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
    set(&mut l, "transform/scale", Value::Vec3([160.0, 160.0, 100.0]));
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
        set(&mut l, &format!("{GROUP}/bevelEmboss/size"), n(9.0));
        set(&mut l, &format!("{GROUP}/bevelEmboss/depth"), n(40.0));
        set(&mut l, &format!("{GROUP}/gradientOverlay/alignWithLayer"), Value::Bool(true));
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
                    assert!(crate::plane3d::MADE_ON_GPU.load(Ordering::Relaxed) > before, "styled {styled} {depth:?} at {scale}: the plane was made on the CPU");
                }
                check(&format!("3D circle, styled {styled}, {depth:?} at {scale}"), d, 0.002);
            }
        }
    }
}
