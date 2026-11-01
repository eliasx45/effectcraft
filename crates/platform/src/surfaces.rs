//! Decoded pictures whose hardware surface is kept for the GPU (HWDEC2: H.264 / HEVC; Fix 2,
//! 1 Nov 2026: ProRes 4:2:2 and 4:4:4 + alpha; safe code).
//!
//! The hardware decoders still copy every picture into the planar samples everything else reads
//! (CPU renders, caches, FilmCraft's GOP cache), and, once the GPU compositor has asked for a
//! surface on a Metal device, also keep the picture's `CVPixelBuffer` here, keyed by its luma
//! plane and layout. The GPU's native YUV path ([`effectcraft_gpu::surface`]) then samples that
//! surface instead of uploading the planes (a 4K ProRes 4444 + alpha frame: 66 MB, about 15 ms).
//! An entry is only found while its frame's luma plane is alive (a weak reference: an address
//! reused by a later allocation never matches a dead entry).
//!
//! Held surfaces are bounded by bytes ([`budget`], [`DEFAULT_BUDGET`]) and by count
//! ([`MAX_SURFACES`]): past either, pictures that are gone leave first, then those the GPU
//! already read (it keeps what it made of them), then the oldest. VideoToolbox allocates a new
//! buffer for each picture while earlier ones are held, so holding never stalls a decoder; if a
//! ProRes decode fails while surfaces are held, [`shed`] lets them all go before the frame is
//! decoded in software, so a pool that ran dry can't turn into a stream given up on. A frame
//! whose surface was let go is uploaded as before.
//!
//! With Settings ▸ Previews ▸ Decode Footage at 8 Bits (Elias's default), the media pool rounds
//! each decoded 12-bit / 10-bit ProRes frame to 8 bits and keeps only that: [`rounded_to_8bit`]
//! (installed as `effectcraft_media::hw`'s hook) hands the surface over to the 8-bit frame, and
//! the GPU rounds the surface's samples to 8 bits exactly as `frame_to_8bit` does, so the 8-bit
//! frame isn't uploaded either.
//!
//! For Fix 3 (frames that stay on the GPU, a CPU copy only on demand): the registry is the one
//! place that owns decoded surfaces and their memory; a frame type carrying its surface would
//! replace the luma-address key and the budget would then cover the frame itself.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use filmcraft_frame::{PixelData, VideoFrame};

use effectcraft_gpu::surface::{ImportedPlanes, SurfaceKey};

use crate::metal_surface::{self, Surface};

/// Surfaces kept at once, at most (also bounded by [`budget`]).
pub const MAX_SURFACES: usize = 64;
/// Bytes of surfaces kept at once by default: about eleven 4K ProRes 4444 + alpha pictures
/// (`s4as`, 66 MB each), 45 at 1080p, or 250 1080p 8-bit H.264 ones (the count bound comes first).
pub const DEFAULT_BUDGET: usize = 768 << 20;

/// Set once the GPU asked for a surface on a Metal device: until then nothing is kept.
static ENABLED: AtomicBool = AtomicBool::new(false);
static BUDGET: AtomicUsize = AtomicUsize::new(DEFAULT_BUDGET);

/// A decoded picture's luma plane (weak).
pub(crate) enum Luma {
    U8(Weak<Vec<u8>>),
    U16(Weak<Vec<u16>>),
}

impl Luma {
    /// The address of the plane's samples while the plane is alive.
    fn address(&self) -> Option<usize> {
        match self {
            Luma::U8(w) => w.upgrade().map(|v| v.as_ptr() as usize),
            Luma::U16(w) => w.upgrade().map(|v| v.as_ptr() as usize),
        }
    }
}

/// A kept picture's layout (what the GPU must ask for to get it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub size: (u32, u32),
    pub bits: u32,
    /// Chroma subsampling shifts (4:2:0 `[1, 1]`, 4:2:2 `[1, 0]`, 4:4:4 `[0, 0]`).
    pub subsampling: [u32; 2],
    /// The picture has alpha (and the surface holds it).
    pub alpha: bool,
    /// The picture is the surface's samples (at this many bits, limited range) rounded to 8
    /// bits (`bits` is 8); 0: the surface's own samples.
    pub eight_from: u32,
}

impl Layout {
    fn matches(&self, k: &SurfaceKey) -> bool {
        self.size == (k.width, k.height) && self.bits == k.bits && self.subsampling == k.subsampling && self.alpha == k.alpha
    }
}

struct Entry {
    luma: Luma,
    address: usize,
    layout: Layout,
    /// The picture's origin in the surface.
    origin: (u32, u32),
    bytes: usize,
    /// The GPU imported it at least once.
    used: bool,
    surface: Surface,
}

#[derive(Default)]
struct Kept {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl Kept {
    /// Take out entry `i`.
    fn take(&mut self, i: usize) -> Option<Entry> {
        let e = self.entries.remove(i)?;
        self.bytes = self.bytes.saturating_sub(e.bytes);
        Some(e)
    }

    /// Let go of entries until `extra` more bytes and one more entry fit: dead ones, then used
    /// ones, then the oldest.
    fn make_room(&mut self, extra: usize, budget: usize, out: &mut Vec<Entry>) {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries.get(i) {
                Some(e) if e.luma.address() != Some(e.address) => out.extend(self.take(i)),
                _ => i += 1,
            }
        }
        let over = |k: &Kept| k.entries.len() >= MAX_SURFACES || k.bytes.saturating_add(extra) > budget;
        while over(self) {
            let Some(i) = self.entries.iter().position(|e| e.used) else { break };
            out.extend(self.take(i));
        }
        while over(self) {
            let Some(e) = self.take(0) else { break };
            out.push(e);
        }
    }
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { entries: VecDeque::new(), bytes: 0 });

fn kept_lock() -> std::sync::MutexGuard<'static, Kept> {
    KEPT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether decoded pictures' surfaces are kept.
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Keep `surface` for the picture with this luma plane (`layout`, at `origin` of the surface).
/// A surface larger than the whole budget is not kept.
/// The 8-bit frame `eight_bit` was made from `full` (`effectcraft_media::hw::EightBitHook`): the
/// surface kept for `full`, if any, now belongs to `eight_bit` (and is no longer found for `full`). Only limited-range
/// pictures of a hardware decoder's own depth (10 or 12 bits, as `frame_to_8bit` rounds them).
pub fn rounded_to_8bit(full: &VideoFrame, eight_bit: &VideoFrame) {
    let (PixelData::Yuv16 { planes: src, bits, .. }, PixelData::Yuv8 { planes: dst, alpha, .. }) = (&full.data, &eight_bit.data) else { return };
    if full.color.range != filmcraft_color::Range::Limited || eight_bit.color.range != full.color.range || !matches!(*bits, 10 | 12) {
        return;
    }
    let address = src[0].as_ptr() as usize;
    let mut k = kept_lock();
    let Some(e) = k.entries.iter_mut().find(|e| e.address == address && e.layout.eight_from == 0 && e.luma.address() == Some(address)) else { return };
    if e.layout.size != (eight_bit.width, eight_bit.height) || e.layout.bits != *bits || e.layout.alpha != alpha.is_some() {
        return;
    }
    let luma = Luma::U8(Arc::downgrade(&dst[0]));
    let Some(to) = luma.address() else { return };
    // (the full frame is about to go: the entry moves to the 8-bit one)
    e.layout = Layout { bits: 8, eight_from: *bits, ..e.layout };
    e.luma = luma;
    e.address = to;
    e.used = false;
}

pub(crate) fn keep(luma: Luma, layout: Layout, origin: (u32, u32), surface: Surface) {
    let Some(address) = luma.address() else { return };
    let bytes = surface.bytes();
    let budget = budget();
    let mut dropped = Vec::new();
    {
        let mut k = kept_lock();
        k.make_room(bytes, budget, &mut dropped);
        if bytes <= budget && k.entries.len() < MAX_SURFACES {
            k.bytes = k.bytes.saturating_add(bytes);
            k.entries.push_back(Entry { luma, address, layout, origin, bytes, used: false, surface });
        } else {
            drop(surface);
        }
    }
    // (released outside the lock)
    drop(dropped);
}

/// The kept surface of the picture `key` describes, marked as read by the GPU.
fn find(key: &SurfaceKey) -> Option<(Surface, (u32, u32), u32)> {
    let mut k = kept_lock();
    let e = k.entries.iter_mut().rev().find(|e| e.address == key.luma && e.layout.matches(key) && e.luma.address() == Some(key.luma))?;
    e.used = true;
    Some((e.surface.clone(), e.origin, e.layout.eight_from))
}

/// Let every kept surface go (a hardware decode failed: maybe its buffer pool ran dry). How many
/// were held.
pub fn shed() -> usize {
    let all = {
        let mut k = kept_lock();
        k.bytes = 0;
        std::mem::take(&mut k.entries)
    };
    all.len()
}

/// Surfaces kept right now.
pub fn kept() -> usize {
    kept_lock().entries.len()
}

/// Bytes of the surfaces kept right now.
pub fn kept_bytes() -> usize {
    kept_lock().bytes
}

/// The byte bound of kept surfaces.
pub fn budget() -> usize {
    BUDGET.load(Ordering::Relaxed)
}

/// Change the byte bound of kept surfaces (0 keeps none). Surfaces beyond it leave with the next
/// picture kept.
pub fn set_budget(bytes: usize) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

/// The importer [`crate::register`] installs ([`effectcraft_gpu::surface::SurfaceImport`]).
pub fn import(device: &wgpu::Device, key: SurfaceKey) -> Option<ImportedPlanes> {
    if !enabled() {
        // the first ask: keep surfaces from now on when this device can share them
        if metal_surface::is_metal(device) {
            ENABLED.store(true, Ordering::Relaxed);
        }
        return None;
    }
    let (surface, origin, eight_from) = find(&key)?;
    match metal_surface::import(device, &surface, key, origin, eight_from) {
        Ok(p) => Some(p),
        Err(why) => {
            log::debug!("decoded surface uploaded instead: {why}");
            None
        }
    }
}
