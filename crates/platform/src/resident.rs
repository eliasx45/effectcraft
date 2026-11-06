//! Hardware-decoded ProRes pictures kept on the GPU, their CPU copy made only when something
//! asks for it (Fix 3, 1 Nov 2026; safe code).
//!
//! Until Fix 3 every picture VideoToolbox decoded was copied out of its surface into planes on the
//! decoder's callback thread (~1.5 ms at 1080p 4444 + alpha, ~4 ms at 4K, plus faulting in the
//! fresh pages) and, with Decode Footage at 8 Bits, rounded to 8 bits on the CPU (~2.7 / ~6 ms),
//! even when the GPU compositor then read the picture straight from the surface (Fix 2).
//!
//! Now, when the media pool reads a movie it keeps open on the hardware decoder
//! ([`effectcraft_media::hw::resident_allowed`]) and the GPU shares surfaces
//! ([`crate::surfaces::enabled`]), the decode only keeps the decoded buffer ([`Resident`]) and
//! hands FilmCraft a *stand-in* frame ([`stand_in`]): the picture's size, layout and colour, a
//! one-sample-capacity luma allocation that keys the picture (its address is what the GPU asks
//! [`crate::surfaces::import`] for, as with Fix 2's luma planes), and shared zero "ballast" planes
//! (one allocation per length, virtual memory only) so the stand-in's byte size is the picture's
//! and FilmCraft's GOP cache keeps as many frames as before. The media pool turns a stand-in into
//! a frame of its cache that holds the picture ([`resolve`]); its CPU samples are copied out of
//! the surface the first time a CPU reader asks ([`Resident::planes`]), into recycled buffers.
//!
//! **Lifetime.** A picture stays here while its stand-in is alive (the GOP cache or the media
//! pool's frame holds it): the registry holds the picture, the stand-in's luma allocation is
//! watched weakly, and pictures whose stand-in is gone leave at the next decode or [`sweep`].
//!
//! **Bounds; never stalling.** Held pictures are bounded by bytes ([`budget`],
//! [`DEFAULT_BUDGET`]): past it a decode copies its picture out as before. Pictures only a
//! decoder's own cache of recent frames still holds (*spare*: FilmCraft's GOP cache keeps the last
//! 64 frames of each movie, and the media pool let go of these or never took them) are counted in
//! the media pool's share of the cache budget and are the first to go when it needs room
//! ([`spare_bytes`], [`shed_spare`]); each decoder keeps at most [`SPARE_PER_DECODER`] of them, all
//! together at most an eighth of the [`budget`] (Elias, 1 Nov 2026: macOS can't compress GPU
//! memory and swaps it whole; on 30 Oct an embedded EffectCraft had 28 GB of GPU memory swapped
//! out and renders waited 77 of 88 ms). A frame whose spare picture went is decoded again when
//! read (~2-3 ms). VideoToolbox gives each
//! picture a new buffer while earlier ones are held, so holding never blocks a decode; if a
//! hardware decode fails while pictures are held, decodes copy their pictures out for
//! [`PRESSURE_FOR`] ([`pressure`]) and the held ones leave with their frames (their caches evict
//! them as usual). `EFFECTCRAFT_RESIDENT_FRAMES=0` (or [`set_enabled`]) turns all of this off:
//! every picture is copied out at decode, as before Fix 3.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use effectcraft_gpu::surface::SurfaceKey;
use effectcraft_media::hw::{Planes16, ResidentFrame};
use filmcraft_frame::{Chroma, PixelData, VideoFrame};

use crate::convert::Planes;
use crate::metal_surface::Surface;
use crate::videotoolbox;

/// Bytes of pictures held in surfaces at once by default (about 60 4K ProRes 4444 + alpha
/// pictures, 240 at 1080p; Elias, 1 Nov 2026: 4 GiB, not 8). The media pool holds what its
/// budget allows, decoders' caches only a few spare ones ([`SPARE_PER_DECODER`]).
pub const DEFAULT_BUDGET: usize = 4 << 30;
/// Spare pictures (held only by a decoder's cache of recent frames) each decoder keeps at most.
pub const SPARE_PER_DECODER: usize = 8;
/// After a hardware decode failed while pictures were held, decodes copy their pictures out for
/// this long.
pub const PRESSURE_FOR: Duration = Duration::from_secs(5);

static ON: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(std::env::var("EFFECTCRAFT_RESIDENT_FRAMES").map_or(true, |v| !matches!(v.trim(), "0" | "off" | "false"))));
static BUDGET: AtomicUsize = AtomicUsize::new(DEFAULT_BUDGET);
/// Milliseconds since [`EPOCH`] of the last hardware failure while pictures were held (0: none).
static PRESSURE: AtomicU64 = AtomicU64::new(0);
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
/// Pictures copied out of their surface on demand so far.
static COPIED: AtomicU64 = AtomicU64::new(0);
/// Pictures left in their surface at decode so far.
static KEPT: AtomicU64 = AtomicU64::new(0);
/// Spare pictures let go so far (their frames are decoded again when read).
static SHED: AtomicU64 = AtomicU64::new(0);
/// Bytes of spare pictures at the last look (see [`spare_bytes`]).
static SPARE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Decoders that held pictures (ids, see [`Resident::new`]).
static DECODER_IDS: AtomicU64 = AtomicU64::new(1);

/// A new id for a decoder whose pictures are held here.
pub(crate) fn decoder_id() -> u64 {
    DECODER_IDS.fetch_add(1, Ordering::Relaxed)
}

/// A decoded picture left in its VideoToolbox buffer.
pub(crate) struct Resident {
    surface: Surface,
    pixel_format: u32,
    width: u32,
    height: u32,
    /// Coded depth of the samples the CPU copy holds (10: `x422`, 12: `s4as`).
    bits: u32,
    subsampling: [u32; 2],
    alpha: bool,
    bytes: usize,
    /// The decoder that made it ([`decoder_id`]).
    decoder: u64,
}

impl Resident {
    /// The picture `surface` holds: `pixel_format` ([`videotoolbox::S4AS`] / [`videotoolbox::X422`]),
    /// `width`×`height`, `bits` deep, alpha when `alpha`, made by `decoder` ([`decoder_id`]).
    pub(crate) fn new(surface: Surface, pixel_format: u32, (width, height): (u32, u32), bits: u32, subsampling: [u32; 2], alpha: bool, decoder: u64) -> Self {
        let bytes = surface.bytes();
        Self { surface, pixel_format, width, height, bits, subsampling, alpha, bytes, decoder }
    }

    /// Copy the samples out (what the decoder's callback did before Fix 3).
    pub(crate) fn copy(&self, reuse: Planes) -> Result<Planes, String> {
        videotoolbox::copy_surface(&self.surface, self.pixel_format, self.width as usize, self.height as usize, self.alpha, reuse)
    }

    /// The surface itself (a picture copied out after all keeps it for the GPU, Fix 2).
    pub(crate) fn into_surface(self) -> Surface {
        self.surface
    }
}

impl ResidentFrame for Resident {
    fn planes(&self, reuse: Planes16) -> Result<Planes16, String> {
        let p = self.copy(Planes { y: reuse.y, cb: reuse.cb, cr: reuse.cr, alpha: reuse.alpha })?;
        COPIED.fetch_add(1, Ordering::Relaxed);
        Ok(Planes16 { y: p.y, cb: p.cb, cr: p.cr, alpha: p.alpha })
    }
    fn bytes(&self) -> usize {
        self.bytes
    }
}

struct Entry {
    /// The stand-in's luma allocation (its address is the key).
    luma: Weak<Vec<u16>>,
    picture: Arc<Resident>,
    /// Order of arrival.
    seq: u64,
}

impl Entry {
    /// Only the registry holds the picture: no frame of the media pool has it, only the stand-in
    /// in a decoder's cache of recent frames. (Checked under the registry's lock, which every new
    /// reference ([`resolve`]) is made under.)
    fn spare(&self) -> bool {
        Arc::strong_count(&self.picture) == 1
    }
}

#[derive(Default)]
struct Registry {
    entries: HashMap<usize, Entry>,
    bytes: usize,
    seq: u64,
    /// Shared zero planes by length (see [`stand_in`]).
    ballast: HashMap<usize, Weak<Vec<u16>>>,
}

impl Registry {
    /// Let go of pictures whose stand-in is gone.
    fn sweep(&mut self) -> Vec<Arc<Resident>> {
        let mut gone = Vec::new();
        self.entries.retain(|_, e| {
            let alive = e.luma.strong_count() > 0;
            if !alive {
                gone.push(e.picture.clone());
            }
            alive
        });
        self.bytes = self.entries.values().map(|e| e.picture.bytes).fold(0, usize::saturating_add);
        self.ballast.retain(|_, w| w.strong_count() > 0);
        self.count_spare();
        gone
    }

    /// Bytes of spare pictures now (also stored for [`spare_bytes`]).
    fn count_spare(&self) -> usize {
        let n = self.entries.values().filter(|e| e.spare()).map(|e| e.picture.bytes).fold(0, usize::saturating_add);
        SPARE_BYTES.store(n, Ordering::Relaxed);
        n
    }

    /// Take out the entry at `key`.
    fn take(&mut self, key: usize, gone: &mut Vec<Arc<Resident>>) -> usize {
        let Some(e) = self.entries.remove(&key) else { return 0 };
        let b = e.picture.bytes;
        self.bytes = self.bytes.saturating_sub(b);
        SHED.fetch_add(1, Ordering::Relaxed);
        gone.push(e.picture);
        b
    }

    /// Let spare pictures go: each decoder keeps its newest [`SPARE_PER_DECODER`], all together
    /// at most `bound` bytes (oldest first).
    fn trim(&mut self, bound: usize, gone: &mut Vec<Arc<Resident>>) {
        let mut spare: Vec<(u64, u64, usize, usize)> =
            self.entries.iter().filter(|(_, e)| e.spare()).map(|(k, e)| (e.picture.decoder, e.seq, *k, e.picture.bytes)).collect();
        // newest first within each decoder
        spare.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut keep = Vec::with_capacity(spare.len());
        let mut run = (u64::MAX, 0usize);
        for s in spare {
            run = if run.0 == s.0 { (s.0, run.1.saturating_add(1)) } else { (s.0, 1) };
            if run.1 > SPARE_PER_DECODER {
                self.take(s.2, gone);
            } else {
                // (each decoder's newest stays: the media pool is about to take it)
                keep.push((s, run.1 == 1));
            }
        }
        // then the oldest overall, past the byte bound
        keep.sort_unstable_by_key(|(s, _)| s.1);
        let mut bytes = keep.iter().map(|(s, _)| s.3).fold(0, usize::saturating_add);
        for (s, newest) in keep {
            if bytes <= bound {
                break;
            }
            if !newest {
                bytes = bytes.saturating_sub(self.take(s.2, gone));
            }
        }
        SPARE_BYTES.store(bytes, Ordering::Relaxed);
    }

    /// The shared zero plane of `len` samples.
    fn ballast(&mut self, len: usize) -> Arc<Vec<u16>> {
        if let Some(b) = self.ballast.get(&len).and_then(Weak::upgrade) {
            return b;
        }
        // `vec![0; n]` is calloc: fresh zero pages that are never written stay virtual
        let b = Arc::new(vec![0u16; len]);
        self.ballast.insert(len, Arc::downgrade(&b));
        b
    }
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    u64::try_from(EPOCH.elapsed().as_millis()).unwrap_or(u64::MAX).max(1)
}

/// Whether pictures may be left in their surfaces at all ([`set_enabled`]).
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Leave hardware pictures in their surfaces (on by default; `EFFECTCRAFT_RESIDENT_FRAMES=0`
/// starts with it off), or copy every one out at decode as before Fix 3.
pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// The byte bound of held pictures.
pub fn budget() -> usize {
    BUDGET.load(Ordering::Relaxed)
}

/// Change the byte bound of held pictures (0: none is held; the ones held stay until their frames
/// go).
pub fn set_budget(bytes: usize) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

/// A hardware decode failed while pictures were held: copy pictures out for [`PRESSURE_FOR`].
pub(crate) fn pressure() {
    PRESSURE.store(now_ms(), Ordering::Relaxed);
}

fn pressured() -> bool {
    let at = PRESSURE.load(Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < u64::try_from(PRESSURE_FOR.as_millis()).unwrap_or(u64::MAX)
}

/// Whether the decode about to run on this thread should leave its picture in the surface.
pub(crate) fn wanted() -> bool {
    enabled() && effectcraft_media::hw::resident_allowed() && crate::surfaces::enabled() && !pressured()
}

/// A stand-in frame for `picture` (chroma `chroma`, colour `color`), the picture held until the
/// stand-in is gone; or the picture back when holding it would pass the [`budget`].
pub(crate) fn stand_in(picture: Resident, chroma: Chroma, color: filmcraft_color::ColorInfo) -> Result<VideoFrame, Resident> {
    let (width, height) = (picture.width, picture.height);
    let (w, h) = (width as usize, height as usize);
    let (sx, sy) = chroma.shifts();
    if sx > 1 || sy > 1 {
        return Err(picture);
    }
    let Some(n) = w.checked_mul(h).filter(|n| *n > 0) else { return Err(picture) };
    let Some(cn) = w.div_ceil(1 << sx).checked_mul(h.div_ceil(1 << sy)) else { return Err(picture) };
    // the key: an allocation of its own, shorter than the picture (so it is known for a stand-in)
    let luma: Arc<Vec<u16>> = Arc::new(Vec::with_capacity(1));
    let key = luma.as_ptr() as usize;
    let mut gone;
    let frame = {
        let mut r = registry();
        gone = r.sweep();
        if r.bytes.saturating_add(picture.bytes) > budget() {
            drop(r);
            drop(gone);
            return Err(picture);
        }
        // ballast: the stand-in's byte size is the picture's (luma + Cb in one, Cr, alpha)
        let cb = r.ballast(n.saturating_add(cn));
        let cr = r.ballast(cn);
        let alpha = picture.alpha.then(|| r.ballast(n));
        let bits = picture.bits;
        r.bytes = r.bytes.saturating_add(picture.bytes);
        let picture = Arc::new(picture);
        r.seq = r.seq.saturating_add(1);
        let seq = r.seq;
        if let Some(old) = r.entries.insert(key, Entry { luma: Arc::downgrade(&luma), picture, seq }) {
            // (a dead entry at a reused address)
            r.bytes = r.bytes.saturating_sub(old.picture.bytes);
            gone.push(old.picture);
        }
        // (the new picture is spare until the media pool takes it, and the newest of its decoder)
        r.trim(spare_bound(), &mut gone);
        VideoFrame {
            width,
            height,
            data: PixelData::Yuv16 { planes: [luma, cb, cr], chroma, bits, alpha },
            color,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        }
    };
    // (surfaces are released outside the lock)
    drop(gone);
    KEPT.fetch_add(1, Ordering::Relaxed);
    Ok(frame)
}

/// The key of a stand-in frame (its luma allocation's address).
fn key_of(f: &VideoFrame) -> Option<usize> {
    match &f.data {
        PixelData::Yuv16 { planes, .. } => Some(planes[0].as_ptr() as usize),
        _ => None,
    }
}

/// The picture behind the stand-in frame `f` (`effectcraft_media::hw::ResidentHooks::resolve`).
pub fn resolve(f: &VideoFrame) -> Option<Arc<dyn ResidentFrame>> {
    let PixelData::Yuv16 { planes, .. } = &f.data else { return None };
    let key = key_of(f)?;
    let r = registry();
    let e = r.entries.get(&key)?;
    // (the entry is this stand-in's own: its luma allocation is the one watched)
    let alive = e.luma.upgrade()?;
    if !Arc::ptr_eq(&alive, &planes[0]) {
        return None;
    }
    let p: Arc<dyn ResidentFrame> = e.picture.clone();
    Some(p)
}

/// Let go of pictures whose stand-ins are gone (`effectcraft_media::hw::ResidentHooks::sweep`).
pub fn sweep() {
    let gone = registry().sweep();
    drop(gone);
}

/// Pictures held now and their bytes, after a [`sweep`] (`ResidentHooks::usage`).
pub fn usage() -> (usize, usize) {
    let (n, bytes, gone) = {
        let mut r = registry();
        let gone = r.sweep();
        (r.entries.len(), r.bytes, gone)
    };
    drop(gone);
    (n, bytes)
}

/// The byte bound of spare pictures: an eighth of the [`budget`].
fn spare_bound() -> usize {
    budget() / 8
}

/// Bytes of spare pictures (held only by decoders' caches of recent frames) at the last look:
/// counted in the media pool's share of the cache budget (`ResidentHooks::spare`).
pub fn spare_bytes() -> usize {
    SPARE_BYTES.load(Ordering::Relaxed)
}

/// Let the oldest spare picture go (the cache budget needs room): its bytes, 0 when there is
/// none (`ResidentHooks::shed`). Its frame is decoded again when read.
pub fn shed_spare() -> usize {
    let mut gone = Vec::new();
    let freed = {
        let mut r = registry();
        // (each decoder's newest stays: the media pool may be about to take it)
        let mut newest: HashMap<u64, u64> = HashMap::new();
        for e in r.entries.values() {
            let n = newest.entry(e.picture.decoder).or_default();
            *n = (*n).max(e.seq);
        }
        let oldest = r.entries.iter().filter(|(_, e)| e.spare() && newest.get(&e.picture.decoder) != Some(&e.seq)).min_by_key(|(_, e)| e.seq).map(|(k, _)| *k);
        let freed = oldest.map_or(0, |k| r.take(k, &mut gone));
        r.count_spare();
        freed
    };
    // (released outside the lock)
    drop(gone);
    freed
}

/// Pictures held now (no sweep).
pub(crate) fn held() -> usize {
    registry().entries.len()
}

/// Pictures left in their surface at decode, and pictures copied out of one on demand, so far.
pub fn counters() -> (u64, u64) {
    (KEPT.load(Ordering::Relaxed), COPIED.load(Ordering::Relaxed))
}

/// Spare pictures let go so far ([`shed_spare`], [`SPARE_PER_DECODER`]).
pub fn shed_count() -> u64 {
    SHED.load(Ordering::Relaxed)
}

/// The held picture the GPU asks for (`key.luma`: a stand-in's key), and the depth the frame
/// asked for was rounded to 8 bits from (0: the picture's own samples), for
/// [`crate::surfaces::import`].
pub(crate) fn find(key: &SurfaceKey) -> Option<(Surface, u32)> {
    let r = registry();
    let e = r.entries.get(&key.luma)?;
    if e.luma.strong_count() == 0 {
        return None;
    }
    let p = &e.picture;
    if (p.width, p.height) != (key.width, key.height) || p.subsampling != key.subsampling || p.alpha != key.alpha {
        return None;
    }
    let eight_from = match key.bits {
        b if b == p.bits => 0,
        8 => p.bits,
        _ => return None,
    };
    Some((p.surface.clone(), eight_from))
}
