//! Who may use a hardware video decoder.
//!
//! Hardware decoders (registered by a platform layer with FilmCraft's decoder registry: ProRes,
//! H.264 and HEVC on macOS) give pictures that look the same as the pure-Rust decoders' but are
//! not guaranteed bit-identical, so only previews may use them: exports, the Render Queue and
//! other final renders decode exactly.
//! (Exception, opt-in: a draft / fast export that asked for hardware decoding reads the pool's
//! preview view: `effectcraft_export::speed::hardware_decode`, `effectcraft-cli render
//! --hw-decode` / `--fast`.)
//!
//! FilmCraft creates decoders through one process-wide registry, so the choice is made per
//! decoding thread: [`MediaPool`](crate::MediaPool) runs every movie decode inside a [`scope`]
//! saying whether that read is a preview read, and a hardware decoder factory declines (the
//! software decoder is used) unless [`allowed`] says yes. A scope always sets the flag (off as
//! well as on) and restores the previous value when dropped, so a decode nested inside another
//! one on the same thread (a rayon worker that picked up another job while waiting) sees its own
//! consumer's choice. Threads outside any scope decode exactly.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

thread_local! {
    static ALLOWED: Cell<bool> = const { Cell::new(false) };
}

/// Set once a hardware decoder factory is registered: until then the pool keeps one cache for
/// previews and exports (nothing would differ).
static AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Whether the decode running on this thread may use a hardware decoder.
pub fn allowed() -> bool {
    ALLOWED.with(Cell::get)
}

/// Restores the previous [`allowed`] value of this thread when dropped.
#[must_use = "the flag is restored when the scope is dropped"]
pub struct Scope {
    prev: bool,
}

impl Drop for Scope {
    fn drop(&mut self) {
        ALLOWED.with(|a| a.set(self.prev));
    }
}

/// Decodes on this thread may (`on`) or may not use a hardware decoder until the returned scope
/// is dropped.
pub fn scope(on: bool) -> Scope {
    Scope { prev: ALLOWED.with(|a| a.replace(on)) }
}

/// A platform layer registered a hardware decoder (see [`available`]).
pub fn set_available(on: bool) {
    AVAILABLE.store(on, Ordering::Relaxed);
}

/// Whether a hardware decoder was registered.
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// Told when a preview read rounds a decoded frame to 8 bits (`full` to `eight_bit`, Settings ▸
/// Previews ▸ Decode Footage at 8 Bits): a platform layer that keeps the hardware decoder's
/// surface of `full` for the GPU hands it over to `eight_bit`, so the GPU reads the 8-bit frame
/// from the surface too instead of uploading its planes (Fix 2, 1 Nov 2026).
pub type EightBitHook = fn(full: &filmcraft_frame::VideoFrame, eight_bit: &filmcraft_frame::VideoFrame);

static EIGHT_BIT: OnceLock<EightBitHook> = OnceLock::new();

/// Install the platform's [`EightBitHook`] (once; later calls are ignored).
pub fn set_eight_bit_hook(f: EightBitHook) {
    let _ = EIGHT_BIT.set(f);
}

/// The 8-bit frame a preview read at 8 bits keeps of `f`, made as the media pool makes it (and
/// the [`EightBitHook`] told); `None` when `f` is 8-bit already. (Tests and benchmarks of the
/// platform's surface handover.)
#[doc(hidden)]
pub fn round_to_8bit(f: &filmcraft_frame::VideoFrame) -> Option<filmcraft_frame::VideoFrame> {
    let e = crate::convert::frame_to_8bit(f)?;
    rounded_to_8bit(f, &e);
    Some(e)
}

/// `full` was rounded to `eight_bit` ([`EightBitHook`]).
pub(crate) fn rounded_to_8bit(full: &filmcraft_frame::VideoFrame, eight_bit: &filmcraft_frame::VideoFrame) {
    if let Some(f) = EIGHT_BIT.get() {
        f(full, eight_bit);
    }
}

// ---------------------------------------------------------------------------------------------
// Fix 3 (1 Nov 2026): frames kept on the GPU, their CPU copy made on demand.
// ---------------------------------------------------------------------------------------------

thread_local! {
    static RESIDENT: Cell<bool> = const { Cell::new(false) };
}

/// Whether a hardware decode running on this thread may leave its picture in the decoder's
/// surface ([`ResidentFrame`]) instead of copying it out: only inside the media pool's own reads
/// of a movie it keeps open (see [`resident_scope`]); everything else (proxies, thumbnails of
/// files outside the project, exports, which never decode on the hardware anyway) gets planes.
pub fn resident_allowed() -> bool {
    RESIDENT.with(Cell::get)
}

/// Restores the previous [`resident_allowed`] value of this thread when dropped.
#[must_use = "the flag is restored when the scope is dropped"]
pub(crate) struct ResidentScope {
    prev: bool,
}

impl Drop for ResidentScope {
    fn drop(&mut self) {
        RESIDENT.with(|a| a.set(self.prev));
    }
}

/// Hardware decodes on this thread may (`on`) or may not leave pictures in their surface until
/// the scope is dropped.
pub(crate) fn resident_scope(on: bool) -> ResidentScope {
    ResidentScope { prev: RESIDENT.with(|a| a.replace(on)) }
}

/// Planar samples in the layout of FilmCraft's decoders (`u16` planes, `bits` significant bits),
/// for a [`ResidentFrame`] to copy itself into.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Planes16 {
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
    pub alpha: Option<Vec<u16>>,
}

/// A decoded picture the hardware decoder left in its surface (Fix 3): the GPU reads it there
/// (`effectcraft_gpu::surface`, keyed by its stand-in frame's luma allocation), and a CPU
/// reader asks for its samples, which are copied out then.
///
/// The decoder hands FilmCraft a *stand-in* frame for it: the picture's size, layout and
/// colour, with planes that don't hold its samples (a short luma plane, the allocation that keys
/// the surface, and shared zero planes so the frame's byte size is the picture's and FilmCraft's
/// GOP cache keeps as many as before). [`resident_of`] finds the picture behind a stand-in; the
/// media pool never lets a stand-in's planes be read ([`is_stand_in`]).
pub trait ResidentFrame: Send + Sync {
    /// The picture's samples, as the decoder would have copied them, written into `reuse`'s
    /// buffers where their lengths fit (the media pool recycles them; every sample is
    /// overwritten).
    fn planes(&self, reuse: Planes16) -> std::result::Result<Planes16, String>;
    /// Bytes the surface holds.
    fn bytes(&self) -> usize;
}

/// What a platform layer that leaves hardware pictures in their surfaces installs.
#[derive(Clone, Copy)]
pub struct ResidentHooks {
    /// The picture behind a stand-in frame (`None`: not a stand-in, or one whose picture is gone).
    pub resolve: fn(&filmcraft_frame::VideoFrame) -> Option<Arc<dyn ResidentFrame>>,
    /// Let go of the pictures whose stand-ins are all gone (after a purge).
    pub sweep: fn(),
    /// Pictures held in surfaces now, and their bytes (cached or not).
    pub usage: fn() -> (usize, usize),
    /// Bytes of *spare* pictures: held only by decoders' caches of recent frames (no frame of the
    /// pool has them). The pool counts them in its share of the cache budget (Fix 3 follow-up,
    /// 1 Nov 2026: GPU memory must not sit outside the budget).
    pub spare: fn() -> usize,
    /// Let the oldest spare picture go: its bytes (0: none). Its frame is decoded again when read.
    pub shed: fn() -> usize,
}

static RESIDENT_HOOKS: OnceLock<ResidentHooks> = OnceLock::new();

/// Install the platform's [`ResidentHooks`] (once; later calls are ignored).
pub fn set_resident_hooks(h: ResidentHooks) {
    let _ = RESIDENT_HOOKS.set(h);
}

/// The picture behind the stand-in frame `f` ([`ResidentFrame`]).
pub(crate) fn resident_of(f: &filmcraft_frame::VideoFrame) -> Option<Arc<dyn ResidentFrame>> {
    (RESIDENT_HOOKS.get()?.resolve)(f)
}

/// Let go of pictures nothing refers to any more ([`ResidentHooks::sweep`]).
pub(crate) fn sweep_resident() {
    if let Some(h) = RESIDENT_HOOKS.get() {
        (h.sweep)();
    }
}

/// Bytes of spare pictures ([`ResidentHooks::spare`]).
pub(crate) fn spare_resident() -> usize {
    RESIDENT_HOOKS.get().map_or(0, |h| (h.spare)())
}

/// Let the oldest spare picture go ([`ResidentHooks::shed`]): its bytes.
pub(crate) fn shed_resident() -> usize {
    RESIDENT_HOOKS.get().map_or(0, |h| (h.shed)())
}

/// Pictures kept in decoder surfaces now and their bytes ([`ResidentHooks::usage`]).
pub fn resident_usage() -> (usize, usize) {
    RESIDENT_HOOKS.get().map_or((0, 0), |h| (h.usage)())
}

/// Whether `f` is a stand-in ([`ResidentFrame`]): planar Y'CbCr whose luma plane is shorter than
/// the picture. No decoder makes such a frame otherwise; its planes must never be read.
pub(crate) fn is_stand_in(f: &filmcraft_frame::VideoFrame) -> bool {
    let n = (f.width as usize).saturating_mul(f.height as usize);
    match &f.data {
        filmcraft_frame::PixelData::Yuv16 { planes, .. } => planes[0].len() < n,
        filmcraft_frame::PixelData::Yuv8 { planes, .. } => planes[0].len() < n,
        _ => false,
    }
}

/// Whether footage with this codec description ([`effectcraft_project::Footage::codec`]) is one
/// a registered hardware decoder may take: Apple ProRes, H.264 and HEVC (FilmCraft's labels,
/// e.g. "Apple ProRes 4444", "H.264 (High)", "HEVC"). The decoder factory itself still declines
/// streams it doesn't take (10-bit H.264, 4:2:2, interlaced, …), which then decode in software.
///
/// An empty description (footage saved by older versions, or never probed) is unknown, not "no":
/// the factory looks at the stream itself (HWDEC3: a 6K H.264 clip in the owner's project had an
/// empty codec and always decoded in software).
pub fn takes_codec(codec: &str) -> bool {
    let c = codec.trim().to_ascii_lowercase();
    c.is_empty() || ["prores", "h.264", "h264", "avc", "hevc", "h.265", "h265"].iter().any(|k| c.contains(k))
}

/// Coded sizes of streams the hardware decoder refused or could not take (noted by the
/// platform layer's factory), for the automatic proxy suggestion.
static REFUSED: std::sync::Mutex<std::collections::BTreeSet<(u32, u32)>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

/// The platform layer's hardware decoder refused (or does not take) a stream of this coded size.
pub fn note_refused(coded_w: u32, coded_h: u32) {
    if let Ok(mut r) = REFUSED.lock()
        && r.len() < 256
    {
        r.insert((coded_w, coded_h));
    }
}

/// Whether previews of `codec` footage of `width`×`height` decode in software although a hardware
/// decoder is registered: a stream of that size was refused ([`note_refused`]; coded sizes are
/// rounded up to whole macroblocks), or it is H.264 beyond what VideoToolbox's H.264 decoder
/// takes (4096×2304: the 6K "Smoke" clip, Level 6.0, is refused).
pub fn refuses(codec: &str, width: u32, height: u32) -> bool {
    if !available() || width == 0 || height == 0 {
        return false;
    }
    let close = |coded: u32, n: u32| coded >= n && coded - n < 64;
    if REFUSED.lock().map(|r| r.iter().any(|&(w, h)| close(w, width) && close(h, height))).unwrap_or(false) {
        return true;
    }
    let c = codec.trim().to_ascii_lowercase();
    let h264 = ["h.264", "h264", "avc"].iter().any(|k| c.contains(k));
    h264 && (width.max(height) > 4096 || width.min(height) > 2304)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_nest_and_restore() {
        assert!(!allowed(), "threads outside any scope decode exactly");
        {
            let _a = scope(true);
            assert!(allowed());
            {
                let _b = scope(false);
                assert!(!allowed(), "a nested exact read is exact");
            }
            assert!(allowed());
        }
        assert!(!allowed());
        assert!(takes_codec("Apple ProRes 4444") && takes_codec("H.264 (High)") && takes_codec("HEVC") && takes_codec("H.265"));
        assert!(!takes_codec("VP9") && !takes_codec("AV1") && !takes_codec("DNxHD") && !takes_codec("mjpeg"));
        assert!(takes_codec("") && takes_codec("  "), "unknown codecs are left to the decoder factory");
    }
}
