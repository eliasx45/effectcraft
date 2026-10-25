//! The text raster plan, walked on the CPU as the accelerator's kernels walk it (coverage from
//! the scan edges, solid colour composited Normal in order), gives `text::render`'s pixels:
//! whole and windowed, fills and strokes, runs and animated characters.

use effectcraft_color::BlendMode;
use effectcraft_keyframe::{TextDoc, Value};
use effectcraft_path::FillRule;
use effectcraft_raster::Image;

use super::*;
use crate::shapes::skia_scan::tests::coverage;
use crate::text::tests::{animator, ctx, set, setup, text_layer};

/// `shapes.wgsl`'s `sh_row` + `paint_px`, on the CPU.
fn walk(plan: &RasterPlan) -> Image {
    let mut img = Image::new(plan.width, plan.height);
    for d in &plan.draws {
        let [x0, y0, w, h] = d.rect;
        assert!(x0 + w <= plan.width && y0 + h <= plan.height, "draw inside the buffer");
        let cov = coverage(&d.edges, w, h, FillRule::NonZero);
        let PlanPaint::Solid(col) = d.paint.clone() else { panic!("text paints solid colour") };
        for y in 0..h {
            for x in 0..w {
                let al = cov[(y * w + x) as usize] * d.opacity * col[3];
                if al <= 0.0 {
                    continue;
                }
                let px = &mut img.data[((y0 + y) * plan.width + x0 + x) as usize];
                *px = effectcraft_color::blend_pixel(BlendMode::Normal, *px, [col[0] * al, col[1] * al, col[2] * al, al], 0.5);
            }
        }
    }
    img
}

fn same(label: &str, want: &Image, got: &Image) {
    assert_eq!((want.width, want.height), (got.width, got.height), "{label}: size");
    let mut near = 0;
    for (i, (a, b)) in want.data.iter().zip(&got.data).enumerate() {
        let d = (0..4).map(|c| (a[c] - b[c]).abs()).fold(0.0f32, f32::max);
        // One coverage level where a run's outlines cross themselves (see `skia_scan`).
        assert!(d <= 1.5 / 255.0, "{label}: pixel {i} {a:?} vs {b:?}");
        near += usize::from(d > 1e-6);
    }
    assert!(near * 100 <= want.data.len(), "{label}: {near} of {} pixels a level off", want.data.len());
}

#[test]
fn walked_text_plans_give_text_renders_pixels() {
    let (mut p, cid, comp) = setup(240, 120);
    let title = TextDoc {
        text: "Spotlight".into(),
        size: 34.0,
        fill: [0.95, 0.9, 0.8, 1.0],
        stroke: [0.1, 0.2, 0.7, 1.0],
        stroke_width: 3.0,
        apply_stroke: true,
        faux_bold: true,
        ..Default::default()
    };
    let mut l = text_layer(&mut p, &comp, title);
    set(&mut l, "transform/position", Value::Vec3([20.3, 60.6, 0.0]));
    animator(&mut p, &mut l, &["position", "rotation", "opacity", "fillColor"], &["range"]);
    set(&mut l, "text/animators/#1/properties/position", Value::Vec3([0.0, -14.0, 0.0]));
    set(&mut l, "text/animators/#1/properties/rotation", Value::Scalar(30.0));
    set(&mut l, "text/animators/#1/properties/opacity", Value::Scalar(35.0));
    set(&mut l, "text/animators/#1/properties/fillColor", Value::Color([0.9, 0.3, 0.1, 1.0]));
    set(&mut l, "text/animators/#1/selectors/#1/end", Value::Scalar(45.0));
    set(&mut l, "text/animators/#1/selectors/#1/offset", Value::Scalar(20.0));
    p.comp_mut(cid).unwrap().layers.push(l);

    let plain = TextDoc { text: "ffiAWAV".into(), size: 28.0, tracking: -60.0, ..Default::default() };
    let mut l = text_layer(&mut p, &comp, plain);
    set(&mut l, "transform/position", Value::Vec3([10.0, 100.0, 0.0]));
    p.comp_mut(cid).unwrap().layers.push(l);

    let line = TextDoc {
        text: "Wavy line".into(),
        size: 24.0,
        fill: [0.2, 0.8, 0.5, 0.85],
        stroke: [0.9, 0.2, 0.3, 1.0],
        stroke_width: 4.0,
        apply_stroke: true,
        faux_italic: true,
        ..Default::default()
    };
    let mut l = text_layer(&mut p, &comp, line);
    set(&mut l, "transform/position", Value::Vec3([12.0, 40.0, 0.0]));
    set(&mut l, "text/moreOptions/fillStroke", Value::Enum(1));
    animator(&mut p, &mut l, &["scale", "strokeWidth"], &["range"]);
    set(&mut l, "text/animators/#1/properties/scale", Value::Vec3([140.0, 70.0, 100.0]));
    set(&mut l, "text/animators/#1/properties/strokeWidth", Value::Scalar(1.5));
    set(&mut l, "text/animators/#1/selectors/#1/end", Value::Scalar(60.0));
    p.comp_mut(cid).unwrap().layers.push(l);

    let c = ctx(&p, cid, 0.0);
    for (li, layer) in c.comp.layers.iter().enumerate() {
        let geom = text_geom(&c, layer).expect("laid out");
        for s in [1.0, 0.5, 1.7] {
            let label = format!("layer {li} scale {s}");
            let cpu = super::super::render(&c, layer, s);
            let whole = plan(Some(&geom), s, None).expect("plans");
            assert!(!whole.draws.is_empty(), "{label}: draws");
            assert_eq!(whole.offset, cpu.offset, "{label}: offset");
            same(&label, &cpu.img, &walk(&whole));
            // Windows inside, across the right and bottom edges, and past them.
            let (w, h) = (cpu.img.width, cpu.img.height);
            for win in [[3, 2, w.saturating_sub(7).max(1), h.saturating_sub(5).max(1)], [w / 2, h / 3, w, h], [w + 3, 1, 9, 9]] {
                let label = format!("{label} window {win:?}");
                let got = plan(Some(&geom), s, Some(win)).expect("plans");
                assert_eq!(got.offset, [cpu.offset[0] - f64::from(win[0]), cpu.offset[1] - f64::from(win[1])], "{label}: offset");
                let want = cpu.img.crop(i64::from(win[0]), i64::from(win[1]), win[2], win[3]);
                same(&label, &want, &walk(&got));
            }
        }
    }
    // No text: the 1 × 1 empty buffer.
    let empty = plan(None, 1.0, None).expect("plans");
    assert_eq!((empty.width, empty.height, empty.draws.len()), (1, 1, 0));
}
