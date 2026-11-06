//! Classic 3D planes made on the GPU: a shape layer's effects (`effects::GpuFx`) and Layer
//! Styles (`fx_styles`) run on the device and the plane is drawn from the result. The CPU path
//! reads each GPU effect run back, styles the layer on the CPU and uploads the plane again: a
//! 3D shape on a comp-sized effect canvas at its continuous-raster scale moves hundreds of
//! megabytes a frame that way (P01 Clock's circle: 39 megapixels).

use std::collections::HashMap;
use std::sync::Arc;

use effectcraft_effects::Buf;
use effectcraft_project::{Layer, LayerSource};
use effectcraft_raster::Image;
use effectcraft_render::three_d::{PlaneBufs, plane_key};
use effectcraft_render::{EvalCtx, Renderer, styles};

use crate::codes::Pixels;
use crate::context::{Enc, GpuImage, Params};

/// Plane-sized images the GPU styles hold at once, at most (see [`plane`]).
const STYLE_IMAGES: u64 = 9;

/// Styled planes whose styles ran on part of the plane only (its view, see [`plane`]).
#[cfg(test)]
pub(crate) static CUT_TO_VIEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static MADE_ON_GPU: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The planes of `run` the GPU makes for an `out` canvas (see [`plane`]): their pixel-less
/// buffers for `Renderer::prepare_3d_run_with`, and their images by the address of those
/// buffers for `classic3d::draw_run_made`.
pub(crate) fn make(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, run: &[&Layer], out: (u32, u32)) -> (PlaneBufs, HashMap<usize, Pixels>) {
    let mut bufs = PlaneBufs::new();
    let mut made = HashMap::new();
    for &l in run {
        match plane(e, r, ctx, l, out) {
            Ok((shape, img)) => {
                let shape = Arc::new(shape);
                made.insert(Arc::as_ptr(&shape) as usize, img);
                bufs.insert(plane_key(l), shape);
            }
            Err(why) => log::debug!("3D plane {:?} on the CPU: {why}", l.name),
        }
    }
    (bufs, made)
}

/// A 3D shape layer's plane made on the GPU. Without Layer Styles: [`shape_plane`]. With them:
/// its source (with masks, from the CPU), its effect stack, then its Layer Styles when every one
/// renders on the GPU and none paints outside the layer (Drop Shadow, Outer Glow: the CPU
/// flattens those passes). `None` leaves the layer to the CPU: an unchanged layer the layer
/// cache holds already (one it doesn't hold is made here: the CPU would style it once at its
/// full raster size, seconds for a large Bevel and Emboss),
/// linear blending, or a step the GPU can't do. The styles only style the part of the plane the
/// `out` canvas can show (and what reaches it): a layer scaled up far past the frame keeps
/// its styles' images small.
fn plane(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer, out: (u32, u32)) -> Result<(Buf, Pixels), &'static str> {
    if r.pipe().to_blend().is_some() {
        return Err("linear blending");
    }
    if r.layer_is_static(ctx, layer) && r.layer_buf_in_memory(ctx, layer) {
        return Err("held, in the layer cache");
    }
    if matches!(layer.source, LayerSource::Comp { .. }) {
        return precomp_plane(e, r, ctx, layer);
    }
    let styled = styles::active(ctx, layer);
    let has_fx = layer.switches.effects && layer.effects().is_some_and(|f| !f.children.is_empty());
    if !matches!(layer.source, LayerSource::Shape) || layer.switches.adjustment {
        return Err("not a shape layer, nor a precomp");
    }
    if !styled {
        return shape_plane(e, r, ctx, layer, has_fx);
    }
    // Only this plane's own steps count (see `alpha_bounds`).
    e.take_missing();
    let input = r.layer_input(ctx, layer, 0).ok_or("no source")?;
    if input.img.is_empty() {
        return Err("empty source");
    }
    let src = e.upload_buf(&input).ok_or("source upload")?;
    let mut fx = crate::effects::GpuFx::new(e, src, input.offset, input.scale, r.pipe().levels);
    let origin = r.run_layer_effects_on(ctx, layer, &mut fx);
    let (img, offset, scale) = fx.finish().ok_or("an effect the GPU can't run")?;
    let offset = [offset[0] - origin[0] * scale, offset[1] - origin[1] * scale];
    if img.width == 0 || img.height == 0 {
        return Err("empty after effects");
    }
    log::debug!(
        "3D plane {:?}: effect canvas {}x{}, seen {:?}",
        layer.name,
        img.width,
        img.height,
        r.plane_view_rect(ctx, layer, out)
            .map(|v| [v[0] * scale + offset[0], v[1] * scale + offset[1], v[2] * scale + offset[0], v[3] * scale + offset[1]].map(|x| x.round()))
    );
    let bounds = alpha_bounds(e, &img)?;
    let size = effectcraft_render::source_size(r.project, layer);
    let layer_size = if size.0 == 0 { [ctx.comp.width as f64, ctx.comp.height as f64] } else { [size.0 as f64, size.1 as f64] };
    // The seen part of the plane in content pixels, plus 4 for the plane's sampling filter.
    let view = r
        .plane_view_rect(ctx, layer, out)
        .map(|v| [v[0] * scale + offset[0] - 4.0, v[1] * scale + offset[1] - 4.0, v[2] * scale + offset[0] + 4.0, v[3] * scale + offset[1] + 4.0]);
    // The plan's decline reason (`cpu_why::declined`) is for a 2D styled layer's note; this
    // plane has its own. A reason left before it stays for its caller.
    let pending = effectcraft_render::cpu_why::take();
    let prep = styles::gpu_prepare_shape(ctx, layer, [img.width, img.height], offset, scale, bounds, layer_size, view);
    let _ = effectcraft_render::cpu_why::take();
    if let Some(why) = pending {
        effectcraft_render::cpu_why::declined(why);
    }
    let prep = prep.ok_or("a style the GPU can't run")?;
    if !prep.plan.pass_modes().is_empty() {
        return Err("styles that paint outside the layer");
    }
    let [x0, y0, x1, y1] = prep.region;
    #[cfg(test)]
    if bounds.is_some_and(|(a, b, c, d)| (x1 - x0) < c - a || (y1 - y0) < d - b) {
        CUT_TO_VIEW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let (pw, ph) = (prep.shape.img.width, prep.shape.img.height);
    if !e.g.fits(pw, ph) || x1 > img.width || y1 > img.height || x0 >= x1 || y0 >= y1 {
        return Err("style canvas too large");
    }
    // The styles hold about this many plane-sized images at once (Bevel and Emboss: distance
    // field, height, blurs, packed normals, the result). On a discrete card a third of its memory
    // is the most they take: running out retires readbacks, and every GPU step after that falls
    // back to the CPU.
    let need = u64::from(pw).saturating_mul(u64::from(ph)).saturating_mul(16 * STYLE_IMAGES);
    if e.g.video_memory().is_some_and(|m| need > m.total / 3) {
        return Err("styles would need too much video memory");
    }
    // A fresh zeroed image of its own: `Enc::zeros` is shared and must never be written.
    let padded = e.image(pw, ph);
    e.copy_region_to(&img, &padded, x0, y0, x1 - x0, y1 - y0, prep.pad, prep.pad);
    drop(img);
    let st = crate::fx_styles::render(e, &padded, &prep.plan).ok_or("style kernels failed")?;
    finish(e, prep.shape, st.body.into())
}

/// A 3D precomp layer's plane made on the GPU: its nested comp walked on the device
/// (`cull::nested`, as a 2D precomp layer's `walk::draw_precomp`) and its own effects run there
/// (`walk::prepare_processed`). The CPU path renders the nested comp on the CPU compositor, reads
/// every GPU effect run inside it back and uploads the plane again (10-10 MAIN's Clock 1 and
/// Clock 2: their glowing circles' effect canvases, about a second a frame). Masks, Layer
/// Styles, a collapsed precomp and whatever `Renderer::precomp_source` declines stay on the CPU.
fn precomp_plane(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> Result<(Buf, Pixels), &'static str> {
    if layer.switches.collapse || layer.switches.adjustment {
        return Err("a collapsed or adjustment precomp");
    }
    if layer.masks().is_some_and(|m| !m.children.is_empty()) || styles::active(ctx, layer) {
        return Err("a precomp with masks or styles");
    }
    let (sub, item, lt, bs) = r.precomp_source(ctx, layer).ok_or("a precomp the CPU renders")?;
    let clip = r.precomp_window(ctx, layer);
    let sub = sub.with_clip(clip);
    // Only this plane's own steps count (see `alpha_bounds`).
    e.take_missing();
    let src = crate::cull::nested(e, ctx, &sub, item, lt).ok_or("a nested comp the GPU declines")?;
    let image = crate::walk::prepare_processed(e, r, ctx, layer, src, bs, clip.offset(), false).ok_or("an effect the GPU can't run")?;
    let (w, h) = image.img.size();
    if w == 0 || h == 0 {
        return Err("empty precomp");
    }
    let shape = Buf { img: Image { width: w, height: h, data: Vec::new() }, offset: image.offset, scale: image.scale };
    finish(e, shape, image.img)
}

/// A 3D shape layer without Layer Styles (with or without effects): its plane is the layer's
/// Processed pixels (source → masks → effects, layer space), made on the GPU as a 2D shape
/// layer's are: `shapes_gpu` rasterises its paths and paints its fills, strokes and gradients
/// with the CPU's own coverage and gradient model (no per-pixel gradient sampling on the CPU, no
/// 8 bpc packing and upload: C0666's BG Glow planes, 14 radial-gradient ellipses of 1900 px,
/// were most of a frame's CPU time), then its effects run on the device. Through
/// `walk::processed_resident` (as a 2D layer: the retained image, or the layer cache's buffer
/// uploaded, else made here and retained when its content repeats); when this encoder can't
/// retain images, made here each frame. Masks the GPU can't draw, non-Normal draw blend modes,
/// Taper / Wave strokes and effects without a GPU kernel leave the layer to the CPU.
fn shape_plane(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer, has_fx: bool) -> Result<(Buf, Pixels), &'static str> {
    if !has_fx && !crate::shapes_gpu::eligible(r, ctx, layer) {
        return Err("a shape the GPU can't rasterise");
    }
    // Only this plane's own steps count (see `alpha_bounds`).
    e.take_missing();
    if let Some((image, keep)) = crate::walk::processed_resident(e, r, ctx, layer, false) {
        let (w, h) = (image.img.width(), image.img.height());
        if w == 0 || h == 0 {
            return Err("empty source");
        }
        let shape = Buf { img: Image { width: w, height: h, data: Vec::new() }, offset: image.offset, scale: image.scale };
        let made = finish(e, shape, image.img.clone())?;
        crate::walk::keep_processed(e, r, ctx, layer, keep, image);
        return Ok(made);
    }
    let clip = r.content_clip(ctx, layer);
    let (src, at, bs) = match crate::shapes_gpu::resident_source(e, r, ctx, layer, clip) {
        Some(made) => made,
        // The source from the CPU, the effects here (as before shape planes were made here).
        None if has_fx => {
            let input = r.layer_input(ctx, layer, 0).ok_or("no source")?;
            if input.img.is_empty() {
                return Err("empty source");
            }
            (e.upload_buf(&input).ok_or("source upload")?, input.offset, input.scale)
        }
        None => return Err("a shape the GPU can't rasterise"),
    };
    let mut fx = crate::effects::GpuFx::new(e, src, at, bs, r.pipe().levels);
    let origin = r.run_layer_effects_in(ctx, layer, &mut fx, clip);
    let (img, offset, scale) = fx.finish().ok_or("an effect the GPU can't run")?;
    if img.width == 0 || img.height == 0 {
        return Err("empty after effects");
    }
    let offset = [offset[0] - origin[0] * scale, offset[1] - origin[1] * scale];
    let shape = Buf { img: Image { width: img.width, height: img.height, data: Vec::new() }, offset, scale };
    finish(e, shape, img.into())
}

fn finish(e: &mut Enc, shape: Buf, img: Pixels) -> Result<(Buf, Pixels), &'static str> {
    // A plane on the 8 bpc grid waits for its run as its codes (LVR1: a quarter of the bytes,
    // the same values; the run's atlas is made of codes then), its float image back in the pool
    // for the next plane: C0666 N2's 14 glow planes are ~800 MB a frame as floats.
    let img = if matches!(&img, Pixels::Float(f) if f.levels == Some(crate::codes::LEVELS)) {
        match crate::codes::exact_codes(e, &img) {
            Some(c) => Pixels::Codes(c),
            None => img,
        }
    } else {
        img
    };
    if e.take_missing() || e.g.check_health().is_err() {
        return Err("a kernel is missing or the device failed");
    }
    #[cfg(test)]
    MADE_ON_GPU.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok((shape, img))
}

/// `Image::alpha_bounds` of a GPU image (pixels with alpha over 1e-6): one row each per
/// thread (`sty_bounds`), the per-row spans read back. `Some(None)` = nothing visible; `None` =
/// the device failed.
pub(crate) fn alpha_bounds(e: &mut Enc, img: &GpuImage) -> Result<Option<(u32, u32, u32, u32)>, &'static str> {
    // A flag left by earlier work on this encoder isn't this step's (as `fx_styles::render`).
    e.take_missing();
    if !e.g.fits(img.height, 1) {
        return Err("alpha bounds: too many rows");
    }
    let rows = e.scratch(img.height, 1);
    e.dispatch("sty_bounds", &Params::default(), img, None, &rows, None, (img.height.div_ceil(64), 1));
    if e.take_missing() {
        return Err("alpha bounds: no sty_bounds kernel");
    }
    if !e.g.can_wait() {
        return Err("alpha bounds: readbacks can't be waited for");
    }
    let spans = e.download(&rows).ok_or("alpha bounds: readback failed")?;
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
    Ok(b)
}
