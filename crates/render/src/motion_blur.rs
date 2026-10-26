//! Motion blur: the shutter, sub-sample matrices through collapsed precomps, adaptive
//! sample counts, and content (shape/text) motion blur.

use crate::collapse::Through;
use crate::{EvalCtx, Image, Renderer, cache, masks};
use effectcraft_effects::Buf;
use effectcraft_geom::{Mat3, vec2};
use effectcraft_project::{Comp, Layer, LayerSource};
use effectcraft_time::Tick;
use std::cell::Cell;
use std::collections::HashMap;

/// A motion-blur shutter: the comp's, or for a collapsed precomp's layers the outermost
/// containing comp's (they are drawn into its frames).
#[derive(Clone, Copy)]
pub(crate) struct Shutter {
    /// Shutter angle and phase as fractions of a frame.
    angle: f64,
    phase: f64,
    /// Frame duration (seconds).
    frame: f64,
    /// Samples Per Frame and Adaptive Sample Limit.
    samples: u32,
    limit: u32,
}

impl Shutter {
    /// Seconds from the frame time to sub-sample `i` of `n`, spread over the open shutter.
    pub(crate) fn offset(&self, i: usize, n: usize) -> f64 {
        let f = if n > 1 { i as f64 / (n - 1) as f64 } else { 0.0 };
        (self.phase + self.angle * f) * self.frame
    }
}

impl<'a> Renderer<'a> {
    /// The collapsed precomp layers this comp is drawn through, outermost first.
    fn links(&self) -> impl Iterator<Item = &Through<'a>> {
        self.through.iter().map_while(Option::as_ref)
    }

    /// The shutter of a layer's motion blur: its comp's, or for a collapsed precomp's layers the
    /// outermost containing comp's.
    pub(crate) fn shutter(&self, ctx: &EvalCtx) -> Shutter {
        let c: &Comp = match self.links().next() {
            Some(l) => l.comp,
            None => ctx.comp,
        };
        Shutter {
            angle: c.shutter_angle / 360.0,
            phase: c.shutter_phase / 360.0,
            frame: c.frame_duration().seconds(),
            samples: c.motion_blur_samples,
            limit: c.motion_blur_adaptive_limit,
        }
    }

    /// Whether a layer moves within the shutter: its own motion blur, that of a collapsed
    /// precomp layer it is drawn through, or a parent's Transform effect it follows with Children
    /// Get Motion Blur (see [`Self::follow_shutter`]).
    pub(crate) fn blurred(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        self.mb_on(ctx, layer) || (!self.through_lost && self.links().any(|l| l.blur)) || self.follow_shutter(ctx, layer).is_some()
    }

    /// The shutter a layer's sub-samples spread over: the comp's (see [`Self::shutter`]) when it
    /// or a collapsed precomp above it is motion blurred, else that of the parents' Transform
    /// effect it follows.
    pub(crate) fn layer_shutter(&self, ctx: &EvalCtx, layer: &Layer) -> Shutter {
        if self.mb_on(ctx, layer) || (!self.through_lost && self.links().any(|l| l.blur)) {
            return self.shutter(ctx);
        }
        self.follow_shutter(ctx, layer).unwrap_or_else(|| self.shutter(ctx))
    }

    /// The shutter of the nearest parent's Transform effect that the layer follows with Children
    /// Get Motion Blur, as that effect blurs its own layer (`effectcraft_effects::transform_shutter`):
    /// the comp's when Use Composition's Shutter Angle is on and the parent is motion blurred,
    /// else its Shutter Angle centred on the frame, 16 samples. A 2D child's buffer is then drawn
    /// once and placed at each sub-sample, as the effect warps its parent's pixels.
    pub(crate) fn follow_shutter(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Shutter> {
        let mut cur = layer.parent;
        let mut guard = 0;
        while let Some(pid) = cur {
            guard += 1;
            let p = ctx.layer(pid).filter(|_| guard <= 64)?;
            if p.switches.effects
                && !p.switches.adjustment
                && let Some(fx) = p.effects()
            {
                for g in fx
                    .groups()
                    .filter(|g| g.enabled && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effect == "ec.distort.transform"))
                {
                    let pr = effectcraft_effects::flatten_params(g, &mut |q| ctx.value(p, q));
                    if !effectcraft_effects::transform_follow_blur(&pr) {
                        continue;
                    }
                    let frame = ctx.comp.frame_duration().seconds();
                    let (angle, phase, n) = if pr.b("useCompositionShutterAngle") {
                        if !self.mb_on(ctx, p) {
                            continue;
                        }
                        (ctx.comp.shutter_angle, ctx.comp.shutter_phase, ctx.comp.motion_blur_samples)
                    } else {
                        let a = pr.f("shutterAngle").clamp(0.0, 720.0);
                        (a, -a * 0.5, 16)
                    };
                    if angle > 0.0 {
                        let n = n.clamp(2, 64);
                        return Some(Shutter { angle: angle / 360.0, phase: phase / 360.0, frame, samples: n, limit: n.max(16) });
                    }
                }
            }
            cur = p.parent;
        }
        None
    }

    /// The collapsed precomps' matrix (nested comp → the frame's comp) and the nested comp's time
    /// `dt` seconds into the outermost comp's shutter: each precomp layer moves if it is motion
    /// blurred, and times map down through each one (stretch, time remapping).
    fn outer_at(&self, ctx: &EvalCtx, dt: f64) -> (Option<Mat3>, Tick) {
        let shift = Tick::from_seconds_f64(dt);
        let mut outer: Option<Mat3> = None;
        let mut t: Option<Tick> = None;
        for l in self.links() {
            let at = EvalCtx { comp_id: l.comp_id, comp: l.comp, time: t.unwrap_or(l.time + shift), ..*ctx };
            let m = if l.blur { self.collapse_matrix(&at, l.layer) } else { self.collapse_matrix(&EvalCtx { time: l.time, ..at }, l.layer) };
            outer = Some(outer.map_or(m, |o| o * m));
            t = Some(at.nested_time(l.layer));
        }
        (outer, t.unwrap_or(ctx.time + shift))
    }

    /// [`Self::buf_matrix`] for the motion-blur sub-sample `dt` seconds into the shutter: the
    /// layer moves if it is motion blurred, the collapsed precomps above it if theirs are.
    pub(crate) fn buf_matrix_at(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, dt: f64, mo: &MotionMemo) -> Mat3 {
        let l2c = mo.at(dt, || {
            let own = self.mb_on(ctx, layer);
            if self.through_lost || self.links().next().is_none() {
                let at = ctx.at(ctx.time + Tick::from_seconds_f64(dt));
                if own {
                    return self.comp_matrix(&at, layer);
                }
                // Only the followed Transform effects of its parents move it.
                let (l2c, _) = ctx.layer_to_comp_fx(layer, &at);
                return self.outer.map_or(l2c, |o| o * l2c);
            }
            let (outer, t) = self.outer_at(ctx, dt);
            let (l2c, _) = if own { ctx.at(t).layer_to_comp(layer) } else { ctx.layer_to_comp_fx(layer, &ctx.at(t)) };
            outer.map_or(l2c, |o| o * l2c)
        });
        self.place_matrix(l2c, buf)
    }

    /// Shape and text content that animates within the shutter (paths, Trim Paths, text
    /// animators…) of a motion-blurred layer: the times its source is drawn at and averaged,
    /// Samples Per Frame of them, as After Effects does for shape layers. `None` when the content
    /// holds still (compared through its cache key, which leaves the transform out).
    pub(crate) fn content_times(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Vec<Tick>> {
        if !matches!(layer.source, LayerSource::Shape | LayerSource::Text) || !self.mb_on(ctx, layer) {
            return None;
        }
        let sh = self.shutter(ctx);
        if sh.angle <= 0.0 {
            return None;
        }
        let n = if self.opts.draft { 4 } else { (sh.samples as usize).clamp(2, 64) };
        let times: Vec<Tick> = (0..n).map(|i| self.outer_at(ctx, sh.offset(i, n)).1).collect();
        let key = |t: Tick| cache::layer_key(&ctx.at(t), layer, 1.0, false, false);
        let k0 = key(*times.first()?)?;
        let moves = [times.get(n / 2), times.last()].into_iter().flatten().any(|t| key(*t) != Some(k0));
        moves.then_some(times)
    }

    /// The layer's source drawn at each of `times` and averaged (premultiplied) on the union of
    /// their bounds. Content that also changes its raster scale within the shutter, or would need
    /// an outsized buffer, is drawn once.
    pub(crate) fn blurred_source(&self, ctx: &EvalCtx, layer: &Layer, times: &[Tick]) -> Option<Buf> {
        let bufs: Vec<Buf> = times.iter().filter_map(|t| self.source(&ctx.at(*t), layer)).collect();
        let scale = bufs.first()?.scale;
        // Pixel = layer point × scale + offset: with the largest offset every buffer lands at
        // x + (off - its offset) ≥ 0.
        let off = [0, 1].map(|i| bufs.iter().map(|b| b.offset[i]).fold(f64::MIN, f64::max));
        let w = bufs.iter().map(|b| off[0] - b.offset[0] + b.img.width as f64).fold(0.0, f64::max).ceil();
        let h = bufs.iter().map(|b| off[1] - b.offset[1] + b.img.height as f64).fold(0.0, f64::max).ceil();
        if bufs.iter().any(|b| (b.scale - scale).abs() > 1e-9) || !(w * h).is_finite() || w * h > (1u64 << 26) as f64 {
            return self.source(ctx, layer);
        }
        let mut acc = Image::new(w as u32, h as u32);
        let k = 1.0 / bufs.len() as f32;
        for b in &bufs {
            let m = Mat3::translate(vec2(off[0] - b.offset[0], off[1] - b.offset[1]));
            effectcraft_raster::accumulate_warp(&mut acc, &b.img, &m, effectcraft_raster::Sampling::Bilinear, k);
        }
        Some(Buf { img: acc, offset: off, scale })
    }

    /// A processed-layer cache key that also covers content motion blur (the averaged source
    /// depends on the content at every sub-sample).
    pub(crate) fn content_key(&self, ctx: &EvalCtx, layer: &Layer, key: Option<u64>) -> Option<u64> {
        let k = key?;
        let Some(times) = self.content_times(ctx, layer) else { return Some(k) };
        times.iter().try_fold(cache::derive(k, 0x6d62_6c72), |d, t| Some(cache::derive(d, cache::layer_key(&ctx.at(*t), layer, 1.0, false, false)?)))
    }

    /// Whether a layer is motion blurred: the render options, the comp's switch and the layer's
    /// switch. Layer motion, mask motion blur, effects that read the shutter and Advanced 3D all
    /// ask this.
    pub(crate) fn mb_on(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        self.opts.motion_blur && ctx.comp.enable_motion_blur && self.layer_motion_blur(layer)
    }

    /// What a layer's mask motion blur may do in this render (see [`masks::MaskBlur`]).
    pub(crate) fn mask_blur(&self, ctx: &EvalCtx, layer: &Layer) -> masks::MaskBlur<'a> {
        masks::MaskBlur {
            allowed: self.opts.motion_blur && ctx.comp.enable_motion_blur,
            layer: self.layer_motion_blur(layer),
            samples: if self.opts.draft { 4 } else { ctx.comp.motion_blur_samples as usize },
            accel: self.active_accel(),
        }
    }

    /// Motion-blur sub-samples for a layer buffer (1 = no motion blur). As in After Effects, a 2D
    /// layer gets about one sample per output pixel its corners travel during the shutter,
    /// at least Samples Per Frame and at most the comp's Adaptive Sample Limit; a layer that
    /// does not move within the shutter is drawn once.
    pub(crate) fn mb_samples(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, mo: &MotionMemo) -> usize {
        if !self.blurred(ctx, layer) {
            return 1;
        }
        let travel = self.shutter_travel(ctx, layer, buf, mo);
        if travel < 1e-3 {
            return 1;
        }
        if self.opts.draft {
            return 4;
        }
        let sh = self.layer_shutter(ctx, layer);
        adaptive_samples(travel, sh.samples, sh.limit)
    }

    /// Largest distance (output pixels) a corner of the layer buffer moves during the shutter,
    /// measured along the path at a few points.
    fn shutter_travel(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, mo: &MotionMemo) -> f64 {
        let sh = self.layer_shutter(ctx, layer);
        let (w, h) = (buf.img.width as f64, buf.img.height as f64);
        let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h), (w * 0.5, h * 0.5)];
        const PROBES: usize = 8;
        let mut prev: Option<Vec<effectcraft_geom::Vec2>> = None;
        let mut travel = vec![0.0f64; corners.len()];
        for i in 0..PROBES {
            let m = self.buf_matrix_at(ctx, layer, buf, sh.offset(i, PROBES), mo);
            let pts: Vec<_> = corners.iter().map(|&(x, y)| m.apply(vec2(x, y))).collect();
            if let Some(p) = &prev {
                for (k, (a, b)) in p.iter().zip(&pts).enumerate() {
                    travel[k] += (b.x - a.x).hypot(b.y - a.y);
                }
            }
            prev = Some(pts);
        }
        travel.into_iter().fold(0.0, f64::max)
    }
}

/// A layer's layer → (top-level) comp matrices during one draw, at rest and at each shutter
/// offset. Drawing a layer asks for them several times (the sample count probes the shutter, then
/// every sub-sample is placed; styles place each pass; the changed region measures them again),
/// and each one evaluates the layer's and its parents' transforms: the memo evaluates each once.
/// The matrices don't depend on the buffer, so every buffer of the draw shares them.
#[derive(Default)]
pub(crate) struct MotionMemo {
    still: Cell<Option<Mat3>>,
    at: Cell<HashMap<u64, Mat3>>,
}

impl MotionMemo {
    /// The matrix at rest (the frame time).
    pub(crate) fn still(&self, f: impl FnOnce() -> Mat3) -> Mat3 {
        if let Some(m) = self.still.get() {
            return m;
        }
        let m = f();
        self.still.set(Some(m));
        m
    }

    /// The matrix `dt` seconds into the shutter.
    pub(crate) fn at(&self, dt: f64, f: impl FnOnce() -> Mat3) -> Mat3 {
        let mut map = self.at.take();
        let m = *map.entry(dt.to_bits()).or_insert_with(f);
        self.at.set(map);
        m
    }

    /// Matrices evaluated so far (tests).
    #[cfg(test)]
    pub(crate) fn evaluated(&self) -> usize {
        let map = self.at.take();
        let n = map.len() + self.still.get().is_some() as usize;
        self.at.set(map);
        n
    }
}

/// Adaptive motion-blur sample count for a layer whose corners travel `travel` output pixels
/// during the shutter: one sample per pixel, clamped to Samples Per Frame … Adaptive Sample Limit.
pub fn adaptive_samples(travel: f64, per_frame: u32, limit: u32) -> usize {
    let lo = per_frame.clamp(2, 64) as f64;
    let hi = (limit.clamp(16, 256) as f64).max(lo);
    travel.ceil().clamp(lo, hi) as usize
}
