//! Classic 3D planes made on the GPU: a shape layer's effects (`effects::GpuFx`) and Layer
//! Styles (`fx_styles`) run on the device and the plane is drawn from the result. The CPU path
//! reads each GPU effect run back, styles the layer on the CPU and uploads the plane again: a
//! 3D shape on a comp-sized effect canvas at its continuous-raster scale moves hundreds of
//! megabytes a frame that way (P01 Clock's circle: 39 megapixels).

use std::collections::HashMap;
use std::sync::Arc;

use effectcraft_project::{Layer, LayerSource};
use effectcraft_raster::Image;
use effectcraft_render::three_d::{PlaneBufs, plane_key};
use effectcraft_effects::Buf;
use effectcraft_render::{EvalCtx, Renderer, styles};

use crate::context::{Enc, GpuImage, Params};

#[cfg(test)]
pub(crate) static MADE_ON_GPU: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The planes of `run` the GPU makes (see [`plane`]): their pixel-less buffers for
/// `Renderer::prepare_3d_run_with`, and their images by the address of those buffers for
/// `classic3d::draw_run_made`.
pub(crate) fn make(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, run: &[&Layer]) -> (PlaneBufs, HashMap<usize, GpuImage>) {
    let mut bufs = PlaneBufs::new();
    let mut made = HashMap::new();
    for &l in run {
        if let Some((shape, img)) = plane(e, r, ctx, l) {
            let shape = Arc::new(shape);
            made.insert(Arc::as_ptr(&shape) as usize, img);
            bufs.insert(plane_key(l), shape);
        }
    }
    (bufs, made)
}

/// A 3D shape layer's plane made on the GPU: its source (with masks, from the CPU), its effect
/// stack, then its Layer Styles when every one renders on the GPU and none paints outside the
/// layer (Drop Shadow, Outer Glow: the CPU flattens those passes). `None` leaves the layer to
/// the CPU: no effects or styles to run, an unchanged layer (the layer cache holds it),
/// linear blending, or a step the GPU can't do.
fn plane(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> Option<(Buf, GpuImage)> {
    let styled = styles::active(ctx, layer);
    let has_fx = layer.switches.effects && layer.effects().is_some_and(|f| !f.children.is_empty());
    if !matches!(layer.source, LayerSource::Shape)
        || layer.switches.adjustment
        || !(styled || has_fx)
        || r.pipe().to_blend().is_some()
        || r.layer_is_static(ctx, layer)
    {
        return None;
    }
    let input = r.layer_input(ctx, layer, 0)?;
    if input.img.is_empty() {
        return None;
    }
    let src = e.upload_buf(&input)?;
    let mut fx = crate::effects::GpuFx::new(e, src, input.offset, input.scale, r.pipe().levels);
    let origin = r.run_layer_effects_on(ctx, layer, &mut fx);
    let (img, offset, scale) = fx.finish()?;
    let offset = [offset[0] - origin[0] * scale, offset[1] - origin[1] * scale];
    if img.width == 0 || img.height == 0 {
        return None;
    }
    if !styled {
        let shape = Buf { img: Image { width: img.width, height: img.height, data: Vec::new() }, offset, scale };
        return finish(e, shape, img);
    }
    let bounds = alpha_bounds(e, &img)?;
    let size = effectcraft_render::source_size(r.project, layer);
    let layer_size = if size.0 == 0 { [ctx.comp.width as f64, ctx.comp.height as f64] } else { [size.0 as f64, size.1 as f64] };
    let prep = styles::gpu_prepare_shape(ctx, layer, [img.width, img.height], offset, scale, bounds, layer_size)?;
    if !prep.plan.pass_modes().is_empty() {
        return None;
    }
    let [x0, y0, x1, y1] = prep.region;
    let padded = e.zeros(prep.shape.img.width, prep.shape.img.height);
    let part = e.crop(&img, x0, y0, x1 - x0, y1 - y0);
    e.copy_into(&part, &padded, prep.pad, prep.pad);
    let st = crate::fx_styles::render(e, &padded, &prep.plan)?;
    finish(e, prep.shape, st.body)
}

fn finish(e: &mut Enc, shape: Buf, img: GpuImage) -> Option<(Buf, GpuImage)> {
    if e.take_missing() || e.g.check_health().is_err() {
        return None;
    }
    #[cfg(test)]
    MADE_ON_GPU.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some((shape, img))
}

/// `Image::alpha_bounds` of a GPU image (pixels with alpha over 1e-6): one row each per
/// thread (`sty_bounds`), the per-row spans read back. `Some(None)` = nothing visible; `None` =
/// the device failed.
fn alpha_bounds(e: &mut Enc, img: &GpuImage) -> Option<Option<(u32, u32, u32, u32)>> {
    let rows = e.scratch(img.height, 1);
    e.dispatch("sty_bounds", &Params::default(), img, None, &rows, None, (img.height.div_ceil(64), 1));
    if e.take_missing() {
        return None;
    }
    let spans = e.download(&rows)?;
    let mut b: Option<(u32, u32, u32, u32)> = None;
    for (y, px) in spans.data.iter().enumerate() {
        if px[3] <= 0.0 {
            continue;
        }
        let (f, l, y) = (px[0] as u32, px[1] as u32, y as u32);
        b = Some(match b {
            None => (f, y, l, y + 1),
            Some((x0, y0, x1, _)) => (x0.min(f), y0, x1.max(l), y + 1),
        });
    }
    Some(b)
}
