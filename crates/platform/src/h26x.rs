//! The VideoToolbox H.264 / HEVC decoder behind FilmCraft's decoder registry (safe code; macOS).
//!
//! [`factory`] makes a [`HybridDecoder`] around a [`VtNalDecoder`] for an `avcC` / `hvcC` sample
//! entry when the decode runs in a preview read and the stream is one the hardware path takes
//! ([`crate::nal::select`]); anything else goes to FilmCraft's software decoders. The hardware
//! decoder is stateful (reference pictures), so each decoder owns its own session: unlike ProRes,
//! there is no session pool.
//!
//! VideoToolbox emits pictures in decoding order; a reorder buffer of the stream's own depth
//! (`max_num_reorder_frames` / `sps_max_num_reorder_pics`) puts them in presentation order, as the
//! software decoders do, with the pts each sample was fed with. A run that starts at an HEVC CRA /
//! BLA leaves out its RASL pictures, as the software decoder does. [`VideoDecoder::reset`] (a
//! seek) starts a fresh session, so no reference picture survives a seek.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use filmcraft_codecs::{CodecError, DecodedFrame, Result, VideoDecoder};
use filmcraft_isobmff::SampleEntry;

use crate::hybrid::HybridDecoder;
use crate::nal::{self, Decline, NalCodec, NalStream};
use crate::videotoolbox_nal::{self, Session};

/// Access units decoding at once (asynchronous decompression overlaps decoding a picture with
/// copying out the previous one).
const MAX_IN_FLIGHT: usize = 2;

/// A stream layout whose session could not be made is not tried again for this long (HWDEC3: the
/// M1's H.264 engine refuses 6K, and every decoder FilmCraft makes for such a clip, one per seek
/// run and per nested request, would otherwise pay a failed session creation first).
const RETRY: Duration = Duration::from_secs(60);

/// Layouts (codec, coded size, bit depth) VideoToolbox refused, and when.
static REFUSED: LazyLock<Mutex<HashMap<(NalCodec, (u32, u32), u32), Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether this system has a hardware decoder for H.264 or HEVC.
pub fn hardware_available() -> bool {
    videotoolbox_nal::hardware_decode_supported(NalCodec::H264) || videotoolbox_nal::hardware_decode_supported(NalCodec::Hevc)
}

/// The factory [`crate::register`] registers: a hardware decoder (with a software fallback) for
/// H.264 / HEVC sample entries in preview reads, `None` (the software decoder) otherwise.
pub fn factory(e: &SampleEntry) -> Option<Result<Box<dyn VideoDecoder>>> {
    let stream = match nal::select(e, effectcraft_media::hw::allowed(), videotoolbox_nal::hardware_decode_supported) {
        Ok(s) => s,
        Err(Decline::Unsupported(why)) => {
            log::debug!("VideoToolbox {}: not used: {why}", e.codec.name());
            return None;
        }
        Err(_) => return None,
    };
    let layout = (stream.codec, stream.layout.coded, stream.layout.bits);
    if REFUSED.lock().unwrap_or_else(PoisonError::into_inner).get(&layout).is_some_and(|at| at.elapsed() < RETRY) {
        return None;
    }
    match VtNalDecoder::new(stream.clone()) {
        Ok(hw) => {
            crate::DECODERS.fetch_add(1, Ordering::Relaxed);
            Some(Ok(Box::new(HybridDecoder::new(Box::new(hw), stream))))
        }
        Err(why) => {
            log::info!("VideoToolbox {} {}x{}: decoding in software: {why}", e.codec.name(), layout.1.0, layout.1.1);
            REFUSED.lock().unwrap_or_else(PoisonError::into_inner).insert(layout, Instant::now());
            None
        }
    }
}

/// The VideoToolbox decoder for one `avcC` / `hvcC` stream. Errors are the caller's to answer
/// ([`HybridDecoder`] switches to the software decoder).
pub struct VtNalDecoder {
    stream: NalStream,
    /// `None` after a reset until the next sample (a fresh session per decoding run).
    session: Option<Session>,
    /// Decoded pictures waiting for presentation order (sorted by pts).
    pending: Vec<DecodedFrame>,
    /// Presentation times of submitted access units whose picture has not arrived yet.
    in_flight: Vec<i64>,
    /// The run started at a CRA / BLA picture: its RASL pictures are left out.
    skip_rasl: bool,
    first: bool,
    /// Test hook: fail every decode once this many samples were fed.
    fail_after: Option<u64>,
    fed: u64,
    name: &'static str,
}

impl VtNalDecoder {
    /// A decoder for the stream, or why VideoToolbox does not take it (no hardware decoder for
    /// its profile, level or size: the session is made now, so those are declined up front).
    pub fn new(stream: NalStream) -> std::result::Result<Self, String> {
        let session = Session::new(&stream)?;
        let name = match stream.codec {
            NalCodec::H264 => "VideoToolbox H.264",
            NalCodec::Hevc => "VideoToolbox HEVC",
        };
        Ok(Self { stream, session: Some(session), pending: Vec::new(), in_flight: Vec::new(), skip_rasl: false, first: true, fail_after: None, fed: 0, name })
    }

    /// Test hook: from the `n`-th sample on, every decode fails as a lost session would.
    #[doc(hidden)]
    pub fn fail_after(&mut self, n: u64) {
        self.fail_after = Some(n);
    }

    fn insert(&mut self, f: DecodedFrame) {
        let at = self.pending.partition_point(|p| p.pts <= f.pts);
        self.pending.insert(at, f);
    }

    /// Pictures that are due: everything beyond the reorder depth, smallest pts first.
    fn bump(&mut self) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        // A picture still decoding counts towards the depth and may come before the waiting ones.
        while self.pending.len() + self.in_flight.len() > self.stream.reorder {
            let first = self.pending.first().map(|f| f.pts);
            match (first, self.in_flight.iter().min()) {
                (Some(p), Some(&q)) if q < p => break,
                (Some(_), _) => out.push(self.pending.remove(0)),
                (None, _) => break,
            }
        }
        out
    }

    /// Move the callback's pictures into the reorder buffer.
    fn collect(&mut self) -> std::result::Result<(), String> {
        let Some(session) = self.session.as_ref() else { return Ok(()) };
        for o in session.take() {
            let f = o?;
            if let Some(i) = self.in_flight.iter().position(|&p| p == f.pts) {
                self.in_flight.swap_remove(i);
            }
            self.insert(f);
        }
        Ok(())
    }
}

impl VideoDecoder for VtNalDecoder {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        self.fed += 1;
        if self.fail_after.is_some_and(|n| self.fed > n) {
            return Err(CodecError::Decode("VideoToolbox session failed (test hook)".into()));
        }
        if self.stream.is_irap(sample) {
            // CRA / BLA starting a run: its RASL pictures reference pictures we never decoded.
            self.skip_rasl = self.first && self.stream.starts_with_rasl(sample);
        } else if self.skip_rasl && self.stream.is_rasl(sample) {
            self.first = false;
            return Ok(Vec::new());
        }
        self.first = false;
        if self.session.is_none() {
            self.session = Some(Session::new(&self.stream).map_err(CodecError::Decode)?);
        }
        let Some(session) = self.session.as_ref() else {
            return Err(CodecError::Decode("no VideoToolbox session".into()));
        };
        session.submit(sample, pts).map_err(CodecError::Decode)?;
        self.in_flight.push(pts);
        if self.in_flight.len() > MAX_IN_FLIGHT {
            session.wait();
        }
        self.collect().map_err(CodecError::Decode)?;
        Ok(self.bump())
    }

    fn flush(&mut self) -> Vec<DecodedFrame> {
        if let Some(s) = self.session.as_ref() {
            s.wait();
        }
        if let Err(e) = self.collect() {
            log::warn!("{}: {e} while flushing", self.name);
        }
        self.in_flight.clear();
        std::mem::take(&mut self.pending)
    }

    fn reset(&mut self) {
        // A fresh session per run (made on the next sample): no reference pictures survive a seek.
        self.session = None;
        self.pending.clear();
        self.in_flight.clear();
        self.skip_rasl = false;
        self.first = true;
    }

    fn name(&self) -> &str {
        self.name
    }

    fn is_random_access(&self, _sample: &[u8]) -> Option<bool> {
        None
    }

    fn is_disposable(&self, sample: &[u8]) -> bool {
        self.stream.is_disposable(sample)
    }
}
