//! The layer effect stack: the effect host (layer parameters, other times, the comp's
//! camera and lights), runs of GPU effects and the CPU effects, in effect space.

use crate::{Accelerator, EvalCtx, FxStep, FxTarget, Image, Renderer, masks, three_d};
use effectcraft_effects::{Buf, EffectCtx, EffectEnv, EffectHost, LayerPixels, Params};
use effectcraft_geom::{Mat3, vec2};
use effectcraft_project::{GroupKind, ItemKind, Layer, LayerSource};
use effectcraft_time::Tick;
use std::sync::Arc;

/// Layer parameters and audio for effects (see [`effectcraft_effects::EffectHost`]). What it
/// hands out is in effect space (see [`effect_bounds`]): the other layer's for its pixels, the
/// running layer's for its own frames and the comp scene.
pub(crate) struct FxHost<'r, 'a, 'c> {
    pub(crate) r: &'r Renderer<'a>,
    pub(crate) ctx: &'c EvalCtx<'a>,
    pub(crate) layer: &'c Layer,
    /// Where the stack's effect space starts in layer coordinates (zero on adjustment layers,
    /// whose effects run on the comp below).
    pub(crate) origin: [f64; 2],
    /// Index of the effect being rendered (bounds `self_at` to effects before it).
    pub(crate) index: usize,
    /// The stack's input before any effect ran (CC Composite's original).
    pub(crate) original: Option<&'c Buf>,
}

impl FxHost<'_, '_, '_> {
    /// Another layer's pixels for a layer parameter, in its effect space; `layer_space`: `buf`
    /// is still in layer space (its source or its content).
    fn pixels(&self, other: &Layer, mut buf: Buf, layer_space: bool) -> LayerPixels {
        let (size, origin) = self.ctx.effect_bounds(other);
        if layer_space {
            buf.rebase(origin);
        }
        LayerPixels { buf, size }
    }
}

/// The comp's camera and first light relative to `layer`, in its layer pixels (see
/// [`EffectHost::comp_scene`]): the comp camera's view of the layer (as if 3D at its transform),
/// brought back into the layer's own pixel grid through the inverse of how the layer itself
/// composites.
pub(crate) fn comp_scene(ctx: &EvalCtx, layer: &Layer) -> Option<effectcraft_effects::CompScene> {
    let world = ctx.world_matrix(layer);
    let (cam, _, _) = ctx.camera();
    let p = (cam * world).0;
    let back = ctx.layer_to_comp(layer).0.inverse()?.0;
    let rows = [p[0], p[1], p[3]];
    let camera: [[f64; 4]; 3] = std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| back[i][k] * rows[k][j]).sum()));
    let inv = world.inverse();
    let light = three_d::light::lights_at(ctx).first().and_then(|l| {
        let inv = inv.as_ref()?;
        let pos = inv.apply(l.pos);
        let dir = inv.apply_vec(l.dir);
        let len = (dir.x * dir.x + dir.y * dir.y + dir.z * dir.z).sqrt().max(1e-12);
        let kind = match l.kind {
            effectcraft_project::LightKind::Parallel => 0,
            effectcraft_project::LightKind::Ambient => 2,
            _ => 1,
        };
        Some(effectcraft_effects::CompLight { pos: [pos.x, pos.y, pos.z], dir: [dir.x / len, dir.y / len, dir.z / len], color: l.color, kind })
    });
    let camera_layer = ctx.comp.active_camera(ctx.time).is_some();
    Some(effectcraft_effects::CompScene { camera: Some(camera), camera_layer, light })
}

/// Nesting limit for effects that render layers (other layers, other times).
pub(crate) const MAX_FX_DEPTH: usize = 8;

impl EffectHost for FxHost<'_, '_, '_> {
    fn original(&self) -> Option<&Buf> {
        self.original
    }

    fn particles(&self) -> Option<&dyn effectcraft_effects::psim::ParticleSim> {
        self.r.active_accel().and_then(|a| a.particles())
    }
    fn layer(&self, id: u64, masks_and_effects: bool) -> Option<LayerPixels> {
        let other = self.ctx.layer(effectcraft_project::LayerId(id))?;
        if (other.id == self.layer.id && masks_and_effects) || self.r.depth > MAX_FX_DEPTH {
            return None;
        }
        let sub = self.r.nested();
        // The cached layer buffer is shared; effects get their own copy.
        let buf = if masks_and_effects { (*sub.content_buf(self.ctx, other)?).clone() } else { sub.source(self.ctx, other)? };
        Some(self.pixels(other, buf, true))
    }

    fn layer_masks(&self, id: u64) -> Option<LayerPixels> {
        let other = self.ctx.layer(effectcraft_project::LayerId(id))?;
        if self.r.depth > MAX_FX_DEPTH {
            return None;
        }
        let sub = self.r.nested();
        let buf = (*sub.layer_input(self.ctx, other, 0)?).clone();
        Some(self.pixels(other, buf, false))
    }

    fn audio(&self, id: u64, start: f64, frames: usize, rate: u32) -> Option<Vec<f32>> {
        let other = self.ctx.layer(effectcraft_project::LayerId(id))?;
        let LayerSource::Footage { item } = &other.source else { return None };
        let ItemKind::Footage(f) = &self.r.project.item(*item)?.kind else { return None };
        if !f.has_audio {
            return None;
        }
        let st = other.layer_time(Tick::from_seconds_f64(start));
        self.r.footage.audio(*item, f, st, frames, rate)
    }

    fn self_at(&self, layer_time: f64, effects: usize) -> Option<Buf> {
        if self.r.depth > MAX_FX_DEPTH || self.layer.switches.adjustment {
            return None;
        }
        let n = effects.min(self.index);
        let t = self.layer.comp_time(Tick::from_seconds_f64(layer_time));
        let sub = self.r.nested();
        sub.layer_input(&self.ctx.at(t), self.layer, n).map(|b| (*b).clone())
    }

    fn aux(&self) -> Option<Arc<effectcraft_raster::AuxChannels>> {
        match &self.layer.source {
            LayerSource::Footage { item } => {
                let ItemKind::Footage(f) = &self.r.project.item(*item)?.kind else { return None };
                let a = self.r.footage.aux(*item, f, self.ctx.source_time(self.layer))?;
                // Aux pixels per layer pixel (the layer is the footage at its nominal size).
                if f.width > 0 && a.width > 0 {
                    let mut a = (*a).clone();
                    a.scale = a.width as f64 / f.width as f64;
                    return Some(Arc::new(a));
                }
                Some(a)
            }
            LayerSource::Comp { item } => {
                if self.r.depth > MAX_FX_DEPTH || self.r.project.comp_contains(*item, self.ctx.comp_id) {
                    return None;
                }
                self.r.nested().comp_aux(*item, self.ctx.nested_time(self.layer)).map(Arc::new)
            }
            _ => None,
        }
    }

    fn params_at(&self, layer_time: f64) -> Option<Params> {
        let i = self.index;
        let g = self.layer.effects()?.groups().nth(i)?;
        let ctx = self.ctx.at(self.layer.comp_time(Tick::from_seconds_f64(layer_time)));
        Some(effectcraft_effects::flatten_params(g, &mut |pr| ctx.value(self.layer, pr)))
    }

    fn comp_scene(&self) -> Option<effectcraft_effects::CompScene> {
        Some(comp_scene(self.ctx, self.layer)?.rebased(self.origin))
    }

    fn layer_at(&self, id: u64, comp_time: f64, masks_and_effects: bool) -> Option<LayerPixels> {
        let other = self.ctx.layer(effectcraft_project::LayerId(id))?;
        if (other.id == self.layer.id && masks_and_effects) || !comp_time.is_finite() || self.r.depth > MAX_FX_DEPTH {
            return None;
        }
        let ctx = self.ctx.at(Tick::from_seconds_f64(comp_time));
        let sub = self.r.nested();
        Some(if masks_and_effects {
            self.pixels(other, (*sub.content_buf(&ctx, other)?).clone(), true)
        } else if other.id == self.layer.id {
            self.pixels(other, sub.source(&ctx, other)?, true)
        } else {
            // Preserve other-layer reads: time effects historically see source + masks.
            self.pixels(other, (*sub.layer_input(&ctx, other, 0)?).clone(), false)
        })
    }
}

/// An accelerator's kept effect result (see [`Accelerator::effects_kept`]): adopted by the
/// buffer that stores its pixels, released otherwise (when dropped).
pub(crate) struct Kept<'a> {
    accel: &'a dyn Accelerator,
    ticket: Option<u64>,
}

impl Kept<'_> {
    /// `buf` stores the kept pixels unchanged (offsets may differ).
    pub(crate) fn adopt(mut self, buf: &Arc<Buf>) {
        if let Some(t) = self.ticket.take() {
            self.accel.adopt(t, buf);
        }
    }
}

impl Drop for Kept<'_> {
    fn drop(&mut self) {
        if let Some(t) = self.ticket.take() {
            self.accel.release(t);
        }
    }
}

/// The CPU effect target (GPU chains go through [`Accelerator::effects`]: upload, readback).
struct CpuFx<'a> {
    owner: Option<crate::cache::GpuOwner>,
    accel: Option<&'a dyn Accelerator>,
    levels: Option<f32>,
    buf: Option<Buf>,
    /// The accelerator's copy of `buf`, while the last step was a GPU chain.
    kept: Option<Kept<'a>>,
}

impl FxTarget for CpuFx<'_> {
    fn gpu(&mut self, steps: &[FxStep]) -> bool {
        // The buffer is on the CPU: a run of light effects (box-pass blurs) costs less here
        // than its upload and readback (a styled layer's Gaussian Blur: 0.3 ms here, 10-40 ms
        // through the GPU at Half).
        if steps.iter().all(|s| effectcraft_effects::is_light(s.spec.id)) {
            return false;
        }
        let (Some(a), Some(b)) = (self.accel, &self.buf) else { return false };
        match a.effects_kept_owned(steps, b, self.levels, self.owner.as_ref()) {
            Some((out, ticket)) => {
                self.buf = Some(out);
                self.kept = ticket.map(|t| Kept { accel: a, ticket: Some(t) });
                true
            }
            None => false,
        }
    }
    fn cpu(&mut self, f: &mut dyn FnMut(Buf) -> Buf) {
        if let Some(b) = self.buf.take() {
            self.kept = None;
            self.buf = Some(f(b));
        }
    }
    fn shift(&mut self, d: [f64; 2]) {
        if let Some(b) = &mut self.buf {
            b.offset = [b.offset[0] - d[0], b.offset[1] - d[1]];
        }
    }
    fn extent(&self) -> Option<([u32; 2], [f64; 2], f64)> {
        self.buf.as_ref().map(|b| ([b.img.width, b.img.height], b.offset, b.scale))
    }
    fn keep(&mut self, key: u64, cache: &crate::cache::LayerCache) {
        if let Some(b) = &self.buf {
            cache.insert(key, Arc::new(b.clone()));
        }
    }
    fn restore(&mut self, key: u64, cache: &crate::cache::LayerCache) -> bool {
        let Some(b) = cache.get(key) else { return false };
        self.kept = None;
        self.buf = Some((*b).clone());
        true
    }
}

/// Does any property of `g` animate (keyframes or an expression)?
fn animates(g: &effectcraft_project::PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        effectcraft_project::Node::Prop(p) => !p.keys.is_empty() || p.has_expression(),
        effectcraft_project::Node::Group(sub) => animates(sub),
    })
}

impl<'a> Renderer<'a> {
    /// The effects at the top of a layer's stack that make the same pixels every frame, when an
    /// effect follows them: (how many, the key their result is kept under). A card's Drop Shadow
    /// and Deep Glow under an animated Transform effect are rendered once and restored on the
    /// next frames (a 1080p card with a Radius 295 glow: ~20 ms of GPU work a frame saved). They
    /// count when none of them animates or reads the time or the comp's camera, and the layer's
    /// input through them has the same key as one frame earlier (its source, masks and the
    /// layers they read are unchanged). The key also holds where the stack runs (`origin`,
    /// `view`) and the buffer it starts from (`extent`).
    #[allow(clippy::too_many_arguments)]
    fn static_prefix(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        stack: &[(usize, &effectcraft_project::PropGroup, &'static effectcraft_effects::EffectSpec)],
        adjustment: bool,
        origin: [f64; 2],
        view: Option<[f64; 4]>,
        extent: Option<([u32; 2], [f64; 2], f64)>,
    ) -> Option<(usize, u64)> {
        if adjustment || self.cache_to_keep().is_none() || matches!(layer.source, LayerSource::Comp { .. }) {
            return None;
        }
        let p = stack
            .iter()
            .take_while(|(_, g, s)| !animates(g) && !effectcraft_effects::is_time_dependent(s.id) && !effectcraft_effects::READS_COMP_SCENE.contains(&s.id))
            .count();
        let &(next, _, _) = stack.get(p)?;
        if p == 0 {
            return None;
        }
        let src = match layer.source {
            LayerSource::Footage { item } => Some(self.proxy_for(item).is_some() as u64),
            _ => None,
        };
        let (scale, draft, blur) = (self.raster_scale(ctx, layer), self.opts.draft, self.mb_on(ctx, layer));
        let now = crate::cache::input_key_src(ctx, layer, scale, draft, blur, next, src)?;
        let prev = ctx.at(ctx.time - ctx.comp.frame_rate.frame_duration());
        if crate::cache::input_key_src(&prev, layer, scale, draft, blur, next, src)? != now {
            return None;
        }
        let ([w, h], at, bs) = extent?;
        let mut key = crate::cache::derive(now, 0x5f9a_7e1c_0d3b_6241);
        for v in [origin[0], origin[1], at[0], at[1], bs].into_iter().chain(view.into_iter().flatten()) {
            key = crate::cache::derive(key, v.to_bits());
        }
        key = crate::cache::derive(key, (u64::from(w) << 32) | u64::from(h));
        Some((p, key))
    }

    /// Evaluate an effect instance's parameters.
    pub(crate) fn effect_params(&self, ctx: &EvalCtx, layer: &Layer, g: &effectcraft_project::PropGroup) -> Params {
        // Nested groups (Paint strokes, Puppet meshes and pins) are flattened too.
        effectcraft_effects::flatten_params(g, &mut |pr| ctx.value(layer, pr))
    }

    pub(crate) fn apply_effects(&self, ctx: &EvalCtx, layer: &Layer, buf: Buf, adjustment: bool) -> Buf {
        self.apply_effects_timed(ctx, layer, buf, adjustment, usize::MAX, None)
    }

    /// Run the first `limit` effects of the layer's stack (by position; disabled ones count) on
    /// `buf` (layer space, or the comp below for an adjustment layer).
    pub(crate) fn apply_effects_timed(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        buf: Buf,
        adjustment: bool,
        limit: usize,
        timing: Option<&mut Vec<(String, f64)>>,
    ) -> Buf {
        self.apply_effects_kept(ctx, layer, buf, adjustment, limit, timing, None).0
    }

    /// [`Renderer::apply_effects_timed`], with the accelerator's copy of the result when the
    /// stack ended with a GPU chain (see [`Kept`]).
    pub(crate) fn apply_effects_kept(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        mut buf: Buf,
        adjustment: bool,
        limit: usize,
        timing: Option<&mut Vec<(String, f64)>>,
        view: Option<[f64; 4]>,
    ) -> (Buf, Option<Kept<'a>>) {
        let origin = fx_origin(ctx, layer, adjustment);
        buf.rebase(origin);
        let mut t = CpuFx {
            owner: self.cache.and_then(crate::cache::LayerCache::gpu_owner),
            accel: self.active_accel(),
            levels: self.pipe.levels,
            buf: Some(buf),
            kept: None,
        };
        self.run_effect_stack(ctx, layer, adjustment, limit, timing, origin, &mut t, view);
        let mut out = t.buf.take().unwrap_or_else(|| Buf { img: Image::new(1, 1), offset: [0.0; 2], scale: self.opts.scale });
        out.rebase([-origin[0], -origin[1]]);
        (out, t.kept.take())
    }

    /// [`Renderer::apply_effects_timed`] on a buffer already in effect space (measured from
    /// `origin`, in layer coordinates); the result stays in effect space.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn effect_space_stack(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        buf: Buf,
        adjustment: bool,
        limit: usize,
        timing: Option<&mut Vec<(String, f64)>>,
        origin: [f64; 2],
    ) -> Buf {
        let mut t = CpuFx {
            owner: self.cache.and_then(crate::cache::LayerCache::gpu_owner),
            accel: self.active_accel(),
            levels: self.pipe.levels,
            buf: Some(buf),
            kept: None,
        };
        self.run_effect_stack(ctx, layer, adjustment, limit, timing, origin, &mut t, None);
        t.buf.unwrap_or_else(|| Buf { img: Image::new(1, 1), offset: [0.0; 2], scale: self.opts.scale })
    }

    /// A Transform effect that leaves every pixel where it is (Anchor Point = Position, Scale
    /// 100 %, no Rotation or Skew) and isn't motion blurred: it only fades the layer, so a text
    /// or shape layer needs no comp-sized effect canvas for it (#659 is about effects that move
    /// pixels past the glyphs; the Transform effect grows its own buffer when it does).
    pub(crate) fn transform_moves_nothing(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        g: &effectcraft_project::PropGroup,
        spec: &effectcraft_effects::EffectSpec,
    ) -> bool {
        if spec.id != "ec.distort.transform" || ALWAYS_TEXT_CANVAS.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        let p = self.effect_params(ctx, layer, g);
        let sh = p.f("scaleHeight");
        let sw = if p.b("uniform") { sh } else { p.f("scaleWidth") };
        let still = p.v2("anchor") == p.v2("position") && sw == 100.0 && sh == 100.0 && p.f("rotation") == 0.0 && p.f("skew") == 0.0;
        // Motion blur samples the Transform at other times, where it may move.
        let blurred = if p.b("useCompositionShutterAngle") { layer.switches.motion_blur && ctx.comp.enable_motion_blur } else { p.f("shutterAngle") > 0.0 };
        still && !blurred
    }

    /// The part of a text or shape layer's comp-sized effect canvas (`[0, size]`, effect-space
    /// layer units) that `stack` can make non-transparent: the drawn buffer (`extent`) grown by
    /// each effect's reach in turn, clipped to the canvas. Outside it every effect reads and
    /// writes transparent pixels, so the effects run on it alone give the same pixels as on the
    /// whole canvas (a 3D layer's canvas at its raster scale can hold 40 megapixels around a
    /// small shape). `None` when an effect's reach is not known: the whole canvas then.
    fn canvas_support(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        stack: &[(usize, &effectcraft_project::PropGroup, &'static effectcraft_effects::EffectSpec)],
        size: [f64; 2],
        (dims, offset, scale): ([u32; 2], [f64; 2], f64),
    ) -> Option<([f64; 2], [f64; 2])> {
        if WHOLE_TEXT_CANVAS.load(std::sync::atomic::Ordering::Relaxed) || !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let px = 1.0 / scale;
        // Blur sizes of a layer whose effects run in comp space are in comp pixels.
        let c = fx_comp_px(ctx, layer).unwrap_or([1.0; 2]);
        let c = c[0].max(c[1]).max(1.0);
        let mut lo = [-offset[0] * px, -offset[1] * px];
        let mut hi = [(f64::from(dims[0]) - offset[0]) * px, (f64::from(dims[1]) - offset[1]) * px];
        // CC Radial Fast Blur samples around Center measured from the buffer's offset: a canvas
        // that starts elsewhere rounds it differently (its sample count flips at a few pixels),
        // so the canvas keeps its top-left corner then.
        let mut anchored = false;
        for (_, g, spec) in stack {
            let p = self.effect_params(ctx, layer, g);
            // Generous bounds (crates/effects misc.rs, blur2.rs): a larger region only costs time.
            let grow = match spec.id {
                "ec.blur.gaussian" => 4.0 * effectcraft_effects::gaussian_sigma(p.f("blurriness")).max(0.0) * c,
                "ec.blur.fastbox" => (p.f("radius").max(0.0) * c + px) * p.f("iterations").round().clamp(1.0, 50.0),
                "ec.blur.directional" => p.f("length").abs() * c,
                "ec.stylize.glow" => 4.0 * (p.f("radius").max(0.0) * effectcraft_effects::GLOW_SIGMA_PER_RADIUS * c).max(0.5 * px),
                // The shadow moves by Distance and its blur (σ = Softness / 2) reaches 3σ.
                "ec.perspective.dropshadow" => p.f("distance").abs() + 1.5 * p.f("softness").max(0.0) + 2.0 * px,
                // A Mask Layer is fitted to the buffer, so it keeps the whole canvas.
                "ec.stylize.deepglow" => deep_glow_reach(&p, scale)?,
                // Shade or recolour the layer's own pixels: transparent ones stay transparent.
                "ec.perspective.bevelalpha" | "ec.generate.fill" => 0.0,
                "ec.blur.ccradialfast" => {
                    // A pixel averages the source on its way to Center, at most halfway there:
                    // what it shows lies at most 1 / (1 − Amount/200) times as far out.
                    let k = 1.0 / (1.0 - (p.f("amount") / 100.0).clamp(0.0, 1.0) * 0.5);
                    anchored = true;
                    let o = p.v2("center");
                    for i in 0..2 {
                        lo[i] = lo[i].min(o[i] + (lo[i] - o[i]) * k);
                        hi[i] = hi[i].max(o[i] + (hi[i] - o[i]) * k);
                    }
                    0.0
                }
                // Colour changes that keep transparent pixels transparent, Expression Controls.
                _ if !text_needs_canvas(spec) => 0.0,
                _ => return None,
            } + 4.0 * px;
            lo = lo.map(|v| v - grow);
            hi = hi.map(|v| v + grow);
        }
        let from = if anchored { [0.0; 2] } else { [lo[0].max(0.0), lo[1].max(0.0)] };
        let to = [hi[0].min(size[0]), hi[1].min(size[1])];
        if from.iter().chain(&to).any(|v| !v.is_finite()) {
            return None;
        }
        // Nothing reaches the canvas: the buffer as it is.
        if to[0] < from[0] || to[1] < from[1] {
            return Some(([-offset[0] * px, -offset[1] * px], [-offset[0] * px, -offset[1] * px]));
        }
        Some((from, to))
    }

    /// Run the layer's video effect stack on `target` (in effect space, measured from `origin`
    /// in layer coordinates): runs of GPU-capable effects go to [`FxTarget::gpu`] (when an
    /// accelerator is active and supports them), the rest to [`FxTarget::cpu`] one by one
    /// (quantised to the bit depth after each).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_effect_stack(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        adjustment: bool,
        limit: usize,
        mut timing: Option<&mut Vec<(String, f64)>>,
        origin: [f64; 2],
        target: &mut dyn FxTarget,
        view: Option<[f64; 4]>,
    ) {
        let _profile = crate::profile::span("run_effect_stack");
        if !layer.switches.effects {
            return;
        }
        let Some(fx) = layer.effects() else { return };
        let lt = layer.layer_time(ctx.time);
        let layer_size = ctx.effect_bounds(layer).0;
        let mut mask_shapes = masks::shapes(ctx, layer);
        if origin != [0.0; 2] {
            for q in mask_shapes.iter_mut().flat_map(|m| m.points.iter_mut()) {
                *q = [q[0] - origin[0], q[1] - origin[1]];
            }
        }
        // Capture once, before preceding effects can expand or replace the input. An
        // adjustment stack's original is the comp below, not its solid source.
        let needs_original =
            fx.groups().take(limit).any(|g| g.enabled && matches!(&g.kind, GroupKind::Effect { effect } if effect == "ec.channel.cccomposite"));
        let mut original = None;
        if needs_original {
            target.cpu(&mut |buf| {
                original = Some(buf.clone());
                buf
            });
        }
        let host = FxHost { r: self, ctx, layer, origin, index: 0, original: original.as_ref() };
        let env = EffectEnv {
            masks: &mask_shapes,
            host: Some(&host),
            comp_time: ctx.time.seconds(),
            frame_rate: ctx.comp.frame_rate.as_f64(),
            effect_index: 0,
            working_space: self.pipe.space,
            working_linear: self.pipe.linear,
            shutter: self.mb_on(ctx, layer).then_some((ctx.comp.shutter_angle, ctx.comp.shutter_phase, ctx.comp.motion_blur_samples)),
            // Comp space maps effect space, so it follows the origin.
            comp_space: fx_comp_space(ctx, layer).map(|m| {
                let m = m * Mat3::translate(vec2(origin[0], origin[1]));
                [m.0[0], m.0[1]]
            }),
            comp_px_3d: fx_comp_px_3d(ctx, layer),
        };
        // Video effects in stack order (index, group, spec); disabled and audio effects skipped.
        let stack: Vec<(usize, &effectcraft_project::PropGroup, &'static effectcraft_effects::EffectSpec)> = fx
            .groups()
            .enumerate()
            .take_while(|(i, _)| *i < limit)
            .filter(|(_, g)| g.enabled)
            .filter_map(|(i, g)| match &g.kind {
                GroupKind::Effect { effect } => effectcraft_effects::find(effect).map(|s| (i, g, s)),
                _ => None,
            })
            .filter(|(_, _, s)| !effectcraft_effects::audio_fx::is_audio_effect(s.id))
            // A blur of 0 changes nothing: skipped, it costs no GPU round trip either.
            .filter(|(_, g, s)| !(effectcraft_effects::may_be_identity(s.id) && effectcraft_effects::is_identity(s.id, &self.effect_params(ctx, layer, g))))
            .collect();
        // Text and shape rasterisation is tight to the glyphs or the drawn paths. Distortions
        // and tilers need transparent output pixels beyond that rectangle, on CPU and GPU alike
        // (#659).
        if matches!(layer.source, LayerSource::Text | LayerSource::Shape)
            && !adjustment
            && stack.iter().any(|(_, g, spec)| text_needs_canvas(spec) && !self.transform_moves_nothing(ctx, layer, g, spec))
        {
            let (from, to) = target.extent().and_then(|e| self.canvas_support(ctx, layer, &stack, layer_size, e)).unwrap_or(([0.0; 2], layer_size));
            // Only the part that can show (and what it reads), see [`Renderer::fx_view`].
            let (from, to) = match view {
                Some(v) => ([from[0].max(v[0]), from[1].max(v[1])], [to[0].min(v[2]), to[1].min(v[3])]),
                None => (from, to),
            };
            if to[0] >= from[0] && to[1] >= from[1] {
                target.effect_canvas_in(from, to);
            }
        }
        let accel = self.active_accel();
        let mut k = 0;
        // The static first effects: restored when kept on an earlier frame, else kept once run.
        let mut prefix = self.static_prefix(ctx, layer, &stack, adjustment, origin, view, target.extent());
        if let (Some((p, key)), Some(c)) = (prefix, self.cache_to_keep())
            && target.restore(key, c)
        {
            k = p;
            prefix = None;
        }
        while k < stack.len() {
            if let (Some((p, key)), Some(c)) = (prefix, self.cache_to_keep())
                && k == p
            {
                target.keep(key, c);
                prefix = None;
            }
            let (i, g, spec) = stack[k];
            // A run of GPU effects goes to the accelerator in one go (stopping where the static
            // first effects end, to keep their result).
            let end = prefix.map_or(stack.len(), |(p, _)| p);
            if let Some(a) = accel
                && spec.gpu
                && a.supports_effect(spec.id)
            {
                let run: Vec<_> = stack[k..end].iter().take_while(|(_, _, s)| s.gpu && a.supports_effect(s.id)).collect();
                let params: Vec<Params> = run.iter().map(|(_, g, _)| self.effect_params(ctx, layer, g)).collect();
                let hosts: Vec<_> = run.iter().map(|(i, _, _)| FxHost { index: *i, ..host }).collect();
                let steps: Vec<FxStep> = run
                    .iter()
                    .zip(&params)
                    .zip(&hosts)
                    .map(|(((i, g, s), params), host)| FxStep {
                        spec: s,
                        ctx: EffectCtx {
                            params,
                            time: lt.seconds(),
                            layer_size,
                            seed: g.uid as u32,
                            adjustment,
                            env: EffectEnv { host: Some(host), effect_index: *i, ..env },
                        },
                    })
                    .collect();
                let t0 = web_time::Instant::now();
                let done = target.gpu(&steps);
                if done {
                    if let Some(v) = timing.as_deref_mut() {
                        let ms = t0.elapsed().as_secs_f64() * 1e3 / run.len() as f64;
                        v.extend(run.iter().map(|(_, _, s)| (format!("{} (gpu)", s.id), ms)));
                    }
                    k += run.len();
                    continue;
                }
            }
            k += 1;
            let params = self.effect_params(ctx, layer, g);
            let host = FxHost { index: i, ..host };
            let env = EffectEnv { host: Some(&host), effect_index: i, ..env };
            let ectx = EffectCtx { params: &params, time: lt.seconds(), layer_size, seed: g.uid as u32, adjustment, env };
            let t0 = web_time::Instant::now();
            target.cpu(&mut |buf| {
                let mut buf = effectcraft_effects::apply(spec, &ectx, buf);
                // 8/16 bpc effects write integer pixels.
                self.pipe.quantize(&mut buf.img);
                buf
            });
            if let Some(v) = timing.as_deref_mut() {
                v.push((spec.id.to_string(), t0.elapsed().as_secs_f64() * 1e3));
            }
        }
    }
}

/// Layer units around a pixel that Deep Glow paints and reads (crates/effects misc.rs
/// `deep_glow`): its widest Gaussian (σ = 0.42 × Radius in the display-space profile, stretched
/// by Aspect Ratio) as its box passes spread it, in the downsampled pass and the reduced working
/// resolution of a very wide glow too, plus their resampling. `None` with a Mask Layer (fitted to
/// the buffer, it depends on the whole canvas).
pub(crate) fn deep_glow_reach(p: &Params, scale: f64) -> Option<f64> {
    if p.get("PEDG-0056").and_then(|v| v.as_layer()).is_some() || !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let radius = (p.f("PEDG-0001") * scale).clamp(0.0, 2000.0);
    if !radius.is_finite() || radius < 0.5 {
        return Some(0.0);
    }
    let aspect = p.f("PEDG-0010").clamp(0.0, 2.0);
    let work = (512.0 / radius).min(1.0);
    let sigma = radius * work * 0.42 * aspect.max(2.0 - aspect);
    let support = |s: f64| effectcraft_raster::gaussian_support(s) as f64;
    let mut px = support(sigma) + 2.0;
    let ds = (p.f("PEDG-0019") / 100.0).clamp(0.1, 1.0);
    if ds < 0.999 {
        px = px.max((support(sigma * ds) + 2.0) / ds + 2.0);
    }
    let px = px / work + 2.0 / work + 2.0;
    px.is_finite().then_some(px / scale)
}

/// Where a layer's effect space starts, in layer coordinates (see [`EvalCtx::effect_bounds`]).
/// The comp below an adjustment layer stays as it is, except that a shape layer's effects
/// always measure from its comp-sized canvas centred on the anchor (as After Effects does).
pub(crate) fn fx_origin(ctx: &EvalCtx, layer: &Layer, adjustment: bool) -> [f64; 2] {
    if adjustment && !matches!(layer.source, LayerSource::Shape) { [0.0; 2] } else { ctx.effect_bounds(layer).1 }
}

/// After Effects runs a Continuously Rasterize shape or text layer's effects in comp space:
/// measured with CC Light Sweep, its centre, angle and width stay put in the comp whatever the
/// layer's position, scale and rotation. The layer → comp affine of such a 2D layer (see
/// [`EffectEnv::comp_space`]).
/// Layer units per comp pixel of a continuously rasterised 3D shape or text layer (see
/// `EffectEnv::comp_px_3d`): the projection's local scale at its anchor, camera included.
/// `None` for other layers, or a projection degenerate there.
pub(crate) fn fx_comp_px_3d(ctx: &EvalCtx, layer: &Layer) -> Option<[f64; 2]> {
    if !layer.switches.collapse || !layer.is_3d() || !matches!(layer.source, LayerSource::Shape | LayerSource::Text) {
        return None;
    }
    let h = ctx.layer_to_comp(layer).0.0;
    let a = layer.transform().map(|tr| ctx.v3(layer, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
    // The homography's Jacobian at the anchor: d(x', y') / d(x, y).
    let w = h[2][0] * a[0] + h[2][1] * a[1] + h[2][2];
    if !w.is_finite() || w.abs() < 1e-12 {
        return None;
    }
    let (x, y) = ((h[0][0] * a[0] + h[0][1] * a[1] + h[0][2]) / w, (h[1][0] * a[0] + h[1][1] * a[1] + h[1][2]) / w);
    let j = [[(h[0][0] - x * h[2][0]) / w, (h[0][1] - x * h[2][1]) / w, 0.0], [(h[1][0] - y * h[2][0]) / w, (h[1][1] - y * h[2][1]) / w, 0.0]];
    let det = j[0][0] * j[1][1] - j[0][1] * j[1][0];
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    Some(EffectEnv { comp_space: Some(j), ..Default::default() }.comp_px())
}

/// Effects that read `EffectEnv::comp_px`: their sizes are comp pixels on a continuously
/// rasterised layer.
const COMP_PX_EFFECTS: &[&str] = &["ec.blur.gaussian", "ec.blur.fastbox", "ec.stylize.glow"];

/// Effects that place what they draw in comp space on a continuously rasterised 2D layer
/// (`EffectEnv::comp_space`: Gradient Ramp's points, CC Light Sweep's centre).
const COMP_SPACE_EFFECTS: &[&str] = &["ec.generate.gradientramp", "ec.generate.cclightsweep"];

/// The layer → comp affine that places a [`COMP_SPACE_EFFECTS`] effect of the layer, when it
/// runs one ([`fx_comp_space`]): its pixels move with it, unlike the rest of its content.
pub(crate) fn comp_space_placement(ctx: &EvalCtx, layer: &Layer) -> Option<Mat3> {
    let runs = layer.switches.effects
        && layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| g.enabled && matches!(&g.kind, GroupKind::Effect { effect } if COMP_SPACE_EFFECTS.contains(&effect.as_str())))
        });
    if runs { fx_comp_space(ctx, layer) } else { None }
}

/// Does the layer run an effect sized in comp pixels ([`COMP_PX_EFFECTS`])?
pub(crate) fn sizes_in_comp_px(layer: &Layer) -> bool {
    layer.switches.effects
        && layer
            .effects()
            .is_some_and(|fx| fx.groups().any(|g| g.enabled && matches!(&g.kind, GroupKind::Effect { effect } if COMP_PX_EFFECTS.contains(&effect.as_str()))))
}

/// Layer units per comp pixel of a layer whose effects size things in comp pixels: a 2D one in
/// comp space ([`fx_comp_space`]) or a 3D one ([`fx_comp_px_3d`]); `None` = layer pixels.
pub(crate) fn fx_comp_px(ctx: &EvalCtx, layer: &Layer) -> Option<[f64; 2]> {
    match fx_comp_space(ctx, layer) {
        Some(m) => Some(EffectEnv { comp_space: Some([m.0[0], m.0[1]]), ..Default::default() }.comp_px()),
        None => fx_comp_px_3d(ctx, layer),
    }
}

pub(crate) fn fx_comp_space(ctx: &EvalCtx, layer: &Layer) -> Option<Mat3> {
    if !comp_space_layer(layer) {
        return None;
    }
    Some(ctx.layer_to_comp(layer).0)
}

/// A 2D Continuously Rasterize shape or text layer: it runs its effects in comp space
/// ([`fx_comp_space`]).
fn comp_space_layer(layer: &Layer) -> bool {
    layer.switches.collapse && !layer.is_3d() && matches!(layer.source, LayerSource::Shape | LayerSource::Text)
}

/// Are the point parameters of `effect` on `layer` comp pixels (`COMP_SPACE_EFFECTS` on a 2D
/// Continuously Rasterize shape or text layer: Gradient Ramp's points, CC Light Sweep's centre)
/// rather than effect space? Comp edits that move the picture (crop, resize with an anchor) move
/// such points with it.
pub fn effect_points_in_comp_space(layer: &Layer, effect: &str) -> bool {
    COMP_SPACE_EFFECTS.contains(&effect) && comp_space_layer(layer)
}

/// Does `layer`'s effect space start at an origin set by the comp's size (`fx_origin`,
/// [`crate::effect_bounds`]): a shape or text layer, whose effects run on a comp-sized canvas
/// centred on the layer? Its effect point parameters must move by half the change when the
/// comp is resized, to stay where they are on the layer.
pub fn effect_space_follows_comp_size(project: &effectcraft_project::Project, layer: &Layer) -> bool {
    !(layer.switches.adjustment && !matches!(layer.source, LayerSource::Shape)) && crate::eval::source_size(project, layer).0 == 0
}

/// Test switch: every distortion gets the comp-sized text canvas (the reference for
/// `Renderer::transform_moves_nothing`).
pub(crate) static ALWAYS_TEXT_CANVAS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Test switch: text and shape effects run on the whole comp-sized canvas (the reference for
/// `Renderer::canvas_support`).
pub(crate) static WHOLE_TEXT_CANVAS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Colour-only effects and expression controls neither move pixels nor create coverage.
/// The colour effects listed here can modify zero alpha, so keep their larger canvas.
pub(crate) fn text_needs_canvas(spec: &effectcraft_effects::EffectSpec) -> bool {
    match spec.category {
        "Expression Controls" => false,
        "Color Correction" => matches!(
            spec.id,
            "ec.color.levels" | "ec.color.levelsic" | "ec.color.curves" | "ec.color.colorlink" | "ec.color.colorama" | "ec.color.psarbitrarymap"
        ),
        _ => true,
    }
}

/// Bounded extent and integer copy offset for extending a buffer to its effect canvas.
/// `None` means unchanged, invalid input, or over budget.
pub fn effect_canvas_extent(dimensions: [u32; 2], offset: [f64; 2], scale: f64, size: [f64; 2]) -> Option<([u32; 2], [u32; 2])> {
    effect_canvas_extent_in(dimensions, offset, scale, [0.0; 2], size)
}

/// [`effect_canvas_extent`] of the canvas region from `from` to `to` (effect-space layer units).
pub fn effect_canvas_extent_in(dimensions: [u32; 2], offset: [f64; 2], scale: f64, from: [f64; 2], to: [f64; 2]) -> Option<([u32; 2], [u32; 2])> {
    // min/max would otherwise hide NaN inputs before the allocation checks.
    if !scale.is_finite() || scale <= 0.0 || offset.iter().chain(&from).chain(&to).any(|v| !v.is_finite()) || to[0] < from[0] || to[1] < from[1] {
        log::warn!("Invalid text effect canvas bounds; keeping the existing extent");
        return None;
    }
    let lo = [(offset[0] + from[0] * scale).floor().min(0.0), (offset[1] + from[1] * scale).floor().min(0.0)];
    let hi = [(offset[0] + to[0] * scale).ceil().max(dimensions[0] as f64), (offset[1] + to[1] * scale).ceil().max(dimensions[1] as f64)];
    let (w, h) = (hi[0] - lo[0], hi[1] - lo[1]);
    if !w.is_finite() || !h.is_finite() || w > 16384.0 || h > 16384.0 || w * h > 64_000_000.0 {
        log::warn!("Text effect canvas exceeds the render limit; reduce the composition size or preview resolution");
        return None;
    }
    if lo == [0.0; 2] && w == dimensions[0] as f64 && h == dimensions[1] as f64 {
        return None;
    }
    Some(([w as u32, h as u32], [-lo[0] as u32, -lo[1] as u32]))
}

/// Extend without moving or resampling the original glyph pixels.
#[cfg(test)]
pub(crate) fn text_effect_canvas(buf: Buf, size: [f64; 2]) -> Buf {
    text_effect_canvas_in(buf, [0.0; 2], size)
}

/// [`text_effect_canvas`] of the canvas region from `from` to `to` (effect-space layer units).
pub(crate) fn text_effect_canvas_in(mut buf: Buf, from: [f64; 2], to: [f64; 2]) -> Buf {
    let Some(([width, height], pad)) = effect_canvas_extent_in([buf.img.width, buf.img.height], buf.offset, buf.scale, from, to) else { return buf };
    let Some(count) = (width as usize).checked_mul(height as usize) else { return buf };
    let mut data = Vec::new();
    if data.try_reserve_exact(count).is_err() {
        log::warn!("Not enough memory to extend text effects; reduce the preview resolution");
        return buf;
    }
    data.resize(count, [0.0; 4]);
    let mut img = Image { width, height, data };
    use rayon::prelude::*;
    img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            *px = buf.img.get(x as i64 - pad[0] as i64, y as i64 - pad[1] as i64);
        }
    });
    buf.img = img;
    buf.offset[0] += pad[0] as f64;
    buf.offset[1] += pad[1] as f64;
    buf
}

#[cfg(test)]
mod text_canvas_tests {
    use super::*;

    #[test]
    fn expansion_keeps_the_pixel_grid_and_existing_extent() {
        let source = Buf { img: Image::filled(3, 2, [0.2, 0.3, 0.4, 1.0]), offset: [-2.25, 1.5], scale: 0.5 };
        let out = text_effect_canvas(source.clone(), [20.0, 10.0]);
        assert_eq!((out.img.width, out.img.height), (11, 7));
        assert_eq!(out.offset, [0.75, 1.5]);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(out.img.get(x + 3, y), source.img.get(x, y));
            }
        }
        assert_eq!(out.img.get(0, 0), [0.0; 4]);
        assert_eq!(out.img.get(10, 6), [0.0; 4]);
    }

    #[test]
    fn invalid_canvas_coordinates_and_unchanged_extent_need_no_expansion() {
        for offset in [[f64::NAN, 0.0], [0.0, f64::INFINITY], [-f64::MAX, 0.0]] {
            assert_eq!(effect_canvas_extent([3, 2], offset, 1.0, [20.0, 10.0]), None);
        }
        for scale in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert_eq!(effect_canvas_extent([3, 2], [0.0; 2], scale, [20.0, 10.0]), None);
        }
        assert_eq!(effect_canvas_extent([3, 2], [0.0; 2], 1.0, [3.0, 2.0]), None);
        assert_eq!(effect_canvas_extent([3, 2], [0.25; 2], 0.5, [2.0, 2.0]), None);
        assert_eq!(effect_canvas_extent([u32::MAX, 2], [0.0; 2], 1.0, [3.0, 2.0]), None);
    }

    #[test]
    fn oversized_canvas_keeps_existing_pixels() {
        let source = Buf { img: Image::filled(3, 2, [0.2, 0.3, 0.4, 1.0]), offset: [0.0; 2], scale: 1.0 };
        for size in [[f64::MAX, 10.0], [10.0, f64::INFINITY], [f64::NAN, 10.0], [10.0, -1.0], [16000.0; 2]] {
            let out = text_effect_canvas(source.clone(), size);
            assert_eq!(out.img, source.img);
            assert_eq!(out.offset, source.offset);
        }
    }
}
