//! The VideoToolbox ProRes decoder behind FilmCraft's decoder registry (safe code).
//!
//! [`factory`] creates a [`VtProResDecoder`] for a ProRes sample entry when the decode runs in a
//! preview read ([`effectcraft_media::hw::allowed`]) and the system has a hardware ProRes
//! decoder. Hardware sessions are pooled process-wide ([`MAX_SESSIONS`] at most: Apple silicon
//! has one or two ProRes engines, and a comp with dozens of layers of one clip asks for many
//! decoders at once); a decoder borrows one per frame. Any sample the hardware doesn't take or
//! fails on is decoded by FilmCraft's own ProRes decoder instead.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use filmcraft_codecs::video::ProResDecoder;
use filmcraft_codecs::{DecodedFrame, Result, VideoDecoder};
use filmcraft_frame::{Chroma, PixelData, VideoFrame};
use filmcraft_isobmff::{CodecConfig, SampleEntry};
use filmcraft_prores::{AlphaType, ChromaFormat, FrameHeader, Interlace};

use crate::surfaces;
use crate::videotoolbox::{self, S4AS, Session, X422};

/// Hardware sessions alive at once (all formats).
pub const MAX_SESSIONS: usize = 4;
/// How long a decode waits for a busy session before decoding in software.
const WAIT: Duration = Duration::from_millis(250);
/// A format whose session could not be created is tried again after this long.
const RETRY: Duration = Duration::from_secs(30);
/// A decoder whose hardware decodes failed this many times in a row decodes the rest of its
/// samples in software (a stream the hardware rejects, e.g. one it reports as malformed, would
/// otherwise cost a failed hardware attempt and a new session per frame).
const GIVE_UP_AFTER: u32 = 3;
/// Largest picture side the hardware path takes.
const MAX_SIDE: u32 = 8192;

const FOURCCS: [&[u8; 4]; 6] = [b"apco", b"apcs", b"apcn", b"apch", b"ap4h", b"ap4x"];

/// What a session decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Format {
    codec: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
}

#[derive(Default)]
struct Pool {
    idle: Vec<(Format, Session)>,
    /// Sessions alive (idle or lent out).
    live: usize,
    /// Formats whose session failed to create, and when.
    failed: HashMap<Format, Instant>,
}

static POOL: LazyLock<(Mutex<Pool>, Condvar)> = LazyLock::new(|| (Mutex::new(Pool::default()), Condvar::new()));

fn pool() -> MutexGuard<'static, Pool> {
    POOL.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A session for `f`: an idle one, a new one while fewer than [`MAX_SESSIONS`] are alive (or in
/// place of an idle one of another format), or the next one returned within [`WAIT`].
fn acquire(f: Format) -> std::result::Result<Session, String> {
    let deadline = Instant::now() + WAIT;
    let mut p = pool();
    if p.failed.get(&f).is_some_and(|at| at.elapsed() < RETRY) {
        return Err("no hardware session for this format".into());
    }
    let replaced = loop {
        if let Some(i) = p.idle.iter().position(|(k, _)| *k == f) {
            return Ok(p.idle.swap_remove(i).1);
        }
        if p.live < MAX_SESSIONS {
            p.live += 1;
            break None;
        }
        if let Some(i) = p.idle.iter().position(|(k, _)| *k != f) {
            break Some(p.idle.swap_remove(i).1);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(BUSY.into());
        }
        p = POOL.1.wait_timeout(p, left).map_or_else(|e| e.into_inner().0, |(g, _)| g);
    };
    drop(p);
    // (the replaced session is closed outside the lock; the new one takes its place)
    drop(replaced);
    match Session::new(f.codec, f.width, f.height, f.pixel_format) {
        Ok(s) => Ok(s),
        Err(e) => {
            let mut p = pool();
            p.live = p.live.saturating_sub(1);
            p.failed.insert(f, Instant::now());
            drop(p);
            POOL.1.notify_one();
            Err(e)
        }
    }
}

const BUSY: &str = "every hardware session is busy";
/// Ends the error of a hardware decode that failed while surfaces were held for the GPU (they were
/// let go; the failure doesn't count towards [`GIVE_UP_AFTER`]).
const SHED: &str = " (decoded surfaces held for the GPU were let go)";

/// Give a working session back.
fn release(f: Format, s: Session) {
    pool().idle.push((f, s));
    POOL.1.notify_one();
}

/// Close a session that failed.
fn discard(s: Session) {
    drop(s);
    let mut p = pool();
    p.live = p.live.saturating_sub(1);
    drop(p);
    POOL.1.notify_one();
}

/// Whether this system has a hardware ProRes decoder.
pub fn hardware_available() -> bool {
    videotoolbox::hardware_decode_supported(u32::from_be_bytes(*b"apcn")) || videotoolbox::hardware_decode_supported(u32::from_be_bytes(*b"ap4h"))
}

/// The factory [`crate::register`] registers: a hardware decoder for ProRes sample entries in
/// preview reads, `None` (the software decoder) otherwise.
pub fn factory(e: &SampleEntry) -> Option<Result<Box<dyn VideoDecoder>>> {
    if !effectcraft_media::hw::allowed() {
        return None;
    }
    let CodecConfig::ProRes { fourcc } = &e.codec else { return None };
    let v = e.video.as_ref()?;
    match VtProResDecoder::new(fourcc.0, u32::from(v.width), u32::from(v.height)) {
        Ok(d) => Some(Ok(Box::new(d))),
        Err(why) => {
            log::debug!("VideoToolbox ProRes: not used for {}: {why}", String::from_utf8_lossy(&fourcc.0));
            None
        }
    }
}

/// A ProRes decoder that decodes on the media engines through VideoToolbox, and in software
/// (FilmCraft's ProRes decoder) whatever the hardware doesn't take.
///
/// Pictures are the software decoder's layout: `Yuv16` planes, 10-bit 4:2:2 or 12-bit 4:4:4
/// with alpha at the same depth when the frame has alpha, Rec. 709 colour with the frame
/// header's matrix and transfer. Visually identical, not bit-identical: previews only.
pub struct VtProResDecoder {
    format: Format,
    chroma: ChromaFormat,
    software: ProResDecoder,
    /// Test hook: every hardware decode fails.
    fail: bool,
    /// Hardware decodes that failed in a row (see [`GIVE_UP_AFTER`]).
    failures: u32,
    warned: bool,
}

impl VtProResDecoder {
    /// A decoder for ProRes `fourcc` pictures of `width`×`height`, or why the hardware doesn't
    /// take them.
    pub fn new(fourcc: [u8; 4], width: u32, height: u32) -> std::result::Result<Self, String> {
        if !FOURCCS.contains(&&fourcc) {
            return Err("not a ProRes fourcc".into());
        }
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(format!("picture size {width}x{height}"));
        }
        let codec = u32::from_be_bytes(fourcc);
        if !videotoolbox::hardware_decode_supported(codec) {
            return Err("no hardware decoder for it".into());
        }
        let four = matches!(&fourcc, b"ap4h" | b"ap4x");
        let format = Format { codec, width, height, pixel_format: if four { S4AS } else { X422 } };
        // Make (or reuse) a session now, so formats the hardware refuses are declined up front.
        release(format, acquire(format)?);
        crate::DECODERS.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            format,
            chroma: if four { ChromaFormat::Yuv444 } else { ChromaFormat::Yuv422 },
            software: ProResDecoder,
            fail: false,
            failures: 0,
            warned: false,
        })
    }

    /// Test hook: from now on every hardware decode fails (as a lost session would), so every
    /// frame comes from the software decoder.
    #[doc(hidden)]
    pub fn fail_hardware(&mut self) {
        self.fail = true;
    }

    /// Decode `sample` on the hardware.
    fn hardware(&self, sample: &[u8]) -> std::result::Result<VideoFrame, String> {
        if self.fail {
            return Err("hardware decoding failed (test hook)".into());
        }
        if self.failures >= GIVE_UP_AFTER {
            return Err("the hardware failed on this stream: decoding it in software".into());
        }
        let (hdr, ..) = FrameHeader::parse(sample).map_err(|e| e.to_string())?;
        let f = self.format;
        if hdr.interlace != Interlace::Progressive {
            return Err("interlaced frame".into());
        }
        if (u32::from(hdr.width), u32::from(hdr.height)) != (f.width, f.height) || hdr.chroma != self.chroma {
            return Err(format!("frame header ({}x{}) doesn't match the sample entry", hdr.width, hdr.height));
        }
        let alpha = hdr.alpha != AlphaType::None && self.chroma == ChromaFormat::Yuv444;
        let session = acquire(f)?;
        let decoded = match session.decode(sample, alpha) {
            Ok(p) => {
                release(f, session);
                p
            }
            Err(e) => {
                discard(session);
                // Surfaces held for the GPU might have drained the decoder's buffer pool: let
                // them go, and don't count this failure towards giving the stream up.
                if surfaces::shed() > 0 {
                    return Err(format!("{e}{SHED}"));
                }
                return Err(e);
            }
        };
        let planes = decoded.planes;
        let (chroma, bits) = match self.chroma {
            ChromaFormat::Yuv422 => (Chroma::C422, 10),
            ChromaFormat::Yuv444 => (Chroma::C444, 12),
        };
        // the software decoder's colour (filmcraft_codecs::video::ProResDecoder)
        let mut color = filmcraft_color::ColorInfo::REC709;
        if let Some(m) = filmcraft_color::Matrix::from_code(hdr.color.matrix) {
            color.matrix = m;
        }
        if let Some(t) = filmcraft_color::Transfer::from_code(hdr.color.transfer) {
            color.transfer = t;
        }
        let y = Arc::new(planes.y);
        // Fix 2: the GPU may read this picture from the decoder's surface instead of uploading it
        if let Some(surface) = decoded.surface {
            let subsampling = if chroma == Chroma::C444 { [0, 0] } else { [1, 0] };
            let layout = surfaces::Layout { size: (f.width, f.height), bits, subsampling, alpha: planes.alpha.is_some(), eight_from: 0 };
            surfaces::keep(surfaces::Luma::U16(Arc::downgrade(&y)), layout, (0, 0), surface);
        }
        Ok(VideoFrame {
            width: f.width,
            height: f.height,
            data: PixelData::Yuv16 { planes: [y, Arc::new(planes.cb), Arc::new(planes.cr)], chroma, bits, alpha: planes.alpha.map(Arc::new) },
            color,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        })
    }
}

impl VideoDecoder for VtProResDecoder {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        match self.hardware(sample) {
            Ok(frame) => {
                self.failures = 0;
                crate::FRAMES.fetch_add(1, Ordering::Relaxed);
                Ok(vec![DecodedFrame { pts, frame, draft: false }])
            }
            Err(why) => {
                if why != BUSY && !why.ends_with(SHED) {
                    self.failures = self.failures.saturating_add(1);
                }
                if !self.warned {
                    self.warned = true;
                    log::info!("VideoToolbox ProRes: decoding in software ({why})");
                }
                crate::FALLBACKS.fetch_add(1, Ordering::Relaxed);
                self.software.decode(sample, pts)
            }
        }
    }
    fn flush(&mut self) -> Vec<DecodedFrame> {
        Vec::new()
    }
    fn reset(&mut self) {}
    fn name(&self) -> &str {
        "VideoToolbox ProRes"
    }
    fn intra_only(&self) -> bool {
        true
    }
}
