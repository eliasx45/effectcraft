//! Text layers as a [`RasterPlan`] for an accelerator (TEXT-GPU1): the buffer of
//! [`super::render`] (cropped to the key's window as `masked_source` crops it) as the same draws
//! [`super::draw_glyphs`] makes on the CPU: per run of glyphs painted alike, its outlines (after
//! layout, faux bold / italic, per-character animators and Path Options, as [`TextGeom`] holds
//! them) filled non-zero, or stroked with round joins as `stroke_coverage` strokes them, each
//! the scan edges tiny-skia would walk ([`crate::shapes::skia_scan`]) and a solid colour,
//! composited Normal in drawing order. The accelerator walks them with its shape kernels.
//!
//! [`Renderer::text_raster_plan`] declines (`None`: the CPU renders the layer) for masks the
//! GPU can't apply ([`crate::masks::gpu_plan`]), motion-blurred sub-frames, Blur and Gradient Fill animators (per-pixel colour, blurred
//! patches), Inter-Character Blending other than Normal, masks tiny-skia would tile and runaway
//! edge counts.

use std::borrow::Cow;

use effectcraft_color::BlendMode;
use effectcraft_geom::{Mat3, vec2};
use effectcraft_path::{BezPath, StrokeStyle};
use effectcraft_project::{Layer, LayerSource, PropGroup};

use super::{TextGeom, runs, text_geom};
use crate::eval::EvalCtx;
use crate::shapes::gpu_plan::{PlanDraw, PlanPaint, RasterPlan};
use crate::shapes::skia_scan::{MAX_EDGES, Scan, ScanEdge, scan_edges};
use crate::{ContentClip, Renderer};

impl Renderer<'_> {
    /// Whether a text layer may be rasterised from a [`RasterPlan`] at all (a cheap test of its
    /// properties, without laying it out; [`Self::text_raster_plan`] can still decline).
    pub fn text_raster_eligible(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        matches!(layer.source, LayerSource::Text)
            && crate::masks::gpu_plan::gpu_eligible(layer)
            && self.content_times(ctx, layer).is_none()
            && layer.props.sub("text").is_some_and(|t| plain_paint(ctx, layer, t))
    }

    /// The raster plan of [`Self::resident_source`] for a text layer in the window `clip` that
    /// came with its key ([`Self::resident_content`]); `None` when the CPU must render it.
    pub fn text_raster_plan(&self, ctx: &EvalCtx, layer: &Layer, clip: ContentClip) -> Option<RasterPlan> {
        if !self.text_raster_eligible(ctx, layer) {
            return None;
        }
        let s = self.raster_scale(ctx, layer);
        if !s.is_finite() || s <= 0.0 {
            return None;
        }
        let geom = text_geom(ctx, layer);
        let mut plan = plan(geom.as_ref(), s, clip.rect())?;
        let origin = crate::fx::fx_origin(ctx, layer, false);
        plan.offset = [plan.offset[0] + origin[0] * plan.scale, plan.offset[1] + origin[1] * plan.scale];
        Some(plan)
    }
}

/// No animator that blurs or paints a gradient (present at all: the cheap test can't evaluate
/// them), and Normal inter-character blending.
fn plain_paint(ctx: &EvalCtx, layer: &Layer, text: &PropGroup) -> bool {
    let animated = text.sub("animators").is_some_and(|a| {
        a.groups().any(|g| g.enabled && g.sub("properties").is_some_and(|p| p.get("blur").is_some() || p.get("gradientFillColors").is_some()))
    });
    let blend_idx = text.sub("moreOptions").map(|m| ctx.e(layer, m, "interCharBlend")).unwrap_or(0) as usize;
    let blend = effectcraft_project::build::INTER_CHAR_BLEND_MODES.get(blend_idx).and_then(|n| BlendMode::from_name(n)).unwrap_or_default();
    !animated && blend == BlendMode::Normal
}

/// [`super::render`] at scale `s` (then cropped to `window`, `[x, y, w, h]`) as a plan.
fn plan(geom: Option<&TextGeom>, s: f64, window: Option<[u32; 4]>) -> Option<RasterPlan> {
    let (w, h, offset, draws) = match geom.map(|g| whole(g, s)) {
        None | Some(Ok(None)) => (1, 1, [0.0; 2], vec![]),
        Some(Ok(Some(whole))) => whole,
        Some(Err(())) => return None,
    };
    let Some([wx, wy, ww, wh]) = window else {
        return Some(RasterPlan { width: w, height: h, offset, scale: s, draws: draws.into_iter().map(WholeDraw::into_plan).collect() });
    };
    // `masked_source` crops the whole buffer (transparent outside it).
    let (ww, wh) = (ww.max(1), wh.max(1));
    let draws = draws.into_iter().filter_map(|d| clip_draw(d, [i64::from(wx), i64::from(wy)], [ww, wh])).collect();
    Some(RasterPlan { width: ww, height: wh, offset: [offset[0] - f64::from(wx), offset[1] - f64::from(wy)], scale: s, draws })
}

/// A draw in the whole buffer: its rectangle `[x0, y0, x1, y1]` (whole-buffer pixels), edges
/// in that rectangle's mask, and paint.
struct WholeDraw {
    rect: [i64; 4],
    edges: Vec<ScanEdge>,
    color: [f32; 4],
}

type Whole = (u32, u32, [f64; 2], Vec<WholeDraw>);

/// The whole buffer of [`super::render`] and its draws (`draw_glyphs`, every glyph): `Ok(None)`
/// when nothing is drawn (the 1 × 1 empty buffer), `Err` when the CPU must draw it.
fn whole(geom: &TextGeom, s: f64) -> Result<Option<Whole>, ()> {
    if geom.blend != BlendMode::Normal {
        return Err(());
    }
    let mut bounds: Option<kurbo::Rect> = None;
    for g in &geom.glyphs {
        if let Some(b) = effectcraft_path::bounds(&[g.path()]) {
            let pad = g.stroke_width + 2.0 + g.xf.blur[0].max(g.xf.blur[1]) * 1.5;
            let b = b.inflate(pad, pad);
            bounds = Some(bounds.map_or(b, |a| a.union(b)));
        }
    }
    let Some(b) = bounds else { return Ok(None) };
    let b = b.intersect(kurbo::Rect::new(-20000.0, -20000.0, 20000.0, 20000.0));
    let (x0, y0) = ((b.x0 * s).floor(), (b.y0 * s).floor());
    let w = ((b.x1 * s).ceil() - x0 + 4.0).clamp(1.0, 16384.0) as u32;
    let h = ((b.y1 * s).ceil() - y0 + 4.0).clamp(1.0, 16384.0) as u32;
    let offset = [-x0 + 2.0, -y0 + 2.0];
    let to_px = Mat3::translate(vec2(offset[0], offset[1])) * Mat3::scale(vec2(s, s));
    let (wi, hi) = (i64::from(w), i64::from(h));
    let mut draws = Vec::new();
    let mut total = 0usize;
    for (run, stroke) in runs(geom) {
        let Some(g) = run.first().and_then(|&gi| geom.glyphs.get(gi)) else { continue };
        // As `draw_glyphs_with`: one glyph from its own outline, a run from its glyphs'
        // outlines in layer space, all wound the same way.
        let (paths, m): (Cow<[BezPath]>, Mat3) = match run.as_slice() {
            [_] => (Cow::Borrowed(std::slice::from_ref(&g.local)), to_px * g.m2()),
            _ => {
                let wound = |p: BezPath| if kurbo::Shape::area(&p) < 0.0 { p.reverse_subpaths() } else { p };
                (Cow::Owned(run.iter().filter_map(|&gi| geom.glyphs.get(gi)).map(|g| wound(g.path())).collect()), to_px)
            }
        };
        let Some(pb) = effectcraft_path::bounds(&paths) else { continue };
        let blur = [g.xf.blur[0] * s, g.xf.blur[1] * s];
        // Blurred patches and per-pixel gradient colour stay on the CPU.
        if blur[0] > 0.05 || blur[1] > 0.05 || (!stroke && g.fills.is_some()) {
            return Err(());
        }
        let pad = g.stroke_width * m.mean_scale() + 2.0 + blur[0].max(blur[1]) * 1.5;
        let r = m.map_rect(&effectcraft_geom::Rect::new(pb.x0, pb.y0, pb.x1, pb.y1));
        let rect = [
            ((r.x0 - pad).floor() as i64).max(0),
            ((r.y0 - pad).floor() as i64).max(0),
            ((r.x1 + pad).ceil() as i64).min(wi),
            ((r.y1 + pad).ceil() as i64).min(hi),
        ];
        if rect[2] <= rect[0] || rect[3] <= rect[1] {
            continue;
        }
        let (rw, rh) = ((rect[2] - rect[0]) as u32, (rect[3] - rect[1]) as u32);
        let mm = Mat3::translate(vec2(-rect[0] as f64, -rect[1] as f64)) * m;
        // `raster_pass`: `stroke_coverage` (the outline, filled non-zero) or `fill_coverage`.
        let scan = if stroke {
            let st = StrokeStyle { width: g.stroke_width, join: effectcraft_path::Join::Round, ..Default::default() };
            match effectcraft_path::stroke_outline(&paths, &st, mm.mean_scale()) {
                Some(o) => scan_edges(std::slice::from_ref(&o), &mm, rw, rh),
                None => Scan::Empty,
            }
        } else {
            scan_edges(&paths, &mm, rw, rh)
        };
        let edges = match scan {
            Scan::Empty => continue,
            Scan::Edges(e) => e,
            Scan::Decline => return Err(()),
        };
        total = total.saturating_add(edges.len());
        if total > MAX_EDGES {
            return Err(());
        }
        // `paint_coverage`: colour × coverage × (alpha × opacity), the product taken first.
        let op = (g.xf.opacity / 100.0).clamp(0.0, 1.0) as f32;
        let col = if stroke { g.stroke } else { g.fill };
        draws.push(WholeDraw { rect, edges, color: [col[0], col[1], col[2], col[3] * op] });
    }
    Ok(Some((w, h, offset, draws)))
}

impl WholeDraw {
    fn into_plan(self) -> PlanDraw {
        let [x0, y0, x1, y1] = self.rect;
        PlanDraw {
            rect: [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32],
            even_odd: false,
            edges: self.edges,
            opacity: 1.0,
            paint: PlanPaint::Solid(self.color),
        }
    }
}

/// The part of a whole-buffer draw inside the window at `at` (whole-buffer pixels) of `size`,
/// in window pixels. Its edges stay those of the draw's own mask, moved to the part's corner:
/// what lies left of the part is gathered on its first sample column (the kernels clamp there),
/// so every sample in it keeps its winding; rows and columns outside are skipped.
fn clip_draw(d: WholeDraw, at: [i64; 2], size: [u32; 2]) -> Option<PlanDraw> {
    let [x0, y0, x1, y1] = [d.rect[0] - at[0], d.rect[1] - at[1], d.rect[2] - at[0], d.rect[3] - at[1]];
    let (cx0, cy0) = (x0.max(0), y0.max(0));
    let (cx1, cy1) = (x1.min(i64::from(size[0])), y1.min(i64::from(size[1])));
    if cx1 <= cx0 || cy1 <= cy0 {
        return None;
    }
    let (dx, dy) = (cx0 - x0, cy0 - y0);
    // Within one mask (≤ 8191 px a side), so these fit.
    let (sx, sy) = (i32::try_from(dx.checked_mul(4 << 16)?).ok()?, i32::try_from(dy.checked_mul(4)?).ok()?);
    let edges = d.edges.into_iter().map(|e| [e[0].wrapping_sub(sy), e[1], e[2].wrapping_sub(sx), e[3]]).collect();
    Some(PlanDraw {
        rect: [cx0 as u32, cy0 as u32, (cx1 - cx0) as u32, (cy1 - cy0) as u32],
        even_odd: false,
        edges,
        opacity: 1.0,
        paint: PlanPaint::Solid(d.color),
    })
}

#[cfg(test)]
#[path = "tests_gpu_plan.rs"]
mod tests;
