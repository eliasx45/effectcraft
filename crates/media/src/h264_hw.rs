//! A hook for a hardware H.264 encoder (opt-in, for fast exports).
//!
//! As with [`crate::prores_enc`], FilmCraft has no encoder registry, so a platform layer
//! (`effectcraft-platform` on macOS: VideoToolbox, the media engines of Apple silicon)
//! [`register`]s one function that opens an encoding session. The movie exporter uses it only
//! when asked for (`effectcraft_export::speed`, `effectcraft-cli render --hw-h264` / `--fast`):
//! its bits differ from FilmCraft's pure-Rust encoder (Apple's own rate control and mode
//! decisions), and it exists only on some systems. When no session opens, or the first frames
//! fail, the exporter encodes the whole movie in software instead.
//!
//! Elias approved hardware H.264 encoding for exports on 6 Nov 2026 ("E2b", export speed); his
//! 12 Oct "no hardware H.264" decision was about footage decoding.

use std::sync::RwLock;

/// What a session encodes: `width`×`height` 8-bit 4:2:0 BT.709 limited-range pictures at
/// `fps_num / fps_den` frames per second, about `kbps` kilobits per second on average, a key
/// frame at least every `keyint` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H264Config {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub kbps: u32,
    pub keyint: u32,
}

/// One coded picture: an MP4 sample (NAL units, each after a 4-byte big-endian length), whether
/// it is a sync sample (IDR), and the index of the picture it codes (the `index` it went in with).
/// Sessions code pictures in presentation order (no B-frame reordering), so samples come out in
/// the order the pictures went in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264Sample {
    pub data: Vec<u8>,
    pub key: bool,
    pub index: u64,
}

/// An open hardware encoding session.
pub trait H264Session: Send {
    /// Encode picture `index` (planes `y` `w`×`h`, `u` and `v` `⌈w/2⌉`×`⌈h/2⌉`, no padding).
    /// Returns the samples finished so far, in order (possibly none yet).
    fn encode(&mut self, y: &[u8], u: &[u8], v: &[u8], index: u64) -> Result<Vec<H264Sample>, String>;
    /// Finish every picture handed in so far and return their samples. The session stays usable.
    fn complete(&mut self) -> Result<Vec<H264Sample>, String>;
    /// The stream's sequence and picture parameter sets (NAL units without length or start
    /// code), once the first sample has come out.
    fn parameter_sets(&self) -> Option<(Vec<u8>, Vec<u8>)>;
}

/// Opens a session for a configuration, or says why not (no hardware encoder, a size it doesn't
/// take…).
pub type OpenH264 = fn(&H264Config) -> Result<Box<dyn H264Session>, String>;

static OPEN: RwLock<Option<OpenH264>> = RwLock::new(None);

/// Install the platform's hardware H.264 encoder (call once at startup; a later call replaces it).
pub fn register(f: OpenH264) {
    *OPEN.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
}

/// Whether a hardware H.264 encoder is registered.
pub fn registered() -> bool {
    OPEN.read().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
}

/// A session for `cfg` on the registered encoder; an error when none is registered or it declines.
pub fn open(cfg: &H264Config) -> Result<Box<dyn H264Session>, String> {
    let f = (*OPEN.read().unwrap_or_else(std::sync::PoisonError::into_inner)).ok_or("no hardware H.264 encoder on this system")?;
    f(cfg)
}

/// Whether an MP4 sample (4-byte length-prefixed NAL units) holds an IDR picture (NAL type 5).
/// A malformed length ends the scan.
pub fn has_idr(sample: &[u8]) -> bool {
    let mut rest = sample;
    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
        let n = u32::from_be_bytes(*len) as usize;
        let Some(nal) = tail.get(..n) else { return false };
        if nal.first().is_some_and(|h| h & 0x1f == 5) {
            return true;
        }
        rest = tail.get(n..).unwrap_or_default();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_samples_are_found_by_their_nal_type() {
        // SEI (6) then IDR slice (5)
        let idr = [&[0, 0, 0, 2][..], &[0x06, 0xff], &[0, 0, 0, 3], &[0x65, 0x88, 0x80]].concat();
        assert!(has_idr(&idr));
        // a non-IDR slice (1)
        assert!(!has_idr(&[0, 0, 0, 2, 0x41, 0x9a]));
        // a length running past the end, an empty sample
        assert!(!has_idr(&[0, 0, 0, 9, 0x65]));
        assert!(!has_idr(&[]));
    }

    #[test]
    fn nothing_registered_is_an_error() {
        // (no test registers an encoder in this binary)
        if !registered() {
            let cfg = H264Config { width: 64, height: 64, fps_num: 30, fps_den: 1, kbps: 1000, keyint: 60 };
            assert!(open(&cfg).is_err());
        }
    }
}
