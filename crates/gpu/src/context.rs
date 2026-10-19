//! Device, pipelines, textures, uploads and readback.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use effectcraft_effects::Buf;
use effectcraft_raster::Image;
use effectcraft_render::cache::{GpuOwner, GpuPending, GpuSlot, GpuStage};

/// Compute entry points in `kernels.wgsl` (one pipeline each).
const ENTRIES: &[&str] = &[
    "warp_blend",
    "warp_accum",
    "blend_full",
    "matte",
    "preserve",
    "knockout",
    "channel_mix",
    "quantize",
    "convert",
    "half",
    "adjust_mix",
    "bokeh_boost",
    "bokeh_prefix",
    "bokeh_gather",
    "fill",
];

/// Effect entry points in `kernels.wgsl`. Like the `fx_*` kernels, one that fails to build
/// (a driver's shader compiler rejecting it, #613) leaves its effects to the CPU instead of
/// turning the whole compositor off.
/// (The in-place `*_at` passes too: without one, its full-canvas pass runs instead.)
const EFFECT_ENTRIES: &[&str] = &[
    "box_h",
    "box_v",
    "directional",
    "glow_bright",
    "glow_combine",
    "glow_ab",
    "shadow_make",
    "shadow_combine",
    "pointwise",
    "warp_blend_at",
    "warp_accum_at",
    "blend_full_at",
    "matte_at",
    "channel_mix_at",
];

/// Reference kernels the tests compare against (`box_h_serial`: `box_h` as it was).
const TEST_ENTRIES: &[&str] = if cfg!(test) { &["box_h_serial"] } else { &[] };

/// Entry points that also bind group 1 (four read-only storage buffers; see
/// [`Enc::dispatch_ext`]).
const EXT_ENTRIES: &[&str] = &["classic3d"];

/// Pixel format of every working texture: premultiplied RGBA, 32-bit float (32 bpc headroom;
/// 8/16 bpc are emulated by clamping and quantising, as on the CPU).
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// Uploaded layer buffers kept on the GPU (bytes), at most (see [`GpuContext::set_cache_budget`]).
const UPLOAD_BUDGET: usize = 1536 << 20;

/// Disable encoder-local reuse for output comparisons. Submit barriers always remain active.
pub(crate) static TEXTURE_REUSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Effect chain results awaiting adoption (see [`GpuContext::keep`]), at most.
const MAX_KEPT: usize = 16;

/// Working textures kept for reuse once their last image is dropped (bytes), at most.
const POOL_BUDGET: usize = 768 << 20;

/// Free upload textures of integer samples (packed layer codes, video planes, masks) kept for
/// reuse (bytes), at most (see [`GpuContext::plane_texture`]).
const PLANE_BUDGET: usize = 128 << 20;

/// Small auxiliary caches must be bounded across sizes, not independently per size.
const AUX_CACHE_BUDGET: u64 = 64 << 20;

/// A GPU image: premultiplied RGBA f32 in a texture (cheap to clone; immutable once written,
/// except that an image no one else can see may be drawn on in place: see [`Enc::owns`]).
/// Working images come from the context's texture pool and go back to it when the last clone
/// is dropped.
#[derive(Clone, Debug)]
pub struct GpuImage {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    /// The texture's default view, made once (kernels bind it on every dispatch).
    pub(crate) view: wgpu::TextureView,
    _lease: Option<Arc<Lease>>,
    /// Already clamped and quantised to this many levels (8/16 bpc), so quantising again is a
    /// no-op (see `ops::quantize`).
    pub(crate) levels: Option<f32>,
    // Last field: resource handles/lease are dropped before the accounting pin.
    pub(crate) allocation: Option<Arc<effectcraft_render::allocation::GpuAllocation>>,
}

/// Profiling builds: `EFFECTCRAFT_PACKED_RETAIN=0` keeps retained 8 bpc images as 32-bit floats
/// (A/B of [`PackedImage`]).
fn packed_retention() -> bool {
    if !cfg!(feature = "profile") {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_PACKED_RETAIN").as_deref() != Ok("0"))
}

/// Profiling builds: `EFFECTCRAFT_PRIVATE_REUSE=0` makes every released working texture wait
/// for all open encoders again (A/B of [`Lease`]'s private release).
fn private_reuse() -> bool {
    if !cfg!(feature = "profile") {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_PRIVATE_REUSE").as_deref() != Ok("0"))
}

impl GpuImage {
    /// Mark the image as readable by other encoders (it is going into a cache): once dropped,
    /// its texture waits for every encoder open then before it is reused.
    pub(crate) fn share(&self) {
        if let Some(l) = &self._lease {
            l.shared.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// An image on a texture of its own (not pooled).
    pub fn new(texture: wgpu::Texture, width: u32, height: u32) -> GpuImage {
        Self::in_generation(texture, width, height, effectcraft_render::allocation::GpuGeneration::default())
    }
    fn in_generation(texture: wgpu::Texture, width: u32, height: u32, generation: effectcraft_render::allocation::GpuGeneration) -> GpuImage {
        let allocation = (texture.format() == FORMAT && texture.mip_level_count() == 1 && texture.sample_count() == 1 && texture.depth_or_array_layers() == 1)
            .then(|| effectcraft_render::allocation::GpuAllocation::rgba_f32(texture.width(), texture.height(), generation))
            .flatten();
        let view = texture.create_view(&Default::default());
        GpuImage { allocation, texture, width, height, view, _lease: None, levels: None }
    }
}

/// Hands a pooled texture back to its [`Pool`] when the last [`GpuImage`] holding it drops.
struct Lease {
    pooled: Pooled,
    pool: Weak<Mutex<Pool>>,
    /// The command encoder the texture was taken for (see [`Enc::owns`]).
    encoder: Option<u64>,
    /// Counts the texture as handed out until this lease drops (see [`VramReport`]).
    meter: Arc<Meter>,
    /// The command encoder the taking [`Enc`] has open now (0: none; see [`Enc::cell`]), for
    /// working images; `None` for uploads and other images any encoder may read.
    cell: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// The image went where other encoders can read it (a cache: see [`GpuImage::share`]).
    shared: std::sync::atomic::AtomicBool,
    /// Where it was taken (see [`VramReport::peak_sites`]).
    site: &'static std::panic::Location<'static>,
}

/// A pooled working texture with its default view, made once with the texture and reused with
/// it (a view per dispatch's scratch image costs CPU time; the pixels are the texture's).
#[derive(Clone)]
pub(crate) struct Pooled {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl From<wgpu::Texture> for Pooled {
    fn from(texture: wgpu::Texture) -> Pooled {
        let view = texture.create_view(&Default::default());
        Pooled { texture, view }
    }
}

impl Pooled {
    fn bytes(&self) -> usize {
        tex_bytes(&self.texture)
    }
    /// The pool key: size and format (8 bpc code and float textures never alias).
    fn size(&self) -> PoolKey {
        (self.texture.width(), self.texture.height(), self.texture.format())
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lease")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.meter.sub(self.pooled.bytes(), self.site);
        use std::sync::atomic::Ordering::Acquire;
        // A working image only its own `Enc` ever recorded (never cached) waits for that `Enc`'s
        // open encoder alone, not for every encoder open anywhere (other frames in flight).
        let private = self.cell.as_ref().filter(|_| !self.shared.load(Acquire) && private_reuse()).map(|c| c.load(Acquire));
        if let Some(pool) = self.pool.upgrade()
            && let Ok(mut p) = pool.lock()
        {
            p.release(self.pooled.clone(), private);
        }
    }
}

/// Bytes of pooled textures handed out (working images, uploads, retained results) and not yet
/// given back, with the high-water mark since the last [`GpuContext::reset_vram_peak`]
/// (diagnostics: `bench --rt-profile`). Profiling builds also count them by the call site that
/// took them, and keep that breakdown as it was at the peak.
#[derive(Default)]
pub(crate) struct Meter {
    live: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    /// Images handed to a cache so far, by kind: (count, bytes).
    published: Mutex<std::collections::BTreeMap<&'static str, (u64, u64)>>,
    #[cfg(feature = "profile")]
    sites: Mutex<SiteBytes>,
}

#[cfg(feature = "profile")]
#[derive(Default)]
struct SiteBytes {
    live: HashMap<&'static std::panic::Location<'static>, usize>,
    at_peak: HashMap<&'static std::panic::Location<'static>, usize>,
}

impl Meter {
    pub(crate) fn publish(&self, kind: &'static str, img: &GpuImage) {
        self.publish_bytes(kind, u64::from(img.width).saturating_mul(u64::from(img.height)).saturating_mul(16));
    }
    pub(crate) fn publish_bytes(&self, kind: &'static str, bytes: u64) {
        if let Ok(mut p) = self.published.lock() {
            let e = p.entry(kind).or_default();
            e.0 = e.0.saturating_add(1);
            e.1 = e.1.saturating_add(bytes);
        }
    }
    fn add(&self, bytes: usize, site: &'static std::panic::Location<'static>) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.live.fetch_add(bytes, Relaxed).saturating_add(bytes);
        let new_peak = self.peak.fetch_max(now, Relaxed) < now;
        #[cfg(feature = "profile")]
        if let Ok(mut s) = self.sites.lock() {
            let e = s.live.entry(site).or_default();
            *e = e.saturating_add(bytes);
            if new_peak {
                s.at_peak = s.live.clone();
            }
        }
        let _ = (new_peak, site);
    }
    fn sub(&self, bytes: usize, site: &'static std::panic::Location<'static>) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut cur = self.live.load(Relaxed);
        while let Err(now) = self.live.compare_exchange_weak(cur, cur.saturating_sub(bytes), Relaxed, Relaxed) {
            cur = now;
        }
        #[cfg(feature = "profile")]
        if let Ok(mut s) = self.sites.lock()
            && let Some(e) = s.live.get_mut(site)
        {
            *e = e.saturating_sub(bytes);
        }
        let _ = site;
    }
}

/// Where the device's memory is, by what holds it (see [`GpuContext::vram_report`]).
#[derive(Clone, Debug, Default)]
pub struct VramReport {
    /// The device allocator's own totals (Vulkan, Direct3D 12): memory blocks reserved from the
    /// driver, and the bytes of live allocations in them.
    pub reserved: Option<u64>,
    pub allocated: Option<u64>,
    /// Live allocations by label (bytes, count), most bytes first.
    pub by_label: Vec<(String, u64, usize)>,
    /// Pooled textures handed out now (working images in use, uploads, retained results), and
    /// the most since [`GpuContext::reset_vram_peak`].
    pub leased: usize,
    pub leased_peak: usize,
    /// Free pooled textures, and released ones waiting for an open encoder to be submitted.
    pub pool_free: usize,
    pub pool_released: usize,
    /// Indexed uploads of layer buffers (a subset of `leased`).
    pub uploads: usize,
    /// Transparent read-only images, free integer upload textures, readback staging buffers.
    pub zeros: u64,
    pub planes: u64,
    pub staging: u64,
    /// Profiling builds: `leased` by call site at the peak, most bytes first.
    pub peak_sites: Vec<(String, usize)>,
    /// Images handed to a cache so far, by kind (resident, upload, adopt, kept, native, mips):
    /// (kind, count, bytes).
    pub published: Vec<(String, u64, u64)>,
}

/// Recycled working textures: allocating (and zero-initialising) a 1080p RGBA f32 texture
/// costs more than compositing a small layer into it, and a frame makes several per layer.
///
/// A released texture may still be read by commands recorded in an encoder that has not been
/// submitted yet. Reusing it in another encoder that is submitted first would overwrite it
/// before those reads run, so a released texture waits until every encoder that was open when
/// it was released has been submitted (or dropped).
#[derive(Default)]
pub(crate) struct Pool {
    free: HashMap<PoolKey, Vec<Pooled>>,
    /// (texture, encoder it waits for, private): a shared texture waits for the newest encoder
    /// open at its release (and every older one); a private one for its own `Enc`'s encoder.
    released: Vec<(Pooled, u64, bool)>,
    released_bytes: usize,
    /// Encoders with recorded, unsubmitted work.
    open: BTreeSet<u64>,
    next: u64,
    bytes: usize,
    /// Byte budget of the free textures (`None`: [`POOL_BUDGET`]).
    budget: Option<usize>,
    /// Working textures reused / newly allocated (diagnostics, tests).
    hits: u64,
    misses: u64,
    /// When each free size was last released or reused (a counter, see [`Pool::evict_stalest`]).
    last_used: HashMap<PoolKey, u64>,
    tick: u64,
}

/// Processed pixels can be GPU-only. CPU mirrors, resident pixels and pending
/// outputs already share the layer owner's physical budget; retain them up to
/// that budget on either adapter type, with the device's existing hard ceiling.
///
/// `memory` is the machine's physical RAM on an integrated GPU, its video memory on a discrete
/// one, in bytes (0: unknown). Integrated GPUs share the RAM, so their working-texture pool
/// scales with it: 128 MiB below 16 GiB (weak iGPUs, small laptops), rising linearly to
/// [`POOL_BUDGET`] at 32 GiB and more (Apple Silicon with plenty of unified memory, where a
/// 128 MiB pool misses on most layer sizes). Discrete GPUs with plenty of video memory get a
/// larger pool ([`discrete_pool_cap`]).
fn cache_budgets(layer_cache: usize, integrated: bool, memory: u64) -> (usize, usize, u64) {
    let upload_cap = retained_budget_override().unwrap_or_else(|| retained_cap_for(integrated, memory));
    let pool_cap = if integrated { unified_pool_cap(memory) } else { discrete_pool_cap(memory) };
    // Keep room for one 1080p RGBA-f32 buffer even on shared memory.
    let auxiliary_cap = if integrated { 32 << 20 } else { AUX_CACHE_BUDGET };
    (layer_cache.min(upload_cap), (layer_cache / 2).min(pool_cap), ((layer_cache / 16) as u64).min(auxiliary_cap))
}

/// The ceiling of retained textures (layer uploads, kept results): [`UPLOAD_BUDGET`], or
/// [`RETAINED_MAX`] with at least 16 GiB of video memory (an eighth of it) or 32 GiB of shared RAM.
/// Retained 8 bpc images are codes ([`PackedImage`], a quarter of the float bytes), so 3 GiB
/// holds a 1080p section of a few hundred layers (the 260 photos of a scrolling wall needed
/// ~1.7 GiB; at 1.5 GiB they evicted each other every frame and re-rendered on the CPU).
fn retained_cap_for(integrated: bool, memory: u64) -> usize {
    const GIB: u64 = 1 << 30;
    let cap = match integrated {
        true if memory >= 32 * GIB => RETAINED_MAX as u64,
        false if memory >= 16 * GIB => (memory / 8).min(RETAINED_MAX as u64),
        _ => UPLOAD_BUDGET as u64,
    };
    usize::try_from(cap).unwrap_or(UPLOAD_BUDGET).max(UPLOAD_BUDGET)
}

/// Largest retained-texture ceiling (see [`retained_cap_for`]).
const RETAINED_MAX: usize = 3 << 30;

/// The working-texture pool's cap on an integrated GPU with `system_memory` bytes of RAM (see
/// [`cache_budgets`]).
fn unified_pool_cap(system_memory: u64) -> usize {
    const SMALL: u64 = 128 << 20;
    const LOW: u64 = 16 << 30;
    const HIGH: u64 = 32 << 30;
    let full = POOL_BUDGET as u64;
    let cap = if system_memory < LOW {
        SMALL
    } else if system_memory >= HIGH {
        full
    } else {
        SMALL + (full - SMALL) * ((system_memory - LOW) >> 20) / ((HIGH - LOW) >> 20)
    };
    usize::try_from(cap).unwrap_or(POOL_BUDGET)
}

/// The working-texture pool's cap on a discrete GPU with `vram` bytes of video memory (0:
/// unknown; see [`cache_budgets`]): [`POOL_BUDGET`], or a sixteenth of the video memory from
/// 12 GiB on, up to [`DISCRETE_POOL_MAX`] (CR1: 1.5 GiB on a 24 GB card, where many frames
/// render at once and a 768 MiB pool misses).
fn discrete_pool_cap(vram: u64) -> usize {
    usize::try_from(vram / 16).unwrap_or(DISCRETE_POOL_MAX).clamp(POOL_BUDGET, DISCRETE_POOL_MAX)
}

/// Largest working-texture pool on a discrete GPU (see [`discrete_pool_cap`]).
const DISCRETE_POOL_MAX: usize = 2 << 30;

/// Profiling builds: `EFFECTCRAFT_RT_POOL_MB` overrides the working-texture pool's budget
/// (measuring how much a larger pool saves; see [`GpuContext::set_cache_budget`]).
fn pool_budget_override() -> Option<usize> {
    if !cfg!(feature = "profile") {
        return None;
    }
    static MB: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    MB.get_or_init(|| std::env::var("EFFECTCRAFT_RT_POOL_MB").ok()?.parse::<usize>().ok()?.checked_mul(1 << 20)).to_owned()
}

/// Profiling builds: `EFFECTCRAFT_RT_RETAINED_MB` overrides the retained-texture ceiling
/// ([`UPLOAD_BUDGET`]; measuring whether a frame's retained set thrashes it).
fn retained_budget_override() -> Option<usize> {
    if !cfg!(feature = "profile") {
        return None;
    }
    static MB: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    MB.get_or_init(|| std::env::var("EFFECTCRAFT_RT_RETAINED_MB").ok()?.parse::<usize>().ok()?.checked_mul(1 << 20)).to_owned()
}

/// Where working textures missed the pool (profiling builds): (call site, width, height) →
/// count, drained by [`take_pool_miss_sites`].
#[cfg(feature = "profile")]
static MISS_SITES: Mutex<std::collections::BTreeMap<(String, u32, u32), u64>> = Mutex::new(std::collections::BTreeMap::new());

/// Pool misses by call site and size since the last call (profiling builds), most first.
#[cfg(feature = "profile")]
pub fn take_pool_miss_sites() -> Vec<(String, u32, u32, u64)> {
    let map = std::mem::take(&mut *MISS_SITES.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    let mut v: Vec<_> = map.into_iter().map(|((site, w, h), n)| (site, w, h, n)).collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.3));
    v
}

/// Counts of uploads (CPU → GPU) and readbacks (GPU → CPU).
#[derive(Default)]
pub(crate) struct Transfers {
    ups: std::sync::atomic::AtomicU64,
    up_bytes: std::sync::atomic::AtomicU64,
    downs: std::sync::atomic::AtomicU64,
    down_bytes: std::sync::atomic::AtomicU64,
    yuv_conversions: std::sync::atomic::AtomicU64,
    yuv_bytes: std::sync::atomic::AtomicU64,
    /// GPU allocations (textures and buffers made, not reused) and their bytes.
    made: std::sync::atomic::AtomicU64,
    made_bytes: std::sync::atomic::AtomicU64,
}

/// A snapshot of [`GpuContext::alloc_stats`]: GPU textures and buffers newly allocated (not
/// reused from a pool) so far, and their bytes. A fresh allocation costs CPU time to make and,
/// on first use, GPU time to clear and map its pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    pub allocations: u64,
    pub bytes: u64,
}

/// A snapshot of [`GpuContext::transfer_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    pub uploads: u64,
    pub upload_bytes: u64,
    pub readbacks: u64,
    pub readback_bytes: u64,
    /// Native plane conversion dispatches and their input sample bytes.
    pub yuv_conversions: u64,
    pub yuv_bytes: u64,
}

impl Transfers {
    pub(crate) fn count_yuv(&self, bytes: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.yuv_conversions.fetch_add(1, Relaxed);
        self.yuv_bytes.fetch_add(bytes as u64, Relaxed);
    }

    pub(crate) fn count_made(&self, bytes: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.made.fetch_add(1, Relaxed);
        self.made_bytes.fetch_add(bytes, Relaxed);
    }

    pub(crate) fn count(&self, up: bool, bytes: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        let (n, b) = if up { (&self.ups, &self.up_bytes) } else { (&self.downs, &self.down_bytes) };
        n.fetch_add(1, Relaxed);
        b.fetch_add(bytes as u64, Relaxed);
    }
}

/// Levels of the 8 and 16 bpc grids (`effectcraft_render::packed`: 255 and After Effects' 32768).
const LEVELS_8: f32 = 255.0;
const LEVELS_16: f32 = 32768.0;

/// `data` as integer codes `k` with `k as f32 * (1.0 / levels)` bitwise equal to each value, the
/// layer cache's lossless packing (`effectcraft_render::packed`), or `None` when a value is off
/// that grid, out of range, negative zero or NaN (or the codes can't be allocated).
fn pack_codes<T: Copy + Default + Send>(data: &[[f32; 4]], levels: f32, conv: impl Fn(u32) -> T + Sync + Send) -> Option<Vec<[T; 4]>> {
    use rayon::prelude::*;
    let inv = 1.0 / levels;
    let level = |v: f32| -> Option<T> {
        if !(0.0..=1.0).contains(&v) {
            return None;
        }
        let k = (v * levels + 0.5) as u32;
        ((k as f32 * inv).to_bits() == v.to_bits()).then(|| conv(k))
    };
    let mut out = Vec::new();
    out.try_reserve_exact(data.len()).ok()?;
    out.resize(data.len(), [T::default(); 4]);
    out.par_chunks_mut(4096).zip(data.par_chunks(4096)).try_for_each(|(dst, src)| {
        for (d, p) in dst.iter_mut().zip(src) {
            *d = [level(p[0])?, level(p[1])?, level(p[2])?, level(p[3])?];
        }
        Some(())
    })?;
    Some(out)
}

fn tex_bytes(t: &wgpu::Texture) -> usize {
    (t.width() as usize).saturating_mul(t.height() as usize).saturating_mul(t.format().block_copy_size(None).unwrap_or(16) as usize)
}

/// A pooled texture's size and format.
type PoolKey = (u32, u32, wgpu::TextureFormat);

impl Pool {
    fn budget(&self) -> usize {
        self.budget.unwrap_or(POOL_BUDGET)
    }

    /// Whether `bytes` more would exceed the budget.
    fn over(&self, bytes: usize) -> bool {
        self.bytes.saturating_add(self.released_bytes).saturating_add(bytes) > self.budget()
    }

    /// Make room for a released texture of `bytes` by dropping free textures of the sizes
    /// used longest ago (never of size `keep`, the incoming one's). A full pool used to drop
    /// the incoming texture instead, keeping stale sizes: at Full resolution a frame's working
    /// set outgrew the budget and every frame allocated its canvases anew (RT10b).
    fn make_room(&mut self, bytes: usize, keep: PoolKey) {
        if bytes > self.budget() {
            return;
        }
        while self.over(bytes) && self.evict_stalest(keep) {}
    }

    /// Drop one free texture of the stalest size other than `keep`; `false` when there is none.
    fn evict_stalest(&mut self, keep: PoolKey) -> bool {
        let Some(key) = self.free.keys().filter(|k| **k != keep).min_by_key(|k| self.last_used.get(k).copied().unwrap_or(0)).copied() else {
            return false;
        };
        if let Some(t) = self.free.get_mut(&key).and_then(Vec::pop) {
            self.bytes = self.bytes.saturating_sub(t.bytes());
        }
        if self.free.get(&key).is_some_and(Vec::is_empty) {
            self.free.remove(&key);
            self.last_used.remove(&key);
        }
        true
    }

    fn touch(&mut self, key: PoolKey) {
        self.tick = self.tick.saturating_add(1);
        self.last_used.insert(key, self.tick);
    }

    /// `private`: the texture held a working image only one `Enc` recorded, whose open
    /// encoder is this (0: none open, so every reader is submitted).
    fn release(&mut self, t: impl Into<Pooled>, private: Option<u64>) {
        let t = t.into();
        let wait = match private {
            Some(0) => None,
            Some(own) if self.open.contains(&own) => Some((own, true)),
            Some(_) => None,
            None => self.open.last().map(|&newest| (newest, false)),
        };
        match wait {
            Some((w, private)) => {
                let bytes = t.bytes();
                self.make_room(bytes, t.size());
                if !self.over(bytes) {
                    self.released.push((t, w, private));
                    self.released_bytes += bytes;
                }
                // Dropping our handle is safe: the encoder retains resources it still uses.
            }
            None => self.make_free(t),
        }
    }

    fn make_free(&mut self, t: Pooled) {
        let b = t.bytes();
        self.make_room(b, t.size());
        if self.over(b) {
            return;
        }
        self.bytes += b;
        self.touch(t.size());
        self.free.entry(t.size()).or_default().push(t);
    }

    fn open(&mut self) -> u64 {
        self.next += 1;
        self.open.insert(self.next);
        self.next
    }

    /// Encoder `id` was submitted or dropped: free what no open encoder can still read.
    fn close(&mut self, id: u64) {
        self.open.remove(&id);
        let oldest = self.open.first().copied().unwrap_or(u64::MAX);
        let ready = |(_, w, private): &(Pooled, u64, bool)| if *private { *w == id } else { *w < oldest };
        if !self.released.iter().any(ready) {
            return;
        }
        let (ready, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.released).into_iter().partition(ready);
        self.released = wait;
        self.released_bytes = self.released.iter().map(|(t, _, _)| t.bytes()).sum();
        for (t, _, _) in ready {
            self.make_free(t);
        }
    }

    /// A last CPU lease released within the sole open encoder can be reused by
    /// later commands in that encoder, and so can a private one (see [`Pool::release`]) with
    /// other encoders open. Separate encoders retain the submit barrier.
    #[cfg(test)]
    fn take_for_encoder(&mut self, w: u32, h: u32, encoder: Option<u64>) -> Option<Pooled> {
        self.take_format(w, h, FORMAT, encoder)
    }

    /// [`Pool::take_for_encoder`] of a texture in `format`.
    fn take_format(&mut self, w: u32, h: u32, format: wgpu::TextureFormat, encoder: Option<u64>) -> Option<Pooled> {
        let sole = self.open.len() == 1;
        if let Some(id) = encoder.filter(|id| *id != 0)
            && TEXTURE_REUSE.load(std::sync::atomic::Ordering::Relaxed)
            && self.open.contains(&id)
            && let Some(index) =
                self.released.iter().position(|(texture, owner, private)| *owner == id && (sole || *private) && texture.size() == (w, h, format))
        {
            let (texture, _, _) = self.released.swap_remove(index);
            self.released_bytes = self.released_bytes.saturating_sub(texture.bytes());
            self.hits = self.hits.saturating_add(1);
            return Some(texture);
        }
        self.take(w, h, format)
    }

    fn take(&mut self, w: u32, h: u32, format: wgpu::TextureFormat) -> Option<Pooled> {
        let key = (w, h, format);
        let t = self.free.get_mut(&key).and_then(Vec::pop);
        if self.free.get(&key).is_some_and(Vec::is_empty) {
            self.free.remove(&key);
            self.last_used.remove(&key);
        } else if t.is_some() {
            self.touch(key);
        }
        match &t {
            Some(t) => {
                self.bytes -= t.bytes();
                self.hits += 1;
            }
            None => {
                self.misses += 1;
                // A cached large texture must not permanently prevent reuse at a
                // smaller preview size. Evict only a free size that cannot coexist
                // with the requested texture at all. Preserve useful mixed smaller
                // sizes when their collective occupancy reaches the budget.
                let px = |f: wgpu::TextureFormat| f.block_copy_size(None).unwrap_or(16) as usize;
                let required = (w as usize).saturating_mul(h as usize).saturating_mul(px(format));
                let budget = self.budget.unwrap_or(POOL_BUDGET);
                if required <= budget {
                    while self.bytes.saturating_add(self.released_bytes).saturating_add(required) > budget {
                        let Some(key) = self
                            .free
                            .keys()
                            .filter(|(w, h, f)| (*w as usize).saturating_mul(*h as usize).saturating_mul(px(*f)).saturating_add(required) > budget)
                            .max_by_key(|(w, h, f)| (*w as usize).saturating_mul(*h as usize).saturating_mul(px(*f)))
                            .copied()
                        else {
                            break;
                        };
                        if let Some(texture) = self.free.get_mut(&key).and_then(Vec::pop) {
                            self.bytes = self.bytes.saturating_sub(texture.bytes());
                        }
                        if self.free.get(&key).is_some_and(Vec::is_empty) {
                            self.free.remove(&key);
                            self.last_used.remove(&key);
                        }
                    }
                }
            }
        }
        t
    }

    fn clear(&mut self) {
        self.free.clear();
        self.last_used.clear();
        self.released.clear();
        self.released_bytes = 0;
        self.bytes = 0;
    }

    /// Bound free and pending-reuse textures together (drop excess handles now).
    fn set_budget(&mut self, budget: usize) {
        self.budget = Some(budget);
        while self.bytes.saturating_add(self.released_bytes) > budget {
            let Some((texture, _, _)) = self.released.pop() else { break };
            self.released_bytes = self.released_bytes.saturating_sub(texture.bytes());
        }
        for list in self.free.values_mut() {
            while self.bytes > budget {
                let Some(t) = list.pop() else { break };
                self.bytes = self.bytes.saturating_sub(t.bytes());
            }
        }
        self.free.retain(|_, l| !l.is_empty());
        let free = &self.free;
        self.last_used.retain(|k, _| free.contains_key(k));
    }
}

/// Bytes of a [`Params`] block.
const PARAMS_SIZE: u64 = 256;

/// [`Params`] blocks per uniform ring: one per dispatch until [`Enc::dispatch_ext`] submits.
const RING_SLOTS: usize = 256;

/// Uniform rings kept for reuse once their [`Enc`] is dropped, at most.
const RINGS_KEPT: usize = 8;

/// Uniform parameter block shared by every kernel (see `shaders/common.wgsl`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Params {
    pub u: [[u32; 4]; 4],
    pub f: [[f32; 4]; 12],
}

impl Params {
    /// The block as uploaded (little-endian `u` then `f`), on the stack: no allocation per dispatch.
    fn bytes(&self) -> [u8; PARAMS_SIZE as usize] {
        let mut v = [0u8; PARAMS_SIZE as usize];
        let words = self.u.iter().flatten().copied().chain(self.f.iter().flatten().map(|x| x.to_bits()));
        for (dst, x) in v.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            dst.copy_from_slice(&x.to_le_bytes());
        }
        v
    }
}

#[derive(Clone)]
struct SlotRef {
    owner: GpuOwner,
    slot: Weak<GpuSlot>,
    epoch: Weak<()>,
}
impl SlotRef {
    fn release(self) {
        self.owner.release(&self.slot);
    }
    fn get<T: Clone + 'static>(&self, generation: &effectcraft_render::allocation::GpuGeneration, stage: GpuStage) -> Option<T> {
        self.owner.get(&self.slot, generation, stage)?.payload::<T>().cloned()
    }
    fn alive(&self) -> bool {
        self.owner.valid() && self.epoch.strong_count() > 0 && self.slot.strong_count() > 0
    }
}
#[derive(Clone)]
struct Upload {
    buf: Weak<Buf>,
    slot: SlotRef,
}
#[derive(Clone)]
pub(crate) struct NativeImage {
    pub img: GpuImage,
    pub offset: [f64; 2],
    pub scale: f64,
}

/// A layer image the compositor places, f32 or 8 bpc codes, with its placement (a retained
/// Processed image, an uploaded layer buffer).
#[derive(Clone)]
pub(crate) struct SourceImage {
    pub img: crate::codes::Pixels,
    pub offset: [f64; 2],
    pub scale: f64,
}

impl From<NativeImage> for SourceImage {
    fn from(n: NativeImage) -> SourceImage {
        SourceImage { img: n.img.into(), offset: n.offset, scale: n.scale }
    }
}

/// A layer buffer's texture ([`GpuContext::upload_buf_owned`]).
pub(crate) enum Uploaded {
    Image(GpuImage),
    Packed(PackedImage),
}

/// A retained [`NativeImage`] on the 8 bpc grid kept as its integer codes (`Rgba8Uint`, a quarter
/// of the 32-bit float bytes), expanded to the identical floats each time it is drawn
/// ([`Enc::resident`]) or read back. A frame's retained set then fits the retained allowance
/// four times over instead of evicting itself every frame.
#[derive(Clone)]
pub(crate) struct PackedImage {
    codes: crate::codes::CodeImage,
    offset: [f64; 2],
    scale: f64,
}

impl PackedImage {
    fn source(&self) -> SourceImage {
        SourceImage { img: crate::codes::Pixels::Codes(self.codes.clone()), offset: self.offset, scale: self.scale }
    }
}

/// A texture of codes made outside the pool, as a [`crate::codes::CodeImage`] accounted at its bytes.
fn code_image(texture: wgpu::Texture, generation: &effectcraft_render::allocation::GpuGeneration) -> Option<crate::codes::CodeImage> {
    let (width, height) = (texture.width(), texture.height());
    let view = texture.create_view(&Default::default());
    let allocation = Some(effectcraft_render::allocation::GpuAllocation::of_bytes(tex_bytes(&texture), generation.clone()));
    crate::codes::CodeImage::wrap(GpuImage { allocation, texture, width, height, view, _lease: None, levels: Some(LEVELS_8) })
}
#[derive(Clone)]
struct NativeUpload {
    owner: Weak<dyn effectcraft_render::footage::FramePixels>,
    token: effectcraft_render::cache::ResidentToken,
    metadata: Vec<u64>,
    slot: SlotRef,
}
#[derive(Default)]
struct Uploads {
    map: HashMap<(usize, usize), Upload>,
    native: HashMap<(usize, usize, u64), NativeUpload>,
    /// Per-device ceiling; the LayerCache owns the authoritative shared budget.
    budget: Option<usize>,
    kept: Vec<(u64, SlotRef)>,
    resident: Vec<SlotRef>,
    next_ticket: u64,
    keep_off: bool,
    epoch: Arc<()>,
}
impl Uploads {
    /// Only weak metadata leaves this lock. Release its owner slot afterwards.
    fn stale(&mut self) -> Option<SlotRef> {
        if let Some(k) = self.map.iter().find(|(_, e)| e.buf.strong_count() == 0 || !e.slot.alive()).map(|(k, _)| *k) {
            return self.map.remove(&k).map(|e| e.slot);
        }
        if let Some(k) = self.native.iter().find(|(_, e)| e.owner.strong_count() == 0 || !e.token.valid() || !e.slot.alive()).map(|(k, _)| *k) {
            return self.native.remove(&k).map(|e| e.slot);
        }
        if let Some(i) = self.kept.iter().position(|(_, e)| !e.alive()) {
            return Some(self.kept.remove(i).1);
        }
        if let Some(i) = self.resident.iter().position(|e| !e.alive()) {
            return Some(self.resident.remove(i));
        }
        None
    }
}

/// Drain weak indexes and release every scoped owner after the index lock.
fn clear_retained(uploads: &Mutex<Uploads>) {
    let old = if let Ok(mut u) = uploads.lock() {
        u.epoch = Arc::new(());
        Some((std::mem::take(&mut u.map), std::mem::take(&mut u.native), std::mem::take(&mut u.kept), std::mem::take(&mut u.resident)))
    } else {
        None
    };
    if let Some((map, native, kept, resident)) = old {
        for (_, e) in map {
            e.slot.release();
        }
        for (_, e) in native {
            e.slot.release();
        }
        for e in resident {
            e.release();
        }
        for (_, e) in kept {
            e.release();
        }
    }
}

/// The device and everything compiled for it.
pub struct GpuContext {
    pub(crate) generation: effectcraft_render::allocation::GpuGeneration,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// RT10e: the window's traffic light, when this device is the window's (its submissions
    /// then pause while a surface reconfigures; see [`crate::traffic_light`]).
    light: std::sync::OnceLock<Arc<crate::TrafficLight>>,
    /// The device belongs to a `wgpu::Instance` of its own ([`Self::request`],
    /// [`Self::request_off_window`]), not the window's.
    own_instance: bool,
    pub(crate) name: String,
    pub(crate) max_dim: u32,
    integrated: bool,
    /// A discrete GPU (video memory of its own), and how much of it, where it can be found out
    /// ([`crate::vram`]).
    discrete: bool,
    vram: Option<crate::vram::VideoMemory>,
    /// The adapter (type, PCI vendor, device) and its video memory as last asked, when (see
    /// [`GpuContext::live_video_memory`]).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    adapter_ids: (wgpu::DeviceType, u32, u32),
    vram_live: Arc<Mutex<Option<(web_time::Instant, crate::vram::VideoMemory)>>>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    vram_asking: Arc<std::sync::atomic::AtomicBool>,
    /// Out-of-memory backoff (CR1): renders in flight now, and at most how many after a render
    /// ran out of memory (`usize::MAX`: no limit yet). See [`GpuContext::max_inflight`].
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    inflight: Arc<std::sync::atomic::AtomicUsize>,
    inflight_ceiling: std::sync::atomic::AtomicUsize,
    /// Physical RAM in bytes (0: unknown; see [`GpuContext::set_system_memory`]).
    system_memory: std::sync::atomic::AtomicU64,
    /// The layer cache budget last given to [`GpuContext::set_cache_budget`].
    layer_budget: std::sync::atomic::AtomicUsize,
    /// Explicit upload ceiling; zero follows the owner-derived memory budget.
    upload_limit: std::sync::atomic::AtomicUsize,
    upload_full_warned: std::sync::atomic::AtomicBool,
    auxiliary_budget: std::sync::atomic::AtomicU64,
    pub(crate) bgl: wgpu::BindGroupLayout,
    bgl_ext: wgpu::BindGroupLayout,
    pub(crate) fusion: crate::fusion::Pipelines,
    pub(crate) yuv: Option<crate::yuv::Pipelines>,
    pipelines: HashMap<&'static str, wgpu::ComputePipeline>,
    /// Effect kernels that failed to build, and why (their effects render on the CPU).
    unavailable: Vec<(&'static str, String)>,
    display_bgl: wgpu::BindGroupLayout,
    display: wgpu::ComputePipeline,
    /// The display texture's next mip level from the one above (`display.wgsl` `mip`).
    display_mip: wgpu::ComputePipeline,
    /// A 1 × 1 image bound where a kernel reads no image.
    pub(crate) dummy: GpuImage,
    /// The kernels that read and write 8 bpc codes (`None`: f32 only, see [`crate::codes`]).
    pub(crate) codes: Option<crate::codes::Bank>,
    dummy_buf: wgpu::Buffer,
    /// Bytes between the [`Params`] blocks of a uniform ring ([`Enc::dispatch_ext`]).
    uniform_stride: u64,
    /// Uniform rings no [`Enc`] holds (reused: see [`Enc::dispatch_ext`]).
    uniforms: Mutex<Vec<wgpu::Buffer>>,
    uploads: Arc<Mutex<Uploads>>,
    /// Minification chains kept with retained images (see [`crate::mips`]).
    pub(crate) mips: crate::mips::Mips,
    pool: Arc<Mutex<Pool>>,
    /// Pooled textures handed out (see [`VramReport`]).
    pub(crate) leased: Arc<Meter>,
    /// CPU ↔ GPU traffic (diagnostics: `bench --gpu`).
    pub(crate) transfers: Transfers,
    /// Readback staging buffers by size.
    staging: Mutex<HashMap<u64, Vec<wgpu::Buffer>>>,
    /// Free upload textures of integer samples by size and format (see
    /// [`GpuContext::plane_texture`]).
    planes: Mutex<HashMap<(u32, u32, wgpu::TextureFormat), Vec<wgpu::Texture>>>,
    /// Transparent read-only images by size (see [`Enc::zeros`]).
    zeros: Mutex<HashMap<(u32, u32), GpuImage>>,
    /// See [`GpuContext::unpack8_scale`].
    unpack8: std::sync::OnceLock<wgpu::Buffer>,
    /// Advanced 3D render pipelines (built on first use).
    pub(crate) adv3d: std::sync::OnceLock<Result<crate::adv3d::Pipes, String>>,
    pub(crate) static_pipeline_count: std::sync::atomic::AtomicU64,
    /// The adapter can run the Advanced 3D rasteriser (`adv3d::raster_unsupported`); without
    /// it Advanced 3D scenes render on the CPU.
    pub(crate) adv3d_raster: bool,
    /// GPU particle pipeline (built on first use) and simulation checkpoints.
    pub(crate) particles: crate::particles::PipesCell,
    pub(crate) particle_states: crate::particles::StatesCell,
    /// Deferred readbacks (a browser worker's WebGPU device), see [`crate::deferred`].
    pub(crate) deferred: Option<Arc<crate::deferred::Deferred>>,
    readbacks: Arc<crate::readback::Readbacks>,
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_tex_entry(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format, view_dimension: wgpu::TextureViewDimension::D2 },
        count: None,
    }
}

/// Counts a render out of [`GpuContext::enter_render`]'s in-flight count.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) struct InflightGuard(Arc<std::sync::atomic::AtomicUsize>);

impl InflightGuard {
    /// Renders in flight now, this one included.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn in_flight(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Paired with `enter_render`'s increment: never below zero.
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Logs (target `effectcraft_stall`, debug) a GPU resource that took long to create.
#[cfg(not(target_arch = "wasm32"))]
struct SlowLog<'a>(&'a str, web_time::Instant);
#[cfg(not(target_arch = "wasm32"))]
impl Drop for SlowLog<'_> {
    fn drop(&mut self) {
        let took = self.1.elapsed();
        if took >= std::time::Duration::from_millis(150) {
            log::debug!(target: "effectcraft_stall", "gpu: creating {} took {:.0} ms on {}", self.0, took.as_secs_f64() * 1000.0, std::thread::current().name().unwrap_or("?"));
        }
    }
}

/// Contain synchronous native initialization errors without replacing the device owner's
/// handlers. Scopes are thread-local with wgpu's enabled `std` feature. The pinned native
/// wgpu-core backend returns an immediately-ready future from pop_error_scope, so these
/// drains do not poll or wait for unrelated submitted GPU work.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn init_resource<T>(device: &wgpu::Device, label: &str, create: impl FnOnce() -> T) -> Result<T, String> {
    let t0 = web_time::Instant::now();
    let _slow = SlowLog(label, t0);
    let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    // Some native drivers/wgpu paths can still unwind instead of reporting a scoped error.
    // Keep the last-resort guard inside the scopes, then drain every scope even on failure.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(create));
    let validation_error = pollster::block_on(validation.pop());
    let internal_error = pollster::block_on(internal.pop());
    let oom_error = pollster::block_on(oom.pop());
    if let Some(error) = validation_error.or(internal_error).or(oom_error) {
        return Err(format!("GPU initialization ({label}): {error}"));
    }
    result.map_err(|payload| {
        let reason =
            payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("native backend panicked");
        format!("GPU initialization ({label}): {reason}")
    })
}

/// Browser scope completion is asynchronous. Preserve the existing browser constructor;
/// never block its event loop or introduce a guard across an asynchronous boundary here.
#[cfg(target_arch = "wasm32")]
pub(crate) fn init_resource<T>(_device: &wgpu::Device, _label: &str, create: impl FnOnce() -> T) -> Result<T, String> {
    Ok(create())
}

/// A buffer holding `contents` (padded to 4 bytes), written through the queue. Unlike
/// `DeviceExt::create_buffer_init`, which maps the buffer at creation and panics when the
/// allocation failed (a driver reports it as an internal error, not out-of-memory: Adreno's
/// `ERROR_INITIALIZATION_FAILED`, #591), a failed allocation only yields an invalid buffer and
/// an error for the caller's error scope.
pub(crate) fn buffer_init(device: &wgpu::Device, queue: &wgpu::Queue, label: Option<&str>, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    let align = wgpu::COPY_BUFFER_ALIGNMENT as usize;
    let padded = contents.len().div_ceil(align).saturating_mul(align);
    let buffer =
        device.create_buffer(&wgpu::BufferDescriptor { label, size: padded as u64, usage: usage | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    if padded > 0 {
        if padded == contents.len() {
            queue.write_buffer(&buffer, 0, contents);
        } else {
            let mut data = contents.to_vec();
            data.resize(padded, 0);
            queue.write_buffer(&buffer, 0, &data);
        }
    }
    buffer
}

impl GpuContext {
    /// [`buffer_init`] on this context's device.
    pub(crate) fn buffer_init(&self, label: Option<&str>, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        buffer_init(&self.device, &self.queue, label, contents, usage)
    }

    /// Build on an existing device (the desktop app shares egui-wgpu's). `Err` when the
    /// adapter cannot run the compositor (no compute shaders, e.g. WebGL2; no float storage
    /// textures) or the device was created with limits below what its pipelines bind.
    /// Preserves the host's device handlers. The host must forward failures using
    /// [`Self::device_failure_notifier`] for immediate readback cancellation.
    pub fn new(adapter: &wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Result<GpuContext, String> {
        let info = adapter.get_info();
        if !adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
            return Err(format!("{} ({:?}): no compute shaders", info.name, info.backend));
        }
        // OpenGL (Window Graphics ▸ OpenGL for drivers that crash otherwise, #243) translates the
        // kernels for tens of seconds before one fails validation: CPU compositing at once.
        if info.backend == wgpu::Backend::Gl {
            return Err(format!("{} (Gl): the GPU compositor needs DirectX 12, Vulkan, Metal or WebGPU", info.name));
        }
        // egui-wgpu asks for WebGL2's limits on GL (no storage buffers at all): building the
        // pipelines there would be a validation error, which release builds only log.
        let l = device.limits();
        let needs = [
            // `adv3d`'s post kernels bind 7 (its rasteriser's fragment stage 6).
            ("max_storage_buffers_per_shader_stage", l.max_storage_buffers_per_shader_stage, 7),
            ("max_storage_textures_per_shader_stage", l.max_storage_textures_per_shader_stage, 1),
            ("max_bind_groups", l.max_bind_groups, 2),
            ("max_compute_workgroup_size_x", l.max_compute_workgroup_size_x, 64),
            ("max_compute_workgroup_size_y", l.max_compute_workgroup_size_y, 16),
            ("max_compute_invocations_per_workgroup", l.max_compute_invocations_per_workgroup, 256),
        ];
        if let Some((limit, have, need)) = needs.iter().find(|(_, have, need)| have < need) {
            return Err(format!("{} ({:?}): device limit {limit} is {have}, needs {need}", info.name, info.backend));
        }
        for f in [FORMAT, wgpu::TextureFormat::Rgba8Unorm] {
            if !adapter.get_texture_format_features(f).allowed_usages.contains(wgpu::TextureUsages::STORAGE_BINDING) {
                return Err(format!("{} ({:?}): {f:?} storage textures unsupported", info.name, info.backend));
            }
        }
        let readbacks = crate::readback::Readbacks::new()?;
        let name = format!("{} ({:?})", info.name, info.backend);
        let adv3d_unsupported = crate::adv3d::raster_unsupported(adapter);
        if let Some(why) = &adv3d_unsupported {
            log::info!("gpu {name}: Advanced 3D renders on the CPU ({why})");
        }
        let src = [
            include_str!("shaders/common.wgsl"),
            include_str!("shaders/kernels.wgsl"),
            include_str!("shaders/classic3d.wgsl"),
            include_str!("shaders/fx_color.wgsl"),
            include_str!("shaders/fx_distort.wgsl"),
            include_str!("shaders/fx_generate.wgsl"),
            include_str!("shaders/fx_key.wgsl"),
            include_str!("shaders/fx_stylize.wgsl"),
            include_str!("shaders/fx_noise.wgsl"),
            include_str!("shaders/fx_tone.wgsl"),
            include_str!("shaders/fx_warp.wgsl"),
            include_str!("shaders/fx_extra.wgsl"),
            include_str!("shaders/fx_depth.wgsl"),
            include_str!("shaders/fx_lut.wgsl"),
            include_str!("shaders/fx_sim.wgsl"),
            include_str!("shaders/fx_particles.wgsl"),
            include_str!("shaders/fx_vr.wgsl"),
            include_str!("shaders/fx_light.wgsl"),
            include_str!("shaders/fx_transition.wgsl"),
            include_str!("shaders/fx_text.wgsl"),
            include_str!("shaders/fx_time.wgsl"),
            include_str!("shaders/fx_pixel2.wgsl"),
            include_str!("shaders/fx_gen2.wgsl"),
            include_str!("shaders/fx_deepglow.wgsl"),
            include_str!("shaders/fx_styles.wgsl"),
            include_str!("shaders/sky.wgsl"),
        ]
        .concat();
        let compile_start = web_time::Instant::now();
        let module = init_resource(&device, "kernels shader module", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("effectcraft kernels"), source: wgpu::ShaderSource::Wgsl(src.into()) })
        })?;
        let bgl = init_resource(&device, "kernels bind-group layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("effectcraft kernels"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                        count: None,
                    },
                    tex_entry(1),
                    tex_entry(2),
                    storage_tex_entry(3, FORMAT),
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            })
        })?;
        let layout = init_resource(&device, "kernels pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("effectcraft kernels"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            })
        })?;
        let storage = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl_ext = init_resource(&device, "extended kernels bind-group layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("effectcraft kernels (ext)"),
                entries: &[storage(0), storage(1), storage(2), storage(3)],
            })
        })?;
        let layout_ext = init_resource(&device, "extended kernels pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("effectcraft kernels (ext)"),
                bind_group_layouts: &[Some(&bgl), Some(&bgl_ext)],
                immediate_size: 0,
            })
        })?;
        // Metal compiles with fast math, which reassociates the staged `box_h`'s running sums
        // differently from the serial kernel's (a few ulps apart), so Metal keeps the serial one.
        let metal = info.backend == wgpu::Backend::Metal;
        let build = |e: &'static str, layout: &wgpu::PipelineLayout| {
            let entry = if metal && e == "box_h" { "box_h_serial" } else { e };
            init_resource(&device, e, || {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(e),
                    layout: Some(layout),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
        };
        // The compositor's own kernels are required; an effect kernel that doesn't build is
        // left out, and its effects render on the CPU (see `Enc::take_missing`).
        let mut pipelines = ENTRIES
            .iter()
            .chain(crate::adv3d::SKY_KERNELS)
            .map(|e| (e, &layout))
            .chain(EXT_ENTRIES.iter().map(|e| (e, &layout_ext)))
            .map(|(e, layout)| Ok((*e, build(e, layout)?)))
            .collect::<Result<HashMap<_, _>, String>>()?;
        let mut unavailable = vec![];
        for e in EFFECT_ENTRIES
            .iter()
            .chain(TEST_ENTRIES)
            .chain(crate::fx_color::KERNELS)
            .chain(crate::fx_distort::KERNELS)
            .chain(crate::fx_generate::KERNELS)
            .chain(crate::fx_key::KERNELS)
            .chain(crate::fx_stylize::KERNELS)
            .chain(crate::fx_noise::KERNELS)
            .chain(crate::fx_tone::KERNELS)
            .chain(crate::fx_warp::KERNELS)
            .chain(crate::fx_extra::KERNELS)
            .chain(crate::fx_depth::KERNELS)
            .chain(crate::fx_lut::KERNELS)
            .chain(crate::fx_sim::KERNELS)
            .chain(crate::fx_particles::KERNELS)
            .chain(crate::fx_vr::KERNELS)
            .chain(crate::fx_light::KERNELS)
            .chain(crate::fx_transition::KERNELS)
            .chain(crate::fx_text::KERNELS)
            .chain(crate::fx_time::KERNELS)
            .chain(crate::fx_pixel2::KERNELS)
            .chain(crate::fx_gen2::KERNELS)
            .chain(crate::fx_deepglow::KERNELS)
            .chain(crate::fx_styles::KERNELS)
        {
            match build(e, &layout) {
                Ok(p) => {
                    pipelines.insert(*e, p);
                }
                Err(why) => {
                    log::warn!("gpu {name}: kernel {e} unavailable, its effects render on the CPU: {why}");
                    unavailable.push((*e, why));
                }
            }
        }
        log::info!(
            "gpu {name}: {} compute kernels built in {:.1} s ({} unavailable)",
            pipelines.len(),
            compile_start.elapsed().as_secs_f64(),
            unavailable.len()
        );
        let dmodule = init_resource(&device, "display shader module", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("effectcraft display"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/display.wgsl").into()),
            })
        })?;
        let display_bgl = init_resource(&device, "display bind-group layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("effectcraft display"),
                entries: &[tex_entry(1), storage_tex_entry(3, wgpu::TextureFormat::Rgba8Unorm)],
            })
        })?;
        let dlayout = init_resource(&device, "display pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("effectcraft display"),
                bind_group_layouts: &[Some(&display_bgl)],
                immediate_size: 0,
            })
        })?;
        let display_pipeline = |entry: &'static str| {
            init_resource(&device, "display compute pipeline", || {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&dlayout),
                    module: &dmodule,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
        };
        let display = display_pipeline("display")?;
        let display_mip = display_pipeline("mip")?;
        let dummy = init_resource(&device, "dummy texture", || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("dummy"),
                size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        })?;
        let dummy_buf =
            init_resource(&device, "dummy storage buffer", || buffer_init(&device, &queue, Some("dummy"), &[0u8; 16], wgpu::BufferUsages::STORAGE))?;
        let dummy_tex = init_resource(&device, "dummy texture view", || dummy.create_view(&Default::default()))?;
        let generation = effectcraft_render::allocation::GpuGeneration::default();
        let allocation = effectcraft_render::allocation::GpuAllocation::rgba_f32(1, 1, generation.clone());
        let dummy = GpuImage { allocation, texture: dummy, width: 1, height: 1, view: dummy_tex, _lease: None, levels: None };
        let align = u64::from(device.limits().min_uniform_buffer_offset_alignment.max(1));
        let uniform_stride = PARAMS_SIZE.div_ceil(align).saturating_mul(align);
        let max_dim = device.limits().max_texture_dimension_2d;
        let integrated = info.device_type == wgpu::DeviceType::IntegratedGpu;
        let vram = if integrated { None } else { crate::vram::dedicated_memory(&info) };
        let (upload_budget, pool_budget, auxiliary_budget) = cache_budgets(UPLOAD_BUDGET, integrated, vram.map_or(0, |m| m.total));
        let yuv = match crate::yuv::Pipelines::new(adapter, &device) {
            Ok(p) => Some(p),
            Err(reason) => {
                unavailable.push(("yuv", reason));
                None
            }
        };
        let static_pipeline_count = std::sync::atomic::AtomicU64::new(pipelines.len() as u64 + 2 + if yuv.is_some() { 3 } else { 0 });
        let codes = match crate::codes::enabled() {
            true => crate::codes::Bank::new(adapter, &device, &queue).map_err(|why| log::info!("gpu {name}: 8 bpc images stay f32 ({why})")).ok(),
            false => None,
        };
        Ok(GpuContext {
            generation,
            device,
            queue,
            light: std::sync::OnceLock::new(),
            own_instance: false,
            name,
            max_dim,
            bgl,
            bgl_ext,
            pipelines,
            fusion: Default::default(),
            yuv,
            unavailable,
            display_bgl,
            display,
            display_mip,
            dummy,
            codes,
            dummy_buf,
            uniform_stride,
            uniforms: Default::default(),
            uploads: Arc::new(Mutex::new(Uploads { budget: Some(upload_budget), ..Default::default() })),
            mips: Default::default(),
            pool: Arc::new(Mutex::new(Pool { budget: Some(pool_budget), ..Default::default() })),
            leased: Default::default(),
            integrated,
            discrete: info.device_type == wgpu::DeviceType::DiscreteGpu,
            vram,
            adapter_ids: (info.device_type, info.vendor, info.device),
            vram_live: Arc::new(Mutex::new(vram.map(|m| (web_time::Instant::now(), m)))),
            vram_asking: Arc::default(),
            inflight: Arc::default(),
            inflight_ceiling: std::sync::atomic::AtomicUsize::new(usize::MAX),
            system_memory: std::sync::atomic::AtomicU64::new(0),
            layer_budget: std::sync::atomic::AtomicUsize::new(UPLOAD_BUDGET),
            upload_limit: std::sync::atomic::AtomicUsize::new(0),
            upload_full_warned: Default::default(),
            auxiliary_budget: std::sync::atomic::AtomicU64::new(auxiliary_budget),
            staging: Default::default(),
            planes: Default::default(),
            transfers: Default::default(),
            zeros: Default::default(),
            unpack8: Default::default(),
            adv3d: std::sync::OnceLock::new(),
            static_pipeline_count,
            adv3d_raster: adv3d_unsupported.is_none(),
            particles: Default::default(),
            particle_states: Default::default(),
            deferred: None,
            readbacks,
        })
    }

    /// An integrated GPU (its memory is the machine's RAM).
    pub fn integrated(&self) -> bool {
        self.integrated
    }

    /// A discrete GPU: video memory of its own.
    pub fn discrete(&self) -> bool {
        self.discrete
    }

    /// Video memory of a discrete GPU (total, and free when the device was made), where it can
    /// be found out (CR1, [`crate::vram`]).
    pub fn video_memory(&self) -> Option<crate::vram::VideoMemory> {
        self.vram
    }

    /// The discrete GPU's video memory now, as far as known: asked again in the background at
    /// most every second (`nvidia-smi` takes a while), so the answer can be a second or two
    /// old. Other programs (another EffectCraft) take and give back memory meanwhile (CR1).
    pub fn live_video_memory(&self) -> Option<crate::vram::VideoMemory> {
        self.vram?;
        let latest = *self.vram_live.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(not(target_arch = "wasm32"))]
        if latest.is_none_or(|(t, _)| t.elapsed() >= std::time::Duration::from_secs(1)) && !self.vram_asking.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let (live, asking, (kind, vendor, device)) = (self.vram_live.clone(), self.vram_asking.clone(), self.adapter_ids);
            let ask = move || {
                if let Some(m) = crate::vram::query(kind, vendor, device) {
                    *live.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((web_time::Instant::now(), m));
                }
                asking.store(false, std::sync::atomic::Ordering::Release);
            };
            if std::thread::Builder::new().name("ec-vram".into()).spawn(ask).is_err() {
                self.vram_asking.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        latest.map(|(_, m)| m).or(self.vram)
    }

    /// Device memory wgpu's allocator holds for this device now, in bytes (Vulkan and Direct3D
    /// 12; `None` elsewhere). Walks every allocation: not for every dispatch.
    pub fn device_bytes(&self) -> Option<u64> {
        self.device.generate_allocator_report().map(|r| r.total_reserved_bytes)
    }

    /// At most how many renders should be in flight at once on this device, after one ran out
    /// of memory: half as many as were in flight then, each time (at least one). `None` until
    /// then (CR1; an export's GPU frames in flight follow it).
    pub fn max_inflight(&self) -> Option<usize> {
        let n = self.inflight_ceiling.load(std::sync::atomic::Ordering::Relaxed);
        (n != usize::MAX).then_some(n)
    }

    /// A render enters the device ([`GpuContext::max_inflight`]); the guard counts it out.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn enter_render(&self) -> InflightGuard {
        self.inflight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        InflightGuard(self.inflight.clone())
    }

    /// A render ran out of memory with `in_flight` renders on the device (itself included):
    /// at most half as many from now on.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn render_out_of_memory(&self, in_flight: usize) {
        let half = (in_flight / 2).max(1);
        let before = self.inflight_ceiling.fetch_min(half, std::sync::atomic::Ordering::AcqRel);
        if half < before {
            log::warn!("gpu: out of memory with {in_flight} renders in flight; at most {half} from now on");
        }
    }

    /// Whether this context has observed a readback timeout or device failure.
    /// A successful check is not a guarantee against faults after it returns.
    /// Shared-device hosts must forward their device failures through the notifier.
    pub fn check_health(&self) -> Result<(), String> {
        let result = self.readbacks.check();
        if result.is_err() {
            self.generation.retire();
            clear_retained(&self.uploads);
        }
        result
    }

    /// RT10e: submit through `light` from now on (the device is a window's, whose surface
    /// configuration must not overlap a submission). Once; `false` when a light is already set.
    pub fn set_traffic_light(&self, light: Arc<crate::TrafficLight>) -> bool {
        self.light.set(light).is_ok()
    }

    /// The device belongs to a `wgpu::Instance` of its own. `wgpu::Device` equality compares
    /// ids that are only unique within one instance, so such a device can compare equal to the
    /// window's without being it: never share textures with the window then.
    pub fn own_instance(&self) -> bool {
        self.own_instance
    }

    /// The traffic light the compositor submits through, if any.
    pub fn traffic_light(&self) -> Option<&Arc<crate::TrafficLight>> {
        self.light.get()
    }

    /// Every compositor submission: through the traffic light when there is one.
    pub(crate) fn submit(&self, commands: wgpu::CommandBuffer) -> wgpu::SubmissionIndex {
        match self.light.get() {
            Some(light) => light.pass(|| self.queue.submit([commands])),
            None => self.queue.submit([commands]),
        }
    }

    /// A submission with no work (through the traffic light): it completes once everything
    /// submitted before it has, so its index fences the work so far.
    pub fn fence(&self) -> wgpu::SubmissionIndex {
        match self.light.get() {
            Some(light) => light.pass(|| self.queue.submit([])),
            None => self.queue.submit([]),
        }
    }

    /// A notification hook for the owner of a shared device's error/loss handlers.
    /// Invoke it with diagnostic context when the host observes a device failure.
    /// It retires this context and settles pending readbacks without replacing any
    /// host handler or keeping this context/device alive. It is harmless after drop.
    ///
    /// Shared-device construction does not automatically subscribe to device faults:
    /// without host forwarding, readbacks rely on poll errors and their deadlines.
    pub fn device_failure_notifier(&self) -> impl Fn(String) + Send + Sync + 'static {
        let notify = crate::readback::failure_notifier(&self.readbacks);
        let uploads = Arc::downgrade(&self.uploads);
        let generation = self.generation.clone();
        move |message| {
            generation.retire();
            notify(message);
            if let Some(uploads) = uploads.upgrade() {
                clear_retained(&uploads);
            }
        }
    }

    /// Wait up to `timeout` for `submission` (all submitted work when `None`): `Ok(false)` while
    /// it is still running. Unlike [`Self::poll_readbacks`], running out of time does not retire
    /// the context: a deep queue of frames that keeps completing is not a hang. The readback
    /// deadlines, which restart whenever an earlier readback completes, decide that.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wait_for(&self, submission: Option<wgpu::SubmissionIndex>, timeout: std::time::Duration) -> Result<bool, String> {
        self.readbacks.check()?;
        match self.device.poll(wgpu::PollType::Wait { submission_index: submission, timeout: Some(timeout) }) {
            Ok(_) => Ok(true),
            Err(wgpu::PollError::Timeout) => Ok(false),
            Err(e) => {
                let error = format!("GPU readback poll: {e}");
                self.readbacks.retire(error.clone());
                log::error!("{error}");
                Err(error)
            }
        }
    }

    /// All device polling routes through this failure bridge.
    pub(crate) fn poll_readbacks(&self, kind: wgpu::PollType) -> Result<wgpu::PollStatus, String> {
        self.readbacks.check()?;
        self.device.poll(kind).map_err(|e| {
            let error = format!("GPU readback poll: {e}");
            self.readbacks.retire(error.clone());
            log::error!("{error}");
            error
        })
    }

    /// Start a bounded buffer mapping. Retired contexts reject new requests immediately.
    pub(crate) fn map_readback(
        &self,
        buffer: &wgpu::Buffer,
        row: usize,
        stride: usize,
        height: usize,
        done: impl FnOnce(Result<Vec<u8>, String>) + wgpu::WasmNotSend + 'static,
    ) {
        let ticket = match self.readbacks.start(done) {
            Ok(t) => t,
            Err(e) => {
                log::error!("gpu: {e}");
                return;
            }
        };
        if !ticket.active() {
            return;
        }
        let b = buffer.clone();
        buffer.map_async(wgpu::MapMode::Read, .., move |r| {
            if !ticket.active() {
                b.unmap();
                return;
            }
            let result = r.map_err(|e| format!("GPU buffer mapping: {e}")).and_then(|_| {
                let view = b.get_mapped_range(..).map_err(|e| format!("GPU mapped range: {e}"))?;
                crate::readback::packed(&view, row, stride, height)
            });
            b.unmap();
            ticket.complete(result);
        });
    }

    /// A bounded native readback: polling and receiving share one deadline.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn read_buffer(&self, buffer: &wgpu::Buffer, row: usize, stride: usize, height: usize) -> Result<Vec<u8>, String> {
        self.read_mapped(buffer, None, |view| crate::readback::packed(view, row, stride, height))
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn read_buffer(&self, _buffer: &wgpu::Buffer, _row: usize, _stride: usize, _height: usize) -> Result<Vec<u8>, String> {
        Err("synchronous GPU readback is unavailable in the browser".into())
    }

    /// Map `buffer`, wait for it (bounded: polling and receiving share one deadline) and `read`
    /// the mapped bytes in place. With `submission` (the one that wrote the buffer) only that
    /// submission is waited for, not the GPU work other render workers submitted since.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_mapped<T>(
        &self,
        buffer: &wgpu::Buffer,
        submission: Option<wgpu::SubmissionIndex>,
        read: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Result<T, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let unmap = buffer.clone();
        let ticket = self.readbacks.start(move |r| {
            // Nobody waits any more (timed out): don't leave the buffer mapped.
            if let Err(std::sync::mpsc::SendError(Ok(_))) = tx.send(r) {
                unmap.unmap();
            }
        })?;
        let watch = ticket.watch();
        if ticket.active() {
            let b = buffer.clone();
            buffer.map_async(wgpu::MapMode::Read, .., move |r| {
                if !ticket.active() {
                    b.unmap();
                    return;
                }
                match r {
                    // The waiter reads the mapped range and unmaps.
                    Ok(()) => ticket.complete(Ok(Vec::new())),
                    Err(e) => {
                        b.unmap();
                        ticket.complete(Err(format!("GPU buffer mapping: {e}")));
                    }
                }
            });
        }
        loop {
            self.readbacks.check()?;
            // The deadline restarts whenever a readback queued ahead of this one completes, so
            // waiting behind a deep queue of frames is not a timeout (`None`: settled already).
            let deadline = watch.deadline();
            let remaining = deadline.map_or(std::time::Duration::from_millis(1), |d| d.saturating_duration_since(std::time::Instant::now()));
            if remaining.is_zero() {
                let error = "GPU readback receive timed out; context retired".to_string();
                self.readbacks.retire(error.clone());
                return Err(error);
            }
            // Block until the GPU is done rather than polling and sleeping, which adds latency to
            // every readback; in slices, so the deadline above can move on.
            if deadline.is_some() {
                self.wait_for(submission.clone(), remaining.min(std::time::Duration::from_millis(250)))?;
            }
            // The callback may be running on another thread that polled the same device.
            match rx.recv_timeout(remaining.min(std::time::Duration::from_millis(1))) {
                Ok(result) => {
                    result?;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => return Err(format!("GPU readback completion channel: {e}")),
            }
        }
        let result = buffer.get_mapped_range(..).map_err(|e| format!("GPU mapped range: {e}")).and_then(|view| read(&view));
        buffer.unmap();
        result
    }

    /// Switch to deferred readbacks (see [`crate::deferred`]): readbacks are never waited for.
    pub fn set_deferred(&mut self, on: bool) {
        self.readbacks.cancel_pending("GPU readback mode changed".into());
        if let Some(d) = self.deferred.take() {
            d.cancel();
        }
        self.deferred = on.then(Default::default);
    }

    pub fn deferred(&self) -> Option<&Arc<crate::deferred::Deferred>> {
        self.deferred.as_ref()
    }

    /// A device of its own on the best adapter, without blocking (a browser worker: WebGPU
    /// only; natively any backend). `Err` says why there is none.
    pub async fn request() -> Result<GpuContext, String> {
        #[allow(unused_mut)]
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Match eframe's backend override so CLI and headless acceptance tests
            // exercise the backend requested for the native viewer.
            desc.backends = wgpu::Backends::from_env().unwrap_or(desc.backends);
            desc.memory_budget_thresholds = crate::vram::memory_budget_thresholds();
        }
        #[cfg(target_arch = "wasm32")]
        {
            desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        }
        let instance = wgpu::Instance::new(desc);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })
            .await
            .map_err(|e| format!("no adapter: {e}"))?;
        let mut context = Self::request_on_adapter(&adapter).await?;
        context.own_instance = true;
        Ok(context)
    }

    /// A device of its own on the best Vulkan, Metal or DirectX 12 adapter, when the window's
    /// adapter `window` is OpenGL or a CPU device and a better one exists (see
    /// [`crate::adapter_choice`]); `None` to stay on the window's. Ignores `WGPU_BACKEND`,
    /// which chose the window's API.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_off_window(window: &wgpu::AdapterInfo) -> Option<Result<GpuContext, String>> {
        use crate::adapter_choice::{pick_compositor_adapter, window_adapter_unfit};
        if !window_adapter_unfit(window.backend, window.device_type) {
            return None;
        }
        let backends = wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12;
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = backends;
        desc.memory_budget_thresholds = crate::vram::memory_budget_thresholds();
        let instance = wgpu::Instance::new(desc);
        let adapters = instance.enumerate_adapters(backends).await;
        let kinds: Vec<_> = adapters.iter().map(|a| a.get_info()).map(|i| (i.backend, i.device_type)).collect();
        let adapter = adapters.get(pick_compositor_adapter((window.backend, window.device_type), &kinds)?)?;
        Some(
            Self::request_on_adapter(adapter)
                .await
                .map(|mut context| {
                    context.own_instance = true;
                    context
                })
                .map_err(|e| format!("{} ({:?}): {e}", adapter.get_info().name, adapter.get_info().backend)),
        )
    }

    /// Request a separate device on an existing adapter (the desktop window's adapter).
    pub async fn request_on_adapter(adapter: &wgpu::Adapter) -> Result<GpuContext, String> {
        let limits = adapter.limits();
        let required_limits = wgpu::Limits {
            max_texture_dimension_2d: limits.max_texture_dimension_2d.min(16384),
            max_buffer_size: limits.max_buffer_size,
            ..wgpu::Limits::default()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("effectcraft gpu"),
                required_limits,
                required_features: profile_features(adapter.features()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no device: {e}"))?;
        let context = GpuContext::new(adapter, device, queue)?;
        // This path owns the device; install handlers only after construction succeeds.
        crate::readback::install(&context.device, &context.readbacks);
        Ok(context)
    }

    /// A device of its own on the best adapter (CLI, tests, benchmarks). `None` when no
    /// adapter qualifies.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn headless() -> Option<GpuContext> {
        pollster::block_on(GpuContext::request()).map_err(|e| log::info!("gpu: {e}")).ok()
    }

    /// Readbacks (GPU → CPU) are possible: waited for natively, or deferred (a browser
    /// worker, [`crate::deferred`]). Impossible on the browser's main thread.
    /// Effect kernels the device couldn't build, with why: their effects render on the CPU.
    pub fn unavailable_kernels(&self) -> &[(&'static str, String)] {
        &self.unavailable
    }

    /// Leave kernel `entry` out, as if it had failed to build (tests of the CPU fallback).
    #[cfg(test)]
    pub(crate) fn drop_kernel(&mut self, entry: &'static str) {
        self.pipelines.remove(entry);
        self.unavailable.push((entry, "dropped by a test".into()));
    }

    /// Bytes of readback staging buffers kept for reuse (tests).
    #[cfg(test)]
    pub(crate) fn staging_bytes(&self) -> u64 {
        self.staging.lock().map(|s| s.iter().map(|(k, v)| k * v.len() as u64).sum()).unwrap_or(0)
    }

    pub fn can_readback(&self) -> bool {
        self.readbacks.check().is_ok() && (self.can_wait() || self.deferred.is_some())
    }

    /// Readbacks can be waited for (natively, unless deferred). Steps that read back without a
    /// key ([`crate::deferred`]) need this.
    pub fn can_wait(&self) -> bool {
        self.readbacks.check().is_ok() && cfg!(not(target_arch = "wasm32")) && self.deferred.is_none()
    }

    pub(crate) fn fits(&self, w: u32, h: u32) -> bool {
        w > 0 && h > 0 && w <= self.max_dim && h <= self.max_dim
    }

    /// A new (zeroed) texture of its own (not pooled).
    pub(crate) fn image(&self, w: u32, h: u32) -> GpuImage {
        GpuImage::in_generation(self.texture(w, h), w, h, self.generation.clone())
    }

    fn texture(&self, w: u32, h: u32) -> wgpu::Texture {
        self.texture_format(w, h, FORMAT)
    }

    /// A new working texture in `format` (f32, or 8 bpc codes: see [`crate::codes`]).
    fn texture_format(&self, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
        let px = u64::from(format.block_copy_size(None).unwrap_or(16));
        self.transfers.count_made(u64::from(w.max(1)).saturating_mul(u64::from(h.max(1))).saturating_mul(px));
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(if format == FORMAT { "working image" } else { "working 8 bpc codes" }),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// A pooled working texture: (image, reused). A reused texture holds old pixels; a new
    /// one is zeroed.
    #[track_caller]
    fn pooled_for_encoder(&self, w: u32, h: u32, encoder: Option<u64>, cell: Option<Arc<std::sync::atomic::AtomicU64>>) -> (GpuImage, bool) {
        self.pooled_format(w, h, FORMAT, encoder, cell)
    }

    /// [`GpuContext::pooled_for_encoder`] in `format`. Only [`crate::codes`] asks for another
    /// format than [`FORMAT`] and wraps the image so no float kernel can bind it.
    #[track_caller]
    pub(crate) fn pooled_format(
        &self,
        w: u32,
        h: u32,
        format: wgpu::TextureFormat,
        encoder: Option<u64>,
        cell: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> (GpuImage, bool) {
        let made_for = encoder;
        #[cfg(feature = "profile")]
        let encoder = {
            static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *ENABLED.get_or_init(|| std::env::var("EFFECTCRAFT_ENCODER_REUSE").as_deref() != Ok("0")) { encoder } else { None }
        };

        let (w, h) = (w.max(1), h.max(1));
        let reused = self.pool.lock().ok().and_then(|mut p| p.take_format(w, h, format, encoder));
        let was_reused = reused.is_some();
        #[cfg(feature = "profile")]
        if !was_reused {
            let at = std::panic::Location::caller();
            let site = format!("{}:{}", at.file().rsplit('/').next().unwrap_or(at.file()), at.line());
            *MISS_SITES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry((site, w, h)).or_default() += 1;
        }
        let pooled = reused.unwrap_or_else(|| Pooled::from(self.texture_format(w, h, format)));
        let (texture, view) = (pooled.texture.clone(), pooled.view.clone());
        let site = std::panic::Location::caller();
        self.leased.add(pooled.bytes(), site);
        let lease =
            Arc::new(Lease { pooled, pool: Arc::downgrade(&self.pool), encoder: made_for, meter: self.leased.clone(), site, cell, shared: Default::default() });
        let allocation = if format == FORMAT {
            effectcraft_render::allocation::GpuAllocation::rgba_f32(w, h, self.generation.clone())
        } else {
            Some(effectcraft_render::allocation::GpuAllocation::of_bytes(tex_bytes(&texture), self.generation.clone()))
        };
        (GpuImage { allocation, texture, width: w, height: h, view, _lease: Some(lease), levels: None }, was_reused)
    }

    /// Uploads and readbacks so far (images; uploads of cached layers count once).
    pub fn transfer_stats(&self) -> TransferStats {
        use std::sync::atomic::Ordering::Relaxed;
        let t = &self.transfers;
        TransferStats {
            uploads: t.ups.load(Relaxed),
            upload_bytes: t.up_bytes.load(Relaxed),
            readbacks: t.downs.load(Relaxed),
            readback_bytes: t.down_bytes.load(Relaxed),
            yuv_conversions: t.yuv_conversions.load(Relaxed),
            yuv_bytes: t.yuv_bytes.load(Relaxed),
        }
    }

    /// GPU allocations so far (see [`AllocStats`]).
    pub fn alloc_stats(&self) -> AllocStats {
        use std::sync::atomic::Ordering::Relaxed;
        AllocStats { allocations: self.transfers.made.load(Relaxed), bytes: self.transfers.made_bytes.load(Relaxed) }
    }

    /// Texture pool counters: (working textures reused, newly allocated).
    pub fn pool_stats(&self) -> (u64, u64) {
        self.pool.lock().map(|p| (p.hits, p.misses)).unwrap_or_default()
    }

    /// Where the device's memory is now (diagnostics: `bench --rt-profile`). `allocator`
    /// also asks the device allocator for every live allocation (slower: it walks them all).
    pub fn vram_report(&self, allocator: bool) -> VramReport {
        use std::sync::atomic::Ordering::Relaxed;
        let mut r = VramReport { leased: self.leased.live.load(Relaxed), leased_peak: self.leased.peak.load(Relaxed), ..Default::default() };
        if let Ok(p) = self.leased.published.lock() {
            r.published = p.iter().map(|(k, (n, b))| ((*k).to_owned(), *n, *b)).collect();
        }
        if let Ok(p) = self.pool.lock() {
            r.pool_free = p.bytes;
            r.pool_released = p.released_bytes;
        }
        r.uploads = self.upload_stats().1;
        r.zeros = self.zeros.lock().map(|z| z.values().map(|i| u64::from(i.width).saturating_mul(u64::from(i.height)).saturating_mul(16)).sum()).unwrap_or(0);
        r.planes = self
            .planes
            .lock()
            .map(|p| {
                p.values()
                    .flatten()
                    .map(|t| {
                        u64::from(t.width()).saturating_mul(u64::from(t.height())).saturating_mul(u64::from(t.format().block_copy_size(None).unwrap_or(16)))
                    })
                    .sum()
            })
            .unwrap_or(0);
        r.staging = self.staging.lock().map(|s| s.iter().map(|(k, v)| k.saturating_mul(v.len() as u64)).sum()).unwrap_or(0);
        if allocator && let Some(report) = self.device.generate_allocator_report() {
            r.reserved = Some(report.total_reserved_bytes);
            r.allocated = Some(report.total_allocated_bytes);
            let mut by: HashMap<String, (u64, usize)> = HashMap::new();
            for a in &report.allocations {
                let e = by.entry(a.name.clone()).or_default();
                e.0 = e.0.saturating_add(a.size);
                e.1 += 1;
            }
            r.by_label = by.into_iter().map(|(k, (b, n))| (k, b, n)).collect();
            r.by_label.sort_by_key(|e| std::cmp::Reverse(e.1));
        }
        #[cfg(feature = "profile")]
        if let Ok(s) = self.leased.sites.lock() {
            let mut v: Vec<_> = s
                .at_peak
                .iter()
                .filter(|(_, b)| **b > 0)
                .map(|(at, b)| (format!("{}:{}", at.file().rsplit('/').next().unwrap_or(at.file()), at.line()), *b))
                .collect();
            v.sort_by_key(|e| std::cmp::Reverse(e.1));
            r.peak_sites = v;
        }
        r
    }

    /// Start a new high-water mark for [`VramReport::leased_peak`] (from what is handed out now).
    pub fn reset_vram_peak(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.leased.peak.store(self.leased.live.load(Relaxed), Relaxed);
        #[cfg(feature = "profile")]
        if let Ok(mut s) = self.leased.sites.lock() {
            s.at_peak = s.live.clone();
        }
    }

    /// A `COPY_DST | TEXTURE_BINDING` texture for uploaded samples: a free one of this size and
    /// format when there is one (its old samples are overwritten by the upload), else a new one.
    /// Hand it to [`Enc::stage`] once a dispatch reads it, so it is reused only after that
    /// dispatch is submitted (an earlier `write_texture` would otherwise overwrite it first).
    pub(crate) fn plane_texture(&self, w: u32, h: u32, format: wgpu::TextureFormat, label: &'static str) -> wgpu::Texture {
        let key = (w.max(1), h.max(1), format);
        if let Some(t) = self.planes.lock().ok().and_then(|mut p| p.get_mut(&key).and_then(Vec::pop)) {
            return t;
        }
        let bpp = format.block_copy_size(None).unwrap_or(16);
        self.transfers.count_made(u64::from(key.0).saturating_mul(u64::from(key.1)).saturating_mul(u64::from(bpp)));
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: key.0, height: key.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// Plane textures whose every reader has been submitted (or discarded) are free again.
    fn free_planes(&self, textures: Vec<wgpu::Texture>) {
        if textures.is_empty() || self.check_health().is_err() {
            return;
        }
        let bytes = |t: &wgpu::Texture| {
            (t.width() as usize).saturating_mul(t.height() as usize).saturating_mul(t.format().block_copy_size(None).unwrap_or(16) as usize)
        };
        let Ok(mut p) = self.planes.lock() else { return };
        let mut held = p.values().flatten().map(bytes).fold(0usize, usize::saturating_add);
        for t in textures {
            let b = bytes(&t);
            if held.saturating_add(b) > PLANE_BUDGET {
                continue;
            }
            held = held.saturating_add(b);
            p.entry((t.width(), t.height(), t.format())).or_default().push(t);
        }
    }

    /// Upload a CPU image (`None` when it does not fit the device). An image on the 8 or 16 bpc
    /// grid (every layer buffer of an 8/16 bpc project) goes up as its integer codes, a quarter
    /// or half of the bytes, and is expanded on the GPU to the identical floats.
    pub fn upload_image(&self, img: &Image) -> Option<GpuImage> {
        self.upload_image_keeping(img, false).map(|(g, _)| g)
    }

    /// [`GpuContext::upload_image`], also handing back the 8 bpc codes it uploaded when `keep`
    /// (to retain as a [`PackedImage`]).
    fn upload_image_keeping(&self, img: &Image, keep: bool) -> Option<(GpuImage, Option<wgpu::Texture>)> {
        let _profile = effectcraft_render::profile::span("upload");
        if !self.fits(img.width, img.height) {
            return None;
        }
        // A free pooled texture: every encoder that could read its old pixels is submitted.
        let (mut g, _) = self.pooled_for_encoder(img.width, img.height, None, None);
        if let Some((levels, kept)) = self.upload_packed(img, &g, keep) {
            // Every value is on that grid (clamped and quantised), so quantising is a no-op.
            g.levels = Some(levels);
            return Some((g, kept));
        }
        self.transfers.count(true, img.data.len() * 16);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &g.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bytemuck::cast_slice(&img.data),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(img.width * 16), rows_per_image: Some(img.height) },
            wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
        );
        Some((g, None))
    }

    /// [`GpuContext::upload_image`] of an image on the 8 or 16 bpc grid into `out`: the grid's
    /// levels (`None`: off the grid, or no kernel; nothing was written).
    /// `keep`: also hand back the 8 bpc codes texture (see [`GpuContext::upload_codes`]).
    fn upload_packed(&self, img: &Image, out: &GpuImage, keep: bool) -> Option<(f32, Option<wgpu::Texture>)> {
        if self.yuv.is_none() || img.width == 0 || img.height == 0 {
            return None;
        }
        if let Some(codes) = pack_codes(&img.data, LEVELS_8, |k| k as u8) {
            return self.upload_codes(out, wgpu::TextureFormat::Rgba8Uint, bytemuck::cast_slice(&codes), 4, LEVELS_8, keep).map(|kept| (LEVELS_8, kept));
        }
        if cfg!(target_endian = "little")
            && let Some(codes) = pack_codes(&img.data, LEVELS_16, |k| k as u16)
        {
            return self.upload_codes(out, wgpu::TextureFormat::Rgba16Uint, bytemuck::cast_slice(&codes), 8, LEVELS_16, false).map(|_| (LEVELS_16, None));
        }
        None
    }

    /// Upload a CPU image for the compositor walk: its codes when it is on the 8 bpc grid (and
    /// the codes kernels exist), else as [`GpuContext::upload_image`].
    pub(crate) fn upload_image_px(&self, img: &Image) -> Option<crate::codes::Pixels> {
        if let Some(c) = self.upload_codes8(img) {
            return Some(crate::codes::Pixels::Codes(c));
        }
        self.upload_image(img).map(crate::codes::Pixels::Float)
    }

    /// A CPU image on the 8 bpc grid uploaded as its codes into a pooled codes image (no float
    /// copy: [`crate::codes`]); `None` off the grid, without the codes bank, or not fitting.
    fn upload_codes8(&self, img: &Image) -> Option<crate::codes::CodeImage> {
        self.codes.as_ref()?;
        if !self.fits(img.width, img.height) {
            return None;
        }
        let _profile = effectcraft_render::profile::span("upload");
        let codes = pack_codes(&img.data, LEVELS_8, |k| k as u8)?;
        let (g, _) = self.pooled_format(img.width, img.height, crate::codes::FORMAT, None, None);
        let row = img.width.checked_mul(4)?;
        let bytes: &[u8] = bytemuck::cast_slice(&codes);
        if (row as usize).checked_mul(img.height as usize) != Some(bytes.len()) {
            return None;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &g.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(img.height) },
            wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
        );
        self.transfers.count(true, bytes.len());
        crate::codes::CodeImage::wrap(g)
    }

    /// The uniform `unpack.wgsl` reads for the 8 bpc grid (made once).
    pub(crate) fn unpack8_scale(&self) -> &wgpu::Buffer {
        self.unpack8.get_or_init(|| {
            let scale = [(1.0 / LEVELS_8).to_bits(), 0, 0, 0];
            self.buffer_init(Some("unpack scale"), bytemuck::cast_slice(&scale), wgpu::BufferUsages::UNIFORM)
        })
    }

    /// Upload integer codes (`bpp` bytes per pixel, `out`'s size) and expand them into `out`:
    /// each channel `k` becomes `k as f32 * (1.0 / levels)`, rounded as on the CPU.
    /// `keep`: hand back the codes texture (retained as a [`PackedImage`]) instead of reusing it.
    /// `None`: nothing was written.
    fn upload_codes(&self, out: &GpuImage, format: wgpu::TextureFormat, bytes: &[u8], bpp: u32, levels: f32, keep: bool) -> Option<Option<wgpu::Texture>> {
        let p = self.yuv.as_ref()?;
        let (w, h) = (out.width, out.height);
        let row = w.checked_mul(bpp)?;
        if (row as usize).checked_mul(h as usize) != Some(bytes.len()) {
            return None;
        }
        let texture = self.plane_texture(w, h, format, "packed upload");
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.transfers.count(true, bytes.len());
        let scale = [(1.0 / levels).to_bits(), 0, 0, 0];
        let uniform = self.buffer_init(Some("unpack scale"), bytemuck::cast_slice(&scale), wgpu::BufferUsages::UNIFORM);
        let view = texture.create_view(&Default::default());
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("packed upload"),
            layout: &p.unpack_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&out.view) },
                wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 9, resource: uniform.as_entire_binding() },
            ],
        });
        // Submitted now: `out` is written before any later submission reads it.
        let mut e = Enc::new(self);
        e.custom_pixels("upload_unpack", &p.unpack, &bg, (w, h));
        if keep {
            e.submit();
            return Some(Some(texture));
        }
        e.stage(texture);
        e.submit();
        Some(None)
    }

    /// Upload a (layer cache) buffer, reusing the texture while the same `Arc` is alive: static
    /// layers upload once, not every frame.
    fn upload_epoch(&self) -> Weak<()> {
        self.uploads.lock().map(|u| Arc::downgrade(&u.epoch)).unwrap_or_default()
    }
    fn prune_uploads(&self) {
        loop {
            let stale = self.uploads.lock().ok().and_then(|mut u| u.stale());
            let Some(stale) = stale else { break };
            stale.release();
        }
    }
    /// Set an additional upload ceiling while retaining the owner's physical-memory cap.
    pub fn set_upload_budget(&self, bytes: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.upload_limit.store(bytes.clamp(1 << 20, UPLOAD_BUDGET), Relaxed);
        self.set_cache_budget(self.layer_budget.load(Relaxed));
    }
    pub fn upload_budget(&self) -> usize {
        self.retained_cap()
    }
    fn warn_upload_full(&self) {
        if !self.upload_full_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::warn!(target: "effectcraft_gpu", "GPU retained upload cache is full ({} MiB allowance); unreused layer textures upload again when needed", self.retained_cap() >> 20);
        }
    }
    pub(crate) fn retained_cap(&self) -> usize {
        self.uploads.lock().map(|u| u.budget.unwrap_or(UPLOAD_BUDGET)).unwrap_or(0)
    }
    /// The texture of a layer buffer, uploaded once while the same `Arc` is alive. An 8 bpc
    /// buffer is retained as its codes ([`Uploaded::Packed`]: the caller expands it).
    fn upload_buf_owned(&self, buf: &Arc<Buf>, owner: Option<&GpuOwner>) -> Option<Uploaded> {
        let epoch = self.upload_epoch();
        self.prune_uploads();
        if let Some(owner) = owner {
            let key = (owner.identity(), Arc::as_ptr(buf) as usize);
            let entry = self.uploads.lock().ok().and_then(|u| u.map.get(&key).cloned());
            if let Some(entry) = entry
                && entry.buf.ptr_eq(&Arc::downgrade(buf))
            {
                if let Some(img) = entry.slot.get(&self.generation, GpuStage::CpuMirror) {
                    return Some(Uploaded::Image(img));
                }
                if let Some(packed) = entry.slot.get::<PackedImage>(&self.generation, GpuStage::CpuMirror) {
                    return Some(Uploaded::Packed(packed));
                }
            }
        }
        // 8 bpc layers stay codes: the compositor samples them as they are (see `crate::codes`).
        if let Some(codes) = self.upload_codes8(&buf.img) {
            let packed = PackedImage { codes, offset: buf.offset, scale: buf.scale };
            if let Some(owner) = owner
                && packed_retention()
                && let Some(allocation) = packed.codes.allocation().cloned()
            {
                packed.codes.share();
                self.leased.publish_bytes("upload packed", packed.codes.bytes() as u64);
                self.register_payload(buf, Arc::new(packed.clone()), allocation, owner, &epoch);
            }
            return Some(Uploaded::Packed(packed));
        }
        let keep = owner.is_some() && packed_retention();
        let (img, codes) = self.upload_image_keeping(&buf.img, keep)?;
        if let Some(owner) = owner {
            match codes.and_then(|c| code_image(c, &self.generation)) {
                Some(codes) => {
                    let packed = PackedImage { codes, offset: buf.offset, scale: buf.scale };
                    let bytes = packed.codes.bytes();
                    self.leased.publish_bytes("upload packed", bytes as u64);
                    let allocation = effectcraft_render::allocation::GpuAllocation::of_bytes(bytes, self.generation.clone());
                    self.register_payload(buf, Arc::new(packed), allocation, owner, &epoch);
                }
                None => self.register(buf, img.clone(), owner, &epoch, "upload"),
            }
        }
        Some(Uploaded::Image(img))
    }
    fn register(&self, buf: &Arc<Buf>, img: GpuImage, owner: &GpuOwner, epoch: &Weak<()>, kind: &'static str) {
        let Some(allocation) = img.allocation.clone() else { return };
        if !allocation.generation().same(&self.generation) {
            return;
        }
        self.mips.seal(&img);
        img.share();
        self.leased.publish(kind, &img);
        self.register_payload(buf, Arc::new(img), allocation, owner, epoch);
    }
    fn register_payload<T: std::any::Any + Send + Sync>(
        &self,
        buf: &Arc<Buf>,
        payload: Arc<T>,
        allocation: Arc<effectcraft_render::allocation::GpuAllocation>,
        owner: &GpuOwner,
        epoch: &Weak<()>,
    ) {
        let pending = match owner.prepare(payload, allocation, GpuStage::CpuMirror, self.retained_cap()) {
            Ok(pending) => pending,
            Err(effectcraft_render::retained::AdmissionError::Capacity) => {
                self.warn_upload_full();
                return;
            }
            Err(_) => return,
        };
        let Ok(slot) = pending.publish() else { return };
        let entry = SlotRef { owner: owner.clone(), slot, epoch: epoch.clone() };
        let key = (owner.identity(), Arc::as_ptr(buf) as usize);
        let old = {
            let mut u = match self.uploads.lock() {
                Ok(u) => u,
                Err(poison) => {
                    drop(poison);
                    entry.release();
                    return;
                }
            };
            if !self.generation.valid()
                || !self.generation.within_cap()
                || epoch.strong_count() == 0
                || u.map.len() >= effectcraft_render::retained::MAX_RECORDS
                || u.map.try_reserve(1).is_err()
            {
                drop(u);
                entry.release();
                return;
            }
            u.map.insert(key, Upload { buf: Arc::downgrade(buf), slot: entry }).map(|e| e.slot)
        };
        if let Some(old) = old {
            old.release();
        }
    }
    fn keep_owned(&self, img: GpuImage, owner: Option<&GpuOwner>, epoch: &Weak<()>) -> Option<u64> {
        let owner = owner?;
        self.prune_uploads();
        if self.uploads.lock().ok()?.keep_off {
            return None;
        }
        let allocation = img.allocation.clone()?;
        if !allocation.generation().same(&self.generation) {
            return None;
        }
        img.share();
        self.leased.publish("kept", &img);
        let slot = owner.prepare(Arc::new(img), allocation, GpuStage::Processed, self.retained_cap()).ok()?.publish().ok()?;
        let entry = SlotRef { owner: owner.clone(), slot, epoch: epoch.clone() };
        let result = {
            let mut u = match self.uploads.lock() {
                Ok(u) => u,
                Err(poison) => {
                    drop(poison);
                    entry.release();
                    return None;
                }
            };
            if !self.generation.valid()
                || !self.generation.within_cap()
                || epoch.strong_count() == 0
                || u.keep_off
                || u.kept.len() >= MAX_KEPT
                || u.kept.try_reserve(1).is_err()
            {
                drop(u);
                entry.release();
                return None;
            }
            let Some(ticket) = u.next_ticket.checked_add(1) else {
                drop(u);
                entry.release();
                return None;
            };
            u.next_ticket = ticket;
            u.kept.push((ticket, entry));
            ticket
        };
        Some(result)
    }
    pub(crate) fn adopt(&self, ticket: u64, buf: &Arc<Buf>) {
        let Some(entry) = self.take_kept(ticket) else { return };
        if let Some(img) = entry.get::<GpuImage>(&self.generation, GpuStage::Processed)
            && (img.width, img.height) == (buf.img.width, buf.img.height)
            && !buf.img.is_empty()
        {
            self.register(buf, img, &entry.owner, &entry.epoch, "adopt");
        }
        entry.release();
    }
    pub(crate) fn release(&self, ticket: u64) {
        if let Some(entry) = self.take_kept(ticket) {
            entry.release();
        }
    }
    fn take_kept(&self, ticket: u64) -> Option<SlotRef> {
        let mut u = self.uploads.lock().ok()?;
        let i = u.kept.iter().position(|(t, _)| *t == ticket)?;
        Some(u.kept.remove(i).1)
    }
    #[cfg(test)]
    pub(crate) fn kept_count(&self) -> usize {
        self.prune_uploads();
        self.uploads.lock().map(|u| u.kept.len()).unwrap_or_default()
    }
    #[cfg(test)]
    pub(crate) fn set_keep_effects(&self, on: bool) {
        let old = if let Ok(mut u) = self.uploads.lock() {
            u.keep_off = !on;
            std::mem::take(&mut u.kept)
        } else {
            Vec::new()
        };
        for (_, entry) in old {
            entry.release();
        }
    }

    /// The machine's physical RAM in bytes (0: unknown), which an integrated GPU shares: with
    /// enough of it the working-texture pool may grow (see [`GpuContext::set_cache_budget`]).
    /// Cheap when unchanged.
    pub fn set_system_memory(&self, total: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.system_memory.swap(total, Relaxed) != total {
            self.set_cache_budget(self.layer_budget.load(Relaxed));
        }
    }

    /// Size the GPU-side caches after the layer cache's memory budget (Memory & CPU settings,
    /// after RAM Reserved for Other Applications): uploaded layer textures mirror cached layer
    /// buffers while Processed entries remain GPU-only until CPU demand. Both allocations
    /// of a CPU mirror count against the owner's shared physical budget, as do pending
    /// GPU outputs. Retention follows that budget (capped at [`UPLOAD_BUDGET`]); free working
    /// textures get half (capped at [`POOL_BUDGET`]; on unified memory 128 MiB, more with
    /// 16 GiB of RAM or more, see [`GpuContext::set_system_memory`]).
    /// Auxiliary staging and zero-image caches each get
    /// 1/16 of the layer budget, capped at 32 MiB integrated or 64 MiB discrete.
    pub fn set_cache_budget(&self, layer_cache: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.layer_budget.store(layer_cache, Relaxed);
        let memory = if self.integrated { self.system_memory.load(Relaxed) } else { self.vram.map_or(0, |m| m.total) };
        let (upload, pool, auxiliary) = cache_budgets(layer_cache, self.integrated, memory);
        let limit = self.upload_limit.load(Relaxed);
        let upload = if limit == 0 { upload } else { upload.min(limit) };
        let pool = pool_budget_override().unwrap_or(pool);
        self.auxiliary_budget.store(auxiliary, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut u) = self.uploads.lock() {
            u.budget = Some(upload);
            self.generation.set_cap(upload);
        }
        let owners = self
            .uploads
            .lock()
            .map(|u| {
                u.map
                    .values()
                    .map(|e| e.slot.owner.clone())
                    .chain(u.native.values().map(|e| e.slot.owner.clone()))
                    .chain(u.kept.iter().map(|(_, e)| e.owner.clone()))
                    .chain(u.resident.iter().map(|e| e.owner.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for owner in owners {
            owner.enforce_device_cap(&self.generation, upload);
        }
        self.prune_uploads();
        if let Ok(mut p) = self.pool.lock() {
            p.set_budget(pool);
        }
        if let Ok(mut s) = self.staging.lock() {
            let retained = s.iter().map(|(size, buffers)| size.saturating_mul(buffers.len() as u64)).sum::<u64>();
            if retained > auxiliary {
                s.clear();
            }
        }
        if let Ok(mut z) = self.zeros.lock() {
            let retained = z.values().map(|img| u64::from(img.width).saturating_mul(u64::from(img.height)).saturating_mul(16)).sum::<u64>();
            if retained > auxiliary {
                z.clear();
            }
        }
    }

    pub(crate) fn native_get(
        &self,
        owner: &effectcraft_render::footage::SharedFrame,
        key: u64,
        token: &effectcraft_render::cache::ResidentToken,
        metadata: &[u64],
        cache: Option<&GpuOwner>,
    ) -> Option<NativeImage> {
        let cache = cache?;
        let _profile = effectcraft_render::profile::span("native_cache_lookup");
        if !token.valid() || token.identity() != cache.identity() || self.check_health().is_err() {
            return None;
        }
        self.prune_uploads();
        let k = (Arc::as_ptr(owner) as *const () as usize, token.identity(), key);
        let entry = self.uploads.lock().ok()?.native.get(&k).cloned()?;
        if entry.metadata != metadata || !entry.owner.ptr_eq(&Arc::downgrade(owner)) {
            return None;
        }
        entry.slot.get(&self.generation, GpuStage::Blend)
    }
    fn native_keep(&self, entry: PendingNative) {
        if !entry.token.valid() || entry.epoch.strong_count() == 0 || self.check_health().is_err() {
            return;
        }
        let Ok(slot) = entry.pending.publish() else { return };
        let retained = SlotRef { owner: entry.cache, slot, epoch: entry.epoch.clone() };
        let k = (Arc::as_ptr(&entry.owner) as *const () as usize, entry.token.identity(), entry.key);
        let old = {
            let mut u = match self.uploads.lock() {
                Ok(u) => u,
                Err(poison) => {
                    drop(poison);
                    retained.release();
                    return;
                }
            };
            if !self.generation.valid()
                || !self.generation.within_cap()
                || entry.epoch.strong_count() == 0
                || u.native.len() >= 1024
                || u.native.try_reserve(1).is_err()
            {
                drop(u);
                retained.release();
                return;
            }
            u.native
                .insert(k, NativeUpload { owner: Arc::downgrade(&entry.owner), token: entry.token, metadata: entry.metadata, slot: retained })
                .map(|e| e.slot)
        };
        if let Some(old) = old {
            old.release();
        }
    }
    /// Live indexed uploads. The authoritative physical accounting is the layer owner.
    pub fn upload_stats(&self) -> (usize, usize) {
        self.prune_uploads();
        let entries = self
            .uploads
            .lock()
            .map(|u| u.map.values().map(|e| e.slot.clone()).chain(u.native.values().map(|e| e.slot.clone())).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut identities = BTreeSet::new();
        let mut bytes = 0usize;
        let mut count = 0usize;
        for entry in entries {
            let slot = entry.slot.upgrade();
            if let Some(slot) = slot {
                count += 1;
                if identities.insert(Arc::as_ptr(slot.allocation()) as usize) {
                    bytes = bytes.saturating_add(slot.allocation().bytes());
                }
            }
        }
        (count, bytes)
    }
    /// Healthy clear invalidates pending native publication without retiring the device.
    pub fn clear_uploads(&self) {
        clear_retained(&self.uploads);
        self.mips.clear();
        if let Ok(mut p) = self.pool.lock() {
            p.clear();
        }
        let old = self.staging.lock().ok().map(|mut s| std::mem::take(&mut *s));
        drop(old);
        let old = self.planes.lock().ok().map(|mut s| std::mem::take(&mut *s));
        drop(old);
        let old = self.zeros.lock().ok().map(|mut z| std::mem::take(&mut *z));
        drop(old);
    }
}

impl Drop for GpuContext {
    fn drop(&mut self) {
        self.generation.retire();
        clear_retained(&self.uploads);
        self.mips.clear();
    }
}

fn profile_features(available: wgpu::Features) -> wgpu::Features {
    if cfg!(feature = "profile") {
        available & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    } else {
        wgpu::Features::empty()
    }
}

/// Records GPU work for one render (a command encoder submitted at readback or the end).
struct PendingNative {
    owner: effectcraft_render::footage::SharedFrame,
    key: u64,
    token: effectcraft_render::cache::ResidentToken,
    metadata: Vec<u64>,
    cache: GpuOwner,
    pending: GpuPending,
    epoch: Weak<()>,
}

struct LazyReadback(Weak<GpuContext>);
impl effectcraft_render::cache::ResidentReadback for LazyReadback {
    fn readback(&self, slot: &GpuSlot) -> Option<Buf> {
        let ctx = self.0.upgrade()?;
        if slot.stage() != GpuStage::Processed || !slot.allocation().generation().same(&ctx.generation) {
            return None;
        }
        let _profile = effectcraft_render::profile::span("processed_readback");
        if let Some(packed) = slot.payload::<PackedImage>() {
            // The codes back as the floats `unpack` gives (and the CPU's packing reads as).
            return crate::Gpu::within_context(&ctx, || {
                let img = Enc::new(&ctx).download_codes(&packed.codes)?;
                Some(Buf { img, offset: packed.offset, scale: packed.scale })
            })
            .ok()
            .flatten();
        }
        let image = slot.payload::<NativeImage>()?;
        crate::Gpu::within_context(&ctx, || {
            let mut enc = Enc::new(&ctx);
            Some(Buf { img: enc.download(&image.img)?, offset: image.offset, scale: image.scale })
        })
        .ok()
        .flatten()
    }
}

pub(crate) struct Enc<'g> {
    epoch: Weak<()>,
    pub(crate) owner: Option<GpuOwner>,
    native_pending: Vec<PendingNative>,
    resident_pending: Vec<(GpuPending, effectcraft_render::cache::FlightGuard)>,
    readback: Option<Arc<dyn effectcraft_render::cache::ResidentReadback>>,
    #[cfg(feature = "profile")]
    pub(crate) timestamps: Option<crate::timestamps::Batch>,
    pub g: &'g GpuContext,
    /// The open command encoder and its id in the texture pool.
    enc: Option<(wgpu::CommandEncoder, u64)>,
    pending: usize,
    /// The latest submission (readbacks wait for it, not for every worker's GPU work).
    submitted: Option<wgpu::SubmissionIndex>,
    /// This encoder's uniform ring (see [`Enc::dispatch_ext`]).
    ring: Option<wgpu::Buffer>,
    /// The compute pass consecutive dispatches record into, and whether later dispatches may
    /// join it (see [`Enc::compute_pass`]).
    pass: Option<(wgpu::ComputePass<'static>, bool)>,
    /// Batch dispatches into one compute pass and reuse minification chains (tests compare
    /// against one pass per dispatch and chains rebuilt every time).
    pub(crate) reuse: bool,
    /// Minification levels this encoder built (see [`crate::mips`]).
    pub(crate) mips: crate::mips::Local,
    /// A dispatch named a kernel the device doesn't have (see [`Enc::take_missing`]).
    missing: bool,
    /// Nested comp frames shared by identical precomp instances of this frame (`cull`).
    pub(crate) shared: crate::cull::Shared,
    /// Upload textures read by recorded dispatches, free again once submitted ([`Enc::stage`]).
    planes: Vec<wgpu::Texture>,
    /// Pool ids of the command encoders this `Enc` submitted (newest last, at most
    /// [`SUBMITTED_KEPT`]): their images may still be drawn on in place ([`Enc::owns`]).
    submitted_ids: Vec<u64>,
    /// When the open command encoder recorded its first command (see [`Enc::submit_early`]).
    opened: Option<std::time::Instant>,
    /// The pool id of the open command encoder (0: none), shared with the leases of the
    /// working images this `Enc` takes (see [`Pool::release`]).
    cell: Arc<std::sync::atomic::AtomicU64>,
}

/// Submitted encoder ids an [`Enc`] remembers for [`Enc::owns`] (older ones lose in-place).
const SUBMITTED_KEPT: usize = 64;

/// Dispatches after which an open command encoder is submitted so the GPU starts on them while
/// the walk records the rest (RT10c), and the shorter count for an encoder open for longer than
/// [`SUBMIT_AFTER`].
const SUBMIT_EVERY: usize = 64;
const SUBMIT_EVERY_SLOW: usize = 8;
const SUBMIT_AFTER: std::time::Duration = std::time::Duration::from_millis(4);

/// The early-submit cadence: (submit early at all, also by age). Profiling builds:
/// `EFFECTCRAFT_RT_SUBMIT=off` keeps the old one submit per frame (or 256 dispatches) and
/// `=count` submits by dispatch count only, for A/B measurement.
fn early_submit() -> (bool, bool) {
    if !cfg!(feature = "profile") {
        return (true, true);
    }
    static ON: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("EFFECTCRAFT_RT_SUBMIT").as_deref() {
        Ok("off") => (false, false),
        Ok("count") => (true, false),
        _ => (true, true),
    })
}

impl Drop for Enc<'_> {
    fn drop(&mut self) {
        // End the pass before the encoder it records into goes.
        self.pass = None;
        // Unsubmitted work is discarded, so nothing recorded will read pooled textures.
        if let Some((_, id)) = self.enc.take()
            && let Ok(mut p) = self.g.pool.lock()
        {
            self.cell.store(0, std::sync::atomic::Ordering::Release);
            p.close(id);
        }
        self.g.free_planes(std::mem::take(&mut self.planes));
        if let Some(ring) = self.ring.take()
            && let Ok(mut u) = self.g.uniforms.lock()
            && u.len() < RINGS_KEPT
        {
            u.push(ring);
        }
    }
}

impl<'g> Enc<'g> {
    pub fn new(g: &'g GpuContext) -> Enc<'g> {
        Self::with_owner(g, None)
    }
    pub fn with_resident(g: &'g Arc<GpuContext>, owner: Option<GpuOwner>) -> Enc<'g> {
        let mut enc = Self::with_owner(g, owner);
        enc.readback = Some(Arc::new(LazyReadback(Arc::downgrade(g))));
        enc
    }
    pub(crate) fn can_retain_resident(&self) -> bool {
        self.readback.is_some() && self.owner.as_ref().is_some_and(GpuOwner::valid) && self.g.deferred.is_none()
    }
    /// A retained image: f32, or the codes of a packed one (sampled as they are where the
    /// codes kernels exist, else expanded into a working image of this encoder).
    pub(crate) fn resident(&mut self, key: u64, stage: GpuStage) -> Option<SourceImage> {
        let slot = self.owner.as_ref()?.resident_in(key, stage, Some(&self.g.generation))?;
        if let Some(image) = slot.payload::<NativeImage>() {
            return Some(image.clone().into());
        }
        let packed = slot.payload::<PackedImage>()?.clone();
        drop(slot);
        if self.g.codes.is_some() {
            return Some(packed.source());
        }
        self.unpack(&packed).map(SourceImage::from)
    }

    /// Whether [`Enc::resident`] has an image for `key` (without expanding a packed one).
    pub(crate) fn has_resident(&self, key: u64, stage: GpuStage) -> bool {
        self.owner
            .as_ref()
            .and_then(|o| o.resident_in(key, stage, Some(&self.g.generation)))
            .is_some_and(|slot| slot.payload::<NativeImage>().is_some() || slot.payload::<PackedImage>().is_some())
    }

    /// `image` as codes, when it is on the 8 bpc grid and the device can pack (see
    /// [`PackedImage`]); recorded on this encoder, after the commands that wrote it.
    fn pack(&mut self, image: &NativeImage) -> Option<PackedImage> {
        if image.img.levels != Some(LEVELS_8) || !packed_retention() {
            return None;
        }
        let (pipe, layout) = self.g.yuv.as_ref()?.pack.as_ref()?;
        let (w, h) = (image.img.width, image.img.height);
        self.g.transfers.count_made(u64::from(w).saturating_mul(u64::from(h)).saturating_mul(4));
        let codes = self.g.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("retained 8 bpc codes"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Uint,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = codes.create_view(&Default::default());
        let bg = self.g.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pack"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&image.img.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
            ],
        });
        let pipe = pipe.clone();
        self.custom_pixels("pack8", &pipe, &bg, (w, h));
        if self.take_missing() {
            return None;
        }
        drop(view);
        Some(PackedImage { codes: code_image(codes, &self.g.generation)?, offset: image.offset, scale: image.scale })
    }

    /// Codes decoded into a working image of this encoder: the floats `unpack.wgsl` gives (the
    /// CPU's `k as f32 * (1.0 / 255.0)`), recorded after the commands that wrote the codes.
    fn unpack_codes(&mut self, c: &crate::codes::CodeImage) -> Option<GpuImage> {
        let yuv = self.g.yuv.as_ref()?;
        let mut out = self.scratch(c.width(), c.height());
        let bg = self.g.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("decode 8 bpc codes"),
            layout: &yuv.unpack_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&out.view) },
                wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(c.view()) },
                wgpu::BindGroupEntry { binding: 9, resource: self.g.unpack8_scale().as_entire_binding() },
            ],
        });
        let pipe = yuv.unpack.clone();
        self.custom_pixels("unpack_retained", &pipe, &bg, (c.width(), c.height()));
        out.levels = Some(LEVELS_8);
        Some(out)
    }

    /// A [`PackedImage`] expanded into a working image of this encoder (the identical floats).
    fn unpack(&mut self, p: &PackedImage) -> Option<NativeImage> {
        let img = match self.g.codes {
            Some(_) => crate::codes::decode(self, &p.codes)?,
            None => self.unpack_codes(&p.codes)?,
        };
        Some(NativeImage { img, offset: p.offset, scale: p.scale })
    }
    pub(crate) fn keep_resident(
        &mut self,
        key: u64,
        image: SourceImage,
        flight: effectcraft_render::cache::FlightGuard,
        intent: Option<effectcraft_render::disk_cache::LayerWriteIntent>,
    ) {
        let Some(owner) = self.owner.clone() else { return };
        if self.g.deferred.is_some() || self.resident_pending.len() >= 256 || self.epoch.strong_count() == 0 {
            return;
        }
        let image = match image.img {
            // Already codes (an 8 bpc upload, a codes canvas): kept as they are, no packing pass.
            crate::codes::Pixels::Codes(codes) => {
                let Some(allocation) = codes.allocation().cloned() else { return };
                codes.share();
                self.g.leased.publish_bytes("resident packed", codes.bytes() as u64);
                let packed = PackedImage { codes, offset: image.offset, scale: image.scale };
                let Ok(pending) = owner.prepare(Arc::new(packed), allocation, GpuStage::Processed, self.g.retained_cap()) else { return };
                if self.resident_pending.try_reserve(1).is_ok() {
                    self.resident_pending.push((pending.with_content(key, self.readback.clone()).with_persistence(intent), flight));
                }
                return;
            }
            crate::codes::Pixels::Float(img) => NativeImage { img, offset: image.offset, scale: image.scale },
        };
        let Some(allocation) = image.img.allocation.clone() else { return };
        // On the 8 bpc grid: kept as codes, a quarter of the bytes (the float image stays this
        // frame's working image).
        let prepared = match self.pack(&image) {
            Some(packed) => {
                let bytes = packed.codes.bytes();
                self.g.leased.publish_bytes("resident packed", bytes as u64);
                let allocation = effectcraft_render::allocation::GpuAllocation::of_bytes(bytes, self.g.generation.clone());
                owner.prepare(Arc::new(packed), allocation, GpuStage::Processed, self.g.retained_cap())
            }
            None => {
                self.g.mips.seal(&image.img);
                image.img.share();
                self.g.leased.publish("resident", &image.img);
                owner.prepare(Arc::new(image), allocation, GpuStage::Processed, self.g.retained_cap())
            }
        };
        let Ok(pending) = prepared else { return };
        if self.resident_pending.try_reserve(1).is_ok() {
            self.resident_pending.push((pending.with_content(key, self.readback.clone()).with_persistence(intent), flight));
        }
    }
    pub fn with_owner(g: &'g GpuContext, owner: Option<GpuOwner>) -> Enc<'g> {
        Enc {
            epoch: g.upload_epoch(),
            owner,
            native_pending: Vec::new(),
            resident_pending: Vec::new(),
            readback: None,
            g,
            enc: None,
            pending: 0,
            submitted: None,
            ring: None,
            pass: None,
            reuse: true,
            mips: Default::default(),
            missing: false,
            shared: Default::default(),
            planes: Vec::new(),
            submitted_ids: Vec::new(),
            opened: None,
            cell: Default::default(),
            #[cfg(feature = "profile")]
            timestamps: None,
        }
    }

    /// Whether a dispatch since the last call named a kernel the device doesn't have (one
    /// that failed to build): its output is wrong, and the effect must render on the CPU.
    pub(crate) fn take_missing(&mut self) -> bool {
        std::mem::take(&mut self.missing)
    }

    /// A pass this encoder needed could not be recorded (see [`Enc::take_missing`]).
    pub(crate) fn mark_missing(&mut self) {
        self.missing = true;
    }

    /// A new pooled codes image for a kernel that writes every pixel of it (see [`Enc::scratch`]).
    #[track_caller]
    pub(crate) fn scratch_codes(&mut self, w: u32, h: u32) -> Option<crate::codes::CodeImage> {
        self.g.codes.as_ref()?;
        self.open_encoder();
        let owner = self.enc.as_ref().map(|(_, id)| *id);
        let (img, _) = self.g.pooled_format(w, h, crate::codes::FORMAT, owner, Some(self.cell.clone()));
        crate::codes::CodeImage::wrap(img)
    }

    /// The command encoder, for anything but a dispatch: ends the open compute pass first, so
    /// copies, clears and other passes record after the dispatches before them.
    pub(crate) fn encoder(&mut self) -> &mut wgpu::CommandEncoder {
        self.pass = None;
        self.open_encoder()
    }

    /// The command encoder, leaving an open compute pass open (only to start the encoder: it
    /// must not record into an encoder with a pass open).
    fn open_encoder(&mut self) -> &mut wgpu::CommandEncoder {
        let g = self.g;
        let cell = &self.cell;
        &mut self
            .enc
            .get_or_insert_with(|| {
                let id = g.pool.lock().map(|mut p| p.open()).unwrap_or(0);
                cell.store(id, std::sync::atomic::Ordering::Release);
                (g.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("effectcraft") }), id)
            })
            .0
    }

    /// Submit what is recorded once enough dispatches wait (or fewer, recorded over a longer
    /// time), so the GPU works through them while the CPU records the rest of the frame instead
    /// of starting only at its end. Called after each dispatch.
    fn submit_early(&mut self) {
        let (on, by_age) = early_submit();
        if !on || self.pending == 0 {
            return;
        }
        let slow = by_age && self.pending >= SUBMIT_EVERY_SLOW && self.opened.is_some_and(|t| t.elapsed() >= SUBMIT_AFTER);
        if self.pending >= SUBMIT_EVERY || slow {
            self.submit();
        }
    }

    /// A new zeroed image (pooled; a reused texture is cleared by a kernel).
    #[track_caller]
    pub fn image(&mut self, w: u32, h: u32) -> GpuImage {
        self.open_encoder();
        let owner = self.enc.as_ref().map(|(_, id)| *id);
        let (img, reused) = self.g.pooled_for_encoder(w, h, owner, Some(self.cell.clone()));
        if reused {
            self.fill(&img, 0.0);
        }
        img
    }

    /// A new image for a kernel that writes every pixel of it (pooled; contents undefined).
    /// Under test it starts out as NaNs, so a kernel that skips pixels fails the CPU oracle.
    #[track_caller]
    pub fn scratch(&mut self, w: u32, h: u32) -> GpuImage {
        self.open_encoder();
        let owner = self.enc.as_ref().map(|(_, id)| *id);
        let (img, _) = self.g.pooled_for_encoder(w, h, owner, Some(self.cell.clone()));
        if cfg!(test) {
            self.fill(&img, f32::NAN);
        }
        img
    }

    /// A transparent image to read from (shared per size; never written).
    pub fn zeros(&self, w: u32, h: u32) -> GpuImage {
        let (w, h) = (w.max(1), h.max(1));
        let Ok(mut z) = self.g.zeros.lock() else { return self.g.image(w, h) };
        if let Some(img) = z.get(&(w, h)) {
            return img.clone();
        }
        let bytes = u64::from(w).saturating_mul(u64::from(h)).saturating_mul(16);
        let budget = self.g.auxiliary_budget.load(std::sync::atomic::Ordering::Relaxed);
        // Oversized images are valid working resources, but must not be retained forever.
        if bytes > budget {
            return self.g.image(w, h);
        }
        let retained = z.values().map(|img| u64::from(img.width).saturating_mul(u64::from(img.height)).saturating_mul(16)).sum::<u64>();
        if retained.saturating_add(bytes) > budget {
            z.clear();
        }
        let img = self.g.image(w, h);
        z.insert((w, h), img.clone());
        img
    }

    /// Set every pixel of `img` to `v`.
    fn fill(&mut self, img: &GpuImage, v: f32) {
        let mut p = Params::default();
        p.f[0] = [v; 4];
        let dummy = self.g.dummy.clone();
        self.pixels("fill", &p, &dummy, None, img, None);
    }

    /// Run kernel `entry` writing `out` (dispatched over `groups` workgroups).
    pub fn dispatch(
        &mut self,
        entry: &str,
        p: &Params,
        src: &GpuImage,
        aux: Option<&GpuImage>,
        out: &GpuImage,
        data: Option<&wgpu::Buffer>,
        groups: (u32, u32),
    ) {
        self.dispatch_ext(entry, p, src, aux, out, data, groups, None);
    }

    /// [`Enc::dispatch`] for the kernels in `EXT_ENTRIES`, with their group 1 storage buffers.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_ext(
        &mut self,
        entry: &str,
        p: &Params,
        src: &GpuImage,
        aux: Option<&GpuImage>,
        out: &GpuImage,
        data: Option<&wgpu::Buffer>,
        groups: (u32, u32),
        ext: Option<[&wgpu::Buffer; 4]>,
    ) {
        let g = self.g;
        let Some(pipe) = g.pipelines.get(entry) else {
            if !g.unavailable.iter().any(|(e, _)| *e == entry) {
                log::error!("gpu: no kernel {entry}");
            }
            self.missing = true;
            return;
        };
        self.dispatch_pipeline(entry, pipe, p, src, aux, out, data, groups, ext);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_pipeline(
        &mut self,
        entry: &str,
        pipe: &wgpu::ComputePipeline,
        p: &Params,
        src: &GpuImage,
        aux: Option<&GpuImage>,
        out: &GpuImage,
        data: Option<&wgpu::Buffer>,
        groups: (u32, u32),
        ext: Option<[&wgpu::Buffer; 4]>,
    ) {
        let g = self.g;
        let views = [&src.view, aux.map_or(&g.dummy.view, |a| &a.view), &out.view];
        self.dispatch_views(entry, pipe, &g.bgl, p, views, data, groups, ext, None);
    }

    /// Record a dispatch of `pipe` (bind group layout `layout`: the kernels' one, or a
    /// [`crate::codes`] variant's, which adds the decode table `lut` at binding 6) reading the
    /// views `[src, aux]` and writing `out`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_views(
        &mut self,
        entry: &str,
        pipe: &wgpu::ComputePipeline,
        layout: &wgpu::BindGroupLayout,
        p: &Params,
        [src, aux, out]: [&wgpu::TextureView; 3],
        data: Option<&wgpu::Buffer>,
        groups: (u32, u32),
        ext: Option<[&wgpu::Buffer; 4]>,
        lut: Option<&wgpu::Buffer>,
    ) {
        let g = self.g;
        if self.pending >= RING_SLOTS {
            self.submit();
        }
        let record = effectcraft_render::profile::span("dispatch_record");
        // Each dispatch until the next submit gets its own block of the encoder's uniform ring.
        // `write_buffer` lands before the next submission's commands and after earlier
        // submissions', so a ring is reused (after a submit, or by a later encoder) safely.
        let offset = (self.pending as u64).saturating_mul(g.uniform_stride);
        let ring = self.ring.get_or_insert_with(|| {
            g.uniforms.lock().ok().and_then(|mut u| u.pop()).unwrap_or_else(|| {
                g.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("params"),
                    size: g.uniform_stride.saturating_mul(RING_SLOTS as u64),
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
        });
        g.queue.write_buffer(ring, offset, &p.bytes());
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: ring, offset, size: wgpu::BufferSize::new(PARAMS_SIZE) }),
            },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(aux) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(out) },
            wgpu::BindGroupEntry { binding: 4, resource: data.unwrap_or(&g.dummy_buf).as_entire_binding() },
        ];
        if let Some(lut) = lut {
            entries.push(wgpu::BindGroupEntry { binding: 6, resource: lut.as_entire_binding() });
        }
        let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout, entries: &entries });
        let bg_ext = ext.map(|b| {
            g.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &g.bgl_ext,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: b[0].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: b[1].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: b[2].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: b[3].as_entire_binding() },
                ],
            })
        });
        #[cfg(feature = "profile")]
        let mut batch = self.timestamps.take().or_else(|| crate::timestamps::Batch::new(g));
        #[cfg(feature = "profile")]
        let timestamp_writes = batch.as_mut().and_then(|b| b.mark(entry));
        #[cfg(not(feature = "profile"))]
        let timestamp_writes = None;
        {
            let pass = self.compute_pass(entry, timestamp_writes);
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, &bg, &[]);
            if let Some(b) = &bg_ext {
                pass.set_bind_group(1, b, &[]);
            }
            pass.dispatch_workgroups(groups.0.max(1), groups.1.max(1), 1);
        }
        self.end_unshared_pass();
        #[cfg(feature = "profile")]
        {
            self.timestamps = batch;
        }
        #[allow(clippy::drop_non_drop)] // A unit span without the `profile` feature.
        drop(record);
        if self.pending == 0 {
            self.opened = Some(std::time::Instant::now());
        }
        self.pending += 1;
        // Keep command buffers (and the transient textures they hold) bounded.
        if self.pending >= 256 {
            self.submit();
        }
        self.submit_early();
    }

    /// Dispatch a dedicated texture layout while retaining profiling and batch bounds.
    pub(crate) fn custom_pixels(&mut self, entry: &str, pipe: &wgpu::ComputePipeline, bg: &wgpu::BindGroup, size: (u32, u32)) {
        #[cfg(feature = "profile")]
        let mut batch = self.timestamps.take().or_else(|| crate::timestamps::Batch::new(self.g));
        #[cfg(feature = "profile")]
        let timestamp_writes = batch.as_mut().and_then(|b| b.mark(entry));
        #[cfg(not(feature = "profile"))]
        let timestamp_writes = None;
        {
            let pass = self.compute_pass(entry, timestamp_writes);
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, bg, &[]);
            pass.dispatch_workgroups(size.0.div_ceil(16), size.1.div_ceil(16), 1);
        }
        self.end_unshared_pass();
        #[cfg(feature = "profile")]
        {
            self.timestamps = batch;
        }
        if self.pending == 0 {
            self.opened = Some(std::time::Instant::now());
        }
        self.pending += 1;
        if self.pending >= 256 {
            self.submit();
        }
        self.submit_early();
    }

    /// The compute pass to record a dispatch into. Consecutive dispatches share one pass (a pass
    /// costs a backend encoder: a Metal compute encoder each); WebGPU makes every dispatch in a
    /// compute pass its own usage scope, so wgpu orders each dispatch after the writes of the ones
    /// before it, exactly as across passes. Anything else recorded on the encoder ends the pass
    /// first ([`Enc::encoder`]), so the recorded order is unchanged. A timed dispatch (profiling)
    /// gets a pass of its own.
    fn compute_pass(&mut self, entry: &str, timestamp_writes: Option<wgpu::ComputePassTimestampWrites<'_>>) -> &mut wgpu::ComputePass<'static> {
        let share = self.reuse && timestamp_writes.is_none();
        let pass = match self.pass.take() {
            Some((pass, true)) if share => pass,
            open => {
                drop(open);
                let label = if share { "effectcraft" } else { entry };
                self.open_encoder().begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(label), timestamp_writes }).forget_lifetime()
            }
        };
        &mut self.pass.insert((pass, share)).0
    }

    /// End a pass no later dispatch may join (a timed one, or without [`Enc::reuse`]) right after
    /// its dispatch, as before passes were shared.
    fn end_unshared_pass(&mut self) {
        if self.pass.as_ref().is_some_and(|(_, shared)| !shared) {
            self.pass = None;
        }
    }

    /// Whether `img` may be drawn on in place by this encoder's next commands instead of
    /// copied into a new image: its pooled texture was taken for the command encoder still
    /// recording, or for one this `Enc` already submitted (so every earlier reader of it is
    /// recorded before, or submitted no later than, the commands that draw on it: one queue
    /// runs submissions in order) and no other image holds it (the caller's `&mut` is the only handle; raw
    /// texture handles are only borrowed while recording). Images from other encoders, shared
    /// ones (`zeros`, caches, clones), sealed ones (their kept mip levels would go stale) and
    /// unpooled ones are never owned.
    pub(crate) fn owns(&self, img: &GpuImage) -> bool {
        let current = self.enc.as_ref().map(|(_, id)| *id).filter(|id| *id != 0);
        let mine = |e: Option<u64>| e.is_some_and(|e| e != 0 && (Some(e) == current || self.submitted_ids.contains(&e)));
        img._lease.as_ref().is_some_and(|l| mine(l.encoder) && Arc::strong_count(l) == 1 && Arc::weak_count(l) == 0) && !self.g.mips.is_sealed(img)
    }

    /// [`Enc::owns`] of f32 or codes.
    pub(crate) fn owns_px(&self, img: &crate::codes::Pixels) -> bool {
        match img {
            crate::codes::Pixels::Float(g) => self.owns(g),
            crate::codes::Pixels::Codes(c) => c.owned_by(self),
        }
    }

    /// Copy the `w` × `h` region of texture `src` at (`x`, `y`) to the top-left of `out` (the
    /// same format: f32 to f32, codes to codes).
    pub(crate) fn copy_texture_region(&mut self, src: &wgpu::Texture, out: &wgpu::Texture, x: u32, y: u32, w: u32, h: u32) {
        self.timed("copy", |enc| {
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: src, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: out, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            )
        });
    }

    /// Whether the kernel `entry` built on this device.
    pub(crate) fn has_kernel(&self, entry: &str) -> bool {
        self.g.pipelines.contains_key(entry)
    }

    /// Per-pixel kernel over `out`'s size (16×16 workgroups).
    pub fn pixels(&mut self, entry: &str, p: &Params, src: &GpuImage, aux: Option<&GpuImage>, out: &GpuImage, data: Option<&wgpu::Buffer>) {
        let groups = (out.width.div_ceil(16), out.height.div_ceil(16));
        self.dispatch(entry, p, src, aux, out, data, groups);
    }

    /// Copy `src` into a larger zeroed image at (`x`, `y`) (Buf::pad).
    pub fn copy_into(&mut self, src: &GpuImage, dst: &GpuImage, x: u32, y: u32) {
        self.timed("copy", |enc| {
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: &src.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: &dst.texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: src.width, height: src.height, depth_or_array_layers: 1 },
            )
        });
    }

    /// Copy the `w`×`h` region of `src` at (`x`, `y`) into a new image (the caller checks
    /// that it lies inside `src`).
    #[track_caller]
    pub fn crop(&mut self, src: &GpuImage, x: u32, y: u32, w: u32, h: u32) -> GpuImage {
        let out = self.scratch(w, h);
        self.copy_region(src, &out, x, y, w, h);
        out
    }

    /// Copy the `w` × `h` region of `src` at (`x`, `y`) into `out` at (`dx`, `dy`) (the caller
    /// checks that both lie inside their images).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn copy_region_to(&mut self, src: &GpuImage, out: &GpuImage, x: u32, y: u32, w: u32, h: u32, dx: u32, dy: u32) {
        self.timed("copy", |enc| {
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: &src.texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo {
                    texture: &out.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: dx, y: dy, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            )
        });
    }

    /// Copy the `w` × `h` region of `src` at (`x`, `y`) to the top-left of `out`.
    pub(crate) fn copy_region(&mut self, src: &GpuImage, out: &GpuImage, x: u32, y: u32, w: u32, h: u32) {
        self.timed("copy", |enc| {
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: &src.texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: &out.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            )
        });
    }

    pub(crate) fn keep_native(
        &mut self,
        owner: effectcraft_render::footage::SharedFrame,
        key: u64,
        token: effectcraft_render::cache::ResidentToken,
        metadata: Vec<u64>,
        image: NativeImage,
    ) {
        let Some(cache) = self.owner.as_ref() else { return };
        if self.g.deferred.is_some() || self.native_pending.len() >= 256 || token.identity() != cache.identity() {
            return;
        }
        let Some(allocation) = image.img.allocation.clone() else { return };
        if !allocation.generation().same(&self.g.generation) {
            return;
        }
        let epoch = self.epoch.clone();
        self.g.mips.seal(&image.img);
        image.img.share();
        self.g.leased.publish("native", &image.img);
        let Ok(pending) = cache.prepare(Arc::new(image), allocation, GpuStage::Blend, self.g.retained_cap()) else { return };
        if self.native_pending.try_reserve(1).is_ok() {
            self.native_pending.push(PendingNative { owner, key, token, metadata, cache: cache.clone(), pending, epoch });
        }
    }
    pub(crate) fn publish_native(&mut self) {
        if self.g.check_health().is_err() {
            self.native_pending.clear();
            self.resident_pending.clear();
            return;
        }
        for (pending, _flight) in self.resident_pending.drain(..) {
            if self.epoch.strong_count() == 0 {
                continue;
            }
            let Some(owner) = self.owner.as_ref() else { continue };
            let Ok(slot) = pending.publish() else { continue };
            let entry = SlotRef { owner: owner.clone(), slot, epoch: self.epoch.clone() };
            let mut u = match self.g.uploads.lock() {
                Ok(u) => u,
                Err(poison) => {
                    drop(poison);
                    entry.release();
                    continue;
                }
            };
            if self.epoch.strong_count() == 0
                || !self.g.generation.valid()
                || !self.g.generation.within_cap()
                || u.resident.len() >= 4096
                || u.resident.try_reserve(1).is_err()
            {
                drop(u);
                entry.release();
                continue;
            }
            u.resident.push(entry);
        }
        for entry in self.native_pending.drain(..) {
            self.g.native_keep(entry);
        }
        crate::mips::publish(self);
    }
    /// A layer buffer's texture as floats (for float-only consumers: effects, 3D).
    pub(crate) fn upload_buf(&mut self, buf: &Arc<Buf>) -> Option<GpuImage> {
        match self.g.upload_buf_owned(buf, self.owner.as_ref())? {
            Uploaded::Image(img) => Some(img),
            Uploaded::Packed(packed) => self.unpack(&packed).map(|n| n.img),
        }
    }

    /// A layer buffer's texture for the compositor: 8 bpc codes stay codes where the codes
    /// kernels exist ([`crate::codes`]).
    pub(crate) fn upload_pixels(&mut self, buf: &Arc<Buf>) -> Option<crate::codes::Pixels> {
        match self.g.upload_buf_owned(buf, self.owner.as_ref())? {
            Uploaded::Image(img) => Some(img.into()),
            Uploaded::Packed(packed) if self.g.codes.is_some() => Some(crate::codes::Pixels::Codes(packed.codes)),
            Uploaded::Packed(packed) => self.unpack(&packed).map(|n| n.img.into()),
        }
    }
    pub(crate) fn keep(&self, img: GpuImage) -> Option<u64> {
        self.g.keep_owned(img, self.owner.as_ref(), &self.epoch)
    }

    /// Whether everything recorded so far has been submitted.
    pub(crate) fn submitted_all(&self) -> bool {
        self.enc.is_none()
    }

    /// Profiling: two timestamp query indices for commands recorded on the encoder itself
    /// (`None` unless the device can write timestamps between encoder commands: not Apple
    /// silicon, whose Metal counters sample only at pass boundaries).
    #[cfg(feature = "profile")]
    fn reserve_stamp(&mut self, name: &str) -> Option<(wgpu::QuerySet, u32)> {
        if !self.g.device.features().contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS) {
            return None;
        }
        let mut batch = self.timestamps.take().or_else(|| crate::timestamps::Batch::new(self.g));
        let mark = batch.as_mut().and_then(|b| b.reserve(name));
        self.timestamps = batch;
        mark
    }

    /// Record `f` on the encoder (copies, clears), timed as `name` in profiling builds.
    fn timed<R>(&mut self, name: &str, f: impl FnOnce(&mut wgpu::CommandEncoder) -> R) -> R {
        #[cfg(feature = "profile")]
        let mark = {
            self.encoder();
            self.reserve_stamp(name)
        };
        #[cfg(not(feature = "profile"))]
        let _ = name;
        let enc = self.encoder();
        #[cfg(feature = "profile")]
        if let Some((q, i)) = &mark {
            enc.write_timestamp(q, *i);
        }
        let r = f(enc);
        #[cfg(feature = "profile")]
        if let Some((q, i)) = &mark {
            enc.write_timestamp(q, i + 1);
        }
        r
    }

    /// Submit recorded work.
    pub fn submit(&mut self) {
        self.pass = None;
        #[allow(unused_mut)]
        if let Some((mut enc, id)) = self.enc.take() {
            self.cell.store(0, std::sync::atomic::Ordering::Release);
            let _profile = effectcraft_render::profile::span("submit");
            #[cfg(feature = "profile")]
            let timing = self.timestamps.take().and_then(|b| b.resolve(self.g, &mut enc));
            self.submitted = Some(self.g.submit(enc.finish()));
            #[cfg(feature = "profile")]
            if let Some(timing) = timing {
                timing.map(self.g.queue.get_timestamp_period());
            }
            if let Ok(mut p) = self.g.pool.lock() {
                p.close(id);
            }
            if id != 0 {
                if self.submitted_ids.len() >= SUBMITTED_KEPT {
                    self.submitted_ids.remove(0);
                }
                self.submitted_ids.push(id);
            }
        }
        self.g.free_planes(std::mem::take(&mut self.planes));
        self.pending = 0;
        self.opened = None;
    }

    /// Hand back a [`GpuContext::plane_texture`] read by dispatches recorded so far: it is
    /// reused once they are submitted (or this encoder is dropped).
    pub(crate) fn stage(&mut self, texture: wgpu::Texture) {
        if self.planes.try_reserve(1).is_ok() {
            self.planes.push(texture);
        }
    }

    /// Read an image back to the CPU (submits and waits). `None` where readback is impossible.
    pub fn download(&mut self, img: &GpuImage) -> Option<Image> {
        let layout = crate::readback::Layout::new(img.width, img.height, 16, self.g.device.limits().max_buffer_size)
            .map_err(|e| log::error!("gpu image readback: {e}"))
            .ok()?;
        let mut data: Vec<[f32; 4]> = Vec::new();
        data.try_reserve_exact(layout.len / 16).map_err(|e| log::error!("gpu image allocation: {e}")).ok()?;
        // Rows go straight from the mapped staging buffer into the image (no zero fill, no
        // intermediate copy).
        self.read_rows(&img.texture, img.width, img.height, 16, |r| match bytemuck::try_cast_slice::<u8, [f32; 4]>(r) {
            Ok(px) => data.extend_from_slice(px),
            Err(_) => data.extend(r.as_chunks::<16>().0.iter().map(|c| bytemuck::pod_read_unaligned::<[f32; 4]>(c))),
        })?;
        if data.len() != layout.len / 16 {
            log::error!("gpu image readback size mismatch");
            return None;
        }
        Some(Image { width: img.width, height: img.height, data })
    }

    /// 8 bpc codes read back as the floats they stand for (4 bytes a pixel over the bus,
    /// decoded with the CPU's own table).
    pub(crate) fn download_codes(&mut self, c: &crate::codes::CodeImage) -> Option<Image> {
        let (w, h) = (c.width(), c.height());
        let codes = self.read_texture(c.texture(), w, h, 4)?;
        let table = crate::codes::decode_table();
        let mut data: Vec<[f32; 4]> = Vec::new();
        data.try_reserve_exact((w as usize).saturating_mul(h as usize)).ok()?;
        data.extend(codes.as_chunks::<4>().0.iter().map(|k| k.map(|k| table[usize::from(k)])));
        (data.len() == (w as usize).saturating_mul(h as usize)).then_some(Image { width: w, height: h, data })
    }

    /// Tightly packed texel bytes of a texture (`bpp` bytes per pixel).
    pub fn read_texture(&mut self, texture: &wgpu::Texture, w: u32, h: u32, bpp: u32) -> Option<Vec<u8>> {
        let layout =
            crate::readback::Layout::new(w, h, bpp, self.g.device.limits().max_buffer_size).map_err(|e| log::error!("gpu texture readback: {e}")).ok()?;
        let mut out = Vec::new();
        out.try_reserve_exact(layout.len).map_err(|e| log::error!("gpu readback allocation: {e}")).ok()?;
        self.read_rows(texture, w, h, bpp, |r| out.extend_from_slice(r))?;
        if out.len() != layout.len {
            log::error!("gpu texture readback size mismatch");
            return None;
        }
        Some(out)
    }

    /// [`Enc::read_texture`] row by row (`w × bpp` bytes each, top to bottom) through a reused
    /// staging buffer: `row` reads each straight from the mapped buffer.
    fn read_rows(&mut self, texture: &wgpu::Texture, w: u32, h: u32, bpp: u32, row: impl FnMut(&[u8])) -> Option<()> {
        let _profile = effectcraft_render::profile::span("readback");
        if !self.g.can_wait() {
            return None;
        }
        let layout =
            crate::readback::Layout::new(w, h, bpp, self.g.device.limits().max_buffer_size).map_err(|e| log::error!("gpu texture readback: {e}")).ok()?;
        self.g.transfers.count(false, layout.len);
        let padded = layout.stride;
        let size = layout.size;
        let buf = self
            .g
            .staging
            .lock()
            .ok()
            .and_then(|mut s| {
                let buf = s.get_mut(&size).and_then(Vec::pop);
                s.retain(|_, buffers| !buffers.is_empty());
                buf
            })
            .unwrap_or_else(|| {
                self.g.transfers.count_made(size);
                self.g.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            });
        self.timed("readback_copy", |enc| {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            )
        });
        self.submit();
        self.read_staging(&buf, layout.row as usize, padded as usize, h as usize, row).map_err(|e| log::error!("gpu texture readback: {e}")).ok()?;
        // Resized layers used to retain two buffers for every size ever read back (#591).
        if let Ok(mut s) = self.g.staging.lock() {
            let budget = self.g.auxiliary_budget.load(std::sync::atomic::Ordering::Relaxed);
            if size > budget {
                return Some(());
            }
            let retained = s.iter().map(|(size, buffers)| size.saturating_mul(buffers.len() as u64)).sum::<u64>();
            if retained.saturating_add(size) > budget {
                s.clear();
            }
            let v = s.entry(size).or_default();
            if v.len() < 2 {
                v.push(buf);
            }
        }
        Some(())
    }

    /// Wait for this encoder's latest submission only and hand the mapped rows of `buf` to `f`.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_staging(&self, buf: &wgpu::Buffer, row: usize, stride: usize, height: usize, f: impl FnMut(&[u8])) -> Result<(), String> {
        self.g.read_mapped(buf, self.submitted.clone(), |view| crate::readback::for_rows(view, row, stride, height, f))
    }

    #[cfg(target_arch = "wasm32")]
    fn read_staging(&self, _buf: &wgpu::Buffer, _row: usize, _stride: usize, _height: usize, _f: impl FnMut(&[u8])) -> Result<(), String> {
        Err("synchronous GPU readback is unavailable in the browser".into())
    }

    /// [`Enc::read_texture`] without waiting for the GPU (the browser's main thread can't): `done`
    /// gets the bytes once the copy is mapped (from the browser's event loop on the web; from
    /// a later device poll natively).
    pub fn read_texture_async(&mut self, texture: &wgpu::Texture, w: u32, h: u32, bpp: u32, done: impl FnOnce(Option<Vec<u8>>) + wgpu::WasmNotSend + 'static) {
        if let Err(e) = self.g.readbacks.check() {
            log::error!("gpu readback: {e}");
            crate::readback::reject(done, e);
            return;
        }
        let layout = match crate::readback::Layout::new(w, h, bpp, self.g.device.limits().max_buffer_size) {
            Ok(layout) => layout,
            Err(e) => {
                log::error!("gpu texture readback: {e}");
                crate::readback::reject(done, e);
                return;
            }
        };
        let row = layout.row;
        let padded = layout.stride;
        let buf = self.g.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback-async"),
            size: layout.size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.encoder().copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.submit();
        self.g.map_readback(&buf, row as usize, padded as usize, h as usize, move |r| {
            done(r.map_err(|e| log::error!("gpu texture readback: {e}")).ok());
        });
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.g.poll_readbacks(wgpu::PollType::Poll);
    }

    /// This encoder's latest submission (the one a readback it started copies in).
    pub(crate) fn last_submission(&self) -> Option<wgpu::SubmissionIndex> {
        self.submitted.clone()
    }

    /// A deferred readback of `img` (RGBA f32) under `key` (see [`crate::deferred`]): the bytes
    /// once they arrived (`Some(None)`: the readback failed), else `None` after starting the
    /// copy if needed and marking the pass missed.
    pub(crate) fn read_keyed(&mut self, img: &GpuImage, key: u128, offset: [f64; 2], scale: f64) -> Option<Option<crate::deferred::Readback>> {
        use crate::deferred::Lookup;
        let d = self.g.deferred.clone()?;
        match d.lookup(key) {
            Lookup::Ready(r) => return Some(r),
            Lookup::InFlight => {}
            Lookup::Absent => {
                self.g.transfers.count(false, img.width as usize * img.height as usize * 16);
                let done = d.start(key, img.width, img.height, offset, scale);
                self.read_texture_async(&img.texture, img.width, img.height, 16, done);
            }
        }
        d.miss();
        None
    }

    /// A data buffer for kernels that read tables (curves).
    pub fn data(&self, v: &[f32]) -> wgpu::Buffer {
        let mut bytes = Vec::with_capacity(v.len().max(4) * 4);
        for x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        self.bytes(bytes)
    }

    /// Copy an image into a storage buffer (RGBA f32 rows) for kernels that read a third image
    /// through `data`: (buffer, row length in pixels).
    pub fn image_rows(&mut self, img: &GpuImage) -> (wgpu::Buffer, u32) {
        let row = (img.width * 16).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buf = self.g.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image rows"),
            size: row as u64 * img.height as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.encoder().copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &img.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(img.height) },
            },
            wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
        );
        (buf, row / 16)
    }

    /// A read-only storage buffer holding `bytes` (padded to 16 bytes).
    pub fn bytes(&self, mut bytes: Vec<u8>) -> wgpu::Buffer {
        while bytes.len() < 16 || !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        self.g.buffer_init(Some("data"), &bytes, wgpu::BufferUsages::STORAGE)
    }

    /// [`Enc::download`] of f32 or codes (codes come back as their floats, 4 bytes a pixel).
    pub(crate) fn download_px(&mut self, img: &crate::codes::Pixels) -> Option<Image> {
        match img {
            crate::codes::Pixels::Float(g) => self.download(g),
            crate::codes::Pixels::Codes(c) => self.download_codes(c),
        }
    }

    /// [`Enc::display`] of f32 or codes (level 0 decodes the codes: the same RGBA8).
    pub(crate) fn display_px(&mut self, img: &crate::codes::Pixels) -> Option<wgpu::Texture> {
        match img {
            crate::codes::Pixels::Float(g) => Some(self.display(g)),
            crate::codes::Pixels::Codes(c) => match crate::codes::display_parts(self.g) {
                Some(parts) => Some(self.display_from(c.view(), c.width(), c.height(), Some(parts))),
                None => {
                    let f = img.float(self)?;
                    Some(self.display(&f))
                }
            },
        }
    }

    /// Convert to the viewer's premultiplied RGBA8 texture (egui-wgpu's native texture format),
    /// with a full mip chain so the viewer minifies it without aliasing (#417).
    pub fn display(&mut self, img: &GpuImage) -> wgpu::Texture {
        self.display_from(&img.view, img.width, img.height, None)
    }

    /// [`Enc::display`] of the image `view` (`width` × `height`); `codes`: it holds 8 bpc codes,
    /// and level 0 runs the codes `display` pass (pipeline, layout, decode table).
    fn display_from(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        codes: Option<(&wgpu::ComputePipeline, &wgpu::BindGroupLayout, &wgpu::Buffer)>,
    ) -> wgpu::Texture {
        let g = self.g;
        let img = (width, height);
        let levels = mip_levels(img.0, img.1);
        g.transfers.count_made(u64::from(img.0).saturating_mul(u64::from(img.1)).saturating_mul(4).saturating_mul(4) / 3);
        let tex = g.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer frame"),
            size: wgpu::Extent3d { width: img.0, height: img.1, depth_or_array_layers: 1 },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let level = |l: u32| tex.create_view(&wgpu::TextureViewDescriptor { base_mip_level: l, mip_level_count: Some(1), ..Default::default() });
        // Level 0 from the frame, then each level the 2 × 2 mean of the one above.
        let mut steps = vec![(&g.display, view.clone(), level(0), img.0, img.1)];
        for l in 1..levels {
            steps.push((&g.display_mip, level(l - 1), level(l), (img.0 >> l).max(1), (img.1 >> l).max(1)));
        }
        #[cfg(feature = "profile")]
        let mut batch = self.timestamps.take().or_else(|| crate::timestamps::Batch::new(g));
        let enc = self.encoder();
        for (step, (pipeline, src, dst, w, h)) in steps.iter().enumerate() {
            let entry = if step == 0 { "display" } else { "display_mip" };
            #[cfg(feature = "profile")]
            let timestamp_writes = batch.as_mut().and_then(|b| b.mark(entry));
            #[cfg(not(feature = "profile"))]
            let timestamp_writes = None;
            let (pipeline, bg) = match codes.filter(|_| step == 0) {
                Some((pipe, layout, lut)) => (
                    pipe,
                    g.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("display codes"),
                        layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
                            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(dst) },
                            wgpu::BindGroupEntry { binding: 6, resource: lut.as_entire_binding() },
                        ],
                    }),
                ),
                None => (
                    *pipeline,
                    g.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &g.display_bgl,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
                            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(dst) },
                        ],
                    }),
                ),
            };
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(entry), timestamp_writes });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(w.div_ceil(16), h.div_ceil(16), 1);
        }
        #[cfg(feature = "profile")]
        {
            self.timestamps = batch;
        }
        tex
    }
}

/// Mip levels of a full chain for a `w × h` texture (down to 1 × 1).
pub(crate) fn mip_levels(w: u32, h: u32) -> u32 {
    u32::BITS - w.max(h).max(1).leading_zeros()
}

#[cfg(test)]
mod upload_budget_tests {
    use super::*;

    #[test]
    fn alloc_stats_count_new_textures_not_reused_ones() {
        let Some(g) = crate::tests::gpu() else { return };
        let ctx = &g.ctx;
        let before = ctx.alloc_stats();
        let mut e = Enc::new(ctx);
        // A size no other test uses: the first is new, the second reuses it after submit.
        drop(e.scratch(37, 23));
        e.submit();
        let mid = ctx.alloc_stats();
        drop(e.scratch(37, 23));
        e.submit();
        let after = ctx.alloc_stats();
        assert!(mid.allocations > before.allocations && mid.bytes >= before.bytes + 37 * 23 * 16);
        // Other tests may allocate concurrently, but never fewer than nothing.
        assert!(after.allocations >= mid.allocations);
    }

    #[test]
    fn pool_encoder_local_reuse_keeps_cross_encoder_barrier() {
        let Some(g) = crate::tests::gpu() else { return };
        let mut pool = Pool::default();
        let first = pool.open();
        pool.release(g.ctx.texture(8, 8), None);
        assert!(pool.take_for_encoder(8, 8, None).is_none());
        let second = pool.open();
        assert!(pool.take_for_encoder(8, 8, Some(first)).is_none());
        assert!(pool.take_for_encoder(8, 8, Some(second)).is_none());
        pool.close(second);
        assert!(pool.take_for_encoder(8, 8, Some(first)).is_some());
        assert_eq!(pool.released_bytes, 0);
        pool.release(g.ctx.texture(8, 8), None);
        pool.clear();
        assert!(pool.take_for_encoder(8, 8, Some(first)).is_none(), "purged releases cannot reappear");
    }

    /// A texture only one `Enc` recorded waits for that `Enc`'s encoder alone (other frames in
    /// flight keep theirs open), and with no encoder of its own open it is free at once; a
    /// shared one keeps waiting for every encoder open when it was released.
    #[test]
    fn pool_private_release_waits_only_for_its_own_encoder() {
        let Some(g) = crate::tests::gpu() else { return };
        let mut pool = Pool::default();
        let mine = pool.open();
        let other = pool.open();
        pool.release(g.ctx.texture(8, 8), Some(mine));
        assert!(pool.take_for_encoder(8, 8, Some(other)).is_none(), "another frame's encoder cannot take it");
        assert!(pool.take_for_encoder(8, 8, Some(mine)).is_some(), "its own encoder reuses it with another frame open");
        pool.release(g.ctx.texture(8, 8), Some(mine));
        pool.release(g.ctx.texture(8, 8), None);
        pool.close(mine);
        assert_eq!(pool.free.get(&(8, 8, FORMAT)).map(Vec::len), Some(1), "free once its own encoder is submitted");
        assert_eq!(pool.released.len(), 1, "the shared one still waits for the other frame");
        pool.release(g.ctx.texture(8, 8), Some(0));
        assert_eq!(pool.free.get(&(8, 8, FORMAT)).map(Vec::len), Some(2), "no encoder of its own open: free at once");
        pool.close(other);
        assert_eq!(pool.free.get(&(8, 8, FORMAT)).map(Vec::len), Some(3));
        assert_eq!(pool.released_bytes, 0);
    }

    /// Two frames in flight: one reuses its own scratch textures while the other's encoder is
    /// open (it used to allocate a new texture for every step), without disturbing reads recorded
    /// before; an image that went into a cache is not reused while another encoder is open.
    #[test]
    fn pool_private_reuse_with_another_frame_in_flight() {
        crate::tests::hold_gpu_lock();
        let g = GpuContext::headless();
        assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
        let Some(g) = g else { return };
        let mut other = Enc::new(&g);
        let theirs = other.image(8, 8);
        other.fill(&theirs, 0.5);
        let mut e = Enc::new(&g);
        let source = e.image(8, 8);
        e.fill(&source, 0.25);
        let saved = e.scratch(8, 8);
        e.copy_into(&source, &saved, 0, 0);
        drop(source);
        let before = g.pool_stats();
        let reused = e.scratch(8, 8);
        assert_eq!(g.pool_stats().0, before.0 + 1, "reused with another encoder open");
        e.fill(&reused, 0.75);
        let cached = e.scratch(8, 8);
        e.fill(&cached, 1.0);
        cached.share();
        drop(cached);
        let before = g.pool_stats();
        let fresh = e.scratch(8, 8);
        assert_eq!(g.pool_stats().1, before.1 + 1, "a shared texture waits for the other frame");
        drop(fresh);
        assert!(e.download(&saved).unwrap().data.iter().flatten().all(|v| *v == 0.25));
        assert!(e.download(&reused).unwrap().data.iter().flatten().all(|v| *v == 0.75));
        assert!(other.download(&theirs).unwrap().data.iter().flatten().all(|v| *v == 0.5));
    }

    #[test]
    fn pool_encoder_local_reuse_preserves_prior_reads() {
        crate::tests::hold_gpu_lock();
        let g = GpuContext::headless();
        assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
        let Some(g) = g else { return };
        let mut e = Enc::new(&g);
        let source = e.image(8, 8);
        e.fill(&source, 0.25);
        let saved = e.scratch(8, 8);
        e.copy_into(&source, &saved, 0, 0);
        drop(source);
        let before = g.pool_stats();
        let reused = e.scratch(8, 8);
        assert_eq!(g.pool_stats().0, before.0 + 1, "reuse before submit");
        e.fill(&reused, 0.75);
        let prior = e.download(&saved).unwrap();
        let later = e.download(&reused).unwrap();
        assert!(prior.data.iter().flatten().all(|v| *v == 0.25));
        assert!(later.data.iter().flatten().all(|v| *v == 0.75));
    }

    #[test]
    fn pool_encoder_local_reuse_reverse_submission_and_rollover() {
        crate::tests::hold_gpu_lock();
        let g = GpuContext::headless();
        assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
        let Some(g) = g else { return };
        let mut first = Enc::new(&g);
        let source = first.image(8, 8);
        first.fill(&source, 0.25);
        first.submit();
        let saved = first.scratch(8, 8);
        first.copy_into(&source, &saved, 0, 0);
        drop(source);
        let mut second = Enc::new(&g);
        let other = second.scratch(8, 8);
        second.fill(&other, 0.75);
        second.submit();
        assert!(first.download(&saved).unwrap().data.iter().flatten().all(|v| *v == 0.25), "later encoder submitted first cannot overwrite pending input");
        let mut current = saved;
        for _ in 0..260 {
            let next = first.scratch(8, 8);
            first.copy_into(&current, &next, 0, 0);
            current = next;
        }
        assert!(first.download(&current).unwrap().data.iter().flatten().all(|v| *v == 0.25), "ring rollover preserves reads");
    }

    #[test]
    fn pool_size_transition_reuses_small_textures_without_touching_pending() {
        let Some(g) = crate::tests::gpu() else { return };
        let mut pool = Pool::default();
        pool.set_budget(8 * 8 * 16);
        pool.release(g.ctx.texture(8, 8), None);
        assert!(pool.take(4, 4, FORMAT).is_none());
        assert_eq!(pool.bytes, 0, "large free texture makes room for new size");
        let encoder = pool.open();
        pool.release(g.ctx.texture(4, 4), None);
        assert_eq!(pool.released.len(), 1);
        assert!(pool.take(4, 4, FORMAT).is_none(), "unsubmitted texture is not reused");
        assert_eq!(pool.released.len(), 1, "pending resources are not evicted");
        pool.close(encoder);
        assert!(pool.take(4, 4, FORMAT).is_some(), "new preview size now reuses its allocation");
        pool.release(g.ctx.texture(4, 4), None);
        pool.release(g.ctx.texture(4, 4), None);
        pool.release(g.ctx.texture(4, 4), None);
        assert!(pool.take(6, 4, FORMAT).is_none());
        assert_eq!(pool.bytes, 3 * 4 * 4 * 16, "collective pressure preserves useful smaller sizes");
        assert_eq!(pool.free.get(&(4, 4, FORMAT)).map(Vec::len), Some(3));
    }

    /// A full pool makes room for a released texture by dropping the size used longest ago,
    /// instead of dropping the released one (RT10b: Full frames re-allocated their canvases).
    #[test]
    fn full_pool_evicts_the_stalest_size_for_a_released_texture() {
        let Some(g) = crate::tests::gpu() else { return };
        let mut pool = Pool::default();
        pool.set_budget(2 * 8 * 8 * 16);
        pool.release(g.ctx.texture(4, 4), None);
        pool.release(g.ctx.texture(2, 2), None);
        pool.release(g.ctx.texture(8, 8), None);
        // 4×4 is the stalest once 2×2 is reused and released again.
        let t = pool.take(2, 2, FORMAT);
        assert!(t.is_some());
        pool.release(t.into_iter().next().map(|p| p.texture).unwrap(), None);
        pool.release(g.ctx.texture(8, 8), None);
        assert_eq!(pool.free.get(&(8, 8, FORMAT)).map(Vec::len), Some(2), "the released texture is kept");
        assert!(!pool.free.contains_key(&(4, 4, FORMAT)), "the stalest size made room");
        assert!(pool.bytes <= 2 * 8 * 8 * 16);
        // An oversized texture never empties the pool.
        pool.release(g.ctx.texture(16, 16), None);
        assert_eq!(pool.free.get(&(8, 8, FORMAT)).map(Vec::len), Some(2));
        assert!(!pool.free.contains_key(&(16, 16, FORMAT)));
    }

    #[test]
    fn pool_pending_reuse_is_bounded_and_purge_cannot_repopulate_it() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        let mut pool = Pool::default();
        pool.set_budget(2 * 8 * 8 * 16);
        let encoder = pool.open();
        for _ in 0..8 {
            pool.release(g.texture(8, 8), None);
        }
        assert_eq!(pool.released.len(), 2);
        assert_eq!(pool.released_bytes, 2 * 8 * 8 * 16);
        pool.set_budget(8 * 8 * 16);
        assert_eq!(pool.released.len(), 1);
        pool.clear();
        pool.close(encoder);
        assert_eq!(pool.bytes, 0);
        assert_eq!(pool.released_bytes, 0);
        assert!(pool.free.is_empty());
    }

    /// Kept effect results awaiting adoption count against the upload budget: an oversized
    /// one is not kept, older ones and uploads make room, and a lower budget drops them.
    #[test]
    fn kept_effect_results_count_against_the_upload_budget() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        let cache = effectcraft_render::LayerCache::new(UPLOAD_BUDGET);
        let owner = cache.gpu_owner().unwrap();
        let one = 8 * 8 * 16;
        g.set_cache_budget(2 * one);
        assert!(g.keep_owned(g.image(16, 16), Some(&owner), &g.upload_epoch()).is_none(), "larger than the budget");
        let buf = Arc::new(Buf { img: Image::new(8, 8), offset: [0.0; 2], scale: 1.0 });
        assert!(g.upload_buf_owned(&buf, Some(&owner)).is_some());
        let a = g.keep_owned(g.image(8, 8), Some(&owner), &g.upload_epoch()).unwrap();
        // An 8 bpc buffer is retained as its codes (a quarter of the bytes).
        let uploaded = if g.yuv.is_some() { one / 4 } else { one };
        assert_eq!((g.kept_count(), g.upload_stats()), (1, (1, uploaded)));
        let b = g.keep_owned(g.image(8, 8), Some(&owner), &g.upload_epoch()).unwrap();
        assert_eq!((g.kept_count(), g.upload_stats()), (2, (0, 0)), "the upload makes room");
        let _c = g.keep_owned(g.image(8, 8), Some(&owner), &g.upload_epoch()).unwrap();
        assert_eq!(g.kept_count(), 2, "the oldest kept result makes room");
        g.adopt(a, &buf);
        assert_eq!(g.upload_stats(), (0, 0), "the dropped ticket adopts nothing");
        g.set_cache_budget(one);
        assert_eq!(g.kept_count(), 1);
        g.adopt(b, &buf);
        assert_eq!(g.upload_stats(), (0, 0));
        g.set_cache_budget(0);
        assert_eq!(g.kept_count(), 0);
        g.set_cache_budget(UPLOAD_BUDGET);
        let _d = g.keep_owned(g.image(8, 8), Some(&owner), &g.upload_epoch()).unwrap();
        g.clear_uploads();
        assert_eq!(g.kept_count(), 0);
    }

    /// Accounting-only payload: no large texture or CPU pixel allocation is made.
    #[test]
    fn integrated_processed_retention_above_old_ceiling_stays_shared_budget_bounded() {
        use effectcraft_render::allocation::{GpuAllocation, GpuGeneration};
        use effectcraft_render::retained::AdmissionError;
        let budget = 1 << 30;
        let (cap, _, _) = cache_budgets(budget, true, 0);
        let generation = GpuGeneration::default();
        generation.set_cap(cap);
        let allocation = GpuAllocation::rgba_f32(4097, 4096, generation.clone()).unwrap();
        let bytes = allocation.bytes();
        assert!(bytes > 256 << 20);
        let cache = effectcraft_render::LayerCache::new(budget);
        let owner = cache.gpu_owner().unwrap();
        let pending = owner.prepare(Arc::new(()), allocation.clone(), GpuStage::Processed, cap).unwrap();
        assert_eq!(cache.retained_stats().pending_bytes, bytes);
        assert_eq!(cache.retained_stats().live_bytes, 0);
        let slot = pending.publish().unwrap();
        assert!(owner.get(&slot, &generation, GpuStage::Processed).is_some());
        assert_eq!(cache.retained_stats().live_bytes, bytes);
        assert_eq!(cache.retained_stats().pending_bytes, 0);
        owner.set_device_cap(&generation, 256 << 20);
        assert!(owner.get(&slot, &generation, GpuStage::Processed).is_none());
        drop(allocation);
        assert_eq!(cache.retained_stats().live_bytes, 0);
        let small = effectcraft_render::LayerCache::new(256 << 20);
        let generation = GpuGeneration::default();
        let allocation = GpuAllocation::rgba_f32(4097, 4096, generation).unwrap();
        assert!(matches!(small.gpu_owner().unwrap().prepare(Arc::new(()), allocation, GpuStage::Processed, cap), Err(AdmissionError::Capacity)));
        assert_eq!(small.retained_stats().pending_bytes, 0);
    }

    #[test]
    fn shared_memory_retention_uses_shared_budget_and_scratch_keeps_lower_caps() {
        assert_eq!(cache_budgets(0, true, 0), (0, 0, 0));
        assert_eq!(cache_budgets(64 << 20, true, 0), (64 << 20, 32 << 20, 4 << 20));
        assert_eq!(cache_budgets(1 << 30, true, 0), (1 << 30, 128 << 20, 32 << 20));
        assert_eq!(cache_budgets(usize::MAX, true, 0), (UPLOAD_BUDGET, 128 << 20, 32 << 20));
        assert_eq!(cache_budgets(usize::MAX, false, 0), (UPLOAD_BUDGET, POOL_BUDGET, AUX_CACHE_BUDGET));
    }

    /// Big video memory (or shared RAM) retains more; the layer budget still bounds it.
    #[test]
    fn retained_ceiling_grows_with_memory() {
        const GIB: u64 = 1 << 30;
        let retained = |layer: usize, integrated: bool, memory: u64| cache_budgets(layer, integrated, memory).0;
        assert_eq!(retained(usize::MAX, false, 8 * GIB), UPLOAD_BUDGET);
        assert_eq!(retained(usize::MAX, false, 16 * GIB), 2 << 30);
        assert_eq!(retained(usize::MAX, false, 24 * GIB), RETAINED_MAX);
        assert_eq!(retained(usize::MAX, false, 80 * GIB), RETAINED_MAX);
        assert_eq!(retained(usize::MAX, true, 16 * GIB), UPLOAD_BUDGET);
        assert_eq!(retained(usize::MAX, true, 64 * GIB), RETAINED_MAX);
        assert_eq!(retained(1 << 30, false, 24 * GIB), 1 << 30);
    }

    /// RT9a: unified memory with plenty of RAM gets the discrete pool; small or unknown RAM
    /// (weak iGPUs) keeps 128 MiB; the layer budget still bounds it.
    #[test]
    fn unified_memory_pool_cap_scales_with_ram() {
        const GIB: u64 = 1 << 30;
        let pool = |layer: usize, integrated: bool, ram: u64| cache_budgets(layer, integrated, ram).1;
        assert_eq!(pool(usize::MAX, true, 0), 128 << 20);
        assert_eq!(pool(usize::MAX, true, 8 * GIB), 128 << 20);
        assert_eq!(pool(usize::MAX, true, 16 * GIB - 1), 128 << 20);
        assert_eq!(pool(usize::MAX, true, 16 * GIB), 128 << 20);
        assert_eq!(pool(usize::MAX, true, 24 * GIB), 448 << 20);
        assert_eq!(pool(usize::MAX, true, 32 * GIB), POOL_BUDGET);
        assert_eq!(pool(usize::MAX, true, 64 * GIB), POOL_BUDGET);
        assert_eq!(pool(usize::MAX, true, u64::MAX), POOL_BUDGET);
        assert_eq!(pool(1 << 30, true, 64 * GIB), 512 << 20);
        assert_eq!(pool(usize::MAX, false, 8 * GIB), POOL_BUDGET);
        // Scratch caches keep their unified-memory caps.
        assert_eq!(cache_budgets(usize::MAX, true, 64 * GIB).2, 32 << 20);
    }

    /// CR1: a discrete GPU with plenty of video memory gets a larger pool (a sixteenth of it, up
    /// to 2 GiB); unknown or small video memory keeps [`POOL_BUDGET`]; the layer budget still
    /// bounds it.
    #[test]
    fn discrete_pool_cap_scales_with_video_memory() {
        const GIB: u64 = 1 << 30;
        let pool = |layer: usize, vram: u64| cache_budgets(layer, false, vram).1;
        assert_eq!(pool(usize::MAX, 0), POOL_BUDGET);
        assert_eq!(pool(usize::MAX, 8 * GIB), POOL_BUDGET);
        assert_eq!(pool(usize::MAX, 12 * GIB), POOL_BUDGET);
        assert_eq!(pool(usize::MAX, 24 * GIB), 1536 << 20);
        assert_eq!(pool(usize::MAX, 48 * GIB), DISCRETE_POOL_MAX);
        assert_eq!(pool(usize::MAX, u64::MAX), DISCRETE_POOL_MAX);
        assert_eq!(pool(1 << 30, 24 * GIB), 512 << 20);
    }

    #[test]
    fn auxiliary_caches_are_bounded_across_sizes_and_purged() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        // A tiny budget makes changing layer extents exercise eviction with kilobytes.
        g.set_cache_budget(16 * 4096);
        for width in 8..40 {
            let mut enc = Enc::new(&g);
            let zero = enc.zeros(width, 8);
            let pixels = enc.download(&zero).unwrap();
            assert!(pixels.data.iter().all(|p| *p == [0.0; 4]));
            let retained = g.zeros.lock().unwrap().values().map(|img| tex_bytes(&img.texture)).sum::<usize>();
            assert!(retained <= 4096, "zero cache retained {retained} bytes");
            let staging = g.staging.lock().unwrap();
            let retained = staging.iter().map(|(size, buffers)| size * buffers.len() as u64).sum::<u64>();
            assert!(retained <= 4096, "staging cache retained {retained} bytes");
            assert!(staging.values().all(|buffers| !buffers.is_empty()));
        }
        g.clear_uploads();
        assert!(g.zeros.lock().unwrap().is_empty());
        assert!(g.staging.lock().unwrap().is_empty());
        g.set_cache_budget(0);
        let mut enc = Enc::new(&g);
        let zero = enc.zeros(8, 8);
        assert!(enc.download(&zero).is_some());
        assert!(g.zeros.lock().unwrap().is_empty());
        assert!(g.staging.lock().unwrap().is_empty());
    }

    /// Readback copies rows straight out of the padded staging buffer: an odd width (row
    /// padding) round-trips bit-exactly through `download` and `read_texture`.
    #[test]
    fn readback_round_trips_padded_rows() {
        crate::tests::hold_gpu_lock();
        let Some(g) = GpuContext::headless() else { return };
        let (w, h) = (7, 5);
        let data: Vec<[f32; 4]> = (0..w * h).map(|i| [i as f32, -0.5 * i as f32, f32::MIN_POSITIVE * i as f32, 1.0 / (i as f32 + 3.0)]).collect();
        let img = Image { width: w, height: h, data };
        let up = g.upload_image(&img).unwrap();
        let mut e = Enc::new(&g);
        let down = e.download(&up).unwrap();
        assert_eq!((down.width, down.height), (w, h));
        assert_eq!(bytemuck::cast_slice::<_, u8>(&down.data), bytemuck::cast_slice::<_, u8>(&img.data));
        let bytes = e.read_texture(&up.texture, w, h, 16).unwrap();
        assert_eq!(bytes, bytemuck::cast_slice::<_, u8>(&img.data));
    }

    /// Packed 8/16 bpc uploads expand on the GPU to exactly the floats the CPU unpack makes
    /// (`k as f32 * (1.0 / levels)`), edge codes included, and match the f32 upload bit for
    /// bit; images on the grid take the packed path, others the f32 one; upload textures are
    /// pooled.
    #[test]
    fn packed_uploads_match_f32_uploads() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        let (w, h) = (37u32, 19u32);
        let n = (w * h) as usize;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let f32_upload = |data: &[[f32; 4]]| {
            let img = Image { width: w, height: h, data: data.to_vec() };
            let out = g.image(w, h);
            g.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &out.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                bytemuck::cast_slice(&img.data),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 16), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            Enc::new(&g).download(&out).unwrap().data
        };
        let bits = |d: &[[f32; 4]]| d.iter().flatten().map(|v| v.to_bits()).collect::<Vec<u32>>();
        for (levels, edges, max) in [(LEVELS_8, vec![0u32, 255], 255u32), (LEVELS_16, vec![0, 255, 32768, 65535], 65535)] {
            let mut codes: Vec<[u32; 4]> = (0..n).map(|_| [0; 4].map(|_: u32| (rand() % (u64::from(max) + 1)) as u32)).collect();
            for (i, e) in edges.iter().enumerate() {
                codes[i] = [*e; 4];
                codes[n - 1 - i] = [*e, 0, max, *e];
            }
            let inv = 1.0 / levels;
            let floats: Vec<[f32; 4]> = codes.iter().map(|c| c.map(|k| k as f32 * inv)).collect();
            let out = g.image(w, h);
            let ok = if levels == LEVELS_8 {
                let c: Vec<[u8; 4]> = codes.iter().map(|c| c.map(|k| k as u8)).collect();
                g.upload_codes(&out, wgpu::TextureFormat::Rgba8Uint, bytemuck::cast_slice(&c), 4, levels, false)
            } else {
                let c: Vec<[u16; 4]> = codes.iter().map(|c| c.map(|k| k as u16)).collect();
                g.upload_codes(&out, wgpu::TextureFormat::Rgba16Uint, bytemuck::cast_slice(&c), 8, levels, false)
            };
            assert!(ok.is_some());
            let packed = Enc::new(&g).download(&out).unwrap().data;
            assert_eq!(bits(&packed), bits(&floats), "{levels} levels: GPU expansion differs from the CPU unpack");
            assert_eq!(bits(&packed), bits(&f32_upload(&floats)), "{levels} levels: packed upload differs from the f32 upload");

            // Through `upload_image`: in-range grid images go up packed.
            let grid: Vec<[f32; 4]> = floats.iter().map(|p| p.map(|v| v.min(1.0))).collect();
            let before = g.transfer_stats().upload_bytes;
            let up = g.upload_image(&Image { width: w, height: h, data: grid.clone() }).unwrap();
            let bpp = if levels == LEVELS_8 { 4 } else { 8 };
            assert_eq!(g.transfer_stats().upload_bytes - before, (n * bpp) as u64, "{levels} levels: not uploaded packed");
            assert_eq!(bits(&Enc::new(&g).download(&up).unwrap().data), bits(&grid));
        }
        // Off the grid (32 bpc): the f32 path, unchanged.
        let off: Vec<[f32; 4]> = (0..n).map(|i| [i as f32 * 0.001, -0.25, 1.5, f32::from_bits(rand() as u32 & 0x3f7f_ffff)]).collect();
        let before = g.transfer_stats().upload_bytes;
        let up = g.upload_image(&Image { width: w, height: h, data: off.clone() }).unwrap();
        assert_eq!(g.transfer_stats().upload_bytes - before, (n * 16) as u64);
        assert_eq!(bits(&Enc::new(&g).download(&up).unwrap().data), bits(&off));
        // The code textures went back to the pool, one per format, and are reused.
        let free = |f| g.planes.lock().unwrap().get(&(w, h, f)).map_or(0, Vec::len);
        assert_eq!((free(wgpu::TextureFormat::Rgba8Uint), free(wgpu::TextureFormat::Rgba16Uint)), (1, 1));
        let grid: Vec<[f32; 4]> = (0..n).map(|i| [(i % 256) as f32 * (1.0 / LEVELS_8); 4]).collect();
        let up = g.upload_image(&Image { width: w, height: h, data: grid.clone() }).unwrap();
        assert_eq!(free(wgpu::TextureFormat::Rgba8Uint), 1);
        assert_eq!(bits(&Enc::new(&g).download(&up).unwrap().data), bits(&grid));
    }

    /// Every dispatch reads its own uniform ring slot, also across the automatic submit after
    /// [`RING_SLOTS`] dispatches and when a dropped encoder's ring is reused.
    #[test]
    fn uniform_ring_slots_keep_each_dispatch_params() {
        crate::tests::hold_gpu_lock();
        let Some(g) = GpuContext::headless() else { return };
        for _ in 0..2 {
            let mut e = Enc::new(&g);
            let n = RING_SLOTS + 44;
            let imgs: Vec<GpuImage> = (0..n)
                .map(|i| {
                    let img = GpuImage::new(g.texture(2, 2), 2, 2);
                    e.fill(&img, i as f32);
                    img
                })
                .collect();
            for (i, img) in imgs.iter().enumerate() {
                let px = e.download(img).unwrap();
                assert!(px.data.iter().all(|p| *p == [i as f32; 4]), "dispatch {i}: {:?}", px.data);
            }
        }
        assert!(g.uniforms.lock().unwrap().len() <= RINGS_KEPT);
    }

    /// A [`Params`] block fills exactly one uniform ring slot's binding.
    #[test]
    fn params_fill_one_ring_slot() {
        assert_eq!(Params::default().bytes().len() as u64, PARAMS_SIZE);
    }

    fn buf(w: u32) -> Arc<Buf> {
        Arc::new(Buf { img: Image::filled(w, 8, [0.5, 0.25, 0.125, 0.5]), offset: [0.0; 2], scale: 1.0 })
    }

    /// Textures of layer buffers that are gone are dropped at the next upload, not only once the
    /// upload budget overflows; the budget follows the layer cache's.
    #[test]
    fn dead_uploads_are_purged_and_budget_follows_prefs() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        eprintln!("retained upload adapter {}", g.name);
        let cache = effectcraft_render::LayerCache::new(UPLOAD_BUDGET);
        let owner = cache.gpu_owner().unwrap();
        let a = buf(8);
        let b = buf(16);
        assert!(g.upload_buf_owned(&a, Some(&owner)).is_some() && g.upload_buf_owned(&b, Some(&owner)).is_some());
        assert_eq!(g.upload_stats(), (2, (8 + 16) * 8 * 16));
        drop(a);
        let c = buf(4);
        assert!(g.upload_buf_owned(&c, Some(&owner)).is_some());
        assert_eq!(g.upload_stats(), (2, (16 + 4) * 8 * 16), "a's texture dropped");
        // A budget below b + c keeps the most recently used.
        g.set_cache_budget(4 * 8 * 16 + 100);
        assert_eq!(g.upload_stats(), (1, 4 * 8 * 16));
        // Re-uploading the same live buffer is a hit.
        let ups = g.transfer_stats().uploads;
        assert!(g.upload_buf_owned(&c, Some(&owner)).is_some());
        assert_eq!(g.transfer_stats().uploads, ups);
        drop(b);
    }
    #[test]
    fn retained_indexes_drop_outside_lock_and_fan_out_to_two_owners() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Probe {
            index: Weak<Mutex<Uploads>>,
            cache: Weak<effectcraft_render::LayerCache>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                if let Some(index) = self.index.upgrade() {
                    assert!(index.try_lock().is_ok(), "index lock held during backend drop");
                }
                if let Some(cache) = self.cache.upgrade() {
                    let _ = cache.retained_stats();
                }
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let index = Arc::new(Mutex::new(Uploads::default()));
        let generation = effectcraft_render::allocation::GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let a = Arc::new(effectcraft_render::LayerCache::new(64));
        let b = Arc::new(effectcraft_render::LayerCache::new(64));
        let pixel = Arc::new(Buf { img: Image::new(1, 1), offset: [0.0; 2], scale: 1.0 });
        for (key, cache) in [(1, &a), (2, &b)] {
            cache.insert(key, pixel.clone());
            let owner = cache.gpu_owner().unwrap();
            let payload = Arc::new(Probe { index: Arc::downgrade(&index), cache: Arc::downgrade(cache), drops: drops.clone() });
            let allocation = effectcraft_render::allocation::GpuAllocation::rgba_f32(1, 1, generation.clone()).unwrap();
            let slot = owner.prepare(payload, allocation, GpuStage::CpuMirror, 64).unwrap().publish().unwrap();
            let mut u = index.lock().unwrap();
            let epoch = Arc::downgrade(&u.epoch);
            u.map.insert((owner.identity(), key as usize), Upload { buf: Arc::downgrade(&pixel), slot: SlotRef { owner, slot, epoch } });
        }
        generation.retire();
        clear_retained(&index);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert!(a.get(1).is_some() && b.get(2).is_some());
        assert_eq!(a.retained_stats().live_bytes, 16);
        assert_eq!(b.retained_stats().live_bytes, 16);
    }
    #[test]
    fn ordinary_resident_device_failure_rejects_readback_and_preserves_materialized_cpu() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(context) = context else { return };
        let gpu = crate::Gpu::from_context(context);
        eprintln!("ordinary failure adapter {}", gpu.ctx.name);
        let mut scene = crate::tests::Scene::new(effectcraft_project::BitDepth::Bpc32);
        let layer = scene.solid([0.3, 0.5, 0.7], 20, 15);
        let id = scene.push(layer);
        let cache = effectcraft_render::LayerCache::default();
        let mut renderer = effectcraft_render::Renderer::new(
            &scene.p,
            &crate::tests::Pattern,
            effectcraft_render::RenderOpts { backend: effectcraft_render::Backend::Gpu, ..Default::default() },
        );
        renderer.accel = Some(&gpu);
        renderer.cache = Some(&cache);
        gpu.within_memory(|| gpu.render_display(&renderer, scene.cid, effectcraft_time::Tick::ZERO)).unwrap().unwrap();
        let ctx = renderer.eval_ctx(scene.cid, effectcraft_time::Tick::ZERO).unwrap();
        let layer = ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
        let key = renderer.resident_content_key(&ctx, layer).unwrap();
        let slot = cache.gpu_owner().unwrap().resident(key, GpuStage::Processed).unwrap();
        let cpu = cache.get(key).unwrap();
        let before = gpu.ctx.transfer_stats();
        gpu.ctx.device_failure_notifier()("synthetic ordinary-resident device failure".into());
        assert!(effectcraft_render::cache::ResidentReadback::readback(&LazyReadback(Arc::downgrade(&gpu.ctx)), &slot).is_none());
        assert_eq!(gpu.ctx.transfer_stats().readbacks, before.readbacks, "failed device performs no readback allocation/map");
        assert!(Arc::ptr_eq(&cpu, &cache.get(key).unwrap()));
        assert!(cache.gpu_owner().unwrap().resident(key, GpuStage::Processed).is_none());
        assert!(gpu.render_display(&renderer, scene.cid, effectcraft_time::Tick::ZERO).is_none());
        let fallback = renderer.comp_frame(scene.cid, effectcraft_time::Tick::ZERO);
        let oracle =
            effectcraft_render::Renderer::new(&scene.p, &crate::tests::Pattern, Default::default()).comp_frame_cpu(scene.cid, effectcraft_time::Tick::ZERO);
        assert_eq!(fallback, oracle, "public renderer falls back to the exact CPU frame after device failure");
        assert_eq!(gpu.ctx.transfer_stats().readbacks, before.readbacks, "fallback does not retry failed-device readback");
        drop(slot);
        assert_eq!(cache.retained_stats().live_bytes, cpu.img.data.capacity() * std::mem::size_of::<[f32; 4]>());
    }

    #[test]
    fn retained_gpu_identity_clear_and_device_failure_preserve_cpu() {
        crate::tests::hold_gpu_lock();
        let context = GpuContext::headless();
        assert!(context.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
        let Some(g) = context else { return };
        eprintln!("retained ownership adapter {}", g.name);
        let a = effectcraft_render::LayerCache::new(256);
        let b = effectcraft_render::LayerCache::new(256);
        let ao = a.gpu_owner().unwrap();
        let bo = b.gpu_owner().unwrap();
        let pixels = Arc::new(Buf { img: Image::new(1, 1), offset: [0.0; 2], scale: 1.0 });
        a.insert(1, pixels.clone());
        b.insert(2, pixels.clone());
        let first = g.upload_buf_owned(&pixels, Some(&ao)).unwrap();
        let uploads = g.transfer_stats().uploads;
        let alias = g.upload_buf_owned(&pixels, Some(&ao)).unwrap();
        // The second call is the retained texture: a float image, or the codes of an 8 bpc one.
        match (&first, &alias) {
            (Uploaded::Image(first), Uploaded::Image(alias)) => assert!(Arc::ptr_eq(first.allocation.as_ref().unwrap(), alias.allocation.as_ref().unwrap())),
            (Uploaded::Image(_), Uploaded::Packed(_)) => assert!(g.yuv.is_some()),
            (Uploaded::Packed(_), Uploaded::Packed(_)) => assert!(g.codes.is_some()),
            _ => panic!("a fresh upload hands back its float image (its codes with the codes kernels)"),
        }
        assert_eq!(g.transfer_stats().uploads, uploads);
        assert_eq!(g.image(0, 0).allocation.as_ref().unwrap().bytes(), 16);
        assert_eq!(GpuImage::new(g.texture(2, 2), 100, 100).allocation.as_ref().unwrap().bytes(), 64);
        drop(first);
        drop(alias);
        g.clear_uploads();
        assert_eq!(g.upload_stats(), (0, 0));
        assert!(g.upload_buf_owned(&pixels, Some(&ao)).is_some());
        assert!(g.upload_buf_owned(&pixels, Some(&bo)).is_some());
        assert_eq!(g.upload_stats(), (2, if g.yuv.is_some() { 8 } else { 32 }));
        g.device_failure_notifier()("synthetic retained ownership device failure".to_owned());
        assert!(g.check_health().is_err());
        assert_eq!(g.upload_stats(), (0, 0));
        assert!(a.get(1).is_some() && b.get(2).is_some());
        assert_eq!((a.retained_stats().live_bytes, b.retained_stats().live_bytes), (16, 16));
    }
}
