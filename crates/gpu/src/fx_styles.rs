//! GPU Layer Styles (kernels in `shaders/fx_styles.wgsl`, every entry point prefixed `sty_`):
//! `render::styles::render`'s steps for the styles that need no distance field (Drop Shadow,
//! Inner Shadow, Softer Outer and Inner Glow, Satin, Color and Gradient Overlay; spread and
//! choke 0), on the content cropped to its visible part and padded by the styles' reach
//! (`styles::gpu_prepare`). The soft shapes are the shared box-pass Gaussian; each pass and
//! interior style is one kernel. A styled layer then stays on the GPU: the CPU path styled it
//! on the CPU (on a comp-sized canvas for a shape layer with effects) and uploaded every pass.

use effectcraft_color::BlendMode;
use effectcraft_render::styles::{GpuGlow, GpuShadow, GpuStyles};

use crate::context::{Enc, GpuImage, Params};
use crate::effects::gaussian_blur;

/// Compute entry points in `fx_styles.wgsl`.
pub(crate) const KERNELS: &[&str] = &["sty_alpha", "sty_pack", "sty_scale", "sty_pass", "sty_body"];

/// A styled layer on the GPU: exterior passes (bottom first, with their modes) and the body,
/// all the size of the padded content they were made from.
pub(crate) struct GpuStyled {
    pub passes: Vec<(GpuImage, BlendMode)>,
    pub body: GpuImage,
}

/// The planes [`render`] blurs: `(alpha, 1 − alpha, 0, alpha)` of the content, blurred by
/// `sigma` (edges repeated, as `effects::util::gauss_plane`), memoised per sigma.
struct Planes<'a> {
    content: &'a GpuImage,
    base: Option<GpuImage>,
    blurred: Vec<(u64, GpuImage)>,
}

impl Planes<'_> {
    fn base(&mut self, e: &mut Enc) -> GpuImage {
        if let Some(b) = &self.base {
            return b.clone();
        }
        let out = e.scratch(self.content.width, self.content.height);
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
        let b = gaussian_blur(e, &base, sigma, sigma, true);
        self.blurred.push((key, b.clone()));
        b
    }

    /// `(plane, 0, 0, content alpha)`; `inverse` takes the `1 − alpha` plane.
    fn packed(&mut self, e: &mut Enc, sigma: f64, inverse: bool) -> GpuImage {
        let b = self.blurred(e, sigma);
        let mut p = Params::default();
        p.u[0] = [inverse as u32, 0, 0, 0];
        let out = e.scratch(self.content.width, self.content.height);
        e.pixels("sty_pack", &p, &b, Some(self.content), &out, None);
        out
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
    p.u[0] = [kind, plan.seed, 0, g.gradient.is_some() as u32 | if g.center { 8 } else { 0 }];
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
    let mut planes = Planes { content, base: None, blurred: vec![] };
    let mut passes = vec![];
    for d in &plan.drops {
        let shape = planes.blurred(e, d.sigma);
        let out = e.scratch(w, h);
        e.pixels("sty_pass", &shadow_params(plan, d, 0), &shape, Some(content), &out, None);
        passes.push((out, d.mode));
    }
    if let Some(g) = &plan.outer_glow {
        let shape = planes.blurred(e, g.sigma);
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
        let aux = planes.packed(e, g.sigma, true);
        let mut p = glow_params(plan, g, 3);
        // The body kernel keeps the seed in `u[0].z` (`u[0].y` is the mode).
        p.u[0][2] = plan.seed;
        body_mode(&mut p, g.mode);
        let data = g.gradient.as_ref().map(|v| e.data(v));
        step(e, &mut body, &p, &aux, data.as_ref());
    }
    for d in &plan.inner_shadows {
        let aux = planes.packed(e, d.sigma, true);
        let mut p = shadow_params(plan, d, 4);
        p.u[0] = [4, 0, plan.seed, 0];
        body_mode(&mut p, d.mode);
        step(e, &mut body, &p, &aux, None);
    }
    if plan.interior_group && plan.fill < 1.0 {
        body = scale(e, &body, plan.fill);
    }
    if e.take_missing() || e.g.check_health().is_err() {
        return None;
    }
    Some(GpuStyled { passes, body })
}
