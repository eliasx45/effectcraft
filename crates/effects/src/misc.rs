//! Blur & Sharpen, Stylize, Perspective, Channel, Noise and Transition effects.

use effectcraft_color::luminance;
use effectcraft_keyframe::Value;
use effectcraft_project::ParamUi;
#[cfg(test)]
use effectcraft_raster::resample;
use effectcraft_raster::{Image, Px, Sparse, gaussian_blur, gaussian_blur_sparse, hash_noise, resample_sparse};
use rayon::prelude::*;

use crate::{Buf, EffectCtx, EffectSpec, col, num, p, popup, slider};

fn spec(id: &'static str, name: &'static str, category: &'static str, params: Vec<crate::ParamSpec>, render: crate::RenderFn) -> EffectSpec {
    EffectSpec { id, name, category, params, render, gpu: false, float: true }
}

fn dims_xy(dim: u32) -> (f64, f64) {
    match dim {
        1 => (1.0, 0.0),
        2 => (0.0, 1.0),
        _ => (1.0, 1.0),
    }
}

/// Gaussian Blur: Blurriness in comp pixels for a layer whose effects run in comp space
/// ([`crate::EffectEnv::comp_px`]), else in layer pixels.
fn gaussian(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let s = crate::gaussian_sigma(ctx.params.f("blurriness")) * b.scale;
    if s <= 0.0 {
        return b;
    }
    let (kx, ky) = dims_xy(ctx.params.e("dimensions"));
    let [cx, cy] = ctx.env.comp_px();
    let (sx, sy) = (s * cx * kx, s * cy * ky);
    let repeat = ctx.params.b("repeatEdge");
    if !repeat && !ctx.adjustment {
        b.pad((sx.max(sy) * 3.0).ceil().min(1.0e5) as u32);
    }
    b.img = gaussian_blur(&b.img, sx, sy, repeat || ctx.adjustment);
    b
}

/// Fast Box Blur: Blur Radius in comp pixels for a layer whose effects run in comp space
/// ([`crate::EffectEnv::comp_px`]), else in layer pixels.
fn box_blur(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let r = ctx.params.f("radius").max(0.0) * b.scale;
    let (kx, ky) = dims_xy(ctx.params.e("dimensions"));
    let [cx, cy] = ctx.env.comp_px();
    let rx = (r * cx * kx).round().min(1.0e5) as usize;
    let ry = (r * cy * ky).round().min(1.0e5) as usize;
    if rx == 0 && ry == 0 {
        return b;
    }
    let it = ctx.params.f("iterations").round().clamp(1.0, 50.0) as usize;
    let repeat = ctx.params.b("repeatEdge");
    if !repeat && !ctx.adjustment {
        b.pad((rx.max(ry) * it) as u32 + 1);
    }
    b.img = effectcraft_raster::box_blur(&b.img, rx, ry, it, repeat || ctx.adjustment);
    b
}

fn directional(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let len = ctx.params.f("length") * b.scale;
    if len < 0.5 {
        return b;
    }
    if !ctx.adjustment {
        b.pad(len.ceil() as u32 + 1);
    }
    // On an adjustment layer the frame edge repeats, as After Effects keeps it opaque (measured:
    // a full-frame black solid under a 108 px Directional Blur adjustment stays black to the edge).
    b.img = effectcraft_raster::directional_blur(&b.img, ctx.params.f("direction"), len, ctx.adjustment);
    b
}

fn radial(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let amt = ctx.params.f("amount");
    let zoom = ctx.params.e("type") == 1;
    let c = b.to_px(ctx.params.v2("center"));
    b.img = effectcraft_raster::radial_blur(&b.img, c, if zoom { amt / 100.0 } else { amt }, zoom);
    b
}

fn unsharp_core(img: &Image, amount: f32, sigma: f64, threshold: f32) -> Image {
    let blurred = gaussian_blur(img, sigma, sigma, true);
    let mut out = img.clone();
    out.data.par_iter_mut().zip(blurred.data.par_iter()).for_each(|(o, bl)| {
        for c in 0..3 {
            let d = o[c] - bl[c];
            if d.abs() * 255.0 >= threshold {
                o[c] = (o[c] + d * amount).max(0.0);
            }
        }
    });
    out
}

fn sharpen(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let a = ctx.params.f("amount") as f32 / 50.0;
    b.img = unsharp_core(&b.img, a, 1.0 * b.scale, 0.0);
    b
}

fn unsharp(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let a = ctx.params.f("amount") as f32 / 100.0;
    b.img = unsharp_core(&b.img, a, (ctx.params.f("radius") * b.scale).max(0.1), ctx.params.f("threshold") as f32);
    b
}

/// Position (0 = Color A, 1 = Color B) in Glow's A & B gradient for brightness `l`: `loops`
/// cycles starting at `phase` (revolutions), shaped by Color Looping (0 Sawtooth A>B,
/// 1 Sawtooth B>A, 2 Triangle A>B>A, 3 Triangle B>A>B) and biased so `mid` maps to the middle.
pub fn glow_ab_t(l: f32, looping: u32, loops: f32, phase: f32, mid: f32) -> f32 {
    let u = l.clamp(0.0, 1.0) * loops.max(0.0) + phase;
    let mut t = u - u.floor();
    if u > 0.0 && t <= 0.0 {
        t = 1.0;
    }
    let tri = 1.0 - (2.0 * t - 1.0).abs();
    let t = match looping {
        0 => t,
        1 => 1.0 - t,
        3 => 1.0 - tri,
        _ => tri,
    };
    let m = mid.clamp(0.01, 0.99);
    t.max(0.0).powf(0.5f32.ln() / m.ln())
}

/// Glow's Composite Original (0 On Top, 1 Behind, 2 None) as the composite it draws: 0 the glow
/// over the original with the Glow Operation, 1 the glow only behind it. The option names where
/// the original goes; None draws like Behind (aerender, SceneYard UI Envelope: Behind brightens
/// the card itself, On Top leaves it as it was with a halo around it, None matches Behind to
/// the pixel).
pub fn glow_composite(operation: u32) -> u32 {
    match operation {
        0 => 1,
        _ => 0,
    }
}

/// Glow's Gaussian sigma per pixel of Glow Radius (aerender, SceneYard UI Envelope card at
/// Radius 10, 100 and 300: the halo is a normalised Gaussian of the bright pass with sigma =
/// radius / 2.75, rms under 0.004).
pub const GLOW_SIGMA_PER_RADIUS: f64 = 1.0 / 2.75;

/// Width of Glow Threshold's soft knee: the bright pass is `(v - threshold) / GLOW_KNEE + 0.5`
/// clamped, nearly a step (aerender, SceneYard UI Envelope card: a red channel of 178 glows
/// fully at thresholds 0 to 153, 0.84 / 0.62 / 0.44 at 166 / 173 / 179; a blue channel of 28
/// glows 0.82 / 0.58 / 0.33 at thresholds 20 / 28 / 36).
pub const GLOW_KNEE: f32 = 0.12;

/// Glow's bright pass for value `v` (a colour channel, the luminance or the alpha) at
/// `threshold` (0..1), see [`GLOW_KNEE`]. Values near black fade out over the knee's width, so
/// black never glows (aerender, SceneYard UI Chatbot: black shapes at a threshold of 2.7% and
/// Intensity 5.4 leave no halo).
pub fn glow_bright(v: f32, threshold: f32) -> f32 {
    ((v - threshold) / GLOW_KNEE + 0.5).clamp(0.0, 1.0) * (v / GLOW_KNEE).clamp(0.0, 1.0)
}

/// Position (0 = Color A, 1 = Color B) of Glow's A & B gradient for the blurred bright pass
/// `b`: Color A is the bright end (aerender, SceneYard UI Envelope with red A and blue B: Sawtooth
/// A>B is red where the glow is strong and blue where it fades, Triangle A>B>A is red at half
/// strength and blue at both ends).
pub fn glow_ab_position(b: f32, looping: u32, loops: f32, phase: f32, mid: f32) -> f32 {
    1.0 - glow_ab_t(b, looping, loops, phase, mid)
}

/// Glow Operation options (blend modes the glow is applied with).
pub const GLOW_OPERATIONS: [&str; 17] = [
    "Normal",
    "Add",
    "Multiply",
    "Screen",
    "Overlay",
    "Soft Light",
    "Hard Light",
    "Color Dodge",
    "Color Burn",
    "Darken",
    "Lighten",
    "Difference",
    "Exclusion",
    "Hue",
    "Saturation",
    "Color",
    "Luminosity",
];

/// Glow's Glow Operation as a blend mode (Add for projects saved before it existed).
pub fn glow_operation(ctx: &EffectCtx) -> effectcraft_color::BlendMode {
    let name = GLOW_OPERATIONS.get(ctx.params.get("glowOperation").map(|v| v.as_enum()).unwrap_or(1) as usize).copied().unwrap_or("Add");
    effectcraft_color::BlendMode::ALL.iter().copied().find(|m| m.label() == name).unwrap_or(effectcraft_color::BlendMode::Add)
}

/// Glow's Arbitrary Map: red, green and blue curves of the glow brightness, as Curves point
/// lists `x,y …` separated by `|` (an empty or missing curve is the identity).
fn glow_map(s: &str) -> [Option<crate::Curve>; 3] {
    let mut parts = s.split('|');
    [(); 3].map(|_| parts.next().and_then(crate::Curve::parse))
}

/// Glow's blur σ along x and y (buffer pixels) for a radius in buffer pixels and the layer units
/// per comp pixel `cx`, `cy` (`EffectEnv::comp_px`): never under half a pixel.
pub fn glow_sigmas(radius: f64, cx: f64, cy: f64) -> (f64, f64) {
    let s = radius * GLOW_SIGMA_PER_RADIUS;
    ((s * cx).max(0.5), (s * cy).max(0.5))
}

fn glow(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let thr = ctx.params.f("threshold") as f32 / 100.0;
    let radius = ctx.params.f("radius") * b.scale;
    let intensity = ctx.params.f("intensity") as f32;
    let alpha_based = ctx.params.e("based") == 0;
    let use_colors = ctx.params.e("colors") == 1;
    let arbitrary = (ctx.params.e("colors") == 2).then(|| glow_map(ctx.params.s("arbitraryMap")));
    let glow_op = glow_operation(ctx);
    let looping = ctx.params.e("colorLooping");
    let loops = ctx.params.f("colorLoops") as f32;
    let phase = (ctx.params.f("colorPhase") / 360.0) as f32;
    let mid = ctx.params.f("abMidpoint") as f32 / 100.0;
    let ca = ctx.params.color("colorA");
    let cb = ctx.params.color("colorB");
    let (kx, ky) = dims_xy(ctx.params.e("glowDimensions"));
    // Glow Radius in comp pixels on a layer whose effects size things there (see `comp_px`).
    let [cx, cy] = ctx.env.comp_px();
    let (sx, sy) = glow_sigmas(radius, cx, cy);
    if !ctx.adjustment {
        b.pad((sx.max(sy) * 3.0).ceil() as u32 + 2);
    }
    // Bright pass: a soft step at the threshold (see `GLOW_KNEE`). A & B Colors and the Arbitrary
    // Map glow by one value per pixel, the luminance or the alpha, coloured after the blur;
    // Original Colors with Color Channels steps each channel on its own (aerender, SceneYard UI
    // Envelope: a red card glows pure red at 60%; at 41.6% with A & B Colors only its yellow and
    // white parts glow, whose luminance is over the threshold).
    let scalar = use_colors || arbitrary.is_some();
    let mut bright = b.img.clone();
    bright.data.par_iter_mut().for_each(|px| {
        let a = px[3];
        if a <= 0.0 {
            *px = [0.0; 4];
            return;
        }
        if scalar {
            let l = if alpha_based { a } else { luminance(px[0] / a, px[1] / a, px[2] / a) };
            *px = [glow_bright(l, thr) * a; 4];
        } else if alpha_based {
            *px = px.map(|v| v * glow_bright(a, thr));
        } else {
            let [r, g, bl] = [px[0], px[1], px[2]].map(|v| glow_bright(v / a, thr));
            *px = [r * a, g * a, bl * a, r.max(g).max(bl) * a];
        }
    });
    let mut blurred = gaussian_blur(&bright, sx * kx, sy * ky, false);
    if scalar {
        // The colour follows the blurred glow's strength, so A & B Colors run across the falloff.
        blurred.data.par_iter_mut().for_each(|px| {
            let v = px[3].clamp(0.0, 1.0);
            let c = if let Some(map) = &arbitrary {
                let t = glow_ab_t(v, looping, loops, phase, mid);
                map.each_ref().map(|cv| cv.as_ref().map_or(t, |cv| cv.eval(t)))
            } else {
                let t = glow_ab_position(v, looping, loops, phase, mid);
                [ca[0] + (cb[0] - ca[0]) * t, ca[1] + (cb[1] - ca[1]) * t, ca[2] + (cb[2] - ca[2]) * t]
            };
            *px = [c[0] * v, c[1] * v, c[2] * v, v];
        });
    }
    let mode = glow_composite(ctx.params.e("operation"));
    b.img.data.par_iter_mut().zip(blurred.data.par_iter()).for_each(|(o, g)| {
        let g = g.map(|v| v * intensity);
        match mode {
            // On Top: the original over the glow, which shows only where it is transparent.
            1 => {
                let k = 1.0 - o[3];
                for c in 0..4 {
                    o[c] += g[c] * k;
                }
            }
            // Behind / None with another Glow Operation: that blend mode.
            _ if glow_op != effectcraft_color::BlendMode::Add => *o = effectcraft_color::blend_pixel(glow_op, *o, g, 0.0),
            // Behind / None: add (screen-like add on premultiplied colour; alpha grows by the glow's alpha).
            _ => {
                for c in 0..3 {
                    o[c] += g[c];
                }
                o[3] = (o[3] + g[3] * (1.0 - o[3])).min(1.0);
            }
        }
        o[3] = o[3].clamp(0.0, 1.0);
    });
    b
}

/// Deep Glow's threshold pass on one premultiplied pixel (Threshold / Threshold Smooth).
#[inline]
fn deep_glow_key(px: Px, threshold: f32, smooth: f32) -> Px {
    let a = px[3];
    let l = if a > 0.0 { luminance(px[0] / a, px[1] / a, px[2] / a) } else { 0.0 };
    let k = if smooth > 0.0 {
        ((l - threshold) / smooth).clamp(0.0, 1.0)
    } else if l >= threshold {
        1.0
    } else {
        0.0
    };
    px.map(|v| v * k)
}

/// How much of the glow input Deep Glow's Mask Layer keeps at mask pixel `m`.
#[inline]
fn deep_glow_mask(m: Px, luma: bool, invert: bool) -> f32 {
    let mut amount = if luma && m[3] > 0.0 { luminance(m[0] / m[3], m[1] / m[3], m[2] / m[3]) * m[3] } else { m[3] };
    if invert {
        amount = 1.0 - amount;
    }
    amount.clamp(0.0, 1.0)
}

/// Deep Glow's display → linear conversion of one pixel (Unmult: of the values as seen over
/// black, else of the unpremultiplied colour).
#[inline]
fn deep_glow_to_linear(px: &mut Px, unmult: bool) {
    let a = px[3];
    if unmult {
        for v in px.iter_mut().take(3) {
            *v = dg_decode(v.clamp(0.0, 1.0));
        }
    } else if a > 0.0 {
        for v in px.iter_mut().take(3) {
            *v = dg_decode((*v / a).clamp(0.0, 1.0)) * a;
        }
    }
}

/// Whether every channel of every pixel of `px` is ±0 (a branch-free scan that vectorises).
#[inline]
fn all_zero(px: &[Px]) -> bool {
    px.iter().fold(0u32, |acc, p| acc | p[0].to_bits() | p[1].to_bits() | p[2].to_bits() | p[3].to_bits()) & 0x7fff_ffff == 0
}

/// Pixels per block when skipping transparent runs.
const ZERO_RUN: usize = 32;

/// Bounding box `[x0, x1) × [y0, y1)` of the pixels of `img` for which `nonzero` holds (which
/// must be false for transparent pixels), and for each row whether it is entirely ±0.
fn bounding_box(img: &Image, nonzero: impl Fn(&Px) -> bool + Sync) -> (Option<(u32, u32, u32, u32)>, Vec<bool>) {
    let w = img.width.max(1) as usize;
    let rows: Vec<(bool, Option<(usize, usize)>)> = img
        .data
        .par_chunks(w)
        .map(|row| {
            if all_zero(row) {
                return (true, None);
            }
            let find = |(i, chunk): (usize, &[Px]), last: bool| -> Option<usize> {
                if all_zero(chunk) {
                    return None;
                }
                let at = if last { chunk.iter().rposition(&nonzero) } else { chunk.iter().position(&nonzero) };
                at.map(|x| i * ZERO_RUN + x)
            };
            let x0 = row.chunks(ZERO_RUN).enumerate().find_map(|c| find(c, false));
            let x1 = row.chunks(ZERO_RUN).enumerate().rev().find_map(|c| find(c, true));
            (false, x0.zip(x1).map(|(a, b)| (a, b + 1)))
        })
        .collect();
    let bbox = rows
        .iter()
        .enumerate()
        .filter_map(|(y, r)| r.1.map(|(x0, x1)| (x0 as u32, y as u32, x1 as u32, y as u32 + 1)))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)));
    (bbox, rows.into_iter().map(|r| r.0).collect())
}

/// Deep Glow's Exposure on Deep Glow 2's own scale (the same numbers as in After Effects).
/// Projects saved with the earlier half-strength scale are halved once on open
/// ([`crate::migrate::ALIAS_VALUE_SCALES`]).
pub fn exposure_gain(exposure: f64) -> f32 {
    exposure.clamp(0.0, 128.0) as f32
}

/// The linear model's encoding both ways: a plain power (Deep Glow 2's glow falloff and its
/// coloured halos fit it better than the sRGB curve).
pub const DEEP_GLOW_GAMMA: f32 = 2.865;
/// Exposure's power in the linear model.
pub const DEEP_GLOW_EXPOSURE_POWER: f32 = 1.218;

/// One of Deep Glow's blur passes: its Gaussian sigma (working pixels, before Aspect Ratio),
/// weight, and whether it is blurred at the Downsample size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlowPass {
    pub sigma: f64,
    pub weight: f32,
    pub downsample: bool,
}

/// Deep Glow's blur passes for a `radius` in working pixels at layer `scale` (shared with the
/// GPU kernels).
///
/// Linear model (Gamma Correction): fitted to Deep Glow 2 in After Effects 2026 (white bar,
/// square and 2 px line; Radius 25-1000, Exposure 0.04-2; 2.2 levels RMS, 1.4 at Radius 282 /
/// Exposure 0.04): a 2.16 px pass plus blurs from 4.19 px growing x1.437 up to 0.1227 x Radius,
/// the widest fading in as Radius grows (those of 24 px and more at the Downsample size). Radius adds wider levels and leaves the ones by the
/// edge alone, so the glow next to a shape is as bright at any Radius, as in Deep Glow 2.
/// Without it, the older display-space profile.
pub fn deep_glow_passes(radius: f64, scale: f64, linear_model: bool, spread: f32) -> Vec<GlowPass> {
    if !linear_model {
        let near = (0.30 - 0.35 * spread).max(0.05);
        let far = 0.30 + 0.44 * spread;
        return [(0.035, near * 13.5, false), (0.14, (1.0 - near - far) * 13.5, false), (0.42, far * 13.5, true)]
            .map(|(sigma, weight, downsample)| GlowPass { sigma: radius * sigma, weight, downsample })
            .to_vec();
    }
    let (first, ratio) = (4.188 * scale.max(1e-3), 1.437f64);
    let levels = ((0.1227 * radius).max(first) / first).ln() / ratio.ln();
    let mut passes = vec![GlowPass { sigma: 2.159 * scale.max(1e-3), weight: 1.107, downsample: false }];
    let mut k = 0u32;
    while f64::from(k) < levels + 1.0 && k < 32 {
        let fade = (levels + 1.0 - f64::from(k)).min(1.0) as f32;
        let sigma = first * ratio.powi(k as i32);
        // Downsample applies to the wide passes (it only softens what is already soft).
        passes.push(GlowPass { sigma, weight: 0.5479 * (-0.0600 * k as f32).exp() * fade, downsample: sigma >= 24.0 * scale });
        k += 1;
    }
    passes
}

/// Where the linear model's power curve turns into a straight line to black (linear 1e-5,
/// display ~4.6/255): the power's infinite slope at 0 would otherwise lift a blur's faintest
/// tail (linear 1e-6, where the blurs end) into a visible 1-2 level edge.
pub const DEEP_GLOW_TOE: f32 = 1e-5;

/// The linear model's display -> linear and linear -> display values.
#[inline]
fn dg_decode(v: f32) -> f32 {
    let knee = DEEP_GLOW_TOE.powf(1.0 / DEEP_GLOW_GAMMA);
    let v = v.max(0.0);
    if v < knee { v * DEEP_GLOW_TOE / knee } else { v.powf(DEEP_GLOW_GAMMA) }
}
#[inline]
fn dg_encode(v: f32) -> f32 {
    let knee = DEEP_GLOW_TOE.powf(1.0 / DEEP_GLOW_GAMMA);
    let v = v.max(0.0);
    if v < DEEP_GLOW_TOE { v * knee / DEEP_GLOW_TOE } else { v.powf(1.0 / DEEP_GLOW_GAMMA) }
}

/// Editable, clean-room approximation of Plugin Everything Deep Glow v1. The public product
/// description specifies a wide inverse-square-like falloff; three Gaussian scales approximate
/// that profile without depending on the third-party plug-in.
///
/// The glow input is non-zero only around the layer's bright content, so the blurs and
/// resamples run on that rectangle ([`Sparse`]) instead of the whole padded frame, and the
/// conversions, composite, Unmult and Dither run in one pass over the frame.
fn deep_glow(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let radius = (ctx.params.f("PEDG-0001") * b.scale).clamp(0.0, 2000.0);
    if !radius.is_finite() || radius < 0.5 {
        return b;
    }
    // Mode 2 is the expanded transparent-edge setting found in every SceneYard instance.
    let edge = ctx.params.e("PEDG-0055");
    let wanted_pad = if edge == 0 || ctx.adjustment { 0 } else { (radius * if ctx.params.e("PEDG-0084") == 1 { 1.5 } else { 1.0 }).ceil() as u32 };
    // Keep the output bounded, but still render a glow when its full expansion will not fit.
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
    let pad = low;
    if pad > 0 {
        b.pad(pad);
    }
    let threshold = (ctx.params.f("PEDG-0004") / 100.0).max(0.0) as f32;
    let smooth = (ctx.params.f("PEDG-0005") / 100.0).max(0.0) as f32;
    let mask = ctx.layer_param("PEDG-0056", true).map(|layer| crate::util::fit_layer(ctx, &b, &layer, false));
    let luma = ctx.params.e("PEDG-0057") == 1;
    let invert = ctx.params.b("PEDG-0059");
    let masked = |px: Px, x: usize, y: usize| -> Px {
        let px = deep_glow_key(px, threshold, smooth);
        match &mask {
            Some(m) => {
                let amount = deep_glow_mask(m.get(x as i64, y as i64), luma, invert);
                px.map(|v| v * amount)
            }
            None => px,
        }
    };
    // AE View 1 (native index 0) previews the glow input; View 2 renders the final composite.
    if ctx.params.e("PEDG-0023") == 0 {
        b.img.rows_mut().for_each(|(y, row)| {
            for (x, px) in row.iter_mut().enumerate() {
                *px = masked(*px, x, y);
            }
        });
        return b;
    }
    // Gamma Correction (Auto Detect, on by default): the glow is built and composited in
    // linear light ([`DEEP_GLOW_GAMMA`]), with Deep Glow 2's passes ([`deep_glow_passes`])
    // scaled by Exposure^[`DEEP_GLOW_EXPOSURE_POWER`]. Without it, the older display-space profile.
    let linear_model = ctx.params.b("PEDG-0043") || ctx.params.b("PEDG-0009");
    let convert = linear_model && !ctx.env.working_linear;
    // With Unmult the glow works on the layer as seen over black (premultiplied values), so a
    // pixel the glow does not touch comes back unchanged.
    let unmult = ctx.params.b("PEDG-0025");
    // The glow input, kept to the rectangle around its non-zero pixels (a mask only removes).
    let (fw, fh) = (b.img.width, b.img.height);
    let (bbox, zero_rows) = bounding_box(&b.img, |px| *px != [0.0; 4] && deep_glow_key(*px, threshold, smooth) != [0.0; 4]);
    let source = match bbox {
        Some((x0, y0, x1, y1)) => {
            let mut img = Image::new(x1 - x0, y1 - y0);
            let src = &b.img;
            img.rows_mut().for_each(|(ry, row)| {
                let y = y0 as usize + ry;
                let srow = &src.data[y * fw as usize + x0 as usize..y * fw as usize + x1 as usize];
                for (rx, (px, s)) in row.iter_mut().zip(srow).enumerate() {
                    *px = masked(*s, x0 as usize + rx, y);
                    if convert {
                        deep_glow_to_linear(px, unmult);
                    }
                }
            });
            Sparse { img, x: x0, y: y0, width: fw, height: fh }
        }
        None => Sparse::empty(fw, fh),
    };
    drop(mask);
    let spread = (ctx.params.f("PEDG-0011") / 100.0).clamp(0.0, 1.0) as f32;
    let passes = deep_glow_passes(radius, b.scale, linear_model, spread);
    let aspect = ctx.params.f("PEDG-0010").clamp(0.0, 2.0);
    let chroma = if ctx.params.b("PEDG-0013") { ctx.params.f("PEDG-0015").clamp(0.0, 2.0) as f32 } else { 0.0 };
    let downsample = (ctx.params.f("PEDG-0019") / 100.0).clamp(0.1, 1.0);
    // Very wide glows run at a reduced working resolution; the final halo is enlarged once.
    // Radii at or below 512 retain the calibrated full-resolution path exactly.
    let work_scale = (512.0 / radius).min(1.0);
    let work_source = if work_scale < 1.0 {
        let w = ((fw as f64 * work_scale).round() as u32).max(1);
        let h = ((fh as f64 * work_scale).round() as u32).max(1);
        resample_sparse(&source, w, h)
    } else {
        source
    };
    let (ww, wh) = (work_source.width, work_source.height);
    let blurred: Vec<Sparse> = passes
        .iter()
        .map(|pass| {
            let (sx, sy) = (pass.sigma * work_scale * aspect, pass.sigma * work_scale * (2.0 - aspect));
            if pass.downsample && downsample < 0.999 {
                let (w, h) = ((ww as f64 * downsample).round().max(1.0) as u32, (wh as f64 * downsample).round().max(1.0) as u32);
                let small = resample_sparse(&work_source, w, h);
                resample_sparse(&gaussian_blur_sparse(&small, sx * downsample, sy * downsample), ww, wh)
            } else {
                gaussian_blur_sparse(&work_source, sx, sy)
            }
        })
        .collect();
    drop(work_source);
    // The weighted passes summed on the union of their rectangles.
    let union = blurred
        .iter()
        .filter(|s| !s.img.is_empty())
        .map(|s| {
            let (ex, ey) = s.end();
            (s.x, s.y, ex, ey)
        })
        .reduce(|a, c| (a.0.min(c.0), a.1.min(c.1), a.2.max(c.2), a.3.max(c.3)));
    let halo = match union {
        Some((x0, y0, x1, y1)) => {
            let mut img = Image::new(x1 - x0, y1 - y0);
            img.rows_mut().for_each(|(ry, row)| {
                let y = y0 + ry as u32;
                for (step, (part, weight)) in blurred.iter().zip(passes.iter().map(|p| p.weight)).enumerate() {
                    let (ex, ey) = part.end();
                    if part.img.is_empty() || y < part.y || y >= ey {
                        continue;
                    }
                    // Chromatic Aberration: the widest pass redder, the narrowest bluer.
                    let fringe = [if step + 1 == passes.len() { 1.0 + chroma } else { 1.0 }, 1.0, if step == 0 { 1.0 + chroma } else { 1.0 }, 1.0];
                    let pw = part.img.width as usize;
                    let start = (y - part.y) as usize * pw;
                    let prow = &part.img.data[start..start + pw];
                    let off = (part.x - x0) as usize;
                    for (out, px) in row[off..off + (ex - part.x) as usize].iter_mut().zip(prow) {
                        for c in 0..4 {
                            out[c] += px[c] * weight * fringe[c];
                        }
                    }
                }
            });
            Sparse { img, x: x0, y: y0, width: ww, height: wh }
        }
        None => Sparse::empty(ww, wh),
    };
    drop(blurred);
    let halo = if work_scale < 1.0 { resample_sparse(&halo, fw, fh) } else { halo };
    let exposure = exposure_gain(ctx.params.f("glowExposure"));
    let exposure = if linear_model { exposure.powf(DEEP_GLOW_EXPOSURE_POWER) } else { exposure };
    let source_opacity = (ctx.params.f("PEDG-0024") / 100.0).clamp(0.0, 1.0) as f32;
    let screen = ctx.params.e("PEDG-0008") == 0;
    let tint = ctx.params.b("PEDG-0037");
    let tint_color = ctx.params.color("PEDG-0038");
    let tint_mode = ctx.params.e("PEDG-0039");
    let tint_mix = (ctx.params.f("PEDG-0041") / 100.0).clamp(0.0, 1.0) as f32;
    let dither = ctx.params.b("PEDG-0031");
    let dither_amount = (ctx.params.f("PEDG-0033") / 100.0).clamp(0.0, 1.0) as f32 / 255.0;
    let monochrome = ctx.params.b("PEDG-0032");
    let seed = ctx.seed;
    // One pixel of the final composite: the original (to linear), plus the exposed, tinted
    // glow, back to display values, Unmult's alpha, then Dither.
    let finish = |px: Px, glow: Px, x: u32, y: u32| -> Px {
        let mut original = px;
        if convert {
            deep_glow_to_linear(&mut original, unmult);
        }
        let mut out = original;
        for c in 0..3 {
            let mut g = glow[c] * exposure;
            if tint {
                let t = tint_color[c];
                let tinted = match tint_mode {
                    1 if g <= 0.5 => 2.0 * g * t,
                    1 => 1.0 - 2.0 * (1.0 - g) * (1.0 - t),
                    2 if t <= 0.5 => g - (1.0 - 2.0 * t) * g * (1.0 - g),
                    2 => g + (2.0 * t - 1.0) * ((if g <= 0.25 { ((16.0 * g - 12.0) * g + 4.0) * g } else { g.sqrt() }) - g),
                    _ => g * t,
                };
                g += (tinted - g) * tint_mix;
            }
            let s = original[c] * source_opacity;
            out[c] = if screen { (s + g - s * g).clamp(0.0, 1.0) } else { s + g };
        }
        // With Unmult the glow's alpha is its brightness (set below), not its summed coverage:
        // the weighted passes add up to ~10× the source alpha, which would turn a faint halo
        // into an opaque dark one.
        out[3] = if unmult { original[3] * source_opacity } else { glow[3] * exposure + original[3] * source_opacity }.clamp(0.0, 1.0);
        if convert {
            // Back to display values as seen over black (what AE shows); with Unmult the glow's
            // alpha follows its brightness, so a faint halo is not darkened by premultiplication.
            let a = out[3];
            if unmult {
                for v in out.iter_mut().take(3) {
                    *v = dg_encode(v.clamp(0.0, 1.0));
                }
            } else if a > 0.0 {
                for v in out.iter_mut().take(3) {
                    *v = dg_encode((*v / a).max(0.0)).min(1.0) * a;
                }
            }
        }
        if unmult {
            // Unmult: the result as seen over black, with alpha = its brightest channel (black is
            // transparent), so a faint halo over a light background stays light. SceneYard's "Magic
            // button item reveal" (Radius 500 over near-white): frame MAE vs AE 0.080 → 0.024.
            out[3] = out[3].max(out[0].max(out[1]).max(out[2])).clamp(0.0, 1.0);
        } else {
            out[3] = 1.0;
        }
        if dither && out[3] >= 0.999 {
            for c in 0..3u32 {
                let n = (hash_noise(x.wrapping_add(if monochrome { 0 } else { c.wrapping_mul(fw) }), y, seed) - 0.5) * dither_amount;
                out[c as usize] = (out[c as usize] + n).max(0.0);
            }
        }
        out
    };
    // Pixels with neither content nor glow are left as they are when that is the result, so
    // transparent rows and runs outside the halo are skipped without per-pixel work.
    let keep_empty = finish([0.0; 4], [0.0; 4], 0, 0) == [0.0; 4];
    let (hx1, hy1) = halo.end();
    let (hx0, hy0) = (halo.x.min(hx1) as usize, halo.y);
    let hw = halo.img.width as usize;
    b.img.rows_mut().for_each(|(y, row)| {
        let in_halo = !halo.img.is_empty() && (hy0..hy1).contains(&(y as u32));
        if keep_empty && !in_halo && zero_rows.get(y).copied().unwrap_or(false) {
            return;
        }
        let (gx0, gx1) = if in_halo { (hx0, hx1 as usize) } else { (0, 0) };
        let glow_row = if in_halo { &halo.img.data[(y - hy0 as usize) * hw..(y - hy0 as usize + 1) * hw] } else { &[] };
        let y = y as u32;
        // Outside the halo: the original alone.
        let plain = |px: &mut [Px], x0: usize| {
            for (i, chunk) in px.chunks_mut(ZERO_RUN).enumerate() {
                if keep_empty && all_zero(chunk) {
                    continue;
                }
                for (j, p) in chunk.iter_mut().enumerate() {
                    if !(keep_empty && *p == [0.0; 4]) {
                        *p = finish(*p, [0.0; 4], (x0 + i * ZERO_RUN + j) as u32, y);
                    }
                }
            }
        };
        let (left, rest) = row.split_at_mut(gx0.min(row.len()));
        let (mid, right) = rest.split_at_mut((gx1 - gx0).min(rest.len()));
        plain(left, 0);
        for (i, (p, glow)) in mid.iter_mut().zip(glow_row).enumerate() {
            if !(keep_empty && *p == [0.0; 4] && *glow == [0.0; 4]) {
                *p = finish(*p, *glow, (gx0 + i) as u32, y);
            }
        }
        plain(right, gx1);
    });
    b
}

/// The full-frame implementation [`deep_glow`] replaced (reference for its tests).
#[cfg(test)]
fn deep_glow_reference(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let radius = (ctx.params.f("PEDG-0001") * b.scale).clamp(0.0, 2000.0);
    if !radius.is_finite() || radius < 0.5 {
        return b;
    }
    // Mode 2 is the expanded transparent-edge setting found in every SceneYard instance.
    let edge = ctx.params.e("PEDG-0055");
    let wanted_pad = if edge == 0 || ctx.adjustment { 0 } else { (radius * if ctx.params.e("PEDG-0084") == 1 { 1.5 } else { 1.0 }).ceil() as u32 };
    // Keep the output bounded, but still render a glow when its full expansion will not fit.
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
    let pad = low;
    if pad > 0 {
        b.pad(pad);
    }
    let threshold = (ctx.params.f("PEDG-0004") / 100.0).max(0.0) as f32;
    let smooth = (ctx.params.f("PEDG-0005") / 100.0).max(0.0) as f32;
    let mut source = b.img.clone();
    source.data.par_iter_mut().for_each(|px| {
        let a = px[3];
        let l = if a > 0.0 { luminance(px[0] / a, px[1] / a, px[2] / a) } else { 0.0 };
        let k = if smooth > 0.0 {
            ((l - threshold) / smooth).clamp(0.0, 1.0)
        } else if l >= threshold {
            1.0
        } else {
            0.0
        };
        for v in px.iter_mut() {
            *v *= k;
        }
    });
    if let Some(layer) = ctx.layer_param("PEDG-0056", true) {
        let mask = crate::util::fit_layer(ctx, &b, &layer, false);
        let luma = ctx.params.e("PEDG-0057") == 1;
        let invert = ctx.params.b("PEDG-0059");
        source.data.par_iter_mut().zip(mask.data.par_iter()).for_each(|(px, m)| {
            let mut amount = if luma && m[3] > 0.0 { luminance(m[0] / m[3], m[1] / m[3], m[2] / m[3]) * m[3] } else { m[3] };
            if invert {
                amount = 1.0 - amount;
            }
            for channel in px.iter_mut() {
                *channel *= amount.clamp(0.0, 1.0);
            }
        });
    }
    // AE View 1 (native index 0) previews the glow input; View 2 renders the final composite.
    if ctx.params.e("PEDG-0023") == 0 {
        b.img = source;
        return b;
    }
    // Gamma Correction (Auto Detect, on by default): the glow is built and composited in
    // linear light ([`DEEP_GLOW_GAMMA`]), with Deep Glow 2's passes ([`deep_glow_passes`])
    // scaled by Exposure^[`DEEP_GLOW_EXPOSURE_POWER`]. Without it, the older display-space profile.
    let linear_model = ctx.params.b("PEDG-0043") || ctx.params.b("PEDG-0009");
    let convert = linear_model && !ctx.env.working_linear;
    // With Unmult the glow works on the layer as seen over black (premultiplied values), so a
    // pixel the glow does not touch comes back unchanged.
    let unmult = ctx.params.b("PEDG-0025");
    let to_linear = |img: &mut Image| {
        img.data.par_iter_mut().for_each(|px| {
            let a = px[3];
            if unmult {
                for v in px.iter_mut().take(3) {
                    *v = dg_decode(v.clamp(0.0, 1.0));
                }
            } else if a > 0.0 {
                for v in px.iter_mut().take(3) {
                    *v = dg_decode((*v / a).clamp(0.0, 1.0)) * a;
                }
            }
        })
    };
    if convert {
        to_linear(&mut source);
    }
    let spread = (ctx.params.f("PEDG-0011") / 100.0).clamp(0.0, 1.0) as f32;
    let passes = deep_glow_passes(radius, b.scale, linear_model, spread);
    let aspect = ctx.params.f("PEDG-0010").clamp(0.0, 2.0);
    let chroma = if ctx.params.b("PEDG-0013") { ctx.params.f("PEDG-0015").clamp(0.0, 2.0) as f32 } else { 0.0 };
    let downsample = (ctx.params.f("PEDG-0019") / 100.0).clamp(0.1, 1.0);
    // Very wide glows run at a reduced working resolution; the final halo is enlarged once.
    // Radii at or below 512 retain the calibrated full-resolution path exactly.
    let work_scale = (512.0 / radius).min(1.0);
    let work_source = if work_scale < 1.0 {
        let w = ((source.width as f64 * work_scale).round() as u32).max(1);
        let h = ((source.height as f64 * work_scale).round() as u32).max(1);
        let reduced = resample(&source, w, h);
        drop(source);
        reduced
    } else {
        source
    };
    let mut halo = Image::new(work_source.width, work_source.height);
    for (step, pass) in passes.iter().enumerate() {
        let weight = pass.weight;
        let (sx, sy) = (pass.sigma * work_scale * aspect, pass.sigma * work_scale * (2.0 - aspect));
        let blurred = if pass.downsample && downsample < 0.999 {
            let (w, h) = ((work_source.width as f64 * downsample).round().max(1.0) as u32, (work_source.height as f64 * downsample).round().max(1.0) as u32);
            let small = resample(&work_source, w, h);
            resample(&gaussian_blur(&small, sx * downsample, sy * downsample, false), work_source.width, work_source.height)
        } else {
            gaussian_blur(&work_source, sx, sy, false)
        };
        halo.data.par_iter_mut().zip(blurred.data.par_iter()).for_each(|(out, px)| {
            for c in 0..4 {
                let fringe = if c == 0 && step + 1 == passes.len() {
                    1.0 + chroma
                } else if c == 2 && step == 0 {
                    1.0 + chroma
                } else {
                    1.0
                };
                out[c] += px[c] * weight * fringe;
            }
        });
    }
    let halo = if work_scale < 1.0 { resample(&halo, b.img.width, b.img.height) } else { halo };
    let exposure = exposure_gain(ctx.params.f("glowExposure"));
    let exposure = if linear_model { exposure.powf(DEEP_GLOW_EXPOSURE_POWER) } else { exposure };
    if convert {
        to_linear(&mut b.img);
    }
    let source_opacity = (ctx.params.f("PEDG-0024") / 100.0).clamp(0.0, 1.0) as f32;
    let screen = ctx.params.e("PEDG-0008") == 0;
    let tint = ctx.params.b("PEDG-0037");
    let tint_color = ctx.params.color("PEDG-0038");
    let tint_mode = ctx.params.e("PEDG-0039");
    let tint_mix = (ctx.params.f("PEDG-0041") / 100.0).clamp(0.0, 1.0) as f32;
    b.img.data.par_iter_mut().zip(halo.data.par_iter()).for_each(|(out, glow)| {
        let original = *out;
        for c in 0..3 {
            let mut g = glow[c] * exposure;
            if tint {
                let t = tint_color[c];
                let tinted = match tint_mode {
                    1 if g <= 0.5 => 2.0 * g * t,
                    1 => 1.0 - 2.0 * (1.0 - g) * (1.0 - t),
                    2 if t <= 0.5 => g - (1.0 - 2.0 * t) * g * (1.0 - g),
                    2 => g + (2.0 * t - 1.0) * ((if g <= 0.25 { ((16.0 * g - 12.0) * g + 4.0) * g } else { g.sqrt() }) - g),
                    _ => g * t,
                };
                g += (tinted - g) * tint_mix;
            }
            let s = original[c] * source_opacity;
            out[c] = if screen { (s + g - s * g).clamp(0.0, 1.0) } else { s + g };
        }
        // With Unmult the glow's alpha is its brightness (set below), not its summed coverage:
        // the weighted passes add up to ~10× the source alpha, which would turn a faint halo
        // into an opaque dark one.
        out[3] = if unmult { original[3] * source_opacity } else { glow[3] * exposure + original[3] * source_opacity }.clamp(0.0, 1.0);
    });
    if convert {
        // Back to display values as seen over black (what AE shows); with Unmult the glow's
        // alpha follows its brightness, so a faint halo is not darkened by premultiplication.
        b.img.data.par_iter_mut().for_each(|px| {
            let a = px[3];
            if unmult {
                for v in px.iter_mut().take(3) {
                    *v = dg_encode(v.clamp(0.0, 1.0));
                }
            } else if a > 0.0 {
                for v in px.iter_mut().take(3) {
                    *v = dg_encode((*v / a).max(0.0)).min(1.0) * a;
                }
            }
        });
    }
    if unmult {
        // Unmult: the result as seen over black, with alpha = its brightest channel (black is
        // transparent), so a faint halo over a light background stays light. SceneYard's "Magic
        // button item reveal" (Radius 500 over near-white): frame MAE vs AE 0.080 → 0.024.
        b.img.data.par_iter_mut().for_each(|px| px[3] = px[3].max(px[0].max(px[1]).max(px[2])).clamp(0.0, 1.0));
    }
    if !ctx.params.b("PEDG-0025") {
        b.img.data.par_iter_mut().for_each(|px| px[3] = 1.0);
    }
    if ctx.params.b("PEDG-0031") {
        let amount = (ctx.params.f("PEDG-0033") / 100.0).clamp(0.0, 1.0) as f32 / 255.0;
        let monochrome = ctx.params.b("PEDG-0032");
        let width = b.img.width;
        b.img.rows_mut().for_each(|(y, row)| {
            for (x, px) in row.iter_mut().enumerate() {
                if px[3] < 0.999 {
                    continue;
                }
                let channel_noise =
                    |c: u32| (hash_noise((x as u32).wrapping_add(if monochrome { 0 } else { c.wrapping_mul(width) }), y as u32, ctx.seed) - 0.5) * amount;
                for c in 0..3 {
                    px[c] = (px[c] + channel_noise(c as u32)).max(0.0);
                }
            }
        });
    }
    b
}

fn drop_shadow(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let color = ctx.params.color("color");
    let opacity = ctx.params.f("opacity") as f32 / 100.0;
    let dir = ctx.params.f("direction").to_radians();
    let dist = ctx.params.f("distance") * b.scale;
    let soft = ctx.params.f("softness") * b.scale;
    let only = ctx.params.b("shadowOnly");
    let pad = (dist + soft * 1.5).ceil() as u32 + 2;
    if !ctx.adjustment {
        b.pad(pad);
    }
    let (dx, dy) = (dir.sin() * dist, -dir.cos() * dist);
    let mut sh = Image::new(b.img.width, b.img.height);
    let src = &b.img;
    sh.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let a = src.sample_bilinear(x as f64 + 0.5 - dx, y as f64 + 0.5 - dy)[3] * opacity * color[3];
            *px = [color[0] * a, color[1] * a, color[2] * a, a];
        }
    });
    if soft > 0.0 {
        sh = gaussian_blur(&sh, soft / 2.0, soft / 2.0, false);
    }
    if !only {
        sh.data.par_iter_mut().zip(b.img.data.par_iter()).for_each(|(s, o)| {
            let k = 1.0 - o[3];
            for c in 0..4 {
                s[c] = o[c] + s[c] * k;
            }
        });
    }
    b.img = sh;
    b
}

/// Invert's Channel options (After Effects order): RGB / HLS / YIQ groups, each followed by its
/// single channels, then Alpha.
pub const INVERT_CHANNELS: [&str; 13] =
    ["RGB", "Red", "Green", "Blue", "HLS", "Hue", "Lightness", "Saturation", "YIQ", "Luminance", "In Phase Chrominance", "Quadrature Chrominance", "Alpha"];
/// Index of "Alpha" in [`INVERT_CHANNELS`].
pub const INVERT_ALPHA: u32 = 12;

fn rgb_to_yiq(c: [f32; 3]) -> [f32; 3] {
    [0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2], 0.596 * c[0] - 0.274 * c[1] - 0.322 * c[2], 0.211 * c[0] - 0.523 * c[1] + 0.312 * c[2]]
}

fn yiq_to_rgb(c: [f32; 3]) -> [f32; 3] {
    [c[0] + 0.956 * c[1] + 0.621 * c[2], c[0] - 0.272 * c[1] - 0.647 * c[2], c[0] - 1.106 * c[1] + 1.703 * c[2]]
}

/// Invert straight colour `s` in the colour space / channel `ch` (an [`INVERT_CHANNELS`] index
/// other than Alpha).
fn invert_color(ch: u32, s: [f32; 3]) -> [f32; 3] {
    match ch {
        1 => [1.0 - s[0], s[1], s[2]],
        2 => [s[0], 1.0 - s[1], s[2]],
        3 => [s[0], s[1], 1.0 - s[2]],
        4..=7 => {
            let (mut h, mut sat, mut l) = effectcraft_color::rgb_to_hsl(s[0], s[1], s[2]);
            if matches!(ch, 4 | 5) {
                h = 1.0 - h;
            }
            if matches!(ch, 4 | 6) {
                l = 1.0 - l;
            }
            if matches!(ch, 4 | 7) {
                sat = 1.0 - sat;
            }
            let (r, g, b) = effectcraft_color::hsl_to_rgb(h, sat, l);
            [r, g, b]
        }
        8..=11 => {
            // Y inverts around 1, the (signed) chrominance axes around 0.
            let mut q = rgb_to_yiq(s);
            if matches!(ch, 8 | 9) {
                q[0] = 1.0 - q[0];
            }
            if matches!(ch, 8 | 10) {
                q[1] = -q[1];
            }
            if matches!(ch, 8 | 11) {
                q[2] = -q[2];
            }
            yiq_to_rgb(q).map(|v| v.max(0.0))
        }
        _ => [1.0 - s[0], 1.0 - s[1], 1.0 - s[2]],
    }
}

fn invert(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let ch = ctx.params.e("channel");
    let blend = 1.0 - ctx.params.f("blend") as f32 / 100.0;
    b.img.data.par_iter_mut().for_each(|px| {
        let a = px[3];
        if ch == INVERT_ALPHA {
            let na = 1.0 - a;
            let k = if a > 0.0 { na / a } else { 0.0 };
            let inv = [px[0] * k, px[1] * k, px[2] * k, na];
            for c in 0..4 {
                px[c] += (inv[c] - px[c]) * blend;
            }
            return;
        }
        if a <= 0.0 {
            return;
        }
        let s = [px[0] / a, px[1] / a, px[2] / a];
        let o = invert_color(ch, s);
        for c in 0..3 {
            px[c] = (s[c] + (o[c] - s[c]) * blend) * a;
        }
    });
    b
}

fn mosaic(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let hb = ctx.params.f("horizontal").round().max(1.0) as usize;
    let vb = ctx.params.f("vertical").round().max(1.0) as usize;
    let (w, h) = (b.img.width as usize, b.img.height as usize);
    let cw = (w as f64 / hb as f64).max(1.0);
    let ch = (h as f64 / vb as f64).max(1.0);
    let src = b.img.clone();
    if ctx.params.b("sharpColors") {
        // Each tile takes the colour of the pixel at its centre.
        b.img.rows_mut().for_each(|(y, row)| {
            let cy = ((y as f64 / ch).floor() + 0.5) * ch;
            for (x, px) in row.iter_mut().enumerate() {
                let cx = ((x as f64 / cw).floor() + 0.5) * cw;
                *px = src.get_clamped(cx as i64, cy as i64);
            }
        });
        return b;
    }
    // Each tile takes the average colour of its region.
    let (nx, ny) = ((w as f64 / cw).ceil() as usize, (h as f64 / ch).ceil() as usize);
    let mut sums = vec![[0.0f64; 5]; nx * ny];
    for y in 0..h {
        let ty = ((y as f64 / ch) as usize).min(ny - 1);
        for x in 0..w {
            let tx = ((x as f64 / cw) as usize).min(nx - 1);
            let p = src.get(x as i64, y as i64);
            let s = &mut sums[ty * nx + tx];
            for c in 0..4 {
                s[c] += p[c] as f64;
            }
            s[4] += 1.0;
        }
    }
    b.img.rows_mut().for_each(|(y, row)| {
        let ty = ((y as f64 / ch) as usize).min(ny - 1);
        for (x, px) in row.iter_mut().enumerate() {
            let tx = ((x as f64 / cw) as usize).min(nx - 1);
            let s = sums[ty * nx + tx];
            *px = [0, 1, 2, 3].map(|c| (s[c] / s[4].max(1.0)) as f32);
        }
    });
    b
}

fn posterize(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let lv = ctx.params.f("level").round().clamp(2.0, 255.0) as f32 - 1.0;
    b.img.map_straight(|c| c.map(|v| (v.clamp(0.0, 1.0) * lv).round() / lv));
    b
}

fn threshold(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let t = ctx.params.f("level") as f32 / 255.0;
    b.img.map_straight(|c| {
        let v = if luminance(c[0], c[1], c[2]) >= t { 1.0 } else { 0.0 };
        [v, v, v]
    });
    b
}

fn find_edges(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let inv = ctx.params.b("invert");
    let blend = ctx.params.f("blend") as f32 / 100.0;
    let src = b.img.clone();
    b.img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let l = |dx: i64, dy: i64| {
                let p = src.get_clamped(x as i64 + dx, y as i64 + dy);
                luminance(p[0], p[1], p[2])
            };
            let gx = l(1, -1) + 2.0 * l(1, 0) + l(1, 1) - l(-1, -1) - 2.0 * l(-1, 0) - l(-1, 1);
            let gy = l(-1, 1) + 2.0 * l(0, 1) + l(1, 1) - l(-1, -1) - 2.0 * l(0, -1) - l(1, -1);
            let e = (gx * gx + gy * gy).sqrt().min(1.0);
            let v = if inv { e } else { 1.0 - e };
            let a = px[3];
            *px = blend_with(*px, [v * a, v * a, v * a, a], blend);
        }
    });
    b
}

/// Blend With Original: `orig` over the effect result `fx` by `k` (0 = effect only).
fn blend_with(orig: [f32; 4], fx: [f32; 4], k: f32) -> [f32; 4] {
    [0, 1, 2, 3].map(|c| fx[c] + (orig[c] - fx[c]) * k)
}

fn emboss(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let ang = ctx.params.f("direction").to_radians();
    let relief = ctx.params.f("relief") * b.scale;
    let contrast = ctx.params.f("contrast") as f32 / 100.0;
    let blend = ctx.params.f("blend") as f32 / 100.0;
    let (dx, dy) = (ang.cos() * relief, -ang.sin() * relief);
    let src = b.img.clone();
    b.img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let a = src.sample_bilinear_clamped(x as f64 + 0.5 + dx, y as f64 + 0.5 + dy);
            let c = src.sample_bilinear_clamped(x as f64 + 0.5 - dx, y as f64 + 0.5 - dy);
            let d = luminance(a[0], a[1], a[2]) - luminance(c[0], c[1], c[2]);
            let v = (0.5 + d * contrast).clamp(0.0, 1.0);
            let al = px[3];
            *px = blend_with(*px, [v * al, v * al, v * al, al], blend);
        }
    });
    b
}

fn noise(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let amt = ctx.params.f("amount") as f32 / 100.0;
    let color = ctx.params.b("color");
    // Old saves lack the switch: clipping was always on.
    let clip = ctx.params.get("clip").is_none_or(|v| v.as_bool());
    let frame_seed = (ctx.time * 1000.0) as u32 ^ ctx.seed;
    let w = b.img.width;
    b.img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let a = px[3];
            if a <= 0.0 {
                continue;
            }
            let n = |k: u32| (hash_noise(x as u32 + k * w, y as u32, frame_seed) - 0.5) * amt;
            let (r, g, bl) = if color {
                (n(0), n(1), n(2))
            } else {
                let v = n(0);
                (v, v, v)
            };
            if clip {
                px[0] = (px[0] + r * a).clamp(0.0, a);
                px[1] = (px[1] + g * a).clamp(0.0, a);
                px[2] = (px[2] + bl * a).clamp(0.0, a);
            } else {
                // Unclipped: the noise may leave the 0..1 range (over-range in 32 bpc); an 8 or
                // 16 bpc project clamps it after the effect like any other. It never wraps
                // around (SceneYard Drone History: AE leaves cream paper with fine grain, not
                // red that wrapped to cyan).
                px[0] += r * a;
                px[1] += g * a;
                px[2] += bl * a;
            }
        }
    });
    b
}

/// Linear Wipe's feather as After Effects draws it (measured against AE 2026, LW1).
///
/// The edge sits at `lo + completion · (hi − lo)` along the wipe direction, where `lo`/`hi`
/// are the layer rectangle's extreme projections (the feather does not stretch that travel).
/// Across the edge alpha follows a Gaussian CDF with σ = [`WIPE_SIGMA_PER_FEATHER`] · feather,
/// cut off and renormalised at ± feather. Where the edge comes within about σ of either end of
/// the travel AE weakens the wipe toward that end: the transparent side by
/// `s_lo = 1 − e^(−k·(c − lo)/σ)`, the opaque side by `s_hi = 1 − e^(−k·(hi − c)/σ)` with
/// k = [`WIPE_END_RATE`], so 0 % leaves the layer whole and 100 % clears it:
/// `alpha = 1 − s_lo · (1 − s_hi · g)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearWipe {
    /// Wipe direction in buffer pixels: d = x·dx + y·dy (angle 90° = left → right).
    pub dx: f64,
    pub dy: f64,
    /// Edge position along d (buffer pixels).
    pub edge: f64,
    /// Gaussian σ (buffer pixels); 0 = a hard edge with one pixel of coverage.
    pub sigma: f64,
    /// Cut-off of the Gaussian (± feather, buffer pixels).
    pub reach: f64,
    /// Φ(−reach/σ), subtracted before renormalising the cut-off Gaussian.
    pub tail: f64,
    /// Strength of the wipe toward its start and its end (see the type docs).
    pub s_lo: f64,
    pub s_hi: f64,
}

/// σ of Linear Wipe's Gaussian feather per unit of Feather (≈ 1 / 2.58: ± feather is ± 2.58 σ).
pub const WIPE_SIGMA_PER_FEATHER: f64 = 0.388;
/// How fast the wipe reaches full strength away from either end of its travel, per σ.
pub const WIPE_END_RATE: f64 = 3.4;

/// Standard normal CDF via Abramowitz & Stegun 7.1.26 (|error| < 1.5e-7); the GPU kernel
/// evaluates the same polynomial.
pub fn wipe_phi(z: f64) -> f64 {
    let x = (z / std::f64::consts::SQRT_2).abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let poly = ((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t;
    let erf = 1.0 - poly * (-x * x).exp();
    if z < 0.0 { 0.5 * (1.0 - erf) } else { 0.5 * (1.0 + erf) }
}

impl LinearWipe {
    /// The wipe for `completion` (%), `angle` (°) and `feather` (layer px) over a layer of
    /// `layer_size` layer pixels whose origin sits at `offset` in a buffer of `scale` px per
    /// layer px (`buffer` is the buffer's size, used when the layer has no size).
    pub fn plan(completion: f64, angle: f64, feather: f64, layer_size: [f64; 2], offset: [f64; 2], scale: f64, buffer: [f64; 2]) -> LinearWipe {
        let fin = |v: f64, or: f64| if v.is_finite() { v } else { or };
        let done = (fin(completion, 0.0) / 100.0).clamp(0.0, 1.0);
        let ang = fin(angle, 0.0).to_radians();
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        let (dx, dy) = (ang.sin(), -ang.cos());
        let (x0, y0, w, h) = if layer_size[0] > 0.0 && layer_size[1] > 0.0 && layer_size.iter().all(|v| v.is_finite()) {
            (fin(offset[0], 0.0), fin(offset[1], 0.0), layer_size[0] * scale, layer_size[1] * scale)
        } else {
            (0.0, 0.0, buffer[0], buffer[1])
        };
        let proj = [(x0, y0), (x0 + w, y0), (x0, y0 + h), (x0 + w, y0 + h)].map(|(x, y)| x * dx + y * dy);
        let lo = proj.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = proj.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let edge = lo + (hi - lo) * done;
        let reach = (fin(feather, 0.0) * scale).max(0.0);
        // Under a pixel of feather the edge is hard (one pixel of coverage), as in AE.
        let sigma = if reach < 1.0 { 0.0 } else { WIPE_SIGMA_PER_FEATHER * reach };
        let end = |dist: f64| if sigma > 0.0 { 1.0 - (-WIPE_END_RATE * dist.max(0.0) / sigma).exp() } else { 1.0 };
        let tail = if sigma > 0.0 { wipe_phi(-reach / sigma) } else { 0.0 };
        // 0 % leaves the layer whole (s_lo = 0) and 100 % clears it, whatever the feather.
        let (s_lo, s_hi) = if done <= 0.0 {
            (0.0, 1.0)
        } else if done >= 1.0 {
            (1.0, 0.0)
        } else {
            (end(edge - lo), end(hi - edge))
        };
        LinearWipe { dx, dy, edge, sigma, reach, tail, s_lo, s_hi }
    }
    /// Alpha factor at signed distance `t` (buffer px) past the edge (t < 0: wiped side).
    pub fn factor(&self, t: f64) -> f64 {
        let g = if self.sigma <= 0.0 {
            (t + 0.5).clamp(0.0, 1.0)
        } else if t <= -self.reach {
            0.0
        } else if t >= self.reach {
            1.0
        } else {
            ((wipe_phi(t / self.sigma) - self.tail) / (1.0 - 2.0 * self.tail).max(1e-9)).clamp(0.0, 1.0)
        };
        (1.0 - self.s_lo * (1.0 - self.s_hi * g)).clamp(0.0, 1.0)
    }
}

fn linear_wipe(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let wipe = LinearWipe::plan(
        ctx.params.f("completion"),
        ctx.params.f("angle"),
        ctx.params.f("feather"),
        ctx.layer_size,
        b.offset,
        b.scale,
        [b.img.width as f64, b.img.height as f64],
    );
    b.img.rows_mut().for_each(|(y, row)| {
        let yd = (y as f64 + 0.5) * wipe.dy;
        for (x, px) in row.iter_mut().enumerate() {
            let t = (x as f64 + 0.5) * wipe.dx - wipe.edge + yd;
            let k = wipe.factor(t) as f32;
            for c in px.iter_mut() {
                *c *= k;
            }
        }
    });
    b
}

fn radial_wipe(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let done = ctx.params.f("completion") / 100.0;
    let start = ctx.params.f("startAngle");
    let c = b.to_px(ctx.params.v2("center"));
    let dir = ctx.params.e("wipe");
    let feather = (ctx.params.f("feather")).max(0.01);
    b.img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let a = ((x as f64 + 0.5 - c.0).atan2(-(y as f64 + 0.5 - c.1)).to_degrees() - start).rem_euclid(360.0);
            // Both: the wipe opens in both directions from the start angle at once.
            let (a, edge) = match dir {
                1 => (360.0 - a, done * 360.0),
                2 => (a.min(360.0 - a), done * 180.0),
                _ => (a, done * 360.0),
            };
            let k = ((a - edge) / feather + 0.5).clamp(0.0, 1.0) as f32;
            for ch in px.iter_mut() {
                *ch *= k;
            }
        }
    });
    b
}

fn venetian(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let done = ctx.params.f("completion") / 100.0;
    let ang = ctx.params.f("direction").to_radians();
    let width = (ctx.params.f("width") * b.scale).max(1.0);
    let feather = (ctx.params.f("feather") * b.scale).max(0.001);
    let (dx, dy) = (ang.cos(), ang.sin());
    b.img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let d = ((x as f64 + 0.5) * dx + (y as f64 + 0.5) * dy).rem_euclid(width);
            let k = ((d - done * width) / feather + 0.5).clamp(0.0, 1.0) as f32;
            for c in px.iter_mut() {
                *c *= k;
            }
        }
    });
    b
}

pub fn specs() -> Vec<EffectSpec> {
    let dims = || popup(&["Horizontal and Vertical", "Horizontal", "Vertical"]);
    vec![
        spec(
            "ec.blur.gaussian",
            "Gaussian Blur",
            "Blur & Sharpen",
            vec![
                p("blurriness", "Blurriness", num(0.0), slider(0.0, 3000.0, 0.0, 50.0, 1)),
                p("dimensions", "Blur Dimensions", Value::Enum(0), dims()),
                p("repeatEdge", "Repeat Edge Pixels", Value::Bool(false), ParamUi::Checkbox),
            ],
            gaussian,
        ),
        spec(
            "ec.blur.fastbox",
            "Fast Box Blur",
            "Blur & Sharpen",
            vec![
                p("radius", "Blur Radius", num(0.0), slider(0.0, 3000.0, 0.0, 50.0, 1)),
                p("iterations", "Iterations", num(3.0), slider(1.0, 50.0, 1.0, 5.0, 0)),
                p("dimensions", "Blur Dimensions", Value::Enum(0), dims()),
                p("repeatEdge", "Repeat Edge Pixels", Value::Bool(false), ParamUi::Checkbox),
            ],
            box_blur,
        ),
        spec(
            "ec.blur.directional",
            "Directional Blur",
            "Blur & Sharpen",
            vec![p("direction", "Direction", num(0.0), ParamUi::Angle), p("length", "Blur Length", num(0.0), slider(0.0, 1000.0, 0.0, 100.0, 1))],
            directional,
        ),
        spec(
            "ec.blur.radial",
            "Radial Blur",
            "Blur & Sharpen",
            vec![
                p("amount", "Amount", num(10.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("center", "Center", Value::Vec2([0.5, 0.5]), ParamUi::Point),
                p("type", "Type", Value::Enum(0), popup(&["Spin", "Zoom"])),
                // Our blur is always fully anti-aliased; the choice is kept for compatibility.
                p("antialiasing", "Antialiasing (Best Quality)", Value::Enum(0), popup(&["Low", "High"])),
            ],
            radial,
        ),
        spec("ec.blur.sharpen", "Sharpen", "Blur & Sharpen", vec![p("amount", "Sharpen Amount", num(0.0), slider(0.0, 500.0, 0.0, 100.0, 0))], sharpen),
        spec(
            "ec.blur.unsharp",
            "Unsharp Mask",
            "Blur & Sharpen",
            vec![
                p("amount", "Amount", num(50.0), slider(0.0, 500.0, 0.0, 500.0, 0)),
                p("radius", "Radius", num(1.0), slider(0.1, 500.0, 0.1, 50.0, 1)),
                p("threshold", "Threshold", num(0.0), slider(0.0, 255.0, 0.0, 255.0, 0)),
            ],
            unsharp,
        ),
        spec(
            "ec.stylize.glow",
            "Glow",
            "Stylize",
            vec![
                p("based", "Glow Based On", Value::Enum(1), popup(&["Alpha Channel", "Color Channels"])),
                p("threshold", "Glow Threshold", num(60.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("radius", "Glow Radius", num(10.0), slider(0.0, 1000.0, 0.0, 100.0, 1)),
                p("intensity", "Glow Intensity", num(1.0), slider(0.0, 255.0, 0.0, 4.0, 1)),
                // AE's default, Behind: the original behind the glow (see `glow_composite`).
                p("operation", "Composite Original", Value::Enum(1), popup(&["On Top", "Behind", "None"])),
                p("glowOperation", "Glow Operation", Value::Enum(1), popup(&GLOW_OPERATIONS)),
                p("colors", "Glow Colors", Value::Enum(0), popup(&["Original Colors", "A & B Colors", "Arbitrary Map"])),
                // Arbitrary Map: "red points | green points | blue points" (Curves format).
                p("arbitraryMap", "Arbitrary Map", Value::Str(String::new()), ParamUi::Hidden),
                p("colorLooping", "Color Looping", Value::Enum(2), popup(&["Sawtooth A>B", "Sawtooth B>A", "Triangle A>B>A", "Triangle B>A>B"])),
                p("colorLoops", "Color Loops", num(1.0), slider(1.0, 127.0, 1.0, 10.0, 1)),
                p("colorPhase", "Color Phase", num(0.0), ParamUi::Angle),
                p("abMidpoint", "A & B Midpoint", num(50.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("colorA", "Color A", col(1.0, 1.0, 1.0), ParamUi::Color),
                p("colorB", "Color B", col(0.0, 0.0, 0.0), ParamUi::Color),
                p("glowDimensions", "Glow Dimensions", Value::Enum(0), dims()),
            ],
            glow,
        ),
        spec(
            "ec.stylize.deepglow",
            "Deep Glow (Native)",
            "Stylize",
            vec![
                p("PEDG-0001", "Radius", num(116.0), slider(0.0, 2000.0, 0.0, 500.0, 0)),
                p("glowExposure", "Exposure", num(0.04), slider(0.0, 128.0, 0.0, 4.0, 3)),
                p("PEDG-0004", "Threshold", num(0.0), slider(0.0, 1600.0, 0.0, 100.0, 0)),
                p("PEDG-0005", "Threshold Smooth", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PEDG-0008", "Blend Mode", Value::Enum(0), popup(&["Screen", "Add"])),
                p("PEDG-0009", "Gamma Correction", Value::Bool(false), ParamUi::Checkbox),
                p("PEDG-0010", "Aspect Ratio", num(1.0), slider(0.0, 2.0, 0.0, 2.0, 2)),
                p("PEDG-0011", "Spread", num(33.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PEDG-0019", "Downsample", num(80.0), slider(10.0, 100.0, 10.0, 100.0, 0)),
                p("PEDG-0025", "Unmult (Generate Alpha)", Value::Bool(true), ParamUi::Checkbox),
                p("PEDG-0031", "Dither", Value::Bool(true), ParamUi::Checkbox),
                p("PEDG-0032", "Monochromatic Dither", Value::Bool(false), ParamUi::Checkbox),
                p("PEDG-0033", "Dither Amount", num(50.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PEDG-0055", "Edge Behaviour", num(1.0), slider(0.0, 2.0, 0.0, 2.0, 0)),
                p("PEDG-0084", "Buffer Expansion", Value::Enum(0), popup(&["Auto", "Compatibility"])),
                p("PEDG-0013", "Chromatic Aberration", Value::Bool(false), ParamUi::Checkbox),
                p("PEDG-0015", "Chromatic Amount", num(0.25), slider(0.0, 2.0, 0.0, 2.0, 2)),
                p("PEDG-0037", "Tint", Value::Bool(false), ParamUi::Checkbox),
                p("PEDG-0038", "Tint Color", col(1.0, 0.0, 0.0), ParamUi::Color),
                p("PEDG-0039", "Tint Mode", Value::Enum(0), popup(&["Standard", "Overlay", "Soft Light"])),
                p("PEDG-0041", "Tint Mix", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("PEDG-0043", "Auto Detect Gamma", Value::Bool(true), ParamUi::Checkbox),
                p("PEDG-0056", "Mask Layer", Value::Layer(None), ParamUi::Layer),
                p("PEDG-0057", "Mask Mode", Value::Enum(0), popup(&["Alpha", "Luminance"])),
                p("PEDG-0059", "Invert Mask", Value::Bool(false), ParamUi::Checkbox),
                p("PEDG-0023", "View", Value::Enum(1), popup(&["Glow Input", "Final"])),
                p("PEDG-0024", "Source Opacity", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
            ],
            deep_glow,
        ),
        spec(
            "ec.stylize.mosaic",
            "Mosaic",
            "Stylize",
            vec![
                p("horizontal", "Horizontal Blocks", num(10.0), slider(1.0, 4000.0, 1.0, 200.0, 0)),
                p("vertical", "Vertical Blocks", num(10.0), slider(1.0, 4000.0, 1.0, 200.0, 0)),
                p("sharpColors", "Sharp Colors", Value::Bool(false), ParamUi::Checkbox),
            ],
            mosaic,
        ),
        spec("ec.stylize.posterize", "Posterize", "Stylize", vec![p("level", "Level", num(6.0), slider(2.0, 255.0, 2.0, 32.0, 0))], posterize),
        spec("ec.stylize.threshold", "Threshold", "Stylize", vec![p("level", "Level", num(128.0), slider(0.0, 255.0, 0.0, 255.0, 0))], threshold),
        spec(
            "ec.stylize.findedges",
            "Find Edges",
            "Stylize",
            vec![p("invert", "Invert", Value::Bool(false), ParamUi::Checkbox), p("blend", "Blend With Original", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0))],
            find_edges,
        ),
        spec(
            "ec.stylize.emboss",
            "Emboss",
            "Stylize",
            vec![
                p("direction", "Direction", num(45.0), ParamUi::Angle),
                p("relief", "Relief", num(1.5), slider(0.0, 10.0, 0.0, 10.0, 2)),
                p("contrast", "Contrast", num(100.0), slider(0.0, 500.0, 0.0, 500.0, 0)),
                p("blend", "Blend With Original", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
            ],
            emboss,
        ),
        spec(
            "ec.perspective.dropshadow",
            "Drop Shadow",
            "Perspective",
            vec![
                p("color", "Shadow Color", col(0.0, 0.0, 0.0), ParamUi::Color),
                p("opacity", "Opacity", num(50.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("direction", "Direction", num(135.0), ParamUi::Angle),
                p("distance", "Distance", num(5.0), slider(0.0, 32000.0, 0.0, 120.0, 1)),
                p("softness", "Softness", num(0.0), slider(0.0, 1000.0, 0.0, 250.0, 1)),
                p("shadowOnly", "Shadow Only", Value::Bool(false), ParamUi::Checkbox),
            ],
            drop_shadow,
        ),
        spec(
            "ec.channel.invert",
            "Invert",
            "Channel",
            vec![
                p("channel", "Channel", Value::Enum(0), popup(&INVERT_CHANNELS)),
                p("blend", "Blend With Original", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
            ],
            invert,
        ),
        spec(
            "ec.noise.noise",
            "Noise",
            "Noise & Grain",
            vec![
                p("amount", "Amount of Noise", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("color", "Use Color Noise", Value::Bool(true), ParamUi::Checkbox),
                p("clip", "Clip Result Values", Value::Bool(true), ParamUi::Checkbox),
            ],
            noise,
        ),
        spec(
            "ec.transition.linearwipe",
            "Linear Wipe",
            "Transition",
            vec![
                p("completion", "Transition Completion", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("angle", "Wipe Angle", num(90.0), ParamUi::Angle),
                p("feather", "Feather", num(0.0), slider(0.0, 32000.0, 0.0, 500.0, 1)),
            ],
            linear_wipe,
        ),
        spec(
            "ec.transition.radialwipe",
            "Radial Wipe",
            "Transition",
            vec![
                p("completion", "Transition Completion", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("startAngle", "Start Angle", num(0.0), ParamUi::Angle),
                p("center", "Wipe Center", Value::Vec2([0.5, 0.5]), ParamUi::Point),
                p("wipe", "Wipe", Value::Enum(0), popup(&["Clockwise", "Counterclockwise", "Both"])),
                p("feather", "Feather", num(0.0), slider(0.0, 360.0, 0.0, 50.0, 1)),
            ],
            radial_wipe,
        ),
        spec(
            "ec.transition.venetian",
            "Venetian Blinds",
            "Transition",
            vec![
                p("completion", "Transition Completion", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 0)),
                p("direction", "Direction", num(90.0), ParamUi::Angle),
                p("width", "Width", num(30.0), slider(2.0, 1000.0, 2.0, 200.0, 0)),
                p("feather", "Feather", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
            ],
            venetian,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectEnv, Params, run_fx};

    /// Linear Wipe's feather against After Effects 2026 (LW1): alpha along y = 540 of a
    /// 1920 × 1080 layer, read from AE's own frames. `S10 Shade L`: completion 50.598, angle
    /// 270, feather 866.34 (AE values / 217, its 85 % opacity); footage: completion 25, angle 90,
    /// feather 400 (layer x); a 400 px layer at 1 %, feather 40, where AE weakens the wipe
    /// near its start. The old linear ramp was off by up to 40 levels.
    #[test]
    fn linear_wipe_matches_after_effects_profile() {
        let alpha_row = |w: u32, vals: &[(&str, Value)]| {
            let out = run_fx("ec.transition.linearwipe", vals, effectcraft_raster::Image::filled(w, 2, [1.0; 4]), 0.0, EffectEnv::default());
            (0..w as i64).map(|x| out.img.get(x, 1)[3] as f64).collect::<Vec<_>>()
        };
        let n = Value::Scalar;
        let shade = alpha_row(1920, &[("completion", n(50.59765625)), ("angle", n(270.0)), ("feather", n(866.3411458333335))]);
        for (x, ae) in [(0, 217.0), (300, 213.0), (600, 186.0), (960, 106.0), (1200, 49.0), (1500, 9.0), (1800, 0.0)] {
            let got = shade[x] * 217.0;
            assert!((got - ae).abs() <= 1.5, "shade x {x}: {got:.2} vs AE {ae}");
        }
        let foot = alpha_row(1920, &[("completion", n(25.0)), ("angle", n(90.0)), ("feather", n(400.0))]);
        for (x, ae) in [(0, 0.0), (120, 1.0), (300, 30.0), (480, 128.0), (660, 225.0), (900, 255.0), (1919, 255.0)] {
            let got = foot[x] * 255.0;
            assert!((got - ae).abs() <= 1.5, "footage x {x}: {got:.2} vs AE {ae}");
        }
        let start = alpha_row(400, &[("completion", n(1.0)), ("angle", n(90.0)), ("feather", n(40.0))]);
        for (x, ae) in [(0, 168.0), (8, 198.0), (20, 235.0), (40, 254.0)] {
            let got = start[x] * 255.0;
            assert!((got - ae).abs() <= 2.0, "1 % x {x}: {got:.2} vs AE {ae}");
        }
        // 0 % leaves the layer whole and 100 % clears it, even with a feather wider than it.
        let whole = alpha_row(400, &[("completion", n(0.0)), ("angle", n(90.0)), ("feather", n(1000.0))]);
        let clear = alpha_row(400, &[("completion", n(100.0)), ("angle", n(90.0)), ("feather", n(1000.0))]);
        assert!(whole.iter().all(|a| *a == 1.0) && clear.iter().all(|a| *a == 0.0));
    }

    /// The wipe travels across the layer's own rectangle, not the effect buffer: a buffer
    /// padded by an earlier effect (layer origin at x = 300) keeps its transparent margin
    /// transparent and puts the footage case's half-way point at layer x 480, as AE does.
    #[test]
    fn linear_wipe_stays_inside_the_layer_of_a_padded_buffer() {
        let params = Params {
            values: [("completion", 25.0), ("angle", 90.0), ("feather", 400.0)].into_iter().map(|(k, v)| (k.to_string(), Value::Scalar(v))).collect(),
        };
        let ctx = EffectCtx { params: &params, time: 0.0, layer_size: [1920.0, 2.0], seed: 1, adjustment: false, env: EffectEnv::default() };
        let mut img = effectcraft_raster::Image::new(2520, 2);
        for x in 300..2220 {
            img.set(x, 1, [1.0; 4]);
        }
        let out = linear_wipe(&ctx, Buf { img, offset: [300.0, 0.0], scale: 1.0 });
        let a = |x: i64| out.img.get(x, 1)[3] as f64;
        assert!((0..300).chain(2220..2520).all(|x| a(x) == 0.0), "alpha outside the layer");
        assert!((a(300 + 480) * 255.0 - 128.0).abs() <= 1.5, "{}", a(780) * 255.0);
        assert!((a(300 + 900) * 255.0 - 255.0).abs() <= 1.0);
    }

    /// A bright square on mid grey.
    fn scene() -> effectcraft_raster::Image {
        let mut img = effectcraft_raster::Image::filled(24, 24, [0.3, 0.3, 0.3, 1.0]);
        for y in 8..16 {
            for x in 8..16 {
                img.set(x, y, [1.0, 1.0, 1.0, 1.0]);
            }
        }
        img
    }

    /// Spread (standard deviation, layer px) of a blurred 1-px-wide line along x and y.
    fn blur_spread(id: &str, vals: &[(&str, Value)], env: EffectEnv) -> (f64, f64) {
        let mut img = effectcraft_raster::Image::new(161, 161);
        for i in 0..161 {
            img.set(80, i, [1.0; 4]);
            img.set(i, 80, [1.0; 4]);
        }
        let out = run_fx(id, vals, img, 0.0, env);
        let sd = |along_x: bool| {
            let (mut w, mut m, mut m2) = (0.0, 0.0, 0.0);
            for i in 0..out.img.width.min(out.img.height) as i64 {
                // Through the line's crossing-free part: 40 px from the other line.
                let (x, y) = if along_x { (i, 40 + out.offset[1] as i64) } else { (40 + out.offset[0] as i64, i) };
                let a = out.img.get(x, y)[3] as f64;
                let c = i as f64;
                w += a;
                m += a * c;
                m2 += a * c * c;
            }
            (m2 / w - (m / w).powi(2)).max(0.0).sqrt()
        };
        (sd(true), sd(false))
    }

    /// Gaussian Blur's σ is 0.28 × Blurriness (measured on After Effects 2026), and on a layer
    /// whose effects run in comp space (Continuously Rasterize shape / text: Billiard Balls'
    /// highlight, an ellipse scaled 305 % × 241 % with Blurriness 77.9) Blurriness is in comp
    /// pixels: AE blurred that highlight by σ ≈ 21.5 comp px in both directions, where a layer
    /// space blur spread it 118 × 93 px into a faint glow.
    #[test]
    fn gaussian_blur_sigma_and_comp_space() {
        let vals = [("blurriness", Value::Scalar(20.0))];
        let (sx, sy) = blur_spread("ec.blur.gaussian", &vals, EffectEnv::default());
        assert!((sx - 5.6).abs() < 0.3 && (sy - 5.6).abs() < 0.3, "{sx} {sy}");
        // Scaled 300 % × 100 % to comp: a third of the spread along x in layer pixels.
        let env = EffectEnv { comp_space: Some([[3.0, 0.0, 7.0], [0.0, 1.0, -2.0]]), ..Default::default() };
        let (sx, sy) = blur_spread("ec.blur.gaussian", &vals, env);
        assert!((sx - 5.6 / 3.0).abs() < 0.3 && (sy - 5.6).abs() < 0.3, "{sx} {sy}");
        // Fast Box Blur follows comp space too.
        let vals = [("radius", Value::Scalar(12.0)), ("iterations", Value::Scalar(1.0))];
        let (bx, by) = blur_spread("ec.blur.fastbox", &vals, env);
        assert!(bx * 2.5 < by, "{bx} {by}");
    }

    /// Comp-pixel footprint: uniform scale with rotation is isotropic; a degenerate matrix
    /// falls back to layer pixels.
    #[test]
    fn comp_px_of_rotated_and_degenerate_layers() {
        let (c, s) = (30f64.to_radians().cos() * 2.0, 30f64.to_radians().sin() * 2.0);
        let env = EffectEnv { comp_space: Some([[c, -s, 0.0], [s, c, 0.0]]), ..Default::default() };
        let [kx, ky] = env.comp_px();
        assert!((kx - 0.5).abs() < 1e-9 && (ky - 0.5).abs() < 1e-9, "{kx} {ky}");
        let env = EffectEnv { comp_space: Some([[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]), ..Default::default() };
        assert_eq!(env.comp_px(), [1.0, 1.0]);
        let env = EffectEnv { comp_space: Some([[f64::NAN, 0.0, 0.0], [0.0, 1.0, 0.0]]), ..Default::default() };
        assert_eq!(env.comp_px(), [1.0, 1.0]);
    }

    /// Noise with Clip Result Values off never wraps a bright pixel to black (SceneYard Drone
    /// History infographics: 10% mono noise on cream paper turned it cyan).
    #[test]
    fn unclipped_noise_does_not_wrap() {
        let img = effectcraft_raster::Image::filled(32, 32, [0.98, 0.95, 0.92, 1.0]);
        let vals = [("amount", Value::Scalar(10.0)), ("color", Value::Bool(false)), ("clip", Value::Bool(false))];
        let out = run_fx("ec.noise.noise", &vals, img, 0.0, EffectEnv::default());
        // Within ±5% of the input everywhere: no pixel jumped across the range.
        assert!(out.img.data.iter().all(|p| (p[0] - 0.98).abs() <= 0.051 && (p[2] - 0.92).abs() <= 0.051), "wrapped noise");
        assert!(out.img.data.iter().any(|p| p[0] > 1.0), "unclipped noise may go over range");
    }

    #[test]
    fn glow_operation_and_arbitrary_map() {
        // Pixels addressed in layer space (Glow pads its buffer).
        struct Out(Buf);
        impl Out {
            fn get(&self, x: i64, y: i64) -> [f32; 4] {
                self.0.img.get(x + self.0.offset[0] as i64, y + self.0.offset[1] as i64)
            }
        }
        let run = |vals: &[(&str, Value)]| Out(run_fx("ec.stylize.glow", vals, scene(), 0.0, EffectEnv::default()));
        let add = run(&[]);
        assert_eq!(GLOW_OPERATIONS[1], "Add");
        // Multiply darkens around the square where Add brightens.
        let mul = run(&[("glowOperation", Value::Enum(2))]);
        // Composite Original names where the original goes: On Top keeps the square as it was
        // and only shows the glow around it (aerender, SceneYard UI Envelope).
        let on_top = run(&[("operation", Value::Enum(0)), ("threshold", Value::Scalar(0.0))]);
        let behind = run(&[("threshold", Value::Scalar(0.0))]);
        let inside = scene().get(4, 4);
        assert!((on_top.get(4, 4)[0] - inside[0]).abs() < 1e-4, "{:?} {inside:?}", on_top.get(4, 4));
        assert!(behind.get(4, 4)[0] > inside[0] + 0.01, "{:?} {inside:?}", behind.get(4, 4));
        assert!(add.get(6, 12)[0] > 0.31 && mul.get(6, 12)[0] <= 0.3 + 1e-4, "{:?} {:?}", add.get(6, 12), mul.get(6, 12));
        assert_eq!(
            glow_operation(&EffectCtx {
                params: &crate::Params::default(),
                time: 0.0,
                layer_size: [1.0; 2],
                seed: 0,
                adjustment: false,
                env: Default::default()
            }),
            effectcraft_color::BlendMode::Add
        );
        // Arbitrary Map: the glow takes the mapped colour (here pure red for every brightness).
        let red = run(&[("colors", Value::Enum(2)), ("arbitraryMap", Value::Str("0,1 1,1|0,0 1,0|0,0 1,0".into()))]);
        let p = red.get(6, 12);
        assert!(p[0] > 0.35 && (p[1] - 0.3).abs() < 1e-4 && (p[2] - 0.3).abs() < 1e-4, "{p:?}");
    }

    /// Measured with aerender on SceneYard UI Envelope's card (Composite Original None, which
    /// draws like Behind): an edge whose channels clear the threshold glows like a normalised
    /// Gaussian of a full-strength step with sigma = radius / 2.75 (half strength at the edge,
    /// 0.42 / 0.34 / 0.20 / 0.05 at 8 / 16 / 32 / 64 px for Radius 100), channels under the
    /// threshold do not glow, and the A & B colours run across the falloff from Color A where the
    /// glow is strong.
    #[test]
    fn glow_matches_ae_falloff_and_ab_colours() {
        let (w, h) = (360, 40);
        let mut img = Image::new(w, h);
        for y in 0..h {
            for x in 0..180 {
                img.set(x, y, [0.878, 0.937, 0.11, 1.0]);
            }
        }
        let run = |vals: &[(&str, Value)]| {
            let mut v = vec![("threshold", Value::Scalar(60.0)), ("radius", Value::Scalar(100.0)), ("glowDimensions", Value::Enum(1))];
            v.extend_from_slice(vals);
            run_fx("ec.stylize.glow", &v, img.clone(), 0.0, EffectEnv::default())
        };
        let out = run(&[]);
        let at = |b: &Buf, x: i64| b.img.get(x + b.offset[0] as i64, 20 + b.offset[1] as i64);
        for (d, ae) in [(8, 0.418), (16, 0.341), (32, 0.204), (64, 0.047)] {
            let p = at(&out, 179 + d);
            assert!((p[0] - ae).abs() < 0.03 && (p[1] - ae).abs() < 0.03 && p[2] < 1e-4, "{d}: {p:?}");
        }
        // The soft threshold: a channel just under it still glows a little, well under it not at all.
        assert!((glow_bright(0.698, 0.702) - 0.47).abs() < 0.03 && (glow_bright(0.698, 0.651) - 0.89).abs() < 0.03);
        assert_eq!(glow_bright(0.11, 0.6), 0.0);
        assert_eq!(glow_bright(0.0, 0.0), 0.0);
        // A & B Colors, Sawtooth A>B with red A and blue B: half-way at the edge, blue far out.
        let ab = run(&[
            ("colors", Value::Enum(1)),
            ("colorLooping", Value::Enum(0)),
            ("colorA", Value::Color([1.0, 0.0, 0.0, 1.0])),
            ("colorB", Value::Color([0.0, 0.0, 1.0, 1.0])),
        ]);
        let near = at(&ab, 181);
        let far = at(&ab, 240);
        assert!(near[0] > 0.9 * near[2] && far[2] > 10.0 * far[0], "{near:?} {far:?}");
        // Triangle A>B>A from black A to white B on an opaque dark shape glowing by its alpha
        // (SceneYard phone animation's main rectangle): the inside stays dark, the edge glows.
        let dark = Image::filled(w, h, [0.0, 0.0, 0.0, 0.0]);
        let mut dark = dark;
        for y in 0..h {
            for x in 0..180 {
                dark.set(x, y, [0.1, 0.1, 0.1, 1.0]);
            }
        }
        let v = [
            ("based", Value::Enum(0)),
            ("threshold", Value::Scalar(57.6)),
            ("radius", Value::Scalar(10.0)),
            ("colors", Value::Enum(1)),
            ("colorLooping", Value::Enum(2)),
            ("colorA", Value::Color([1.0, 1.0, 1.0, 1.0])),
            ("colorB", Value::Color([0.0, 0.0, 0.0, 1.0])),
        ];
        let rect = run_fx("ec.stylize.glow", &v, dark, 0.0, EffectEnv::default());
        assert!(at(&rect, 90)[0] < 0.11, "{:?}", at(&rect, 90));
        assert!(at(&rect, 180)[0] > 0.3, "{:?}", at(&rect, 180));
    }

    /// Measured with aerender (SceneYard Music app's pill, a 217 px wide dark red bar, Deep
    /// Glow Radius 100, Exposure 1, Add, gamma correction on): the glow lifts the inside to
    /// about (253, 162, 204) and reaches 116 / 63 / 28 (red) at 4 / 12 / 22 px outside.
    #[test]
    fn deep_glow_matches_deep_glow_2_on_a_white_bar() {
        // After Effects 2026, Deep Glow 2 defaults (Screen, Unmult, Auto Detect Gamma) on a
        // 200 x 24 white bar over black, 8 bpc: the values below its bottom edge, d px out.
        let mut img = Image::new(640, 360);
        for y in 168..192 {
            for x in 220..420 {
                img.set(x, y, [1.0; 4]);
            }
        }
        let cases: [(f64, f64, [f32; 6]); 3] = [
            (282.0, 0.04, [78.0, 65.0, 56.0, 45.0, 32.0, 15.0]),
            (100.0, 0.5, [191.0, 182.0, 126.0, 74.0, 24.0, 0.0]),
            (25.0, 1.0, [232.0, 205.0, 63.0, 10.0, 0.0, 0.0]),
        ];
        for (radius, exposure, ae) in cases {
            let set = [("PEDG-0001", num(radius)), ("glowExposure", num(exposure)), ("PEDG-0031", Value::Bool(false))];
            let o = run_fx("ec.stylize.deepglow", &set, img.clone(), 0.0, EffectEnv::default());
            for (d, want) in [0, 1, 7, 15, 31, 63].into_iter().zip(ae) {
                let v = o.img.get(320 + o.offset[0] as i64, 192 + d + o.offset[1] as i64)[0] * 255.0;
                // The old v1 fit put a hot ring at the edge (Radius 282 / Exposure 0.04: 131 vs 78).
                assert!((v - want).abs() < 24.0, "Radius {radius} Exposure {exposure}, {d} px out: {v} vs AE {want}");
            }
        }
    }

    #[test]
    fn deep_glow_renders_editable_halo() {
        assert_eq!(crate::lookup("PEDG").map(|s| s.id), Some("ec.stylize.deepglow"));
        let mut img = Image::new(17, 17);
        img.set(8, 8, [1.0, 0.25, 0.1, 1.0]);
        let run = |vals: &[(&str, Value)]| run_fx("ec.stylize.deepglow", vals, img.clone(), 0.0, EffectEnv::default());
        let base = run(&[("PEDG-0001", num(8.0))]);
        let pixel = |b: &Buf, x: i64, y: i64| b.img.get(x + b.offset[0] as i64, y + b.offset[1] as i64);
        assert!(pixel(&base, 11, 8)[0] > 0.0, "radius must create a halo");
        assert!(pixel(&base, 11, 8)[0] > pixel(&base, 11, 8)[1], "halo keeps source hue");
        let brighter = run(&[("PEDG-0001", num(8.0)), ("glowExposure", num(4.0))]);
        assert!(pixel(&brighter, 11, 8)[0] > pixel(&base, 11, 8)[0], "exposure must brighten halo");
        let cut = run(&[("PEDG-0001", num(8.0)), ("PEDG-0004", num(200.0))]);
        assert_eq!(pixel(&cut, 11, 8)[0], 0.0, "threshold must remove dim sources");
        let source = run(&[("PEDG-0001", num(8.0)), ("PEDG-0023", Value::Enum(0))]);
        assert_eq!(pixel(&source, 11, 8)[0], 0.0, "input view must omit halo");
        // Downsample blurs the wide passes (24 px and more) at its size.
        let wide = run(&[("PEDG-0001", num(400.0))]);
        let lower_quality = run(&[("PEDG-0001", num(400.0)), ("PEDG-0019", num(75.0))]);
        assert_ne!(pixel(&lower_quality, 40, 8), pixel(&wide, 40, 8), "Downsample must change the broad halo");
        let clipped = run(&[("PEDG-0001", num(8.0)), ("PEDG-0055", Value::Enum(0))]);
        assert_eq!(clipped.offset, [0.0, 0.0], "edge mode 0 keeps layer bounds");
        let compatible = run(&[("PEDG-0001", num(8.0)), ("PEDG-0084", Value::Enum(1))]);
        assert!(compatible.offset[0] > base.offset[0], "compatibility buffer expands farther");
        // The AEP importer converts one-based AE enum values to zero-based native indices.
        let native = |raw: u32| Value::Enum(raw - 1);
        let corpus = run(&[("PEDG-0001", num(8.0)), ("PEDG-0008", native(1)), ("PEDG-0023", native(2)), ("PEDG-0055", native(2)), ("PEDG-0084", native(1))]);
        assert_eq!(corpus.offset, base.offset);
        assert!(pixel(&corpus, 11, 8)[0] > 0.0, "AE View 2 must render final glow");
        let mut dim = Image::new(17, 17);
        dim.set(8, 8, [0.2, 0.05, 0.02, 1.0]);
        let dim_run = |mode| run_fx("ec.stylize.deepglow", &[("PEDG-0001", num(8.0)), ("PEDG-0008", native(mode))], dim.clone(), 0.0, EffectEnv::default());
        let (dim_add, dim_screen) = (dim_run(2), dim_run(1));
        assert!(pixel(&dim_add, 8, 8)[0] > pixel(&dim_screen, 8, 8)[0], "AE Blend Mode 2 (Add) lifts the source more than Screen");
        let low_auto = run(&[("PEDG-0001", num(8.0)), ("glowExposure", num(0.74))]);
        let low_linear = run(&[("PEDG-0001", num(8.0)), ("glowExposure", num(0.74)), ("PEDG-0043", Value::Bool(false))]);
        assert_ne!(pixel(&low_auto, 11, 8), pixel(&low_linear, 11, 8), "gamma correction changes the glow model");
        let low_manual = run(&[("PEDG-0001", num(8.0)), ("glowExposure", num(0.74)), ("PEDG-0043", Value::Bool(false)), ("PEDG-0009", Value::Bool(true))]);
        assert_eq!(pixel(&low_auto, 11, 8), pixel(&low_manual, 11, 8), "manual gamma can replace auto detection");
        let tint = [("PEDG-0001", num(8.0)), ("PEDG-0037", Value::Bool(true)), ("PEDG-0038", Value::Color([0.8, 0.2, 0.5, 1.0]))];
        let standard = run(&[tint[0].clone(), tint[1].clone(), tint[2].clone(), ("PEDG-0039", native(1))]);
        let overlay = run(&[tint[0].clone(), tint[1].clone(), tint[2].clone(), ("PEDG-0039", native(2))]);
        assert_ne!(pixel(&standard, 11, 8), pixel(&overlay, 11, 8), "Tint Mode must alter the glow");
        let host = crate::channel2::tests::FakeHost(Image::new(17, 17));
        let masked = |invert| {
            run_fx(
                "ec.stylize.deepglow",
                &[("PEDG-0001", num(8.0)), ("PEDG-0056", Value::Layer(Some(7))), ("PEDG-0059", Value::Bool(invert))],
                img.clone(),
                0.0,
                EffectEnv { host: Some(&host), ..Default::default() },
            )
        };
        assert_eq!(pixel(&masked(false), 11, 8)[0], 0.0, "transparent mask must suppress the glow");
        assert!(pixel(&masked(true), 11, 8)[0] > 0.0, "inverted mask restores the glow");

        let mut impulse = Image::new(9, 9);
        impulse.set(4, 4, [1.0; 4]);
        let wide = run_fx("ec.stylize.deepglow", &[("PEDG-0001", num(320.0))], impulse.clone(), 0.0, EffectEnv::default());
        // The halo spans a fraction of the radius (AE: a 439 px glow fades out ~200 px past an edge).
        assert!(pixel(&wide, 64, 4)[0] > 0.0, "a 320 px radius reaches 60 px");
        let opaque = run_fx("ec.stylize.deepglow", &[("PEDG-0001", num(8.0)), ("PEDG-0025", Value::Bool(false))], impulse, 0.0, EffectEnv::default());
        assert_eq!(pixel(&opaque, 7, 7)[3], 1.0, "Unmult off keeps a black opaque backdrop");
        let gray = Image::filled(9, 9, [0.3, 0.3, 0.3, 1.0]);
        let dithered = run_fx("ec.stylize.deepglow", &[("PEDG-0001", num(8.0))], gray.clone(), 0.0, EffectEnv::default());
        let clean = run_fx("ec.stylize.deepglow", &[("PEDG-0001", num(8.0)), ("PEDG-0031", Value::Bool(false))], gray, 0.0, EffectEnv::default());
        assert!(dithered.img.data.iter().zip(&clean.img.data).any(|(a, b)| a != b), "Dither must affect opaque pixels");
    }

    #[test]
    fn deep_glow_unmult_alpha_is_glow_brightness() {
        // SceneYard "Magic button item reveal": a faint violet halo over near-white must stay
        // translucent; summing the passes' alpha made it an opaque dark-purple vignette.
        let mut img = Image::new(41, 41);
        for y in 15..26 {
            for x in 15..26 {
                img.set(x, y, [0.35, 0.15, 0.6, 1.0]);
            }
        }
        let set = [
            ("PEDG-0001", num(100.0)),
            ("glowExposure", num(0.8)),
            ("PEDG-0008", Value::Enum(0)),
            ("PEDG-0025", Value::Bool(true)),
            ("PEDG-0031", Value::Bool(false)),
            ("PEDG-0055", num(2.0)),
        ];
        let o = run_fx("ec.stylize.deepglow", &set, img, 0.0, EffectEnv::default());
        let px = |x: i64| o.img.get(x + o.offset[0] as i64, 20 + o.offset[1] as i64);
        let inside = px(20);
        assert!(inside[3] > 0.999, "the opaque source stays opaque: {inside:?}");
        for x in [27, 32, 45] {
            let h = px(x);
            let bright = h[0].max(h[1]).max(h[2]);
            assert!(bright > 0.01 && h[3] < 0.9, "{x}: a faint halo is translucent: {h:?}");
            assert!((h[3] - bright).abs() < 1e-5, "{x}: alpha follows the halo's brightness: {h:?}");
            assert!(h[2] > h[0] && h[0] > h[1], "{x}: unpremultiplied colour keeps the violet hue: {h:?}");
        }
    }

    /// A comp-sized frame (1920 × 1080 at `scale`) holding one anti-aliased ~200 px ring, like
    /// a shape layer's layer image.
    fn ring_frame(scale: f64) -> Image {
        let (w, h) = ((1920.0 * scale).round() as u32, (1080.0 * scale).round() as u32);
        let mut img = Image::new(w, h);
        let (cx, cy) = (w as f64 * 0.45, h as f64 * 0.55);
        let (outer, inner) = (100.0 * scale, 80.0 * scale);
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let a = ((outer - d + 0.5).clamp(0.0, 1.0) * (d - inner + 0.5).clamp(0.0, 1.0)) as f32;
                if a > 0.0 {
                    img.set(x, y, [0.95 * a, 0.45 * a, 0.12 * a, a]);
                }
            }
        }
        img
    }

    /// Deep Glow's defaults overridden by `vals` (Elias's case: Radius 150, Exposure 0.01).
    fn deep_glow_params(vals: &[(&str, Value)], size: [f64; 2]) -> crate::Params {
        let spec = crate::find("ec.stylize.deepglow");
        let mut params =
            crate::Params { values: spec.map(|s| s.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, size))).collect()).unwrap_or_default() };
        for (k, v) in [("PEDG-0001", num(150.0)), ("glowExposure", num(0.02))].iter().chain(vals) {
            assert!(params.values.contains_key(*k), "unknown param {k}");
            params.values.insert(k.to_string(), v.clone());
        }
        params
    }

    fn run_deep_glow(f: fn(&EffectCtx, Buf) -> Buf, params: &crate::Params, img: &Image, scale: f64, env: EffectEnv) -> Buf {
        let layer_size = [img.width as f64 / scale, img.height as f64 / scale];
        let ctx = EffectCtx { params, time: 0.0, layer_size, seed: 7, adjustment: false, env };
        f(&ctx, Buf { img: img.clone(), offset: [0.0, 0.0], scale })
    }

    /// Largest per-channel difference between the new and the reference Deep Glow.
    fn deep_glow_diff(vals: &[(&str, Value)], img: &Image, scale: f64, env: EffectEnv) -> f32 {
        let params = deep_glow_params(vals, [img.width as f64 / scale, img.height as f64 / scale]);
        let new = run_deep_glow(deep_glow, &params, img, scale, env);
        let old = run_deep_glow(deep_glow_reference, &params, img, scale, env);
        assert_eq!((new.img.width, new.img.height, new.offset), (old.img.width, old.img.height, old.offset));
        new.img.data.iter().zip(&old.img.data).flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs())).fold(0.0, f32::max)
    }

    /// The content-rectangle Deep Glow renders what the full-frame one did: on Elias's case
    /// (Radius 150, Exposure 0.01, defaults: Unmult, Dither, Auto Detect Gamma, Downsample 80,
    /// Edge Behaviour 1) at Full and Quarter resolution, and with the other options.
    #[test]
    fn deep_glow_matches_full_frame_reference() {
        // Float rounding, and the faintest tail past where the cropped blurs end (linear
        // ~1e-6, which the linear model's curve shows brighter than sRGB would): under half an
        // 8-bit level.
        const TOLERANCE: f32 = 2e-3;
        for scale in [1.0, 0.25] {
            let d = deep_glow_diff(&[], &ring_frame(scale), scale, EffectEnv::default());
            println!("scale {scale}: max abs diff {d:e}");
            assert!(d < TOLERANCE, "scale {scale}: {d}");
        }
        let small = ring_frame(0.3);
        let options: [&[(&str, Value)]; 11] = [
            &[("PEDG-0025", Value::Bool(false))],
            &[("PEDG-0031", Value::Bool(false)), ("PEDG-0008", Value::Enum(1)), ("glowExposure", num(3.0))],
            &[("PEDG-0043", Value::Bool(false))],
            &[("PEDG-0019", num(30.0)), ("PEDG-0010", num(0.4))],
            &[("PEDG-0019", num(100.0)), ("PEDG-0013", Value::Bool(true))],
            &[("PEDG-0001", num(1500.0)), ("glowExposure", num(2.0))],
            &[("PEDG-0004", num(60.0)), ("PEDG-0005", num(20.0))],
            &[("PEDG-0037", Value::Bool(true)), ("PEDG-0039", Value::Enum(2)), ("PEDG-0024", num(40.0))],
            &[("PEDG-0055", num(0.0))],
            &[("PEDG-0023", Value::Enum(0))],
            &[("PEDG-0004", num(1600.0))],
        ];
        for vals in options {
            let d = deep_glow_diff(vals, &small, 1.0, EffectEnv::default());
            assert!(d < TOLERANCE, "{vals:?}: {d}");
        }
        let d = deep_glow_diff(&[], &small, 1.0, EffectEnv { working_linear: true, ..Default::default() });
        assert!(d < TOLERANCE, "linear working space: {d}");
        // A Mask Layer (here an opaque one, inverted and not).
        let host = crate::channel2::tests::FakeHost(Image::filled(small.width, small.height, [1.0; 4]));
        for invert in [false, true] {
            let env = EffectEnv { host: Some(&host), ..Default::default() };
            let d = deep_glow_diff(&[("PEDG-0056", Value::Layer(Some(7))), ("PEDG-0059", Value::Bool(invert))], &small, 1.0, env);
            assert!(d < TOLERANCE, "mask invert {invert}: {d}");
        }
        // Content at the frame edge, and an empty layer.
        let mut edge = Image::new(300, 200);
        for y in 0..200 {
            for x in 0..12 {
                edge.set(x, y, [0.8, 0.8, 0.8, 1.0]);
            }
        }
        for vals in [&[("PEDG-0055", num(0.0))][..], &[]] {
            assert!(deep_glow_diff(vals, &edge, 1.0, EffectEnv::default()) < TOLERANCE);
        }
        assert_eq!(deep_glow_diff(&[], &Image::new(64, 48), 1.0, EffectEnv::default()), 0.0);
    }

    /// Timing of the new and the full-frame Deep Glow on Elias's case (run in release:
    /// `cargo test --release -p effectcraft-effects -- --ignored --nocapture deep_glow_bench`).
    #[test]
    #[ignore]
    fn deep_glow_bench() {
        let time = |f: fn(&EffectCtx, Buf) -> Buf, params: &crate::Params, img: &Image, scale: f64| {
            let mut ms: Vec<f64> = (0..7)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(run_deep_glow(f, params, img, scale, EffectEnv::default()));
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            ms.sort_by(f64::total_cmp);
            ms[ms.len() / 2]
        };
        for scale in [1.0, 0.5, 0.25] {
            // The layer image is comp-sized at every resolution (1920 × 1080 at Full).
            let img = ring_frame(scale);
            let params = deep_glow_params(&[], [1920.0, 1080.0]);
            let old = time(deep_glow_reference, &params, &img, scale);
            let new = time(deep_glow, &params, &img, scale);
            println!("deep_glow {}x{} scale {scale}: old {old:.2} ms, new {new:.2} ms ({:.1}x)", img.width, img.height, old / new);
        }
    }

    #[test]
    fn deep_glow_renders_radius_above_512() {
        let mut impulse = Image::new(9, 9);
        impulse.set(4, 4, [1.0; 4]);
        let wide = run_fx(
            "ec.stylize.deepglow",
            &[("PEDG-0001", num(1603.0)), ("glowExposure", num(2.0)), ("PEDG-0031", Value::Bool(false))],
            impulse,
            0.0,
            EffectEnv::default(),
        );
        assert_eq!(wide.offset, [1603.0; 2]);
        assert_eq!(wide.img.width, 3215);
        // The widest Gaussian's three boxes reach ~3.6 sigma (~580 px here); past that the
        // full-frame blur only left a 1e-10 running-sum residue.
        assert!(wide.img.get(2107, 1607)[0] > 1e-6, "the 1603 px glow reaches 500 px from its source");
    }

    #[test]
    fn glow_map_array_matches_collected_vec() {
        for spec in ["", "0,1 1,1|0,0 1,0|0,0 1,0", "0,0 0.5,0.8 1,1|0,1 1,0|", "|0,0 0.3,0.9 1,0.2", "a|b|c|d"] {
            let map = glow_map(spec);
            for k in 0..=1000 {
                let t = k as f32 / 1000.0;
                // The per-pixel Vec this replaced.
                let c = map.iter().map(|cv| cv.as_ref().map_or(t, |cv| cv.eval(t))).collect::<Vec<f32>>();
                let old = [c.first().copied().unwrap_or(t), c.get(1).copied().unwrap_or(t), c.get(2).copied().unwrap_or(t)];
                let new = map.each_ref().map(|cv| cv.as_ref().map_or(t, |cv| cv.eval(t)));
                assert_eq!(old.map(f32::to_bits), new.map(f32::to_bits), "{spec} {t}");
            }
        }
    }
}
