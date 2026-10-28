//! GPU Layer Styles (kernels in `shaders/fx_styles.wgsl`, every entry point prefixed `sty_`):
//! `render::styles::render`'s steps for every style (Drop Shadow, Inner Shadow, Outer and Inner
//! Glow, Satin, Color and Gradient Overlay, Stroke, Bevel and Emboss), on the content cropped to
//! its visible part and padded by the styles' reach (`styles::gpu_prepare`). The soft shapes are
//! the shared box-pass Gaussian; Spread / Choke, Precise glows, Stroke and Bevel and Emboss read
//! one distance field of the content, exact within the farthest of their radii (GLS1). Each
//! pass and interior style is one kernel. A styled layer then stays on the GPU: the CPU path
//! styled it on the CPU (on a comp-sized canvas for a shape layer with effects) and uploaded
//! every pass.

use effectcraft_color::BlendMode;
use effectcraft_render::styles::{GpuBevel, GpuGlow, GpuShadow, GpuStroke, GpuStyles};

use crate::context::{Enc, GpuImage, Params};
use crate::effects::{Precision, gaussian_blur_owned};

/// Compute entry points in `fx_styles.wgsl`.
pub(crate) const KERNELS: &[&str] = &[
    "sty_alpha",
    "sty_pack",
    "sty_scale",
    "sty_pass",
    "sty_body",
    "sty_edt_rows",
    "sty_edt_cols",
    "sty_bev_height",
    "sty_bev_pack",
    "sty_bevel",
    "sty_bounds",
    "sty_field_shape",
    "sty_stroke",
];

/// Rows (or columns) a distance-field tile covers (`BEV_T` in `fx_styles.wgsl`).
const BEV_TILE: u32 = 256;

/// A styled layer on the GPU: exterior passes (bottom first, with their modes) and the body,
/// all the size of the padded content they were made from.
pub(crate) struct GpuStyled {
    pub passes: Vec<(GpuImage, BlendMode)>,
    pub body: GpuImage,
}

/// The planes [`render`] blurs: `(alpha, 1 − alpha, 0, alpha)` of the content, blurred by
/// `sigma` (edges repeated, as `effects::util::gauss_plane`), memoised per sigma. Only the style
/// kernels read them, so on a display render with half working images they are RGBA16F
/// (`half`, see [`crate::half`]): alpha planes need no more than 8-bit-visible precision.
struct Planes<'a> {
    content: &'a GpuImage,
    base: Option<GpuImage>,
    blurred: Vec<(u64, GpuImage)>,
    half: bool,
}

impl Planes<'_> {
    fn base(&mut self, e: &mut Enc) -> GpuImage {
        if let Some(b) = &self.base {
            return b.clone();
        }
        let out = e.scratch_as(self.content.width, self.content.height, self.half);
        e.pixels("sty_alpha", &Params::default(), self.content, None, &out, None);
        self.base = Some(out.clone());
        out
    }

    fn blurred(&mut self, e: &mut Enc, sigma: f64) -> GpuImage {
        let base = self.base(e);
        if sigma <= 0.05 || !sigma.is_finite() {
            return base;
        }
        let key = sigma.to_bits();
        if let Some((_, b)) = self.blurred.iter().find(|(k, _)| *k == key) {
            return b.clone();
        }
        let b = gaussian_blur_owned(e, base, sigma, sigma, true, if self.half { Precision::Half } else { Precision::F32 });
        self.blurred.push((key, b.clone()));
        b
    }

    /// `(plane, 0, 0, content alpha)`; `inverse` takes the `1 − alpha` plane.
    fn packed(&mut self, e: &mut Enc, sigma: f64, inverse: bool) -> GpuImage {
        let b = self.blurred(e, sigma);
        self.pack(e, &b, inverse)
    }

    /// `(plane.x, 0, 0, content alpha)`, or `plane.y` when `inverse`.
    fn pack(&self, e: &mut Enc, plane: &GpuImage, inverse: bool) -> GpuImage {
        let mut p = Params::default();
        p.u[0] = [inverse as u32, 0, 0, 0];
        let out = e.scratch_as(self.content.width, self.content.height, self.half);
        e.pixels("sty_pack", &p, plane, Some(self.content), &out, None);
        out
    }

    /// A shape from the distance field (`sty_field_shape`): `(outer, inner, 0, alpha)`. Kind 0
    /// = the content grown by `r` (`styles::grown`, inner: the outside grown by `r`); kind 1 =
    /// a Precise glow's intensity (`styles::glow_intensity`, hard part `r`, falloff `soft`).
    fn field_shape(&self, e: &mut Enc, field: &GpuImage, kind: u32, r: f32, soft: f32) -> GpuImage {
        let mut p = Params::default();
        p.u[0] = [0, 0, kind, 0];
        p.f[0] = [r, soft, 0.0, 0.0];
        let out = e.scratch(self.content.width, self.content.height);
        e.pixels("sty_field_shape", &p, field, None, &out, None);
        out
    }

    /// `styles::soft_shape` with Spread / Choke: the content grown by `r` on the field, blurred
    /// by `sigma` (edges repeated); `(outer, inner, 0, ·)`.
    fn grown(&self, e: &mut Enc, field: &GpuImage, r: f32, sigma: f64) -> GpuImage {
        let base = self.field_shape(e, field, 0, r, 0.0);
        if sigma <= 0.05 || !sigma.is_finite() {
            return base;
        }
        gaussian_blur_owned(e, base, sigma, sigma, true, if self.half { Precision::Half } else { Precision::F32 })
    }

    /// A glow's shape: Precise from the field, Softer the (grown) content blurred.
    fn glow(&mut self, e: &mut Enc, field: Option<&GpuImage>, g: &GpuGlow) -> Option<GpuImage> {
        Some(match g.precise {
            Some(soft) => self.field_shape(e, field?, 1, g.spread, soft),
            None if g.spread > 0.0 => self.grown(e, field?, g.spread, g.sigma),
            None => self.blurred(e, g.sigma),
        })
    }
}

fn rgb_opacity(c: [f32; 3], opacity: f32) -> [f32; 4] {
    [c[0], c[1], c[2], opacity]
}

fn shadow_params(plan: &GpuStyles, d: &GpuShadow, kind: u32) -> Params {
    let mut p = Params::default();
    p.u[0] = [kind, plan.seed, d.knocks_out as u32, 0];
    p.u[1] = [plan.org[0], plan.org[1], 0, 0];
    p.f[0] = rgb_opacity(d.color, d.opacity);
    p.f[1] = [d.offset[0], d.offset[1], d.noise, 0.0];
    p
}

fn glow_params(plan: &GpuStyles, g: &GpuGlow, kind: u32) -> Params {
    let mut p = Params::default();
    // Flags: 1 gradient, 8 from the centre, 32 Precise (the plane is the intensity itself).
    let flags = g.gradient.is_some() as u32 | if g.center { 8 } else { 0 } | if g.precise.is_some() { 32 } else { 0 };
    p.u[0] = [kind, plan.seed, 0, flags];
    p.u[1] = [plan.org[0], plan.org[1], 0, 0];
    p.f[0] = rgb_opacity(g.color, g.opacity);
    p.f[1] = [0.0, 0.0, g.noise, g.jitter];
    p.f[2] = [g.range, 0.0, 0.0, 0.0];
    p
}

/// An interior style kernel's mode words: `u[0].y` the mode, flag 16 for Dissolve's noise.
fn body_mode(p: &mut Params, mode: BlendMode) {
    p.u[0][1] = crate::ops::mode_id(mode);
    if mode == BlendMode::Dissolve {
        p.u[0][3] |= 16;
    }
}

/// Render `plan` around `content` (the padded content `styles::gpu_prepare` returned, on the
/// GPU). `None` when a kernel is missing or the device failed: the layer goes back to the CPU.
pub(crate) fn render(e: &mut Enc, content: &GpuImage, plan: &GpuStyles) -> Option<GpuStyled> {
    let (w, h) = (content.width, content.height);
    if w == 0 || h == 0 || !e.g.fits(w, h) {
        return None;
    }
    e.take_missing();
    let half = Precision::pick(e, &["sty_alpha", "sty_pack"], true) == Precision::Half;
    // One distance field for every style that reads it, exact to the farthest one's radius.
    let field = (plan.field > 0).then(|| distance_field(e, content, plan.field));
    let field = field.as_ref();
    let mut planes = Planes { content, base: None, blurred: vec![], half };
    let mut passes = vec![];
    for d in &plan.drops {
        let shape = if d.spread > 0.0 { planes.grown(e, field?, d.spread, d.sigma) } else { planes.blurred(e, d.sigma) };
        let out = e.scratch(w, h);
        e.pixels("sty_pass", &shadow_params(plan, d, 0), &shape, Some(content), &out, None);
        passes.push((out, d.mode));
    }
    if let Some(g) = &plan.outer_glow {
        let shape = planes.glow(e, field, g)?;
        let data = g.gradient.as_ref().map(|v| e.data(v));
        let out = e.scratch(w, h);
        e.pixels("sty_pass", &glow_params(plan, g, 1), &shape, Some(content), &out, data.as_ref());
        passes.push((out, g.mode));
    }

    let mut body = content.clone();
    let scale = |e: &mut Enc, img: &GpuImage, k: f32| {
        let mut p = Params::default();
        p.f[0] = [k, 0.0, 0.0, 0.0];
        let out = e.scratch(w, h);
        e.pixels("sty_scale", &p, img, None, &out, None);
        out
    };
    if !plan.interior_group && plan.fill < 1.0 {
        body = scale(e, &body, plan.fill);
    }
    let step = |e: &mut Enc, body: &mut GpuImage, p: &Params, aux: &GpuImage, data: Option<&wgpu::Buffer>| {
        let out = e.scratch(w, h);
        e.pixels("sty_body", p, body, Some(aux), &out, data);
        *body = out;
    };
    if let Some(go) = &plan.gradient {
        let mut p = Params::default();
        p.u[0] = [1, 0, plan.seed, if go.reverse { 2 } else { 0 }];
        p.u[1] = [plan.org[0], plan.org[1], go.style, 0];
        p.f[0] = [0.0, 0.0, 0.0, go.opacity];
        p.f[2] = [0.0, go.center[0], go.center[1], go.ext];
        p.f[3] = [go.dir[0], go.dir[1], go.radius, go.angle];
        body_mode(&mut p, go.mode);
        let data = e.data(&go.stops);
        step(e, &mut body, &p, content, Some(&data));
    }
    if let Some((mode, color, opacity)) = plan.color {
        let mut p = Params::default();
        p.u[0] = [0, 0, plan.seed, 0];
        p.u[1] = [plan.org[0], plan.org[1], 0, 0];
        p.f[0] = rgb_opacity(color, opacity);
        body_mode(&mut p, mode);
        step(e, &mut body, &p, content, None);
    }
    if let Some(sa) = &plan.satin {
        let aux = planes.packed(e, sa.sigma, false);
        let mut p = Params::default();
        p.u[0] = [2, 0, plan.seed, if sa.invert { 4 } else { 0 }];
        p.u[1] = [plan.org[0], plan.org[1], 0, 0];
        p.f[0] = rgb_opacity(sa.color, sa.opacity);
        p.f[1] = [sa.offset[0], sa.offset[1], 0.0, 0.0];
        body_mode(&mut p, sa.mode);
        step(e, &mut body, &p, &aux, None);
    }
    if let Some(g) = &plan.inner_glow {
        let aux = if g.precise.is_none() && g.spread <= 0.0 {
            planes.packed(e, g.sigma, true)
        } else {
            let shape = planes.glow(e, field, g)?;
            planes.pack(e, &shape, true)
        };
        let mut p = glow_params(plan, g, 3);
        // The body kernel keeps the seed in `u[0].z` (`u[0].y` is the mode).
        p.u[0][2] = plan.seed;
        body_mode(&mut p, g.mode);
        let data = g.gradient.as_ref().map(|v| e.data(v));
        step(e, &mut body, &p, &aux, data.as_ref());
    }
    for d in &plan.inner_shadows {
        let aux = if d.spread > 0.0 {
            let shape = planes.grown(e, field?, d.spread, d.sigma);
            planes.pack(e, &shape, true)
        } else {
            planes.packed(e, d.sigma, true)
        };
        let mut p = shadow_params(plan, d, 4);
        p.u[0] = [4, 0, plan.seed, 0];
        body_mode(&mut p, d.mode);
        step(e, &mut body, &p, &aux, None);
    }
    if plan.interior_group && plan.fill < 1.0 {
        body = scale(e, &body, plan.fill);
    }
    // The memoised planes are done with: back to the pool before the bevel's working images.
    drop(planes);
    if let Some(sk) = &plan.stroke {
        body = stroke(e, field?, &body, plan, sk);
    }
    if let Some(bv) = &plan.bevel {
        body = bevel(e, field?, &body, plan, bv);
    }
    if e.take_missing() || e.g.check_health().is_err() {
        return None;
    }
    Some(GpuStyled { passes, body })
}

/// The content's distance field (`sty_edt_rows` then `sty_edt_cols`): per pixel the squared
/// distances to the nearest pixel inside (alpha >= 0.5) and outside, exact up to `reach`
/// pixels (capped there), and the content alpha. f32: squared distances overflow half floats.
fn distance_field(e: &mut Enc, content: &GpuImage, reach: u32) -> GpuImage {
    let (w, h) = (content.width, content.height);
    let mut p = Params::default();
    p.u[0] = [reach, BEV_TILE, 0, 0];
    let rows = e.scratch(w, h);
    e.dispatch("sty_edt_rows", &p, content, None, &rows, None, (h.div_ceil(64), w.div_ceil(BEV_TILE)));
    let field = e.scratch(w, h);
    e.dispatch("sty_edt_cols", &p, &rows, None, &field, None, (w.div_ceil(64), h.div_ceil(BEV_TILE)));
    field
}

/// Stroke (`styles::render`'s Stroke step) over `body`: the inside ring blended with the
/// stroke's mode inside the content, then the outside ring added where it leaves the pixel
/// uncovered, both from the distance field.
fn stroke(e: &mut Enc, field: &GpuImage, body: &GpuImage, plan: &GpuStyles, sk: &GpuStroke) -> GpuImage {
    let mut p = Params::default();
    p.u[0] = [0, crate::ops::mode_id(sk.mode), plan.seed, if sk.mode == BlendMode::Dissolve { 16 } else { 0 }];
    p.u[1] = [plan.org[0], plan.org[1], 0, 0];
    p.f[0] = rgb_opacity(sk.color, sk.opacity);
    p.f[1] = [sk.inside, sk.outside, 0.0, 0.0];
    let out = e.scratch(body.width, body.height);
    e.pixels("sty_stroke", &p, body, Some(field), &out, None);
    out
}

/// Bevel and Emboss (`styles::bevel`) over `body`: from the content's distance field (exact
/// within `bv.reach` pixels of its edge, see `fx_styles.wgsl`), the smoothed height profile,
/// then the shadow and highlight blended by the light.
///
/// Each working image goes back to the texture pool as soon as the last pass reading it is
/// recorded (the next one of its size reuses it within the same encoder). On a display render
/// with half working images, the height profile, blur passes, packed height and region are
/// RGBA16F; the distance field keeps f32 (squared distances overflow half floats).
fn bevel(e: &mut Enc, field: &GpuImage, body: &GpuImage, plan: &GpuStyles, bv: &GpuBevel) -> GpuImage {
    let (w, h) = (field.width, field.height);
    let mut p = Params::default();
    p.u[0] = [bv.reach, BEV_TILE, bv.style, 0];
    p.f[0] = [bv.size, 0.0, 0.0, 0.0];
    let half = Precision::pick(e, &["sty_bev_height", "sty_bev_pack"], true) == Precision::Half;
    let precision = if half { Precision::Half } else { Precision::F32 };
    let height = e.scratch_as(w, h, half);
    e.pixels("sty_bev_height", &p, field, None, &height, None);
    // The blurs own their inputs: `height` (and the first smoothing) go back after one pass.
    let mut smooth = gaussian_blur_owned(e, height, bv.sigma, bv.sigma, true, precision);
    if bv.soften > 0.0 {
        smooth = gaussian_blur_owned(e, smooth, bv.soften, bv.soften, true, precision);
    }
    let packed = e.scratch_as(w, h, half);
    e.pixels("sty_bev_pack", &p, &smooth, Some(field), &packed, None);
    drop(smooth);
    let (sh_mode, sh_color, sh_opacity) = bv.shadow;
    let (hl_mode, hl_color, hl_opacity) = bv.highlight;
    let mut q = Params::default();
    let dissolve = |m: BlendMode, bit: u32| if m == BlendMode::Dissolve { bit } else { 0 };
    q.u[0] = [0, crate::ops::mode_id(sh_mode), plan.seed, dissolve(sh_mode, 16) | dissolve(hl_mode, 32)];
    q.u[1] = [plan.org[0], plan.org[1], crate::ops::mode_id(hl_mode), 0];
    q.f[0] = rgb_opacity(sh_color, sh_opacity);
    q.f[1] = rgb_opacity(hl_color, hl_opacity);
    q.f[2] = [bv.k, bv.light[0], bv.light[1], bv.light[2]];
    let out = e.scratch(w, h);
    e.pixels("sty_bevel", &q, body, Some(&packed), &out, None);
    out
}
