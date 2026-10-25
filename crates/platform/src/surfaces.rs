//! Decoded H.264 / HEVC pictures whose hardware surface is kept for the GPU (HWDEC2; safe code).
//!
//! The hardware decoder still copies every picture into the planar samples everything else reads
//! (CPU renders, caches, FilmCraft's GOP cache), and, once the GPU compositor has asked for a
//! surface on a Metal device, also keeps the picture's `CVPixelBuffer` here, keyed by its luma
//! plane. The GPU's native YUV path ([`effectcraft_gpu::surface`]) then samples that surface
//! instead of uploading the planes. An entry is only found while its frame's luma plane is alive
//! (a weak reference: an address reused by a later allocation never matches a dead entry), and
//! at most [`MAX_SURFACES`] are kept, oldest out first, so held surfaces stay bounded (about 50 MB
//! of 1080p 8-bit pictures); a frame whose surface was let go is uploaded as before.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, Weak};

use effectcraft_gpu::surface::ImportedPlanes;

use crate::metal_surface::{self, Surface};

/// Surfaces kept at once.
pub const MAX_SURFACES: usize = 16;

/// Set once the GPU asked for a surface on a Metal device: until then nothing is kept.
static ENABLED: AtomicBool = AtomicBool::new(false);

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

struct Entry {
    luma: Luma,
    address: usize,
    size: (u32, u32),
    bits: u32,
    /// The picture's origin in the surface.
    origin: (u32, u32),
    surface: Surface,
}

static KEPT: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());

/// Whether decoded pictures' surfaces are kept.
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Keep `surface` for the picture with this luma plane (`size`, `bits`, at `origin` of the
/// surface).
pub(crate) fn keep(luma: Luma, size: (u32, u32), bits: u32, origin: (u32, u32), surface: Surface) {
    let Some(address) = luma.address() else { return };
    let dropped: Vec<Entry> = {
        let mut k = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
        let mut dropped = Vec::new();
        // forget pictures that are gone, then the oldest beyond the bound
        let (alive, dead): (VecDeque<Entry>, VecDeque<Entry>) = std::mem::take(&mut *k).into_iter().partition(|e| e.luma.address() == Some(e.address));
        *k = alive;
        dropped.extend(dead);
        k.push_back(Entry { luma, address, size, bits, origin, surface });
        while k.len() > MAX_SURFACES {
            if let Some(e) = k.pop_front() {
                dropped.push(e);
            }
        }
        dropped
    };
    // (released outside the lock)
    drop(dropped);
}

/// The kept surface of the picture whose luma samples start at `address`.
fn find(address: usize, size: (u32, u32), bits: u32) -> Option<(Surface, (u32, u32))> {
    let k = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    k.iter()
        .rev()
        .find(|e| e.address == address && e.size == size && e.bits == bits && e.luma.address() == Some(address))
        .map(|e| (e.surface.clone(), e.origin))
}

/// Surfaces kept right now.
pub fn kept() -> usize {
    KEPT.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// The importer [`crate::register`] installs ([`effectcraft_gpu::surface::SurfaceImport`]).
pub fn import(device: &wgpu::Device, luma: usize, width: u32, height: u32, bits: u32) -> Option<ImportedPlanes> {
    if !enabled() {
        // the first ask: keep surfaces from now on when this device can share them
        if metal_surface::is_metal(device) {
            ENABLED.store(true, Ordering::Relaxed);
        }
        return None;
    }
    let (surface, origin) = find(luma, (width, height), bits)?;
    match metal_surface::import(device, &surface, width, height, bits, origin) {
        Ok(p) => Some(p),
        Err(why) => {
            log::debug!("decoded surface uploaded instead: {why}");
            None
        }
    }
}
