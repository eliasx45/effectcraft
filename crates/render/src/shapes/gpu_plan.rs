//! A shape layer's contents as a raster plan for an accelerator (layer content on the GPU,
//! milestone 1: `effectcraft-gpu`'s `shapes_gpu` rasterises it).
//!
//! Everything that decides *what* is drawn stays here, shared with [`super::render_window`]: the
//! stack of paths, operators and paints ([`super::collect`]), the buffer size, offset and window,
//! each draw's pixel rectangle, stroke outlines (joins, caps, dashes: the same
//! [`effectcraft_path::stroke_outline`] the CPU fills) and the gradient ramps. Each draw's fill
//! becomes the scan edges tiny-skia would walk for it ([`super::skia_scan`]), so the accelerator
//! makes the CPU's very coverage masks (non-zero or even-odd), and paints them like
//! [`super::paint`] (Normal blend). Glyph outlines (milestone 2, text) are fills too and can use
//! [`super::skia_scan::scan_edges`] unchanged.
//!
//! [`Renderer::shape_raster_plan`] declines (`None`: the CPU renders the layer) whatever the plan
//! can't reproduce yet: masks the GPU can't apply ([`crate::masks::gpu_plan`]), motion-blurred shape sub-frames, draws in blend modes other than
//! Normal, Taper / Wave strokes (supersampled further on the CPU), masks over 8191 px and runaway
//! edge counts. Paths reaching outside their draw's mask are clipped as tiny-skia clips them.

use effectcraft_color::BlendMode;
use effectcraft_geom::{Mat3, vec2};
use effectcraft_keyframe::Gradient;
use effectcraft_path::{BezPath, FillRule, StrokeStyle};
use effectcraft_project::{Layer, LayerSource};

use super::skia_scan::{MAX_EDGES, Scan, ScanEdge, scan_edges};
use super::{Draw, GradientRamp, Paint, PxRect, collect, draw_bounds, extent};
use crate::eval::EvalCtx;
use crate::{ContentClip, Renderer};

/// What one draw paints with.
#[derive(Clone, Debug)]
pub enum PlanPaint {
    /// Straight RGBA.
    Solid([f32; 4]),
    Gradient(Box<PlanGradient>),
}

/// A gradient fill or stroke: the ramp of [`GradientRamp`] and the stops of
/// [`Gradient::sample`], evaluated per pixel.
#[derive(Clone, Debug)]
pub struct PlanGradient {
    /// Buffer pixel → paint space, as `[a, c, e, b, d, f]` (x' = a x + c y + e, y' = b x + d y + f).
    pub to_local: [f64; 6],
    /// 0 linear, 1 radial, 2 angle, 3 diamond (see [`GradientRamp::t`]).
    pub kind: u32,
    /// Paint space → unit gradient space (affine gradients), same layout as `to_local`.
    pub affine: Option<[f64; 6]>,
    pub start: [f64; 2],
    pub dir: [f64; 2],
    pub len2: f64,
    pub focal: Option<[f64; 2]>,
    /// Colour stops `(position, straight rgba, midpoint to the next)`.
    pub colors: Vec<(f32, [f32; 4], f32)>,
    /// Opacity stops `(position, alpha, midpoint to the next)`.
    pub opacities: Vec<(f32, f32, f32)>,
}

/// One paint item of the layer, bottom first.
#[derive(Clone, Debug)]
pub struct PlanDraw {
    /// `[x0, y0, w, h]` in buffer pixels.
    pub rect: [u32; 4],
    pub even_odd: bool,
    /// The fill's scan edges in the `rect`-sized mask ([`super::skia_scan`]).
    pub edges: Vec<ScanEdge>,
    pub opacity: f32,
    pub paint: PlanPaint,
    /// The sub-sample this draw belongs to (motion blur: each group is composited on its own,
    /// then the groups are summed, `accumulate_warp`); 0 for a layer drawn once.
    pub group: u32,
    /// The weight of the draw's group in that sum (1 for a layer drawn once).
    pub weight: f32,
}

/// A shape layer's buffer as [`Renderer::resident_source`] makes it, before the working-space
/// conversion (`Pipe::authored_in`) and quantisation, which the accelerator applies.
#[derive(Clone, Debug)]
pub struct RasterPlan {
    pub width: u32,
    pub height: u32,
    /// Offset and scale of the buffer, in effect space (rebased as `resident_source`).
    pub offset: [f64; 2],
    pub scale: f64,
    pub draws: Vec<PlanDraw>,
}

impl RasterPlan {
    pub fn edge_count(&self) -> usize {
        self.draws.iter().map(|d| d.edges.len()).sum()
    }
}

impl Renderer<'_> {
    /// Whether a shape layer may be rasterised from a [`RasterPlan`] at all (a cheap test,
    /// without evaluating its contents; [`Self::shape_raster_plan`] can still decline).
    pub fn shape_raster_eligible(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        matches!(layer.source, LayerSource::Shape)
            && crate::masks::gpu_plan::gpu_eligible(layer)
            && self.content_times(ctx, layer).is_none()
            && layer.props.sub("contents").is_some()
    }

    /// The raster plan of [`Self::resident_source`] for a shape layer in the window `clip` that
    /// came with its key ([`Self::resident_content`]); `None` when the CPU must render it.
    pub fn shape_raster_plan(&self, ctx: &EvalCtx, layer: &Layer, clip: ContentClip) -> Option<RasterPlan> {
        if !self.shape_raster_eligible(ctx, layer) {
            return None;
        }
        let contents = layer.props.sub("contents")?;
        let s = self.raster_scale(ctx, layer);
        let mut plan = plan(ctx, layer, contents, s, clip.rect())?;
        let origin = crate::fx::fx_origin(ctx, layer, false);
        plan.offset = [plan.offset[0] + origin[0] * plan.scale, plan.offset[1] + origin[1] * plan.scale];
        Some(plan)
    }
}

/// [`super::render_window`] as a plan.
fn plan(ctx: &EvalCtx, layer: &Layer, contents: &effectcraft_project::PropGroup, s: f64, window: Option<[u32; 4]>) -> Option<RasterPlan> {
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    let mut arena = Vec::new();
    let (draws, _) = collect(ctx, layer, contents, &mut arena);
    let resolved: Vec<(&Draw, Vec<BezPath>, kurbo::Rect)> = draws
        .iter()
        .rev()
        .filter(|d| d.opacity > 0.0)
        .filter_map(|d| {
            let paths: Vec<BezPath> = d.slots.iter().filter_map(|&i| arena.get(i).cloned()).filter(|p| !p.elements().is_empty()).collect();
            let b = draw_bounds(d, &paths)?;
            Some((d, paths, b))
        })
        .collect();
    if resolved.iter().any(|(d, ..)| d.blend != BlendMode::Normal) {
        crate::cpu_why::declined(crate::cpu_why::SHAPE_BLEND);
        return None;
    }
    let Some(b) = resolved.iter().map(|r| r.2).reduce(|a, b| a.union(b)) else {
        return Some(RasterPlan { width: 1, height: 1, offset: [0.0, 0.0], scale: s, draws: vec![] });
    };
    let (w, h, offset) = extent(b, s);
    let (w, h, offset) = match window {
        Some([x, y, ww, wh]) => {
            let (x, y) = (x.min(w - 1), y.min(h - 1));
            let (ww, wh) = (ww.clamp(1, w - x), wh.clamp(1, h - y));
            (ww, wh, [offset[0] - x as f64, offset[1] - y as f64])
        }
        None => (w, h, offset),
    };
    let m = Mat3::translate(vec2(offset[0], offset[1])) * Mat3::scale(vec2(s, s));
    let px_rect = |r: kurbo::Rect| -> Option<PxRect> {
        let x0 = (r.x0 * s + offset[0]).floor().max(0.0);
        let y0 = (r.y0 * s + offset[1]).floor().max(0.0);
        let x1 = (r.x1 * s + offset[0]).ceil().min(w as f64);
        let y1 = (r.y1 * s + offset[1]).ceil().min(h as f64);
        (x1 > x0 && y1 > y0).then_some(PxRect { x0: x0 as u32, y0: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
    };
    let mut out = Vec::with_capacity(resolved.len());
    let mut total = 0usize;
    for (d, paths, b) in &resolved {
        let Some(r) = px_rect(*b) else { continue };
        let sub = Mat3::translate(vec2(-(r.x0 as f64), -(r.y0 as f64))) * m;
        let stroke = |style: &StrokeStyle| -> Option<Vec<BezPath>> {
            if style.is_variable() {
                // `stroke_coverage` supersamples Taper and Wave further (`fill_coverage_fine`).
                crate::cpu_why::declined(crate::cpu_why::TAPER_STROKE);
                return None;
            }
            let mut st = style.clone();
            st.width *= d.xf.mean_scale();
            // As `effectcraft_path::stroke_coverage`: the outline, filled non-zero.
            Some(effectcraft_path::stroke_outline(paths, &st, sub.mean_scale()).into_iter().collect())
        };
        let (fill, rule): (Vec<BezPath>, FillRule) = match &d.paint {
            Paint::Fill { rule, .. } => (paths.clone(), *rule),
            Paint::Stroke { style, .. } | Paint::Gradient { stroke: Some(style), .. } => (stroke(style)?, FillRule::NonZero),
            Paint::Gradient { rule, .. } => (paths.clone(), *rule),
        };
        let edges = match scan_edges(&fill, &sub, r.w, r.h) {
            Scan::Empty => continue,
            Scan::Edges(e) => e,
            Scan::Decline => {
                crate::cpu_why::declined(crate::cpu_why::PATH_TOO_BIG);
                return None;
            }
        };
        total = total.saturating_add(edges.len());
        if total > MAX_EDGES {
            crate::cpu_why::declined(crate::cpu_why::TOO_MANY_EDGES);
            return None;
        }
        let paint = match &d.paint {
            Paint::Fill { color, .. } | Paint::Stroke { color, .. } => PlanPaint::Solid(*color),
            Paint::Gradient { g, kind, width, start, end, highlight, .. } => {
                // As `render_window`: identity when the paint space is degenerate.
                let to_local = (m * d.xf).inverse().unwrap_or(Mat3::IDENTITY);
                PlanPaint::Gradient(Box::new(gradient(g, GradientRamp::new(*kind, *start, *end, *width, *highlight), &to_local)?))
            }
        };
        out.push(PlanDraw { rect: [r.x0, r.y0, r.w, r.h], even_odd: rule == FillRule::EvenOdd, edges, opacity: d.opacity, paint, group: 0, weight: 1.0 });
    }
    Some(RasterPlan { width: w, height: h, offset, scale: s, draws: out })
}

fn coeffs(m: &Mat3) -> Option<[f64; 6]> {
    let a = &m.0;
    let c = [a[0][0], a[0][1], a[0][2], a[1][0], a[1][1], a[1][2]];
    (m.is_affine() && c.iter().all(|v| v.is_finite())).then_some(c)
}

fn gradient(g: &Gradient, ramp: GradientRamp, to_local: &Mat3) -> Option<PlanGradient> {
    let affine = match ramp.affine {
        Some(a) => {
            let [a, b, c, d, e, f] = a.as_coeffs();
            Some([a, c, e, b, d, f])
        }
        None => None,
    };
    let mid = |m: &[f64], i: usize| m.get(i).copied().filter(|m| m.is_finite()).unwrap_or(0.5).clamp(1e-6, 1.0 - 1e-6) as f32;
    Some(PlanGradient {
        to_local: coeffs(to_local)?,
        kind: ramp.kind,
        affine,
        start: ramp.s,
        dir: ramp.d,
        len2: ramp.len2,
        focal: ramp.focal,
        colors: g.colors.iter().enumerate().map(|(i, (p, c))| (*p as f32, *c, mid(&g.color_midpoints, i))).collect(),
        opacities: g.opacities.iter().enumerate().map(|(i, (p, a))| (*p as f32, *a, mid(&g.opacity_midpoints, i))).collect(),
    })
}
