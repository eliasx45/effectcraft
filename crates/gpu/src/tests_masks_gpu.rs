//! Mask feathers on the GPU (MASK-GPU1) against the CPU: the blur of a coverage plane alone
//! (Gaussian and Linear falloffs, crops of a padded plane) and whole frames whose layers carry
//! feathered masks in every mode, inverted, with opacity and expansion.

use effectcraft_color::Label;
use effectcraft_keyframe::{ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, FeatherFalloff, GroupKind, ItemKind, Layer, MaskMode, PropGroup};
use effectcraft_render::Accelerator;
use effectcraft_render::masks::FeatherBlur;
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{self, Scene, check, compare_at, opts};

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
