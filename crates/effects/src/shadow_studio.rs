//! Clean-room native equivalents for the directional Shadow Studio 2/3 instances in SceneYard.

use effectcraft_project::ParamUi;
use effectcraft_raster::{Image, gaussian_blur};
use rayon::prelude::*;

use crate::{Buf, EffectCtx, EffectSpec, col, num, p, slider};

/// Shadow Studio's steps for one buffer, from its controls (shared by the CPU render below and
/// the GPU kernels, `effectcraft-gpu` `fx_shadowstudio`, so both convert them the same way).
pub struct ShadowStudioPlan {
    /// Transparent pixels added on the left, top, right and bottom (0 on an adjustment layer).
    pub pad: [u32; 4],
    /// The padded buffer's size.
    pub size: [u32; 2],
    /// The shadow colour (straight, its alpha already in the steps' opacities).
    pub color: [f32; 4],
    pub source_opacity: f32,
    /// Source pixels at or below this alpha cast nothing.
    pub alpha_threshold: f32,
    /// The cast's Gaussian σ is half of this (0: no blur).
    pub softness: f64,
    /// The cast's steps, nearest first: (x, y shift in pixels, opacity), only those with
    /// opacity above 0. Each pixel keeps the most opaque step that reaches it.
    pub steps: Vec<(i64, i64, f32)>,
}

/// The plan for a `width` × `height` buffer at `scale` (Shadow Studio 3 when `v3`, else 2);
/// `None` when the effect leaves the buffer as it is (non-finite controls, an empty buffer, or
/// a padded raster past 32 Mpx).
pub fn shadow_studio_plan(ctx: &EffectCtx, width: u32, height: u32, scale: f64, v3: bool) -> Option<ShadowStudioPlan> {
    let id = |v2, v3_id| if v3 { v3_id } else { v2 };
    let angle = ctx.params.f(id("PESS2-0002", "PESS3-0002"));
    let distance = (ctx.params.f(id("PESS2-0004", "PESS3-0004")) * scale).clamp(0.0, 2048.0);
    let softness = if v3 { ctx.params.f("PESS3-0076") * ctx.params.f("PESS3-0075") / 100.0 } else { ctx.params.f("PESS2-0005") };
    let softness = (softness * scale).clamp(0.0, 512.0);
    if !angle.is_finite() || !distance.is_finite() || !softness.is_finite() || width == 0 || height == 0 {
        return None;
    }
    let color = ctx.params.color(id("PESS2-0006", "PESS3-0006"));
    let start = (ctx.params.f(id("PESS2-0008", "PESS3-0086")) / 100.0).clamp(0.0, 1.0) as f32;
    let end = (ctx.params.f(id("PESS2-0009", "PESS3-0087")) / 100.0).clamp(0.0, 1.0) as f32;
    let ease = if v3 { (ctx.params.f("PESS3-0084") / 50.0).clamp(0.1, 2.0) as f32 } else { 1.0 };
    let strength = (ctx.params.f(id("PESS2-0044", "PESS3-0038")) / 100.0).clamp(0.0, 1.0) as f32;
    let source_opacity = (ctx.params.f(id("PESS2-0027", "PESS3-0026")) / 100.0).clamp(0.0, 1.0) as f32;
    let offset = ctx.params.f(id("PESS2-0028", "PESS3-0027")).clamp(0.0, 4.0);
    let alpha_threshold = if v3 { ctx.params.f("PESS3-0078").clamp(0.0, 1.0) as f32 } else { 0.0 };
    if !color.iter().all(|v| v.is_finite())
        || ![start, end, ease, strength, source_opacity, alpha_threshold].iter().all(|v| v.is_finite())
        || !offset.is_finite()
    {
        return None;
    }
    let (sin, cos) = angle.to_radians().sin_cos();
    let (dx, dy) = (sin * distance * offset, -cos * distance * offset);
    let feather = (softness * 1.5).ceil() as u32 + 1;
    let pad = if ctx.adjustment {
        [0, 0, 0, 0]
    } else {
        [
            (-dx).max(0.0).ceil() as u32 + feather,
            (-dy).max(0.0).ceil() as u32 + feather,
            dx.max(0.0).ceil() as u32 + feather,
            dy.max(0.0).ceil() as u32 + feather,
        ]
    };
    let w = width.checked_add(pad[0])?.checked_add(pad[2])?;
    let h = height.checked_add(pad[1])?.checked_add(pad[3])?;
    // ponytail: one bounded raster per effect; tile the long-shadow pass if larger frames matter.
    if u64::from(w) * u64::from(h) > 32_000_000 {
        return None;
    }
    let samples = ctx.params.f(id("PESS2-0022", "PESS3-0022"));
    let count = distance.ceil().min(if samples.is_finite() && samples > 0.0 { samples } else { 36.0 }).clamp(1.0, 512.0) as usize;
    let steps = (1..=count)
        .filter_map(|step| {
            let t = step as f64 / count as f64;
            let opacity = (start + (end - start) * (t as f32).powf(ease)) * strength * color[3];
            (opacity > 0.0).then(|| ((dx * t).round() as i64, (dy * t).round() as i64, opacity))
        })
        .collect();
    Some(ShadowStudioPlan { pad, size: [w, h], color, source_opacity, alpha_threshold, softness, steps })
}

fn shadow(ctx: &EffectCtx, mut b: Buf, v3: bool) -> Buf {
    let Some(plan) = shadow_studio_plan(ctx, b.img.width, b.img.height, b.scale, v3) else { return b };
    let ShadowStudioPlan { pad: [left, top, _, _], size: [w, h], color, source_opacity, alpha_threshold, softness, steps } = plan;
    let src = b.img;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (src.width, src.height, 0, 0);
    for y in 0..src.height {
        for x in 0..src.width {
            if src.get(i64::from(x), i64::from(y))[3] > alpha_threshold {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    let mut cast = Image::new(w, h);
    // Each step stamps the source's alpha shifted along the shadow, keeping the strongest stamp
    // per pixel. Output rows run in parallel; within a row the steps run in order, so each pixel
    // sees the same candidates in the same order as stamping step by step over the whole image.
    let sw = src.width as usize;
    if min_x <= max_x && min_y <= max_y {
        cast.rows_mut().for_each(|(py, row)| {
            for &(sx, sy, opacity) in &steps {
                let y = py as i64 - i64::from(top) - sy;
                if y < i64::from(min_y) || y > i64::from(max_y) {
                    continue;
                }
                let Some(srow) = src.data.get(y as usize * sw..(y as usize + 1) * sw) else { continue };
                for x in min_x..=max_x {
                    let Some(source) = srow.get(x as usize) else { break };
                    let source_alpha = source[3];
                    if source_alpha <= alpha_threshold {
                        continue;
                    }
                    let a = source_alpha * opacity;
                    let px = i64::from(x) + i64::from(left) + sx;
                    if px < 0 {
                        continue;
                    }
                    if let Some(out) = row.get_mut(px as usize)
                        && a > out[3]
                    {
                        *out = [color[0] * a, color[1] * a, color[2] * a, a];
                    }
                }
            }
        });
    }
    if softness > 0.0 {
        cast = gaussian_blur(&cast, softness * 0.5, softness * 0.5, false);
    }
    for y in 0..src.height {
        for x in 0..src.width {
            let original = src.get(i64::from(x), i64::from(y));
            let index = (y + top) as usize * w as usize + (x + left) as usize;
            if let Some(out) = cast.data.get_mut(index) {
                let source = original.map(|v| v * source_opacity);
                let behind = 1.0 - source[3];
                for c in 0..3 {
                    out[c] = source[c] + out[c] * behind;
                }
                out[3] = source[3] + out[3] * behind;
            }
        }
    }
    b.img = cast;
    b.offset[0] += f64::from(left);
    b.offset[1] += f64::from(top);
    b
}

fn v2(ctx: &EffectCtx, b: Buf) -> Buf {
    shadow(ctx, b, false)
}
fn v3(ctx: &EffectCtx, b: Buf) -> Buf {
    shadow(ctx, b, true)
}

pub fn specs() -> Vec<EffectSpec> {
    vec![
        EffectSpec {
            id: "ec.perspective.shadowstudio2",
            name: "Shadow Studio 2 (Native)",
            category: "Perspective",
            gpu: false,
            float: true,
            render: v2,
            params: vec![
                p("PESS2-0002", "Angle", num(135.0), ParamUi::Angle),
                p("PESS2-0004", "Distance", num(400.0), slider(0.0, 2048.0, 0.0, 1000.0, 0)),
                p("PESS2-0005", "Softness", num(250.0), slider(0.0, 512.0, 0.0, 300.0, 0)),
                p("PESS2-0006", "Color", col(0.0, 0.0, 0.0), ParamUi::Color),
                p("PESS2-0008", "Opacity Start", num(20.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS2-0009", "Opacity End", num(50.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS2-0044", "Opacity Master", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS2-0022", "Samples", num(36.0), slider(1.0, 512.0, 1.0, 128.0, 0)),
                p("PESS2-0027", "Source Opacity", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS2-0028", "Offset Shadow", num(1.0), slider(0.0, 4.0, 0.0, 2.0, 2)),
            ],
        },
        EffectSpec {
            id: "ec.perspective.shadowstudio3",
            name: "Shadow Studio 3 (Native)",
            category: "Perspective",
            gpu: false,
            float: true,
            render: v3,
            params: vec![
                p("PESS3-0002", "Light Direction", num(135.0), ParamUi::Angle),
                p("PESS3-0004", "Shadow Length", num(200.0), slider(0.0, 2048.0, 0.0, 1000.0, 0)),
                p("PESS3-0075", "Light Softness", num(20.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0076", "Light Radius", num(200.0), slider(0.0, 1000.0, 0.0, 500.0, 0)),
                p("PESS3-0006", "Color", col(0.0, 0.0, 0.0), ParamUi::Color),
                p("PESS3-0086", "Opacity Start", num(45.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0087", "Opacity End", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0084", "Ease Opacity", num(60.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0038", "Shadow Opacity", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0022", "Samples", num(36.0), slider(1.0, 512.0, 1.0, 128.0, 0)),
                p("PESS3-0026", "Source Opacity", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PESS3-0027", "Shadow Offset", num(1.0), slider(0.0, 4.0, 0.0, 2.0, 2)),
                p("PESS3-0078", "Alpha Threshold", num(0.75), slider(0.0, 1.0, 0.0, 1.0, 2)),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectEnv, run_fx};
    use effectcraft_keyframe::Value;

    #[test]
    fn directional_shadow_preserves_source_and_responds_to_controls() {
        assert_eq!(crate::lookup("PESS2").map(|s| s.id), Some("ec.perspective.shadowstudio2"));
        assert_eq!(crate::lookup("PESS3").map(|s| s.id), Some("ec.perspective.shadowstudio3"));
        let mut img = Image::new(12, 12);
        img.set(5, 5, [1.0, 1.0, 1.0, 1.0]);
        let run = |id, vals: &[(&str, Value)]| run_fx(id, vals, img.clone(), 0.0, EffectEnv::default());
        let a = run("ec.perspective.shadowstudio2", &[("PESS2-0002", num(90.0)), ("PESS2-0004", num(8.0)), ("PESS2-0005", num(0.0))]);
        let at = |b: &Buf, x: i64, y: i64| b.img.get(x + b.offset[0] as i64, y + b.offset[1] as i64);
        assert_eq!(at(&a, 5, 5)[0], 1.0);
        assert!(at(&a, 10, 5)[3] > 0.0 && at(&a, 10, 5)[0] == 0.0);
        let b = run("ec.perspective.shadowstudio3", &[("PESS3-0002", num(90.0)), ("PESS3-0004", num(8.0)), ("PESS3-0075", num(0.0))]);
        assert!(at(&b, 10, 5)[3] > 0.0);
        let hidden =
            run("ec.perspective.shadowstudio3", &[("PESS3-0002", num(90.0)), ("PESS3-0004", num(8.0)), ("PESS3-0075", num(0.0)), ("PESS3-0038", num(0.0))]);
        assert_eq!(at(&hidden, 10, 5)[3], 0.0);
        let end = [("PESS3-0002", num(90.0)), ("PESS3-0004", num(8.0)), ("PESS3-0075", num(0.0)), ("PESS3-0087", num(50.0))];
        let full_offset = run("ec.perspective.shadowstudio3", &end);
        let short_offset = run("ec.perspective.shadowstudio3", &[end[0].clone(), end[1].clone(), end[2].clone(), end[3].clone(), ("PESS3-0027", num(0.8))]);
        assert!(at(&full_offset, 13, 5)[3] > at(&short_offset, 13, 5)[3], "Shadow Offset 0.8 must shorten the cast");
        let mut faint = Image::new(12, 12);
        faint.set(5, 5, [0.5; 4]);
        let threshold = |value| {
            run_fx(
                "ec.perspective.shadowstudio3",
                &[("PESS3-0002", num(90.0)), ("PESS3-0004", num(8.0)), ("PESS3-0075", num(0.0)), ("PESS3-0078", num(value))],
                faint.clone(),
                0.0,
                EffectEnv::default(),
            )
        };
        assert_eq!(at(&threshold(0.75), 10, 5)[3], 0.0);
        assert!(at(&threshold(0.25), 10, 5)[3] > 0.0);
    }
}
