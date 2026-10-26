//! VideoToolbox ProRes decompression sessions (FFI: `unsafe` code, docs/adr/0001-platform-ffi.md;
//! H.264 / HEVC sessions are in `videotoolbox_nal`, which shares the sample-buffer helper here).
//!
//! Rules: every `unsafe` block has a `// SAFETY:` comment; no panic may unwind into VideoToolbox
//! (the output callback runs under `catch_unwind`); every failure is a `Result` the caller
//! ([`crate::prores::VtProResDecoder`]) answers by decoding the sample in software.
//!
//! Adapted from FilmCraft's `crates/platform/src/videotoolbox.rs` (storytold/filmcraft, MIT OR
//! Apache-2.0: the session, output callback, base-address lock guard and sample-buffer
//! construction), for ProRes: a session is made from the sample entry's fourcc and size (ProRes
//! has no parameter sets), hardware decoding is required, and each sample is decoded
//! synchronously into a tri-planar `s4as` (4:4:4 + alpha; the decoder's own layout: the packed
//! `y416` it also offers comes with low-passed chroma) or biplanar `x422` (4:2:2) buffer that the
//! callback copies into planes ([`crate::convert`]).

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::{Mutex, Once, PoisonError};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoFormatDescriptionCreate,
    kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use crate::convert::{self, Planes, Rows};

/// Tri-planar 4:4:4 Y', Cb Cr, A, 16 bits each (ProRes 4444 / 4444 XQ, with or without alpha).
pub(crate) const S4AS: u32 = kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar;
/// Biplanar 4:2:2 Y' + Cb Cr, 10 bits in the high bits of 16-bit words (ProRes 422 family).
pub(crate) const X422: u32 = kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange;

/// Let VideoToolbox use the professional video decoders (ProRes) of this system, once.
pub(crate) fn register_professional_decoders() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: a registration call with no arguments; Apple documents it as safe to call at
        // any time, and `Once` makes it run once.
        unsafe { objc2_video_toolbox::VTRegisterProfessionalVideoWorkflowVideoDecoders() };
    });
}

/// Whether VideoToolbox reports a hardware decoder for this codec type (a fourcc).
pub(crate) fn hardware_decode_supported(codec: u32) -> bool {
    register_professional_decoders();
    // SAFETY: a pure query with no pointer arguments.
    unsafe { objc2_video_toolbox::VTIsHardwareDecodeSupported(codec) }
}

/// State shared with the output callback (its `decompressionOutputRefCon`). Boxed by the session
/// and kept alive until the session is invalidated.
struct Shared {
    pixel_format: u32,
    width: usize,
    height: usize,
    out: Mutex<Option<Result<Planes, String>>>,
}

/// A hardware ProRes decompression session for one fourcc, size and output format. Dropping it
/// invalidates the session before the callback state is freed.
pub(crate) struct Session {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// Pointed to by the session's callback record: must outlive `session`.
    shared: Box<Shared>,
}

// SAFETY: a VTDecompressionSession and the CoreMedia objects it uses may be used from any thread
// (Apple: "thread safe"); a `Session` is used by one thread at a time (the session pool hands it
// out exclusively), and the callback state is behind a `Mutex`.
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
    // the frame refcon is not a pointer: it says whether the picture's alpha is wanted
    let alpha = !frame_refcon.is_null();
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
        copy_out(pb, shared, alpha)
    }));
    let r = r.unwrap_or_else(|_| Err("panic while reading a decoded picture".into()));
    *shared.out.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
}

/// Unlocks a pixel buffer's base address when dropped (made after a successful read-only lock).
pub(crate) struct Locked<'a>(&'a CVImageBuffer);

impl<'a> Locked<'a> {
    /// Lock `pb`'s base address read-only until the guard is dropped.
    pub(crate) fn new(pb: &'a CVImageBuffer) -> Result<Self, String> {
        // SAFETY: `pb` is a valid pixel buffer borrowed for 'a; the guard unlocks it with the
        // same flags exactly once.
        if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return Err("cannot lock the decoded picture".into());
        }
        Ok(Locked(pb))
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: `copy_out` locked the buffer with the same flags, and it is still alive (borrowed).
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// The rows of plane `i` of a locked buffer. The returned rows borrow the lock guard, so they
/// cannot outlive the locked base address.
fn rows<'l>(pb: &CVImageBuffer, _lock: &'l Locked<'_>, i: usize) -> Result<Rows<'l>, String> {
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *const u8;
    let (stride, height) = (CVPixelBufferGetBytesPerRowOfPlane(pb, i), CVPixelBufferGetHeightOfPlane(pb, i));
    if base.is_null() || stride == 0 || height == 0 {
        return Err("decoded picture is not mapped".into());
    }
    let len = stride.checked_mul(height).ok_or("plane size overflows")?;
    // SAFETY: the buffer's base address is locked while `_lock` lives, and the returned slice
    // borrows `_lock` ('l), so it cannot be used after the unlock; CoreVideo maps
    // `bytes_per_row * height` bytes from each plane's base address.
    let data = unsafe { std::slice::from_raw_parts(base, len) };
    Ok(Rows { data, stride })
}

/// Copy a decoded buffer into planes.
fn copy_out(pb: &CVImageBuffer, s: &Shared, alpha: bool) -> Result<Planes, String> {
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    if fmt != s.pixel_format {
        return Err(format!("unexpected decoded pixel format {fmt:#x}"));
    }
    let (bw, bh) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    if (bw, bh) != (s.width, s.height) {
        return Err(format!("decoded picture is {bw}x{bh}, expected {}x{}", s.width, s.height));
    }
    let lock = Locked::new(pb)?;
    let planes = match fmt {
        S4AS if CVPixelBufferGetPlaneCount(pb) == 3 => {
            let a = if alpha { Some(rows(pb, &lock, 2)?) } else { None };
            convert::unpack_s4as(rows(pb, &lock, 0)?, rows(pb, &lock, 1)?, a, s.width, s.height)
        }
        X422 if CVPixelBufferGetPlaneCount(pb) == 2 => convert::unpack_x422(rows(pb, &lock, 0)?, rows(pb, &lock, 1)?, s.width, s.height),
        _ => Err("decoded picture has an unexpected plane count".into()),
    };
    drop(lock);
    planes
}

impl Session {
    /// A hardware session for ProRes `codec` (its fourcc as a big-endian `u32`) pictures of
    /// `width`×`height`, decoding into `pixel_format` ([`S4AS`] or [`X422`]).
    pub(crate) fn new(codec: u32, width: u32, height: u32, pixel_format: u32) -> Result<Session, String> {
        register_professional_decoders();
        let (w, h) = (i32::try_from(width).map_err(|_| "width")?, i32::try_from(height).map_err(|_| "height")?);
        let mut fd: *const CMFormatDescription = std::ptr::null();
        // SAFETY: no extensions; the out pointer is a valid local.
        let status = unsafe { CMVideoFormatDescriptionCreate(None, codec, w, h, None, NonNull::from(&mut fd)) };
        let fd =
            NonNull::new(fd as *mut CMFormatDescription).filter(|_| status == 0).ok_or_else(|| format!("CMVideoFormatDescriptionCreate failed ({status})"))?;
        // SAFETY: a "Create" function returns the description retained (+1); `CFRetained` takes it over.
        let format = unsafe { CFRetained::from_raw(fd) };
        // SAFETY: reading an immutable framework constant.
        let key: &CFString = unsafe { kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        let pf = CFNumber::new_i32(pixel_format as i32);
        // SAFETY: reading an immutable framework constant.
        let pf_key: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&[pf_key], &[pf.as_ref()]);
        let shared = Box::new(Shared { pixel_format, width: width as usize, height: height as usize, out: Mutex::new(None) });
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

    /// Decode one ProRes frame (synchronously) into planes; alpha only when `alpha`.
    pub(crate) fn decode(&self, sample: &[u8], alpha: bool) -> Result<Planes, String> {
        let buffer = sample_buffer(sample, 0, &self.format)?;
        *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let mut info = VTDecodeInfoFlags(0);
        let frame_refcon = if alpha { NonNull::<c_void>::dangling().as_ptr() } else { std::ptr::null_mut() };
        // SAFETY: the session and sample buffer are valid (VideoToolbox retains the sample buffer
        // for as long as it needs it); the frame refcon is not a pointer, it carries a flag.
        let status = unsafe { self.session.decode_frame(&buffer, VTDecodeFrameFlags(0), frame_refcon, &mut info) };
        if status != 0 {
            return Err(format!("VTDecompressionSessionDecodeFrame failed ({status})"));
        }
        // (synchronous decoding has called back already; this makes sure)
        // SAFETY: the session is valid.
        unsafe { self.session.wait_for_asynchronous_frames() };
        self.shared.out.lock().unwrap_or_else(PoisonError::into_inner).take().unwrap_or_else(|| Err("VideoToolbox produced no picture".into()))
    }
}

/// A ready `CMSampleBuffer` holding a copy of `sample`, presented at `pts` (timescale 1).
pub(crate) fn sample_buffer(sample: &[u8], pts: i64, format: &CMFormatDescription) -> Result<CFRetained<CMSampleBuffer>, String> {
    if sample.is_empty() {
        return Err("empty sample".into());
    }
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: CoreMedia allocates (AssureMemoryNow) a block of `len` bytes with the default
    // allocator; no external memory is referenced.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            sample.len(),
            None,
            std::ptr::null(),
            0,
            sample.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    let block = NonNull::new(block).filter(|_| status == 0).ok_or_else(|| format!("CMBlockBufferCreateWithMemoryBlock failed ({status})"))?;
    // SAFETY: created retained (+1).
    let block = unsafe { CFRetained::from_raw(block) };
    let src = NonNull::new(sample.as_ptr() as *mut c_void).ok_or("empty sample")?;
    // SAFETY: copies `len` bytes from `sample` into the block, which holds exactly `len` bytes.
    let status = unsafe { CMBlockBuffer::replace_data_bytes(src, &block, 0, sample.len()) };
    if status != 0 {
        return Err(format!("CMBlockBufferReplaceDataBytes failed ({status})"));
    }
    let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
    let timing = CMSampleTimingInfo {
        duration: invalid,
        presentationTimeStamp: CMTime { value: pts, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 },
        decodeTimeStamp: invalid,
    };
    let size = sample.len();
    let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
    // SAFETY: one sample with one timing entry and one size entry, all pointing at locals that
    // outlive the call; the sample buffer retains the block buffer and format description.
    let status = unsafe { CMSampleBuffer::create_ready(None, Some(&block), Some(format), 1, 1, &timing, 1, &size, NonNull::from(&mut out)) };
    let out = NonNull::new(out).filter(|_| status == 0).ok_or_else(|| format!("CMSampleBufferCreateReady failed ({status})"))?;
    // SAFETY: created retained (+1).
    Ok(unsafe { CFRetained::from_raw(out) })
}
