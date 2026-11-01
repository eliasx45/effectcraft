//! ProRes encoding with an optional hardware encoder in front of FilmCraft's software one.
//!
//! FilmCraft has a decoder registry but no encoder registry, so this is the smallest equivalent:
//! a platform layer (`effectcraft-platform` on macOS: VideoToolbox, Apple silicon's ProRes
//! engines) [`register`]s one hardware encode function at startup, and every ProRes writer
//! (movie export, pre-renders, preview proxies) encodes through [`ProResEncoder`], which tries
//! that function first and uses FilmCraft's pure-Rust encoder when there is none, when it
//! declines or fails a frame, or when what it returns doesn't match what was asked for (frame
//! size, chroma format, alpha, colour tags). Both write ordinary ProRes frames, so a movie may mix
//! them and the QuickTime sample entry stays the same.
//!
//! Hardware frames are not bit-identical to the software encoder's (a different quantiser
//! choice, Apple's own rate control). Elias approved that for exports (1 Nov 2026, "Fix 4"):
//! encoding is lossy either way, and decoding stays exact (`crate::hw`).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use filmcraft_prores::header::FrameHeader;
use filmcraft_prores::{AlphaType, ChromaFormat, EncoderConfig, Frame};

/// A hardware encoder: encodes `frame` with `cfg` (profile, size, colour tags, alpha) into one
/// ProRes frame (`frame size`, `'icpf'`, frame header, pictures), or says why not. It may
/// decline anything it doesn't take (an `Err`); the software encoder then encodes that frame.
/// It must be callable from many threads at once (callers encode frames in parallel).
pub type HardwareEncode = fn(&EncoderConfig, &Frame) -> Result<Vec<u8>, String>;

static HARDWARE: RwLock<Option<HardwareEncode>> = RwLock::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);
static HW_FRAMES: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// A failed hardware frame is encoded in software; after this many failures in a row an encoder
/// stops asking the hardware (a stream it rejects would otherwise cost a failed attempt a frame).
const GIVE_UP_AFTER: u32 = 3;

/// Install the platform's hardware ProRes encoder (call once at startup; a later call replaces it).
pub fn register(f: HardwareEncode) {
    *HARDWARE.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
}

/// Allow (`true`, the default) or forbid the hardware encoder process-wide (benchmarks, tests,
/// a user who wants the software encoder's exact output).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether a hardware encoder is registered and allowed.
pub fn hardware_available() -> bool {
    ENABLED.load(Ordering::Relaxed) && hardware().is_some()
}

fn hardware() -> Option<HardwareEncode> {
    *HARDWARE.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Process-wide counters: frames the hardware encoded, and frames it was asked for but the
/// software encoder wrote (declined, failed or inconsistent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub hardware_frames: u64,
    pub fallbacks: u64,
}

/// The counters so far.
pub fn stats() -> Stats {
    Stats { hardware_frames: HW_FRAMES.load(Ordering::Relaxed), fallbacks: FALLBACKS.load(Ordering::Relaxed) }
}

/// A ProRes encoder for one stream: the registered hardware encoder when it takes the frames,
/// else FilmCraft's software encoder. Cheap to clone (one per worker thread is fine); clones
/// share their failure count, so a stream the hardware rejects stops asking it on every thread.
#[derive(Clone, Debug)]
pub struct ProResEncoder {
    sw: filmcraft_prores::Encoder,
    /// Hardware failures in a row; at [`GIVE_UP_AFTER`] this encoder stays in software.
    failures: Arc<AtomicU32>,
    hardware: bool,
}

impl ProResEncoder {
    /// An encoder for `cfg`, using hardware when available.
    ///
    /// The hardware encoder keeps Apple's own data rate (VideoToolbox has no rate setting for
    /// ProRes), so a configuration asking for less than the profile's nominal rate stays in
    /// software: preview proxies of sparse keyed clips are coded at the source's own density so
    /// they decode faster, and a hardware proxy of `p1080_4444a.mov` was 373 KB a frame where the
    /// software one at the source's rate was 200 KB (1 Nov 2026).
    pub fn new(cfg: EncoderConfig) -> ProResEncoder {
        let hardware = cfg.bits_per_mb.is_none_or(|b| b >= cfg.profile.nominal_bits_per_mb());
        ProResEncoder { sw: filmcraft_prores::Encoder::with_config(cfg), failures: Arc::new(AtomicU32::new(0)), hardware }
    }

    /// The same, never using the hardware encoder (exact, reproducible output).
    pub fn software(cfg: EncoderConfig) -> ProResEncoder {
        ProResEncoder { hardware: false, ..ProResEncoder::new(cfg) }
    }

    pub fn config(&self) -> &EncoderConfig {
        self.sw.config()
    }

    /// Whether the next frame will be offered to the hardware encoder.
    pub fn tries_hardware(&self) -> bool {
        self.hardware && self.failures.load(Ordering::Relaxed) < GIVE_UP_AFTER && hardware_available()
    }

    /// Encode one frame (any bit depth 8..=16; chroma matching the profile).
    pub fn encode(&mut self, frame: &Frame) -> filmcraft_prores::Result<Vec<u8>> {
        if self.tries_hardware()
            && let Some(hw) = hardware()
        {
            let r = hw(self.sw.config(), frame).and_then(|data| consistent(self.sw.config(), frame, &data).map(|()| data));
            match r {
                Ok(data) => {
                    self.failures.store(0, Ordering::Relaxed);
                    HW_FRAMES.fetch_add(1, Ordering::Relaxed);
                    return Ok(data);
                }
                Err(e) => {
                    let n = self.failures.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                    FALLBACKS.fetch_add(1, Ordering::Relaxed);
                    if n == 1 || n == GIVE_UP_AFTER {
                        log::info!("hardware ProRes encode fell back to software: {e}");
                    }
                }
            }
        }
        self.sw.encode(frame)
    }
}

/// Whether a hardware-encoded frame is what the software encoder would have described: one whole
/// ProRes frame of the configured size, chroma format, alpha and colour tags, progressive.
pub fn consistent(cfg: &EncoderConfig, frame: &Frame, data: &[u8]) -> Result<(), String> {
    let (h, _, size) = FrameHeader::parse(data).map_err(|e| format!("hardware frame: {e}"))?;
    if size != data.len() {
        return Err(format!("hardware frame is {} bytes but says {size}", data.len()));
    }
    if (u32::from(h.width), u32::from(h.height)) != (cfg.width, cfg.height) {
        return Err(format!("hardware frame is {}x{}, not {}x{}", h.width, h.height, cfg.width, cfg.height));
    }
    if h.chroma != cfg.profile.chroma() || h.interlace != filmcraft_prores::Interlace::Progressive {
        return Err(format!("hardware frame is {:?} {:?}", h.chroma, h.interlace));
    }
    let alpha = cfg.encode_alpha && cfg.profile.chroma() == ChromaFormat::Yuv444 && frame.alpha.is_some();
    if (h.alpha != AlphaType::None) != alpha {
        return Err(format!("hardware frame alpha {:?}, wanted {}", h.alpha, if alpha { "alpha" } else { "none" }));
    }
    if h.color != cfg.color {
        return Err(format!("hardware frame colour {:?}, wanted {:?}", h.color, cfg.color));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_prores::Profile;
    use std::sync::Mutex;

    /// Tests that change the process-wide registration run one at a time.
    static LOCK: Mutex<()> = Mutex::new(());

    fn frame(alpha: bool) -> (EncoderConfig, Frame) {
        let mut cfg = EncoderConfig::new(Profile::P4444, 64, 32);
        cfg.encode_alpha = alpha;
        let mut f = Frame::new(64, 32, ChromaFormat::Yuv444, 12, alpha);
        for (k, y) in f.y.iter_mut().enumerate() {
            *y = (256 + (k as u32 * 37) % 3500) as u16;
        }
        (cfg, f)
    }

    fn failing(_: &EncoderConfig, _: &Frame) -> Result<Vec<u8>, String> {
        Err("no hardware session".into())
    }

    /// A "hardware" encoder that is really the software one: consistent output.
    fn good(cfg: &EncoderConfig, f: &Frame) -> Result<Vec<u8>, String> {
        let mut c = cfg.clone();
        c.encoder_id = *b"test";
        filmcraft_prores::Encoder::with_config(c).encode(f).map_err(|e| e.to_string())
    }

    /// One that drops the alpha channel it was asked to keep.
    fn no_alpha(cfg: &EncoderConfig, f: &Frame) -> Result<Vec<u8>, String> {
        let mut c = cfg.clone();
        c.encode_alpha = false;
        filmcraft_prores::Encoder::with_config(c).encode(f).map_err(|e| e.to_string())
    }

    fn decodes(data: &[u8], alpha: bool) {
        let f = filmcraft_prores::decode_frame(data).unwrap();
        assert_eq!((f.width, f.height), (64, 32));
        assert_eq!(f.alpha.is_some(), alpha);
    }

    #[test]
    fn a_failing_hardware_encoder_falls_back_to_software() {
        let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        register(failing);
        let (cfg, f) = frame(true);
        let mut e = ProResEncoder::new(cfg.clone());
        let before = stats();
        for _ in 0..GIVE_UP_AFTER + 2 {
            let data = e.encode(&f).unwrap();
            decodes(&data, true);
            assert_eq!(data, filmcraft_prores::Encoder::with_config(cfg.clone()).encode(&f).unwrap(), "the software encoder's frame");
        }
        assert!(!e.tries_hardware(), "gives up after {GIVE_UP_AFTER} failures in a row");
        assert!(stats().fallbacks - before.fallbacks >= u64::from(GIVE_UP_AFTER));
    }

    #[test]
    fn hardware_frames_are_used_when_consistent_and_refused_when_not() {
        let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (cfg, f) = frame(true);
        register(good);
        let data = ProResEncoder::new(cfg.clone()).encode(&f).unwrap();
        assert_eq!(&data[12..16], b"test", "the hardware frame was written");
        decodes(&data, true);
        // software-only encoders and a disabled hardware encoder never ask it
        assert_ne!(&ProResEncoder::software(cfg.clone()).encode(&f).unwrap()[12..16], b"test");
        set_enabled(false);
        assert_ne!(&ProResEncoder::new(cfg.clone()).encode(&f).unwrap()[12..16], b"test");
        set_enabled(true);
        // a frame that lost its alpha is refused: the software frame (with alpha) is written
        register(no_alpha);
        let data = ProResEncoder::new(cfg.clone()).encode(&f).unwrap();
        decodes(&data, true);
        assert!(consistent(&cfg, &f, &data).is_ok());
        assert!(consistent(&cfg, &f, &no_alpha(&cfg, &f).unwrap()).is_err());
        assert!(consistent(&cfg, &f, &data[..data.len() - 1]).is_err(), "a truncated frame");
        let mut other = cfg.clone();
        other.color.primaries = 9;
        assert!(consistent(&other, &f, &data).is_err(), "other colour tags");
        register(failing);
    }

    #[test]
    fn a_lower_data_rate_than_nominal_stays_in_software() {
        let (mut cfg, _) = frame(true);
        assert!(ProResEncoder::new(cfg.clone()).hardware);
        cfg.bits_per_mb = Some(cfg.profile.nominal_bits_per_mb());
        assert!(ProResEncoder::new(cfg.clone()).hardware, "the nominal rate is the hardware's own");
        cfg.bits_per_mb = Some(cfg.profile.nominal_bits_per_mb() / 3);
        assert!(!ProResEncoder::new(cfg).hardware, "a sparse proxy's rate");
    }
}
