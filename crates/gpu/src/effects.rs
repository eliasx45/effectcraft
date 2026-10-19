//! GPU effects: the same steps as the CPU effects in `effectcraft-effects` (padding, blur
//! radii, parameter conversions), with the pixel loops as compute kernels.

use effectcraft_effects::{Buf, EffectCtx};
use effectcraft_raster::Sampling;
use effectcraft_render::FxStep;

use crate::codes::{self, Pixels};
use crate::context::{Enc, GpuImage, Params};
use crate::ops;

/// Effects with a GPU implementation (must equal `effectcraft_effects::GPU_EFFECTS`), plus the
/// expression controls (pass-throughs).
pub fn supports(id: &str) -> bool {
    effectcraft_effects::GPU_EFFECTS.contains(&id) || id.starts_with("ec.control.")
}

/// A layer buffer on the GPU (see [`Buf`]).
pub(crate) struct GBuf {
    pub img: GpuImage,
    pub offset: [f64; 2],
    pub scale: f64,
}

impl GBuf {
    pub fn to_px(&self, p: [f64; 2]) -> (f64, f64) {
        (p[0] * self.scale + self.offset[0], p[1] * self.scale + self.offset[1])
    }

    /// Buf::pad: grow by `pad` transparent pixels on every side.
    pub fn pad(&mut self, e: &mut Enc, pad: u32) -> Option<()> {
        if pad == 0 {
            return Some(());
        }
        let (w, h) = (self.img.width + 2 * pad, self.img.height + 2 * pad);
        if !e.g.fits(w, h) {
            return None;
        }
        let out = e.image(w, h);
        e.copy_into(&self.img, &out, pad, pad);
        self.img = out;
        self.offset[0] += pad as f64;
        self.offset[1] += pad as f64;
        Some(())
    }
}

/// An effect's input buffer whose image may still be 8 bpc codes (see `codes`): the effects
/// with kernels reading codes ([`apply_px`]) take it as it is, any other decodes it first.
pub(crate) struct PBuf {
    pub img: Pixels,
    pub offset: [f64; 2],
    pub scale: f64,
}

impl From<GBuf> for PBuf {
    fn from(b: GBuf) -> PBuf {
        PBuf { img: b.img.into(), offset: b.offset, scale: b.scale }
    }
}

impl PBuf {
    /// [`GBuf::pad`] (codes stay codes).
    fn pad(&mut self, e: &mut Enc, pad: u32) -> Option<()> {
        if pad == 0 {
            return Some(());
        }
        match &self.img {
            Pixels::Float(img) => {
                let mut b = GBuf { img: img.clone(), offset: self.offset, scale: self.scale };
                b.pad(e, pad)?;
                *self = b.into();
            }
            Pixels::Codes(c) => {
                self.img = Pixels::Codes(codes::pad(e, c, pad)?);
                self.offset[0] += pad as f64;
                self.offset[1] += pad as f64;
            }
        }
        Some(())
    }

    /// The buffer with its image as floats (codes decoded).
    pub fn float(self, e: &mut Enc) -> Option<GBuf> {
        Some(GBuf { img: self.img.float(e)?, offset: self.offset, scale: self.scale })
    }

    fn with(&self, img: GpuImage) -> GBuf {
        GBuf { img, offset: self.offset, scale: self.scale }
    }
}

/// Run a chain of GPU effects (one upload, one readback), quantising after each when `levels`
/// is set: the result, and its image on the device when it was read back now (the compositor
/// can draw from it, see `GpuContext::keep`). `None` when anything cannot run here.
pub(crate) fn run_chain(e: &mut Enc, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<(Buf, Option<GpuImage>)> {
    if !e.g.can_readback() || buf.img.is_empty() {
        return None;
    }
    // Deferred readbacks (a browser worker): served by key once the bytes arrived.
    let deferred = e.g.deferred.clone();
    let key = deferred.as_ref().map(|_| crate::deferred::chain_key(chain, buf, levels));
    if let (Some(d), Some(k)) = (&deferred, key) {
        match d.lookup(k) {
            crate::deferred::Lookup::Ready(r) => return r.and_then(readback_buf).map(|b| (b, None)),
            crate::deferred::Lookup::InFlight => {
                d.miss();
                return Some((buf.clone(), None));
            }
            crate::deferred::Lookup::Absent => {}
        }
    }
    let misses = deferred.as_ref().map(|d| d.misses());
    let mut b = GBuf { img: e.g.upload_image(&buf.img)?, offset: buf.offset, scale: buf.scale };
    b = apply_chain(e, chain, b, levels)?;
    if let (Some(d), Some(k)) = (&deferred, key) {
        // Data the kernels fetched through the effect host missed meanwhile: not genuine.
        if Some(d.misses()) != misses {
            d.miss();
            return Some((buf.clone(), None));
        }
        // The placeholder (the input) keeps this pass cheap; the next pass gets the result.
        return match e.read_keyed(&b.img, k, b.offset, b.scale) {
            Some(r) => r.and_then(readback_buf).map(|b| (b, None)),
            None => Some((buf.clone(), None)),
        };
    }
    let img = e.download(&b.img)?;
    Some((Buf { img, offset: b.offset, scale: b.scale }, Some(b.img)))
}

/// A layer buffer from deferred readback bytes (RGBA f32).
fn readback_buf(r: crate::deferred::Readback) -> Option<Buf> {
    let mut img = effectcraft_raster::Image::new(r.width, r.height);
    let dst: &mut [u8] = bytemuck::cast_slice_mut(&mut img.data);
    (dst.len() == r.bytes.len()).then(|| dst.copy_from_slice(&r.bytes))?;
    Some(Buf { img, offset: r.offset, scale: r.scale })
}

/// An effect stack running on a GPU-resident image (adjustment layers): GPU chains stay on
/// the device; CPU effects read the image back, run, and upload the result.
/// The image may be 8 bpc codes: the first effects read them where their kernels can.
pub(crate) struct GpuFx<'e, 'g> {
    e: &'e mut Enc<'g>,
    b: Option<PBuf>,
    levels: Option<f32>,
}

impl<'e, 'g> GpuFx<'e, 'g> {
    pub fn new(e: &'e mut Enc<'g>, img: impl Into<Pixels>, offset: [f64; 2], scale: f64, levels: Option<f32>) -> Self {
        GpuFx { e, b: Some(PBuf { img: img.into(), offset, scale }), levels }
    }

    /// The result: (image, offset, scale); `None` when a step failed.
    pub fn finish(self) -> Option<(GpuImage, [f64; 2], f64)> {
        let b = self.b?.float(self.e)?;
        Some((b.img, b.offset, b.scale))
    }

    /// The image as floats (codes decoded); `None` (and no image) when that failed.
    fn float(&mut self) -> Option<GBuf> {
        let b = self.b.take()?.float(self.e)?;
        self.b = Some(PBuf { img: b.img.clone().into(), offset: b.offset, scale: b.scale });
        Some(b)
    }
}

impl effectcraft_render::FxTarget for GpuFx<'_, '_> {
    fn gpu(&mut self, steps: &[FxStep]) -> bool {
        let Some(b) = &self.b else { return false };
        let cur = PBuf { img: b.img.clone(), offset: b.offset, scale: b.scale };
        let Some(cur) = apply_chain_px(self.e, steps, cur, self.levels) else { return false };
        self.b = Some(cur);
        true
    }

    fn cpu(&mut self, f: &mut dyn FnMut(Buf) -> Buf) {
        let Some(b) = self.b.take() else { return };
        // Codes read back as 4 bytes a pixel, decoded with the same table.
        let Some(img) = self.e.download_px(&b.img) else { return };
        let out = f(Buf { img, offset: b.offset, scale: b.scale });
        self.b = self.e.g.upload_image(&out.img).map(|img| PBuf { img: img.into(), offset: out.offset, scale: out.scale });
    }
    fn extent(&self) -> Option<([u32; 2], [f64; 2], f64)> {
        self.b.as_ref().map(|b| ([b.img.width(), b.img.height()], b.offset, b.scale))
    }

    fn effect_canvas_in(&mut self, from: [f64; 2], to: [f64; 2]) {
        let Some(b) = self.float() else { return };
        let Some(([width, height], pad)) = effectcraft_render::effect_canvas_extent_in([b.img.width, b.img.height], b.offset, b.scale, from, to) else {
            return;
        };
        // Failure keeps the existing whole-layer CPU fallback contract; never submit an
        // oversized copy or return a partially cleared canvas as a successful result.
        if !self.e.g.fits(width, height) || self.e.g.check_health().is_err() {
            self.b = None;
            return;
        }
        let mut out = self.e.image(width, height);
        if self.e.take_missing() || self.e.g.check_health().is_err() {
            self.b = None;
            return;
        }
        if b.img.width > 0 && b.img.height > 0 {
            self.e.copy_into(&b.img, &out, pad[0], pad[1]);
        }
        out.levels = b.img.levels;
        if self.e.take_missing() || self.e.g.check_health().is_err() {
            self.b = None;
            return;
        }
        self.b = Some(PBuf { img: out.into(), offset: [b.offset[0] + f64::from(pad[0]), b.offset[1] + f64::from(pad[1])], scale: b.scale });
    }

    fn shift(&mut self, d: [f64; 2]) {
        if let Some(b) = &mut self.b {
            b.offset = [b.offset[0] - d[0], b.offset[1] - d[1]];
        }
    }
}

#[cfg(test)]
static EFFECT_CHAIN_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn apply_chain(e: &mut Enc, chain: &[FxStep], mut b: GBuf, levels: Option<f32>) -> Option<GBuf> {
    #[cfg(test)]
    EFFECT_CHAIN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    e.take_missing();
    let mut remaining = chain;
    while let Some((step, rest)) = remaining.split_first() {
        if let Some((image, count)) = crate::fusion::apply(e, remaining, &b.img, levels) {
            if e.take_missing() {
                return None;
            }
            b.img = image;
            remaining = remaining.get(count..)?;
            continue;
        }
        b = apply(e, step.spec.id, &step.ctx, b)?;
        // A kernel the device could not build keeps the whole chain on the CPU.
        if e.take_missing() {
            return None;
        }
        if let Some(l) = levels {
            b.img = ops::quantize(e, &b.img, l);
        }
        if e.take_missing() {
            return None;
        }
        remaining = rest;
    }
    Some(b)
}

/// [`apply_chain`] on a buffer that may be 8 bpc codes: while it is, each effect whose kernels
/// read codes ([`apply_px`]) runs on them directly (no decoded copy); the first other effect
/// decodes it and the rest of the chain runs as [`apply_chain`].
pub(crate) fn apply_chain_px(e: &mut Enc, chain: &[FxStep], mut b: PBuf, levels: Option<f32>) -> Option<PBuf> {
    if !b.img.is_codes() {
        let b = b.float(e)?;
        return apply_chain(e, chain, b, levels).map(Into::into);
    }
    e.take_missing();
    let mut remaining = chain;
    while let Some((step, rest)) = remaining.split_first() {
        if !b.img.is_codes() || !reads_codes(step.spec.id) {
            break;
        }
        b = apply_px(e, step.spec.id, &step.ctx, b)?;
        if e.take_missing() {
            return None;
        }
        if let Some(l) = levels {
            b.img = match b.img {
                Pixels::Float(img) => Pixels::Float(ops::quantize(e, &img, l)),
                // Codes are on the 8 bpc grid already (`ops::quantize` keeps them as they are).
                codes @ Pixels::Codes(_) if l == codes::LEVELS => codes,
                codes => {
                    let f = codes.float(e)?;
                    Pixels::Float(ops::quantize(e, &f, l))
                }
            };
        }
        if e.take_missing() {
            return None;
        }
        remaining = rest;
    }
    if remaining.is_empty() {
        return Some(b);
    }
    let b = b.float(e)?;
    apply_chain(e, remaining, b, levels).map(Into::into)
}

/// Effects with kernels reading 8 bpc codes (see [`apply_px`]); none of them is fused
/// (`fusion`), so [`apply_chain`] would run each through [`apply`] too.
fn reads_codes(id: &str) -> bool {
    matches!(
        id,
        "ec.blur.gaussian" | "ec.blur.fastbox" | "ec.perspective.dropshadow" | "ec.color.huesaturation" | "ec.generate.fill" | "ec.generate.gradientramp"
    )
}

/// [`apply`] on a buffer that may be codes: the [`reads_codes`] effects as [`apply`] runs them,
/// their first kernels reading the codes.
fn apply_px(e: &mut Enc, id: &str, ctx: &EffectCtx, b: PBuf) -> Option<PBuf> {
    if !effectcraft_effects::catalog::gpu_supported(id, ctx) {
        return None;
    }
    match id {
        "ec.blur.gaussian" => gaussian_px(e, ctx, b),
        "ec.blur.fastbox" => box_blur_px(e, ctx, b),
        "ec.perspective.dropshadow" => drop_shadow_px(e, ctx, b).map(Into::into),
        "ec.color.huesaturation" if !effectcraft_effects::huesat_ranges_identity(ctx) => crate::fx_tone::hue_saturation(e, ctx, b).map(Into::into),
        "ec.color.huesaturation" | "ec.generate.fill" | "ec.generate.gradientramp" => pointwise_px(e, id, ctx, b).map(Into::into),
        _ => {
            let b = b.float(e)?;
            apply(e, id, ctx, b).map(Into::into)
        }
    }
}

fn apply(e: &mut Enc, id: &str, ctx: &EffectCtx, b: GBuf) -> Option<GBuf> {
    // Controls the kernels do not implement render on the CPU.
    if !effectcraft_effects::catalog::gpu_supported(id, ctx) {
        return None;
    }
    match id {
        "ec.blur.gaussian" => gaussian_px(e, ctx, b.into())?.float(e),
        "ec.blur.fastbox" => box_blur_px(e, ctx, b.into())?.float(e),
        "ec.blur.directional" => directional(e, ctx, b),
        "ec.stylize.glow" if crate::fx_stylize::glow_extra(ctx) => crate::fx_stylize::glow(e, ctx, b),
        "ec.distort.transform" if crate::fx_stylize::transform_blur(ctx) => crate::fx_stylize::transform(e, ctx, b),
        "ec.stylize.glow" => glow(e, ctx, b),
        "ec.perspective.dropshadow" => drop_shadow_px(e, ctx, b.into()),
        "ec.distort.transform" => transform(e, ctx, b),
        "ec.color.curves" => curves(e, ctx, b),
        "ec.color.huesaturation" if !effectcraft_effects::huesat_ranges_identity(ctx) => crate::fx_tone::hue_saturation(e, ctx, b.into()),
        "ec.color.levels" if !effectcraft_effects::levels_channels_identity(ctx) => crate::fx_tone::levels(e, ctx, b),
        "ec.channel.invert" if (4..=11).contains(&ctx.params.e("channel")) => crate::fx_tone::invert(e, ctx, b),
        "ec.noise.fractal" if crate::fx_noise::fractal_extra(ctx) => crate::fx_noise::fractal(e, ctx, b, false),
        _ if id.starts_with("ec.control.") => Some(b),
        _ if crate::fx_color::IDS.contains(&id) => crate::fx_color::apply(e, id, ctx, b),
        _ if crate::fx_distort::IDS.contains(&id) => crate::fx_distort::apply(e, id, ctx, b),
        _ if crate::fx_generate::IDS.contains(&id) => crate::fx_generate::apply(e, id, ctx, b),
        _ if crate::fx_key::IDS.contains(&id) => crate::fx_key::apply(e, id, ctx, b),
        _ if crate::fx_stylize::IDS.contains(&id) => crate::fx_stylize::apply(e, id, ctx, b),
        _ if crate::fx_noise::IDS.contains(&id) => crate::fx_noise::apply(e, id, ctx, b),
        _ if crate::fx_tone::IDS.contains(&id) => crate::fx_tone::apply(e, id, ctx, b),
        _ if crate::fx_warp::IDS.contains(&id) => crate::fx_warp::apply(e, id, ctx, b),
        _ if crate::fx_extra::IDS.contains(&id) => crate::fx_extra::apply(e, id, ctx, b),
        _ if crate::fx_depth::IDS.contains(&id) => crate::fx_depth::apply(e, id, ctx, b),
        _ if crate::fx_lut::IDS.contains(&id) => crate::fx_lut::apply(e, id, ctx, b),
        _ if crate::fx_sim::IDS.contains(&id) => crate::fx_sim::apply(e, id, ctx, b),
        _ if crate::fx_particles::IDS.contains(&id) => crate::fx_particles::apply(e, id, ctx, b),
        _ if crate::fx_vr::IDS.contains(&id) => crate::fx_vr::apply(e, id, ctx, b),
        _ if crate::fx_light::IDS.contains(&id) => crate::fx_light::apply(e, id, ctx, b),
        _ if crate::fx_transition::IDS.contains(&id) => crate::fx_transition::apply(e, id, ctx, b),
        _ if crate::fx_text::IDS.contains(&id) => crate::fx_text::apply(e, id, ctx, b),
        _ if crate::fx_time::IDS.contains(&id) => crate::fx_time::apply(e, id, ctx, b),
        _ if crate::fx_pixel2::IDS.contains(&id) => crate::fx_pixel2::apply(e, id, ctx, b),
        _ if crate::fx_gen2::IDS.contains(&id) => crate::fx_gen2::apply(e, id, ctx, b),
        _ if crate::fx_deepglow::IDS.contains(&id) => crate::fx_deepglow::apply(e, id, ctx, b),
        _ => pointwise_px(e, id, ctx, b.into()),
    }
}

// ---------------------------------------------------------------- blurs

/// raster::blur::box_radii.
pub(crate) fn box_radii(sigma: f64, n: usize) -> Vec<usize> {
    if sigma <= 0.0 {
        return vec![0; n];
    }
    let w_ideal = (12.0 * sigma * sigma / n as f64 + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - (n as i64 * wl * wl) as f64 - 4.0 * n as f64 * wl as f64 - 3.0 * n as f64) / (-4.0 * wl as f64 - 4.0);
    let m = m_ideal.round() as i64;
    (0..n as i64).map(|i| (((if i < m { wl } else { wu }) - 1) / 2).max(0) as usize).collect()
}

/// Horizontal passes with radii `rx`, then vertical passes with radii `ry` (raster::blur).
pub(crate) fn box_passes(e: &mut Enc, img: &GpuImage, rx: &[usize], ry: &[usize], repeat: bool) -> GpuImage {
    box_passes_with(e, "box_h", img, rx, ry, repeat)
}

/// [`box_passes`] with the horizontal passes run by kernel `h` (the tests' reference kernel).
pub(crate) fn box_passes_with(e: &mut Enc, h: &str, img: &GpuImage, rx: &[usize], ry: &[usize], repeat: bool) -> GpuImage {
    let mut cur = img.clone();
    let passes = rx.iter().map(|&r| (false, r)).chain(ry.iter().map(|&r| (true, r))).filter(|p| p.1 > 0);
    for (vertical, r) in passes {
        let block = if vertical { (4 * r).clamp(32, 256) } else { 256 } as u32;
        let (lines, n) = if vertical { (cur.width, cur.height) } else { (cur.height, cur.width) };
        let mut p = Params::default();
        p.u[0] = [r as u32, repeat as u32, block, 0];
        let out = e.scratch(cur.width, cur.height);
        e.dispatch(if vertical { "box_v" } else { h }, &p, &cur, None, &out, None, (lines.div_ceil(64), n.div_ceil(block)));
        cur = out;
    }
    cur
}

/// [`box_passes`] from an image that may be 8 bpc codes: the first pass reads the codes, the
/// rest run as [`box_passes`] (no passes: the image as it is).
pub(crate) fn box_passes_px(e: &mut Enc, img: &Pixels, rx: &[usize], ry: &[usize], repeat: bool) -> Pixels {
    if let Pixels::Float(f) = img {
        return box_passes(e, f, rx, ry, repeat).into();
    }
    let first = match rx.iter().position(|&r| r > 0) {
        Some(i) => rx.get(i).zip(rx.get(i + 1..)).map(|(&r, rest)| (false, r, rest, ry)),
        None => ry.iter().position(|&r| r > 0).and_then(|j| ry.get(j).zip(ry.get(j + 1..))).map(|(&r, rest)| (true, r, &[][..], rest)),
    };
    let Some((vertical, r, rx, ry)) = first else { return img.clone() };
    // The pass as `box_passes_with` runs it.
    let (w, h) = img.size();
    let block = if vertical { (4 * r).clamp(32, 256) } else { 256 } as u32;
    let (lines, n) = if vertical { (w, h) } else { (h, w) };
    let mut p = Params::default();
    p.u[0] = [r as u32, repeat as u32, block, 0];
    let out = e.scratch(w, h);
    codes::dispatch(e, if vertical { "box_v" } else { "box_h" }, &p, img, None, &out.clone().into(), None, (lines.div_ceil(64), n.div_ceil(block)));
    box_passes(e, &out, rx, ry, repeat).into()
}

/// raster::gaussian_blur (3 box passes per axis).
pub(crate) fn gaussian_blur(e: &mut Enc, img: &GpuImage, sx: f64, sy: f64, repeat: bool) -> GpuImage {
    let rx = if sx > 0.05 { box_radii(sx, 3) } else { vec![] };
    let ry = if sy > 0.05 { box_radii(sy, 3) } else { vec![] };
    box_passes(e, img, &rx, &ry, repeat)
}

/// [`gaussian_blur`] from an image that may be codes ([`box_passes_px`]).
fn gaussian_blur_px(e: &mut Enc, img: &Pixels, sx: f64, sy: f64, repeat: bool) -> Pixels {
    let rx = if sx > 0.05 { box_radii(sx, 3) } else { vec![] };
    let ry = if sy > 0.05 { box_radii(sy, 3) } else { vec![] };
    box_passes_px(e, img, &rx, &ry, repeat)
}

fn dims_xy(dim: u32) -> (f64, f64) {
    match dim {
        1 => (1.0, 0.0),
        2 => (0.0, 1.0),
        _ => (1.0, 1.0),
    }
}

/// Gaussian Blur (as `effectcraft_effects`' CPU one: σ from [`effectcraft_effects::gaussian_sigma`],
/// comp pixels on a layer whose effects run in comp space).
fn gaussian_px(e: &mut Enc, ctx: &EffectCtx, mut b: PBuf) -> Option<PBuf> {
    let s = effectcraft_effects::gaussian_sigma(ctx.params.f("blurriness")) * b.scale;
    if s <= 0.0 {
        return Some(b);
    }
    let (kx, ky) = dims_xy(ctx.params.e("dimensions"));
    let [cx, cy] = ctx.env.comp_px();
    let (sx, sy) = (s * cx * kx, s * cy * ky);
    let repeat = ctx.params.b("repeatEdge");
    if !repeat && !ctx.adjustment {
        b.pad(e, (sx.max(sy) * 3.0).ceil().min(1.0e5) as u32)?;
    }
    b.img = gaussian_blur_px(e, &b.img, sx, sy, repeat || ctx.adjustment);
    Some(b)
}

fn box_blur_px(e: &mut Enc, ctx: &EffectCtx, mut b: PBuf) -> Option<PBuf> {
    let r = ctx.params.f("radius").max(0.0) * b.scale;
    let (kx, ky) = dims_xy(ctx.params.e("dimensions"));
    let [cx, cy] = ctx.env.comp_px();
    let rx = (r * cx * kx).round().min(1.0e5) as usize;
    let ry = (r * cy * ky).round().min(1.0e5) as usize;
    if rx == 0 && ry == 0 {
        return Some(b);
    }
    let it = ctx.params.f("iterations").round().clamp(1.0, 50.0) as usize;
    let repeat = ctx.params.b("repeatEdge");
    if !repeat && !ctx.adjustment {
        b.pad(e, (rx.max(ry) * it) as u32 + 1)?;
    }
    b.img = box_passes_px(e, &b.img, &vec![rx; it], &vec![ry; it], repeat || ctx.adjustment);
    Some(b)
}

fn directional(e: &mut Enc, ctx: &EffectCtx, mut b: GBuf) -> Option<GBuf> {
    let len = ctx.params.f("length") * b.scale;
    if len < 0.5 {
        return Some(b);
    }
    if !ctx.adjustment {
        b.pad(e, len.ceil() as u32 + 1)?;
    }
    let a = ctx.params.f("direction").to_radians();
    let n = (len.ceil() as usize * 2 + 1).clamp(3, 257);
    let mut p = Params::default();
    p.f[0] = [a.sin() as f32, -a.cos() as f32, len as f32, n as f32];
    p.u[0][0] = ctx.adjustment as u32;
    let out = e.scratch(b.img.width, b.img.height);
    e.pixels("directional", &p, &b.img, None, &out, None);
    b.img = out;
    Some(b)
}

fn glow(e: &mut Enc, ctx: &EffectCtx, mut b: GBuf) -> Option<GBuf> {
    // Other Glow Operations and the Arbitrary Map render on the CPU.
    if ctx.params.e("colors") == 2 || effectcraft_effects::glow_operation(ctx) != effectcraft_color::BlendMode::Add {
        return None;
    }
    let radius = ctx.params.f("radius") * b.scale;
    let intensity = ctx.params.f("intensity") as f32;
    let use_colors = ctx.params.e("colors") == 1;
    let (kx, ky) = dims_xy(ctx.params.e("glowDimensions"));
    let [cx, cy] = ctx.env.comp_px();
    let (sx, sy) = effectcraft_effects::glow_sigmas(radius, cx, cy);
    if !ctx.adjustment {
        b.pad(e, (sx.max(sy) * 3.0).ceil() as u32 + 2)?;
    }
    let p = glow_params(ctx);
    let bright = e.image(b.img.width, b.img.height);
    e.pixels("glow_bright", &p, &b.img, None, &bright, None);
    let mut blurred = gaussian_blur(e, &bright, sx * kx, sy * ky, false);
    if use_colors {
        let coloured = e.image(b.img.width, b.img.height);
        e.pixels("glow_ab", &p, &blurred, None, &coloured, None);
        blurred = coloured;
    }
    let mut p = Params::default();
    p.u[0][0] = effectcraft_effects::glow_composite(ctx.params.e("operation"));
    p.f[0][0] = intensity;
    let out = e.scratch(b.img.width, b.img.height);
    e.pixels("glow_combine", &p, &b.img, Some(&blurred), &out, None);
    b.img = out;
    Some(b)
}

/// Parameters shared by Glow's `glow_bright`, `glow_ab` and `fxs_glow_map` kernels.
pub(crate) fn glow_params(ctx: &EffectCtx) -> Params {
    let mut p = Params::default();
    p.u[0] = [(ctx.params.e("colors") != 0) as u32, (ctx.params.e("based") == 0) as u32, ctx.params.e("colorLooping"), 0];
    p.f[0] = [
        ctx.params.f("threshold") as f32 / 100.0,
        ctx.params.f("colorLoops") as f32,
        (ctx.params.f("colorPhase") / 360.0) as f32,
        ctx.params.f("abMidpoint") as f32 / 100.0,
    ];
    p.f[1] = ctx.params.color("colorA");
    p.f[2] = ctx.params.color("colorB");
    p
}

fn drop_shadow_px(e: &mut Enc, ctx: &EffectCtx, mut b: PBuf) -> Option<GBuf> {
    let color = ctx.params.color("color");
    let opacity = ctx.params.f("opacity") as f32 / 100.0;
    let dir = ctx.params.f("direction").to_radians();
    let dist = ctx.params.f("distance") * b.scale;
    let soft = ctx.params.f("softness") * b.scale;
    let only = ctx.params.b("shadowOnly");
    let pad = (dist + soft * 1.5).ceil() as u32 + 2;
    if !ctx.adjustment {
        b.pad(e, pad)?;
    }
    let (dx, dy) = (dir.sin() * dist, -dir.cos() * dist);
    let mut p = Params::default();
    p.f[0] = color;
    p.f[1] = [opacity, dx as f32, dy as f32, 0.0];
    let (w, h) = b.img.size();
    let mut sh = e.image(w, h);
    codes::pixels(e, "shadow_make", &p, &b.img, None, &sh.clone().into(), None);
    if soft > 0.0 {
        sh = gaussian_blur(e, &sh, soft / 2.0, soft / 2.0, false);
    }
    if !only {
        let out = e.scratch(w, h);
        codes::pixels(e, "shadow_combine", &Params::default(), &sh.into(), Some(&b.img), &out.clone().into(), None);
        sh = out;
    }
    Some(b.with(sh))
}

fn transform(e: &mut Enc, ctx: &EffectCtx, mut b: GBuf) -> Option<GBuf> {
    // Motion blur (shutter angle) renders on the CPU.
    if ctx.env.host.is_some() && effectcraft_effects::transform_shutter(ctx).is_some() {
        return None;
    }
    let m = effectcraft_effects::transform_effect_matrix(ctx.params, b.to_px(ctx.params.v2("anchor")), b.to_px(ctx.params.v2("position")));
    // Content carried past the buffer edges grows the CPU buffer; render that case there.
    if effectcraft_effects::transform_extent(ctx.adjustment, (b.img.width, b.img.height), &[m]) != (0, 0, b.img.width, b.img.height) {
        return None;
    }
    let opacity = ctx.params.f("opacity") as f32 / 100.0;
    let sampling = if ctx.params.e("sampling") == 1 { Sampling::Bicubic } else { Sampling::Bilinear };
    let empty = e.image(b.img.width, b.img.height);
    b.img = ops::warp(e, &empty, &b.img, &m, sampling, effectcraft_color::BlendMode::Normal, opacity, 0, None);
    Some(b)
}

// ---------------------------------------------------------------- per-pixel

fn curves(e: &mut Enc, ctx: &EffectCtx, b: GBuf) -> Option<GBuf> {
    let parsed = ["rgb", "red", "green", "blue", "alpha"].map(|id| effectcraft_effects::Curve::parse(ctx.params.s(id)));
    if parsed.iter().all(Option::is_none) {
        return Some(b);
    }
    let mut data = vec![];
    let mut offs = [-1.0f32; 5];
    for (i, c) in parsed.iter().enumerate() {
        let Some(c) = c else { continue };
        let (xs, ys, ms, lut) = c.tables();
        offs[i] = data.len() as f32;
        data.push(xs.len() as f32);
        data.extend_from_slice(xs);
        data.extend_from_slice(ys);
        data.extend_from_slice(ms);
        data.extend_from_slice(lut);
    }
    let buf = e.data(&data);
    let mut p = Params::default();
    p.u[0][0] = 8;
    p.f[0] = [offs[0], offs[1], offs[2], offs[3]];
    p.f[1][0] = offs[4];
    let out = e.scratch(b.img.width, b.img.height);
    e.pixels("pointwise", &p, &b.img, None, &out, Some(&buf));
    Some(GBuf { img: out, ..b })
}

fn rgb(c: [f32; 4]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

pub(crate) fn pointwise_params(id: &str, ctx: &EffectCtx, offset: [f64; 2], scale: f64) -> Option<Params> {
    let f = |k: &str| ctx.params.f(k);
    let mut p = Params::default();
    match id {
        "ec.color.tint" => {
            p.u[0][0] = 1;
            p.f[0] = rgb(ctx.params.color("black"));
            p.f[1] = rgb(ctx.params.color("white"));
            p.f[2][0] = f("amount") as f32 / 100.0;
        }
        "ec.color.brightnesscontrast" => {
            let br = f("brightness") as f32 / 100.0;
            let ct = f("contrast") as f32 / 100.0;
            let k = if ct >= 0.0 { 1.0 / (1.0 - ct * 0.99) } else { 1.0 + ct };
            let legacy = ctx.params.b("useLegacy");
            // Mode: 0 = identity, 1 = legacy linear, 2 = tone curves (see `bc_modern`).
            let mode = if br == 0.0 && ct == 0.0 {
                0
            } else if legacy {
                1
            } else {
                2
            };
            p.u[0] = [2, mode, 0, 0];
            p.f[0] = [br, k, ct, 0.0];
        }
        "ec.color.huesaturation" => {
            // Colour ranges (Channel Control) render on the CPU.
            if !effectcraft_effects::huesat_ranges_identity(ctx) {
                return None;
            }
            p.u[0] = [3, ctx.params.b("colorize") as u32, 0, 0];
            p.f[0] = [f("hue") as f32 / 360.0, f("saturation") as f32 / 100.0, f("lightness") as f32 / 100.0, 0.0];
            p.f[1] = [f("colorizeHue") as f32 / 360.0, f("colorizeSaturation") as f32 / 100.0, f("colorizeLightness") as f32 / 100.0, 0.0];
        }
        "ec.color.levels" => {
            // Red / Green / Blue / Alpha controls render on the CPU.
            if !effectcraft_effects::levels_channels_identity(ctx) {
                return None;
            }
            let (clip_b, clip_w) = effectcraft_effects::levels_clip(ctx);
            p.u[0] = [4, clip_b as u32, clip_w as u32, 0];
            p.f[0] = [f("inBlack") as f32, f("inWhite") as f32, f("gamma").max(0.01) as f32, f("outBlack") as f32];
            p.f[1][0] = f("outWhite") as f32;
        }
        "ec.color.exposure" => {
            let s = effectcraft_effects::exposure_settings(ctx);
            p.u[0] = [5, ctx.params.b("bypassLinearLight") as u32, 0, 0];
            for i in 0..3 {
                p.f[0][i] = s[i].0;
                p.f[1][i] = s[i].1;
                p.f[2][i] = s[i].2;
            }
        }
        "ec.channel.invert" => {
            // The shader does the RGB channels and Alpha; HLS / YIQ inversions run on the CPU.
            let ch = match ctx.params.e("channel") {
                c @ 0..=3 => c,
                effectcraft_effects::INVERT_ALPHA => 4,
                _ => return None,
            };
            p.u[0] = [6, ch, 0, 0];
            p.f[0][0] = 1.0 - f("blend") as f32 / 100.0;
        }
        "ec.generate.fill" => {
            // Fill Mask / All Masks render on the CPU.
            if effectcraft_effects::fill_uses_masks(ctx) {
                return None;
            }
            p.u[0] = [7, ctx.params.b("invert") as u32, 0, 0];
            p.f[0] = ctx.params.color("color");
            p.f[1][0] = f("opacity") as f32 / 100.0;
        }
        "ec.generate.gradientramp" => {
            let (m, s, en) = effectcraft_effects::gradient_ramp_space(ctx, offset, scale);
            let (dx, dy) = (en.0 - s.0, en.1 - s.1);
            let len2 = (dx * dx + dy * dy).max(1e-9);
            p.u[0] = [9, (ctx.params.e("shape") == 1) as u32, ctx.seed, 0];
            p.f[0] = [s.0 as f32, s.1 as f32, en.0 as f32, en.1 as f32];
            p.f[1] = rgb(ctx.params.color("startColor"));
            p.f[2] = rgb(ctx.params.color("endColor"));
            p.f[3] = [len2 as f32, f("scatter") as f32 / 512.0, f("blend") as f32 / 100.0, 0.0];
            p.f[4] = [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32, 0.0];
            p.f[5] = [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32, 0.0];
        }
        "ec.noise.fractal" => {
            // The shader covers Soft Linear noise, the Basic / Turbulent Smooth / Turbulent Basic
            // types, all Overflow modes, independent width / height scaling, Sub Influence and Sub
            // Scaling, and the None / Normal blending modes; anything else renders on the CPU.
            let pr = ctx.params;
            let kind = pr.e("fractalType");
            let mode = pr.e("blendingMode");
            let sub_offset = pr.v2("subSettings/subOffset");
            if pr.e("noiseType") != 2
                || kind > 2
                || mode > 1
                || pr.b("evolutionOptions/cycleEvolution")
                || pr.b("transform/perspectiveOffset")
                || f("subSettings/subRotation") != 0.0
                || (!pr.b("subSettings/centerSubscale") && sub_offset != [0.0, 0.0])
            {
                return None;
            }
            let octaves = f("complexity").clamp(1.0, 20.0);
            let (sr, cr) = (f("transform/rotation") as f32).to_radians().sin_cos();
            let point = pr.v2("transform/offset");
            let (ox, oy) = (point[0] * scale + offset[0], point[1] * scale + offset[1]);
            let s = f("transform/scale");
            let (sw, sh) = if pr.b("transform/uniformScaling") { (s, s) } else { (f("transform/scaleWidth"), f("transform/scaleHeight")) };
            let sub = (f("subSettings/subScaling") / 100.0).clamp(0.1, 1.0);
            p.u[0] = [10, kind, octaves.ceil() as u32, f("evolutionOptions/seed") as u32 ^ 0x51ed];
            p.u[1] = [pr.b("invert") as u32, pr.e("overflow"), mode, 0];
            p.f[0] = [f("contrast") as f32 / 100.0, f("brightness") as f32 / 100.0, (sw * scale).max(1.0) as f32, (octaves - octaves.floor()) as f32];
            p.f[1] = [ox as f32, oy as f32, sr, cr];
            p.f[2] = [f("evolution") as f32 / 360.0, f("blend") as f32 / 100.0, (f("opacity") as f32 / 100.0).clamp(0.0, 1.0), (sh * scale).max(1.0) as f32];
            p.f[3] = [(f("subSettings/subInfluence") as f32 / 100.0).clamp(0.0, 1.0), 1.0 / sub as f32, 0.0, 0.0];
        }
        _ => return None,
    }
    Some(p)
}

fn pointwise_px(e: &mut Enc, id: &str, ctx: &EffectCtx, b: PBuf) -> Option<GBuf> {
    let p = pointwise_params(id, ctx, b.offset, b.scale)?;
    let out = e.scratch(b.img.width(), b.img.height());
    codes::pixels(e, "pointwise", &p, &b.img, None, &out.clone().into(), None);
    Some(b.with(out))
}

#[cfg(test)]
mod keep_tests {
    use effectcraft_project::BitDepth;
    use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
    use effectcraft_time::Tick;

    use crate::Gpu;
    use crate::tests::{Pattern, Scene, n, set, v3};

    /// Footage and a solid ending in GPU chains (composited from the chain's result), and
    /// footage whose GPU chain is followed by a CPU effect (uploaded as before).
    fn scene(depth: BitDepth, linear: bool) -> Scene {
        let mut s = Scene::new(depth);
        s.p.settings.blend_linear = linear;
        let mut bg = s.footage(97, 61);
        s.effect(&mut bg, "ec.blur.gaussian", &[("blurriness", n(5.0))]);
        s.effect(&mut bg, "ec.color.levels", &[("gamma", n(1.3))]);
        s.push(bg);
        let mut solid = s.solid([0.9, 0.4, 0.1], 30, 20);
        s.effect(&mut solid, "ec.stylize.glow", &[("threshold", n(20.0)), ("radius", n(6.0))]);
        set(&mut solid, "transform/position", v3(30.0, 20.0));
        s.push(solid);
        let mut mixed = s.footage(40, 30);
        s.effect(&mut mixed, "ec.blur.fastbox", &[("radius", n(3.0))]);
        s.effect(&mut mixed, "ec.generate.fractal", &[]);
        set(&mut mixed, "transform/position", v3(70.0, 40.0));
        s.push(mixed);
        s
    }

    /// One frame through a fresh layer cache (as when every layer changes each frame): the
    /// pixels and the uploads it took.
    fn frame(g: &Gpu, s: &Scene, keep: bool) -> (effectcraft_raster::Image, crate::TransferStats) {
        g.ctx.clear_uploads();
        g.ctx.set_keep_effects(keep);
        let cache = LayerCache::default();
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let before = g.ctx.transfer_stats();
        // Exercise the legacy eager effects_kept contract explicitly. The ordinary
        // resident producer has separate demand/transfer tests.
        let img = g
            .within_memory(|| {
                let mut enc = crate::context::Enc::with_owner(&g.ctx, cache.gpu_owner());
                let image = crate::walk::render(&mut enc, &r, s.cid, Tick::ZERO).unwrap();
                let result = enc.download_px(&image).unwrap();
                enc.publish_native();
                result
            })
            .unwrap();
        let after = g.ctx.transfer_stats();
        g.ctx.set_keep_effects(true);
        let d = crate::TransferStats {
            uploads: after.uploads - before.uploads,
            upload_bytes: after.upload_bytes - before.upload_bytes,
            readbacks: after.readbacks - before.readbacks,
            readback_bytes: after.readback_bytes - before.readback_bytes,
            yuv_conversions: after.yuv_conversions - before.yuv_conversions,
            yuv_bytes: after.yuv_bytes - before.yuv_bytes,
        };
        (img, d)
    }

    #[test]
    fn ordinary_resident_transform_and_lazy_cpu_are_stage_exact() {
        let Some(g) = crate::tests::gpu() else { return };
        eprintln!("resident proof adapter: {}", g.ctx.name);
        for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
            for linear in [false, true] {
                for scale in [1.0, 0.5] {
                    g.ctx.clear_uploads();
                    let mut s = Scene::new(depth);
                    s.p.settings.blend_linear = linear;
                    let mut layer = s.solid([0.72, 0.31, 0.13], 31, 23);
                    s.effect(&mut layer, "ec.color.levels", &[("gamma", n(1.3))]);
                    let id = s.push(layer);
                    let cache = LayerCache::default();
                    let opts = RenderOpts { backend: Backend::Gpu, scale, ..Default::default() };
                    {
                        let mut r = Renderer::new(&s.p, &Pattern, opts);
                        r.accel = Some(g);
                        r.cache = Some(&cache);
                        let before = g.ctx.transfer_stats();
                        g.within_memory(|| g.render_display(&r, s.cid, Tick::ZERO)).unwrap().unwrap();
                        let after = g.ctx.transfer_stats();
                        assert_eq!(after.readbacks, before.readbacks, "cold producer stays resident");
                        let ctx = r.eval_ctx(s.cid, Tick::ZERO).unwrap();
                        let layer = ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
                        let key = r.resident_content_key(&ctx, layer).unwrap();
                        assert!(
                            cache.gpu_owner().unwrap().resident(key, effectcraft_render::cache::GpuStage::Processed).is_some(),
                            "actual ordinary resident route"
                        );
                    }
                    set(s.p.comp_mut(s.cid).unwrap().layers.iter_mut().find(|l| l.id == id).unwrap(), "transform/position", v3(42.0, 27.0));
                    let mut r = Renderer::new(&s.p, &Pattern, opts);
                    r.accel = Some(g);
                    r.cache = Some(&cache);
                    let before = g.ctx.transfer_stats();
                    let effect_calls = super::EFFECT_CHAIN_CALLS.load(std::sync::atomic::Ordering::Relaxed);
                    let display = g.within_memory(|| g.render_display(&r, s.cid, Tick::ZERO)).unwrap().unwrap();
                    let warm = g.ctx.transfer_stats();
                    assert_eq!(super::EFFECT_CHAIN_CALLS.load(std::sync::atomic::Ordering::Relaxed), effect_calls, "warm transform effect work");
                    assert_eq!(warm.uploads, before.uploads, "warm transform uploads {depth:?}/{linear}/{scale}");
                    assert_eq!(warm.readbacks, before.readbacks, "warm transform readbacks");
                    let actual = g.read_display(&display).unwrap();
                    let ctx = r.eval_ctx(s.cid, Tick::ZERO).unwrap();
                    let layer = ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
                    let before = g.ctx.transfer_stats();
                    if linear {
                        r.styled_layer(&ctx, layer).unwrap();
                    }
                    let materialized = r.content_buf(&ctx, layer).unwrap();
                    let after = g.ctx.transfer_stats();
                    assert_eq!(after.readbacks - before.readbacks, 1, "one CPU demand readback");
                    assert!(std::sync::Arc::ptr_eq(&materialized, &r.content_buf(&ctx, layer).unwrap()));
                    assert_eq!(g.ctx.transfer_stats().readbacks, after.readbacks);
                    let mut legacy = Renderer::new(&s.p, &Pattern, opts);
                    legacy.accel = Some(g);
                    let legacy_ctx = legacy.eval_ctx(s.cid, Tick::ZERO).unwrap();
                    let legacy_layer = legacy_ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
                    let expected = legacy.content_buf(&legacy_ctx, legacy_layer).unwrap();
                    assert_eq!(materialized.offset, expected.offset);
                    assert_eq!(materialized.scale, expected.scale);
                    assert_eq!(materialized.img.data, expected.img.data, "Processed fidelity {depth:?}/{linear}/{scale}");
                    let expected_display = g
                        .within_memory(|| {
                            let mut enc = crate::context::Enc::with_owner(&g.ctx, None);
                            let img = crate::walk::render(&mut enc, &legacy, s.cid, Tick::ZERO).unwrap();
                            let texture = enc.display_px(&img).unwrap();
                            enc.submit();
                            crate::DisplayFrame { texture, width: img.width(), height: img.height() }
                        })
                        .unwrap();
                    assert_eq!(actual, g.read_display(&expected_display).unwrap(), "display fidelity {depth:?}/{linear}/{scale}");
                    eprintln!(
                        "resident {depth:?} linear={linear} scale={scale}: warm uploads=0 readbacks=0 effect-chains=0; demand=1; exact Processed/display"
                    );
                }
            }
        }
    }

    #[test]
    fn ordinary_resident_cpu_boundary_and_denied_retention_stay_correct() {
        let Some(g) = crate::tests::gpu() else { return };
        for budget in [16 << 20, 0] {
            g.ctx.clear_uploads();
            let mut s = Scene::new(BitDepth::Bpc32);
            let mut layer = s.footage(40, 30);
            s.effect(&mut layer, "ec.blur.fastbox", &[("radius", n(3.0))]);
            s.effect(&mut layer, "ec.generate.fractal", &[]);
            let id = s.push(layer);
            let cache = LayerCache::new(budget);
            let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let before = g.ctx.transfer_stats();
            let display = g.within_memory(|| g.render_display(&r, s.cid, Tick::ZERO)).unwrap().unwrap();
            let cold = g.ctx.transfer_stats();
            assert!(cold.readbacks > before.readbacks, "real CPU-only effect boundary");
            let actual = g.read_display(&display).unwrap();
            let ctx = r.eval_ctx(s.cid, Tick::ZERO).unwrap();
            let layer = ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
            let key = r.resident_content_key(&ctx, layer).unwrap();
            assert_eq!(cache.gpu_owner().unwrap().resident(key, effectcraft_render::cache::GpuStage::Processed).is_some(), budget > 0);
            let before = g.ctx.transfer_stats();
            let calls = super::EFFECT_CHAIN_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            let again = g.within_memory(|| g.render_display(&r, s.cid, Tick::ZERO)).unwrap().unwrap();
            let warm = g.ctx.transfer_stats();
            if budget > 0 {
                assert_eq!(warm.uploads, before.uploads);
                assert_eq!(warm.readbacks, before.readbacks);
                assert_eq!(super::EFFECT_CHAIN_CALLS.load(std::sync::atomic::Ordering::Relaxed), calls);
            } else {
                assert_eq!(cache.retained_stats().live_bytes, 0);
                assert!(warm.readbacks > before.readbacks, "denial safely recomputes");
            }
            assert_eq!(actual, g.read_display(&again).unwrap());
            let mut legacy = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            legacy.accel = Some(g);
            let expected = g
                .within_memory(|| {
                    let mut enc = crate::context::Enc::with_owner(&g.ctx, None);
                    let img = crate::walk::render(&mut enc, &legacy, s.cid, Tick::ZERO).unwrap();
                    let texture = enc.display_px(&img).unwrap();
                    enc.submit();
                    crate::DisplayFrame { texture, width: img.width(), height: img.height() }
                })
                .unwrap();
            assert_eq!(actual, g.read_display(&expected).unwrap(), "CPU-boundary fidelity, budget {budget}");
            eprintln!("ordinary CPU boundary/denial: budget={budget}, correct output and bounded retention");
        }
    }

    /// A layer whose effect stack ends in a GPU chain composites from the chain's result: the
    /// same pixels, one upload fewer for the footage chain (none with linear blending).
    /// The solid is already generated/composited on the GPU, with no second upload to save.
    #[test]
    fn effect_results_composite_without_a_second_upload() {
        let Some(g) = crate::tests::gpu() else { return };
        for (depth, linear, saved) in [(BitDepth::Bpc32, false, 1), (BitDepth::Bpc8, false, 1), (BitDepth::Bpc16, true, 0)] {
            let s = scene(depth, linear);
            let (old, a) = frame(g, &s, false);
            let (new, b) = frame(g, &s, true);
            eprintln!("{depth:?} linear {linear}: re-upload {a:?}, kept {b:?}");
            assert_eq!(old.data, new.data, "{depth:?}: identical frames");
            assert_eq!(a.uploads - b.uploads, saved, "{depth:?} linear {linear}: uploads saved");
            assert_eq!(a.readbacks, b.readbacks);
        }
        // Nothing waits for adoption once the frame is done.
        assert_eq!(g.ctx.kept_count(), 0);
    }
}

#[cfg(test)]
mod text_canvas_tests {
    use super::*;
    use effectcraft_render::{FxTarget, Image};

    struct Cpu(Buf);
    impl FxTarget for Cpu {
        fn gpu(&mut self, _: &[FxStep]) -> bool {
            false
        }
        fn cpu(&mut self, f: &mut dyn FnMut(Buf) -> Buf) {
            self.0 = f(self.0.clone());
        }
        fn shift(&mut self, d: [f64; 2]) {
            self.0.offset = [self.0.offset[0] - d[0], self.0.offset[1] - d[1]];
        }
    }

    fn context() -> Option<crate::context::GpuContext> {
        crate::tests::hold_gpu_lock();
        let g = crate::context::GpuContext::headless();
        assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
        g
    }

    #[test]
    fn resident_text_canvas_matches_cpu_without_transfers() {
        let Some(g) = context() else { return };
        let mut pixels = Image::filled(3, 2, [0.25, 0.5, 0.75, 1.0]);
        pixels.data[1] = [-0.5, 1.5, 0.125, 0.5];
        for scale in [1.0, 0.5] {
            for offset in [[-2.25, 1.5], [1.25, -3.75], [0.0; 2]] {
                let source = Buf { img: pixels.clone(), offset, scale };
                let mut cpu = Cpu(source.clone());
                cpu.effect_canvas([20.0, 10.0]);
                let mut input = g.upload_image(&source.img).unwrap();
                input.levels = Some(255.0);
                let mut e = Enc::new(&g);
                let before = g.transfer_stats();
                let mut target = GpuFx::new(&mut e, input, offset, scale, Some(255.0));
                target.effect_canvas([20.0, 10.0]);
                let (image, actual_offset, actual_scale) = target.finish().unwrap();
                assert_eq!(g.transfer_stats(), before, "canvas growth stays on the GPU");
                assert_eq!(actual_offset, cpu.0.offset);
                assert_eq!(actual_scale, cpu.0.scale);
                assert_eq!(image.levels, Some(255.0));
                assert_eq!(e.download(&image).unwrap(), cpu.0.img);
            }
        }
    }

    #[test]
    fn resident_text_canvas_unchanged_and_hostile_bounds_do_not_transfer() {
        let Some(g) = context() else { return };
        let pixels = Image::filled(3, 2, [0.25, 0.5, 0.75, 1.0]);
        let input = g.upload_image(&pixels).unwrap();
        let mut e = Enc::new(&g);
        let before = g.transfer_stats();
        let mut target = GpuFx::new(&mut e, input, [0.0; 2], 1.0, None);
        for size in [[3.0, 2.0], [1.0, 1.0], [f64::NAN, 10.0], [10.0, f64::INFINITY], [16000.0; 2]] {
            target.effect_canvas(size);
        }
        let (image, offset, scale) = target.finish().unwrap();
        assert_eq!(g.transfer_stats(), before);
        assert_eq!(offset, [0.0; 2]);
        assert_eq!(scale, 1.0);
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(e.download(&image).unwrap(), pixels);
    }

    #[test]
    fn resident_text_canvas_missing_kernel_requests_cpu_fallback() {
        let Some(g) = context() else { return };
        let input = g.upload_image(&Image::filled(3, 2, [0.25, 0.5, 0.75, 1.0])).unwrap();
        let mut e = Enc::new(&g);
        let out = e.scratch(3, 2);
        e.pixels("missing-text-canvas-test-kernel", &Params::default(), &input, None, &out, None);
        let mut target = GpuFx::new(&mut e, input, [0.0; 2], 1.0, None);
        target.effect_canvas([20.0, 10.0]);
        assert!(target.finish().is_none());
    }

    #[test]
    fn resident_text_canvas_device_failure_requests_cpu_fallback() {
        let Some(g) = context() else { return };
        let input = g.upload_image(&Image::filled(3, 2, [0.25, 0.5, 0.75, 1.0])).unwrap();
        let mut e = Enc::new(&g);
        g.device_failure_notifier()("text canvas device failure test".into());
        let mut target = GpuFx::new(&mut e, input, [0.0; 2], 1.0, None);
        target.effect_canvas([20.0, 10.0]);
        assert!(target.finish().is_none());
    }
}
