//! Gradient Fill text animator (EffectCraft extension): a smooth gradient per pixel across the
//! group of characters the selector picks, carried by the characters' animation, blended by the
//! selection and stacked with Fill Color animators in order; text without it draws as before.

use effectcraft_keyframe::{Gradient, TextDoc, Value};
use effectcraft_project::Project;
use effectcraft_time::Tick;

use super::tests::{alpha_bbox, animator, set, setup, text_layer};
use super::*;

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

fn red_to_blue() -> Value {
    Value::gradient(Gradient { colors: vec![(0.0, RED), (1.0, BLUE)], opacities: vec![(0.0, 1.0), (1.0, 1.0)], ..Default::default() })
}

/// "TESTING THIS ANIMATION" in white with a red → blue Gradient Fill on the word "THIS" (Range
/// Selector Based On Words, Index units 1..2), returned as (project, comp); layer 0.
fn project(extra: impl FnOnce(&mut Project, &mut Layer)) -> (Project, effectcraft_project::ItemId) {
    let (mut p, cid, comp) = setup(900, 160);
    let mut l = text_layer(&mut p, &comp, TextDoc { text: "TESTING THIS ANIMATION".into(), size: 70.0, fill: [1.0; 4], ..Default::default() });
    set(&mut l, "transform/position", Value::Vec3([30.0, 100.0, 0.0]));
    animator(&mut p, &mut l, &["gradientFill"], &["range"]);
    set(&mut l, "text/animators/#1/properties/gradientFillColors", red_to_blue());
    set(&mut l, "text/animators/#1/selectors/#1/advanced/basedOn", Value::Enum(2));
    set(&mut l, "text/animators/#1/selectors/#1/advanced/units", Value::Enum(1));
    set(&mut l, "text/animators/#1/selectors/#1/start", Value::Scalar(1.0));
    set(&mut l, "text/animators/#1/selectors/#1/end", Value::Scalar(2.0));
    set(&mut l, "text/animators/#1/selectors/#1/advanced/smoothness", Value::Scalar(0.0));
    extra(&mut p, &mut l);
    p.comp_mut(cid).unwrap().layers.push(l);
    (p, cid)
}

fn frame(p: &Project, cid: effectcraft_project::ItemId) -> crate::Image {
    crate::render_frame(p, cid, Tick::ZERO, 1.0)
}

/// A covered pixel painted by the gradient (white has green 1, red → blue has green 0).
fn graded(px: [f32; 4]) -> bool {
    px[3] > 0.99 && px[1] < 0.05
}

#[test]
fn a_smooth_gradient_runs_across_the_selected_word() {
    let (p, cid) = project(|_, _| {});
    let geom = text_geom(&ctx_at(&p, cid), &p.comp(cid).unwrap().layers[0]).unwrap();
    // Only THIS's four characters carry the gradient; the box is THIS's.
    let with: Vec<usize> = geom.glyphs.iter().filter(|g| g.fills.is_some()).map(|g| g.char_index).collect();
    assert_eq!(with, vec![8, 9, 10, 11]);
    assert_eq!(geom.grad_groups.len(), 1);
    assert_eq!(geom.grad_groups[0].chars, vec![8, 9, 10, 11]);

    let img = frame(&p, cid);
    let b = alpha_bbox(&img).unwrap();
    let y = (b[1] + b[3]) / 2;
    let row: Vec<(i64, [f32; 4])> = (0..img.width as i64).map(|x| (x, img.get(x, y))).filter(|(_, c)| c[3] > 0.99).collect();
    let g: Vec<(i64, [f32; 4])> = row.iter().copied().filter(|(_, c)| graded(*c)).collect();
    let white = row.iter().filter(|(_, c)| c[0] > 0.99 && c[1] > 0.99 && c[2] > 0.99).count();
    assert!(white > 50 && g.len() > 30, "white TESTING / ANIMATION and a graded THIS: {white} white, {} graded", g.len());
    // Left to right: red falls and blue rises, from nearly red to nearly blue.
    assert!(g.windows(2).all(|w| w[1].1[2] >= w[0].1[2] - 1e-4 && w[1].1[0] <= w[0].1[0] + 1e-4), "monotonic: {g:?}");
    assert!(g[0].1[0] > 0.85 && g[g.len() - 1].1[2] > 0.85, "ends: {:?} … {:?}", g[0], g[g.len() - 1]);
    // Smooth per pixel, not one colour per letter: many distinct steps.
    let mut steps: Vec<i32> = g.iter().map(|(_, c)| (c[2] * 255.0).round() as i32).collect();
    steps.dedup();
    assert!(steps.len() > 20, "smooth: {} steps", steps.len());
    // The graded pixels sit between the white words.
    let (first_white, last_white) = (row.iter().find(|(_, c)| c[1] > 0.99).unwrap().0, row.iter().rev().find(|(_, c)| c[1] > 0.99).unwrap().0);
    assert!(g[0].0 > first_white && g[g.len() - 1].0 < last_white);
}

fn ctx_at(p: &Project, cid: effectcraft_project::ItemId) -> EvalCtx<'_> {
    super::tests::ctx(p, cid, 0.0)
}

/// The colours of the graded pixels, by position (x, y).
fn graded_pixels(img: &crate::Image) -> Vec<(i64, i64, [f32; 4])> {
    let mut v = vec![];
    for y in 0..img.height as i64 {
        for x in 0..img.width as i64 {
            let c = img.get(x, y);
            if graded(c) {
                v.push((x, y, c));
            }
        }
    }
    v
}

#[test]
fn the_gradient_moves_and_turns_with_the_word() {
    let (still, cid) = project(|_, _| {});
    let base = graded_pixels(&frame(&still, cid));
    // Position animator on every character: the word slides 50 px right and 20 px down, the
    // colours with it (pixel for pixel).
    let (moved, cid) = project(|p, l| {
        animator(p, l, &["position"], &["range"]);
        set(l, "text/animators/#2/properties/position", Value::Vec3([50.0, 20.0, 0.0]));
    });
    let img = frame(&moved, cid);
    let mut n = 0;
    for (x, y, c) in base.iter().filter(|(_, _, c)| c[3] > 0.999) {
        let m = img.get(x + 50, y + 20);
        assert!((0..4).all(|d| (m[d] - c[d]).abs() < 1e-3), "({x},{y}) {c:?} moved {m:?}");
        n += 1;
    }
    assert!(n > 200);
    // Rotated 90° as a word (Anchor Point Grouping: Word): red now at the top, blue at the bottom.
    let (turned, cid) = project(|p, l| {
        set(l, "text/moreOptions/anchorGrouping", Value::Enum(1));
        animator(p, l, &["rotation"], &["range"]);
        set(l, "text/animators/#2/properties/rotation", Value::Scalar(90.0));
        let _ = p;
    });
    let g = graded_pixels(&frame(&turned, cid));
    assert!(g.len() > 200);
    let (y0, y1) = (g.iter().map(|p| p.1).min().unwrap(), g.iter().map(|p| p.1).max().unwrap());
    let avg = |lo: i64, hi: i64| {
        let s: Vec<&(i64, i64, [f32; 4])> = g.iter().filter(|p| p.1 >= lo && p.1 <= hi).collect();
        let n = s.len().max(1) as f32;
        [s.iter().map(|p| p.2[0]).sum::<f32>() / n, s.iter().map(|p| p.2[2]).sum::<f32>() / n]
    };
    let (top, bottom) = (avg(y0, y0 + (y1 - y0) / 5), avg(y1 - (y1 - y0) / 5, y1));
    assert!(top[0] > 0.7 && bottom[1] > 0.7, "top red {top:?}, bottom blue {bottom:?}");
    // The word stands upright: taller than wide.
    let (x0, x1) = (g.iter().map(|p| p.0).min().unwrap(), g.iter().map(|p| p.0).max().unwrap());
    assert!(y1 - y0 > 2 * (x1 - x0), "{x0}..{x1} × {y0}..{y1}");
}

#[test]
fn the_amount_blends_the_gradient_over_the_fill() {
    let (p, cid) = project(|_, l| set(l, "text/animators/#1/selectors/#1/advanced/amount", Value::Scalar(50.0)));
    let img = frame(&p, cid);
    // White blended halfway with red → blue: green 0.5 everywhere on THIS.
    let half: Vec<[f32; 4]> = (0..img.height as i64)
        .flat_map(|y| (0..img.width as i64).map(move |x| (x, y)))
        .map(|(x, y)| img.get(x, y))
        .filter(|c| c[3] > 0.999 && (c[1] - 0.5).abs() < 0.02)
        .collect();
    assert!(half.len() > 200, "{} half-blended pixels", half.len());
    assert!(half.iter().all(|c| c[0] >= 0.49 && c[2] >= 0.49), "over white: {:?}", half.first());
}

#[test]
fn gradient_and_fill_color_animators_stack_in_order() {
    let green = Value::Color([0.0, 1.0, 0.0, 1.0]);
    // A Fill Color below the gradient overrides it (100 %): THIS is green.
    let (p, cid) = project(|p, l| {
        animator(p, l, &["fillColor"], &["range"]);
        set(l, "text/animators/#2/properties/fillColor", green.clone());
    });
    assert!(graded_pixels(&frame(&p, cid)).is_empty());
    let c = text_geom(&ctx_at(&p, cid), &p.comp(cid).unwrap().layers[0]).unwrap();
    let this = c.glyphs.iter().find(|g| g.char_index == 9).unwrap();
    assert_eq!(gradient_fill_at(this.fills.as_ref().unwrap(), &this.xf, [0.0, 0.0]), [0.0, 1.0, 0.0, 1.0]);
    // A Fill Color above it is overridden where the gradient selects: THIS is graded again, the
    // other words green.
    let (p, cid) = project(|p, l| {
        animator(p, l, &["fillColor"], &["range"]);
        set(l, "text/animators/#2/properties/fillColor", green.clone());
        let anims = l.props.group_mut("text/animators").unwrap();
        anims.children.swap(0, 1);
    });
    let img = frame(&p, cid);
    assert!(graded_pixels(&img).len() > 200);
    let greens = (0..img.width as i64).flat_map(|x| (0..img.height as i64).map(move |y| (x, y))).filter(|&(x, y)| {
        let c = img.get(x, y);
        c[3] > 0.99 && c[1] > 0.99 && c[0] < 0.01
    });
    assert!(greens.count() > 200);
}

#[test]
fn text_without_a_selected_gradient_draws_as_before() {
    // The gradient animator selecting nothing leaves the solid path: the same pixels, bit for
    // bit, as without it.
    let (none, cid) = project(|_, l| set(l, "text/animators/#1/selectors/#1/end", Value::Scalar(1.0)));
    let (mut plain, pid) = project(|_, _| {});
    plain.comp_mut(pid).unwrap().layers[0].props.group_mut("text/animators").unwrap().children.clear();
    let geom = text_geom(&ctx_at(&none, cid), &none.comp(cid).unwrap().layers[0]).unwrap();
    assert!(geom.glyphs.iter().all(|g| g.fills.is_none()) && geom.grad_groups.is_empty());
    let (a, b) = (frame(&none, cid), frame(&plain, pid));
    assert_eq!((a.width, a.height), (b.width, b.height));
    assert!(a.data == b.data, "bit-identical");
}

#[test]
fn radial_gradient_and_hostile_values_never_crash() {
    let (p, cid) = project(|_, l| {
        set(l, "text/animators/#1/properties/gradientType", Value::Enum(1));
        set(l, "text/animators/#1/properties/gradientStart", Value::Vec2([50.0, 50.0]));
        set(l, "text/animators/#1/properties/gradientEnd", Value::Vec2([100.0, 50.0]));
    });
    let geom = text_geom(&ctx_at(&p, cid), &p.comp(cid).unwrap().layers[0]).unwrap();
    let gr = &geom.grad_groups[0];
    let g = geom.glyphs.iter().find(|g| g.fills.is_some()).unwrap();
    let f = g.fills.as_ref().unwrap();
    // Red at the centre, blue at the radius.
    let centre = gradient_fill_at(f, &g.xf, gr.start);
    let edge = gradient_fill_at(f, &g.xf, gr.end);
    assert!(centre[0] > 0.99 && edge[2] > 0.99, "{centre:?} {edge:?}");
    // Start = End, NaN and huge points, and an empty gradient: drawn, no panic.
    for (s, e) in [([50.0, 50.0], [50.0, 50.0]), ([f64::NAN, 0.0], [1e300, f64::INFINITY])] {
        let (p, cid) = project(|_, l| {
            set(l, "text/animators/#1/properties/gradientStart", Value::Vec2(s));
            set(l, "text/animators/#1/properties/gradientEnd", Value::Vec2(e));
            set(l, "text/animators/#1/properties/gradientFillColors", Value::gradient(Gradient { colors: vec![], opacities: vec![], ..Default::default() }));
        });
        let _ = frame(&p, cid);
    }
}
