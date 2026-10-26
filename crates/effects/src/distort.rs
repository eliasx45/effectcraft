//! Distort effects: inverse-mapped warps (each output pixel samples the source).

use effectcraft_geom::{Mat3, vec2};
use effectcraft_keyframe::Value;
use effectcraft_project::ParamUi;
use effectcraft_raster::Image;
use rayon::prelude::*;

use crate::util::layer_rect;
use crate::{Buf, EffectCtx, EffectSpec, num, p, popup, slider};

fn spec(id: &'static str, name: &'static str, params: Vec<crate::ParamSpec>, render: crate::RenderFn) -> EffectSpec {
    EffectSpec { id, name, category: "Distort", params, render, gpu: false, float: true }
}

/// Resample through `f(x, y) -> source point` (pixel coordinates, centres at +0.5).
fn remap(src: &Image, repeat: bool, f: impl Fn(f64, f64) -> (f64, f64) + Sync) -> Image {
    let mut out = Image::new(src.width, src.height);
    out.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let (sx, sy) = f(x as f64 + 0.5, y as f64 + 0.5);
            *px = if repeat { src.sample_bilinear_clamped(sx, sy) } else { src.sample_bilinear(sx, sy) };
        }
    });
    out
}

/// Twirl's radius in buffer pixels: Twirl Radius is a percentage of the larger dimension of
/// the effect's input, so after a growing Motion Tile it spans the tiled canvas. The 1.035 is
/// measured: After Effects renders of Liquid Glass (2560 and 7680 px inputs) put the rim there.
pub fn twirl_radius(pct: f64, buf_w: u32, buf_h: u32) -> f64 {
    (pct / 100.0 * f64::from(buf_w.max(buf_h)) * 1.035).max(1.0)
}

/// Twirl's share of Angle at normalised distance `t` = 1 - d / radius: a smoothstep, so the
/// rotation eases out to nothing at the rim (fitted to After Effects at 90°, 360° and 720°).
#[inline]
pub(crate) fn twirl_falloff(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn twirl(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let ang = ctx.params.f("angle").to_radians();
    let r = twirl_radius(ctx.params.f("radius"), b.img.width, b.img.height);
    let c = b.to_px(ctx.params.v2("center"));
    b.img = remap(&b.img, false, |x, y| {
        let (dx, dy) = (x - c.0, y - c.1);
        let d = (dx * dx + dy * dy).sqrt();
        if d >= r {
            return (x, y);
        }
        let a = -ang * twirl_falloff(1.0 - d / r);
        let (s, co) = a.sin_cos();
        (c.0 + dx * co - dy * s, c.1 + dx * s + dy * co)
    });
    b
}

fn spherize(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let r = (ctx.params.f("radius") * b.scale).max(1.0);
    let c = b.to_px(ctx.params.v2("center"));
    b.img = remap(&b.img, false, |x, y| {
        let (dx, dy) = (x - c.0, y - c.1);
        let d = (dx * dx + dy * dy).sqrt();
        if d >= r || d == 0.0 {
            return (x, y);
        }
        let nd = d / r;
        let k = (nd.asin() / std::f64::consts::FRAC_PI_2) / nd;
        (c.0 + dx * k, c.1 + dy * k)
    });
    b
}

/// Bulge displacement at the centre per unit of Bulge Height (fraction of the radius).
pub const BULGE_STRENGTH: f64 = 0.35;

fn bulge(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let rx = (ctx.params.f("hradius") * b.scale).max(1.0);
    let ry = (ctx.params.f("vradius") * b.scale).max(1.0);
    let c = b.to_px(ctx.params.v2("center"));
    let h = ctx.params.f("height");
    // Taper Radius: higher values make the sides of the bulge shallower (a more peaked profile).
    let taper = 1.0 + ctx.params.f("taperRadius").max(0.0) / 100.0 * 3.0;
    let pin = ctx.params.b("pinAllEdges");
    let (lx, ly, lw, lh) = layer_rect(ctx, &b);
    let edge = (lw.min(lh) * 0.1).max(1.0);
    b.img = remap(&b.img, false, |x, y| {
        let (dx, dy) = ((x - c.0) / rx, (y - c.1) / ry);
        let d2 = dx * dx + dy * dy;
        if d2 >= 1.0 {
            return (x, y);
        }
        // Strength per unit Bulge Height, matched to After Effects renders (SceneYard Assistive
        // Touch, Paper Reveal, Liquid): 0.5 magnified the centre about a third too much.
        let mut amt = h * (1.0 - d2).powf(taper) * BULGE_STRENGTH;
        if pin {
            let ex = ((x - lx - 0.5).min(lx + lw - 0.5 - x) / edge).clamp(0.0, 1.0);
            let ey = ((y - ly - 0.5).min(ly + lh - 0.5 - y) / edge).clamp(0.0, 1.0);
            amt *= ex.min(ey);
        }
        let k = 1.0 - amt;
        (c.0 + dx * rx * k, c.1 + dy * ry * k)
    });
    b
}

/// Wave Warp's wave shapes ([`WAVE_TYPES`] order) at phase `t` (radians), in -1..1. `seed`
/// seeds the noise shapes.
fn wave_shape(kind: u32, t: f64, seed: u32) -> f64 {
    let tau = std::f64::consts::TAU;
    let ph = t.rem_euclid(tau) / tau;
    // A half circle over u in 0..1 (0 at the ends, 1 in the middle).
    let half = |u: f64| (1.0 - (2.0 * u - 1.0).powi(2)).max(0.0).sqrt();
    let noise = |i: f64| crate::util::hash1(i as i64 as u32, 0x5eed, seed) as f64 * 2.0 - 1.0;
    match kind {
        1 => {
            if ph < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        2 => 1.0 - 4.0 * (ph - 0.5).abs(),
        3 => 2.0 * ph - 1.0,
        // Circle: alternating upper and lower half circles.
        4 => {
            if ph < 0.5 {
                half(ph * 2.0)
            } else {
                -half(ph * 2.0 - 1.0)
            }
        }
        // Semicircle: a row of half circles bulging the same way.
        5 => 2.0 * half((ph * 2.0).fract()) - 1.0,
        // Uncircle: the concave gaps between half circles, meeting in cusps.
        6 => 1.0 - 2.0 * half((ph * 2.0).fract()),
        // Noise: a random level per half wave; Smooth Noise eases between the levels.
        7 => noise((t / tau * 2.0).floor()),
        8 => {
            let u = t / tau * 2.0;
            let (i, f) = (u.floor(), u - u.floor());
            let s = f * f * (3.0 - 2.0 * f);
            noise(i) * (1.0 - s) + noise(i + 1.0) * s
        }
        _ => t.sin(),
    }
}

pub(crate) const WAVE_TYPES: [&str; 9] = ["Sine", "Square", "Triangle", "Sawtooth", "Circle", "Semicircle", "Uncircle", "Noise", "Smooth Noise"];
pub(crate) const WAVE_PINNING: [&str; 9] =
    ["None", "All Edges", "Center", "Left Edge", "Top Edge", "Right Edge", "Bottom Edge", "Horizontal Edges", "Vertical Edges"];

/// Wave Warp pinning weight (0 = pinned, 1 = free) at buffer pixel (x, y) of the layer
/// rectangle `(x0, y0, w, h)`.
fn wave_pin(pin: u32, x: f64, y: f64, rect: (f64, f64, f64, f64)) -> f64 {
    let (x0, y0, w, h) = rect;
    // Measured between the outermost pixel centres, so pinned edges stay exactly in place.
    let (u, v) = (((x - x0 - 0.5) / (w - 1.0).max(1.0)).clamp(0.0, 1.0), ((y - y0 - 0.5) / (h - 1.0).max(1.0)).clamp(0.0, 1.0));
    let horiz = (v.min(1.0 - v) * 2.0).clamp(0.0, 1.0);
    let vert = (u.min(1.0 - u) * 2.0).clamp(0.0, 1.0);
    match pin {
        1 => horiz.min(vert),
        2 => (((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt() * 2.0).min(1.0),
        3 => u,
        4 => v,
        5 => 1.0 - u,
        6 => 1.0 - v,
        7 => horiz,
        8 => vert,
        _ => 1.0,
    }
}

/// Wave Warp's wave phase in cycles at input-buffer pixel (x, y): one wave (crest and trough)
/// spans twice Wave Width, measured from the input's top-left, and the waves move with time
/// (`time * speed` cycles subtracted). Fitted to After Effects renders of SceneYard Liquid
/// Glass (Sine, Square and Circle at three times): 1× Wave Width packed twice the waves and
/// drifted the wrong way.
pub fn wave_warp_cycles(x: f64, y: f64, dir: (f64, f64), width: f64, phase_deg: f64, time: f64, speed: f64) -> f64 {
    (x * dir.0 + y * dir.1) / (2.0 * width) + phase_deg / 360.0 - time * speed
}

fn wave_warp(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let h = ctx.params.f("height") * b.scale;
    let w = (ctx.params.f("width") * b.scale).max(1.0);
    let dir = ctx.params.f("direction").to_radians();
    let (speed, phase) = (ctx.params.f("speed"), ctx.params.f("phase"));
    let kind = ctx.params.e("waveType");
    let pin = ctx.params.e("pinning");
    let seed = ctx.seed ^ 0x3a7e;
    let (dx, dy) = (dir.sin(), -dir.cos());
    // The waves are anchored to the input's top-left, before the room for the displacement.
    let pad = if ctx.adjustment { 0.0 } else { h.abs().ceil() + 1.0 };
    if pad > 0.0 {
        b.pad(pad as u32);
    }
    let (x0, y0, lw, lh) = layer_rect(ctx, &b);
    let rect = (x0, y0, lw.max(1.0), lh.max(1.0));
    let time = ctx.time;
    b.img = remap(&b.img, false, |x, y| {
        // Waves travel along `dir`; displacement is perpendicular to it.
        let cyc = wave_warp_cycles(x - pad, y - pad, (dx, dy), w, phase, time, speed);
        let disp = wave_shape(kind, cyc * std::f64::consts::TAU, seed) * h * wave_pin(pin, x, y, rect);
        (x + dy * disp, y - dx * disp)
    });
    b
}

fn ripple(ctx: &EffectCtx, mut b: Buf) -> Buf {
    // Radius is a percentage of the layer: 100 from the layer centre reaches the edge.
    let r = (ctx.params.f("radius") / 100.0 * b.layer_w(ctx).max(b.layer_h(ctx)) * 0.5 * b.scale).max(1.0);
    let asymmetric = ctx.params.e("conversion") == 0;
    let c = b.to_px(ctx.params.v2("center"));
    let w = (ctx.params.f("waveWidth") * b.scale).max(1.0);
    let h = ctx.params.f("waveHeight") * b.scale;
    let phase = ctx.params.f("phase").to_radians() - ctx.time * ctx.params.f("speed") * std::f64::consts::TAU;
    b.img = remap(&b.img, false, |x, y| {
        let (dx, dy) = (x - c.0, y - c.1);
        let d = (dx * dx + dy * dy).sqrt();
        if d >= r || d == 0.0 {
            return (x, y);
        }
        let fall = 1.0 - d / r;
        let arg = d / w * std::f64::consts::TAU + phase;
        let o = arg.sin() * h * fall;
        // Asymmetric ripples also move sideways (a quarter wave out of step).
        let t = if asymmetric { arg.cos() * h * fall * 0.5 } else { 0.0 };
        let (ux, uy) = (dx / d, dy / d);
        (x + ux * o - uy * t, y + uy * o + ux * t)
    });
    b
}

fn mirror(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let c = b.to_px(ctx.params.v2("center"));
    let a = ctx.params.f("angle").to_radians();
    let n = (a.cos(), a.sin());
    b.img = remap(&b.img, false, |x, y| {
        let d = (x - c.0) * n.0 + (y - c.1) * n.1;
        if d > 0.0 { (x - 2.0 * d * n.0, y - 2.0 * d * n.1) } else { (x, y) }
    });
    b
}

fn offset(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let shift = ctx.params.v2("shift");
    let (w, h) = (b.img.width as f64, b.img.height as f64);
    let blend = ctx.params.f("blend") as f32 / 100.0;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (sx, sy) = b.to_px(shift);
    let orig = b.img.clone();
    let mut o = remap(&b.img, false, |x, y| ((x - (sx - cx)).rem_euclid(w), (y - (sy - cy)).rem_euclid(h)));
    if blend > 0.0 {
        for (p, q) in o.data.iter_mut().zip(&orig.data) {
            for i in 0..4 {
                p[i] += (q[i] - p[i]) * blend;
            }
        }
    }
    b.img = o;
    b
}

fn polar(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let amt = ctx.params.f("interpolation") / 100.0;
    let to_polar = ctx.params.e("conversion") == 1;
    let (w, h) = (b.img.width as f64, b.img.height as f64);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let rmax = cx.min(cy);
    b.img = remap(&b.img, true, |x, y| {
        let (px, py) = if to_polar {
            // output is polar: angle → x, radius → y
            let dx = x - cx;
            let dy = y - cy;
            let a = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU);
            let r = (dx * dx + dy * dy).sqrt() / rmax;
            (a / std::f64::consts::TAU * w, h - r * h)
        } else {
            let a = x / w * std::f64::consts::TAU;
            let r = (h - y) / h * rmax;
            (cx + r * a.sin(), cy - r * a.cos())
        };
        (x + (px - x) * amt, y + (py - y) * amt)
    });
    b
}

/// Transform's shutter: (angle, phase) in degrees and the number of samples, or None when it
/// renders without motion blur. Use Composition's Shutter Angle takes the comp's shutter
/// (only when the comp and layer motion-blur switches are on); otherwise Shutter Angle, centred
/// on the frame.
pub fn transform_shutter(ctx: &EffectCtx) -> Option<(f64, f64, u32)> {
    let (angle, phase, n) = if ctx.params.b("useCompositionShutterAngle") {
        ctx.env.shutter?
    } else {
        let a = ctx.params.f("shutterAngle").clamp(0.0, 720.0);
        (a, -a * 0.5, 16)
    };
    (angle > 0.0).then_some((angle, phase, n.clamp(2, 64)))
}

fn transform_matrix(pr: &crate::Params, b: &Buf) -> Mat3 {
    transform_effect_matrix(pr, b.to_px(pr.v2("anchor")), b.to_px(pr.v2("position")))
}

/// The Transform effect's matrix in buffer pixels (`anchor` / `position` already in pixels).
/// As in After Effects, the skew is applied after the rotation: Skew Axis 0 tilts horizontal
/// lines (a vertical shear), so Rotation -20° with Skew 20° gives a pure horizontal shear.
pub fn transform_effect_matrix(pr: &crate::Params, anchor: (f64, f64), pos: (f64, f64)) -> Mat3 {
    let sh = pr.f("scaleHeight");
    let sw = if pr.b("uniform") { sh } else { pr.f("scaleWidth") };
    Mat3::translate(vec2(pos.0, pos.1))
        * Mat3::skew_deg(-pr.f("skew"), pr.f("skewAxis") + 90.0)
        * Mat3::rotate_deg(pr.f("rotation"))
        * Mat3::scale(vec2(sw / 100.0, sh / 100.0))
        * Mat3::translate(vec2(-anchor.0, -anchor.1))
}

/// A "Children Follow" checkbox of the Transform effect: `default` when the instance predates it.
fn follow_on(pr: &crate::Params, id: &str, default: bool) -> bool {
    pr.values.get(id).map_or(default, Value::as_bool)
}

/// What a Transform effect passes on to its layer's children (Children Follow, not in After
/// Effects): the parts of its matrix they follow, in the same pixels as `anchor` / `pos`, and the
/// opacity factor they inherit. Position off keeps the effect's scale, rotation and skew about its
/// Anchor Point. `None` when Children Follow is off.
pub fn transform_follow(pr: &crate::Params, anchor: (f64, f64), pos: (f64, f64)) -> Option<(Mat3, f64)> {
    if !follow_on(pr, "follow", true) {
        return None;
    }
    let pos = if follow_on(pr, "followPosition", true) { pos } else { anchor };
    let (sw, sh) = if follow_on(pr, "followScale", true) {
        let sh = pr.f("scaleHeight");
        (if pr.b("uniform") { sh } else { pr.f("scaleWidth") }, sh)
    } else {
        (100.0, 100.0)
    };
    let rotation = if follow_on(pr, "followRotation", true) { pr.f("rotation") } else { 0.0 };
    let skew = if follow_on(pr, "followSkew", false) { pr.f("skew") } else { 0.0 };
    let m = Mat3::translate(vec2(pos.0, pos.1))
        * Mat3::skew_deg(-skew, pr.f("skewAxis") + 90.0)
        * Mat3::rotate_deg(rotation)
        * Mat3::scale(vec2(sw / 100.0, sh / 100.0))
        * Mat3::translate(vec2(-anchor.0, -anchor.1));
    let opacity = if follow_on(pr, "followOpacity", false) { (pr.f("opacity") / 100.0).clamp(0.0, 1.0) } else { 1.0 };
    Some((m, opacity))
}

/// Whether the children that follow a Transform effect get its motion blur too (Children Get
/// Motion Blur).
pub fn transform_follow_blur(pr: &crate::Params) -> bool {
    follow_on(pr, "follow", true) && follow_on(pr, "followBlur", true)
}

pub fn transform_extent(adjustment: bool, size: (u32, u32), ms: &[Mat3]) -> (i64, i64, u32, u32) {
    // An adjustment layer works on the comp below in place.
    if adjustment {
        return (0, 0, size.0, size.1);
    }
    let (w, h) = (size.0 as f64, size.1 as f64);
    let (mut x0, mut y0, mut x1, mut y1) = (0.0f64, 0.0f64, w, h);
    for m in ms {
        for (x, y) in [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)] {
            let q = m.apply(vec2(x, y));
            if !(q.x.is_finite() && q.y.is_finite()) {
                return (0, 0, size.0, size.1);
            }
            (x0, y0, x1, y1) = (x0.min(q.x), y0.min(q.y), x1.max(q.x), y1.max(q.y));
        }
    }
    let (x0, y0) = (x0.floor(), y0.floor());
    let (nw, nh) = ((x1.ceil() - x0).max(1.0), (y1.ceil() - y0).max(1.0));
    // Cap the allocation (a huge scale would otherwise ask for an unbounded buffer).
    if nw * nh > 64.0e6 || nw > 32768.0 || nh > 32768.0 {
        return (0, 0, size.0, size.1);
    }
    (x0 as i64, y0 as i64, nw as u32, nh as u32)
}

/// Move `b` onto the grown canvas `ext` (pixel (0, 0) = old pixel (x0, y0)).
fn regrid(b: &mut Buf, ext: (i64, i64, u32, u32), img: Image) {
    b.img = img;
    b.offset[0] -= ext.0 as f64;
    b.offset[1] -= ext.1 as f64;
}

fn transform(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let anchor = b.to_px(ctx.params.v2("anchor"));
    let pos = b.to_px(ctx.params.v2("position"));
    let opacity = ctx.params.f("opacity") as f32 / 100.0;
    let sampling = if ctx.params.e("sampling") == 1 { effectcraft_raster::Sampling::Bicubic } else { effectcraft_raster::Sampling::Bilinear };
    // Motion blur: the transform at Samples instants across the shutter, averaged (the
    // transform at other times comes from the host; without one the frame renders sharp).
    if let Some((angle, phase, n)) = transform_shutter(ctx)
        && let Some(host) = ctx.env.host
    {
        let fd = 1.0 / ctx.fps();
        let samples: Vec<(Mat3, f32)> = (0..n)
            .filter_map(|k| {
                let t = ctx.time + (phase + angle * k as f64 / (n - 1) as f64) / 360.0 * fd;
                let pr = host.params_at(t)?;
                Some((transform_matrix(&pr, &b), pr.f("opacity") as f32 / 100.0))
            })
            .collect();
        if !samples.is_empty() {
            let ms: Vec<Mat3> = samples.iter().map(|s| s.0).collect();
            let ext = transform_extent(ctx.adjustment, (b.img.width, b.img.height), &ms);
            let shift = Mat3::translate(vec2(-ext.0 as f64, -ext.1 as f64));
            // Each sample adds straight into one buffer, over its own bounds only (one mip chain
            // for all of them), weighted by its opacity over the sample count.
            let mut acc = Image::new(ext.2, ext.3);
            let mips = effectcraft_raster::MipChain::new(&b.img);
            let k = 1.0 / samples.len() as f32;
            for (m, op) in &samples {
                effectcraft_raster::accumulate_warp_mips(&mut acc, &mips, &(shift * *m), sampling, op * k);
            }
            drop(mips);
            regrid(&mut b, ext, acc);
            return b;
        }
    }
    let m = transform_effect_matrix(ctx.params, anchor, pos);
    let ext = transform_extent(ctx.adjustment, (b.img.width, b.img.height), &[m]);
    let mut out = Image::new(ext.2, ext.3);
    let shift = Mat3::translate(vec2(-ext.0 as f64, -ext.1 as f64));
    effectcraft_raster::composite_warp(&mut out, &b.img, &(shift * m), &effectcraft_raster::WarpOpts { opacity, sampling, ..Default::default() });
    regrid(&mut b, ext, out);
    b
}

fn corner_pin(ctx: &EffectCtx, mut b: Buf) -> Buf {
    let (w, h) = (b.layer_w(ctx), b.layer_h(ctx));
    let q = ["ul", "ur", "lr", "ll"].map(|k| {
        let p = b.to_px(ctx.params.v2(k));
        vec2(p.0, p.1)
    });
    let quad = Mat3::square_to_quad(q);
    let o = b.to_px([0.0, 0.0]);
    let m = quad * Mat3::scale(vec2(1.0 / (w * b.scale), 1.0 / (h * b.scale))) * Mat3::translate(vec2(-o.0, -o.1));
    let mut out = Image::new(b.img.width, b.img.height);
    effectcraft_raster::composite_warp(&mut out, &b.img, &m, &Default::default());
    b.img = out;
    b
}

impl Buf {
    fn layer_w(&self, ctx: &EffectCtx) -> f64 {
        ctx.layer_size[0].max(1.0)
    }
    fn layer_h(&self, ctx: &EffectCtx) -> f64 {
        ctx.layer_size[1].max(1.0)
    }
}

pub fn specs() -> Vec<EffectSpec> {
    let pt = |x, y| Value::Vec2([x, y]);
    vec![
        spec(
            "ec.distort.twirl",
            "Twirl",
            vec![
                p("angle", "Angle", num(0.0), ParamUi::Angle),
                p("radius", "Twirl Radius", num(30.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("center", "Twirl Center", pt(0.5, 0.5), ParamUi::Point),
            ],
            twirl,
        ),
        spec(
            "ec.distort.spherize",
            "Spherize",
            vec![p("radius", "Radius", num(0.0), slider(0.0, 2500.0, 0.0, 2500.0, 1)), p("center", "Center of Sphere", pt(0.5, 0.5), ParamUi::Point)],
            spherize,
        ),
        spec(
            "ec.distort.bulge",
            "Bulge",
            vec![
                p("hradius", "Horizontal Radius", num(50.0), slider(0.0, 4000.0, 0.0, 1000.0, 1)),
                p("vradius", "Vertical Radius", num(50.0), slider(0.0, 4000.0, 0.0, 1000.0, 1)),
                p("center", "Bulge Center", pt(0.5, 0.5), ParamUi::Point),
                p("height", "Bulge Height", num(1.0), slider(-4.0, 4.0, -4.0, 4.0, 2)),
                p("taperRadius", "Taper Radius", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("pinAllEdges", "Pin All Edges", Value::Bool(false), ParamUi::Checkbox),
            ],
            bulge,
        ),
        spec(
            "ec.distort.wavewarp",
            "Wave Warp",
            vec![
                p("waveType", "Wave Type", Value::Enum(0), popup(&WAVE_TYPES)),
                p("height", "Wave Height", num(10.0), slider(-1000.0, 1000.0, -100.0, 100.0, 0)),
                p("width", "Wave Width", num(40.0), slider(1.0, 10000.0, 1.0, 500.0, 0)),
                p("direction", "Direction", num(90.0), ParamUi::Angle),
                p("speed", "Wave Speed", num(1.0), slider(-100.0, 100.0, -5.0, 5.0, 1)),
                p("pinning", "Pinning", Value::Enum(0), popup(&WAVE_PINNING)),
                p("phase", "Phase", num(0.0), ParamUi::Angle),
            ],
            wave_warp,
        ),
        spec(
            "ec.distort.ripple",
            "Ripple",
            vec![
                p("radius", "Radius", num(50.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("center", "Center of Ripple", pt(0.5, 0.5), ParamUi::Point),
                p("conversion", "Type of Conversion", Value::Enum(0), popup(&["Asymmetric", "Symmetric"])),
                p("speed", "Wave Speed", num(1.0), slider(-15.0, 15.0, -5.0, 5.0, 1)),
                p("waveWidth", "Wave Width", num(20.0), slider(1.0, 1000.0, 1.0, 100.0, 1)),
                p("waveHeight", "Wave Height", num(10.0), slider(0.0, 400.0, 0.0, 100.0, 1)),
                p("phase", "Ripple Phase", num(0.0), ParamUi::Angle),
            ],
            ripple,
        ),
        spec(
            "ec.distort.mirror",
            "Mirror",
            vec![p("center", "Reflection Center", pt(0.5, 0.5), ParamUi::Point), p("angle", "Reflection Angle", num(0.0), ParamUi::Angle)],
            mirror,
        ),
        spec(
            "ec.distort.offset",
            "Offset",
            vec![p("shift", "Shift Center To", pt(0.5, 0.5), ParamUi::Point), p("blend", "Blend With Original", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 1))],
            offset,
        ),
        spec(
            "ec.distort.polar",
            "Polar Coordinates",
            vec![
                p("interpolation", "Interpolation", num(0.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("conversion", "Type of Conversion", Value::Enum(0), popup(&["Rect to Polar", "Polar to Rect"])),
            ],
            polar,
        ),
        spec(
            "ec.distort.transform",
            "Transform",
            vec![
                p("anchor", "Anchor Point", pt(0.5, 0.5), ParamUi::Point),
                p("position", "Position", pt(0.5, 0.5), ParamUi::Point),
                p("uniform", "Uniform Scale", Value::Bool(true), ParamUi::Checkbox),
                p("scaleHeight", "Scale Height", num(100.0), slider(-10000.0, 10000.0, 0.0, 600.0, 1)),
                p("scaleWidth", "Scale Width", num(100.0), slider(-10000.0, 10000.0, 0.0, 600.0, 1)),
                p("skew", "Skew", num(0.0), slider(-70.0, 70.0, -70.0, 70.0, 1)),
                p("skewAxis", "Skew Axis", num(0.0), ParamUi::Angle),
                p("rotation", "Rotation", num(0.0), ParamUi::Angle),
                p("opacity", "Opacity", num(100.0), slider(0.0, 100.0, 0.0, 100.0, 1)),
                p("useCompositionShutterAngle", "Use Composition's Shutter Angle", Value::Bool(true), ParamUi::Checkbox),
                p("shutterAngle", "Shutter Angle", num(0.0), slider(0.0, 720.0, 0.0, 360.0, 1)),
                p("sampling", "Sampling", Value::Enum(0), popup(&["Bilinear", "Bicubic"])),
                // Not in After Effects: the layer's children follow the effect (see
                // `transform_follow`).
                p("follow", "Children Follow", Value::Bool(true), ParamUi::Checkbox),
                p("followPosition", "Children Follow Position", Value::Bool(true), ParamUi::Checkbox),
                p("followScale", "Children Follow Scale", Value::Bool(true), ParamUi::Checkbox),
                p("followRotation", "Children Follow Rotation", Value::Bool(true), ParamUi::Checkbox),
                p("followSkew", "Children Follow Skew", Value::Bool(false), ParamUi::Checkbox),
                p("followOpacity", "Children Follow Opacity", Value::Bool(false), ParamUi::Checkbox),
                p("followBlur", "Children Get Motion Blur", Value::Bool(true), ParamUi::Checkbox),
            ],
            transform,
        ),
        spec(
            "ec.distort.cornerpin",
            "Corner Pin",
            vec![
                p("ul", "Upper Left", pt(0.0, 0.0), ParamUi::Point),
                p("ur", "Upper Right", pt(1.0, 0.0), ParamUi::Point),
                p("ll", "Lower Left", pt(0.0, 1.0), ParamUi::Point),
                p("lr", "Lower Right", pt(1.0, 1.0), ParamUi::Point),
            ],
            corner_pin,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Params;

    fn img(w: u32, h: u32) -> Image {
        crate::util::gen_image(w, h, |x, y| [x as f32 / w as f32, y as f32 / h as f32, ((x * 7 + y * 3) % 11) as f32 / 11.0, 1.0])
    }

    /// Run with Point defaults scaled to the layer (like `instantiate`).
    fn run(id: &str, set: &[(&str, Value)], im: Image) -> Buf {
        let s = crate::find(id).unwrap();
        let ls = [im.width as f64, im.height as f64];
        let mut params = Params { values: s.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, ls))).collect() };
        for (k, v) in set {
            assert!(params.values.contains_key(*k), "{id}: unknown param {k}");
            params.values.insert(k.to_string(), v.clone());
        }
        let ctx = EffectCtx { params: &params, time: 0.0, layer_size: ls, seed: 3, adjustment: false, env: Default::default() };
        crate::apply(s, &ctx, Buf { img: im, offset: [0.0, 0.0], scale: 1.0 })
    }

    fn differs(a: Px4, b: Px4) -> bool {
        (0..4).any(|i| (a[i] - b[i]).abs() > 1e-4)
    }
    type Px4 = [f32; 4];

    #[test]
    fn twirl_radius_is_a_percentage_of_the_larger_dimension() {
        // 60×20 layer, radius 45 → 27.9 px: a pixel 20 px right of the centre is twirled (the old
        // smaller-dimension rule gave 5 px), corners (31 px away) are not.
        let im = img(60, 20);
        let o = run("ec.distort.twirl", &[("angle", num(90.0)), ("radius", num(45.0))], im.clone());
        assert!(differs(o.img.get(50, 10), im.get(50, 10)));
        assert_eq!(o.img.get(0, 0), im.get(0, 0));
    }

    #[test]
    fn twirl_radius_follows_the_input_and_eases_out() {
        // Liquid Glass: a 300% Motion Tile triples the input, and Twirl Radius with it.
        assert!((twirl_radius(15.4, 7680, 4320) / twirl_radius(15.4, 2560, 1440) - 3.0).abs() < 1e-9);
        assert!((twirl_radius(15.4, 2560, 1440) - 408.0).abs() < 1.0);
        // Full Angle at the centre, half way at half the radius, nothing at the rim.
        assert_eq!(twirl_falloff(1.0), 1.0);
        assert_eq!(twirl_falloff(0.5), 0.5);
        assert_eq!(twirl_falloff(0.0), 0.0);
    }

    #[test]
    fn wave_warp_wave_spans_twice_the_width_and_moves_backwards_in_time() {
        let dir = (1.0, 0.0);
        // One full wave (crest and trough) per 2 × Wave Width.
        assert!((wave_warp_cycles(200.0, 0.0, dir, 100.0, 0.0, 0.0, 0.0) - 1.0).abs() < 1e-12);
        // Wave Speed 0.5 for 1 s: half a cycle back; Phase 90° a quarter forward.
        assert!((wave_warp_cycles(0.0, 0.0, dir, 100.0, 0.0, 1.0, 0.5) + 0.5).abs() < 1e-12);
        assert!((wave_warp_cycles(0.0, 0.0, dir, 100.0, 90.0, 0.0, 0.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn ripple_radius_reaches_the_edge_at_100_and_conversion_matters() {
        let im = img(40, 40);
        let base = [("radius", num(100.0)), ("waveHeight", num(4.0)), ("waveWidth", num(7.0))];
        let asym = run("ec.distort.ripple", &base, im.clone());
        let mut sym_set = base.to_vec();
        sym_set.push(("conversion", Value::Enum(1)));
        let sym = run("ec.distort.ripple", &sym_set, im.clone());
        assert!(asym.img.data.iter().zip(&sym.img.data).any(|(a, b)| differs(*a, *b)));
        // Corners lie beyond radius 100 % (half the layer) and stay put.
        assert_eq!(asym.img.get(0, 0), im.get(0, 0));
    }

    #[test]
    fn transform_effect_skews_after_rotating_and_keeps_moved_pixels() {
        // SceneYard Calendar Page Tear: Rotation -20 with Skew 20 leans a page into a pure
        // horizontal shear (rows stay rows, about 1/cos 20° further from the anchor), as in AE.
        let mut im = Image::new(60, 60);
        for x in 10..50 {
            im.set(x, 40, [1.0; 4]);
        }
        let set = [("anchor", Value::Vec2([30.0, 30.0])), ("position", Value::Vec2([30.0, 30.0])), ("skew", num(20.0)), ("rotation", num(-20.0))];
        let out = run("ec.distort.transform", &set, im);
        let row = |b: &Buf, y: i64| (0..b.img.width as i64).filter(|&x| b.img.get(x, y)[3] > 0.3).count();
        let y = (40.0 - 30.0) / 20f64.to_radians().cos() + 30.0 + out.offset[1];
        assert!(row(&out, y.floor() as i64) + row(&out, y.ceil() as i64) >= 30, "the bar stays one horizontal row");
        // A rotation that carries content past the old edges grows the buffer instead of cutting it.
        let mut sq = Image::new(40, 10);
        for x in 0..40 {
            for y in 0..10 {
                sq.set(x, y, [1.0; 4]);
            }
        }
        let rot = run("ec.distort.transform", &[("anchor", Value::Vec2([20.0, 5.0])), ("position", Value::Vec2([20.0, 5.0])), ("rotation", num(90.0))], sq);
        assert!(rot.img.height >= 40, "{}x{}", rot.img.width, rot.img.height);
        let covered: f32 = rot.img.data.iter().map(|p| p[3]).sum();
        assert!((covered - 400.0).abs() < 10.0, "{covered}");
    }

    #[test]
    fn bulge_taper_and_pin_all_edges() {
        let im = img(40, 40);
        let big = [("hradius", num(40.0)), ("vradius", num(40.0)), ("height", num(2.0))];
        let free = run("ec.distort.bulge", &big, im.clone());
        let mut pinned_set = big.to_vec();
        pinned_set.push(("pinAllEdges", Value::Bool(true)));
        let pinned = run("ec.distort.bulge", &pinned_set, im.clone());
        assert!(differs(free.img.get(20, 0), im.get(20, 0)));
        assert!(!differs(pinned.img.get(20, 0), im.get(20, 0)));
        assert!(!differs(pinned.img.get(0, 20), im.get(0, 20)));
        let mut taper_set = big.to_vec();
        taper_set.push(("taperRadius", num(100.0)));
        let tapered = run("ec.distort.bulge", &taper_set, im.clone());
        assert!(differs(tapered.img.get(30, 20), free.img.get(30, 20)));
    }

    #[test]
    fn wave_warp_shapes_stay_in_range_and_pinning_holds_the_edge() {
        for k in 0..WAVE_TYPES.len() as u32 {
            for i in 0..200 {
                let v = wave_shape(k, i as f64 * 0.137 - 5.0, 9);
                assert!((-1.0001..=1.0001).contains(&v), "{} {v}", WAVE_TYPES[k as usize]);
            }
        }
        let im = img(40, 30);
        // Direction 0° → horizontal displacement; Left Edge pinning keeps the left column.
        let set = [("direction", num(0.0)), ("height", num(5.0)), ("pinning", Value::Enum(3))];
        let o = run("ec.distort.wavewarp", &set, im.clone());
        let pad = ((o.img.width - im.width) / 2) as i64;
        for y in 0..30 {
            let (a, b) = (o.img.get(pad, y + pad), im.get(0, y));
            assert!((0..4).all(|i| (a[i] - b[i]).abs() < 0.01), "row {y}: {a:?} vs {b:?}");
        }
        let free = run("ec.distort.wavewarp", &[("direction", num(0.0)), ("height", num(5.0))], im);
        assert!(free.img.data.iter().zip(&o.img.data).any(|(a, b)| differs(*a, *b)));
    }

    #[test]
    fn transform_sampling_bicubic_differs_from_bilinear() {
        let im = img(32, 32);
        let a = run("ec.distort.transform", &[("rotation", num(17.0))], im.clone());
        let b = run("ec.distort.transform", &[("rotation", num(17.0)), ("sampling", Value::Enum(1))], im);
        assert!(a.img.data.iter().zip(&b.img.data).any(|(p, q)| differs(*p, *q)));
    }

    #[test]
    fn transform_shutter_angle_motion_blur() {
        /// Position moves right 200 px per second.
        struct Moving;
        impl crate::EffectHost for Moving {
            fn layer(&self, _: u64, _: bool) -> Option<crate::LayerPixels> {
                None
            }
            fn audio(&self, _: u64, _: f64, _: usize, _: u32) -> Option<Vec<f32>> {
                None
            }
            fn params_at(&self, t: f64) -> Option<Params> {
                let s = crate::find("ec.distort.transform").unwrap();
                let mut p = Params { values: s.params.iter().map(|p| (p.id.to_string(), crate::default_value(p, [40.0, 20.0]))).collect() };
                p.values.insert("position".into(), Value::Vec2([20.0 + 200.0 * t, 10.0]));
                Some(p)
            }
        }
        let mut sq = Image::new(40, 20);
        for y in 5..15 {
            for x in 15..25 {
                sq.set(x, y, [1.0, 1.0, 1.0, 1.0]);
            }
        }
        let render = |vals: &[(&str, Value)], shutter: Option<(f64, f64, u32)>| {
            let env = crate::EffectEnv { host: Some(&Moving), frame_rate: 10.0, shutter, ..Default::default() };
            crate::run_fx("ec.distort.transform", vals, sq.clone(), 0.0, env).img
        };
        let partial = |i: &Image| i.data.iter().filter(|p| p[3] > 0.02 && p[3] < 0.98).count();
        // Use Composition Shutter Angle with the comp / layer motion blur off: sharp.
        assert_eq!(partial(&render(&[], None)), 0);
        // With the comp shutter (180°): smeared along x over 10 px.
        let comp = render(&[], Some((180.0, -90.0, 16)));
        assert!(partial(&comp) > 50, "{}", partial(&comp));
        // Own Shutter Angle.
        let own = render(&[("useCompositionShutterAngle", Value::Bool(false)), ("shutterAngle", num(360.0))], None);
        assert!(partial(&own) > partial(&comp));
        assert_eq!(partial(&render(&[("useCompositionShutterAngle", Value::Bool(false))], None)), 0, "0 deg: sharp");
    }
}
