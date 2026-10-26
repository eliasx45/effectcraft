//! A solid layer's source for an accelerator (TEXT-GPU2): its size, offset and scale and the one
//! pixel value `Renderer::source` fills it with, so the accelerator fills the buffer itself
//! instead of receiving a frame-sized upload of one colour (the Spotlight comp's "DOTTED BG": a
//! 1080p solid under GPU effects spent ~3.5 ms a frame filling and uploading it).

use effectcraft_project::{ItemKind, Layer, LayerSource};

use crate::Renderer;
use crate::eval::EvalCtx;

/// [`Renderer::resident_source`] of a solid layer as one value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SolidFill {
    pub width: u32,
    pub height: u32,
    /// Working space, quantised (as `Renderer::source` fills it).
    pub px: [f32; 4],
    /// Offset and scale of the buffer, in effect space (rebased as `resident_source`).
    pub offset: [f64; 2],
    pub scale: f64,
}

impl Renderer<'_> {
    /// Whether a solid layer's resident source may be filled by an accelerator: not an
    /// adjustment layer (that source is the comp below), masks only as
    /// [`crate::masks::gpu_plan`] can apply them.
    pub fn solid_fill_eligible(&self, layer: &Layer) -> bool {
        matches!(layer.source, LayerSource::Solid { .. }) && !layer.switches.adjustment && crate::masks::gpu_plan::gpu_eligible(layer)
    }

    /// The fill of [`Self::resident_source`] for a solid layer (drawn whole: `masked_source`
    /// keeps no window for solids); `None` for other layers.
    pub fn solid_fill(&self, ctx: &EvalCtx, layer: &Layer) -> Option<SolidFill> {
        if !self.solid_fill_eligible(layer) {
            return None;
        }
        let LayerSource::Solid { item } = &layer.source else { return None };
        let ItemKind::Solid(sol) = &self.project.item(*item)?.kind else { return None };
        // As `Renderer::source`.
        let s = self.solid_scale(sol.width, sol.height);
        let width = ((sol.width as f64 * s).ceil() as u32).max(1);
        let height = ((sol.height as f64 * s).ceil() as u32).max(1);
        let mut c = sol.color;
        if let Some(conv) = self.pipe.authored_in() {
            c = conv.apply(c);
        }
        let px = self.pipe.quantize_px([c[0], c[1], c[2], 1.0]);
        let origin = crate::fx::fx_origin(ctx, layer, false);
        Some(SolidFill { width, height, px, offset: [origin[0] * s, origin[1] * s], scale: s })
    }
}
