//! The pre-pass: before a comp's layers are composited one at a time (the CPU walk in
//! `renderer.rs`, the GPU walk in `effectcraft-gpu`), the 2D content they are about to ask for
//! (text, shapes and solids with their masks and layer styles, and the track mattes they read) is
//! produced on every core, so the walk finds it ready instead of rasterising each layer in turn.
//!
//! Exactness: every job calls the very function the walk calls for that layer
//! ([`Renderer::styled_blend`], [`Renderer::matte_buf`], [`Renderer::resident_source`]) with the
//! same renderer and context. The first two leave their result in the layer cache under the keys
//! the walk looks up; the third is handed back under the layer's content key (which fixes those
//! pixels) for the GPU walk to take. A wrong guess about what the walk will ask for only costs
//! time (an unused buffer, or a layer the walk renders itself, as before), never pixels.
//!
//! Left to the walk: 3D layers, precomps (collapsed or not), footage, adjustment layers,
//! Wireframe, and layers with effects (their effects may run on the accelerator) except where
//! the GPU walk runs those effects itself on resident sources.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use effectcraft_effects::Buf;
use effectcraft_project::{Layer, LayerSource, Quality};
use rayon::prelude::*;

use crate::layer_buf::ContentClip;
use crate::motion_blur::MotionMemo;
use crate::{EvalCtx, Renderer, styles};

static OFF: AtomicBool = AtomicBool::new(false);
static PRODUCED: AtomicU64 = AtomicU64::new(0);

/// Turn the pre-pass off (`false`: every layer is produced by the walk, as before) or back on,
/// process-wide: A/B comparisons and tests.
#[doc(hidden)]
pub fn set_prefetch(on: bool) {
    OFF.store(!on, Ordering::SeqCst);
}

/// How many layer buffers the pre-pass has produced (process-wide; diagnostics and tests).
#[doc(hidden)]
pub fn prefetched() -> u64 {
    PRODUCED.load(Ordering::Relaxed)
}

/// Where the GPU walk finds the resident source the pre-pass made for `layer` (content key
/// `key`) in `ctx`: the key fixes the pixels, and the comp, layer and time fix where effect
/// space starts (the buffer's offset).
pub fn prefetch_slot(ctx: &EvalCtx, layer: &Layer, key: u64) -> u64 {
    let k = crate::cache::derive(key, ctx.comp_id.0);
    crate::cache::derive(crate::cache::derive(k, layer.id.0), ctx.time.0 as u64)
}

/// Whether the GPU walk makes track mattes resident on the device (`walk::matte_image` in
/// `effectcraft-gpu`; `EFFECTCRAFT_GPU_MATTES=0`: the CPU draws them and they are uploaded, as
/// before 1 Nov 2026, for A/B). Read once per process.
pub fn gpu_mattes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_GPU_MATTES").as_deref() != Ok("0"))
}

/// One buffer the walk will ask for.
#[derive(Clone, Copy)]
enum Job<'l> {
    /// [`Renderer::styled_blend`] of a layer (what both walks composite), into the layer cache.
    Styled(&'l Layer),
    /// [`Renderer::matte_buf`] of a track matte layer, into the layer cache.
    Matte(&'l Layer),
    /// [`Renderer::resident_source`] of a plain layer the GPU walk keeps resident, under its
    /// content key.
    Resident(&'l Layer, u64, ContentClip),
}

impl<'l> Job<'l> {
    /// The layer the job produces content for (its render time gets the job's).
    fn layer(&self) -> &'l Layer {
        match *self {
            Job::Styled(l) | Job::Matte(l) | Job::Resident(l, ..) => l,
        }
    }
}

/// Layers whose content the pre-pass can produce: rasterised in the renderer, no footage or
/// nested comp behind them.
fn rasterised(layer: &Layer) -> bool {
    matches!(layer.source, LayerSource::Text | LayerSource::Shape | LayerSource::Solid { .. })
}

/// The layer runs effects (on the accelerator, when one is active).
fn has_effects(layer: &Layer) -> bool {
    layer.switches.effects && layer.effects().is_some_and(|f| !f.children.is_empty())
}

impl<'a> Renderer<'a> {
    /// The CPU walk's pre-pass over `visible` (drawn into a `size` canvas).
    pub(crate) fn prefetch_cpu(&self, ctx: &EvalCtx<'a>, visible: &[&'a Layer], size: (u32, u32)) {
        // Freeze frames replace whole runs of layers (see `draw_layers`).
        if self.freeze.is_some() {
            return;
        }
        let _ = self.prefetch(ctx, visible, size, None);
    }

    /// Produce, on every core, the content the walk over `visible` (bottom to top, drawn into a
    /// `size` canvas) is about to ask for and the layer cache doesn't hold. `resident`: the GPU
    /// walk keeps plain layers resident (its `draw_ordinary_resident`), and already holds what
    /// `resident(content key, slot)` answers `true` for; those layers' sources come back as
    /// ([`prefetch_slot`], buffer) for it to take instead of calling [`Self::resident_source`].
    /// Production stops once a quarter of the cache budget is spent; the walk renders the rest
    /// itself.
    pub fn prefetch(&self, ctx: &EvalCtx<'a>, visible: &[&'a Layer], size: (u32, u32), resident: Option<&dyn Fn(u64, u64) -> bool>) -> Vec<(u64, Buf)> {
        let Some(cache) = self.cache else { return vec![] };
        if OFF.load(Ordering::Relaxed) || visible.len() < 2 {
            return vec![];
        }
        let _profile = crate::profile::span("prepass");
        let keep = resident.is_some();
        // What each layer needs, worked out in parallel (keys and bounds evaluate properties).
        let planned: Vec<[Option<Job<'a>>; 2]> =
            visible.par_iter().map(|&l| crate::eval::with_transform_memo(self.project, || self.plan(ctx, l, size, keep))).collect();
        let mut seen = HashSet::new();
        let mut slots = HashSet::new();
        let jobs: Vec<Job<'a>> = planned
            .into_iter()
            .flatten()
            .flatten()
            .filter(|j| match *j {
                Job::Styled(l) => seen.insert((l.id, false)),
                Job::Matte(l) => seen.insert((l.id, true)),
                Job::Resident(l, key, _) => {
                    let slot = prefetch_slot(ctx, l, key);
                    !resident.is_some_and(|have| have(key, slot)) && cache.peek(key).is_none() && slots.insert(slot)
                }
            })
            .collect();
        if jobs.len() < 2 {
            return vec![];
        }
        let budget = cache.budget() / 4;
        let chunk = rayon::current_num_threads().max(1) * 2;
        let mut spent = 0usize;
        let mut out = vec![];
        for part in jobs.chunks(chunk) {
            if spent >= budget {
                break;
            }
            let made: Vec<(usize, Option<(u64, Buf)>)> =
                part.par_iter().map(|j| self.timed(ctx, j.layer(), || crate::eval::with_transform_memo(self.project, || self.produce(ctx, *j)))).collect();
            PRODUCED.fetch_add(part.len() as u64, Ordering::Relaxed);
            for (bytes, r) in made {
                spent = spent.saturating_add(bytes);
                out.extend(r);
            }
        }
        out
    }

    /// The buffers the walk will ask for to draw `layer`: its own, and its track matte's.
    fn plan(&self, ctx: &EvalCtx<'a>, layer: &'a Layer, size: (u32, u32), resident: bool) -> [Option<Job<'a>>; 2] {
        // The walks' own tests, in their order (`draw_layers`, `draw_layer`): 3D runs,
        // adjustment layers and Wireframe go elsewhere; invisible and off-canvas layers draw
        // nothing. (Collapsed precomps aren't `rasterised`.)
        if layer.is_3d() || layer.switches.adjustment || self.quality(layer) == Quality::Wireframe || self.layer_opacity(ctx, layer) <= 0.0 {
            return [None, None];
        }
        if self.off_canvas(ctx, layer, size, &MotionMemo::default()) {
            return [None, None];
        }
        let matte =
            self.track_matte(ctx, layer).map(|(m, _)| m).filter(|m| m.is_active_at(ctx.time) && rasterised(m)).and_then(|m| self.matte_job(ctx, m, resident));
        if !rasterised(layer) {
            return [matte, None];
        }
        if resident
            && !styles::active(ctx, layer)
            && let Some((key, clip)) = self.resident_content(ctx, layer)
        {
            return [matte, Some(Job::Resident(layer, key, clip))];
        }
        // Already cached: the walk's lookup is the only one (a pre-pass lookup would count a
        // second use and take the entry off probation early).
        [matte, (!has_effects(layer) && !self.styled_blend_held(ctx, layer)).then_some(Job::Styled(layer))]
    }

    /// The job for the track matte layer `m`. The GPU walk's resident path (`resident`) makes a
    /// matte's content resident as a plain layer's (`walk::matte_image`), so it gets the
    /// resident source (skipped when the GPU rasterises or holds it); otherwise
    /// [`Self::matte_buf`], for a matte without effects.
    fn matte_job(&self, ctx: &EvalCtx<'a>, m: &'a Layer, resident: bool) -> Option<Job<'a>> {
        if resident
            && gpu_mattes()
            && let Some((key, clip)) = self.resident_content(ctx, m)
        {
            return Some(Job::Resident(m, key, clip));
        }
        (!has_effects(m)).then_some(Job::Matte(m))
    }

    /// Run one job: the bytes it made, and a resident source with its slot.
    fn produce(&self, ctx: &EvalCtx<'a>, job: Job<'a>) -> (usize, Option<(u64, Buf)>) {
        let bytes = |b: &Buf| b.img.data.len().saturating_mul(std::mem::size_of::<effectcraft_raster::Px>());
        match job {
            Job::Styled(l) => {
                let Some(st) = self.styled_blend(ctx, l, None) else { return (0, None) };
                let passes: usize = st.passes.iter().map(|(b, _)| bytes(b)).sum();
                (bytes(&st.body).saturating_add(passes), None)
            }
            Job::Matte(l) => (self.matte_buf(ctx, l).map_or(0, |b| bytes(&b)), None),
            Job::Resident(l, key, clip) => match self.resident_source(ctx, l, clip) {
                Some(b) => (bytes(&b), Some((prefetch_slot(ctx, l, key), b))),
                None => (0, None),
            },
        }
    }
}
