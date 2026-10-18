//! GPU Deep Glow (`ec.stylize.deepglow`; kernels in `shaders/fx_deepglow.wgsl`, every entry
//! point prefixed `fdg_`): the CPU effect's steps (`effects::misc::deep_glow`) with the pixel
//! loops as compute kernels. The glow input (threshold, mask layer, to linear) is one kernel,
//! the three Gaussian scales are the shared box passes on the layer's rectangle grown by the
//! glow's reach (the CPU's sparse rectangles are the same frame blur restricted to where it can
//! be non-zero; the padded frame itself is only written by the key and the finish), the
//! working-resolution and Downsample resamples are `ops::warp` (`raster::resample`), and the
//! weighted sum, exposure, tint, composite, Unmult and Dither are two more kernels. The mask
//! layer is fitted on the CPU (`util::fit_layer`) and uploaded.

use effectcraft_color::BlendMode;
use effectcraft_effects::{Buf, EffectCtx};
use effectcraft_geom::{Mat3, vec2};
use effectcraft_raster::{Image, Sampling};

use crate::context::{Enc, GpuImage, Params};
use crate::effects::{GBuf, gaussian_blur};
use crate::ops;

/// Compute entry points in `fx_deepglow.wgsl`.
pub(crate) const KERNELS: &[&str] = &["fdg_key", "fdg_sum", "fdg_finish"];

/// Effect ids implemented here.
pub(crate) const IDS: &[&str] = &["ec.stylize.deepglow"];

/// Run effect `id` (one of [`IDS`]); `None` = this parameter combination runs on the CPU.
pub(crate) fn apply(e: &mut Enc, id: &str, ctx: &EffectCtx, b: GBuf) -> Option<GBuf> {
    match id {
        "ec.stylize.deepglow" => deep_glow(e, ctx, b),
        _ => None,
    }
}

/// `raster::resample` of `src` to `w × h` (bilinear, box pre-filter when shrinking).
fn resample(e: &mut Enc, src: &GpuImage, w: u32, h: u32) -> Option<GpuImage> {
    if !e.g.fits(w, h) || src.width == 0 || src.height == 0 {
        return None;
    }
    let empty = e.image(w, h);
    let m = Mat3::scale(vec2(w as f64 / src.width as f64, h as f64 / src.height as f64));
    Some(ops::warp(e, &empty, src, &m, Sampling::Bilinear, BlendMode::Normal, 1.0, 0, None))
}

/// [`resample`] of `src` to `fw × fh`, drawn only on the `gw × gh` region of that frame at
/// (`origin`, `origin`) (an enlargement: the same bilinear samples as the whole-frame resample).
fn enlarge(e: &mut Enc, src: &GpuImage, fw: u32, fh: u32, gw: u32, gh: u32, origin: u32) -> Option<GpuImage> {
    if origin == 0 && (gw, gh) == (fw, fh) {
        return resample(e, src, fw, fh);
    }
    if !e.g.fits(gw, gh) || src.width == 0 || src.height == 0 {
        return None;
    }
    let empty = e.image(gw, gh);
    let o = -f64::from(origin);
    let m = Mat3::translate(vec2(o, o)) * Mat3::scale(vec2(fw as f64 / src.width as f64, fh as f64 / src.height as f64));
    Some(ops::warp(e, &empty, src, &m, Sampling::Bilinear, BlendMode::Normal, 1.0, 0, None))
}

/// How far (full-resolution pixels) past the glow input's non-zero pixels the three weighted
/// passes can be non-zero: the box radii of the two full-resolution blurs, and for the
/// downsampled one its radii at the small size plus the resamples' pre-filter and bilinear
/// taps, rounded up generously (past it every pass is exactly transparent).
fn glow_reach(radius: f64, sigmas: [f64; 3], aspect: f64, downsample: f64) -> u32 {
    let support = |sigma: f64| effectcraft_raster::gaussian_support(sigma) as f64;
    let mut reach = 0.0f64;
    for (step, sigma) in sigmas.into_iter().enumerate() {
        for s in [radius * sigma * aspect, radius * sigma * (2.0 - aspect)] {
            let r = if step == 2 && downsample < 0.999 {
                // Small-size radii, plus 4 small pixels of bilinear taps, plus the box
                // halvings' 2^levels ≤ 2 / downsample full pixels, plus slack.
                (support(s * downsample) + 4.0) / downsample + 2.0 / downsample + 4.0
            } else {
                support(s) + 2.0
            };
            reach = reach.max(r);
        }
    }
    if reach.is_finite() { reach.ceil().min(f64::from(u32::MAX / 4)) as u32 } else { u32::MAX / 4 }
}

/// The weighted pass sum `a · wa · fa + b · wb · fb` (per channel, the CPU's operation order).
fn weighted_sum(e: &mut Enc, a: &GpuImage, wa: f32, fa: [f32; 4], b: &GpuImage, wb: f32, fb: [f32; 4]) -> GpuImage {
    let mut p = Params::default();
    p.f[0] = fa;
    p.f[1] = fb;
    p.f[2] = [wa, wb, 0.0, 0.0];
    let out = e.scratch(a.width, a.height);
    e.pixels("fdg_sum", &p, a, Some(b), &out, None);
    out
}

fn deep_glow(e: &mut Enc, ctx: &EffectCtx, mut b: GBuf) -> Option<GBuf> {
    let pr = ctx.params;
    let radius = (pr.f("PEDG-0001") * b.scale).clamp(0.0, 2000.0);
    if !radius.is_finite() || radius < 0.5 {
        return Some(b);
    }
    // The CPU's padding: Edge Behaviour / Buffer Expansion, bounded to 16 Mpx.
    let edge = pr.e("PEDG-0055");
    let wanted_pad = if edge == 0 || ctx.adjustment { 0 } else { (radius * if pr.e("PEDG-0084") == 1 { 1.5 } else { 1.0 }).ceil() as u32 };
    let mut low = 0;
    let mut high = wanted_pad;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let width = u64::from(b.img.width) + 2 * u64::from(mid);
        let height = u64::from(b.img.height) + 2 * u64::from(mid);
        if width * height <= 16_000_000 {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    // The padded frame is not built: the key reads the layer at (pad, pad) in it and the
    // finish writes the frame directly.
    let pad = low;
    let (w0, h0) = (b.img.width, b.img.height);
    let (fw, fh) = (w0.checked_add(pad.checked_mul(2)?)?, h0.checked_add(pad.checked_mul(2)?)?);
    if !e.g.fits(fw, fh) {
        return None;
    }
    b.offset[0] += f64::from(pad);
    b.offset[1] += f64::from(pad);
    let input = b.img.clone();
    let threshold = (pr.f("PEDG-0004") / 100.0).max(0.0) as f32;
    let smooth = (pr.f("PEDG-0005") / 100.0).max(0.0) as f32;
    let mask = match ctx.layer_param("PEDG-0056", true) {
        Some(o) => {
            let cpu = Buf { img: Image::new(fw, fh), offset: b.offset, scale: b.scale };
            Some(e.g.upload_image(&effectcraft_effects::util::fit_layer(ctx, &cpu, &o, false))?)
        }
        None => None,
    };
    let view_input = pr.e("PEDG-0023") == 0;
    let linear_model = pr.b("PEDG-0043") || pr.b("PEDG-0009");
    let convert = linear_model && !ctx.env.working_linear && !view_input;
    let unmult = pr.b("PEDG-0025");
    let mut p = Params::default();
    p.u[0] = [mask.is_some() as u32, (pr.e("PEDG-0057") == 1) as u32, pr.b("PEDG-0059") as u32, convert as u32];
    p.u[1] = [unmult as u32, pad, 0, 0];
    p.f[0] = [threshold, smooth, 0.0, 0.0];
    let source = e.scratch(fw, fh);
    e.pixels("fdg_key", &p, &input, mask.as_ref(), &source, None);
    drop(mask);
    // View 1 (Glow Input): the keyed, masked input as it is.
    if view_input {
        b.img = source;
        return Some(b);
    }
    let spread = (pr.f("PEDG-0011") / 100.0).clamp(0.0, 1.0) as f32;
    let near = (0.30 - 0.35 * spread).max(0.05);
    let far = 0.30 + 0.44 * spread;
    let core = if pr.e("PEDG-0008") == 0 { 3.0 } else { 6.6 };
    let weights = if linear_model { [core, 2.54, 1.30] } else { [near * 13.5, (1.0 - near - far) * 13.5, far * 13.5] };
    let sigmas = if linear_model { [0.003, 0.03, 0.12] } else { [0.035, 0.14, 0.42] };
    let aspect = pr.f("PEDG-0010").clamp(0.0, 2.0);
    let chroma = if pr.b("PEDG-0013") { pr.f("PEDG-0015").clamp(0.0, 2.0) as f32 } else { 0.0 };
    let downsample = (pr.f("PEDG-0019") / 100.0).clamp(0.1, 1.0);
    let work_scale = (512.0 / radius).min(1.0);
    // The glow is transparent past the layer's rectangle grown by the blurs' reach, so at the
    // full working resolution the blurs and sums run on that region of the frame only (a glow
    // much narrower than its Radius pad: the blob's 300 px pad glows ~110 px).
    let reach = if work_scale < 1.0 { pad } else { glow_reach(radius, sigmas, aspect, downsample).min(pad) };
    let origin = pad - reach;
    let (gw, gh) = (w0 + 2 * reach, h0 + 2 * reach);
    let work = if work_scale < 1.0 {
        let w = ((fw as f64 * work_scale).round() as u32).max(1);
        let h = ((fh as f64 * work_scale).round() as u32).max(1);
        resample(e, &source, w, h)?
    } else if origin > 0 {
        e.crop(&source, origin, origin, gw, gh)
    } else {
        source.clone()
    };
    // The frame the working images stand for (their own size, or the region's whole frame).
    let (ww, wh) = if work_scale < 1.0 { (work.width, work.height) } else { (fw, fh) };
    let mut parts = Vec::with_capacity(3);
    for (step, sigma) in sigmas.into_iter().enumerate() {
        let (sx, sy) = (radius * work_scale * sigma * aspect, radius * work_scale * sigma * (2.0 - aspect));
        let part = if step == 2 && downsample < 0.999 {
            // Downsampled from the whole frame (the same samples and pre-filter levels as before),
            // enlarged back onto the working region only.
            let full = if work_scale < 1.0 { &work } else { &source };
            let (w, h) = ((ww as f64 * downsample).round().max(1.0) as u32, (wh as f64 * downsample).round().max(1.0) as u32);
            let small = resample(e, full, w, h)?;
            let blurred = gaussian_blur(e, &small, sx * downsample, sy * downsample, false);
            enlarge(e, &blurred, ww, wh, work.width, work.height, if work_scale < 1.0 { 0 } else { origin })?
        } else {
            gaussian_blur(e, &work, sx, sy, false)
        };
        parts.push(part);
    }
    drop(work);
    drop(source);
    let [p0, p1, p2] = <[GpuImage; 3]>::try_from(parts).ok()?;
    let fringe = |step: usize| [[1.0, 1.0, 1.0 + chroma][step], 1.0, [1.0 + chroma, 1.0, 1.0][step], 1.0];
    let first = weighted_sum(e, &p0, weights[0], fringe(0), &p1, weights[1], fringe(1));
    let mut halo = weighted_sum(e, &first, 1.0, [1.0; 4], &p2, weights[2], fringe(2));
    if work_scale < 1.0 {
        halo = resample(e, &halo, fw, fh)?;
    }
    let exposure = pr.f("PEDG-0002").clamp(0.0, 64.0) as f32;
    let exposure = if linear_model { exposure.powf(0.75) } else { exposure };
    let tint = pr.color("PEDG-0038");
    let mut p = Params::default();
    p.u[0] = [convert as u32, unmult as u32, (pr.e("PEDG-0008") == 0) as u32, pr.b("PEDG-0037") as u32];
    p.u[1] = [pr.e("PEDG-0039"), pr.b("PEDG-0031") as u32, pr.b("PEDG-0032") as u32, fw];
    p.u[2] = [ctx.seed, pad, if work_scale < 1.0 { 0 } else { origin }, 0];
    p.f[0] = [
        exposure,
        (pr.f("PEDG-0024") / 100.0).clamp(0.0, 1.0) as f32,
        (pr.f("PEDG-0041") / 100.0).clamp(0.0, 1.0) as f32,
        (pr.f("PEDG-0033") / 100.0).clamp(0.0, 1.0) as f32 / 255.0,
    ];
    p.f[1] = [tint[0], tint[1], tint[2], 0.0];
    let out = e.scratch(fw, fh);
    e.pixels("fdg_finish", &p, &input, Some(&halo), &out, None);
    b.img = out;
    Some(b)
}
