//! GPU Layer Styles (`fx_styles`) vs the CPU styles (the oracle): Drop Shadow (knockout, noise),
//! Softer Outer and Inner Glow (colour and gradient, jitter), Inner Shadow, Satin, Color and
//! Gradient Overlay (every style), Fill Opacity, Blend Interior Styles as Group, Knockout and
//! the channel switches, Bevel and Emboss (every style and technique, Soften, Dissolve), Stroke
//! (every position, Dissolve, Fill Opacity), Spread / Choke and Precise glows (GLS1: the
//! distance field shared with Bevel and Emboss), at 8 and 32 bpc and Half, with linear
//! blending, and a layer whose Gaussian Blur makes the styles' content.

use std::sync::atomic::Ordering;

use effectcraft_color::BlendMode;
use effectcraft_keyframe::ShapePath;
use effectcraft_keyframe::{Gradient, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{BitDepth, Layer, LayerSource, MaskMode};
use effectcraft_render::{Backend, LayerTimes, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::tests::{Pattern, Scene, check, compare_at, gpu, opts, set, v3};

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

#[test]
fn stroke_matches_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for (pos, size, mode) in [(0, 3.0, NORMAL), (1, 2.4, NORMAL), (2, 5.0, MULTIPLY), (1, 7.5, SCREEN), (0, 1.0, DISSOLVE), (2, 0.6, NORMAL)] {
            case(&format!("stroke position {pos} size {size} mode {mode} {depth:?}"), depth, opts(), |s, l| {
                style(
                    s,
                    l,
                    "stroke",
                    &[
                        ("size", Value::Scalar(size)),
                        ("position", Value::Enum(pos)),
                        ("blendMode", Value::Enum(mode)),
                        ("opacity", Value::Scalar(70.0)),
                        ("color", Value::Color([0.2, 0.9, 1.0, 1.0])),
                    ],
                );
            });
        }
        case(&format!("stroke over overlays with fill opacity {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "colorOverlay", &[("opacity", Value::Scalar(50.0))]);
            style(s, l, "stroke", &[("size", Value::Scalar(4.0)), ("position", Value::Enum(2))]);
            set(l, &format!("{GROUP}/blendingOptions/advancedBlending/fillOpacity"), Value::Scalar(40.0));
        });
        case(&format!("stroke at half resolution {depth:?}"), depth, RenderOpts { scale: 0.5, ..opts() }, |s, l| {
            style(s, l, "stroke", &[("size", Value::Scalar(5.0)), ("position", Value::Enum(1))]);
        });
        // Stroke and Bevel and Emboss read one field, exact to the wider of the two.
        case(&format!("wide stroke under a narrow bevel {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "stroke", &[("size", Value::Scalar(14.0)), ("position", Value::Enum(0))]);
            style(s, l, "bevelEmboss", &[("size", Value::Scalar(4.0)), ("style", Value::Enum(4))]);
        });
    }
}

#[test]
fn spread_choke_and_precise_glows_match_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        case(&format!("drop shadow spread, inner shadow choke {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "dropShadow", &[("size", Value::Scalar(10.0)), ("spread", Value::Scalar(40.0)), ("distance", Value::Scalar(4.0))]);
            style(s, l, "innerShadow", &[("size", Value::Scalar(8.0)), ("choke", Value::Scalar(50.0)), ("noise", Value::Scalar(15.0))]);
        });
        case(&format!("full spread: a hard shadow {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "dropShadow", &[("size", Value::Scalar(6.0)), ("spread", Value::Scalar(100.0)), ("knocksOut", Value::Bool(false))]);
        });
        case(&format!("softer glows with spread and choke {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "outerGlow", &[("size", Value::Scalar(9.0)), ("spread", Value::Scalar(50.0)), ("blendMode", Value::Enum(SCREEN))]);
            style(s, l, "innerGlow", &[("size", Value::Scalar(7.0)), ("choke", Value::Scalar(30.0)), ("source", Value::Enum(0))]);
        });
        case(&format!("precise glows {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "outerGlow", &[("size", Value::Scalar(8.0)), ("technique", Value::Enum(1)), ("spread", Value::Scalar(20.0))]);
            style(s, l, "innerGlow", &[("size", Value::Scalar(6.0)), ("technique", Value::Enum(1)), ("choke", Value::Scalar(35.0))]);
        });
        case(&format!("precise gradient glows {depth:?}"), depth, opts(), |s, l| {
            style(s, l, "outerGlow", &[("size", Value::Scalar(11.0)), ("technique", Value::Enum(1)), ("colorType", Value::Enum(1)), ("colors", gradient())]);
            style(
                s,
                l,
                "innerGlow",
                &[("size", Value::Scalar(5.0)), ("technique", Value::Enum(1)), ("source", Value::Enum(0)), ("noise", Value::Scalar(20.0))],
            );
        });
        case(&format!("precise glow at half resolution {depth:?}"), depth, RenderOpts { scale: 0.5, ..opts() }, |s, l| {
            style(s, l, "outerGlow", &[("size", Value::Scalar(10.0)), ("technique", Value::Enum(1))]);
            style(s, l, "dropShadow", &[("size", Value::Scalar(8.0)), ("spread", Value::Scalar(30.0))]);
        });
    }
}

/// Elias's HV1 03 reel cards (28 Oct, 26-10 HOOK V1 / HV1 03 Content): a portrait video
/// footage layer with a rounded-rect mask, parented to a null that scales the wall, and one
/// style, a white Inside Stroke of 2.4 px at 32 %. The GPU walk handed every one to the CPU
/// ("HV1 03 Reel 1 Cat vlog rendered on the CPU (layer styles the GPU doesn't render) · 729
/// ms"): Stroke needed the distance field only the CPU computed. Now it renders on the GPU, with
/// no CPU note for the layer or the frame, and matches the CPU.
#[test]
fn reel_card_stroke_renders_on_the_gpu_like_the_cpu() {
    let reel_scene = |depth: BitDepth| {
        let mut s = Scene::new(depth);
        let bg = s.solid([0.03, 0.05, 0.09], 97, 61);
        s.push(bg);
        let mut null = build::layer(&mut s.p, &s.comp, "HV1 03 Wall CTRL", LayerSource::Null, (100, 100), None);
        set(&mut null, "transform/position", v3(48.0, 30.0));
        set(&mut null, "transform/scale", Value::Vec3([70.0, 70.0, 100.0]));
        let nid = null.id;
        let mut reel = s.footage(36, 64);
        reel.parent = Some(nid);
        set(&mut reel, "transform/position", v3(50.0, 50.0));
        let mut next = s.p.next_id;
        let rounded = ShapePath::rect([18.0, 32.0], 36.0, 64.0);
        let m = build::mask(&mut Ids(&mut next), "Mask 1", rounded, MaskMode::Add, [177, 180, 79]);
        s.p.next_id = next;
        reel.props.sub_mut("masks").unwrap().children.push(m.into());
        style(
            &mut s,
            &mut reel,
            "stroke",
            &[
                ("color", Value::Color([1.0, 1.0, 1.0, 1.0])),
                ("blendMode", Value::Enum(NORMAL)),
                ("size", Value::Scalar(2.4)),
                ("opacity", Value::Scalar(32.0)),
                ("position", Value::Enum(1)),
            ],
        );
        let rid = s.push(reel);
        s.push(null);
        (s, rid)
    };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let (s, rid) = reel_scene(depth);
        let before = crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed);
        let d = compare_at(&s, opts(), Tick::ZERO);
        if d.is_some() {
            assert!(crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed) > before, "{depth:?}: the reel's stroke ran on the CPU");
        }
        check(&format!("reel card stroke {depth:?}"), d, 0.0);
        check(&format!("reel card stroke {depth:?} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
        let Some(g) = gpu() else { return };
        let times = LayerTimes::default();
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
        r.accel = Some(g);
        r.times = Some(&times);
        g.render(&r, s.cid, Tick::ZERO).expect("the GPU renders the frame");
        assert_eq!(times.frame_cpu(), None, "{depth:?}: the frame ran on the GPU");
        assert_eq!(times.cpu_reason(s.cid, rid), None, "{depth:?}: the stroked reel ran on the GPU");
    }
}

/// The widest styles still match (Bevel and Emboss and Stroke at their 250 px cap, and a glow's
/// full spread): on the GPU within the field, else on the CPU.
#[test]
fn widest_distance_field_styles_match_the_cpu() {
    for (id, vals) in [
        ("dropShadow", vec![("spread", Value::Scalar(40.0))]),
        ("outerGlow", vec![("technique", Value::Enum(1))]),
        ("bevelEmboss", vec![("size", Value::Scalar(400.0))]),
        ("stroke", vec![]),
        ("stroke", vec![("size", Value::Scalar(250.0))]),
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
