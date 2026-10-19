//! The [`Renderer`], its options and the top-level comp walk: a frame bottom-to-top over
//! the visible layers, 3D runs depth-sorted, and the region of interest.

use crate::collapse::{Collapse3d, MAX_THROUGH, Through};
use crate::{Accelerator, AutoKey, AutoPick, Backend, EvalCtx, ExprHost, FootageSource, Image, LayerCache, NoFootage, color, three_d};
use effectcraft_geom::Mat3;
use effectcraft_project::{Comp, ItemId, Layer, LayerId, Project, Quality};
use effectcraft_time::Tick;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RenderOpts {
    /// Output scale relative to comp pixels (1 = Full, 0.5 = Half…).
    pub scale: f64,
    pub motion_blur: bool,
    /// Render guide layers (viewer only).
    pub guides: bool,
    /// Draft quality: fewer motion-blur samples, bilinear only.
    pub draft: bool,
    /// 3D view camera override (viewer Front/Top/Custom views); None = the comp's active camera.
    pub view: Option<three_d::CameraState>,
    /// CPU or GPU compositor (the GPU needs [`Renderer::accel`]).
    pub backend: Backend,
    /// Region of interest `[x, y, w, h]` in comp pixels (viewer only): the top-level frame covers
    /// just this rectangle, and only its pixels are composited.
    pub roi: Option<[f64; 4]>,
    /// Which proxies stand in for footage and compositions (Render Settings ▸ Proxy Use).
    pub proxy: effectcraft_project::render_queue::ProxyUse,
    /// Switches Affect Nested Comps (Settings ▸ General): a precomp layer's Quality switch also
    /// limits the layers of the nested comp (Draft/Wireframe propagate down; they never raise a
    /// nested layer's own setting). Its Motion Blur switch does not reach them in AE.
    pub nested_switches: bool,
    /// Realtime Shadows in Draft (Settings ▸ 3D): draft renders (`draft`) still cast shadows.
    pub draft_shadows: bool,
    /// Sharp zoom (viewer only): a `scale` above 1 is the viewer's magnification, so each layer
    /// buffer is held within [`SHARP_MAX_PIXELS`] (drawn at a lower scale and magnified past it)
    /// and nested comps are drawn only where they show. Exports to a larger output leave it off.
    pub sharp: bool,
}

/// The `Debug` text keys caches (viewer frames on disk, nested comps): `sharp` shows only when on,
/// so every other render keeps the text, and the keys, it had before the field.
impl std::fmt::Debug for RenderOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let RenderOpts { scale, motion_blur, guides, draft, view, backend, roi, proxy, nested_switches, draft_shadows, sharp } = self;
        let mut d = f.debug_struct("RenderOpts");
        d.field("scale", scale)
            .field("motion_blur", motion_blur)
            .field("guides", guides)
            .field("draft", draft)
            .field("view", view)
            .field("backend", backend)
            .field("roi", roi)
            .field("proxy", proxy)
            .field("nested_switches", nested_switches)
            .field("draft_shadows", draft_shadows);
        if *sharp {
            d.field("sharp", sharp);
        }
        d.finish()
    }
}

impl RenderOpts {
    /// Whether 3D layers cast shadows in this render.
    pub fn shadows(&self) -> bool {
        !self.draft || self.draft_shadows
    }
}

/// Sharp zoom ([`RenderOpts::sharp`]: the viewer's magnified region rendered at the
/// magnification): the most pixels one layer buffer may take. A source bigger than this at the
/// output scale is drawn at a lower scale and magnified (still no softer than at 100 %).
pub const SHARP_MAX_PIXELS: f64 = 16.0e6;
/// Sharp zoom: the longest side of one layer buffer (the rasterisers stop at 16384).
const SHARP_MAX_SIDE: f64 = 16000.0;

impl Renderer<'_> {
    /// This is a sharp-zoom render above comp resolution ([`RenderOpts::sharp`]): the per-buffer
    /// budget ([`SHARP_MAX_PIXELS`]) applies. Never at or below 100 % nor in exports, so those
    /// frames are unchanged.
    pub(crate) fn sharp(&self) -> bool {
        self.opts.sharp && self.opts.scale > 1.0
    }

    /// The scale (buffer pixels per layer unit) to draw a `w`×`h` (layer units) buffer at for a
    /// wanted `scale`: within the sharp-zoom budget, but never under `floor` (nor over `scale`).
    /// `scale` itself outside sharp zoom.
    pub(crate) fn sharp_fit(&self, scale: f64, w: f64, h: f64, floor: f64) -> f64 {
        if !self.sharp() || !(w.is_finite() && h.is_finite()) {
            return scale;
        }
        let (w, h) = (w.abs().max(1.0), h.abs().max(1.0));
        let cap = (SHARP_MAX_PIXELS / (w * h)).sqrt().min(SHARP_MAX_SIDE / w.max(h));
        if !cap.is_finite() {
            return scale;
        }
        scale.min(cap.max(floor.min(scale)))
    }

    /// The scale a `w`×`h` solid's buffer is drawn at: the output scale (within the sharp-zoom
    /// budget, see [`Self::sharp_fit`]).
    pub(crate) fn solid_scale(&self, w: u32, h: u32) -> f64 {
        self.sharp_fit(self.opts.scale, f64::from(w), f64::from(h), 1.0)
    }
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            scale: 1.0,
            motion_blur: true,
            guides: false,
            draft: false,
            view: None,
            roi: None,
            backend: Backend::Cpu,
            nested_switches: true,
            draft_shadows: true,
            sharp: false,
            proxy: effectcraft_project::render_queue::ProxyUse::CurrentSettings,
        }
    }
}

pub struct Renderer<'a> {
    pub project: &'a Project,
    pub footage: &'a dyn FootageSource,
    pub expr: Option<&'a dyn ExprHost>,
    pub opts: RenderOpts,
    /// Processed-layer cache shared across frames (see [`cache`]). `None` renders everything.
    pub cache: Option<&'a LayerCache>,
    /// Optional per-layer timing sink (`frame --bench` / perf readouts).
    pub profile: Option<&'a std::sync::Mutex<Vec<LayerTiming>>>,
    /// Each layer's render time on this frame, for the Timeline's Render Time pane (see
    /// [`LayerTimes`]). `None` times nothing.
    pub times: Option<&'a LayerTimes>,
    /// Accepted preview caches only. None for exports and cache generation.
    pub freeze: Option<&'a dyn crate::freeze::FreezeSource>,
    /// GPU backend used when [`RenderOpts::backend`] asks for it.
    pub accel: Option<&'a dyn Accelerator>,
    /// Current nesting depth (precomp recursion guard).
    pub(crate) depth: usize,
    /// The project's colour pipeline (bit depth, working space, linear blending).
    pub(crate) pipe: color::Pipe,
    /// Collapsed precomp being drawn into its parent: nested comp pixels → parent comp pixels
    /// (before the output scale).
    pub(crate) outer: Option<Mat3>,
    /// Opacity of the collapsed precomp layers this comp is drawn through.
    pub(crate) opacity_mul: f32,
    /// Collapsed precomp: nested 3D layers join the parent's 3D space.
    pub(crate) collapse3d: Option<Collapse3d<'a>>,
    /// The worst Quality switch of the precomp layers this comp is rendered through (see
    /// [`RenderOpts::nested_switches`]).
    pub(crate) inherited: Option<Quality>,
    /// Classic 3D depth of field is left to an accelerator ([`three_d::Plane3d::dof`]).
    pub(crate) defer_dof: bool,
    /// Classic 3D planes an accelerator already made (see [`Renderer::prepare_3d_run_with`]).
    pub(crate) plane_bufs: Option<&'a three_d::PlaneBufs>,
    /// This renderer draws into the top-level frame's canvas (the top comp, or a collapsed
    /// precomp drawn straight into it): the region of interest's offset applies.
    pub(crate) top: bool,
    /// The collapsed precomp layers this comp is drawn through, outermost first: motion blur
    /// re-evaluates their transforms at each sub-sample (see [`Renderer::buf_matrix_at`]).
    pub(crate) through: [Option<Through<'a>>; MAX_THROUGH],
    /// More collapsed levels than [`MAX_THROUGH`]: their motion is not blurred.
    pub(crate) through_lost: bool,
    /// The canvas this renderer draws into is the window of the output at this origin (output
    /// pixels): a collapsed precomp drawn into a temporary that covers only what its layers
    /// reach. Every warp and dissolve keeps the output's coordinates, so the pixels are the
    /// ones the whole canvas would get.
    pub(crate) canvas_origin: [u32; 2],
    /// The size (comp pixels) of the comp whose frame a collapsed precomp's layers are drawn
    /// into; `None` draws into this renderer's own comp (see [`Renderer::raster_scale`]).
    pub(crate) out_comp: Option<(u32, u32)>,
    /// A nested comp drawn only where its precomp layer can show (see [`crate::clip`]): the
    /// window of its frame (output pixels at this renderer's scale) the canvas covers, as a
    /// region of interest does at the top level. Collapsed precomps drawn into this canvas keep
    /// it; renders one level deeper start whole.
    pub(crate) window: Option<crate::placement::Window>,
    /// The Layer panel's zoom (layer pixels → screen pixels): Continuously Rasterize layers
    /// (vector footage, text, shapes) are drawn at it instead of at their size in the comp, so
    /// they stay sharp when the panel magnifies them. `None` everywhere else; nested renders
    /// don't inherit it.
    pub layer_view: Option<f64>,
}

/// Time spent on one layer of one frame (milliseconds).
#[derive(Clone, Debug, Default)]
pub struct LayerTiming {
    pub depth: usize,
    pub layer: String,
    /// Source + masks + effects (cache lookup only, on a hit).
    pub process_ms: f64,
    /// Per effect: (effect id, ms). Empty on a cache hit.
    pub effects: Vec<(String, f64)>,
    /// Transform + matte + blend.
    pub composite_ms: f64,
    /// The processed buffer came from the layer cache.
    pub cached: bool,
}

/// The time each layer took to render on one frame (ms), by comp and layer: what After Effects'
/// Render Time pane shows. A layer's time is its wall time on the CPU or GPU compositor: what the
/// pre-pass produced for it on every core, plus its draw (source, masks, effects, styles, matte,
/// transform and blend). A precomp layer's time includes the layers drawn inside it, which are
/// timed under their own comp too. A 3D run's shared scene composite is split evenly between its
/// layers. A layer whose content came from the layer cache takes only the lookup; a GPU
/// compositor's time is the time to prepare and record its work, not the device's own run time.
#[derive(Debug, Default)]
pub struct LayerTimes(std::sync::Mutex<std::collections::HashMap<(ItemId, LayerId), f64>>);

impl LayerTimes {
    /// Add `ms` to `layer` of `comp`.
    pub fn add(&self, comp: ItemId, layer: LayerId, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let mut m = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *m.entry((comp, layer)).or_insert(0.0) += ms;
    }

    /// The time of `layer` of `comp` so far.
    pub fn get(&self, comp: ItemId, layer: LayerId) -> Option<f64> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&(comp, layer)).copied()
    }

    /// Forget the times taken (a frame rendered again from the start).
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    /// The times taken.
    pub fn into_map(self) -> std::collections::HashMap<(ItemId, LayerId), f64> {
        self.0.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<'a> Renderer<'a> {
    /// Run `f` (drawing or producing `layer` of `ctx`'s comp) and add its wall time to the
    /// layer's render time, when this render is timed ([`Self::times`]).
    pub fn timed<T>(&self, ctx: &EvalCtx, layer: &Layer, f: impl FnOnce() -> T) -> T {
        let Some(times) = self.times else { return f() };
        let t0 = web_time::Instant::now();
        let out = f();
        times.add(ctx.comp_id, layer.id, t0.elapsed().as_secs_f64() * 1e3);
        out
    }

    /// [`Self::timed`] for a run of 3D layers drawn as one scene: what `f` timed per layer stays
    /// with that layer, and the rest of the run's time (the shared scene) is split evenly
    /// between the run's layers.
    pub fn timed_run<T>(&self, ctx: &EvalCtx, run: &[&Layer], f: impl FnOnce() -> T) -> T {
        let Some(times) = self.times.filter(|_| !run.is_empty()) else { return f() };
        let own = |times: &LayerTimes| run.iter().filter_map(|l| times.get(ctx.comp_id, l.id)).sum::<f64>();
        let before = own(times);
        let t0 = web_time::Instant::now();
        let out = f();
        let total = t0.elapsed().as_secs_f64() * 1e3;
        let shared = (total - (own(times) - before)).max(0.0) / run.len() as f64;
        for l in run {
            times.add(ctx.comp_id, l.id, shared);
        }
        out
    }

    pub fn new(project: &'a Project, footage: &'a dyn FootageSource, opts: RenderOpts) -> Renderer<'a> {
        Renderer {
            project,
            footage,
            expr: None,
            opts,
            cache: None,
            profile: None,
            times: None,
            freeze: None,
            accel: None,
            depth: 0,
            pipe: color::Pipe::of(&project.settings),
            outer: None,
            opacity_mul: 1.0,
            collapse3d: None,
            inherited: None,
            defer_dof: false,
            plane_bufs: None,
            top: true,
            through: [None; MAX_THROUGH],
            through_lost: false,
            canvas_origin: [0; 2],
            out_comp: None,
            window: None,
            layer_view: None,
        }
    }

    /// The layer cache to keep this render's buffers in: none once its footage reads were
    /// called off, as they may have come back empty ([`FootageSource::called_off`], #329).
    pub fn cache_to_keep(&self) -> Option<&'a LayerCache> {
        self.cache.filter(|_| !self.footage.called_off())
    }

    /// A layer's Quality switch, limited by the precomp layers above it when
    /// [`RenderOpts::nested_switches`] is on.
    pub fn quality(&self, layer: &Layer) -> Quality {
        let rank = |q: Quality| match q {
            Quality::Best => 0,
            Quality::Draft => 1,
            Quality::Wireframe => 2,
        };
        match self.inherited {
            Some(q) if rank(q) > rank(layer.switches.quality) => q,
            _ => layer.switches.quality,
        }
    }

    /// A layer's Motion Blur switch. Unlike Quality, the precomp layers above do not limit it:
    /// AE still motion blurs nested layers under a precomp layer whose Motion Blur is off
    /// (aerender, SceneYard Student Computer Workflow: trimmed strokes two precomps down).
    pub(crate) fn layer_motion_blur(&self, layer: &Layer) -> bool {
        layer.switches.motion_blur
    }

    /// The renderer for the nested comp of precomp layer `layer`: its switches pass down when
    /// [`RenderOpts::nested_switches`] is on.
    pub(crate) fn nested_through(&self, layer: &Layer) -> Renderer<'a> {
        let mut sub = self.nested();
        if self.opts.nested_switches {
            sub.inherited = Some(self.quality(layer));
        }
        sub
    }

    /// A renderer for nested work (precomps, layers read by effects): one level deeper.
    pub(crate) fn nested(&self) -> Renderer<'a> {
        Renderer {
            depth: self.depth + 1,
            outer: None,
            opacity_mul: 1.0,
            collapse3d: None,
            defer_dof: false,
            plane_bufs: None,
            top: false,
            through: [None; MAX_THROUGH],
            through_lost: false,
            canvas_origin: [0; 2],
            out_comp: None,
            window: None,
            layer_view: None,
            ..*self
        }
    }

    pub(crate) fn ctx(&self, comp_id: ItemId, comp: &'a Comp, t: Tick) -> EvalCtx<'a> {
        EvalCtx { project: self.project, comp_id, comp, time: t, expr: self.expr, footage: Some(self.footage) }
    }

    /// This renderer with another accelerator (or none): the passes of
    /// [`passes::in_passes`] render with the accelerator, its CPU fallback without.
    pub fn with_accel(&self, accel: Option<&'a dyn Accelerator>) -> Renderer<'a> {
        Renderer { accel, ..*self }
    }

    /// The accelerator to use for this render, if any (see [`Backend`]).
    pub fn active_accel(&self) -> Option<&'a dyn Accelerator> {
        let a = self.accel?;
        match self.opts.backend {
            Backend::Cpu => None,
            Backend::Gpu => Some(a),
            Backend::Auto => self.project.settings.gpu_acceleration.then_some(a),
        }
    }

    /// Render a composition at comp time `t` (transparent background, comp size × scale).
    /// Top-level frames go to the GPU when [`Self::active_accel`] is set and handles them.
    pub fn comp_frame(&self, comp_id: ItemId, t: Tick) -> Image {
        let _frame = self.begin_frame(comp_id);
        // Text laid out once per layer and time in this frame (see `text::GeomScope`).
        let _text = crate::text::GeomScope::open(self.project);
        // Classic 3D runs composite on the GPU too (M12.7), so Auto sends 3D comps there as well;
        // Auto renders each comp on whichever compositor measured faster ([`AutoPick`]).
        if self.depth == 0
            && self.freeze.is_none()
            && let Some(a) = self.active_accel()
        {
            let auto = self.frame_auto(a, comp_id, false);
            if auto.is_none_or(|(p, key)| p.choose(key)) {
                let t0 = web_time::Instant::now();
                if let Some(img) = a.comp_frame(self, comp_id, t) {
                    if let Some((p, key)) = auto {
                        p.record(key, true, t0.elapsed().as_secs_f64() * 1e3);
                    }
                    return img;
                }
                if let Some((p, key)) = auto {
                    p.declined(key);
                }
            }
            let t0 = web_time::Instant::now();
            let img = self.comp_frame_cpu(comp_id, t);
            if let Some((p, key)) = auto {
                p.record(key, false, t0.elapsed().as_secs_f64() * 1e3);
            }
            return img;
        }
        self.comp_frame_cpu(comp_id, t)
    }

    /// Build a preview cache in blending space, before display/output conversion. Call on a
    /// worker with an immutable, validated snapshot; this never reads other freeze caches.
    pub fn freeze_frame(&self, comp_id: ItemId, t: Tick) -> Result<Image, String> {
        self.freeze_selected_frame(comp_id, t, None)
    }

    /// Isolate a supported 2D section without mutating its scene, expressions or linked
    /// controls. Unselected layers remain available to the effect and expression hosts.
    pub fn freeze_selected_frame(&self, comp_id: ItemId, t: Tick, selected: Option<&[effectcraft_project::LayerId]>) -> Result<Image, String> {
        let comp = self.project.comp(comp_id).ok_or("Smart Freeze composition no longer exists")?;
        let scale = self.opts.scale;
        if !scale.is_finite() || scale <= 0.0 || scale > 1.0 || self.opts.roi.is_some() || self.opts.view.is_some() {
            return Err("Smart Freeze needs a full composition view at a scale between 0 and 1".into());
        }
        let w = (f64::from(comp.width) * scale).round().max(1.0) as u32;
        let h = (f64::from(comp.height) * scale).round().max(1.0) as u32;
        if u64::from(w).saturating_mul(u64::from(h)) > crate::freeze::MAX_PIXELS as u64 {
            return Err("Smart Freeze frame exceeds its pixel budget".into());
        }
        let plain = Renderer { freeze: None, ..*self };
        let ctx = plain.ctx(comp_id, comp, t);
        let mut canvas = Image::new(w, h);
        if let Some(ids) = selected {
            if ids.is_empty() || ids.len() > 10_000 || ids.iter().any(|id| comp.layer(*id).is_none()) {
                return Err("Smart Freeze selection contains missing layers or exceeds its budget".into());
            }
            let solo = comp.layers.iter().any(|l| ids.contains(&l.id) && l.switches.solo);
            let visible: Vec<_> = comp
                .layers
                .iter()
                .rev()
                .filter(|l| ids.contains(&l.id) && l.has_video() && l.is_active_at(t) && (!solo || l.switches.solo) && (plain.opts.guides || !l.switches.guide))
                .collect();
            plain.draw_layers(&ctx, &visible, &mut canvas, true);
        } else {
            plain.draw_comp(&ctx, &mut canvas, true);
        }
        Ok(canvas)
    }

    /// The Auto timing history and key for a top-level frame of `comp` (`None` unless the
    /// backend is [`Backend::Auto`] and the accelerator keeps history).
    pub fn frame_auto(&self, a: &'a dyn Accelerator, comp: ItemId, display: bool) -> Option<(&'a AutoPick, AutoKey)> {
        if self.opts.backend != Backend::Auto || self.opts.roi.is_some() {
            return None;
        }
        Some((a.auto_pick()?, AutoKey::new(comp, self.opts.scale, display)))
    }

    /// [`Self::begin_frame`] for an accelerator drawing a top-level frame itself (the GPU
    /// viewer's display path): the layer cache keeps that frame's working set while it lives.
    pub fn frame_mark(&self, comp_id: ItemId) -> Option<crate::cache::FrameMark<'a>> {
        self.begin_frame(comp_id)
    }

    /// Tell the layer cache a top-level frame starts (it lasts while the mark lives, see
    /// [`crate::cache::LayerCache::begin_frame`]).
    fn begin_frame(&self, comp_id: ItemId) -> Option<crate::cache::FrameMark<'a>> {
        if self.depth != 0 {
            return None;
        }
        let (c, comp) = (self.cache?, self.project.comp(comp_id)?);
        let s = self.opts.scale;
        Some(c.begin_frame((comp.width as f64 * s).round() as u32, (comp.height as f64 * s).round() as u32))
    }

    /// [`Self::comp_frame`] on the CPU compositor only.
    pub fn comp_frame_cpu(&self, comp_id: ItemId, t: Tick) -> Image {
        let _frame = self.begin_frame(comp_id);
        let _text = crate::text::GeomScope::open(self.project);
        let Some(comp) = self.project.comp(comp_id) else { return Image::new(1, 1) };
        let s = self.opts.scale;
        let w = ((comp.width as f64 * s).round() as u32).max(1);
        let h = ((comp.height as f64 * s).round() as u32).max(1);
        // Region of interest on a comp with 3D layers: render the frame and crop it. A region
        // reaching past the comp frame (the Extended Viewer) renders Classic 3D natively — the
        // planes project through the offset camera — and Advanced 3D as the frame, padded.
        if self.depth == 0
            && let Some(r) = self.opts.roi
            && comp.has_3d()
            && (roi_inside(r, comp.width, comp.height) || comp.renderer == effectcraft_project::Renderer::Advanced3D)
        {
            let full = Renderer { opts: RenderOpts { roi: None, ..self.opts }, ..*self }.comp_frame(comp_id, t);
            let (x0, y0) = ((r[0] * s).round() as i64, (r[1] * s).round() as i64);
            let (rw, rh) = (((r[2] * s).round() as u32).max(1), ((r[3] * s).round() as u32).max(1));
            let mut out = Image::new(rw, rh);
            for y in 0..rh as i64 {
                let sy = y + y0;
                if sy < 0 || sy >= full.height as i64 {
                    continue;
                }
                for x in 0..rw as i64 {
                    let sx = x + x0;
                    if sx >= 0 && sx < full.width as i64 {
                        out.data[(y * rw as i64 + x) as usize] = full.data[(sy * full.width as i64 + sx) as usize];
                    }
                }
            }
            return out;
        }
        let (w, h) = match self.roi_offset() {
            Some((_, _, rw, rh)) => (rw, rh),
            None => (w, h),
        };
        let mut canvas = Image::new(w, h);
        if self.depth > 16 {
            return canvas;
        }
        let ctx = self.ctx(comp_id, comp, t);
        self.draw_comp(&ctx, &mut canvas, true);
        // The canvas is already quantised after every layer; only re-encoding changes it.
        if let Some(c) = self.pipe.from_blend() {
            color::convert(&mut canvas, &c);
            self.pipe.quantize(&mut canvas);
        }
        // The top-level comp leaves the working space for the (sRGB) display / output.
        if self.depth == 0
            && let Some(c) = self.pipe.output()
        {
            color::convert(&mut canvas, &c);
            self.pipe.quantize(&mut canvas);
        }
        if self.depth == 0
            && let Some((lin, mode, enc)) = self.pipe.output_hdr()
        {
            color::output_hdr(&mut canvas, lin, mode, enc);
            self.pipe.quantize(&mut canvas);
        }
        canvas
    }

    /// Draw a comp's layers (bottom to top) into `canvas` (blending space).
    /// `blank`: the canvas is fully transparent (lets the first layer skip re-quantising).
    pub(crate) fn draw_comp(&self, ctx: &EvalCtx<'a>, canvas: &mut Image, mut blank: bool) {
        if let Some(opts) = self.freeze_options(ctx)
            && let Some(frame) = self.freeze.and_then(|f| f.frame(ctx.comp_id, &[], ctx.time, &opts))
            && frame.width == canvas.width
            && frame.height == canvas.height
        {
            canvas.blend_from(&frame, effectcraft_color::BlendMode::Normal, 1.0, 0);
            return;
        }
        for (ctx, visible) in self.draw_passes(ctx) {
            self.draw_layers(&ctx, &visible, canvas, blank);
            blank = blank && visible.is_empty();
        }
    }

    fn freeze_options(&self, ctx: &EvalCtx<'_>) -> Option<RenderOpts> {
        if self.outer.is_some() || self.opts.view.is_some() {
            return None;
        }
        let roi = if self.top && self.depth == 0 { self.opts.roi } else { None };
        if roi.is_some_and(|r| !r.iter().all(|v| v.is_finite()) || r[2] <= 0.0 || r[3] <= 0.0 || !roi_inside(r, ctx.comp.width, ctx.comp.height)) {
            return None;
        }
        Some(RenderOpts { roi, ..self.opts })
    }

    /// Draw `visible` (bottom to top) into `canvas`.
    fn draw_layers(&self, ctx: &EvalCtx<'a>, visible: &[&'a Layer], canvas: &mut Image, mut blank: bool) {
        // The layers' content on every core first (`prepass`); the walk then finds it cached.
        self.prefetch_cpu(ctx, visible, (canvas.width, canvas.height));
        // Bottom-to-top, with runs of consecutive 3D layers depth-sorted (farthest first).
        let mut i = 0;
        while i < visible.len() {
            if let Some(opts) = self.freeze_options(ctx)
                && let (Some(freeze), Some(bottom)) = (self.freeze, visible.get(i))
                && let Some(ids) = freeze.selection(ctx.comp_id, bottom.id)
                && !ids.is_empty()
                && let Some(end) = crate::freeze::selection_end(visible, ids, i)
                && let Some(frame) = freeze.frame(ctx.comp_id, ids, ctx.time, &opts)
                && frame.width == canvas.width
                && frame.height == canvas.height
            {
                canvas.blend_from(&frame, effectcraft_color::BlendMode::Normal, 1.0, 0);
                self.pipe.quantize(canvas);
                i = end;
                blank = false;
                continue;
            }
            if visible[i].is_3d() {
                let mut j = i;
                while j < visible.len() && visible[j].is_3d() {
                    j += 1;
                }
                self.timed_run(ctx, &visible[i..j], || three_d::compose::draw_run(self, ctx, &visible[i..j], canvas));
                i = j;
                self.pipe.quantize(canvas);
                blank = false;
            } else {
                let changed = self.timed(ctx, visible[i], || match (self.collapsed(ctx, visible[i]), self.collapsed_fx(ctx, visible[i])) {
                    (Some(item), _) => self.draw_collapsed(ctx, visible[i], item, canvas, blank),
                    (None, Some(item)) => self.draw_collapsed_fx(ctx, visible[i], item, canvas),
                    (None, None) => self.draw_layer(ctx, visible[i], canvas, blank),
                });
                i += 1;
                // 8/16 bpc: the comp is an integer buffer after every layer (only the pixels the
                // layer touched need re-quantising).
                self.pipe.quantize_region(canvas, changed);
                blank = false;
            }
        }
    }
}

/// A region of interest `[x, y, w, h]` lies within a `w`×`h` comp frame.
pub fn roi_inside(r: [f64; 4], w: u32, h: u32) -> bool {
    r[0] >= 0.0 && r[1] >= 0.0 && r[0] + r[2] <= w as f64 + 1e-6 && r[1] + r[3] <= h as f64 + 1e-6
}

/// The Extended Viewer's region of interest: the comp frame grown by `margin` × its size on every
/// side (`margin` = 1 shows a pasteboard one comp wide around the frame), intersected with the
/// `visible` comp-space rectangle `[x0, y0, x1, y1]` and snapped outward to whole 16 px steps from the frame edges
/// so panning re-renders rarely. `None` when the visible area lies inside the frame.
pub fn extended_region(w: u32, h: u32, visible: [f64; 4], margin: f64) -> Option<[f64; 4]> {
    let (w, h) = (w as f64, h as f64);
    let (mx, my) = (w * margin, h * margin);
    let snap_lo = |v: f64| (v / 16.0).floor() * 16.0;
    let snap_hi = |v: f64| (v / 16.0).ceil() * 16.0;
    let x0 = snap_lo(visible[0].max(-mx)).min(0.0);
    let y0 = snap_lo(visible[1].max(-my)).min(0.0);
    let x1 = w + snap_hi(visible[2].min(w + mx) - w).max(0.0);
    let y1 = h + snap_hi(visible[3].min(h + my) - h).max(0.0);
    if x0 >= 0.0 && y0 >= 0.0 && x1 <= w && y1 <= h {
        return None;
    }
    Some([x0, y0, x1 - x0, y1 - y0])
}

pub fn render_frame(project: &Project, comp: ItemId, t: Tick, scale: f64) -> Image {
    Renderer::new(project, &NoFootage, RenderOpts { scale, ..Default::default() }).comp_frame(comp, t)
}
