//! Compositing primitives: each mirrors a CPU operation in `effectcraft-raster` /
//! `effectcraft-render` and returns a new image (GPU images are immutable; only the `*_in`
//! forms draw on a canvas in place, one no one else can see).

use effectcraft_color::{BlendMode, Conversion};
use effectcraft_geom::{Mat3, Rect, vec2};
use effectcraft_project::MatteKind;
use effectcraft_raster::Sampling;

use crate::codes::{self, Pixels};
use crate::context::{Enc, GpuImage, Params};

/// Index of a blend mode in `BlendMode::ALL` (the kernels' mode numbers).
pub fn mode_id(m: BlendMode) -> u32 {
    BlendMode::ALL.iter().position(|x| *x == m).unwrap_or(0) as u32
}

fn sampling_id(s: Sampling) -> u32 {
    match s {
        Sampling::Nearest => 0,
        Sampling::Bilinear => 1,
        Sampling::Bicubic => 2,
    }
}

/// A rectangle `[x0, y0, x1, y1)` of canvas pixels (empty when `x0 >= x1` or `y0 >= y1`).
pub(crate) type Bounds = [u32; 4];

/// The canvas pixels (of a `dst`-sized canvas) a warp of a `src`-sized image by `m` can reach:
/// outside them a warp pass leaves the canvas as it is (`warp_blend_px`, `warp_accum_px`).
pub(crate) fn warp_bounds(dst: (u32, u32), src: (u32, u32), m: &Mat3) -> Bounds {
    let bounds = m.map_rect(&Rect::from_size(src.0 as f64, src.1 as f64)).inflate(1.0).round_out();
    let area = bounds.intersect(&Rect::from_size(dst.0 as f64, dst.1 as f64));
    if area.is_empty() {
        return [0; 4];
    }
    [area.x0.max(0.0) as u32, area.y0.max(0.0) as u32, area.x1.min(dst.0 as f64) as u32, area.y1.min(dst.1 as f64) as u32]
}

/// The smallest rectangle holding `a` and `b`.
pub(crate) fn union(a: Bounds, b: Bounds) -> Bounds {
    let empty = |r: Bounds| r[0] >= r[2] || r[1] >= r[3];
    match (empty(a), empty(b)) {
        (true, _) => b,
        (_, true) => a,
        _ => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
    }
}

/// The copy of the rectangle `b` of the canvas `dst` that the in-place pass `entry` (a `*_at`
/// kernel: `dst` is its output, the copy its input, as a pass can't read and write one
/// texture) reads, with the pass's workgroups; `None` when the full-canvas pass must run
/// instead: `dst` isn't this encoder's own ([`Enc::owns`]), the kernel didn't build, or `b`
/// covers half the canvas or more (cheaper as one full pass: in place, `b` is copied out, then
/// read and written again). The caller checks that the full pass leaves the pixels outside
/// `b` as they are.
fn at(e: &mut Enc, dst: &Pixels, b: Bounds, entry: &'static str, src_codes: bool) -> Option<(Pixels, (u32, u32))> {
    let owned = e.owns_px(dst);
    at_parts(e, dst.size(), owned, dst.is_codes(), px_texture(dst), b, entry, src_codes)
}

/// [`at`] of an f32 canvas.
fn at_f32(e: &mut Enc, dst: &GpuImage, b: Bounds, entry: &'static str, src_codes: bool) -> Option<(GpuImage, (u32, u32))> {
    let owned = e.owns(dst);
    match at_parts(e, (dst.width, dst.height), owned, false, &dst.texture, b, entry, src_codes)? {
        (Pixels::Float(under), groups) => Some((under, groups)),
        (Pixels::Codes(_), _) => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn at_parts(
    e: &mut Enc,
    (dw, dh): (u32, u32),
    owned: bool,
    dst_codes: bool,
    dst_texture: &wgpu::Texture,
    [x0, y0, x1, y1]: Bounds,
    entry: &'static str,
    src_codes: bool,
) -> Option<(Pixels, (u32, u32))> {
    let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    let area = u64::from(w).saturating_mul(u64::from(h));
    let canvas = u64::from(dw).saturating_mul(u64::from(dh));
    let io = codes::Io { src: src_codes, aux: dst_codes, out: dst_codes };
    if w == 0 || h == 0 || area.saturating_mul(2) >= canvas || x1 > dw || y1 > dh || !owned || !has(e, entry, io) {
        return None;
    }
    // The copy goes into a texture rounded up to a size bucket: layer bounds change size from
    // frame to frame, and the pool only reuses a texture of the very same size. The `*_at`
    // kernels read only its top-left w × h. A codes canvas copies into codes.
    let under = scratch_px(e, size_bucket(w, dw), size_bucket(h, dh), dst_codes)?;
    e.copy_texture_region(dst_texture, px_texture(&under), x0, y0, w, h);
    Some((under, (w.div_ceil(16), h.div_ceil(16))))
}

/// Whether kernel `entry` exists for these representations (f32 kernels, or a codes variant).
fn has(e: &Enc, entry: &str, io: codes::Io) -> bool {
    if !(io.src || io.aux || io.out) {
        return e.has_kernel(entry);
    }
    e.g.codes.as_ref().is_some_and(|b| b.has(entry, io))
}

/// A new working image for a pass that writes every pixel: codes when `codes` (and the codes
/// kernels exist), else f32.
#[track_caller]
fn scratch_px(e: &mut Enc, w: u32, h: u32, codes: bool) -> Option<Pixels> {
    if codes {
        return e.scratch_codes(w, h).map(Pixels::Codes);
    }
    Some(Pixels::Float(e.scratch(w, h)))
}

fn px_texture(p: &Pixels) -> &wgpu::Texture {
    match p {
        Pixels::Float(g) => &g.texture,
        Pixels::Codes(c) => c.texture(),
    }
}

/// Whether a pass `entry` reading `src` and `aux` writes its result, quantised to `levels`, as
/// 8 bpc codes: at the 8 bpc grid, where the walk asked for codes (`codes_ok`) and the variant
/// exists. The values are the same either way; codes are a quarter of the bytes.
fn codes_out(e: &Enc, entry: &str, src: &Pixels, aux: &Pixels, levels: Option<f32>, codes_ok: bool) -> bool {
    codes_ok && levels == Some(codes::LEVELS) && has(e, entry, codes::Io { src: src.is_codes(), aux: aux.is_codes(), out: true })
}

/// `n` rounded up to 64 or more, then to the next 2^k or 1.5 · 2^k (at most `cap`, the canvas
/// side, unless `n` is larger).
fn size_bucket(n: u32, cap: u32) -> u32 {
    let n64 = n.max(64);
    let p = n64.checked_next_power_of_two().unwrap_or(n64);
    let b = if p / 4 * 3 >= n64 { p / 4 * 3 } else { p };
    b.min(cap).max(n)
}

fn rows(m: &Mat3) -> [[f32; 4]; 3] {
    m.0.map(|r| [r[0] as f32, r[1] as f32, r[2] as f32, 0.0])
}

/// Box-filter halving (minification pre-filter).
pub fn half(e: &mut Enc, src: &GpuImage) -> GpuImage {
    let out = e.scratch(src.width.div_ceil(2).max(1), src.height.div_ceil(2).max(1));
    e.pixels("half", &Params::default(), src, None, &out, None);
    out
}

/// [`half`] of f32 or codes (a level is a mean, off the 8 bpc grid: always f32).
pub(crate) fn half_px(e: &mut Enc, src: &Pixels) -> GpuImage {
    let out = e.scratch(src.width().div_ceil(2).max(1), src.height().div_ceil(2).max(1));
    codes::pixels(e, "half", &Params::default(), src, None, &Pixels::Float(out.clone()), None);
    out
}

/// `mips::minify` of f32 or codes: a codes image's first level is halved from the codes, the
/// rest as for any f32 image (the same steps and kernels, so the same pixels).
fn minify_px(e: &mut Enc, src: &Pixels, scale: f64) -> (GpuImage, f64) {
    match src {
        Pixels::Float(g) => crate::mips::minify(e, g, scale),
        Pixels::Codes(_) => {
            let first = half_px(e, src);
            if scale * 2.0 < 0.5 && first.width > 1 && first.height > 1 {
                let (img, f) = crate::mips::minify(e, &first, scale * 2.0);
                (img, f * 0.5)
            } else {
                (first, 0.5)
            }
        }
    }
}

/// `raster::composite_warp` (and, with `accumulate = Some(weight)`, `accumulate_warp`): draw
/// `src` transformed by `m` (src pixels → dst pixels) onto `dst`.
pub fn warp(
    e: &mut Enc,
    dst: &GpuImage,
    src: &GpuImage,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
) -> GpuImage {
    warp_q(e, dst, src, m, sampling, mode, opacity, seed, accumulate, None)
}

/// The image a warp samples: `src` itself, or its pre-filtered level for a minifying `m`
/// (halved until the remaining scale is ≥ ½; a retained image keeps its levels: `mips`), and the
/// inverse matrix into it. `None`: `m` can't be inverted.
fn sampled(e: &mut Enc, src: &Pixels, m: &Mat3) -> Option<(Pixels, Mat3)> {
    let inv = m.inverse()?;
    let scale = m.mean_scale();
    if scale < 0.5 && m.is_affine() {
        let (level, f) = minify_px(e, src, scale);
        return Some((Pixels::Float(level), Mat3::scale(vec2(f, f)) * inv));
    }
    Some((src.clone(), inv))
}

/// [`warp`], clamping and quantising the result to `levels` (8/16 bpc: the comp after a layer,
/// in the same pass).
#[allow(clippy::too_many_arguments)]
pub fn warp_q(
    e: &mut Enc,
    dst: &GpuImage,
    src: &GpuImage,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
    levels: Option<f32>,
) -> GpuImage {
    let dst_px = Pixels::Float(dst.clone());
    match warp_onto(e, &dst_px, &Pixels::Float(src.clone()), m, sampling, mode, opacity, seed, accumulate, levels, false, false) {
        Some(Pixels::Float(out)) => out,
        _ => dst.clone(),
    }
}

/// [`warp_q`] onto `dst` itself (`*dst = warp_q(e, dst, ..)`, the same pixels): drawn in
/// place over the layer's bounds only when this encoder owns `dst` ([`Enc::owns`]), instead of
/// writing a whole new canvas per layer. `dst` stays f32 (see [`warp_px_in`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_q_in(
    e: &mut Enc,
    dst: &mut GpuImage,
    src: &Pixels,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
    levels: Option<f32>,
) {
    // Taken out of `dst` (not cloned), so `Enc::owns` still sees the only handle.
    let mut px = Pixels::Float(std::mem::replace(dst, e.g.dummy.clone()));
    warp_in(e, &mut px, src, m, sampling, mode, opacity, seed, accumulate, levels, false);
    match px {
        Pixels::Float(out) => *dst = out,
        Pixels::Codes(_) => log::error!("gpu: an f32 canvas came back as codes"),
    }
}

/// [`warp_q_in`] on a canvas of f32 or codes; the result is codes when it is quantised to the
/// 8 bpc grid (see [`codes_out`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_px_in(
    e: &mut Enc,
    dst: &mut Pixels,
    src: &Pixels,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
    levels: Option<f32>,
) {
    warp_in(e, dst, src, m, sampling, mode, opacity, seed, accumulate, levels, true);
}

#[allow(clippy::too_many_arguments)]
fn warp_in(
    e: &mut Enc,
    dst: &mut Pixels,
    src: &Pixels,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
    levels: Option<f32>,
    codes_ok: bool,
) {
    let owned = e.owns_px(dst);
    match warp_onto(e, dst, src, m, sampling, mode, opacity, seed, accumulate, levels, owned, codes_ok) {
        Some(out) => *dst = out,
        None => {
            if let Pixels::Float(g) = dst {
                g.levels = levels;
            }
        }
    }
}

/// [`warp_q`]'s pass; with `in_place`, drawn into `dst` itself when [`warp_in_place`] can
/// (`None`).
#[allow(clippy::too_many_arguments)]
fn warp_onto(
    e: &mut Enc,
    dst: &Pixels,
    src: &Pixels,
    m: &Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    accumulate: Option<f32>,
    levels: Option<f32>,
    in_place: bool,
    codes_ok: bool,
) -> Option<Pixels> {
    if src.width() == 0 || src.height() == 0 || opacity <= 0.0 {
        return Some(dst.clone());
    }
    // Minification: a pre-filtered level so sampling doesn't alias.
    let Some((img, inv)) = sampled(e, src, m) else { return Some(dst.clone()) };
    let [x0, y0, x1, y1] = warp_bounds(dst.size(), src.size(), m);
    if x0 >= x1 || y0 >= y1 {
        return Some(dst.clone());
    }
    let mut p = Params::default();
    let clear = matches!(mode, BlendMode::StencilAlpha | BlendMode::StencilLuma);
    let flags = u32::from(accumulate.is_some()) | if clear && accumulate.is_none() { 2 } else { 0 };
    p.u[0] = [mode_id(mode), sampling_id(sampling), seed, flags];
    let r = rows(&inv);
    p.f[0] = r[0];
    p.f[1] = r[1];
    p.f[2] = r[2];
    p.f[3] = [opacity, accumulate.unwrap_or(1.0), levels.unwrap_or(0.0), levels.map_or(0.0, |v| 1.0 / v)];
    p.f[4] = [x0 as f32, y0 as f32, x1 as f32, y1 as f32];
    if in_place && warp_in_place(e, dst, &img, &p, [x0, y0, x1, y1], levels) {
        return None;
    }
    let (w, h) = dst.size();
    let mut out = scratch_px(e, w, h, codes_out(e, "warp_blend", &img, dst, levels, codes_ok))?;
    codes::pixels(e, "warp_blend", &p, &img, Some(dst), &out, None);
    if let Pixels::Float(o) = &mut out {
        o.levels = levels;
    }
    Some(out)
}

/// `warp_blend` in place: only the layer's bounds `[x0, y0, x1, y1)` of the canvas `dst` are
/// read and written (`warp_blend_at`), with the same per-pixel arithmetic and quantisation as
/// the full pass, which writes the pixels outside the bounds back unchanged. `false` (nothing
/// recorded) when the full pass must run instead: stencil modes clear outside the bounds, a
/// canvas not already quantised to `levels` is quantised outside them too, a codes canvas holds
/// only results quantised to the 8 bpc grid, and a layer covering half the canvas or more is
/// cheaper as one full pass (in place, its bounds are copied out, then read and written again).
fn warp_in_place(e: &mut Enc, dst: &Pixels, img: &Pixels, p: &Params, b: Bounds, levels: Option<f32>) -> bool {
    // Quantising is idempotent (see `quantize`): a canvas quantised to `levels` stays as it is.
    let outside_unchanged = p.u[0][3] & 2 == 0 && (levels.is_none() || dst.levels() == levels);
    if !outside_unchanged || (dst.is_codes() && levels != Some(codes::LEVELS)) {
        return false;
    }
    let Some((under, groups)) = at(e, dst, b, "warp_blend_at", img.is_codes()) else { return false };
    let mut q = *p;
    q.u[1] = b;
    codes::dispatch(e, "warp_blend_at", &q, img, Some(&under), dst, None, groups);
    true
}

/// Motion-blur sub-samples: `dst` plus `src` warped by each of `matrices` times `weight`, summed
/// in order. The same sums as one [`warp`] accumulate pass per matrix, in far fewer passes: each
/// minification level of `src` is built once and shared by the samples that use it, consecutive
/// samples at the same level sum in one `warp_accum` pass (one write of the canvas instead of one
/// per sample), and samples that land entirely off `dst` (which add exactly nothing) are skipped.
#[cfg(test)]
pub fn warp_accumulate(e: &mut Enc, dst: &GpuImage, src: &GpuImage, matrices: &[Mat3], sampling: Sampling, weight: f32) -> GpuImage {
    let mut acc = dst.clone();
    warp_accumulate_in(e, &mut acc, &Pixels::Float(src.clone()), matrices, sampling, weight);
    acc
}

/// `warp_accumulate` onto `dst` itself: each run summed in place over its samples' bounds
/// (`warp_accum_at`, the same per-pixel sums; `warp_accum` writes the pixels outside them back
/// unchanged) when this encoder owns `dst` (see [`warp_q_in`]). Returns the samples' bounds:
/// outside them `dst` is as it was.
pub(crate) fn warp_accumulate_in(e: &mut Enc, dst: &mut GpuImage, src: &Pixels, matrices: &[Mat3], sampling: Sampling, weight: f32) -> Bounds {
    let reach = matrices.iter().fold([0; 4], |u, m| union(u, warp_bounds((dst.width, dst.height), src.size(), m)));
    if src.width() == 0 || src.height() == 0 || !e.has_kernel("warp_accum") {
        for m in matrices {
            warp_q_in(e, dst, src, m, sampling, BlendMode::Normal, 1.0, 0, Some(weight), None);
        }
        return reach;
    }
    // Minification chain shared by the samples (level i = `src` halved i times).
    let mut mips: Vec<Pixels> = vec![src.clone()];
    // Runs of consecutive samples at the same level: (level, per-sample data, union bounds).
    let mut runs: Vec<(usize, Vec<f32>, [f32; 4])> = Vec::new();
    for m in matrices {
        let Some(inv0) = m.inverse() else { continue };
        // As `warp_q`: a pre-filtered level so sampling doesn't alias.
        let scale = m.mean_scale();
        let (mut level, mut inv) = (0usize, inv0);
        if scale < 0.5 && m.is_affine() {
            let (mut s, mut f) = (scale * 2.0, 0.5);
            level = 1;
            loop {
                while mips.len() <= level {
                    let Some(prev) = mips.last() else { break };
                    let next = half_px(e, prev);
                    mips.push(Pixels::Float(next));
                }
                let Some(img) = mips.get(level) else { break };
                if !(s < 0.5 && img.width() > 1 && img.height() > 1) {
                    break;
                }
                level += 1;
                f *= 0.5;
                s *= 2.0;
            }
            inv = Mat3::scale(vec2(f, f)) * inv;
        }
        let b = warp_bounds((dst.width, dst.height), src.size(), m);
        if b[0] >= b[2] || b[1] >= b[3] {
            continue;
        }
        let b = b.map(|v| v as f32);
        let r = rows(&inv);
        let sample = [r[0], r[1], r[2], b];
        match runs.last_mut() {
            Some((l, data, u)) if *l == level => {
                data.extend(sample.iter().flatten());
                *u = [u[0].min(b[0]), u[1].min(b[1]), u[2].max(b[2]), u[3].max(b[3])];
            }
            _ => runs.push((level, sample.iter().flatten().copied().collect(), b)),
        }
    }
    for (level, data, u) in &runs {
        let Some(img) = mips.get(*level) else { continue };
        let mut p = Params::default();
        p.u[0] = [sampling_id(sampling), (data.len() / 16) as u32, 0, 0];
        p.f[0] = [weight, u[0], u[1], u[2]];
        p.f[1][0] = u[3];
        let buf = e.data(data);
        // The run's bounds are whole pixels (`warp_bounds`).
        let b = u.map(|v| v as u32);
        if let Some((under, groups)) = at_f32(e, dst, b, "warp_accum_at", img.is_codes()) {
            p.u[1] = b;
            codes::dispatch(e, "warp_accum_at", &p, img, Some(&Pixels::Float(under)), &Pixels::Float(dst.clone()), Some(&buf), groups);
            dst.levels = None;
            continue;
        }
        let out = e.scratch(dst.width, dst.height);
        codes::pixels(e, "warp_accum", &p, img, Some(&Pixels::Float(dst.clone())), &Pixels::Float(out.clone()), Some(&buf));
        *dst = out;
    }
    reach
}

/// `Image::blend_from`: blend `src` (same size) onto `dst`, clamping and quantising the result
/// to `levels` when set (see [`warp_q`]).
#[cfg(test)]
pub fn blend_full(e: &mut Enc, dst: &GpuImage, src: &GpuImage, mode: BlendMode, opacity: f32, seed: u32, levels: Option<f32>) -> GpuImage {
    blend_full_preserve(e, dst, src, mode, opacity, seed, levels, false)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn blend_full_preserve(
    e: &mut Enc,
    dst: &GpuImage,
    src: &GpuImage,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    levels: Option<f32>,
    preserve: bool,
) -> GpuImage {
    let mut out = dst.clone();
    blend_full_in(e, &mut out, src, mode, opacity, seed, levels, preserve, None);
    out
}

/// `Image::blend_from` (`blend_full`, Preserve Transparency with `preserve`) onto `dst` itself: in place over `b` (`blend_full_at`, the same
/// per-pixel blend and quantisation) when this encoder owns `dst` and `src` is transparent
/// outside `b` (the caller's promise, e.g. a layer drawn on zeros inside [`warp_bounds`]).
/// There the full pass stores the canvas pixel back, quantised to `levels`, so in place is
/// exact unless the mode is a stencil (which blends even transparent pixels) or the canvas
/// isn't already quantised to `levels` (as [`warp_q_in`]). `dst` stays f32 (see [`blend_px_in`]).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn blend_full_in(
    e: &mut Enc,
    dst: &mut GpuImage,
    src: &GpuImage,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    levels: Option<f32>,
    preserve: bool,
    b: Option<Bounds>,
) {
    // Taken out of `dst` (not cloned), so `Enc::owns` still sees the only handle.
    let mut px = Pixels::Float(std::mem::replace(dst, e.g.dummy.clone()));
    blend_in(e, &mut px, &Pixels::Float(src.clone()), mode, opacity, seed, levels, preserve, b, false);
    match px {
        Pixels::Float(out) => *dst = out,
        Pixels::Codes(_) => log::error!("gpu: an f32 canvas came back as codes"),
    }
}

/// [`blend_full_in`] of f32 or codes onto a canvas of f32 or codes; the result is codes when it
/// is quantised to the 8 bpc grid (see [`codes_out`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn blend_px_in(
    e: &mut Enc,
    dst: &mut Pixels,
    src: &Pixels,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    levels: Option<f32>,
    preserve: bool,
    b: Option<Bounds>,
) {
    blend_in(e, dst, src, mode, opacity, seed, levels, preserve, b, true);
}

#[allow(clippy::too_many_arguments)]
fn blend_in(
    e: &mut Enc,
    dst: &mut Pixels,
    src: &Pixels,
    mode: BlendMode,
    opacity: f32,
    seed: u32,
    levels: Option<f32>,
    preserve: bool,
    b: Option<Bounds>,
    codes_ok: bool,
) {
    let mut p = Params::default();
    p.u[0] = [mode_id(mode), preserve as u32, seed, 0];
    p.f[0] = [opacity, levels.unwrap_or(0.0), levels.map_or(0.0, |v| 1.0 / v), 0.0];
    // `is_stencil` in the kernels.
    let stencil = (32..=35).contains(&mode_id(mode));
    let in_place_ok = !stencil && (levels.is_none() || dst.levels() == levels) && (!dst.is_codes() || levels == Some(codes::LEVELS));
    if let Some(b) = b.filter(|_| in_place_ok)
        && let Some((under, groups)) = at(e, dst, b, "blend_full_at", src.is_codes())
    {
        p.u[1] = b;
        codes::dispatch(e, "blend_full_at", &p, src, Some(&under), dst, None, groups);
        if let Pixels::Float(g) = dst {
            g.levels = levels;
        }
        return;
    }
    let (w, h) = dst.size();
    let Some(mut out) = scratch_px(e, w, h, codes_out(e, "blend_full", src, dst, levels, codes_ok)) else {
        e.mark_missing();
        return;
    };
    codes::pixels(e, "blend_full", &p, src, Some(dst), &out, None);
    if let Pixels::Float(o) = &mut out {
        o.levels = levels;
    }
    *dst = out;
}

/// Multiply an isolated layer by its track matte.
#[cfg(test)]
pub fn matte(e: &mut Enc, iso: &GpuImage, matte: &GpuImage, kind: MatteKind) -> GpuImage {
    let mut out = iso.clone();
    matte_in(e, &mut out, matte, kind, None);
    out
}

/// Multiply an isolated layer by its track matte (`matte`), onto `iso` itself: in place over `b` (`matte_at`, the same per-pixel factor) when
/// this encoder owns `iso` and `iso` is transparent outside `b`. There the full pass writes
/// 0 × a factor clamped to 0..1, transparent again (a zero of either sign, which
/// [`blend_full_in`] skips alike), so the composite that follows is exact.
pub(crate) fn matte_in(e: &mut Enc, iso: &mut GpuImage, matte: &GpuImage, kind: MatteKind, b: Option<Bounds>) {
    let mut p = Params::default();
    p.u[0][0] = match kind {
        MatteKind::Alpha => 0,
        MatteKind::AlphaInverted => 1,
        MatteKind::Luma => 2,
        MatteKind::LumaInverted => 3,
    };
    if let Some(b) = b
        && let Some((under, groups)) = at_f32(e, iso, b, "matte_at", false)
    {
        p.u[1] = b;
        e.dispatch("matte_at", &p, &under, Some(matte), iso, None, groups);
        iso.levels = None;
        return;
    }
    let out = e.scratch(iso.width, iso.height);
    e.pixels("matte", &p, iso, Some(matte), &out, None);
    *iso = out;
}

/// Knockout: clear what lies below the layer's content.
pub fn knockout(e: &mut Enc, canvas: &GpuImage, shape: &GpuImage) -> GpuImage {
    let out = e.scratch(canvas.width, canvas.height);
    e.pixels("knockout", &Params::default(), canvas, Some(shape), &out, None);
    out
}

/// Styled-layer finish: move `canvas` toward `tmp` by `opacity` on the enabled channels.
#[cfg(test)]
pub fn channel_mix(e: &mut Enc, canvas: &GpuImage, tmp: &GpuImage, channels: [bool; 3], opacity: f32) -> GpuImage {
    let mut out = canvas.clone();
    channel_mix_in(e, &mut out, tmp, channels, opacity, None);
    out
}

/// The styled-layer finish (`channel_mix`) onto `canvas` itself: in place over `b` (`channel_mix_at`, the same
/// per-pixel mix) when this encoder owns `canvas` and `tmp` equals it outside `b`. There the
/// full pass writes c + (c − c) × opacity = c (a −0 comes back as +0, an equal value).
pub(crate) fn channel_mix_in(e: &mut Enc, canvas: &mut GpuImage, tmp: &GpuImage, channels: [bool; 3], opacity: f32, b: Option<Bounds>) {
    let mut p = Params::default();
    p.u[0] = [channels[0] as u32, channels[1] as u32, channels[2] as u32, 0];
    p.f[0][0] = opacity;
    if let Some(b) = b
        && let Some((under, groups)) = at_f32(e, canvas, b, "channel_mix_at", false)
    {
        p.u[1] = b;
        e.dispatch("channel_mix_at", &p, &under, Some(tmp), canvas, None, groups);
        canvas.levels = None;
        return;
    }
    let out = e.scratch(canvas.width, canvas.height);
    e.pixels("channel_mix", &p, canvas, Some(tmp), &out, None);
    *canvas = out;
}

/// Adjustment layer finish: move `canvas` toward `adjusted` by `matte`'s alpha × `opacity`.
/// `canvas` may be 8 bpc codes (the kernel reads them).
pub(crate) fn adjust_mix(e: &mut Enc, canvas: &Pixels, adjusted: &GpuImage, matte: &GpuImage, opacity: f32) -> GpuImage {
    let (rows, stride) = e.image_rows(matte);
    let mut p = Params::default();
    p.u[0][0] = stride;
    p.f[0][0] = opacity;
    let out = e.scratch(canvas.width(), canvas.height());
    codes::pixels(e, "adjust_mix", &p, canvas, Some(&adjusted.clone().into()), &out.clone().into(), Some(&rows));
    out
}

/// Clamp and quantise to `levels` steps (8/16 bpc).
pub fn quantize(e: &mut Enc, img: &GpuImage, levels: f32) -> GpuImage {
    // Quantising is idempotent: an image a fused kernel already quantised stays as it is.
    if img.levels == Some(levels) {
        return img.clone();
    }
    let mut p = Params::default();
    p.f[0][0] = levels;
    p.f[0][1] = 1.0 / levels;
    let mut out = e.scratch(img.width, img.height);
    e.pixels("quantize", &p, img, None, &out, None);
    out.levels = Some(levels);
    out
}

/// [`quantize`] of f32 or codes: codes are already on the 8 bpc grid; at that grid an f32 image
/// is quantised into codes (`codes_ok`, where the codes kernels exist), else into f32.
pub(crate) fn quantize_px(e: &mut Enc, img: &Pixels, levels: f32, codes_ok: bool) -> Pixels {
    if img.levels() == Some(levels) {
        return img.clone();
    }
    let Pixels::Float(f) = img else {
        // Codes are on the 8 bpc grid; another grid needs their floats.
        return match img.float(e) {
            Some(f) => Pixels::Float(quantize(e, &f, levels)),
            None => img.clone(),
        };
    };
    let codes = codes_ok && levels == codes::LEVELS && has(e, "quantize", codes::Io { src: false, aux: false, out: true });
    match codes.then(|| e.scratch_codes(f.width, f.height)).flatten() {
        Some(out) => {
            let mut p = Params::default();
            p.f[0][0] = levels;
            p.f[0][1] = 1.0 / levels;
            let out = Pixels::Codes(out);
            codes::pixels(e, "quantize", &p, img, None, &out, None);
            out
        }
        None => Pixels::Float(quantize(e, f, levels)),
    }
}

fn curve_id(s: Option<effectcraft_color::ColorSpace>) -> u32 {
    use effectcraft_color::space::Curve;
    match s.map(|s| s.curve()) {
        None | Some(Curve::Linear) => 0,
        Some(Curve::Srgb) => 1,
        Some(Curve::Gamma24) => 2,
        Some(Curve::Pq) => 3,
        Some(Curve::Hlg) => 4,
    }
}

/// Colour conversion on straight colour (`render::color::convert`).
pub fn convert(e: &mut Enc, img: &GpuImage, c: &Conversion) -> GpuImage {
    let mut p = Params::default();
    p.u[0] = [curve_id(c.decode), c.matrix.is_some() as u32, curve_id(c.encode), 0];
    if let Some(m) = c.matrix {
        for i in 0..3 {
            p.f[i] = [m[i][0], m[i][1], m[i][2], 0.0];
        }
    }
    let out = e.scratch(img.width, img.height);
    e.pixels("convert", &p, img, None, &out, None);
    out
}
