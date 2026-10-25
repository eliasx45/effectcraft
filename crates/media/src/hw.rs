//! Who may use a hardware video decoder.
//!
//! Hardware decoders (registered by a platform layer with FilmCraft's decoder registry: ProRes,
//! H.264 and HEVC on macOS) give pictures that look the same as the pure-Rust decoders' but are
//! not guaranteed bit-identical, so only previews may use them: exports, the Render Queue and
//! other final renders decode exactly.
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
