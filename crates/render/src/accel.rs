//! Accelerated compositors (`effectcraft-gpu`): the backend choice, the [`Accelerator`] and
//! [`FxTarget`] traits, and the pieces of the CPU walk they reuse or fall back to.

use crate::fx::fx_origin;
use crate::{AutoPick, EvalCtx, Image, LayerTiming, Renderer, StyledLayer, cache, color, content_bounds, masks, three_d};
use effectcraft_effects::{Buf, EffectCtx};
use effectcraft_geom::{Mat3, vec2};
use effectcraft_project::{GroupKind, ItemId, Layer, MatteKind};
use effectcraft_time::Tick;
use std::sync::Arc;

/// Which compositor renders a frame (Project Settings ▸ Video Rendering and Effects).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The CPU reference compositor (Mercury Software Only).
    #[default]
    Cpu,
    /// The GPU compositor whenever an [`Accelerator`] is attached, whatever the project says.
    Gpu,
    /// The GPU compositor when an [`Accelerator`] is attached and the project's renderer is
    /// Mercury GPU Acceleration (the viewer's choice); the CPU otherwise.
    Auto,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Backend> {
        match s.to_ascii_lowercase().as_str() {
            "cpu" | "software" | "mercury software only" => Some(Backend::Cpu),
            "gpu" | "mercury gpu acceleration" => Some(Backend::Gpu),
            "auto" => Some(Backend::Auto),
            _ => None,
        }
    }
}

/// One effect of a GPU effect chain (see [`Accelerator::effects`]).
pub struct FxStep<'x> {
    pub spec: &'static effectcraft_effects::EffectSpec,
    pub ctx: EffectCtx<'x>,
}

/// A GPU (or other hardware) backend for the compositor. The CPU [`Renderer`] stays the
/// reference: an accelerator renders what it supports and returns `None` for the rest, which
/// then runs on the CPU. Implemented by `effectcraft-gpu`.
pub trait Accelerator: Send + Sync {
    /// Adapter / backend name for status readouts ("Apple M2 (Metal)").
    fn name(&self) -> String;
    /// Render a top-level comp frame (what [`Renderer::comp_frame`] returns). `None` = not
    /// handled (the CPU renders it).
    fn comp_frame(&self, r: &Renderer, comp: ItemId, t: Tick) -> Option<Image>;
    /// The accelerator implements effect `id` with the CPU effect's semantics.
    fn supports_effect(&self, id: &str) -> bool;
    /// Run a chain of supported effects on `buf`, clamping and quantising to `levels` after
    /// every effect when set (8/16 bpc). `None` = not handled.
    fn effects(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<Buf>;
    /// [`Self::effects`], keeping the result's image on the accelerator under the returned
    /// ticket (`None` = not kept): the renderer hands the ticket back to [`Self::adopt`] once
    /// the pixels are stored unchanged in a shared buffer, or to [`Self::release`].
    /// Optional cache ownership; backends without a cache keep their eager CPU contract.
    fn effects_kept_owned(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>, _owner: Option<&crate::cache::GpuOwner>) -> Option<(Buf, Option<u64>)> {
        self.effects_kept(chain, buf, levels)
    }
    fn effects_kept(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<(Buf, Option<u64>)> {
        self.effects(chain, buf, levels).map(|b| (b, None))
    }
    /// `buf` holds kept result `ticket`'s pixels: its compositor may draw `buf` from the kept
    /// image (while `buf` lives) instead of uploading it again.
    fn adopt(&self, _ticket: u64, _buf: &Arc<Buf>) {}
    /// Forget kept result `ticket`.
    fn release(&self, _ticket: u64) {}
    /// Rasterise and shade an Advanced 3D scene at its raster size (depth buffer, PBR, image
    /// based light, shadow maps), with the CPU rasteriser's semantics
    /// ([`three_d::adv::raster::render`]). `None` = not handled (the CPU renders it).
    fn raster_3d(&self, _scene: &three_d::adv::Scene) -> Option<three_d::adv::Target> {
        None
    }
    /// Render a whole prepared Advanced 3D run (every motion-blur sub-sample rasterised and
    /// resolved, averaged, depth of field, encoding) with [`three_d::adv::render_prepared`]'s
    /// semantics. `None` = not handled (the CPU path runs, using [`Self::raster_3d`]).
    fn render_3d(&self, _run: &three_d::adv::Prepared) -> Option<three_d::adv::Rendered> {
        None
    }
    /// A particle simulation backend (GPU particles) for the stepped particle effects, with
    /// the CPU simulation's semantics. `None` = they simulate on the CPU.
    fn particles(&self) -> Option<&dyn effectcraft_effects::psim::ParticleSim> {
        None
    }
    /// Blur a mask's `width`×`height` coverage plane for its feather ([`masks::FeatherBlur`],
    /// with the CPU blur's semantics) and return the `crop` window (x, y, width, height) of the
    /// result, row-major. `None` = not handled (the CPU blurs it).
    fn mask_feather(&self, _plane: &[f32], _width: u32, _height: u32, _blur: masks::FeatherBlur, _crop: [u32; 4]) -> Option<Vec<f32>> {
        None
    }
    /// Timing history [`Backend::Auto`] uses to send each comp's top-level frames to the
    /// faster compositor. `None` = Auto always tries the accelerator first.
    fn auto_pick(&self) -> Option<&AutoPick> {
        None
    }
    /// False after permanent device failure: exports can release their GPU concurrency cap.
    fn available(&self) -> bool {
        true
    }
    /// At most how many frames should render on the accelerator at once, after it ran out of
    /// memory (CR1). `None`: no limit of its own.
    fn max_inflight(&self) -> Option<usize> {
        None
    }
    /// Deferred readbacks: where a GPU readback cannot be waited for (WebGPU in a browser
    /// worker), a frame renders in *passes*. A readback the accelerator does not have yet is
    /// started, the step returns a cheap placeholder and the pass is marked missed
    /// ([`Self::pass_missed`]); the caller waits for the readbacks in flight (asynchronously)
    /// and renders the frame again, and the steps whose results arrived are served from them.
    /// The frame is done after a pass with no miss. `frame_begin` forgets the previous frame's
    /// results, `pass_begin` clears the miss. Synchronous accelerators never miss.
    fn frame_begin(&self) {}
    fn pass_begin(&self) {}
    fn pass_missed(&self) -> bool {
        false
    }
    /// Raised from a pass's first miss to its end (results computed meanwhile may hold
    /// placeholders): give it to [`LayerCache::set_gate`] so they are not cached.
    fn miss_gate(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        None
    }
    /// With deferred readbacks: a future that resolves once the readbacks in flight have
    /// landed (the browser delivers them from its event loop; natively the device is polled
    /// before it returns, so the future is ready). `None` = nothing to wait for. See
    /// [`passes::in_passes`].
    fn settle(&self) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>> {
        None
    }
}

/// Where an effect stack runs (see [`Renderer::run_effects_on`]): a CPU buffer, or an image
/// resident on an accelerator.
pub trait FxTarget {
    /// Run a chain of accelerator-supported effects (quantising after each to the bit depth).
    /// `false` = not handled: the effects then run one by one through [`FxTarget::cpu`].
    fn gpu(&mut self, steps: &[FxStep]) -> bool;
    /// Run one CPU effect: `f` maps the buffer.
    fn cpu(&mut self, f: &mut dyn FnMut(Buf) -> Buf);
    /// Extend the text effect canvas without resampling existing pixels. Backends may
    /// retain their resident image; the default preserves the CPU reference behavior.
    fn effect_canvas(&mut self, size: [f64; 2]) {
        self.effect_canvas_in([0.0; 2], size);
    }
    /// [`Self::effect_canvas`] of the region from `lo` to `hi` (effect-space layer units)
    /// only: the part of the canvas the effects can make non-transparent.
    fn effect_canvas_in(&mut self, lo: [f64; 2], hi: [f64; 2]) {
        self.cpu(&mut |buf| crate::fx::text_effect_canvas_in(buf, lo, hi));
    }
    /// The buffer's size, offset and scale, when known without reading it back.
    fn extent(&self) -> Option<([u32; 2], [f64; 2], f64)> {
        None
    }
    /// Move the buffer's effect coordinates: subtract `d` (buffer px) from its offset.
    fn shift(&mut self, d: [f64; 2]);
    /// Keep the buffer as it is now (the result of the stack's static first effects, see
    /// `Renderer::static_prefix`) under `key` in `cache`. Default: not kept.
    fn keep(&mut self, _key: u64, _cache: &crate::cache::LayerCache) {}
    /// Replace the buffer with the one kept under `key`; `false` (the buffer unchanged) when
    /// none is.
    fn restore(&mut self, _key: u64, _cache: &crate::cache::LayerCache) -> bool {
        false
    }
}

/// Hooks for accelerated compositors (`effectcraft-gpu`): the pieces of the CPU walk they reuse
/// or fall back to. The CPU compositor itself is [`Renderer::comp_frame_cpu`].
impl<'a> Renderer<'a> {
    /// Conservative source-free preflight guard. Expressions can call sampleImage,
    /// so a planning pass declines them rather than evaluating a different host.
    pub fn effect_plan_safe(layer: &Layer, max_nodes: usize) -> bool {
        fn walk(group: &effectcraft_project::PropGroup, left: &mut usize, depth: usize) -> bool {
            if depth > 32 || group.children.len() > *left {
                return false;
            }
            *left -= group.children.len();
            group.children.iter().all(|node| match node {
                effectcraft_project::Node::Prop(p) => !p.has_expression(),
                effectcraft_project::Node::Group(g) => walk(g, left, depth + 1),
            })
        }
        walk(&layer.props, &mut max_nodes.min(4096), 0)
    }

    /// Visit evaluated GPU runs without executing pixel closures. The effect limit
    /// is checked before run_effect_stack collects its stack or parameters.
    /// Bounds use project metadata; expression-bearing layers are declined.
    pub fn visit_gpu_effect_runs(&self, ctx: &EvalCtx, layer: &Layer, max_effects: usize, visitor: &mut dyn FnMut(&[FxStep])) -> bool {
        let limit = max_effects.min(256);
        if ctx.expr.is_some()
            || !Self::effect_plan_safe(layer, 4096)
            || layer.effects().is_some_and(|g| g.groups().take(limit.saturating_add(1)).count() > limit)
        {
            return false;
        }
        struct Plan<'v>(&'v mut dyn FnMut(&[FxStep]));
        impl FxTarget for Plan<'_> {
            fn gpu(&mut self, steps: &[FxStep]) -> bool {
                (self.0)(steps);
                true
            }
            fn cpu(&mut self, _: &mut dyn FnMut(Buf) -> Buf) {}
            fn shift(&mut self, _: [f64; 2]) {}
        }
        let adjustment = layer.switches.adjustment;
        self.run_effect_stack(ctx, layer, adjustment, limit, None, fx_origin(ctx, layer, adjustment), &mut Plan(visitor), None);
        true
    }

    /// The project's colour pipeline.
    pub fn pipe(&self) -> color::Pipe {
        self.pipe
    }

    /// Nesting depth (0 = the top-level comp).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// The auxiliary 3D channels of comp `comp_id` at comp time `t` (depth, layer IDs, normals,
    /// UVs, Cryptomatte; see `three_d::compose::aux_pass`), at the renderer's output scale.
    pub fn comp_aux(&self, comp_id: ItemId, t: Tick) -> Option<effectcraft_raster::AuxChannels> {
        let ctx = self.eval_ctx(comp_id, t)?;
        Some(three_d::compose::aux_pass(self, &ctx))
    }

    /// Evaluation context of comp `comp_id` at comp time `t`.
    pub fn eval_ctx(&self, comp_id: ItemId, t: Tick) -> Option<EvalCtx<'a>> {
        Some(self.ctx(comp_id, self.project.comp(comp_id)?, t))
    }

    /// The layers drawn at the context time, bottom to top (solo, guides and visibility
    /// applied).
    pub fn visible_layers(&self, ctx: &EvalCtx<'a>) -> Vec<&'a Layer> {
        let (comp, t) = (ctx.comp, ctx.time);
        let any_solo = comp.layers.iter().any(|l| l.switches.solo && l.source.is_av());
        comp.layers
            .iter()
            .rev()
            .filter(|l| l.is_active_at(t) && l.has_video() && !l.environment && (!any_solo || l.switches.solo) && (self.opts.guides || !l.switches.guide))
            .collect()
    }

    /// The comp's visible layers (bottom to top) in drawing passes, each with the time it is
    /// drawn at. A 2D adjustment layer with Posterize Time holds everything below it (and itself)
    /// on its frame grid, as After Effects does: SceneYard UI Google search's "Glow / Posterize
    /// Time" adjustment steps the Gaussian Blur adjustment and the layers under it to 15 fps.
    /// Above that layer the comp runs at the real time. Effects of other layers see their own
    /// pass's time.
    pub fn draw_passes(&self, ctx: &EvalCtx<'a>) -> Vec<(EvalCtx<'a>, Vec<&'a Layer>)> {
        let mut out = Vec::new();
        self.collect_passes(ctx, None, 0, &mut out);
        out
    }

    fn collect_passes(&self, ctx: &EvalCtx<'a>, below: Option<usize>, depth: usize, out: &mut Vec<(EvalCtx<'a>, Vec<&'a Layer>)>) {
        let comp = ctx.comp;
        let index = |l: &Layer| comp.layers.iter().position(|x| std::ptr::eq(x, l)).unwrap_or(0);
        let visible: Vec<&'a Layer> = self.visible_layers(ctx).into_iter().filter(|l| below.is_none_or(|b| index(l) >= b)).collect();
        let split = if depth < 16 {
            visible.iter().enumerate().rev().find_map(|(k, l)| {
                let t = self.posterized_comp_time(ctx, l)?;
                ((t.0 - ctx.time.0).abs() > 0).then_some((k, t))
            })
        } else {
            None
        };
        match split {
            Some((k, t)) => {
                self.collect_passes(&ctx.at(t), Some(index(visible[k])), depth + 1, out);
                out.push((*ctx, visible.get(k + 1..).unwrap_or_default().to_vec()));
            }
            None => out.push((*ctx, visible)),
        }
    }

    /// The comp time a 2D adjustment layer's first enabled Posterize Time holds at `ctx`.
    fn posterized_comp_time(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Tick> {
        if !layer.switches.adjustment || layer.is_3d() || !layer.switches.effects {
            return None;
        }
        let g = layer.effects()?.groups().find(|g| g.enabled && matches!(&g.kind, GroupKind::Effect { effect } if effect == "ec.time.posterizetime"))?;
        let rate = self.effect_params(ctx, layer, g).f("frameRate");
        let lt = layer.layer_time(ctx.time).seconds();
        let held = effectcraft_effects::posterized_time(lt, rate);
        if !held.is_finite() {
            return None;
        }
        // The held time lands on the comp's next frame (SceneYard Twisting text gradient: 20 fps
        // in a 30 fps comp holds frame 44 at frame 44, not at 43.5, so its footage keeps frame
        // 44's picture).
        let t = layer.comp_time(Tick::from_seconds_f64(held)).seconds();
        let fps = ctx.comp.frame_rate.as_f64();
        let t = if fps > 0.0 { (t * fps - 1e-6).ceil() / fps } else { t };
        t.is_finite().then(|| Tick::from_seconds_f64(t))
    }

    /// Draw a run of consecutive 3D layers into `canvas` (CPU).
    pub fn draw_3d_run(&self, ctx: &EvalCtx<'a>, run: &[&Layer], canvas: &mut Image) {
        three_d::compose::draw_run(self, ctx, run, canvas);
    }

    /// A run of consecutive 3D layers prepared for an accelerator (planes, lights, shadow
    /// casters; see [`three_d::Run3d`]) at output size `out`. `None` = draw it with
    /// [`Self::draw_3d_run`] (adjustment or wireframe layers in the run, Advanced 3D).
    pub fn prepare_3d_run(&self, ctx: &EvalCtx<'a>, run: &[&Layer], out: (u32, u32)) -> Option<three_d::Run3d> {
        three_d::compose::gpu_run(self, ctx, run, out)
    }

    /// [`Self::prepare_3d_run`] with the planes of some layers already made by the accelerator
    /// (`bufs`: their size, offset and scale; their pixels stay on the device): those layers'
    /// buffers aren't rendered on the CPU.
    pub fn prepare_3d_run_with(&self, ctx: &EvalCtx<'a>, run: &[&Layer], out: (u32, u32), bufs: &three_d::PlaneBufs) -> Option<three_d::Run3d> {
        three_d::compose::gpu_run(&Renderer { plane_bufs: Some(bufs), ..*self }, ctx, run, out)
    }

    /// The layer-space rectangle of a classic 3D layer's plane that an `out` canvas can show
    /// (see `three_d::compose::plane_view`): an accelerator making the plane need not make the
    /// rest. `None` = no such bound (the whole plane may matter).
    pub fn plane_view_rect(&self, ctx: &EvalCtx<'a>, layer: &Layer, out: (u32, u32)) -> Option<[f64; 4]> {
        three_d::compose::plane_view(self, ctx, layer, out)
    }

    /// A run of consecutive 3D layers of an Advanced 3D comp prepared for an accelerator's
    /// compositor ([`three_d::adv::Prepared`]): rendered as one scene and composited (Normal,
    /// premultiplied over) onto the canvas. `None` = not an Advanced 3D comp, or a layer needs the
    /// 2D compositing path (blend mode, track matte, Preserve Transparency): draw it with
    /// [`Self::draw_3d_run`].
    pub fn prepare_adv_run<'p>(&'p self, ctx: &'p EvalCtx<'a>, run: &'p [&'p Layer], out: (u32, u32)) -> Option<three_d::adv::Prepared<'p, 'a>> {
        three_d::adv::prepare_run(self, ctx, run, out)
    }

    /// A run of consecutive 3D layers of an Advanced 3D comp whose layers need the 2D
    /// compositing path (blend mode, track matte, Preserve Transparency), split for an
    /// accelerator's compositor ([`three_d::adv::SplitRun`]). `None` = [`Self::prepare_adv_run`]
    /// covers it, or it isn't an Advanced 3D run.
    pub fn split_adv_run<'p>(&'p self, ctx: &'p EvalCtx<'a>, run: &'p [&'p Layer], out: (u32, u32)) -> Option<three_d::adv::SplitRun<'p, 'a>> {
        three_d::adv::split_run(self, ctx, run, out)
    }

    /// Whether the comp's 3D layers draw with the Advanced 3D renderer.
    pub fn is_adv3d(&self, ctx: &EvalCtx) -> bool {
        three_d::adv::active(self, ctx)
    }

    /// An Environment Light Background layer ready to draw on an accelerator
    /// ([`three_d::SkyDraw`]; the CPU draws it in `draw_3d_run`). `None` = nothing to draw.
    pub fn sky_draw(&self, ctx: &EvalCtx, layer: &Layer) -> Option<three_d::SkyDraw> {
        three_d::compose::sky_draw(self, ctx, layer)
    }

    /// The pixels a Wireframe-quality layer sets (white, opaque) on a `size` canvas: its
    /// bounds' outline, one pixel wide. Empty when the layer has no bounds.
    pub fn wireframe_pixels(&self, ctx: &EvalCtx, layer: &Layer, size: (u32, u32)) -> Vec<(u32, u32)> {
        let Some(b) = content_bounds(ctx, layer) else { return vec![] };
        let s = self.opts.scale;
        let (l2c, _) = ctx.layer_to_comp(layer);
        let m = Mat3::scale(vec2(s, s)) * self.outer.map_or(l2c, |o| o * l2c);
        let c = [(b[0], b[1]), (b[2], b[1]), (b[2], b[3]), (b[0], b[3])].map(|(x, y)| m.apply(vec2(x, y)));
        let (w, h) = (size.0 as i64, size.1 as i64);
        let mut out = vec![];
        for i in 0..4 {
            let (p, q) = (c[i], c[(i + 1) % 4]);
            let n = ((q.x - p.x).abs().max((q.y - p.y).abs()) * 2.0).ceil().clamp(1.0, 1.0e5) as usize;
            for j in 0..=n {
                let t = j as f64 / n as f64;
                let (x, y) = ((p.x + (q.x - p.x) * t).floor() as i64, (p.y + (q.y - p.y) * t).floor() as i64);
                if x >= 0 && y >= 0 && x < w && y < h {
                    out.push((x as u32, y as u32));
                }
            }
        }
        out
    }

    /// Run the layer's effect stack on `target` (an accelerator's resident image): GPU-capable
    /// runs through [`FxTarget::gpu`], the others through [`FxTarget::cpu`], with the CPU's
    /// parameters and environment. `adjustment`: the stack runs on the comp below (adjustment
    /// layers).
    pub fn run_effects_on(&self, ctx: &EvalCtx, layer: &Layer, adjustment: bool, target: &mut dyn FxTarget) {
        // `target` is the comp below in the layer's space for adjustment layers (the callers);
        // a shape layer's effects still measure from its anchor-centred canvas ([`fx_origin`]).
        let origin = fx_origin(ctx, layer, adjustment);
        let d = [origin[0] * self.opts.scale, origin[1] * self.opts.scale];
        target.shift([-d[0], -d[1]]);
        self.run_effect_stack(ctx, layer, adjustment, usize::MAX, None, origin, target, None);
        target.shift(d);
    }

    /// [`Renderer::run_effects_on`] for a (non-adjustment) layer's own pixels: `target` holds
    /// them in effect space (as [`Renderer::layer_input`] hands them out) and the result stays
    /// there. Returns the effect-space origin in layer coordinates (rebase by its negation).
    pub fn run_layer_effects_on(&self, ctx: &EvalCtx, layer: &Layer, target: &mut dyn FxTarget) -> [f64; 2] {
        self.run_layer_effects_in(ctx, layer, target, crate::ContentClip::default())
    }

    /// [`Self::run_layer_effects_on`] for a source made in `clip` ([`Self::resident_content`]):
    /// a text or shape layer's effect canvas only where it can show, as its key says.
    pub fn run_layer_effects_in(&self, ctx: &EvalCtx, layer: &Layer, target: &mut dyn FxTarget, clip: crate::ContentClip) -> [f64; 2] {
        let origin = fx_origin(ctx, layer, false);
        self.run_effect_stack(ctx, layer, false, usize::MAX, None, origin, target, clip.view());
        origin
    }

    /// A precomp layer's nested comp for an accelerator to render resident (no CPU render and
    /// upload of the nested frame): the renderer one level down, the comp, its time and the
    /// buffer scale, exactly as [`Renderer::source`]'s comp branch would render it. `None`
    /// when that branch does more (composition proxy, Essential Properties overrides, frame
    /// blending, recursion, motion-blur sub-frames) or draws nothing (outside the nested comp's
    /// span): use the CPU path.
    pub fn precomp_source(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(Renderer<'a>, ItemId, Tick, f64)> {
        let effectcraft_project::LayerSource::Comp { item } = layer.source else { return None };
        if layer.switches.adjustment || self.project.comp_contains(item, ctx.comp_id) {
            return None;
        }
        let nc = self.project.comp(item)?;
        let lt = ctx.nested_time(layer);
        if self.content_times(ctx, layer).is_some() {
            return None;
        }
        if !nc.covers(lt)
            || self.proxy_for(item).is_some()
            || !crate::essential_overrides(ctx, layer).is_empty()
            || crate::footage::frame_blend_mode(ctx, layer) != effectcraft_project::FrameBlend::Off
        {
            return None;
        }
        let (sub, bs) = self.precomp_renderer(layer, item);
        Some((sub, item, lt, bs))
    }

    /// The exact CPU mask coverage, without allocating a source RGBA image.
    pub fn source_mask(&self, ctx: &EvalCtx, layer: &Layer, geometry: &masks::Geometry) -> Option<effectcraft_raster::Mask> {
        masks::combined(ctx, layer, geometry, self.mask_blur(ctx, layer))
    }

    /// An adjustment layer's footprint (its source with masks applied, layer space): where
    /// its effects show. `None` = the layer changes nothing.
    pub fn adjustment_footprint(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Arc<Buf>> {
        // Cached, so an accelerator sees the same buffer (and uploads it once) every frame.
        let key = self.cache.and_then(|_| cache::footprint_key(ctx, layer, self.raster_scale(ctx, layer), self.opts.draft, self.mb_on(ctx, layer)));
        if let (Some(c), Some(k)) = (self.cache, key)
            && let Some(b) = c.get(k)
        {
            return Some(b);
        }
        let mut foot = self.source(ctx, layer)?;
        let _ = masks::apply(ctx, layer, &mut foot, self.mask_blur(ctx, layer));
        let foot = Arc::new(foot);
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            c.insert(k, foot.clone());
        }
        Some(foot)
    }

    /// Draw one 2D layer into `canvas` on the CPU (any kind: adjustment, wireframe, styled…).
    pub fn draw_layer_cpu(&self, ctx: &EvalCtx, layer: &Layer, canvas: &mut Image) {
        self.draw_layer(ctx, layer, canvas, false);
    }

    /// Draw a collapsed precomp layer into `canvas` on the CPU.
    pub fn draw_collapsed_cpu(&self, ctx: &EvalCtx<'a>, layer: &Layer, item: ItemId, canvas: &mut Image) {
        self.draw_collapsed(ctx, layer, item, canvas, false);
    }

    /// The layer's finished buffers (styles included) in the blending space.
    pub fn styled_layer(&self, ctx: &EvalCtx, layer: &Layer) -> Option<StyledLayer> {
        let mut timing = self.profile.map(|_| LayerTiming { depth: self.depth, layer: layer.name.clone(), ..Default::default() });
        let t0 = web_time::Instant::now();
        let st = self.styled_blend(ctx, layer, timing.as_mut());
        if let (Some(prof), Some(mut timing)) = (self.profile, timing) {
            timing.process_ms = t0.elapsed().as_secs_f64() * 1e3;
            if let Ok(mut v) = prof.lock() {
                v.push(timing);
            }
        }
        st
    }

    /// Opacity the layer composites with (its Opacity × collapsed precomps' opacity).
    pub fn layer_opacity(&self, ctx: &EvalCtx, layer: &Layer) -> f32 {
        ctx.opacity(layer) as f32 * self.opacity_mul
    }

    /// The layer's track matte layer and kind, if it has a valid one.
    pub fn track_matte<'c>(&self, ctx: &EvalCtx<'c>, layer: &Layer) -> Option<(&'c Layer, MatteKind)> {
        layer.track_matte.and_then(|tm| ctx.comp.layer(tm.layer).filter(|m| m.id != layer.id).map(|m| (m, tm.kind)))
    }
}
