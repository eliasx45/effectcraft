//! VideoToolbox H.264 / HEVC decompression sessions (FFI: `unsafe` code,
//! docs/adr/0001-platform-ffi.md, amendment of 2026-10-25).
//!
//! Rules: every `unsafe` block has a `// SAFETY:` comment; no panic may unwind into VideoToolbox
//! (the output callback runs under `catch_unwind`); every failure is a `Result` the caller
//! ([`crate::h26x::VtNalDecoder`], inside [`crate::hybrid::HybridDecoder`]) answers by switching
//! to the software decoder.
//!
//! Adapted from FilmCraft's `crates/platform/src/videotoolbox.rs` (storytold/filmcraft, MIT OR
//! Apache-2.0): a session is created from the record's parameter sets with hardware decoding
//! required, so streams the hardware cannot take are declined up front. Samples are fed as
//! `CMSampleBuffer`s with asynchronous decompression; the output callback (on a VideoToolbox
//! thread) copies each decoded biplanar `CVPixelBuffer` (`420v`/`420f` 8-bit, `x420`/`xf20`
//! 10-bit) into the planar `Yuv8` / `Yuv16` picture the software decoder outputs, cropped to the
//! output rectangle. The frame refcon carries the sample's pts (not a pointer).

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_codecs::DecodedFrame;
use filmcraft_frame::{Chroma, PixelData, VideoFrame};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{CMFormatDescription, CMTime, CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use crate::nal::{Layout, NalCodec, NalStream};
use crate::videotoolbox::{Locked, sample_buffer};

/// Biplanar Y + interleaved CbCr 4:2:0 buffers: 8-bit, or 10-bit in the high bits of 16-bit words;
/// video or full range.
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const P010_VIDEO: u32 = u32::from_be_bytes(*b"x420");
const P010_FULL: u32 = u32::from_be_bytes(*b"xf20");

fn pixel_format(l: &Layout) -> u32 {
    match (l.bits > 8, l.full_range) {
        (false, false) => NV12_VIDEO,
        (false, true) => NV12_FULL,
        (true, false) => P010_VIDEO,
        (true, true) => P010_FULL,
    }
}

/// Whether VideoToolbox reports a hardware decoder for the codec.
pub(crate) fn hardware_decode_supported(codec: NalCodec) -> bool {
    let t = match codec {
        NalCodec::H264 => u32::from_be_bytes(*b"avc1"),
        NalCodec::Hevc => u32::from_be_bytes(*b"hvc1"),
    };
    // SAFETY: a pure query with no pointer arguments.
    unsafe { objc2_video_toolbox::VTIsHardwareDecodeSupported(t) }
}

/// One output-callback result: a picture (with the pts it was submitted with) or why not.
pub(crate) type Output = Result<DecodedFrame, String>;

/// State shared with the output callback (its `decompressionOutputRefCon`). Boxed by the session
/// and kept alive until the session is invalidated.
struct Shared {
    layout: Layout,
    pixel_format: u32,
    out: Mutex<Vec<Output>>,
}

/// A hardware H.264 / HEVC session with its callback state. Dropping it invalidates the session
/// before the callback state is freed.
pub(crate) struct Session {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// Pointed to by the session's callback record: must outlive `session`.
    shared: Box<Shared>,
}

// SAFETY: a VTDecompressionSession and the CoreMedia objects it uses may be used from any thread
// (Apple: "thread safe"); the decoder owning a `Session` is used by one thread at a time (`&mut
// self`), and the callback state is behind a `Mutex`.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is valid until released; invalidating it guarantees no callback runs
        // afterwards, so `shared` can be freed when this function returns.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// The output callback. Never unwinds: panics are caught and reported as a failed frame.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the `Shared` the session was created with; it lives in a `Box` owned by
    // the `Session`, which invalidates the session (no more callbacks) before freeing it.
    let shared = unsafe { &*(refcon as *const Shared) };
    // the frame refcon is not a pointer: it carries the sample's pts
    let pts = frame_refcon as isize as i64;
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Err(format!("VideoToolbox decode error {status}"));
        }
        let Some(image) = NonNull::new(image) else {
            return Err("VideoToolbox dropped a picture".into());
        };
        // SAFETY: VideoToolbox passes a valid image buffer that stays alive for the duration of
        // the callback; we only borrow it here.
        let pb = unsafe { image.as_ref() };
        copy_out(pb, shared).map(|frame| DecodedFrame { pts, frame, draft: false })
    }));
    let r = r.unwrap_or_else(|_| Err("panic while reading a decoded picture".into()));
    shared.out.lock().unwrap_or_else(PoisonError::into_inner).push(r);
}

/// One plane of a locked pixel buffer.
struct PlaneView<'a> {
    data: &'a [u8],
    stride: usize,
    width: usize,
    height: usize,
}

impl<'a> PlaneView<'a> {
    /// `len` bytes of row `y` from byte `x`.
    fn row(&self, y: usize, x: usize, len: usize) -> Result<&'a [u8], String> {
        let start = y.checked_mul(self.stride).and_then(|s| s.checked_add(x)).ok_or("plane row overflows")?;
        let end = start.checked_add(len).ok_or("plane row overflows")?;
        self.data.get(start..end).ok_or_else(|| "plane row out of range".to_string())
    }
}

/// The returned view borrows the lock guard, so it cannot outlive the locked base address.
fn plane<'l>(pb: &CVImageBuffer, _lock: &'l Locked<'_>, i: usize) -> Result<PlaneView<'l>, String> {
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *const u8;
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
    let (width, height) = (CVPixelBufferGetWidthOfPlane(pb, i), CVPixelBufferGetHeightOfPlane(pb, i));
    if base.is_null() || stride == 0 || height == 0 {
        return Err(format!("decoded picture plane {i} is not mapped"));
    }
    let len = stride.checked_mul(height).ok_or("plane size overflows")?;
    // SAFETY: the buffer's base address is locked while `_lock` lives, and the returned slice
    // borrows `_lock` ('l), so it cannot be used after the unlock; CoreVideo maps
    // `bytes_per_row * height` bytes from each plane's base address.
    let data = unsafe { std::slice::from_raw_parts(base, len) };
    Ok(PlaneView { data, stride, width, height })
}

/// Copy a decoded biplanar 4:2:0 buffer into a planar frame (deinterleaving chroma, cropping to
/// the output rectangle, shifting 10-bit samples down from the high bits).
fn copy_out(pb: &CVImageBuffer, s: &Shared) -> Result<VideoFrame, String> {
    let l = &s.layout;
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    if fmt != s.pixel_format || CVPixelBufferGetPlaneCount(pb) != 2 {
        return Err(format!("unexpected decoded pixel format {fmt:#x}"));
    }
    let (bw, bh) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    let (cx, cy, w, h) = (l.crop.0 as usize, l.crop.1 as usize, l.crop.2 as usize, l.crop.3 as usize);
    // VideoToolbox outputs the cropped picture; a buffer of the coded size is cropped here.
    let (ox, oy) = if (bw, bh) == (w, h) {
        (0, 0)
    } else if (bw, bh) == (l.coded.0 as usize, l.coded.1 as usize) && cx + w <= bw && cy + h <= bh {
        (cx, cy)
    } else {
        return Err(format!("decoded picture is {bw}x{bh}, expected {w}x{h}"));
    };
    let lock = Locked::new(pb)?;
    let (ys, cs) = (plane(pb, &lock, 0)?, plane(pb, &lock, 1)?);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (cox, coy) = (ox / 2, oy / 2);
    let bps = if l.bits > 8 { 2 } else { 1 };
    if ys.width < ox + w || ys.height < oy + h || ys.stride < ys.width * bps || cs.width < cox + cw || cs.height < coy + ch || cs.stride < cs.width * 2 * bps {
        return Err("decoded picture planes are smaller than the picture".into());
    }
    let data = if bps == 1 {
        let mut yp = Vec::with_capacity(w * h);
        for y in 0..h {
            yp.extend_from_slice(ys.row(oy + y, ox, w)?);
        }
        let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
        for y in 0..ch {
            for pair in cs.row(coy + y, cox * 2, cw * 2)?.chunks_exact(2) {
                u.push(pair[0]);
                v.push(pair[1]);
            }
        }
        PixelData::Yuv8 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, alpha: None }
    } else {
        let shift = 16u32.saturating_sub(l.bits);
        let mut yp = Vec::with_capacity(w * h);
        for y in 0..h {
            yp.extend(ys.row(oy + y, ox * 2, w * 2)?.chunks_exact(2).map(|b| u16::from_ne_bytes([b[0], b[1]]) >> shift));
        }
        let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
        for y in 0..ch {
            for q in cs.row(coy + y, cox * 4, cw * 4)?.chunks_exact(4) {
                u.push(u16::from_ne_bytes([q[0], q[1]]) >> shift);
                v.push(u16::from_ne_bytes([q[2], q[3]]) >> shift);
            }
        }
        PixelData::Yuv16 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, bits: l.bits, alpha: None }
    };
    drop(lock);
    Ok(VideoFrame { width: w as u32, height: h as u32, data, color: l.color, par: l.par, pts: filmcraft_time::Tick::ZERO })
}

/// A `CMVideoFormatDescription` from the record's parameter sets.
fn format_description(s: &NalStream) -> Result<CFRetained<CMFormatDescription>, String> {
    let mut ptrs: Vec<NonNull<u8>> = Vec::with_capacity(s.parameter_sets.len());
    let mut sizes: Vec<usize> = Vec::with_capacity(s.parameter_sets.len());
    for p in &s.parameter_sets {
        if p.is_empty() {
            return Err("empty parameter set".into());
        }
        ptrs.push(NonNull::new(p.as_ptr() as *mut u8).ok_or("empty parameter set")?);
        sizes.push(p.len());
    }
    let ptr_array = NonNull::new(ptrs.as_mut_ptr()).ok_or("no parameter sets")?;
    let size_array = NonNull::new(sizes.as_mut_ptr()).ok_or("no parameter sets")?;
    let ls = i32::try_from(s.length_size).map_err(|_| "NAL length size")?;
    let mut out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: `ptrs` / `sizes` hold `len` valid (pointer, size) pairs into `s.parameter_sets`
    // (non-empty, checked above), which outlive the call; CoreMedia copies the parameter sets.
    let status = unsafe {
        match s.codec {
            NalCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(None, ptrs.len(), ptr_array, size_array, ls, NonNull::from(&mut out)),
            NalCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(None, ptrs.len(), ptr_array, size_array, ls, None, NonNull::from(&mut out)),
        }
    };
    let fd = NonNull::new(out as *mut CMFormatDescription).filter(|_| status == 0).ok_or_else(|| format!("bad parameter sets for VideoToolbox ({status})"))?;
    // SAFETY: a "Create" function returns the description retained (+1); `CFRetained` takes it over.
    Ok(unsafe { CFRetained::from_raw(fd) })
}

impl Session {
    /// A hardware session for the stream (fails when the system has no hardware decoder for its
    /// profile, level or size).
    pub(crate) fn new(s: &NalStream) -> Result<Session, String> {
        let format = format_description(s)?;
        let pixel_format = pixel_format(&s.layout);
        // SAFETY: reading an immutable framework constant.
        let key: &CFString = unsafe { kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        let pf = CFNumber::new_i32(pixel_format as i32);
        // SAFETY: reading an immutable framework constant.
        let pf_key: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&[pf_key], &[pf.as_ref()]);
        let shared = Box::new(Shared { layout: s.layout.clone(), pixel_format, out: Mutex::new(Vec::new()) });
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: &*shared as *const Shared as *mut c_void,
        };
        let mut out: *mut VTDecompressionSession = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call (VideoToolbox copies the callback record);
        // the refcon points into `shared`, which the returned `Session` keeps alive until the
        // session is invalidated.
        let status = unsafe { VTDecompressionSession::create(None, &format, Some(spec.as_ref()), Some(attrs.as_ref()), &record, NonNull::from(&mut out)) };
        let session = NonNull::new(out).filter(|_| status == 0).ok_or_else(|| format!("VTDecompressionSessionCreate failed ({status})"))?;
        // SAFETY: a created session is returned retained (+1); `CFRetained` takes that reference over.
        let session = unsafe { CFRetained::from_raw(session) };
        Ok(Session { session, format, shared })
    }

    /// Submit one length-prefixed access unit (asynchronous decompression: its picture arrives
    /// through the callback, possibly after this returns).
    pub(crate) fn submit(&self, sample: &[u8], pts: i64) -> Result<(), String> {
        let buffer = sample_buffer(sample, pts, &self.format)?;
        let mut info = VTDecodeInfoFlags(0);
        // SAFETY: the session and sample buffer are valid (VideoToolbox retains the sample buffer
        // for as long as it needs it); the frame refcon is not a pointer, it carries the pts.
        let status =
            unsafe { self.session.decode_frame(&buffer, VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression, pts as isize as *mut c_void, &mut info) };
        if status != 0 {
            return Err(format!("VTDecompressionSessionDecodeFrame failed ({status})"));
        }
        Ok(())
    }

    /// Wait until every submitted picture has been emitted.
    pub(crate) fn wait(&self) {
        // SAFETY: the session is valid.
        unsafe { self.session.wait_for_asynchronous_frames() };
    }

    /// What the callback produced so far.
    pub(crate) fn take(&self) -> Vec<Output> {
        std::mem::take(&mut *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
