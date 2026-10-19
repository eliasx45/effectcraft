//! The EffectCraft GPU compositor (Mercury GPU Acceleration's counterpart), on wgpu compute
//! shaders: Metal, Vulkan, Direct3D 12 and WebGPU.
//!
//! The CPU [`Renderer`] stays the reference and keeps rendering layer *content* (sources, masks,
//! CPU effects, layer styles) into its layer cache. [`Gpu`] composites: it uploads the cached
//! layer buffers (once per buffer), applies the layer transforms with the CPU's sampling
//! (nearest / bilinear / bicubic, minification pre-filter), motion-blur sub-samples, track
//! mattes, Preserve Transparency, layer styles' passes, all 38 blend modes and the 8/16 bpc
//! clamping and quantisation, and converts colour spaces. Classic 3D runs composite here too
//! (`classic3d`: per-pixel fragment sort, lights, ray-cast shadows), and adjustment layers run
//! their effect stacks on the GPU-resident comp. Wireframe-quality outlines are drawn here too.
//!
//! Advanced 3D comps rasterise on a render pipeline (`advanced3d.wgsl`: depth buffer, PBR,
//! image-based light, shadow maps) and finish in compute kernels (`adv3d.wgsl`: supersampling
//! resolve, motion-blur sub-samples, iris depth of field, encoding, compositing), see
//! [`Accelerator::render_3d`] and `Renderer::prepare_adv_run`. Runs with layers on the 2D
//! compositing path (blend modes, track mattes, Preserve Transparency:
//! `Renderer::split_adv_run`) render each such layer alone, hide it behind the nearer main
//! scene and composite it through the 2D path here too; environment backgrounds (Classic and
//! Advanced 3D) draw their sky in a kernel (`Renderer::sky_draw`).
//!
//! GPU effects ([`effectcraft_effects::GPU_EFFECTS`]) run as compute kernels with the CPU
//! effect's exact steps (padding, box-blur radii, parameter conversions); chains of them are
//! uploaded and read back once. Each family lives in its own module with its own WGSL file
//! (`fx_color`, `fx_deepglow`, `fx_depth`, `fx_distort`, `fx_extra`, `fx_gen2`, `fx_generate`, `fx_key`,
//! `fx_light`, `fx_lut`, `fx_noise`, `fx_particles`, `fx_pixel2`, `fx_sim`, `fx_stylize`,
//! `fx_text`, `fx_time`, `fx_tone`, `fx_transition`, `fx_vr`, `fx_warp`); settings a kernel
//! cannot match fall back to the CPU (`catalog::gpu_supported`, or `None` from the family's
//! `apply`).
//! The 3D Channel effects read the layer's aux channels as an extra texture (`fx_depth`), the
//! colour-management effects interpret colour programs built next to the CPU effects
//! (`fx_lut`), simulations keep their state on the CPU and rasterise its per-frame plan here
//! (`fx_sim`, `fx_particles`), and so do the geometry generators (`fx_gen2`: bolts, strokes,
//! glyphs, audio marks, waves).
//!
//! Working textures come from a pool (`context::Pool`): a frame allocates several full-frame
//! RGBA f32 images per layer, and creating and zeroing those cost more than compositing a small
//! layer. A released texture is reused once every encoder that could still read it has been
//! submitted. 8/16 bpc quantisation after each layer is fused into that layer's composite.
//! [`Backend::Auto`](effectcraft_render::Backend) renders each comp on whichever compositor
//! measured faster for it ([`effectcraft_render::AutoPick`], kept by [`Gpu`]): a light comp
//! that the CPU composites in a millisecond stays there rather than paying for a full-frame
//! readback.
//!
//! GPU particles (`particles`): the stepped particle effects hand their simulation to
//! [`effectcraft_effects::psim::ParticleSim`], implemented here with one invocation per particle
//! and GPU-resident checkpoints.
//!
//! Plug a [`Gpu`] into [`Renderer::accel`] (it implements [`Accelerator`]); renders then use it
//! when [`RenderOpts::backend`](effectcraft_render::RenderOpts) asks for it. The viewer can
//! skip readback entirely with [`Gpu::render_display`], which leaves an RGBA8 texture for
//! egui-wgpu to draw.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod adapter_choice;
mod adv3d;
mod bokeh;
mod classic3d;
mod codes;
mod context;
mod cull;
pub mod deferred;
mod effects;
mod fusion;
mod mips;
mod plane3d;
mod warmup;
pub use warmup::WarmupReport;
/// Whether per-pixel effect fusion is enabled (profiling builds support an A/B override).
pub fn color_fusion_enabled() -> bool {
    fusion::enabled()
}
mod fx_color;
mod fx_deepglow;
mod fx_depth;
mod fx_distort;
mod fx_extra;
mod fx_gen2;
mod fx_generate;
mod fx_key;
mod fx_light;
mod fx_lut;
mod fx_noise;
mod fx_particles;
mod fx_pixel2;
mod fx_sim;
mod fx_styles;
mod fx_stylize;
mod fx_text;
mod fx_time;
mod fx_tone;
mod fx_transition;
mod fx_vr;
mod fx_warp;
mod ops;
mod particles;
mod prepass;
mod readback;
#[cfg(feature = "profile")]
mod timestamps;
pub mod traffic_light;
pub mod vram;
mod walk;

use std::sync::Arc;

#[cfg(feature = "profile")]
pub use context::take_pool_miss_sites;
pub use context::{AllocStats, GpuContext, GpuImage, TransferStats, VramReport};
use effectcraft_effects::Buf;
use effectcraft_project::ItemId;
use effectcraft_raster::Image;
use effectcraft_render::{Accelerator, FxStep, Renderer};
use effectcraft_time::Tick;
pub use traffic_light::TrafficLight;
pub use wgpu;

use crate::context::Enc;

/// The GPU compositor (cheap to clone; one device shared by all clones).
#[derive(Clone)]
pub struct Gpu {
    ctx: Arc<GpuContext>,
    /// Backend::Auto's per-comp CPU / GPU timings.
    auto: Arc<effectcraft_render::AutoPick>,
    /// (comp, reason) pairs already warned about when the GPU declined a frame.
    declined: Arc<std::sync::Mutex<std::collections::HashSet<(u64, String)>>>,
}

/// A viewer frame left on the GPU: premultiplied RGBA8 (`wgpu::TextureFormat::Rgba8Unorm`) with
/// a full mip chain (for minified views), ready for
/// `egui_wgpu::Renderer::register_native_texture_with_sampler_options`.
#[derive(Clone, Debug)]
pub struct DisplayFrame {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
}

impl Gpu {
    /// Compile optional static pipelines through the same single-flight cells used by frames.
    /// Call from a native startup worker; dynamic effect sequences compile separately.
    pub fn warm_pipelines(&self) -> Result<(), String> {
        self.ctx.check_health()?;
        crate::adv3d::warm(&self.ctx)?;
        crate::particles::warm(&self.ctx)?;
        if let Some(bank) = &self.ctx.codes {
            bank.warm().unwrap_or_else(|e| log::warn!("gpu: 8 bpc codes kernels: {e}"));
        }
        self.ctx.check_health()
    }

    /// 8 bpc working images as codes (`crate::codes`): `None` when this device keeps every image
    /// f32; else (passes that read or wrote codes, codes decoded for a float-only consumer).
    pub fn codes_stats(&self) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        self.ctx.codes.as_ref().map(|b| (b.dispatches.load(Relaxed), b.promotions.load(Relaxed)))
    }

    /// Codes decodes by call site (diagnostics).
    pub fn codes_sites(&self) -> Vec<(String, u64)> {
        self.ctx.codes.as_ref().and_then(|b| b.sites.lock().ok().map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())).unwrap_or_default()
    }

    /// Successfully constructed static pipelines (dynamic sequence specializations excluded).
    pub fn static_pipeline_count(&self) -> u64 {
        self.ctx.static_pipeline_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Dynamic sequence compilation attempts, including source/module/layout preparation.
    /// Cached failures and capacity denials do not start another attempt.
    pub fn fusion_compile_attempts(&self) -> u64 {
        self.ctx.fusion.attempted()
    }

    /// Successfully created pipelines, including dynamic effect-sequence specializations.
    /// Failed compilations are excluded; cached failures never count as a created pipeline.
    pub fn pipeline_count(&self) -> u64 {
        self.static_pipeline_count().saturating_add(self.ctx.fusion.created())
    }

    /// On a device of its own (CLI, tests, benchmarks); `None` without a usable adapter.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn headless() -> Option<Gpu> {
        Gpu::try_headless().map_err(|e| log::info!("gpu: {e}")).ok()
    }

    /// [`Gpu::headless`], with why there is none.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn try_headless() -> Result<Gpu, String> {
        pollster::block_on(GpuContext::request()).map(Gpu::from_context)
    }

    /// On an existing device (the desktop app shares egui-wgpu's).
    pub fn new(adapter: &wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Result<Gpu, String> {
        GpuContext::new(adapter, device, queue).map(Gpu::from_context)
    }

    /// A separate device on an existing adapter, so background compositing does not submit to
    /// the window surface's queue while it reconfigures.
    pub async fn request_on_adapter(adapter: &wgpu::Adapter) -> Result<Gpu, String> {
        GpuContext::request_on_adapter(adapter).await.map(Gpu::from_context)
    }

    /// A device of its own on a better adapter than the window's `window`, when that one can't
    /// run the compositor well (see [`adapter_choice`]): `None` to stay on the window's.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_off_window(window: &wgpu::AdapterInfo) -> Option<Result<Gpu, String>> {
        GpuContext::request_off_window(window).await.map(|r| r.map(Gpu::from_context))
    }

    /// A weak handle on this compositor's device: dead once every clone is dropped, and its
    /// video memory freed with it (CR1: a replacement waits for that).
    pub fn downgrade(&self) -> std::sync::Weak<GpuContext> {
        Arc::downgrade(&self.ctx)
    }

    pub fn from_context(ctx: GpuContext) -> Gpu {
        Gpu { ctx: Arc::new(ctx), auto: Default::default(), declined: Default::default() }
    }

    /// A device of its own without blocking, with deferred readbacks (a browser worker, see
    /// [`deferred`]); natively the same device works too (tests). `Err` says why there is none.
    pub async fn request_deferred() -> Result<Gpu, String> {
        let mut ctx = GpuContext::request().await?;
        ctx.set_deferred(true);
        Ok(Gpu::from_context(ctx))
    }

    /// [`Gpu::request_deferred`] blocking (tests of the browser worker's path, natively).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn headless_deferred() -> Option<Gpu> {
        pollster::block_on(Gpu::request_deferred()).map_err(|e| log::info!("gpu: {e}")).ok()
    }

    /// The deferred readback state, when readbacks are deferred.
    pub fn deferred(&self) -> Option<&Arc<deferred::Deferred>> {
        self.ctx.deferred()
    }

    /// Resolves once no deferred readback is in flight (immediately without deferred readbacks).
    pub async fn settled(&self) {
        if let Some(d) = self.ctx.deferred() {
            d.settled().await;
        }
    }

    /// Deliver finished readbacks without blocking (native; the browser delivers them itself).
    pub fn poll(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.ctx.poll_readbacks(wgpu::PollType::Poll);
    }

    /// `{passes, readbacks, inFlight}` of deferred rendering (diagnostics).
    pub fn deferred_stats(&self) -> Option<(u64, u64, usize)> {
        let d = self.ctx.deferred()?;
        Some((d.passes.load(std::sync::atomic::Ordering::Relaxed), d.readbacks.load(std::sync::atomic::Ordering::Relaxed), d.in_flight()))
    }

    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.ctx.device
    }

    /// Render a top-level comp frame and leave it on the GPU as a display texture (no
    /// readback unless a CPU fallback step needs one). `None` = render on the CPU.
    pub fn render_display(&self, r: &Renderer, comp: ItemId, t: Tick) -> Option<DisplayFrame> {
        let _profile = effectcraft_render::profile::span("display_prepare");
        self.ctx.check_health().map_err(|e| log::error!("gpu display: {e}")).ok()?;
        // Declared before the encoder: the frame lasts until its resident outputs are published.
        let _frame = r.frame_mark(comp);
        let mut e = Enc::with_resident(&self.ctx, r.cache.and_then(effectcraft_render::cache::LayerCache::gpu_owner));
        let img = walk::render(&mut e, r, comp, t)?;
        let texture = e.display_px(&img)?;
        e.submit();
        // Reject failures observed during rendering/submission before publishing the frame.
        // A later device fault still requires the presentation host's lifecycle handling.
        self.ctx.check_health().map_err(|e| log::error!("gpu display: {e}")).ok()?;
        // footage reads called off (#329): frames may be missing, so nothing is kept
        if r.footage.called_off() {
            return None;
        }
        e.publish_native();
        Some(DisplayFrame { texture, width: img.width(), height: img.height() })
    }

    /// Read a display frame back as premultiplied RGBA8 (Info panel sampling, eyedroppers).
    pub fn read_display(&self, f: &DisplayFrame) -> Option<Vec<u8>> {
        Enc::new(&self.ctx).read_texture(&f.texture, f.width, f.height, 4)
    }

    /// [`Gpu::read_display`] without blocking (the browser's main thread): `done` gets the bytes
    /// once the GPU has them.
    pub fn read_display_async(&self, f: &DisplayFrame, done: impl FnOnce(Option<Vec<u8>>) + wgpu::WasmNotSend + 'static) {
        if let Err(e) = self.ctx.check_health() {
            log::error!("gpu display readback: {e}");
            crate::readback::reject(done, e);
            return;
        }
        Enc::new(&self.ctx).read_texture_async(&f.texture, f.width, f.height, 4, done);
    }

    /// [`Gpu::read_display_async`] for a native frame worker that goes on to other work instead
    /// of waiting: `done` gets exactly [`Gpu::read_display`]'s bytes, or `None` on failure,
    /// exactly once (possibly before this returns), from whichever thread polls the device after
    /// the copy finished. Returns the submission that copies the frame, for
    /// [`Gpu::wait_submission`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_display_start(&self, f: &DisplayFrame, done: impl FnOnce(Option<Vec<u8>>) + Send + 'static) -> Option<wgpu::SubmissionIndex> {
        if let Err(e) = self.ctx.check_health() {
            log::error!("gpu display readback: {e}");
            crate::readback::reject(done, e);
            return None;
        }
        self.ctx.transfers.count(false, (f.width as usize).saturating_mul(f.height as usize).saturating_mul(4));
        let mut e = Enc::new(&self.ctx);
        e.read_texture_async(&f.texture, f.width, f.height, 4, done);
        e.last_submission()
    }

    /// Wait until `submission` has finished on the GPU, running the callbacks of the readbacks it
    /// completed, or for at most the readback deadline. Running out of time is not a failure (the
    /// work ahead of it may be a deep queue that keeps completing): the readback deadlines retire
    /// the context when the GPU stops completing work.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_submission(&self, submission: &wgpu::SubmissionIndex) -> Result<(), String> {
        self.ctx.wait_for(Some(submission.clone()), crate::readback::TIMEOUT).map(|_| ())
    }

    /// Render a frame on the GPU and read it back (`None` = not handled).
    pub fn render(&self, r: &Renderer, comp: ItemId, t: Tick) -> Option<Image> {
        self.ctx.check_health().ok()?;
        if !self.ctx.can_readback() {
            return None;
        }
        if let Some(d) = self.ctx.deferred.clone() {
            return self.render_deferred(&d, r, comp, t);
        }
        let mut e = Enc::with_resident(&self.ctx, r.cache.and_then(effectcraft_render::cache::LayerCache::gpu_owner));
        let img = walk::render(&mut e, r, comp, t)?;
        let image = e.download_px(&img);
        self.ctx.check_health().ok()?;
        // footage reads called off (#329): frames may be missing, so nothing is kept
        if r.footage.called_off() {
            return None;
        }
        if image.is_some() {
            e.publish_native();
        }
        image
    }

    /// [`Gpu::render`] with deferred readbacks: the frame's readback is keyed by comp, time and
    /// render options; until it arrives (or while layer steps still miss) the pass gets a
    /// placeholder.
    fn render_deferred(&self, d: &Arc<deferred::Deferred>, r: &Renderer, comp: ItemId, t: Tick) -> Option<Image> {
        let mut k = deferred::Key::new(2);
        k.u64(comp.0);
        k.u64(t.0 as u64);
        k.debug(&r.opts);
        let key = k.finish();
        let image = |rb: deferred::Readback| -> Option<Image> {
            let mut img = Image::new(rb.width, rb.height);
            let dst: &mut [u8] = bytemuck::cast_slice_mut(&mut img.data);
            (dst.len() == rb.bytes.len()).then(|| dst.copy_from_slice(&rb.bytes))?;
            Some(img)
        };
        match d.lookup(key) {
            deferred::Lookup::Ready(rb) => return rb.and_then(image),
            deferred::Lookup::InFlight => {
                d.miss();
                return Some(Image::new(1, 1));
            }
            deferred::Lookup::Absent => {}
        }
        let misses = d.misses();
        let mut e = Enc::with_resident(&self.ctx, r.cache.and_then(effectcraft_render::cache::LayerCache::gpu_owner));
        let img = walk::render(&mut e, r, comp, t)?;
        if d.misses() != misses {
            // Layer steps are still in flight: this pass's canvas is not the frame.
            return Some(Image::new(1, 1));
        }
        let Some(img) = img.float(&mut e) else { return Some(Image::new(1, 1)) };
        match e.read_keyed(&img, key, [0.0; 2], 1.0) {
            Some(rb) => rb.and_then(image),
            None => Some(Image::new(1, 1)),
        }
    }

    /// Contain native allocation failures (out-of-memory, internal errors and unwinds) in
    /// thread-local error scopes. A failed result is discarded and cached allocations are
    /// released; callers can retry with a fresh CPU renderer. Some drivers report a failed
    /// allocation as an internal error rather than out-of-memory (Adreno's
    /// `ERROR_INITIALIZATION_FAILED`, #591), so both count.
    /// Browser callers use the host/owned-device failure bridge and bounded readbacks instead
    /// of blocking their event loop on an error-scope future.
    pub fn within_memory<T>(&self, f: impl FnOnce() -> T) -> Result<T, String> {
        Self::within_context(&self.ctx, f)
    }
    pub(crate) fn within_context<T>(ctx: &GpuContext, f: impl FnOnce() -> T) -> Result<T, String> {
        ctx.check_health()?;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let inflight = ctx.enter_render();
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let internal = ctx.device.push_error_scope(wgpu::ErrorFilter::Internal);
            let validation = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            // Some allocation failures (Adreno's ERROR_INITIALIZATION_FAILED) surface as
            // internal errors or mapped-at-creation panics, not OutOfMemory (#591).
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let invalid = pollster::block_on(validation.pop());
            let error = pollster::block_on(internal.pop()).or(pollster::block_on(scope.pop()));
            // A texture or buffer whose allocation failed is invalid, and so is everything
            // using it: those validation errors follow from the out-of-memory one, and the frame
            // fails with it while the device lives on (CR1; uncaught, they retired the device).
            // A validation error alone is a bug and retires the device, as uncaught ones do.
            let error = match (error, invalid) {
                // A resource whose allocation failed on another frame thread (an upload both
                // use): what that thread's out-of-memory error left behind.
                (None, Some(invalid)) if is_invalid_resource(&invalid) => Some(format!("out of memory before this frame: {invalid}")),
                (None, Some(invalid)) => {
                    let message = format!("uncaptured GPU error: {invalid}");
                    log::error!("{message}");
                    (ctx.device_failure_notifier())(message.clone());
                    #[cfg(test)]
                    #[allow(clippy::panic)]
                    {
                        panic!("{message}");
                    }
                    #[cfg(not(test))]
                    Some(message)
                }
                (error, _) => error.map(|e| e.to_string()),
            };
            let result = match (error, out) {
                (Some(error), _) => Err(error),
                (None, Ok(out)) => ctx.check_health().map(|()| out),
                (None, Err(payload)) => {
                    let reason = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("native backend panicked");
                    Err(format!("GPU rendering: {reason}"))
                }
            };
            if result.is_err() {
                // Out of memory (or a failed allocation reported otherwise) on a device that
                // still works: fewer renders at once from now on (CR1).
                if ctx.check_health().is_ok() {
                    ctx.render_out_of_memory(inflight.in_flight());
                }
                ctx.clear_uploads();
            }
            result
        }
        #[cfg(target_arch = "wasm32")]
        {
            let out = f();
            ctx.check_health().map(|()| out)
        }
    }

    /// Why a frame was not rendered (best effort: device health, else size / support).
    fn decline_reason(&self) -> String {
        match self.ctx.check_health() {
            Err(e) => format!("device failure / out of memory: {e}"),
            Ok(()) if !self.ctx.can_readback() => "no GPU readback is available here".into(),
            Ok(()) => format!(
                "the GPU compositor did not handle the frame (a region of interest, unsupported content, a frame over the {} px texture limit, or an allocation failed)",
                self.ctx.max_dim
            ),
        }
    }

    /// Wait for submitted GPU work with the readback deadline (benchmarks).
    pub fn wait(&self) {
        let _ = self.ctx.poll_readbacks(wgpu::PollType::Wait { submission_index: None, timeout: Some(crate::readback::TIMEOUT) });
    }
}

impl Accelerator for Gpu {
    fn name(&self) -> String {
        self.ctx.name.clone()
    }

    fn available(&self) -> bool {
        self.ctx.check_health().is_ok()
    }

    fn max_inflight(&self) -> Option<usize> {
        self.ctx.max_inflight()
    }

    fn comp_frame(&self, r: &Renderer, comp: ItemId, t: Tick) -> Option<Image> {
        let img = self.render(r, comp, t);
        if img.is_none() {
            // The caller renders the frame on the CPU; say so and why, or an export that falls
            // back frame after frame is only mysteriously slow. Once per comp and reason as a
            // warning (the log keeps only the last few hundred, for the System Report), then at
            // debug level.
            let reason = self.decline_reason();
            let first = self.declined.lock().map(|mut seen| seen.len() < 256 && seen.insert((comp.0, reason.clone()))).unwrap_or(false);
            if first {
                log::warn!(target: "effectcraft_gpu", "GPU declined the frame at {:.3} s, the CPU renders it: {reason}", t.seconds());
            } else {
                log::debug!(target: "effectcraft_gpu", "GPU declined the frame at {:.3} s: {reason}", t.seconds());
            }
        }
        img
    }

    fn supports_effect(&self, id: &str) -> bool {
        self.ctx.check_health().is_ok() && self.ctx.can_readback() && effects::supports(id)
    }

    fn effects(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<Buf> {
        self.ctx.check_health().ok()?;
        effects::run_chain(&mut Enc::new(&self.ctx), chain, buf, levels).map(|(b, _)| b)
    }

    fn effects_kept(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<(Buf, Option<u64>)> {
        self.effects(chain, buf, levels).map(|b| (b, None))
    }
    fn effects_kept_owned(
        &self,
        chain: &[FxStep],
        buf: &Buf,
        levels: Option<f32>,
        owner: Option<&effectcraft_render::cache::GpuOwner>,
    ) -> Option<(Buf, Option<u64>)> {
        self.ctx.check_health().ok()?;
        let mut e = Enc::with_owner(&self.ctx, owner.cloned());
        let (b, img) = effects::run_chain(&mut e, chain, buf, levels)?;
        Some((b, img.and_then(|img| e.keep(img))))
    }

    fn adopt(&self, ticket: u64, buf: &Arc<Buf>) {
        self.ctx.adopt(ticket, buf);
    }

    fn release(&self, ticket: u64) {
        self.ctx.release(ticket);
    }

    fn raster_3d(&self, scene: &effectcraft_render::three_d::adv::Scene) -> Option<effectcraft_render::three_d::adv::Target> {
        self.ctx.check_health().ok()?;
        adv3d::render(&self.ctx, scene)
    }

    fn render_3d(&self, run: &effectcraft_render::three_d::adv::Prepared) -> Option<effectcraft_render::three_d::adv::Rendered> {
        self.ctx.check_health().ok()?;
        adv3d::render_prepared(&self.ctx, run)
    }

    fn particles(&self) -> Option<&dyn effectcraft_effects::psim::ParticleSim> {
        self.ctx.check_health().ok()?;
        self.ctx.can_readback().then_some(self as &dyn effectcraft_effects::psim::ParticleSim)
    }

    fn auto_pick(&self) -> Option<&effectcraft_render::AutoPick> {
        Some(&self.auto)
    }

    fn frame_begin(&self) {
        if let Some(d) = self.ctx.deferred() {
            d.frame_begin();
        }
    }

    fn pass_begin(&self) {
        if let Some(d) = self.ctx.deferred() {
            d.pass_begin();
        }
    }

    fn pass_missed(&self) -> bool {
        self.ctx.deferred().is_some_and(|d| d.missed())
    }

    fn miss_gate(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.ctx.deferred().map(|d| d.gate())
    }

    fn settle(&self) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>> {
        let d = self.ctx.deferred()?.clone();
        // Natively the device delivers the readbacks when polled: wait for them here.
        #[cfg(not(target_arch = "wasm32"))]
        if d.in_flight() > 0 {
            let _ = self.ctx.poll_readbacks(wgpu::PollType::Wait { submission_index: None, timeout: Some(crate::readback::TIMEOUT) });
        }
        Some(Box::pin(async move { d.settled().await }))
    }
}

impl effectcraft_effects::psim::ParticleSim for Gpu {
    fn simulate(&self, req: &effectcraft_effects::psim::SimRequest) -> Option<Vec<effectcraft_effects::psim::SimParticle>> {
        self.ctx.check_health().ok()?;
        particles::simulate(&self.ctx, req)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_3d;
#[cfg(test)]
mod tests_adjust;
#[cfg(test)]
mod tests_adv3d;
#[cfg(test)]
mod tests_box;
#[cfg(test)]
mod tests_codes;
#[cfg(test)]
mod tests_deferred;
#[cfg(test)]
mod tests_deferred_jobs;
#[cfg(test)]
mod tests_fx_codes;
#[cfg(test)]
mod tests_fx_color;
#[cfg(test)]
mod tests_fx_deepglow;
#[cfg(test)]
mod tests_fx_depth;
#[cfg(test)]
mod tests_fx_distort;
#[cfg(test)]
mod tests_fx_extra;
#[cfg(test)]
mod tests_fx_gen2;
#[cfg(test)]
mod tests_fx_generate;
#[cfg(test)]
mod tests_fx_key;
#[cfg(test)]
mod tests_fx_light;
#[cfg(test)]
mod tests_fx_lut;
#[cfg(test)]
mod tests_fx_noise;
#[cfg(test)]
mod tests_fx_particles;
#[cfg(test)]
mod tests_fx_pixel2;
#[cfg(test)]
mod tests_fx_sim;
#[cfg(test)]
mod tests_fx_styles;
#[cfg(test)]
mod tests_fx_stylize;
#[cfg(test)]
mod tests_fx_text;
#[cfg(test)]
mod tests_fx_time;
#[cfg(test)]
mod tests_fx_tone;
#[cfg(test)]
mod tests_fx_transition;
#[cfg(test)]
mod tests_fx_vr;
#[cfg(test)]
mod tests_fx_warp;
#[cfg(test)]
mod tests_glow_reach;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_init;
#[cfg(test)]
mod tests_inplace;
#[cfg(test)]
mod tests_layer_times;
#[cfg(test)]
mod tests_particles;
#[cfg(test)]
mod tests_plane3d;
#[cfg(test)]
mod tests_precomp_clip;
#[cfg(test)]
mod tests_precomp_resident;
#[cfg(test)]
mod tests_shape_clip;
#[cfg(test)]
mod tests_traffic_light;
#[cfg(test)]
mod tests_warmup;

mod yuv;

/// Whether native YUV plane transport is enabled. Profiling builds support an
/// A/B switch; ordinary builds always use the validated path when supported.
pub fn yuv_transport_enabled() -> bool {
    #[cfg(feature = "profile")]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var("EFFECTCRAFT_PROFILE_YUV").as_deref() != Ok("0"))
    }
    #[cfg(not(feature = "profile"))]
    {
        true
    }
}

#[cfg(test)]
mod tests_cache_policy;

#[cfg(test)]
mod tests_cull;

#[cfg(test)]
mod tests_collapsed_resident;

#[cfg(test)]
mod tests_motion_blur_accum;

#[cfg(test)]
mod tests_dispatch;

/// A validation error about a resource that is invalid ("Buffer with 'data' label is
/// invalid"): its creation failed, out of memory (CR1).
#[cfg(not(target_arch = "wasm32"))]
fn is_invalid_resource(e: &wgpu::Error) -> bool {
    let text = format!("{e:?} {e}");
    text.contains("is invalid")
}
