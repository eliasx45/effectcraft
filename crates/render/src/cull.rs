//! Work an accelerated compositor can skip without changing a pixel: layers that draw nothing
//! (off the canvas, or behind an empty track matte), layers hidden under an opaque layer that
//! covers the whole canvas, and repeated renders of the same nested comp within a frame.
//! Each predicate only answers `true` when the skipped work provably leaves the comp as it was.

use crate::motion_blur::MotionMemo;
use crate::{EvalCtx, Renderer, color, styles};
use effectcraft_color::BlendMode;
use effectcraft_effects::Buf;
use effectcraft_geom::vec2;
use effectcraft_project::{ItemId, ItemKind, Layer, LayerSource, MatteKind, Quality};
use effectcraft_raster::{Image, Sampling};
use effectcraft_time::Tick;

/// Everything a nested comp render (`Renderer::comp_frame` one level down) depends on: two
/// renders with equal keys produce identical pixels. Shared objects compare by address (they
/// outlive the frame the key is used in).
#[derive(Clone, Debug, PartialEq)]
pub struct NestedKey {
    item: ItemId,
    time: Tick,
    depth: usize,
    inherited: Option<Quality>,
    pipe: color::Pipe,
    /// `RenderOpts` has no `PartialEq`: its `Debug` text (floats print round-trip exact).
    opts: String,
    refs: [usize; 7],
}

fn addr<T: ?Sized>(r: &T) -> usize {
    std::ptr::from_ref(r).cast::<()>().addr()
}

impl<'a> Renderer<'a> {
    /// [`Renderer::draw_layer`]'s off-canvas test for a 2D layer drawn on a `size` canvas (see
    /// `off_canvas`): `true` = the layer certainly changes no pixel of it. The same predicate as
    /// the CPU compositor's, so the two cull exactly the same layers.
    pub fn culled_off_canvas(&self, ctx: &EvalCtx, layer: &Layer, size: (u32, u32)) -> bool {
        self.off_canvas(ctx, layer, size, &MotionMemo::default())
    }

    /// The layer's (non-inverted) track matte is certainly empty: the matte layer is outside its
    /// in/out span or at zero opacity, so the matte image is all zeros, the matted layer
    /// transparent, and blending it (any mode but the stencils, which change what they don't
    /// cover) leaves the comp as it was. Inverted mattes show the whole layer then: never skipped.
    pub fn matte_empty(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        let Some((m, kind)) = self.track_matte(ctx, layer) else { return false };
        if !matches!(kind, MatteKind::Alpha | MatteKind::Luma) || layer.blend_mode.is_stencil() {
            return false;
        }
        // Exactly the conditions under which the compositor leaves the matte image blank.
        !m.is_active_at(ctx.time) || (ctx.opacity(m) as f32) <= 0.0
    }

    /// The 2D layer replaces every pixel of a `w`×`h` canvas with its own: an opaque solid
    /// (alpha exactly 1 after quantising), Normal mode at 100 % opacity, with no masks, effects,
    /// layer styles, track matte, Preserve Transparency or motion-blur sub-samples, placed so
    /// that every canvas pixel samples the solid's interior (no edge texel, no minification).
    /// Nearest and bilinear sampling of a constant alpha of 1 give exactly 1 there (bilinear
    /// as `a + (b − a)·t`), and Normal over with source alpha 1 and opacity 1 returns the
    /// source whatever is below. Anything else (bicubic, rotation into partial pixels, …) is
    /// not an occluder.
    pub fn opaque_cover(&self, ctx: &EvalCtx, layer: &Layer, (w, h): (u32, u32)) -> bool {
        let LayerSource::Solid { item } = layer.source else { return false };
        let Some(ItemKind::Solid(sol)) = self.project.item(item).map(|i| &i.kind) else { return false };
        if layer.is_3d()
            || layer.switches.adjustment
            || layer.blend_mode != BlendMode::Normal
            || layer.track_matte.is_some()
            || layer.preserve_transparency
            || layer.masks().is_some_and(|m| !m.children.is_empty())
            || layer.effects().is_some_and(|f| !f.children.is_empty())
            || styles::active(ctx, layer)
            || self.quality(layer) == Quality::Wireframe
            || self.collapsed(ctx, layer).is_some()
            || self.layer_opacity(ctx, layer).partial_cmp(&1.0).is_none_or(std::cmp::Ordering::is_lt)
        {
            return false;
        }
        // `source`'s solid pixel: its alpha only depends on the input alpha of 1.
        if self.pipe.quantize_px([0.0, 0.0, 0.0, 1.0])[3] != 1.0 {
            return false;
        }
        let s = self.solid_scale(sol.width, sol.height);
        let bw = ((sol.width as f64 * s).ceil() as u32).max(1);
        let bh = ((sol.height as f64 * s).ceil() as u32).max(1);
        let shape = Buf { img: Image { width: bw, height: bh, data: Vec::new() }, offset: [0.0; 2], scale: s };
        let pl = self.placement(ctx, layer, &shape);
        let [m] = pl.matrices.as_slice() else { return false };
        if !matches!(pl.sampling, Sampling::Nearest | Sampling::Bilinear) || m.0[2] != [0.0, 0.0, 1.0] || m.mean_scale() < 0.5 {
            return false;
        }
        let Some(inv) = m.inverse() else { return false };
        // Every pixel centre maps at least a texel inside the solid (the margin also absorbs
        // the kernel's f32 arithmetic and a resident buffer's rounded offset).
        let (x1, y1) = (w as f64 - 0.5, h as f64 - 0.5);
        [(0.5, 0.5), (x1, 0.5), (0.5, y1), (x1, y1)].iter().all(|&(x, y)| {
            let p = inv.apply(vec2(x, y));
            p.x >= 1.0 && p.y >= 1.0 && p.x <= bw as f64 - 1.0 && p.y <= bh as f64 - 1.0
        })
    }

    /// The identity of this (nested) renderer's frame of comp `item` at `time` (see
    /// [`NestedKey`]). `None` for renderers that draw into a parent (collapsed precomps) or
    /// carry view state: those are never shared.
    pub fn nested_key(&self, item: ItemId, time: Tick) -> Option<NestedKey> {
        if self.depth == 0
            || self.top
            || self.outer.is_some()
            || self.opacity_mul != 1.0
            || self.collapse3d.is_some()
            || self.defer_dof
            || self.through_lost
            || self.through.iter().any(Option::is_some)
            || self.canvas_origin != [0; 2]
            || self.out_comp.is_some()
            || self.window.is_some()
            || self.layer_view.is_some()
        {
            return None;
        }
        let refs = [
            addr(self.project),
            addr(self.footage),
            self.expr.map_or(0, addr),
            self.cache.map_or(0, addr),
            self.profile.map_or(0, addr),
            self.freeze.map_or(0, addr),
            self.accel.map_or(0, addr),
        ];
        Some(NestedKey { item, time, depth: self.depth, inherited: self.inherited, pipe: self.pipe, opts: format!("{:?}", self.opts), refs })
    }
}
