//! Layer Styles rendering (Photoshop-style layer effects, as After Effects renders them).
//!
//! Styles run in layer space after masks and effects and before the transform. The layer's
//! processed pixels (the *content*) become:
//!
//! * **exterior passes** — Drop Shadow, then Outer Glow — separate buffers composited onto the
//!   comp *below* the layer, each with its own blend mode (so a Multiply shadow multiplies the
//!   layers underneath, as in AE);
//! * the **body** — the content scaled by Fill Opacity, then the interior styles in Photoshop's
//!   stacking order (Gradient Overlay, Color Overlay, Satin, Inner Glow, Inner Shadow), each
//!   clipped to the content's alpha and blended with its mode, then Stroke and Bevel and Emboss.
//!   With *Blend Interior Styles as Group* the interior styles are faded by Fill Opacity too.
//!
//! The compositor applies the layer's opacity to the whole stack, and Advanced Blending's
//! Knockout and R/G/B channel switches at composite time (see `Renderer::composite_styled`).
//! Every distance is in layer pixels and scales with the render resolution (Half, Quarter…).
//!
//! Shapes come from a signed distance field of the content alpha (exact Euclidean distance
//! transform, Felzenszwalb–Huttenlocher) for spread/choke, strokes, precise glows and bevel
//! profiles, and from the shared separable Gaussian (`effects::util::gauss_plane`) for soft
//! shadows and glows. All per-pixel work is parallel (rayon).

use effectcraft_color::{BlendMode, blend_pixel};
use effectcraft_effects::Buf;
use effectcraft_effects::util::{Plane, gauss_plane};
use effectcraft_keyframe::{Gradient, Value};
use effectcraft_project::styles::{self as st, style_blend_mode};
use effectcraft_project::{Layer, PropGroup};
use effectcraft_raster::{Image, hash_noise};
use rayon::prelude::*;

use crate::eval::EvalCtx;

/// Largest padding (buffer pixels) styles may add on each side.
const MAX_PAD: f64 = 2048.0;

/// A styled layer: exterior passes (bottom first, each with its blend mode) and the body.
#[derive(Clone, Debug)]
pub struct Styled {
    pub passes: Vec<(Buf, BlendMode)>,
    pub body: Buf,
}

impl Styled {
    /// Everything composited into one buffer (for 3D layers, track mattes and layer references).
    pub fn flatten(&self) -> Buf {
        if self.passes.is_empty() {
            return self.body.clone();
        }
        let mut img = Image::new(self.body.img.width, self.body.img.height);
        for (p, m) in &self.passes {
            img.blend_from(&p.img, *m, 1.0, 0);
        }
        img.blend_from(&self.body.img, BlendMode::Normal, 1.0, 0);
        Buf { img, offset: self.body.offset, scale: self.body.scale }
    }
}

/// Advanced Blending options evaluated at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blending {
    pub fill: f32,
    /// Red, green, blue channel switches.
    pub channels: [bool; 3],
    /// 0 None, 1 Shallow, 2 Deep.
    pub knockout: u32,
    pub interior_group: bool,
}

impl Default for Blending {
    fn default() -> Self {
        Blending { fill: 1.0, channels: [true; 3], knockout: 0, interior_group: false }
    }
}

/// The layer's Layer Styles group when styles are switched on.
pub fn group(layer: &Layer) -> Option<&PropGroup> {
    layer.layer_styles().filter(|g| g.enabled && layer.can_have_styles())
}

/// Evaluated Advanced Blending (defaults when the layer has no styles).
pub fn blending(ctx: &EvalCtx, layer: &Layer) -> Blending {
    let Some(adv) = group(layer).and_then(|g| g.sub(st::BLENDING)).and_then(|b| b.sub("advancedBlending")) else { return Blending::default() };
    Blending {
        fill: (ctx.f(layer, adv, "fillOpacity", 100.0) / 100.0).clamp(0.0, 1.0) as f32,
        channels: [ctx.b(layer, adv, "red"), ctx.b(layer, adv, "green"), ctx.b(layer, adv, "blue")],
        knockout: ctx.e(layer, adv, "knockout"),
        interior_group: ctx.b(layer, adv, "blendInteriorAsGroup"),
    }
}

/// Global light (angle, altitude) in degrees for the layer.
fn global_light(ctx: &EvalCtx, layer: &Layer, g: &PropGroup) -> (f64, f64) {
    match g.sub(st::BLENDING) {
        Some(bo) => (ctx.f(layer, bo, "globalLightAngle", st::DEFAULT_ANGLE), ctx.f(layer, bo, "globalLightAltitude", st::DEFAULT_ALTITUDE)),
        None => (st::DEFAULT_ANGLE, st::DEFAULT_ALTITUDE),
    }
}

/// Does the layer need the styled path (any style on, or non-default blending)?
pub fn active(ctx: &EvalCtx, layer: &Layer) -> bool {
    let Some(g) = group(layer) else { return false };
    g.groups().any(|s| s.enabled && s.match_id != st::BLENDING) || blending(ctx, layer) != Blending::default()
}

/// Layer-space margin the layer's styles reach beyond its content ([`render`] pads the content
/// by it, in buffer pixels rounded up, plus two); 0 when the styled path is off. `None` when an
/// exterior pass uses a stencil mode (it changes the comp outside the layer).
pub(crate) fn growth(ctx: &EvalCtx, layer: &Layer) -> Option<f64> {
    if !active(ctx, layer) {
        return Some(0.0);
    }
    if pass_modes(ctx, layer).iter().any(|m| m.is_stencil()) {
        return None;
    }
    let g = group(layer)?;
    Some(reach(&evaluate(ctx, layer, g))).filter(|r| r.is_finite())
}

/// Blend modes of the exterior passes [`render`] produces (Drop Shadow, then Outer Glow).
pub fn pass_modes(ctx: &EvalCtx, layer: &Layer) -> Vec<BlendMode> {
    let Some(g) = group(layer) else { return vec![] };
    g.groups()
        .filter(|s| s.enabled && s.match_id == "dropShadow")
        .chain(g.groups().filter(|s| s.enabled && s.match_id == "outerGlow").last())
        .map(|s| style_blend_mode(ctx.e(layer, s, "blendMode")))
        .collect()
}

// ------------------------------------------------------------------------------------------
// Evaluated styles

struct Shadow {
    mode: BlendMode,
    color: [f32; 3],
    opacity: f32,
    offset: [f64; 2],
    spread: f64,
    size: f64,
    noise: f32,
    knocks_out: bool,
}

struct Glow {
    mode: BlendMode,
    opacity: f32,
    noise: f32,
    gradient: Option<Gradient>,
    color: [f32; 3],
    precise: bool,
    center: bool,
    spread: f64,
    size: f64,
    range: f64,
    jitter: f32,
}

struct Bevel {
    style: u32,
    technique: u32,
    depth: f64,
    down: bool,
    size: f64,
    soften: f64,
    angle: f64,
    altitude: f64,
    hl: (BlendMode, [f32; 3], f32),
    sh: (BlendMode, [f32; 3], f32),
}

struct Satin {
    mode: BlendMode,
    color: [f32; 3],
    opacity: f32,
    offset: [f64; 2],
    size: f64,
    invert: bool,
}

struct Overlay {
    mode: BlendMode,
    color: [f32; 3],
    opacity: f32,
}

struct GradOverlay {
    mode: BlendMode,
    opacity: f32,
    gradient: Gradient,
    angle: f64,
    style: u32,
    reverse: bool,
    align: bool,
    scale: f64,
    offset: [f64; 2],
}

struct Stroke {
    mode: BlendMode,
    color: [f32; 3],
    size: f64,
    opacity: f32,
    position: u32,
}

#[derive(Default)]
struct Styles {
    drop: Vec<Shadow>,
    inner_shadow: Vec<Shadow>,
    outer_glow: Option<Glow>,
    inner_glow: Option<Glow>,
    bevel: Option<Bevel>,
    satin: Option<Satin>,
    color: Option<Overlay>,
    gradient: Option<GradOverlay>,
    stroke: Option<Stroke>,
}

fn rgb(c: [f32; 4]) -> [f32; 3] {
    [c[0], c[1], c[2]]
}

/// Light direction → shadow offset (the shadow falls away from the light).
fn offset(angle: f64, distance: f64) -> [f64; 2] {
    let a = angle.to_radians();
    [-distance * a.cos(), distance * a.sin()]
}

fn evaluate(ctx: &EvalCtx, layer: &Layer, g: &PropGroup) -> Styles {
    let (ga, galt) = global_light(ctx, layer, g);
    let mut s = Styles::default();
    for sg in g.groups().filter(|x| x.enabled) {
        let f = |m: &str, d: f64| ctx.f(layer, sg, m, d);
        let pct = |m: &str, d: f64| (ctx.f(layer, sg, m, d) / 100.0).clamp(0.0, 1.0);
        let mode = |m: &str| style_blend_mode(ctx.e(layer, sg, m));
        let col = |m: &str| rgb(ctx.color(layer, sg, m));
        let ang = || if ctx.b(layer, sg, "useGlobalLight") { ga } else { f("angle", st::DEFAULT_ANGLE) };
        let grad = || match ctx.group_value(layer, sg, "colors") {
            Some(Value::Gradient(g)) => *g,
            _ => Gradient::default(),
        };
        match sg.match_id.as_str() {
            "dropShadow" | "innerShadow" => {
                let inner = sg.match_id == "innerShadow";
                let sh = Shadow {
                    mode: mode("blendMode"),
                    color: col("color"),
                    opacity: pct("opacity", 75.0) as f32,
                    offset: offset(ang(), f("distance", 5.0).max(0.0)),
                    spread: pct(if inner { "choke" } else { "spread" }, 0.0),
                    size: f("size", 5.0).clamp(0.0, 250.0),
                    noise: pct("noise", 0.0) as f32,
                    knocks_out: !inner && ctx.b(layer, sg, "knocksOut"),
                };
                if inner {
                    s.inner_shadow.push(sh);
                } else {
                    s.drop.push(sh);
                }
            }
            "outerGlow" | "innerGlow" => {
                let inner = sg.match_id == "innerGlow";
                let gl = Glow {
                    mode: mode("blendMode"),
                    opacity: pct("opacity", 75.0) as f32,
                    noise: pct("noise", 0.0) as f32,
                    gradient: (ctx.e(layer, sg, "colorType") == 1).then(grad),
                    color: col("color"),
                    precise: ctx.e(layer, sg, "technique") == 1,
                    center: inner && ctx.e(layer, sg, "source") == 0,
                    spread: pct(if inner { "choke" } else { "spread" }, 0.0),
                    size: f("size", 5.0).clamp(0.0, 250.0),
                    range: pct("range", 50.0).max(0.01),
                    jitter: pct("jitter", 0.0) as f32,
                };
                if inner {
                    s.inner_glow = Some(gl);
                } else {
                    s.outer_glow = Some(gl);
                }
            }
            "bevelEmboss" => {
                let gl = ctx.b(layer, sg, "useGlobalLight");
                s.bevel = Some(Bevel {
                    style: ctx.e(layer, sg, "style"),
                    technique: ctx.e(layer, sg, "technique"),
                    depth: f("depth", 100.0).clamp(1.0, 1000.0) / 100.0,
                    down: ctx.e(layer, sg, "direction") == 1,
                    size: f("size", 5.0).clamp(0.0, 250.0),
                    soften: f("soften", 0.0).clamp(0.0, 16.0),
                    angle: if gl { ga } else { f("angle", st::DEFAULT_ANGLE) },
                    altitude: if gl { galt } else { f("altitude", st::DEFAULT_ALTITUDE) }.clamp(0.0, 90.0),
                    hl: (mode("highlightMode"), col("highlightColor"), pct("highlightOpacity", 75.0) as f32),
                    sh: (mode("shadowMode"), col("shadowColor"), pct("shadowOpacity", 75.0) as f32),
                });
            }
            "satin" => {
                s.satin = Some(Satin {
                    mode: mode("blendMode"),
                    color: col("color"),
                    opacity: pct("opacity", 50.0) as f32,
                    offset: offset(f("angle", 19.0), f("distance", 11.0).clamp(0.0, 250.0)),
                    size: f("size", 14.0).clamp(0.0, 250.0),
                    invert: ctx.b(layer, sg, "invert"),
                });
            }
            "colorOverlay" => s.color = Some(Overlay { mode: mode("blendMode"), color: col("color"), opacity: pct("opacity", 100.0) as f32 }),
            "gradientOverlay" => {
                s.gradient = Some(GradOverlay {
                    mode: mode("blendMode"),
                    opacity: pct("opacity", 100.0) as f32,
                    gradient: grad(),
                    angle: f("angle", 90.0),
                    style: ctx.e(layer, sg, "style"),
                    reverse: ctx.b(layer, sg, "reverse"),
                    align: ctx.b(layer, sg, "alignWithLayer"),
                    scale: f("scale", 100.0).clamp(10.0, 150.0) / 100.0,
                    offset: ctx.v2(layer, sg, "offset", [0.0; 2]),
                });
            }
            "stroke" => {
                s.stroke = Some(Stroke {
                    mode: mode("blendMode"),
                    color: col("color"),
                    size: f("size", 3.0).clamp(0.0, 250.0),
                    opacity: pct("opacity", 100.0) as f32,
                    position: ctx.e(layer, sg, "position"),
                });
            }
            _ => {}
        }
    }
    s
}

/// Layer-space margin the styles reach beyond the content.
fn reach(s: &Styles) -> f64 {
    let soft = |size: f64| size * 1.6;
    let mut r: f64 = 0.0;
    for d in &s.drop {
        r = r.max(d.offset[0].abs().max(d.offset[1].abs()) + soft(d.size));
    }
    for d in &s.inner_shadow {
        r = r.max(d.offset[0].abs().max(d.offset[1].abs()) + soft(d.size));
    }
    if let Some(g) = &s.outer_glow {
        r = r.max(soft(g.size));
    }
    if let Some(g) = &s.inner_glow {
        r = r.max(soft(g.size));
    }
    if let Some(b) = &s.bevel {
        r = r.max(b.size + b.soften * 2.0);
    }
    if let Some(st) = &s.stroke {
        r = r.max(st.size);
    }
    if let Some(sa) = &s.satin {
        r = r.max(sa.offset[0].abs().max(sa.offset[1].abs()) + soft(sa.size));
    }
    r
}

// ------------------------------------------------------------------------------------------
// Distance fields

/// "Infinite" squared distance (finite so parabola intersections stay exact in f64).
const FAR: f64 = 1e10;

/// 1D squared Euclidean distance transform (Felzenszwalb & Huttenlocher) of `f` into `d`.
fn dt1d(f: &[f32], d: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let fv = |i: usize| (f[i] as f64).min(FAR);
    let mut v = vec![0usize; n];
    let mut z = vec![0f64; n + 1];
    let mut k = 0usize;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    let cross = |q: usize, p: usize| ((fv(q) + (q * q) as f64) - (fv(p) + (p * p) as f64)) / (2.0 * (q as f64 - p as f64));
    for q in 1..n {
        let mut s = cross(q, v[k]);
        while k > 0 && s <= z[k] {
            k -= 1;
            s = cross(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, dq) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let dx = q as f64 - v[k] as f64;
        *dq = (dx * dx + fv(v[k])) as f32;
    }
}

/// Euclidean distance (pixels) from every pixel to the nearest pixel where `seed` is true.
fn edt(w: usize, h: usize, seed: impl Fn(usize) -> bool + Sync) -> Plane {
    let f = Plane { w, h, data: (0..w * h).into_par_iter().map(|i| if seed(i) { 0.0 } else { FAR as f32 }).collect() };
    let rows = f.map_rows(dt1d);
    let cols = rows.transpose().map_rows(dt1d).transpose();
    cols.map(|v| v.sqrt())
}

/// Signed distance to the content edge in pixels (negative inside), anti-aliased at the edge:
/// a partially covered pixel with alpha `a` sits at `0.5 - a`.
fn sdf(alpha: &Plane) -> Plane {
    let (w, h) = (alpha.w, alpha.h);
    let to_in = edt(w, h, |i| alpha.data[i] >= 0.5);
    let to_out = edt(w, h, |i| alpha.data[i] < 0.5);
    Plane {
        w,
        h,
        data: (0..w * h)
            .into_par_iter()
            .map(|i| {
                let a = alpha.data[i];
                if a > 0.0 && a < 1.0 {
                    0.5 - a
                } else if a >= 0.5 {
                    -(to_out.data[i] - 0.5)
                } else {
                    to_in.data[i] - 0.5
                }
            })
            .collect(),
    }
}

/// Coverage of the content grown by `r` pixels (negative `r` shrinks).
#[inline]
fn grown(sdf: f32, r: f32) -> f32 {
    (r + 0.5 - sdf).clamp(0.0, 1.0)
}

// ------------------------------------------------------------------------------------------
// Helpers

/// Soft shape: content grown by `spread` of `size`, blurred by the rest (`inverse` = the
/// outside of the content, for inner shadows/glows).
fn soft_shape(alpha: &Plane, d: &Lazy, size: f64, spread: f64, inverse: bool) -> Plane {
    let hard = size * spread;
    let base = if hard > 0.01 {
        let sd = d.get(alpha);
        let r = hard as f32;
        if inverse { sd.map(|v| grown(-v, r)) } else { sd.map(|v| grown(v, r)) }
    } else if inverse {
        alpha.map(|a| 1.0 - a)
    } else {
        alpha.clone()
    };
    let sigma = (size - hard).max(0.0) / 2.0;
    if sigma > 0.05 { gauss_plane(&base, sigma, sigma) } else { base }
}

/// Bilinear read of `p` displaced by `off` pixels (the plane moves by `off`).
#[inline]
fn shifted(p: &Plane, x: usize, y: usize, off: [f64; 2]) -> f32 {
    p.sample(x as f64 + 0.5 - off[0], y as f64 + 0.5 - off[1])
}

struct Lazy(std::sync::OnceLock<Plane>);
impl Lazy {
    fn get(&self, alpha: &Plane) -> &Plane {
        self.0.get_or_init(|| sdf(alpha))
    }
}

/// Where a cropped style buffer's pixel (0, 0) sat in the uncropped one (see [`crop_to_content`]):
/// noise and Dissolve read the uncropped buffer's coordinates, so cropping keeps their grain.
#[derive(Clone, Copy, Default)]
struct Org(u32, u32);

impl Org {
    #[inline]
    fn noise(self, x: usize, y: usize, seed: u32) -> f32 {
        hash_noise((x as u32).wrapping_add(self.0), (y as u32).wrapping_add(self.1), seed)
    }
}

#[inline]
fn noisy(o: Org, a: f32, noise: f32, x: usize, y: usize, seed: u32) -> f32 {
    if noise <= 0.0 { a } else { a * (1.0 - noise * o.noise(x, y, seed)) }
}

/// `dst` with the straight colour `c` at coverage `a` blended onto it at pixel (`x`, `y`).
#[inline]
fn blend_style(o: Org, mode: BlendMode, seed: u32, x: usize, y: usize, dst: [f32; 4], c: [f32; 3], a: f32) -> [f32; 4] {
    let n = if mode == BlendMode::Dissolve { o.noise(x, y, seed) } else { 0.5 };
    blend_pixel(mode, dst, [c[0] * a, c[1] * a, c[2] * a, a], n)
}

/// Blend a straight colour with coverage `a` onto every pixel of `img` (`f(x, y)` → (colour, a)).
fn blend_into(o: Org, img: &mut Image, mode: BlendMode, seed: u32, f: impl Fn(usize, usize) -> ([f32; 3], f32) + Sync) {
    let w = img.width as usize;
    img.data.par_chunks_mut(w.max(1)).enumerate().for_each(|(y, row)| {
        for (x, d) in row.iter_mut().enumerate() {
            let (c, a) = f(x, y);
            if a > 0.0 {
                *d = blend_style(o, mode, seed, x, y, *d, c, a);
            }
        }
    });
}

/// Blend an interior style clipped to the content: `f(x, y)` → (straight colour, opacity) is
/// blended onto the layer *inside* its shape, whose coverage is `clip`, so the body keeps its
/// alpha and an anti-aliased edge pixel mixes the style with the layer's colour in the
/// proportion the style covers it, instead of letting the layer's own colour show through
/// around a blended edge (#416).
fn blend_clipped(o: Org, img: &mut Image, clip: &Plane, mode: BlendMode, seed: u32, f: impl Fn(usize, usize) -> ([f32; 3], f32) + Sync) {
    let w = img.width as usize;
    img.data.par_chunks_mut(w.max(1)).zip(clip.data.par_chunks(w.max(1))).enumerate().for_each(|(y, (row, cov))| {
        for (x, (d, &k)) in row.iter_mut().zip(cov).enumerate() {
            if k > 0.0
                && let (c, a) = f(x, y)
                && a > 0.0
            {
                *d = blend_style(o, mode, seed, x, y, d.map(|v| v / k), c, a).map(|v| v * k);
            }
        }
    });
}

/// A pass image filled with a straight colour at coverage `f(x, y)`.
fn pass_image(w: usize, h: usize, f: impl Fn(usize, usize) -> ([f32; 3], f32) + Sync) -> Image {
    let mut img = Image::new(w as u32, h as u32);
    img.data.par_chunks_mut(w.max(1)).enumerate().for_each(|(y, row)| {
        for (x, d) in row.iter_mut().enumerate() {
            let (c, a) = f(x, y);
            let a = a.clamp(0.0, 1.0);
            *d = [c[0] * a, c[1] * a, c[2] * a, a];
        }
    });
    img
}

/// Glow intensity → (colour, alpha) for colour or gradient glows.
fn glow_color(o: Org, g: &Glow, v: f32, x: usize, y: usize) -> ([f32; 3], f32) {
    let v = v.clamp(0.0, 1.0).powf((0.5 / g.range) as f32);
    match &g.gradient {
        None => (g.color, v),
        Some(gr) => {
            let j = if g.jitter > 0.0 { (o.noise(x, y, 77) - 0.5) * g.jitter } else { 0.0 };
            let c = gr.sample((1.0 - v + j) as f64);
            ([c[0], c[1], c[2]], c[3] * (v * 8.0).min(1.0))
        }
    }
}

/// Glow intensity (0..1) from the content (`inner` glows read the inverse).
fn glow_intensity(alpha: &Plane, d: &Lazy, g: &Glow, s: f64, inner: bool) -> Plane {
    let size = g.size * s;
    if g.precise {
        let sd = d.get(alpha);
        let hard = (size * g.spread) as f32;
        let soft = (size as f32 - hard).max(0.5);
        return sd.map(|v| {
            let v = if inner { -v } else { v };
            (1.0 - (v - hard).max(0.0) / soft).clamp(0.0, 1.0)
        });
    }
    soft_shape(alpha, d, size, g.spread, inner).map(|v| (v * 2.0).min(1.0))
}

// ------------------------------------------------------------------------------------------
// Rendering

/// Render the layer's styles around its processed `content` (layer size `layer_size` in layer
/// pixels, for Gradient Overlay without Align with Layer).
///
/// Styles work on the content's visible part only: the content is first cropped to where its
/// alpha is non-zero, then padded by the styles' reach (a shape or text layer with effects is
/// drawn on a comp-sized canvas, mostly transparent; its styles cost 20–90 ms per frame there).
/// The result is the same as styling the whole buffer: outside the crop every style is zero.
pub fn render(ctx: &EvalCtx, layer: &Layer, content: &Buf, layer_size: [f64; 2]) -> Styled {
    render_in(ctx, layer, content, layer_size, true)
}

/// The content's visible part (alpha above 1e-6, as [`Image::alpha_bounds`]) and where its
/// pixel (0, 0) sat in `content`; `None` when that is the whole buffer already.
fn crop_to_content(content: &Buf) -> Option<(Buf, Org)> {
    let (w, h) = (content.img.width, content.img.height);
    // Nothing visible: one transparent pixel keeps the passes and the body in place.
    let (x0, y0, x1, y1) = content.img.alpha_bounds().unwrap_or((0, 0, 1.min(w), 1.min(h)));
    if (x0, y0, x1, y1) == (0, 0, w, h) || x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut img = Image::new(cw as u32, ch as u32);
    let src = content.img.data.chunks(w.max(1) as usize).skip(y0 as usize).take(ch);
    for (d, s) in img.data.chunks_mut(cw.max(1)).zip(src) {
        let row = s.get(x0 as usize..x1 as usize)?;
        d.copy_from_slice(row);
    }
    let offset = [content.offset[0] - x0 as f64, content.offset[1] - y0 as f64];
    Some((Buf { img, offset, scale: content.scale }, Org(x0, y0)))
}

/// [`render`]; `crop` off styles the whole content buffer (the tests' reference).
pub(crate) fn render_in(ctx: &EvalCtx, layer: &Layer, content: &Buf, layer_size: [f64; 2], crop: bool) -> Styled {
    let Some(g) = group(layer) else { return Styled { passes: vec![], body: content.clone() } };
    let styles = evaluate(ctx, layer, g);
    let bl = blending(ctx, layer);
    let s = content.scale.max(1e-6);
    let pad = ((reach(&styles) * s).ceil() + 2.0).min(MAX_PAD) as u32;
    if content.img.is_empty() {
        return Styled { passes: vec![], body: content.clone() };
    }
    let (mut buf, o) = match crop.then(|| crop_to_content(content)).flatten() {
        Some(c) => c,
        None => (content.clone(), Org::default()),
    };
    buf.pad(pad);
    let (w, h) = (buf.img.width as usize, buf.img.height as usize);
    let alpha = Plane::alpha(&buf.img);
    let d = Lazy(std::sync::OnceLock::new());
    let seed = layer.id.0 as u32;

    // ---- exterior passes
    let mut passes = vec![];
    for ds in &styles.drop {
        let shape = soft_shape(&alpha, &d, ds.size * s, ds.spread, false);
        let off = [ds.offset[0] * s, ds.offset[1] * s];
        let img = pass_image(w, h, |x, y| {
            let mut a = shifted(&shape, x, y, off) * ds.opacity;
            if ds.knocks_out {
                a *= 1.0 - alpha.data[y * w + x];
            }
            (ds.color, noisy(o, a, ds.noise, x, y, seed))
        });
        passes.push((Buf { img, offset: buf.offset, scale: buf.scale }, ds.mode));
    }
    if let Some(gl) = &styles.outer_glow {
        let v = glow_intensity(&alpha, &d, gl, s, false);
        let img = pass_image(w, h, |x, y| {
            let i = y * w + x;
            let (c, a) = glow_color(o, gl, v.data[i], x, y);
            // The glow sits outside the content (knocked out under it).
            (c, noisy(o, a * gl.opacity * (1.0 - alpha.data[i]), gl.noise, x, y, seed ^ 0x5a5a))
        });
        passes.push((Buf { img, offset: buf.offset, scale: buf.scale }, gl.mode));
    }

    // ---- body
    let mut body = buf.img.clone();
    if !bl.interior_group && bl.fill < 1.0 {
        body.scale_alpha(bl.fill);
    }
    if let Some(go) = &styles.gradient {
        let (cx, cy, hw, hh) = gradient_frame(go, &buf, &alpha, layer_size);
        let (ux, uy) = (go.angle.to_radians().cos(), -go.angle.to_radians().sin());
        let ext = ((hw * ux).abs() + (hh * uy).abs()).max(1e-3) * go.scale;
        let radius = hw.max(hh).max(1e-3) * go.scale;
        blend_clipped(o, &mut body, &alpha, go.mode, seed, |x, y| {
            let (px, py) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
            let along = px * ux + py * uy;
            let across = -px * uy + py * ux;
            let mut t = match go.style {
                1 => (px * px + py * py).sqrt() / radius,
                2 => {
                    let a = (-py).atan2(px) - go.angle.to_radians();
                    (a / std::f64::consts::TAU).rem_euclid(1.0)
                }
                3 => along.abs() / ext,
                4 => (along.abs() + across.abs()) / ext,
                _ => 0.5 + along / (2.0 * ext),
            };
            if go.reverse {
                t = 1.0 - t;
            }
            let c = go.gradient.sample(t.clamp(0.0, 1.0));
            ([c[0], c[1], c[2]], c[3] * go.opacity)
        });
    }
    if let Some(co) = &styles.color {
        blend_clipped(o, &mut body, &alpha, co.mode, seed, |_, _| (co.color, co.opacity));
    }
    if let Some(sa) = &styles.satin {
        let b = gauss_plane(&alpha, sa.size * s / 2.0, sa.size * s / 2.0);
        let off = [sa.offset[0] * s, sa.offset[1] * s];
        blend_clipped(o, &mut body, &alpha, sa.mode, seed, |x, y| {
            let v = (shifted(&b, x, y, off) - shifted(&b, x, y, [-off[0], -off[1]])).abs().min(1.0);
            let v = if sa.invert { 1.0 - v } else { v };
            (sa.color, v * sa.opacity)
        });
    }
    if let Some(gl) = &styles.inner_glow {
        let v = glow_intensity(&alpha, &d, gl, s, true);
        blend_clipped(o, &mut body, &alpha, gl.mode, seed, |x, y| {
            let i = y * w + x;
            let e = if gl.center { 1.0 - v.data[i] } else { v.data[i] };
            let (c, a) = glow_color(o, gl, e, x, y);
            (c, noisy(o, a * gl.opacity, gl.noise, x, y, seed ^ 0x3c3c))
        });
    }
    for is in &styles.inner_shadow {
        let shape = soft_shape(&alpha, &d, is.size * s, is.spread, true);
        let off = [is.offset[0] * s, is.offset[1] * s];
        blend_clipped(o, &mut body, &alpha, is.mode, seed, |x, y| {
            let a = shifted(&shape, x, y, off).min(1.0) * is.opacity;
            (is.color, noisy(o, a, is.noise, x, y, seed ^ 0x1234))
        });
    }
    if bl.interior_group && bl.fill < 1.0 {
        body.scale_alpha(bl.fill);
    }
    if let Some(sk) = &styles.stroke {
        let sd = d.get(&alpha);
        let size = (sk.size * s) as f32;
        let (r_out, r_in) = match sk.position {
            1 => (0.0, size),
            2 => (size / 2.0, size / 2.0),
            _ => (size, 0.0),
        };
        // Inside part: blended over the body within the content, over the share of each
        // pixel's coverage the ring takes.
        if r_in > 0.0 {
            blend_clipped(o, &mut body, &alpha, sk.mode, seed, |x, y| {
                let i = y * w + x;
                let a = alpha.data[i];
                let ring = ((a - grown(sd.data[i], -r_in)) / a.max(1e-6)).clamp(0.0, 1.0);
                (sk.color, ring * sk.opacity)
            });
        }
        // Outside part: covers exactly what the content leaves uncovered.
        if r_out > 0.0 {
            body.data.par_iter_mut().enumerate().for_each(|(i, p)| {
                let ring = (grown(sd.data[i], r_out) - alpha.data[i]).max(0.0) * sk.opacity;
                if ring > 0.0 {
                    let src = [sk.color[0] * ring, sk.color[1] * ring, sk.color[2] * ring, ring];
                    for c in 0..4 {
                        p[c] += src[c];
                    }
                }
            });
        }
    }
    if let Some(bv) = &styles.bevel {
        bevel(o, &mut body, &alpha, &d, bv, s, seed);
    }
    Styled { passes, body: Buf { img: body, offset: buf.offset, scale: buf.scale } }
}

/// Gradient Overlay frame in buffer pixels: centre and half extents.
fn gradient_frame(go: &GradOverlay, buf: &Buf, alpha: &Plane, layer_size: [f64; 2]) -> (f64, f64, f64, f64) {
    let s = buf.scale;
    let (x0, y0, x1, y1) = if go.align {
        let img = Image { width: alpha.w as u32, height: alpha.h as u32, data: alpha.data.iter().map(|&a| [0.0, 0.0, 0.0, a]).collect() };
        match img.alpha_bounds() {
            Some((a, b, c, d)) => (a as f64, b as f64, c as f64, d as f64),
            None => (0.0, 0.0, 1.0, 1.0),
        }
    } else {
        let (a, b) = buf.to_px([0.0, 0.0]);
        let (c, d) = buf.to_px(layer_size);
        (a, b, c, d)
    };
    ((x0 + x1) / 2.0 + go.offset[0] * s, (y0 + y1) / 2.0 + go.offset[1] * s, (x1 - x0) / 2.0, (y1 - y0) / 2.0)
}

/// Bevel and Emboss: a height field from the content edge, lit by a directional light.
fn bevel(o: Org, body: &mut Image, alpha: &Plane, d: &Lazy, bv: &Bevel, s: f64, seed: u32) {
    let size = (bv.size * s).max(0.5) as f32;
    let sd = d.get(alpha);
    let w = alpha.w;
    // Height profile 0..1 and the region the bevel paints.
    let style = bv.style;
    let mut height = sd.map(|v| match style {
        0 => (1.0 - v.max(0.0) / size).clamp(0.0, 1.0),
        2 | 4 => (0.5 - v / (2.0 * size)).clamp(0.0, 1.0),
        3 => (v.abs() / size).clamp(0.0, 1.0),
        _ => (-v / size + 0.5 / size).clamp(0.0, 1.0),
    });
    // The distance field is exact at pixel centres only; smoothing the profile removes the
    // stair-steps on diagonal edges that lighting would otherwise amplify. Smooth rounds the
    // profile over the bevel size; Chisel Hard keeps it crisp, Chisel Soft slightly softer.
    let sigma = match bv.technique {
        0 => (size as f64 / 4.0).max(1.0),
        2 => 1.5,
        _ => 0.75,
    };
    height = gauss_plane(&height, sigma, sigma);
    if bv.soften > 0.0 {
        let sg = bv.soften * s / 2.0;
        height = gauss_plane(&height, sg, sg);
    }
    let sign = if bv.down { -1.0 } else { 1.0 };
    let k = (size as f64 * bv.depth) as f32 * sign;
    let (th, al) = (bv.angle.to_radians(), bv.altitude.to_radians());
    let light = [(al.cos() * th.cos()) as f32, (-al.cos() * th.sin()) as f32, al.sin() as f32];
    let flat = light[2];
    let region = |i: usize| -> f32 {
        let a = alpha.data[i];
        match style {
            0 => (grown(sd.data[i], size) - a).max(0.0),
            1 => a,
            _ => grown(sd.data[i], size),
        }
    };
    let shade = |x: usize, y: usize| -> (f32, f32) {
        let hx = |xx: i64| height.get_clamped(xx, y as i64);
        let hy = |yy: i64| height.get_clamped(x as i64, yy);
        let gx = (hx(x as i64 + 1) - hx(x as i64 - 1)) * 0.5 * k;
        let gy = (hy(y as i64 + 1) - hy(y as i64 - 1)) * 0.5 * k;
        let n = [-gx, -gy, 1.0];
        let len = (n[0] * n[0] + n[1] * n[1] + 1.0).sqrt();
        let dot = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / len;
        let hl = ((dot - flat) / (1.0 - flat).max(1e-4)).clamp(0.0, 1.0);
        let shd = ((flat - dot) / flat.max(1e-4)).clamp(0.0, 1.0);
        (hl, shd)
    };
    blend_into(o, body, bv.sh.0, seed, |x, y| {
        let (_, shd) = shade(x, y);
        (bv.sh.1, shd * bv.sh.2 * region(y * w + x))
    });
    blend_into(o, body, bv.hl.0, seed, |x, y| {
        let (hl, _) = shade(x, y);
        (bv.hl.1, hl * bv.hl.2 * region(y * w + x))
    });
}

// ------------------------------------------------------------------------------------------
// GPU plan

/// A Drop Shadow or Inner Shadow for an accelerator, in buffer pixels: the content alpha (the
/// inverse for an inner shadow) blurred by `sigma`, moved by `offset`.
#[derive(Clone, Debug)]
pub struct GpuShadow {
    pub mode: BlendMode,
    pub color: [f32; 3],
    pub opacity: f32,
    pub offset: [f32; 2],
    pub sigma: f64,
    pub noise: f32,
    pub knocks_out: bool,
}

/// A Softer Outer or Inner Glow for an accelerator: intensity `min(2 · blur(alpha), 1)`.
#[derive(Clone, Debug)]
pub struct GpuGlow {
    pub mode: BlendMode,
    pub opacity: f32,
    pub noise: f32,
    pub color: [f32; 3],
    /// Gradient stops packed by [`pack_gradient`] when the glow uses a gradient.
    pub gradient: Option<Vec<f32>>,
    pub sigma: f64,
    pub range: f32,
    pub jitter: f32,
    /// Inner Glow from the centre.
    pub center: bool,
}

/// Gradient Overlay for an accelerator: its frame in buffer pixels.
#[derive(Clone, Debug)]
pub struct GpuGradient {
    pub mode: BlendMode,
    pub opacity: f32,
    pub stops: Vec<f32>,
    pub style: u32,
    pub reverse: bool,
    pub center: [f32; 2],
    /// Unit direction of the angle (x, y down).
    pub dir: [f32; 2],
    pub ext: f32,
    pub radius: f32,
    pub angle: f32,
}

/// Satin for an accelerator.
#[derive(Clone, Debug)]
pub struct GpuSatin {
    pub mode: BlendMode,
    pub color: [f32; 3],
    pub opacity: f32,
    pub offset: [f32; 2],
    pub sigma: f64,
    pub invert: bool,
}

/// A layer's styles as an accelerator renders them (see [`gpu_prepare`]): the steps of
/// [`render`] with every distance in buffer pixels. Only styles that need no distance field
/// (spread / choke 0, Softer glows; no Stroke or Bevel and Emboss) have a plan.
#[derive(Clone, Debug)]
pub struct GpuStyles {
    /// Noise origin: where the buffer's pixel (0, 0) sat before it was cropped.
    pub org: [u32; 2],
    pub seed: u32,
    pub fill: f32,
    pub interior_group: bool,
    pub drops: Vec<GpuShadow>,
    pub outer_glow: Option<GpuGlow>,
    pub gradient: Option<GpuGradient>,
    pub color: Option<(BlendMode, [f32; 3], f32)>,
    pub satin: Option<GpuSatin>,
    pub inner_glow: Option<GpuGlow>,
    pub inner_shadows: Vec<GpuShadow>,
}

impl GpuStyles {
    /// Blend modes of the exterior passes, in order (as [`pass_modes`]).
    pub fn pass_modes(&self) -> Vec<BlendMode> {
        self.drops.iter().map(|d| d.mode).chain(self.outer_glow.as_ref().map(|g| g.mode)).collect()
    }
}

/// Gradient stops for a kernel: `[colour count, opacity count]`, then per colour stop
/// `(position, r, g, b, a, midpoint to the next)`, then per opacity stop `(position, opacity,
/// midpoint)`; midpoints as [`Gradient::sample`] reads them.
pub fn pack_gradient(g: &Gradient) -> Vec<f32> {
    let mid = |m: &[f64], i: usize| m.get(i).copied().filter(|m| m.is_finite()).unwrap_or(0.5).clamp(1e-6, 1.0 - 1e-6) as f32;
    let mut v = vec![g.colors.len() as f32, g.opacities.len() as f32];
    for (i, (p, c)) in g.colors.iter().enumerate() {
        v.extend_from_slice(&[*p as f32, c[0], c[1], c[2], c[3], mid(&g.color_midpoints, i)]);
    }
    for (i, (p, a)) in g.opacities.iter().enumerate() {
        v.extend_from_slice(&[*p as f32, *a, mid(&g.opacity_midpoints, i)]);
    }
    v
}

/// The layer's styles as an accelerator renders them, and the buffer they render around: the
/// content cropped to its visible part and padded by their reach, as [`render`] does. `None`
/// when the layer has no styles, its content is empty, or a style needs the CPU (a distance
/// field: spread or choke, Precise glows, Stroke, Bevel and Emboss; a stencil pass mode).
pub fn gpu_prepare(ctx: &EvalCtx, layer: &Layer, content: &Buf, layer_size: [f64; 2]) -> Option<(Buf, GpuStyles)> {
    let g = group(layer)?;
    if content.img.is_empty() || pass_modes(ctx, layer).iter().any(|m| m.is_stencil()) {
        return None;
    }
    let styles = evaluate(ctx, layer, g);
    if styles.stroke.is_some() || styles.bevel.is_some() {
        return None;
    }
    let s = content.scale.max(1e-6);
    let hard = |size: f64, spread: f64| size * s * spread;
    let soft = |size: f64, spread: f64| ((size * s - hard(size, spread)).max(0.0) / 2.0);
    let shadow = |d: &Shadow| {
        (hard(d.size, d.spread) <= 0.01).then(|| GpuShadow {
            mode: d.mode,
            color: d.color,
            opacity: d.opacity,
            offset: [(d.offset[0] * s) as f32, (d.offset[1] * s) as f32],
            sigma: soft(d.size, d.spread),
            noise: d.noise,
            knocks_out: d.knocks_out,
        })
    };
    let glow = |gl: &Glow| {
        (!gl.precise && hard(gl.size, gl.spread) <= 0.01).then(|| GpuGlow {
            mode: gl.mode,
            opacity: gl.opacity,
            noise: gl.noise,
            color: gl.color,
            gradient: gl.gradient.as_ref().map(pack_gradient),
            sigma: soft(gl.size, gl.spread),
            range: gl.range as f32,
            jitter: gl.jitter,
            center: gl.center,
        })
    };
    let drops = styles.drop.iter().map(shadow).collect::<Option<Vec<_>>>()?;
    let inner_shadows = styles.inner_shadow.iter().map(shadow).collect::<Option<Vec<_>>>()?;
    let outer_glow = match &styles.outer_glow {
        Some(gl) => Some(glow(gl)?),
        None => None,
    };
    let inner_glow = match &styles.inner_glow {
        Some(gl) => Some(glow(gl)?),
        None => None,
    };
    let bl = blending(ctx, layer);
    let pad = ((reach(&styles) * s).ceil() + 2.0).min(MAX_PAD) as u32;
    let (mut buf, o) = crop_to_content(content).unwrap_or_else(|| (content.clone(), Org::default()));
    buf.pad(pad);
    let gradient = styles.gradient.as_ref().map(|go| {
        let alpha = Plane::alpha(&buf.img);
        let (cx, cy, hw, hh) = gradient_frame(go, &buf, &alpha, layer_size);
        let (ux, uy) = (go.angle.to_radians().cos(), -go.angle.to_radians().sin());
        GpuGradient {
            mode: go.mode,
            opacity: go.opacity,
            stops: pack_gradient(&go.gradient),
            style: go.style,
            reverse: go.reverse,
            center: [cx as f32, cy as f32],
            dir: [ux as f32, uy as f32],
            ext: (((hw * ux).abs() + (hh * uy).abs()).max(1e-3) * go.scale) as f32,
            radius: (hw.max(hh).max(1e-3) * go.scale) as f32,
            angle: go.angle.to_radians() as f32,
        }
    });
    let satin = styles.satin.as_ref().map(|sa| GpuSatin {
        mode: sa.mode,
        color: sa.color,
        opacity: sa.opacity,
        offset: [(sa.offset[0] * s) as f32, (sa.offset[1] * s) as f32],
        sigma: sa.size * s / 2.0,
        invert: sa.invert,
    });
    let plan = GpuStyles {
        org: [o.0, o.1],
        seed: layer.id.0 as u32,
        fill: bl.fill,
        interior_group: bl.interior_group,
        drops,
        outer_glow,
        gradient,
        color: styles.color.as_ref().map(|c| (c.mode, c.color, c.opacity)),
        satin,
        inner_glow,
        inner_shadows,
    };
    Some((buf, plan))
}
