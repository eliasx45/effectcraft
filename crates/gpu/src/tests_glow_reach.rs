//! Glow on a Continuously Rasterized shape layer is sized in comp pixels, like After Effects:
//! scaling the layer up (2D or 3D) must not widen the glow on screen (AE: ~92 px of reach at
//! radius 100 for scale 100, 283 and 800 %). The GPU must agree with the CPU.

use effectcraft_color::Label;
use effectcraft_keyframe::Value;
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemKind, LayerSource, Project};
use effectcraft_render::{RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Pattern, Scene, check, compare_at, n, set};

const W: u32 = 320;
const H: u32 = 180;

/// A 4 px white dot at the comp centre, scaled `scale` %, with a radius-20 Glow.
fn dot(three_d: bool, scale: f64) -> Scene {
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc32;
    let comp = Comp::new(W, H, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let mut s = Scene { p, cid, comp };
    let mut l = build::layer(&mut s.p, &s.comp, "Dot", LayerSource::Shape, (W, H), None);
    let mut next = s.p.next_id;
    let mut ids = Ids(&mut next);
    let e = build::shape_ellipse(&mut ids, [4.0, 4.0], [0.0, 0.0]);
    let fill = build::shape_fill(&mut ids, [1.0, 1.0, 1.0, 1.0]);
    let g = build::shape_group(&mut ids, "Dot", vec![e, fill]);
    s.p.next_id = next;
    l.props.sub_mut("contents").unwrap().children.push(g.into());
    l.switches.three_d = three_d;
    l.switches.collapse = true;
    set(&mut l, "transform/anchor", Value::Vec3([0.0, 0.0, 0.0]));
    set(&mut l, "transform/position", Value::Vec3([W as f64 / 2.0, H as f64 / 2.0, 0.0]));
    set(&mut l, "transform/scale", Value::Vec3([scale, scale, 100.0]));
    s.effect(&mut l, "ec.stylize.glow", &[("radius", n(20.0)), ("intensity", n(4.0)), ("threshold", n(10.0))]);
    s.push(l);
    s
}

/// How far the glow reaches past the dot's edge along the centre row, in comp pixels.
fn reach(s: &Scene, scale: f64) -> f64 {
    let img = Renderer::new(&s.p, &Pattern, RenderOpts::default()).comp_frame(s.cid, Tick::ZERO);
    let row = (H / 2 * W) as usize;
    let lit = (W / 2..W).take_while(|&x| img.data[row + x as usize][3] > 0.02).count();
    lit as f64 - 0.02 * scale
}

#[test]
fn a_glow_on_a_continuously_rasterized_shape_keeps_its_reach_in_comp_pixels_at_any_scale() {
    for three_d in [false, true] {
        let reaches: Vec<f64> = [100.0, 283.0, 800.0].iter().map(|&k| reach(&dot(three_d, k), k)).collect();
        assert!(reaches[0] > 5.0, "3D {three_d}: the glow shows ({reaches:?})");
        let spread = reaches.iter().cloned().fold(f64::MIN, f64::max) - reaches.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread <= 3.0, "3D {three_d}: glow reach must not grow with layer scale: {reaches:?}");
    }
}

#[test]
fn a_comp_pixel_glow_on_a_scaled_shape_matches_the_cpu_on_the_gpu() {
    for three_d in [false, true] {
        let s = dot(three_d, 283.0);
        check(&format!("glow reach 3D {three_d}"), compare_at(&s, RenderOpts::default(), Tick::ZERO), 0.0);
    }
}
