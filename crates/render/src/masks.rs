//! Layer masks: path coverage with expansion, feather, opacity, invert, combined by mode.

use effectcraft_effects::Buf;
use effectcraft_geom::{Mat3, vec2};
use effectcraft_keyframe::Value;
use effectcraft_path::{FillRule, StrokeStyle};
use effectcraft_project::{FeatherFalloff, GroupKind, Layer, MaskMode, MaskMotionBlur};
use effectcraft_raster::{Mask, box_blur_plane, gaussian_blur_plane};
use rayon::prelude::*;

use crate::eval::EvalCtx;

/// What mask motion blur may do in a render (from the renderer's motion blur gate).
#[derive(Clone, Copy, Debug, Default)]
pub struct MaskBlur {
    /// Motion blur is on in this render: the render options, the comp's switch and the Motion
    /// Blur switches of the precomp layers above (when switches affect nested comps).
    pub allowed: bool,
    /// The layer's own Motion Blur switch, as those precomp layers limit it (Same As Layer).
    pub layer: bool,
    /// Sub-frame samples (the comp's Samples Per Frame; fewer in Draft).
    pub samples: usize,
}

/// Geometry needed to rasterise masks without allocating RGBA source pixels.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub offset: [f64; 2],
    pub scale: f64,
}

/// Coverage of one mask in buffer pixels (before mode/opacity/invert). With mask motion blur
/// (Layer ▸ Mask ▸ Motion Blur: On, or Same As Layer with the layer's switch on) an animated
/// path is averaged over the comp's shutter.
fn coverage(ctx: &EvalCtx, layer: &Layer, g: &effectcraft_project::PropGroup, buf: &Geometry, blur: MaskBlur) -> Option<Mask> {
    let mb = blur.allowed
        && match g.kind {
            GroupKind::Mask { motion_blur, .. } => match motion_blur {
                MaskMotionBlur::On => true,
                MaskMotionBlur::Off => false,
                MaskMotionBlur::SameAsLayer => blur.layer,
            },
            _ => false,
        };
    let animated = g.get("path").is_some_and(|p| p.keys.len() > 1 || p.has_expression());
    if mb && animated {
        let fd = ctx.comp.frame_duration().seconds();
        let (angle, phase) = (ctx.comp.shutter_angle / 360.0, ctx.comp.shutter_phase / 360.0);
        let mut acc: Option<Mask> = None;
        let n = blur.samples.max(2);
        let k = 1.0 / n as f32;
        for i in 0..n {
            let f = phase + angle * i as f64 / (n - 1) as f64;
            let sub = ctx.at(ctx.time + effectcraft_time::Tick::from_seconds_f64(f * fd));
            let Some(c) = coverage_at(&sub, layer, g, buf) else { continue };
            match &mut acc {
                None => {
                    let mut c = c;
                    c.data.iter_mut().for_each(|v| *v *= k);
                    acc = Some(c);
                }
                Some(a) => a.data.iter_mut().zip(&c.data).for_each(|(a, c)| *a += c * k),
            }
        }
        return acc;
    }
    coverage_at(ctx, layer, g, buf)
}

/// How far past the layer buffer a feather draws the mask, per axis, at most: a bigger feather
/// still blurs fully, it just sees the mask no further out than this.
const MAX_FEATHER_PAD: usize = 1024;

fn coverage_at(ctx: &EvalCtx, layer: &Layer, g: &effectcraft_project::PropGroup, out: &Geometry) -> Option<Mask> {
    let Value::Path(sp) = ctx.group_value(layer, g, "path")? else { return None };
    let path = effectcraft_path::to_kurbo(&sp);
    let exp = ctx.f(layer, g, "expansion", 0.0);
    let linear = matches!(g.kind, GroupKind::Mask { feather_falloff: FeatherFalloff::Linear, .. });
    let feather = ctx.v2(layer, g, "feather", [0.0; 2]);
    let feather = feather.map(|f| if f.is_finite() { f.max(0.0) } else { 0.0 });
    let box_r = |f: f64| ((f * out.scale / 2.0).round() as usize).max(if f > 0.0 { 1 } else { 0 });
    let sigma = |f: f64| f * out.scale / 2.0;
    // The feather blurs the mask as drawn, not as cut off by the layer's edges: drawn into a
    // buffer this much larger, so a mask running past an edge stays covered up to that edge
    // (an inverted mask showed a faint band along it, a normal one faded out there).
    let pad = |f: f64| {
        let p = if f <= 0.0 {
            0
        } else if linear {
            box_r(f)
        } else {
            effectcraft_raster::gaussian_support(sigma(f))
        };
        p.min(MAX_FEATHER_PAD)
    };
    let (px, py) = (pad(feather[0]), pad(feather[1]));
    let buf = &Geometry {
        width: out.width.saturating_add(u32::try_from(px.saturating_mul(2)).unwrap_or(u32::MAX)),
        height: out.height.saturating_add(u32::try_from(py.saturating_mul(2)).unwrap_or(u32::MAX)),
        offset: [out.offset[0] + px as f64, out.offset[1] + py as f64],
        scale: out.scale,
    };
    let (w, h) = (buf.width, buf.height);
    let m = Mat3::translate(vec2(buf.offset[0], buf.offset[1])) * Mat3::scale(vec2(buf.scale, buf.scale));
    // Variable-width feather (Mask Feather tool points): expansion and the per-point feather
    // are applied together from the signed distance to the path.
    let variable = !sp.feather.is_empty();
    let mut cov = if variable {
        effectcraft_path::feather::variable_feather_coverage(&sp, &m, w, h, exp, linear)
    } else {
        effectcraft_path::fill_coverage(std::slice::from_ref(&path), &m, w, h, FillRule::NonZero)
    };
    // An expansion past about a quarter of the mask's own size: from the distance to the mask
    // (a stroke that much wider than its curve folds its inner edge over and cancels itself).
    let size = {
        let b = kurbo::Shape::bounding_box(&path);
        b.width().min(b.height()) * buf.scale
    };
    if exp.abs() > 0.01 && !variable && exp.abs() * buf.scale > 0.25 * size {
        cov = expand_by_distance(&cov, exp * buf.scale);
    } else if exp.abs() > 0.01 && !variable {
        let ring = effectcraft_path::stroke_coverage(
            std::slice::from_ref(&path),
            &StrokeStyle { width: exp.abs() * 2.0, join: effectcraft_path::Join::Round, ..Default::default() },
            &m,
            w,
            h,
        );
        cov.data.par_iter_mut().zip(ring.data.par_iter()).for_each(|(c, r)| {
            *c = if exp > 0.0 { (*c + r - *c * r).min(1.0) } else { (*c * (1.0 - r)).max(0.0) };
        });
    }
    if feather[0] > 0.0 || feather[1] > 0.0 {
        // Coverage is one plane: blur just that.
        let (w, h) = (cov.width as usize, cov.height as usize);
        cov.data = if linear {
            // One box pass: a straight ramp `feather` pixels wide across the edge.
            box_blur_plane(&cov.data, w, h, box_r(feather[0]), box_r(feather[1]), 1, false)
        } else {
            gaussian_blur_plane(&cov.data, w, h, sigma(feather[0]), sigma(feather[1]), false)
        };
    }
    Some(crop(cov, px, py, out.width, out.height))
}

/// The `w`×`h` window of `cov` starting `x`, `y` pixels in (the layer buffer inside the padded one).
fn crop(cov: Mask, x: usize, y: usize, w: u32, h: u32) -> Mask {
    if x == 0 && y == 0 && cov.width == w && cov.height == h {
        return cov;
    }
    let src_w = cov.width as usize;
    let mut out = Mask::new(w, h, 0.0);
    for (row, dst) in out.data.chunks_mut((w as usize).max(1)).enumerate() {
        let start = (row + y).saturating_mul(src_w).saturating_add(x);
        if let Some(src) = cov.data.get(start..start.saturating_add(dst.len())) {
            dst.copy_from_slice(src);
        }
    }
    out
}

/// Grow (`e` > 0) or shrink coverage by `e` buffer pixels from the Euclidean distance to the
/// covered (or uncovered) pixels; edges stay anti-aliased within about a pixel.
fn expand_by_distance(cov: &Mask, e: f64) -> Mask {
    let (w, h) = (cov.width as usize, cov.height as usize);
    let mut out = cov.clone();
    if e > 0.0 {
        let inside: Vec<bool> = cov.data.iter().map(|&c| c >= 0.5).collect();
        let d = effectcraft_effects::util::distance_transform(&inside, w, h);
        out.data.par_iter_mut().zip(d.par_iter()).for_each(|(c, &d)| {
            // The grown edge lies `e` beyond the old one, itself half a pixel from the nearest
            // covered pixel centre.
            *c = c.max((e + 1.0 - d as f64).clamp(0.0, 1.0) as f32);
        });
    } else {
        let outside: Vec<bool> = cov.data.iter().map(|&c| c < 0.5).collect();
        let d = effectcraft_effects::util::distance_transform(&outside, w, h);
        out.data.par_iter_mut().zip(d.par_iter()).for_each(|(c, &d)| {
            *c = c.min((d as f64 + e).clamp(0.0, 1.0) as f32);
        });
    }
    out
}

/// The layer's enabled masks at the context time, flattened to polylines in layer space (for
/// effects that use masks as paths: Stroke, Scribble, Inner/Outer Key, Reshape…).
pub fn shapes(ctx: &EvalCtx, layer: &Layer) -> Vec<effectcraft_effects::MaskShape> {
    let Some(masks) = layer.masks() else { return Vec::new() };
    let mut out = Vec::new();
    for g in masks.groups().filter(|g| g.enabled) {
        let GroupKind::Mask { inverted, .. } = g.kind else { continue };
        let Some(Value::Path(sp)) = ctx.group_value(layer, g, "path") else { continue };
        let path = effectcraft_path::to_kurbo(&sp);
        let mut pts: Vec<[f64; 2]> = Vec::new();
        let mut closed = false;
        kurbo::flatten(path.iter(), 0.25, |el| match el {
            effectcraft_path::PathEl::MoveTo(p) | effectcraft_path::PathEl::LineTo(p) if pts.last() != Some(&[p.x, p.y]) => {
                pts.push([p.x, p.y]);
            }
            effectcraft_path::PathEl::ClosePath => closed = true,
            _ => {}
        });
        if closed && pts.len() > 1 && pts.first() == pts.last() {
            pts.pop();
        }
        out.push(effectcraft_effects::MaskShape { name: g.name.clone(), points: pts, closed: closed || sp.closed, inverted });
    }
    out
}

/// Combined layer mask coverage in the supplied geometry.
/// `None` means there is no applicable closed mask or geometry exceeds the cap.
pub fn combined(ctx: &EvalCtx, layer: &Layer, buf: &Geometry, blur: MaskBlur) -> Option<Mask> {
    let masks = layer.masks()?;
    // An open mask (a path the Pen is still drawing) makes no transparency: the layer shows
    // whole until the path is closed (After Effects; effects still use it as a path).
    let closed = |g: &effectcraft_project::PropGroup| matches!(ctx.group_value(layer, g, "path"), Some(Value::Path(sp)) if sp.closed);
    let list: Vec<_> = masks.groups().filter(|g| g.enabled && matches!(g.kind, GroupKind::Mask { mode, .. } if mode != MaskMode::None) && closed(g)).collect();
    if list.is_empty() {
        return None;
    }
    let n = (buf.width as usize).checked_mul(buf.height as usize)?;
    if n > 268_435_456 {
        return None;
    }
    let first_sub = matches!(list.first()?.kind, GroupKind::Mask { mode: MaskMode::Subtract, .. });
    let mut acc = vec![if first_sub { 1.0f32 } else { 0.0 }; n];
    // The first mask has nothing to combine with. Intersect and Darken would multiply or
    // minimise against that empty coverage and hide the layer; they cover like Add instead.
    // Lighten and Difference already do. Filling the accumulator first would make Add show
    // the whole layer.
    let mut first = true;
    for g in list {
        let GroupKind::Mask { mode, inverted, .. } = g.kind else { continue };
        let Some(mut cov) = coverage(ctx, layer, g, buf, blur) else { continue };
        let mode = if first && matches!(mode, MaskMode::Intersect | MaskMode::Darken) { MaskMode::Add } else { mode };
        first = false;
        let op = (ctx.f(layer, g, "opacity", 100.0) / 100.0) as f32;
        cov.data.par_iter_mut().for_each(|c| {
            if inverted {
                *c = 1.0 - *c;
            }
            *c *= op;
        });
        acc.par_iter_mut().zip(cov.data.par_iter()).for_each(|(a, &c)| {
            *a = match mode {
                MaskMode::Add => *a + c - *a * c,
                MaskMode::Subtract => *a * (1.0 - c),
                MaskMode::Intersect => *a * c,
                MaskMode::Lighten => a.max(c),
                MaskMode::Darken => a.min(c),
                MaskMode::Difference => (*a - c).abs(),
                MaskMode::None => *a,
            };
        });
    }
    Some(Mask { width: buf.width, height: buf.height, data: acc })
}

/// Apply the layer's masks to its buffer, sharing coverage with resident sources.
pub fn apply(ctx: &EvalCtx, layer: &Layer, buf: &mut Buf, blur: MaskBlur) -> bool {
    let geometry = Geometry { width: buf.img.width, height: buf.img.height, offset: buf.offset, scale: buf.scale };
    let Some(mask) = combined(ctx, layer, &geometry, blur) else { return false };
    buf.img.mul_mask(&mask.data);
    true
}
