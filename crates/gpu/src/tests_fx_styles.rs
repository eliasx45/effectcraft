//! GPU Layer Styles (`fx_styles`) vs the CPU styles (the oracle): Drop Shadow (knockout, noise),
//! Softer Outer and Inner Glow (colour and gradient, jitter), Inner Shadow, Satin, Color and
//! Gradient Overlay (every style), Fill Opacity, Blend Interior Styles as Group, Knockout and
//! the channel switches, Bevel and Emboss (every style and technique, Soften, Dissolve), at 8
//! and 32 bpc and Half, with linear blending, and a layer whose Gaussian Blur makes the styles'
//! content.

use std::sync::atomic::Ordering;

use effectcraft_color::BlendMode;
use effectcraft_keyframe::{Gradient, Value};
use effectcraft_project::build::Ids;
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{BitDepth, Layer};
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;

use crate::tests::{Scene, check, compare_at, opts, set};

const NORMAL: u32 = 0;
const DISSOLVE: u32 = 1;
const MULTIPLY: u32 = 3;
const SCREEN: u32 = 8;

fn style(s: &mut Scene, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
    let mut next = s.p.next_id;
    st::add_style(l, &mut Ids(&mut next), id, &s.comp.global_light, true).unwrap();
    s.p.next_id = next;
    for (k, v) in vals {
        set(l, &format!("{GROUP}/{id}/{k}"), v.clone());
    }
}

fn gradient() -> Value {
    Value::Gradient(Box::new(Gradient {
        colors: vec![(0.0, [1.0, 0.2, 0.1, 1.0]), (0.4, [0.1, 0.9, 0.3, 1.0]), (1.0, [0.2, 0.3, 1.0, 1.0])],
        color_midpoints: vec![0.3, 0.6],
        opacities: vec![(0.0, 1.0), (0.7, 0.4), (1.0, 0.9)],
        opacity_midpoints: vec![0.5, 0.2],
    }))
}

/// A blurred solid over footage, styled by `f`; compared on the CPU and the GPU, and the
/// styles must have run on the GPU.
fn case(label: &str, depth: BitDepth, o: RenderOpts, f: impl Fn(&mut Scene, &mut Layer)) {
    let mut s = Scene::new(depth);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.solid([0.8, 0.5, 0.2], 36, 26);
    s.effect(&mut l, "ec.blur.gaussian", &[("blurriness", Value::Scalar(6.0))]);
    set(&mut l, "transform/rotation", Value::Scalar(12.0));
    f(&mut s, &mut l);
    s.push(l);
    let before = crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed);
    let d = compare_at(&s, o, Tick::ZERO);
    if d.is_some() {
        assert!(crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed) > before, "{label}: styles ran on the CPU");
    }
    // Dissolve's grain flips where the GPU's coverage differs from the CPU's by a rounding step.
    check(label, d, 0.002);
}

#[test]
fn exterior_passes_match_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        case(&format!("drop shadow + outer glow {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "dropShadow", &[("size", Value::Scalar(9.0)), ("distance", Value::Scalar(5.0)), ("blendMode", Value::Enum(MULTIPLY))]);
            style(s, l, "outerGlow", &[("size", Value::Scalar(8.0)), ("blendMode", Value::Enum(SCREEN)), ("range", Value::Scalar(30.0))]);
        });
        case(&format!("noisy shadows, gradient glow {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "dropShadow", &[("size", Value::Scalar(4.0)), ("noise", Value::Scalar(40.0)), ("knocksOut", Value::Bool(false))]);
            style(
                s,
                l,
                "outerGlow",
                &[
                    ("size", Value::Scalar(12.0)),
                    ("colorType", Value::Enum(1)),
                    ("colors", gradient()),
                    ("jitter", Value::Scalar(30.0)),
                    ("noise", Value::Scalar(20.0)),
                ],
            );
        });
        case(&format!("dissolve shadow {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "dropShadow", &[("size", Value::Scalar(0.0)), ("distance", Value::Scalar(7.0)), ("blendMode", Value::Enum(DISSOLVE))]);
        });
    }
}

#[test]
fn interior_styles_match_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for g in 0..5 {
            case(&format!("gradient overlay style {g} {depth:?}"), depth, opts(), |s, l| {
                style(
                    s,
                    l,
                    "gradientOverlay",
                    &[
                        ("style", Value::Enum(g)),
                        ("colors", gradient()),
                        ("angle", Value::Scalar(30.0)),
                        ("reverse", Value::Bool(g == 2)),
                        ("alignWithLayer", Value::Bool(g % 2 == 0)),
                    ],
                );
            });
        }
        case(&format!("overlays, satin, inner glow and shadow {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "colorOverlay", &[("opacity", Value::Scalar(40.0)), ("blendMode", Value::Enum(MULTIPLY))]);
            style(s, l, "satin", &[("invert", Value::Bool(true))]);
            style(s, l, "innerGlow", &[("size", Value::Scalar(6.0)), ("source", Value::Enum(0)), ("noise", Value::Scalar(10.0))]);
            style(s, l, "innerShadow", &[("size", Value::Scalar(5.0)), ("noise", Value::Scalar(25.0)), ("blendMode", Value::Enum(NORMAL))]);
        });
        case(&format!("gradient inner glow, dissolve overlay {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "colorOverlay", &[("opacity", Value::Scalar(60.0)), ("blendMode", Value::Enum(DISSOLVE))]);
            style(s, l, "innerGlow", &[("size", Value::Scalar(9.0)), ("colorType", Value::Enum(1)), ("colors", gradient()), ("jitter", Value::Scalar(20.0))]);
        });
    }
}

#[test]
fn blending_options_half_and_linear_match_the_cpu() {
    let styled = |s: &mut Scene, l: &mut Layer| {
        style(s, l, "dropShadow", &[("size", Value::Scalar(6.0))]);
        style(s, l, "outerGlow", &[("size", Value::Scalar(5.0))]);
        style(s, l, "colorOverlay", &[("opacity", Value::Scalar(50.0))]);
    };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for (fill, group, knockout, channels) in [(0.6, false, 0, [true; 3]), (0.4, true, 1, [true, false, true]), (1.0, false, 2, [false, true, true])] {
            case(&format!("fill {fill} group {group} knockout {knockout} {depth:?}"), depth, opts(), |s, l| {
                styled(s, l);
                let adv = "blendingOptions/advancedBlending";
                set(l, &format!("{GROUP}/{adv}/fillOpacity"), Value::Scalar(fill * 100.0));
                set(l, &format!("{GROUP}/{adv}/blendInteriorAsGroup"), Value::Bool(group));
                set(l, &format!("{GROUP}/{adv}/knockout"), Value::Enum(knockout));
                for (c, on) in ["red", "green", "blue"].iter().zip(channels) {
                    set(l, &format!("{GROUP}/{adv}/{c}"), Value::Bool(on));
                }
                l.blend_mode = BlendMode::Overlay;
            });
        }
        case(&format!("half resolution {depth:?}"), depth, RenderOpts { scale: 0.5, ..opts() }, styled);
    }
    let mut s = Scene::new(BitDepth::Bpc32);
    s.p.settings.blend_linear = true;
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.solid([0.8, 0.5, 0.2], 36, 26);
    styled(&mut s, &mut l);
    s.push(l);
    check("linear blending", compare_at(&s, opts(), Tick::ZERO), 0.0);
}

#[test]
fn bevel_and_emboss_matches_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for (st, tech) in [(0, 0), (1, 0), (2, 1), (3, 2), (4, 0), (1, 2)] {
            case(&format!("bevel style {st} technique {tech} {depth:?}"), depth, opts(), |s, l| {
                style(
                    s,
                    l,
                    "bevelEmboss",
                    &[
                        ("style", Value::Enum(st)),
                        ("technique", Value::Enum(tech)),
                        ("size", Value::Scalar(7.0 + st as f64)),
                        ("depth", Value::Scalar(140.0)),
                        ("direction", Value::Enum(st % 2)),
                        ("useGlobalLight", Value::Bool(false)),
                        ("angle", Value::Scalar(120.0)),
                        ("altitude", Value::Scalar(30.0)),
                        ("highlightMode", Value::Enum(SCREEN)),
                        ("shadowMode", Value::Enum(MULTIPLY)),
                    ],
                );
            });
        }
        case(&format!("softened bevel over a gradient overlay {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "gradientOverlay", &[("colors", gradient()), ("alignWithLayer", Value::Bool(true))]);
            style(s, l, "bevelEmboss", &[("size", Value::Scalar(12.0)), ("soften", Value::Scalar(3.0)), ("highlightMode", Value::Enum(DISSOLVE))]);
        });
        case(&format!("bevel at half resolution {depth:?}"), depth, RenderOpts { scale: 0.5, ..opts() }, |s, l| {
            style(s, l, "bevelEmboss", &[("size", Value::Scalar(9.0))]);
        });
    }
}

/// Styles the GPU doesn't render (a distance field) still match: the layer goes to the CPU.
#[test]
fn distance_field_styles_fall_back_to_the_cpu() {
    for (id, vals) in [
        ("dropShadow", vec![("spread", Value::Scalar(40.0))]),
        ("outerGlow", vec![("technique", Value::Enum(1))]),
        ("bevelEmboss", vec![("size", Value::Scalar(400.0))]),
        ("stroke", vec![]),
    ] {
        let mut s = Scene::new(BitDepth::Bpc32);
        let bg = s.footage(97, 61);
        s.push(bg);
        let mut l = s.solid([0.8, 0.5, 0.2], 36, 26);
        style(&mut s, &mut l, id, &vals);
        s.push(l);
        check(id, compare_at(&s, opts(), Tick::ZERO), 0.0);
    }
}
