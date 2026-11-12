//! Deep Glow's "Deep Glow 2" glow model (`ec.stylize.deepglow`, Glow Model 0, the default):
//! the glow Plugin Everything's Deep Glow 2 renders, matched to its After Effects output (dot,
//! edge and bar probes at Radius 50-800, every Quality Preset, Exposure, Threshold, Gamma and
//! Blend Mode; reference model and fits in the maintainer's lab notes, RE-deepglow2).
//!
//! The glow is a pyramid of round Gaussians, one per octave of Radius:
//! - the glow input is the layer as seen over black (RGB × alpha), keyed per channel by
//!   Threshold (`x · smoothstep(T − S, T + S, x)`), masked, clamped to 1 for Screen, then taken
//!   to linear light with a plain power (the scene gamma: 2.4, or 1 in a linearised working
//!   space; Auto Detect Gamma off: `1 + 1.2 · Gamma Correction Amount / 100`);
//! - `N = min(10, ⌈log₂ R⌉)` levels; level `i` has half-width `rᵢ = R / 2^(N − i)` and is a
//!   separable Gaussian of σ = 0.2449 rᵢ cut at ±rᵢ, normalised, weighted by a fade that only
//!   dims the first levels of small radii. Each level is blurred at its own reduced size (the
//!   Quality Preset's Resolution, compounding per level), fed by a 13-tap downsample chain and
//!   summed back coarse to fine with a 3 × 3 tent upsample (added, or screened for Screen);
//! - the sum × Exposure (a plain linear gain), tint, back to display values with 1/γ, clamped
//!   for Screen; the layer goes on top in display values (Screen `1 − (1 − g)(1 − s)` or Add),
//!   then Unmult sets alpha = min(1, max(r, g, b)).
//!
//! So the halo is round, ends at R/2 and has equal energy per octave (an inverse-square-like
//! falloff): no square box-blur support, no rim. The GPU kernels (`effectcraft-gpu`
//! `fx_deepglow`, `fdg_*`) run the same steps operation for operation; this CPU version keeps
//! every image to the rectangle where it can be non-zero.
//!
//! Spread, Downsample, Dither, Buffer Expansion and the Gamma Correction checkbox belong to
//! the Legacy model (Deep Glow v1's parameters; Deep Glow 2 has none of them) and are ignored.

use effectcraft_raster::Px;
use rayon::prelude::*;

use crate::{Buf, EffectCtx};

/// Quality Preset (Draft, Low, Medium, Standard, High, Extreme) → (Steps Multiplier,
/// Resolution, extra buffer px).
pub const DG2_PRESETS: [(f64, f64, f64); 6] =
    [(0.25, 0.80, 75.0), (0.25, 0.85, 55.0), (0.33, 0.90, 45.0), (0.5, 0.90, 40.0), (0.5, 0.97, 30.0), (1.0, 1.0, 10.0)];

/// The Glow Model popup (`glowModel`).
pub const DEEP_GLOW_MODELS: [&str; 2] = ["Deep Glow 2", "Legacy"];

/// Whether Deep Glow's parameters pick the Deep Glow 2 model.
pub fn deep_glow_is_dg2(params: &crate::Params) -> bool {
    params.e("glowModel") == 0
}

/// One pyramid level: its size, blur taps per side and their spacing (level px) per axis, and
/// its per-channel weight (fade × Chromatic Aberration).
#[derive(Clone, Debug, PartialEq)]
pub struct Dg2Level {
    pub w: u32,
    pub h: u32,
    pub steps: [u32; 2],
    pub stride: [f32; 2],
    pub fade: [f32; 3],
}

/// Tint Mode / colour / mix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dg2Tint {
    pub color: [f32; 3],
    pub mode: u32,
    pub mix: f32,
}

/// Deep Glow 2's settings, read once from the effect's parameters (shared with the GPU).
#[derive(Clone, Debug, PartialEq)]
pub struct Dg2Settings {
    /// Radius (comp px) and the buffer's scale (preview resolution).
    pub radius: f64,
    pub scale: f64,
    /// Steps Multiplier, per-level resolution, whether every level uses it once (Custom) or it
    /// compounds, and whether the first level is already reduced.
    pub steps_mult: f64,
    pub res: f64,
    pub custom: bool,
    pub reduce_first: bool,
    pub preset_pad: f64,
    pub aspect: f64,
    pub chroma: f32,
    /// The gamma the glow is built in (1 = none).
    pub gamma: f32,
    pub screen: bool,
    pub exposure: f32,
    pub threshold: f32,
    pub smooth: f32,
    pub source_opacity: f32,
    pub unmult: bool,
    pub tint: Option<Dg2Tint>,
    pub view_input: bool,
}

impl Dg2Settings {
    /// `None` when the glow is too small to do anything (the layer comes back unchanged).
    pub fn new(ctx: &EffectCtx, scale: f64) -> Option<Dg2Settings> {
        let pr = ctx.params;
        let radius = pr.f("PEDG-0001").clamp(0.0, 2000.0);
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        if !(radius * scale).is_finite() || radius * scale < 0.5 {
            return None;
        }
        let preset = pr.e("PEDG2-0048");
        let (steps_mult, res, custom, reduce_first, preset_pad) = match DG2_PRESETS.get(preset as usize) {
            Some(&(m, q, pad)) => (m, q, false, preset < 3, pad),
            None => {
                let q = (pr.f("PEDG2-0049") / 100.0).clamp(0.25, 1.0);
                (pr.f("PEDG2-0050").clamp(0.01, 3.0), q, true, true, (1.1 - q) * 40.0)
            }
        };
        let gamma =
            if pr.b("PEDG-0043") { if ctx.env.working_linear { 1.0 } else { 2.4 } } else { 1.0 + 1.2 * pr.f("PEDG2-0154").clamp(0.0, 300.0) / 100.0 } as f32;
        let tint = pr.b("PEDG-0037").then(|| {
            let c = pr.color("PEDG-0038");
            Dg2Tint { color: [c[0], c[1], c[2]], mode: pr.e("PEDG-0039"), mix: (pr.f("PEDG-0041") / 100.0).clamp(0.0, 1.0) as f32 }
        });
        let threshold = (pr.f("PEDG-0004") / 100.0).max(0.0) as f32;
        Some(Dg2Settings {
            radius,
            scale,
            steps_mult,
            res,
            custom,
            reduce_first,
            preset_pad,
            aspect: pr.f("PEDG-0010").clamp(0.0, 2.0),
            chroma: if pr.b("PEDG-0013") { pr.f("PEDG-0015").clamp(0.0, 2.0) as f32 } else { 0.0 },
            gamma: if gamma > 1.01 { gamma } else { 1.0 },
            screen: pr.e("PEDG-0008") == 0,
            exposure: crate::misc::exposure_gain(pr.f("glowExposure")),
            threshold,
            smooth: ((pr.f("PEDG-0005") / 100.0).max(0.001) as f32).min(threshold),
            source_opacity: (pr.f("PEDG-0024") / 100.0).clamp(0.0, 1.0) as f32,
            unmult: pr.b("PEDG-0025"),
            tint,
            view_input: pr.e("PEDG-0023") == 0,
        })
    }

    /// The transparent border the glow needs around a `w × h` buffer (its reach, R/2, plus the
    /// down/upsamples' few pixels), kept to a 16 Mpx frame; none with Edge Behaviour 0 or on an
    /// adjustment layer.
    pub fn pad(&self, ctx: &EffectCtx, w: u32, h: u32) -> u32 {
        if ctx.params.e("PEDG-0055") == 0 || ctx.adjustment {
            return 0;
        }
        let want = (self.radius * self.scale * 0.5 + self.preset_pad * self.scale + 8.0).ceil().min(1e6) as u32;
        let (mut low, mut high) = (0u32, want);
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let fw = u64::from(w) + 2 * u64::from(mid);
            let fh = u64::from(h) + 2 * u64::from(mid);
            if fw * fh <= 16_000_000 {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    }

    /// The pyramid for a `buf_w × buf_h` (padded) buffer.
    pub fn levels(&self, buf_w: u32, buf_h: u32) -> Vec<Dg2Level> {
        let r = self.radius;
        let mut v = r * self.scale;
        let mut n = 10usize;
        for k in 0..10 {
            v *= 0.5;
            if v <= 1.0 {
                n = k + 1;
                break;
            }
        }
        // Fades of the first levels for small radii (a 0 ends the pyramid).
        let raw = [
            1.0,
            r / 20.0,
            r / 40.0,
            (r - 5.0) / 55.0,
            (r - 10.0) / 70.0,
            (r - 20.0) / 100.0,
            (r - 35.0) / 105.0,
            (r - 60.0) / 100.0,
            (r - 90.0) / 90.0,
            (r - 110.0) / 110.0,
        ];
        let sm = self.steps_mult / self.scale;
        let mut out: Vec<Dg2Level> = Vec::with_capacity(n);
        let (mut scale, mut half) = (1.0f64, v);
        let mut i = 0;
        while i < n {
            if i == 0 {
                scale = if self.reduce_first { self.res } else { 1.0 };
            } else {
                scale = if self.custom { self.res } else { scale * self.res };
                half *= 2.0;
            }
            let fade = raw.get(i).copied().unwrap_or(1.0).clamp(0.0, 1.0) as f32;
            let rl = scale * half;
            let (rx, ry) = (self.aspect * rl, (2.0 - self.aspect).abs() * rl);
            let steps = |rr: f64| ((sm * rr).floor().clamp(1.0, 4096.0)) as u32;
            let (sx, sy) = (steps(rx), steps(ry));
            // Chromatic Aberration: the widest level redder, the narrowest bluer.
            let last = i + 1 == n || fade == 0.0;
            let fringe = [if last && i > 0 { 1.0 + self.chroma } else { 1.0 }, 1.0, if i == 0 { 1.0 + self.chroma } else { 1.0 }];
            out.push(Dg2Level {
                w: ((f64::from(buf_w) * scale) as u32).max(1),
                h: ((f64::from(buf_h) * scale) as u32).max(1),
                steps: [sx, sy],
                stride: [(rx / f64::from(sx)) as f32, (ry / f64::from(sy)) as f32],
                fade: fringe.map(|f| f * fade),
            });
            if fade == 0.0 {
                break;
            }
            i += 1;
        }
        out
    }
}

/// Weight of blur tap `k` of `steps` (before normalising).
#[inline]
pub fn dg2_weight(k: i32, steps: u32) -> f32 {
    let t = k as f32 / steps.max(1) as f32;
    (-0.5 * t * t / 0.06).exp()
}

/// 1 / the sum of a level's tap weights.
pub fn dg2_inv_sum(steps: u32) -> f32 {
    let s = steps.max(1) as i32;
    1.0 / (-s..=s).map(|k| dg2_weight(k, steps)).sum::<f32>()
}

/// Tint Mode on one channel (Standard multiplies, Overlay, Soft Light), mixed in.
#[inline]
pub fn dg2_tint(g: f32, t: f32, mode: u32, mix: f32) -> f32 {
    let tinted = match mode {
        1 if g <= 0.5 => 2.0 * g * t,
        1 => 1.0 - 2.0 * (1.0 - g) * (1.0 - t),
        2 if t <= 0.5 => g - (1.0 - 2.0 * t) * g * (1.0 - g),
        2 => g + (2.0 * t - 1.0) * ((if g <= 0.25 { ((16.0 * g - 12.0) * g + 4.0) * g } else { g.sqrt() }) - g),
        _ => g * t,
    };
    g + (tinted - g) * mix
}

#[inline]
fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The glow input of one premultiplied pixel in display values: keyed per channel and masked.
#[inline]
pub fn dg2_key(px: Px, threshold: f32, smooth_w: f32, amount: f32) -> [f32; 3] {
    let mut x = [px[0].max(0.0), px[1].max(0.0), px[2].max(0.0)];
    if threshold > 0.0 {
        for v in &mut x {
            *v *= smooth(threshold - smooth_w, threshold + smooth_w, *v);
        }
    }
    x.map(|v| v * amount)
}

/// One finished pixel: the glow `g` (linear, summed pyramid) exposed, tinted, back to display
/// values, the layer pixel `px` on top, Unmult.
#[inline]
pub fn dg2_finish(st: &Dg2Settings, px: Px, g: [f32; 3]) -> Px {
    let inv = 1.0 / st.gamma;
    let mut o = [0.0f32; 4];
    for c in 0..3 {
        let mut v = g[c] * st.exposure;
        if let Some(t) = st.tint {
            v = dg2_tint(v, t.color[c], t.mode, t.mix);
        }
        if st.gamma > 1.0 {
            v = if v > 0.0 { v.powf(inv) } else { 0.0 };
        }
        if st.screen {
            v = v.min(1.0);
        }
        let s = px[c] * st.source_opacity;
        o[c] = if st.screen { 1.0 - (1.0 - v) * (1.0 - s) } else { v + s };
    }
    o[3] = if st.unmult { o[0].max(o[1]).max(o[2]).clamp(0.0, 1.0) } else { 1.0 };
    o
}

// ---------------------------------------------------------------- CPU pyramid

/// `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Rect {
    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
    fn w(&self) -> usize {
        self.x1.saturating_sub(self.x0)
    }
    fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
}

/// A `w × h` RGB image that is zero outside `r` (only `r` is stored).
struct Plane {
    w: usize,
    h: usize,
    r: Rect,
    d: Vec<[f32; 3]>,
}

impl Plane {
    fn new(w: usize, h: usize, r: Rect) -> Plane {
        let r = if r.is_empty() { Rect::default() } else { r };
        Plane { w, h, r, d: vec![[0.0; 3]; r.w() * (r.y1 - r.y0)] }
    }
    /// Row `y` of the stored rectangle (starting at `r.x0`), if stored.
    #[inline]
    fn row(&self, y: usize) -> Option<&[[f32; 3]]> {
        if y < self.r.y0 || y >= self.r.y1 {
            return None;
        }
        let w = self.r.w();
        self.d.get((y - self.r.y0) * w..(y - self.r.y0 + 1) * w)
    }
    #[inline]
    fn at(row: &[[f32; 3]], x0: usize, x: usize) -> [f32; 3] {
        x.checked_sub(x0).and_then(|i| row.get(i)).copied().unwrap_or([0.0; 3])
    }
    /// Rows of the stored rectangle, in parallel, with their y.
    fn rows_mut(&mut self) -> impl IndexedParallelIterator<Item = (usize, &mut [[f32; 3]])> {
        let (w, y0) = (self.r.w().max(1), self.r.y0);
        self.d.par_chunks_mut(w).enumerate().map(move |(i, r)| (y0 + i, r))
    }
}

/// GL_LINEAR clamp-to-edge taps at index-space position `p` of an `n`-texel axis.
#[inline]
fn lin(p: f32, n: usize) -> (usize, usize, f32) {
    let i0 = p.floor();
    let f = p - i0;
    let last = n.saturating_sub(1) as i64;
    let i = i0 as i64;
    (i.clamp(0, last) as usize, (i + 1).clamp(0, last) as usize, f)
}

#[inline]
fn mix3(a: [f32; 3], b: [f32; 3], f: f32) -> [f32; 3] {
    [a[0] * (1.0 - f) + b[0] * f, a[1] * (1.0 - f) + b[1] * f, a[2] * (1.0 - f) + b[2] * f]
}

#[inline]
fn madd(acc: &mut [f32; 3], w: f32, v: [f32; 3]) {
    acc[0] += w * v[0];
    acc[1] += w * v[1];
    acc[2] += w * v[2];
}

/// The destination indices `[lo, hi)` (of `n`) whose samples at `(j + 0.5) · ratio − 0.5 ± reach`
/// can touch source texels `[x0, x1)` (generous by a texel or two).
fn span(x0: usize, x1: usize, ratio: f64, reach: f64, n: usize) -> (usize, usize) {
    if x0 >= x1 {
        return (0, 0);
    }
    let lo = ((x0 as f64 - reach - 1.0 + 0.5) / ratio - 0.5).floor() - 1.0;
    let hi = ((x1 as f64 + reach + 0.5) / ratio - 0.5).ceil() + 2.0;
    (lo.clamp(0.0, n as f64) as usize, hi.clamp(0.0, n as f64) as usize)
}

/// Sample row `row` (texels `x0..`, zero elsewhere, `n` wide) at index-space `p`.
#[inline]
fn sample_row(row: &[[f32; 3]], x0: usize, n: usize, p: f32) -> [f32; 3] {
    let (a, b, f) = lin(p, n);
    mix3(Plane::at(row, x0, a), Plane::at(row, x0, b), f)
}

/// The 13-tap downsample of `src` to `w × h` (offsets in destination texels).
fn down13(src: &Plane, w: usize, h: usize) -> Plane {
    let (rx, ry) = (src.w as f32 / w as f32, src.h as f32 / h as f32);
    let (x0, x1) = span(src.r.x0, src.r.x1, f64::from(rx), 2.0 * f64::from(rx), w);
    let (y0, y1) = span(src.r.y0, src.r.y1, f64::from(ry), 2.0 * f64::from(ry), h);
    let mut out = Plane::new(w, h, Rect { x0, y0, x1, y1 });
    if out.r.is_empty() || src.r.is_empty() {
        return out;
    }
    // Horizontal parts per source row: the ±2 grid (.25 .5 .25) and the ±1 pair (.5 .5).
    let cols = out.r.w();
    let sy0 = src.r.y0;
    let rows = src.r.y1 - sy0;
    let mut h2 = vec![[0.0f32; 3]; rows * cols];
    let mut h1 = vec![[0.0f32; 3]; rows * cols];
    h2.par_chunks_mut(cols.max(1)).zip(h1.par_chunks_mut(cols.max(1))).enumerate().for_each(|(i, (a, b))| {
        let Some(row) = src.row(sy0 + i) else { return };
        for (k, (pa, pb)) in a.iter_mut().zip(b.iter_mut()).enumerate() {
            let c = ((x0 + k) as f32 + 0.5) * rx - 0.5;
            let s = |o: f32| sample_row(row, src.r.x0, src.w, c + o * rx);
            let mut v2 = [0.0; 3];
            madd(&mut v2, 0.25, s(-2.0));
            madd(&mut v2, 0.5, s(0.0));
            madd(&mut v2, 0.25, s(2.0));
            let mut v1 = [0.0; 3];
            madd(&mut v1, 0.5, s(-1.0));
            madd(&mut v1, 0.5, s(1.0));
            *pa = v2;
            *pb = v1;
        }
    });
    let get = |m: &Vec<[f32; 3]>, sy: usize, k: usize| -> [f32; 3] {
        if sy < sy0 || sy >= sy0 + rows { [0.0; 3] } else { m.get((sy - sy0) * cols + k).copied().unwrap_or([0.0; 3]) }
    };
    let sh = src.h;
    out.rows_mut().for_each(|(y, orow)| {
        let c = (y as f32 + 0.5) * ry - 0.5;
        let taps = |o: f32| lin(c + o * ry, sh);
        let t2 = [(-2.0, 0.25), (0.0, 0.5), (2.0, 0.25)].map(|(o, wt)| (taps(o), wt));
        let t1 = [(-1.0, 0.5), (1.0, 0.5)].map(|(o, wt)| (taps(o), wt));
        for (k, px) in orow.iter_mut().enumerate() {
            let mut v2 = [0.0; 3];
            for &((a, b, f), wt) in &t2 {
                madd(&mut v2, wt, mix3(get(&h2, a, k), get(&h2, b, k), f));
            }
            let mut v1 = [0.0; 3];
            for &((a, b, f), wt) in &t1 {
                madd(&mut v1, wt, mix3(get(&h1, a, k), get(&h1, b, k), f));
            }
            *px = [0.5 * v2[0] + 0.5 * v1[0], 0.5 * v2[1] + 0.5 * v1[1], 0.5 * v2[2] + 0.5 * v1[2]];
        }
    });
    out
}

/// One axis of a level's Gaussian as integer taps: the 2·steps + 1 linear taps `stride` apart
/// all have the same fractional offset at every pixel, so they fold into one kernel `(first
/// offset, weights)` (the same sums as the GPU's per-tap linear samples, up to float rounding).
fn folded_kernel(steps: u32, stride: f32) -> (i64, Vec<f32>) {
    let inv = dg2_inv_sum(steps);
    let s = steps.max(1) as i32;
    let taps: Vec<(i64, f32, f32)> = (-s..=s)
        .map(|k| {
            let off = k as f32 * stride;
            let i = off.floor();
            (i as i64, off - i, dg2_weight(k, steps) * inv)
        })
        .collect();
    let lo = taps.iter().map(|t| t.0).min().unwrap_or(0);
    let hi = taps.iter().map(|t| t.0 + 1).max().unwrap_or(0);
    let mut c = vec![0.0f32; (hi - lo + 1).max(1) as usize];
    for (i, f, w) in taps {
        let j = (i - lo) as usize;
        if let Some(v) = c.get_mut(j) {
            *v += w * (1.0 - f);
        }
        if let Some(v) = c.get_mut(j + 1) {
            *v += w * f;
        }
    }
    (lo, c)
}

/// One level's separable Gaussian (the fade on the horizontal pass).
fn blur(src: &Plane, lv: &Dg2Level) -> Plane {
    let [sx, sy] = lv.steps;
    let [dx, dy] = lv.stride;
    let (kx0, kx) = folded_kernel(sx, dx);
    let (ky0, ky) = folded_kernel(sy, dy);
    let reach_x = kx0.unsigned_abs().max((kx0 + kx.len() as i64).unsigned_abs()) as f64;
    let (x0, x1) = span(src.r.x0, src.r.x1, 1.0, reach_x, src.w);
    let mut tmp = Plane::new(src.w, src.h, Rect { x0, y0: src.r.y0, x1, y1: src.r.y1 });
    let fade = lv.fade;
    let (w, sx0) = (src.w as i64, src.r.x0);
    tmp.rows_mut().for_each(|(y, orow)| {
        let Some(row) = src.row(y) else { return };
        // The source row over [x0 + kx0, x1 + kx0 + len), clamped to the image's edges.
        let a = x0 as i64 + kx0;
        let ext: Vec<[f32; 3]> = (a..x1 as i64 + kx0 + kx.len() as i64).map(|i| Plane::at(row, sx0, i.clamp(0, w - 1) as usize)).collect();
        for (i, px) in orow.iter_mut().enumerate() {
            let mut acc = [0.0; 3];
            for (c, v) in kx.iter().zip(&ext[i..]) {
                madd(&mut acc, *c, *v);
            }
            *px = [acc[0] * fade[0], acc[1] * fade[1], acc[2] * fade[2]];
        }
    });
    let reach_y = ky0.unsigned_abs().max((ky0 + ky.len() as i64).unsigned_abs()) as f64;
    let (y0, y1) = span(src.r.y0, src.r.y1, 1.0, reach_y, src.h);
    let mut out = Plane::new(src.w, src.h, Rect { x0, y0, x1, y1 });
    let h = src.h as i64;
    out.rows_mut().for_each(|(y, orow)| {
        for (j, &c) in ky.iter().enumerate() {
            let sy = (y as i64 + ky0 + j as i64).clamp(0, h - 1) as usize;
            let Some(r) = tmp.row(sy) else { continue };
            for (px, v) in orow.iter_mut().zip(r) {
                madd(px, c, *v);
            }
        }
    });
    out
}

/// `dst` += the 3 × 3 tent upsample of `src` (added, or screened), over the union of both.
fn up_into(src: &Plane, dst: Plane, screen: bool) -> Plane {
    let (rx, ry) = (src.w as f32 / dst.w as f32, src.h as f32 / dst.h as f32);
    let (x0, x1) = span(src.r.x0, src.r.x1, f64::from(rx), f64::from(rx), dst.w);
    let (y0, y1) = span(src.r.y0, src.r.y1, f64::from(ry), f64::from(ry), dst.h);
    let ur = Rect { x0, y0, x1, y1 };
    if ur.is_empty() || src.r.is_empty() {
        return dst;
    }
    let r = ur.union(dst.r);
    // Horizontal tent per source row, on the result's columns.
    let cols = r.w();
    let sy0 = src.r.y0;
    let rows = src.r.y1 - sy0;
    let mut ht = vec![[0.0f32; 3]; rows * cols];
    ht.par_chunks_mut(cols.max(1)).enumerate().for_each(|(i, a)| {
        let Some(row) = src.row(sy0 + i) else { return };
        for (k, p) in a.iter_mut().enumerate() {
            let c = ((r.x0 + k) as f32 + 0.5) * rx - 0.5;
            let mut v = [0.0; 3];
            for (o, wt) in [(-1.0, 0.25), (0.0, 0.5), (1.0, 0.25)] {
                madd(&mut v, wt, sample_row(row, src.r.x0, src.w, c + o * rx));
            }
            *p = v;
        }
    });
    let mut out = Plane::new(dst.w, dst.h, r);
    let sh = src.h;
    out.rows_mut().for_each(|(y, orow)| {
        let c = (y as f32 + 0.5) * ry - 0.5;
        let drow = dst.row(y);
        let taps = [(-1.0, 0.25), (0.0, 0.5), (1.0, 0.25)].map(|(o, wt)| (lin(c + o * ry, sh), wt));
        for (k, px) in orow.iter_mut().enumerate() {
            let mut u = [0.0; 3];
            for &((a, b, f), wt) in &taps {
                let g = |sy: usize| if sy < sy0 || sy >= sy0 + rows { [0.0; 3] } else { ht.get((sy - sy0) * cols + k).copied().unwrap_or([0.0; 3]) };
                madd(&mut u, wt, mix3(g(a), g(b), f));
            }
            let d = drow.map(|row| Plane::at(row, dst.r.x0, r.x0 + k)).unwrap_or([0.0; 3]);
            *px = if screen {
                [u[0] + d[0] * (1.0 - u[0]), u[1] + d[1] * (1.0 - u[1]), u[2] + d[2] * (1.0 - u[2])]
            } else {
                [u[0] + d[0], u[1] + d[1], u[2] + d[2]]
            };
        }
    });
    out
}

/// Deep Glow 2 on the CPU (see the module docs).
pub(crate) fn deep_glow2(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let Some(st) = Dg2Settings::new(ctx, b.scale) else { return b };
    let pad = st.pad(ctx, b.img.width, b.img.height);
    b.pad(pad);
    let (fw, fh) = (b.img.width as usize, b.img.height as usize);
    if fw == 0 || fh == 0 {
        return b;
    }
    let mask = ctx.layer_param("PEDG-0056", true).map(|layer| crate::util::fit_layer(ctx, &b, &layer, false));
    let luma = ctx.params.e("PEDG-0057") == 1;
    let invert = ctx.params.b("PEDG-0059");
    let amount = |x: usize, y: usize| -> f32 { mask.as_ref().map_or(1.0, |m| crate::misc::deep_glow_mask(m.get(x as i64, y as i64), luma, invert)) };
    if st.view_input {
        b.img.rows_mut().for_each(|(y, row)| {
            for (x, px) in row.iter_mut().enumerate() {
                let a = amount(x, y);
                let k = dg2_key(*px, st.threshold, st.smooth, a);
                *px = [k[0], k[1], k[2], (px[3] * a).clamp(0.0, 1.0)];
            }
        });
        return b;
    }
    // The glow input, kept to the rectangle of non-zero pixels.
    let nz: Vec<Option<(usize, usize)>> = b
        .img
        .data
        .par_chunks(fw)
        .map(|row| {
            let x0 = row.iter().position(|p| p[0] > 0.0 || p[1] > 0.0 || p[2] > 0.0)?;
            let x1 = row.iter().rposition(|p| p[0] > 0.0 || p[1] > 0.0 || p[2] > 0.0)?;
            Some((x0, x1 + 1))
        })
        .collect();
    let rect = nz.iter().enumerate().filter_map(|(y, r)| r.map(|(x0, x1)| Rect { x0, y0: y, x1, y1: y + 1 })).fold(Rect::default(), Rect::union);
    let mut input = Plane::new(fw, fh, rect);
    {
        let src = &b.img;
        input.rows_mut().for_each(|(y, orow)| {
            for (i, px) in orow.iter_mut().enumerate() {
                let x = rect.x0 + i;
                let mut k = dg2_key(src.data[y * fw + x], st.threshold, st.smooth, amount(x, y));
                for v in &mut k {
                    if st.screen {
                        *v = v.min(1.0);
                    }
                    if st.gamma > 1.0 {
                        *v = if *v > 0.0 { v.powf(st.gamma) } else { 0.0 };
                    }
                }
                *px = k;
            }
        });
    }
    drop(mask);
    let levels = st.levels(fw as u32, fh as u32);
    let mut blurred: Vec<Plane> = Vec::with_capacity(levels.len());
    let mut prev = input;
    for lv in &levels {
        let a = down13(&prev, lv.w as usize, lv.h as usize);
        blurred.push(blur(&a, lv));
        prev = a;
    }
    drop(prev);
    let mut acc = blurred.pop();
    while let Some(fine) = blurred.pop() {
        acc = Some(match acc {
            Some(coarse) => up_into(&coarse, fine, st.screen),
            None => fine,
        });
    }
    let Some(c0) = acc else { return b };
    // The full-size glow's rectangle, then the finish over the frame.
    let (rx, ry) = (c0.w as f32 / fw as f32, c0.h as f32 / fh as f32);
    let (gx0, gx1) = span(c0.r.x0, c0.r.x1, f64::from(rx), 0.0, fw);
    let (gy0, gy1) = span(c0.r.y0, c0.r.y1, f64::from(ry), 0.0, fh);
    let keep_empty = st.unmult;
    b.img.rows_mut().for_each(|(y, row)| {
        let in_glow = !c0.r.is_empty() && y >= gy0 && y < gy1;
        let (ta, tb, tf) = lin((y as f32 + 0.5) * ry - 0.5, c0.h);
        let (ra, rb) = (c0.row(ta), c0.row(tb));
        for (x, px) in row.iter_mut().enumerate() {
            let g = if in_glow && x >= gx0 && x < gx1 {
                let p = (x as f32 + 0.5) * rx - 0.5;
                let va = ra.map_or([0.0; 3], |r| sample_row(r, c0.r.x0, c0.w, p));
                let vb = rb.map_or([0.0; 3], |r| sample_row(r, c0.r.x0, c0.w, p));
                mix3(va, vb, tf)
            } else {
                [0.0; 3]
            };
            if keep_empty && g == [0.0; 3] && *px == [0.0; 4] {
                continue;
            }
            *px = dg2_finish(&st, *px, g);
        }
    });
    b
}

#[cfg(test)]
mod tests {
    use effectcraft_keyframe::Value;
    use effectcraft_raster::Image;

    use super::*;
    use crate::{EffectEnv, num, run_fx};

    fn settings(vals: &[(&str, Value)]) -> Dg2Settings {
        let spec = crate::find("ec.stylize.deepglow").unwrap();
        let mut params = crate::Params { values: spec.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, [100.0, 100.0]))).collect() };
        for (k, v) in vals {
            assert!(params.values.contains_key(*k), "unknown param {k}");
            params.values.insert(k.to_string(), v.clone());
        }
        let ctx = EffectCtx { params: &params, time: 0.0, layer_size: [100.0, 100.0], seed: 1, adjustment: false, env: EffectEnv::default() };
        Dg2Settings::new(&ctx, 1.0).unwrap()
    }

    /// Medium preset: Radius 400 has 9 levels from σ 0.19 to 49 px (half-widths R/512 … R/2),
    /// at 0.9^(i+1) of the buffer, all at full weight; Radius 50 dims its first levels.
    #[test]
    fn levels_follow_deep_glow_2() {
        let st = settings(&[("PEDG-0001", num(400.0))]);
        let lv = st.levels(2000, 1000);
        assert_eq!(lv.len(), 9);
        for (i, l) in lv.iter().enumerate() {
            let scale = 0.9f64.powi(i as i32 + 1);
            assert_eq!(l.w, (2000.0 * scale) as u32);
            let half = f64::from(l.steps[0]) * f64::from(l.stride[0]) / scale;
            assert!((half - 400.0 / 2f64.powi(9 - i as i32)).abs() < 1e-3, "{i}: {half}");
            assert_eq!(l.fade, [1.0; 3]);
        }
        let last = lv.last().unwrap();
        assert_eq!(last.steps[0], (0.33 * 200.0 * 0.9f64.powi(9)) as u32);
        let small = settings(&[("PEDG-0001", num(50.0))]).levels(500, 500);
        assert_eq!(small.len(), 6);
        let fades: Vec<f32> = small.iter().map(|l| l.fade[0]).collect();
        for (f, want) in fades.iter().zip([1.0, 1.0, 1.0, 45.0 / 55.0, 40.0 / 70.0, 30.0 / 100.0]) {
            assert!((f - want).abs() < 1e-6, "{fades:?}");
        }
        // Custom: every level at Resolution; Extreme: full size.
        let custom = settings(&[("PEDG-0001", num(100.0)), ("PEDG2-0048", Value::Enum(6)), ("PEDG2-0049", num(50.0))]).levels(400, 400);
        assert!(custom.iter().all(|l| l.w == 200));
        let extreme = settings(&[("PEDG-0001", num(100.0)), ("PEDG2-0048", Value::Enum(5))]).levels(400, 400);
        assert!(extreme.iter().all(|l| l.w == 400));
    }

    fn dot(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h);
        for y in h / 2 - 2..h / 2 + 2 {
            for x in w / 2 - 2..w / 2 + 2 {
                img.set(x, y, [1.0; 4]);
            }
        }
        img
    }

    fn glow(vals: &[(&str, Value)], img: Image) -> Buf {
        run_fx("ec.stylize.deepglow", vals, img, 0.0, EffectEnv::default())
    }

    /// The halo is round (no square box-blur support) and ends at R/2.
    #[test]
    fn halo_is_round_and_ends_at_half_the_radius() {
        let o = glow(&[("PEDG-0001", num(200.0)), ("glowExposure", num(1.0))], dot(240, 240));
        let at = |dx: i64, dy: i64| o.img.get(120 + dx + o.offset[0] as i64, 120 + dy + o.offset[1] as i64)[0];
        for d in [20.0f64, 40.0, 60.0] {
            let axis = at(d as i64, 0);
            let diag = at((d / 2f64.sqrt()).round() as i64, (d / 2f64.sqrt()).round() as i64);
            assert!(axis > 0.01 && (diag / axis - 1.0).abs() < 0.08, "{d} px: axis {axis} diagonal {diag}");
        }
        assert!(at(80, 0) > 0.0, "reaches 0.4 R");
        // Past R/2 only the down/upsamples' faint spread is left (well under an 8-bit level).
        assert!(at(120, 0) < 0.5 / 255.0, "{}", at(120, 0));
        assert!(at(90, 90) < 0.5 / 255.0, "no square corner: {}", at(90, 90));
    }

    /// After Effects 2026, Deep Glow 2 defaults, Radius 400 / Exposure 0.08 (Elias's settings), on
    /// a white edge (the left half of a comp-sized layer), 32 bpc: the values d px right of the
    /// edge on its middle row. Before this model EC was 0.10 off (a wider core, and a cliff at
    /// ~145 px where AE tapers to ~200).
    #[test]
    fn matches_after_effects_on_a_white_edge() {
        let mut img = Image::new(1000, 600);
        for y in 0..600 {
            for x in 0..500 {
                img.set(x, y, [1.0; 4]);
            }
        }
        let o = glow(&[("PEDG-0001", num(400.0)), ("glowExposure", num(0.08))], img);
        let ae = [(1, 0.3477), (8, 0.3198), (32, 0.2249), (64, 0.1368), (128, 0.0387), (180, 0.0085), (220, 0.0003)];
        for (d, want) in ae {
            let v = o.img.get(499 + d + o.offset[0] as i64, 300 + o.offset[1] as i64)[0];
            assert!((v - want).abs() < 0.004, "{d} px: {v} vs AE {want}");
        }
    }

    /// Exposure is a plain linear gain on the glow in linear light (gamma 2.4).
    #[test]
    fn exposure_is_linear() {
        let run = |e: f64| glow(&[("PEDG-0001", num(100.0)), ("glowExposure", num(e)), ("PEDG-0024", num(0.0))], dot(120, 120));
        let (a, b) = (run(0.1), run(0.2));
        let px = |o: &Buf| o.img.get(60 + 20 + o.offset[0] as i64, 60 + o.offset[1] as i64)[0];
        let ratio = (px(&b) / px(&a)).powf(2.4);
        assert!((ratio - 2.0).abs() < 1e-3, "{ratio}");
    }

    /// Threshold keys each channel on its own (a dim channel drops out, a bright one stays).
    #[test]
    fn threshold_is_per_channel() {
        let mut img = Image::new(60, 60);
        for y in 28..32 {
            for x in 28..32 {
                img.set(x, y, [1.0, 0.2, 0.0, 1.0]);
            }
        }
        let o = glow(&[("PEDG-0001", num(40.0)), ("glowExposure", num(1.0)), ("PEDG-0004", num(50.0)), ("PEDG-0024", num(0.0))], img);
        let px = o.img.get(36 + o.offset[0] as i64, 30 + o.offset[1] as i64);
        assert!(px[0] > 0.05 && px[1] == 0.0, "{px:?}");
    }

    /// Unmult: alpha = the brightest channel; a pixel the glow does not reach stays clear.
    #[test]
    fn unmult_alpha_is_the_brightest_channel() {
        let o = glow(&[("PEDG-0001", num(60.0)), ("glowExposure", num(0.5))], dot(100, 100));
        for p in &o.img.data {
            assert!((p[3] - p[0].max(p[1]).max(p[2]).min(1.0)).abs() < 1e-6, "{p:?}");
        }
        assert_eq!(o.img.data[0], [0.0; 4]);
        let opaque = glow(&[("PEDG-0001", num(60.0)), ("PEDG-0025", Value::Bool(false))], dot(100, 100));
        assert_eq!(opaque.img.data[0][3], 1.0);
    }

    /// The Legacy model is still there and differs.
    #[test]
    fn legacy_model_is_kept() {
        let new = glow(&[("PEDG-0001", num(60.0))], dot(100, 100));
        let old = glow(&[("PEDG-0001", num(60.0)), ("glowModel", Value::Enum(1))], dot(100, 100));
        assert_ne!(new.offset, old.offset, "Legacy pads by Radius, Deep Glow 2 by its reach");
    }

    /// The rectangle-restricted CPU pyramid equals the plain whole-image one: an empty layer
    /// stays empty, and content at the frame edge (no expansion) clamps like the GPU.
    #[test]
    fn empty_and_edge_layers() {
        let o = glow(&[("PEDG-0001", num(80.0))], Image::new(50, 40));
        assert!(o.img.data.iter().all(|p| *p == [0.0; 4]));
        let mut edge = Image::new(80, 60);
        for y in 0..60 {
            for x in 0..6 {
                edge.set(x, y, [0.9, 0.9, 0.9, 1.0]);
            }
        }
        let o = glow(&[("PEDG-0001", num(80.0)), ("PEDG-0055", num(0.0)), ("glowExposure", num(1.0))], edge);
        assert_eq!(o.offset, [0.0, 0.0]);
        assert!(o.img.get(20, 30)[0] > 0.1 && o.img.get(20, 30)[0] < 1.0);
    }
}

/// CPU timing of the pyramid's stages and of both models (release:
/// `cargo test -p effectcraft-effects --lib deep_glow_2_bench -- --ignored --nocapture`).
#[cfg(test)]
mod bench {
    use effectcraft_keyframe::Value;
    use effectcraft_raster::Image;

    use super::*;
    use crate::{EffectEnv, num};

    fn text(w: u32, h: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> Image {
        let mut img = Image::new(w, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let (gx, gy) = ((x - x0) % 96, (y - y0) % 140);
                if gx < 70 && gy < 110 && !(20..50).contains(&gx) || gy < 20 {
                    img.set(x, y, [1.0; 4]);
                }
            }
        }
        img
    }

    fn ms(f: impl Fn()) -> f64 {
        let mut v: Vec<f64> = (0..9)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        v.sort_by(f64::total_cmp);
        v[4]
    }

    #[test]
    #[ignore = "bench"]
    fn deep_glow_2_bench() {
        for (label, img, r) in [
            ("precomp 1920x1080 text R400", text(1920, 1080, 620, 446, 1296, 711), 400.0),
            ("text 1434x394 R282", text(1434, 394, 0, 0, 1434, 394), 282.0),
            ("full-frame 1920x1080 R150", text(1920, 1080, 240, 300, 1680, 780), 150.0),
        ] {
            for scale in [1.0, 0.5] {
                let img = if scale < 1.0 { effectcraft_raster::resample(&img, img.width / 2, img.height / 2) } else { img.clone() };
                let run = |model: u32| {
                    let vals = [("glowModel", Value::Enum(model)), ("PEDG-0001", num(r)), ("glowExposure", num(0.08))];
                    let spec = crate::find("ec.stylize.deepglow").unwrap();
                    let size = [img.width as f64 / scale, img.height as f64 / scale];
                    let mut params = crate::Params { values: spec.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, size))).collect() };
                    for (k, v) in vals {
                        params.values.insert(k.to_string(), v);
                    }
                    let ctx = EffectCtx { params: &params, time: 0.0, layer_size: size, seed: 1, adjustment: false, env: EffectEnv::default() };
                    std::hint::black_box((spec.render)(&ctx, Buf { img: img.clone(), offset: [0.0; 2], scale }));
                };
                let (old, new) = (ms(|| run(1)), ms(|| run(0)));
                println!("BENCH {label} scale {scale}: legacy {old:.1} ms, deep glow 2 {new:.1} ms");
            }
        }
        // Stages on the first case.
        let img = text(1920, 1080, 620, 446, 1296, 711);
        let pad = 253usize;
        let (fw, fh) = (img.width as usize + 2 * pad, img.height as usize + 2 * pad);
        let mut input = Plane::new(fw, fh, Rect { x0: 620 + pad, y0: 446 + pad, x1: 1296 + pad, y1: 711 + pad });
        input.rows_mut().for_each(|(_, r)| r.iter_mut().for_each(|p| *p = [0.5; 3]));
        let spec = crate::find("ec.stylize.deepglow").unwrap();
        let mut params = crate::Params { values: spec.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, [1920.0, 1080.0]))).collect() };
        params.values.insert("PEDG-0001".into(), num(400.0));
        let ctx = EffectCtx { params: &params, time: 0.0, layer_size: [1920.0, 1080.0], seed: 1, adjustment: false, env: EffectEnv::default() };
        let st = Dg2Settings::new(&ctx, 1.0).unwrap();
        let levels = st.levels(fw as u32, fh as u32);
        let mut a = vec![];
        let mut prev = input;
        let mut t_down = 0.0;
        for lv in &levels {
            let t = std::time::Instant::now();
            let d = down13(&prev, lv.w as usize, lv.h as usize);
            t_down += t.elapsed().as_secs_f64() * 1e3;
            a.push(d);
            prev = Plane { w: a.last().unwrap().w, h: a.last().unwrap().h, r: a.last().unwrap().r, d: a.last().unwrap().d.clone() };
        }
        let t = std::time::Instant::now();
        let c: Vec<Plane> = a.iter().zip(&levels).map(|(p, lv)| blur(p, lv)).collect();
        let t_blur = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let mut c = c;
        let mut acc = c.pop().unwrap();
        while let Some(f) = c.pop() {
            acc = up_into(&acc, f, true);
        }
        let t_up = t.elapsed().as_secs_f64() * 1e3;
        let per: Vec<String> =
            a.iter().zip(&levels).map(|(p, lv)| format!("{}x{} rect {}x{} steps {}", p.w, p.h, p.r.w(), p.r.y1 - p.r.y0, lv.steps[0])).collect();
        println!("STAGES down {t_down:.1} ms, blur {t_blur:.1} ms, up {t_up:.1} ms; c0 rect {}x{}; levels {per:?}", acc.r.w(), acc.r.y1 - acc.r.y0);
    }
}
