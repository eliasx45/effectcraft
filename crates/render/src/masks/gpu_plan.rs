//! A layer's masks as a plan for an accelerator (MASK-GPU2: `effectcraft-gpu`'s `masks_gpu`
//! draws and applies it to a GPU-resident layer image).
//!
//! What is drawn is decided here, shared with [`super::combined`]: the applicable masks in order,
//! each mask's padded coverage plane and feather ([`super::padded`]), its mode (the first mask's
//! Intersect / Darken already as Add), inversion and opacity. A mask path is a non-zero fill:
//! its scan edges are the ones tiny-skia walks for `fill_coverage`
//! ([`crate::shapes::skia_scan`]), so the accelerator makes the CPU's very coverage. A small
//! positive expansion is its round-joined ring ([`effectcraft_path::stroke_outline`], filled
//! non-zero, as `stroke_coverage`), drawn over the fill (`c + r − c r`, the CPU's union).
//!
//! The plan declines (`None`: the CPU renders the layer) what it can't reproduce: variable-width
//! feathers (Mask Feather tool points), mask motion blur over an animated path, a negative
//! expansion or one past a quarter of the mask's size (distance-based on the CPU), planes past
//! tiny-skia's 8191 px and runaway edge counts.

use effectcraft_geom::{Mat3, vec2};
use effectcraft_keyframe::Value;
use effectcraft_path::StrokeStyle;
use effectcraft_project::{GroupKind, Layer, MaskMode, MaskMotionBlur};

use super::{FeatherBlur, Geometry, Padded, applicable, motion_blurred, padded};
use crate::Renderer;
use crate::eval::EvalCtx;
use crate::shapes::skia_scan::{MAX_EDGES, Scan, ScanEdge, scan_edges};

/// One mask as the accelerator draws it.
#[derive(Clone, Debug)]
pub struct MaskDraw {
    /// The padded coverage plane's size: the layer buffer plus the feather's reach each side.
    pub plane: [u32; 2],
    /// Where the layer buffer starts in the plane.
    pub pad: [u32; 2],
    /// The mask path's fill edges in the plane (non-zero; empty: no coverage).
    pub fill: Vec<ScanEdge>,
    /// A positive expansion's ring, drawn over the fill (empty: none).
    pub ring: Vec<ScanEdge>,
    pub feather: Option<FeatherBlur>,
    /// As combined: 0 Add, 1 Subtract, 2 Intersect, 3 Lighten, 4 Darken, 5 Difference.
    pub mode: u32,
    pub inverted: bool,
    pub opacity: f32,
}

/// [`super::combined`] of a `width`×`height` layer buffer as a plan: the accumulator starts
/// at `init` and each mask combines into it in order. No masks: the layer is left as it is.
#[derive(Clone, Debug)]
pub struct GpuMaskPlan {
    pub width: u32,
    pub height: u32,
    pub init: f32,
    pub masks: Vec<MaskDraw>,
}

/// A cheap test, without evaluating the masks: none of the layer's masks is certain to need the
/// CPU (a variable-width feather, or mask motion blur on an animated path). The plan can still
/// decline.
pub fn gpu_eligible(layer: &Layer) -> bool {
    let Some(masks) = layer.masks() else { return true };
    masks.groups().filter(|g| g.enabled).all(|g| {
        let GroupKind::Mask { motion_blur, .. } = g.kind else { return true };
        let Some(path) = g.get("path") else { return true };
        let animated = path.keys.len() > 1 || path.has_expression();
        let variable = |v: &Value| matches!(v, Value::Path(sp) if !sp.feather.is_empty());
        !(animated && motion_blur != MaskMotionBlur::Off) && !variable(&path.value) && !path.keys.iter().any(|k| variable(&k.value))
    })
}

fn mode_code(mode: MaskMode) -> Option<u32> {
    Some(match mode {
        MaskMode::Add => 0,
        MaskMode::Subtract => 1,
        MaskMode::Intersect => 2,
        MaskMode::Lighten => 3,
        MaskMode::Darken => 4,
        MaskMode::Difference => 5,
        MaskMode::None => return None,
    })
}

impl Renderer<'_> {
    /// The masks of a resident layer buffer (`width`×`height`, `offset` and `scale` in effect
    /// space, as [`Self::resident_source`] rebases it) as a plan; `None` when the CPU must
    /// apply them.
    pub fn mask_gpu_plan(&self, ctx: &EvalCtx, layer: &Layer, width: u32, height: u32, offset: [f64; 2], scale: f64) -> Option<GpuMaskPlan> {
        // The CPU masks the buffer before `resident_source` rebases it.
        let origin = crate::fx::fx_origin(ctx, layer, false);
        let out = Geometry { width, height, offset: [offset[0] - origin[0] * scale, offset[1] - origin[1] * scale], scale };
        plan(ctx, layer, &out, self.mask_blur(ctx, layer))
    }
}

/// [`super::combined`] as a plan.
fn plan(ctx: &EvalCtx, layer: &Layer, out: &Geometry, blur: super::MaskBlur<'_>) -> Option<GpuMaskPlan> {
    let list = applicable(ctx, layer);
    let mut plan = GpuMaskPlan { width: out.width, height: out.height, init: 0.0, masks: Vec::new() };
    if list.is_empty() {
        return Some(plan);
    }
    if !out.scale.is_finite() || out.scale <= 0.0 || out.offset.iter().any(|v| !v.is_finite()) {
        return None;
    }
    plan.init = if matches!(list.first()?.kind, GroupKind::Mask { mode: MaskMode::Subtract, .. }) { 1.0 } else { 0.0 };
    let mut total = 0usize;
    let mut first = true;
    for g in list {
        let GroupKind::Mask { mode, inverted, .. } = g.kind else { continue };
        if motion_blurred(g, blur) {
            return None;
        }
        let Some(Value::Path(sp)) = ctx.group_value(layer, g, "path") else { continue };
        if !sp.feather.is_empty() {
            return None;
        }
        let path = effectcraft_path::to_kurbo(&sp);
        let Padded { blur: feather, plane, px, py } = padded(ctx, layer, g, out);
        let m = Mat3::translate(vec2(plane.offset[0], plane.offset[1])) * Mat3::scale(vec2(plane.scale, plane.scale));
        let edges = |paths: &[effectcraft_path::BezPath]| match scan_edges(paths, &m, plane.width, plane.height) {
            Scan::Empty => Some(Vec::new()),
            Scan::Edges(e) => Some(e),
            Scan::Decline => None,
        };
        let fill = edges(std::slice::from_ref(&path))?;
        let exp = ctx.f(layer, g, "expansion", 0.0);
        let ring = if exp.abs() > 0.01 {
            let size = {
                let b = kurbo::Shape::bounding_box(&path);
                b.width().min(b.height()) * plane.scale
            };
            // Shrinking, or growing by the distance to the mask: CPU only.
            if exp < 0.0 || !exp.is_finite() || exp * plane.scale > 0.25 * size {
                return None;
            }
            let style = StrokeStyle { width: exp * 2.0, join: effectcraft_path::Join::Round, ..Default::default() };
            match effectcraft_path::stroke_outline(std::slice::from_ref(&path), &style, m.mean_scale()) {
                Some(o) => edges(std::slice::from_ref(&o))?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        total = total.saturating_add(fill.len()).saturating_add(ring.len());
        if total > MAX_EDGES {
            return None;
        }
        let mode = if first && matches!(mode, MaskMode::Intersect | MaskMode::Darken) { MaskMode::Add } else { mode };
        first = false;
        let opacity = (ctx.f(layer, g, "opacity", 100.0) / 100.0) as f32;
        plan.masks.push(MaskDraw {
            plane: [plane.width, plane.height],
            pad: [u32::try_from(px).ok()?, u32::try_from(py).ok()?],
            fill,
            ring,
            feather,
            mode: mode_code(mode)?,
            inverted,
            opacity,
        });
    }
    Some(plan)
}
