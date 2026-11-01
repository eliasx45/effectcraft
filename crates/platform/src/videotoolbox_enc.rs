//! VideoToolbox ProRes compression sessions (FFI: `unsafe` code, docs/adr/0001-platform-ffi.md,
//! amendment 2026-11-01: hardware ProRes encoding).
//!
//! Rules (as for the decoder sessions in `videotoolbox`): every `unsafe` block has a `// SAFETY:`
//! comment; no panic may unwind into VideoToolbox (the output callback runs under
//! `catch_unwind`); every failure is a `Result` the caller ([`crate::prores_enc`]) answers by
//! encoding the frame in software.
//!
//! A session is made for one ProRes fourcc, size, colour description and alpha choice, with the
//! hardware encoder required. Frames go in as the encoder's own source format, so VideoToolbox
//! never converts them: tri-planar `s4as` (4:4:4 + alpha, 16-bit) for 4444 / 4444 XQ and
//! biplanar `sv22` (4:2:2, 16-bit) for the 422 family, filled from the session's own
//! IOSurface-backed buffer pool by safe code ([`crate::prores_pack`]). Each frame is encoded
//! synchronously (encode, then complete): callers encode frames on several threads, each with a
//! session of its own.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_DCI_P3, kCVImageBufferColorPrimaries_EBU_3213,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_P3_D65,
    kCVImageBufferColorPrimaries_SMPTE_C, kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_Linear, kCVImageBufferTransferFunction_SMPTE_240M_1995, kCVImageBufferTransferFunction_SMPTE_ST_428_1,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferTransferFunction_sRGB, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrix_SMPTE_240M_1995, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_422YpCbCr16BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::prores_pack::RowsMut;

/// Tri-planar 4:4:4 Y', Cb Cr, A, 16 bits each: the hardware 4444 encoder's source format.
pub(crate) const S4AS: u32 = kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar;
/// Biplanar 4:2:2 Y' + Cb Cr, 16 bits each: the hardware 422 encoders' source format.
pub(crate) const SV22: u32 = kCVPixelFormatType_422YpCbCr16BiPlanarVideoRange;

/// The CoreVideo names of ISO/IEC 23091-2 colour codes (primaries, transfer, matrix), or `None`
/// for a code a session can't be told (unspecified, or one CoreVideo has no name for).
fn color_names(primaries: u8, transfer: u8, matrix: u8) -> Option<[&'static CFString; 3]> {
    // SAFETY: reading immutable framework constants.
    let p = unsafe {
        match primaries {
            1 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            5 => kCVImageBufferColorPrimaries_EBU_3213,
            6 => kCVImageBufferColorPrimaries_SMPTE_C,
            9 => kCVImageBufferColorPrimaries_ITU_R_2020,
            11 => kCVImageBufferColorPrimaries_DCI_P3,
            12 => kCVImageBufferColorPrimaries_P3_D65,
            _ => return None,
        }
    };
    // SAFETY: reading immutable framework constants.
    let t = unsafe {
        match transfer {
            1 => kCVImageBufferTransferFunction_ITU_R_709_2,
            7 => kCVImageBufferTransferFunction_SMPTE_240M_1995,
            8 => kCVImageBufferTransferFunction_Linear,
            13 => kCVImageBufferTransferFunction_sRGB,
            16 => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            17 => kCVImageBufferTransferFunction_SMPTE_ST_428_1,
            18 => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            _ => return None,
        }
    };
    // SAFETY: reading immutable framework constants.
    let m = unsafe {
        match matrix {
            1 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            6 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            7 => kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
            9 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            _ => return None,
        }
    };
    Some([p, t, m])
}

/// State shared with the output callback (its `outputCallbackRefCon`). Boxed by the session and
/// kept alive until the session is invalidated.
struct Shared {
    out: Mutex<Option<Result<Vec<u8>, String>>>,
}

/// A hardware ProRes compression session. Dropping it invalidates the session before the callback
/// state is freed.
pub(crate) struct Session {
    session: CFRetained<VTCompressionSession>,
    pixel_format: u32,
    width: usize,
    height: usize,
    /// Presentation time of the next frame (frames must come in increasing time order).
    next_pts: i64,
    /// Pointed to by the session's callback refcon: must outlive `session`.
    shared: Box<Shared>,
}

// SAFETY: a VTCompressionSession and the CoreFoundation objects it uses may be used from any
// thread (Apple: "thread safe"); a `Session` is used by one thread at a time (the session pool
// hands it out exclusively, `&mut self` to encode), and the callback state is behind a `Mutex`.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        // SAFETY: the session is valid until released; completing and invalidating it guarantees
        // no callback runs afterwards, so `shared` can be freed when this function returns.
        unsafe {
            self.session.complete_frames(invalid);
            self.session.invalidate();
        }
    }
}

/// The output callback. Never unwinds: panics are caught and reported as a failed frame.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the `Shared` the session was created with; it lives in a `Box` owned by
    // the `Session`, which invalidates the session (no more callbacks) before freeing it.
    let shared = unsafe { &*(refcon as *const Shared) };
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Err(format!("VideoToolbox encode error {status}"));
        }
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Err("VideoToolbox dropped the frame".into());
        }
        let Some(sample) = NonNull::new(sample) else {
            return Err("VideoToolbox gave no sample".into());
        };
        // SAFETY: VideoToolbox passes a valid sample buffer that stays alive for the duration of
        // the callback; we only borrow it here.
        let sample = unsafe { sample.as_ref() };
        copy_sample(sample)
    }));
    let r = r.unwrap_or_else(|_| Err("panic while reading an encoded frame".into()));
    *shared.out.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
}

/// The bytes of an encoded sample (one ProRes frame).
fn copy_sample(sample: &CMSampleBuffer) -> Result<Vec<u8>, String> {
    // SAFETY: `sample` is a valid sample buffer; the data buffer comes back retained.
    let block = unsafe { sample.data_buffer() }.ok_or("encoded sample has no data")?;
    // SAFETY: `block` is a valid block buffer.
    let len = unsafe { block.data_length() };
    if len == 0 || len > 1 << 30 {
        return Err(format!("encoded sample of {len} bytes"));
    }
    let mut out = vec![0u8; len];
    let dst = NonNull::new(out.as_mut_ptr() as *mut c_void).ok_or("no buffer")?;
    // SAFETY: copies `len` bytes (the block's whole length) into `out`, which holds exactly `len`.
    let status = unsafe { block.copy_data_bytes(0, len, dst) };
    if status != 0 {
        return Err(format!("CMBlockBufferCopyDataBytes failed ({status})"));
    }
    Ok(out)
}

/// Unlocks a pixel buffer's base address when dropped (made after a successful read-write lock).
struct Locked<'a>(&'a CVImageBuffer);

impl<'a> Locked<'a> {
    fn new(pb: &'a CVImageBuffer) -> Result<Self, String> {
        // SAFETY: `pb` is a valid pixel buffer borrowed for 'a; the guard unlocks it with the
        // same flags exactly once.
        if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0)) } != 0 {
            return Err("cannot lock the source picture".into());
        }
        Ok(Locked(pb))
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked with the same flags in `new`, and still alive (borrowed).
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags(0)) };
    }
}

/// The writable rows of every plane of a locked buffer. They borrow the lock guard mutably, so
/// they cannot outlive the locked base address, and each plane is a separate region.
fn planes_mut<'l>(pb: &CVImageBuffer, _lock: &'l mut Locked<'_>) -> Result<Vec<RowsMut<'l>>, String> {
    let n = CVPixelBufferGetPlaneCount(pb);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *mut u8;
        let (stride, height) = (CVPixelBufferGetBytesPerRowOfPlane(pb, i), CVPixelBufferGetHeightOfPlane(pb, i));
        if base.is_null() || stride == 0 || height == 0 {
            return Err("source picture is not mapped".into());
        }
        let len = stride.checked_mul(height).ok_or("plane size overflows")?;
        // SAFETY: the buffer's base address is locked read-write while `_lock` lives, and the
        // returned slices borrow `_lock` mutably ('l), so they cannot be used after the unlock;
        // CoreVideo maps `bytes_per_row * height` bytes from each plane's base address, and the
        // planes of a planar buffer don't overlap, so the mutable slices are disjoint.
        let data = unsafe { std::slice::from_raw_parts_mut(base, len) };
        out.push(RowsMut { data, stride });
    }
    Ok(out)
}

impl Session {
    /// A hardware session for ProRes `codec` (its fourcc as a big-endian `u32`) frames of
    /// `width`×`height` with these colour codes, keeping alpha (4444) when `alpha`.
    pub(crate) fn new(codec: u32, width: u32, height: u32, color: (u8, u8, u8), alpha: bool) -> Result<Session, String> {
        let four444 = matches!(&codec.to_be_bytes(), b"ap4h" | b"ap4x");
        let pixel_format = if four444 { S4AS } else { SV22 };
        let names = color_names(color.0, color.1, color.2).ok_or_else(|| format!("colour {color:?} has no CoreVideo name"))?;
        let (w, h) = (i32::try_from(width).map_err(|_| "width")?, i32::try_from(height).map_err(|_| "height")?);
        // SAFETY: reading an immutable framework constant.
        let key: &CFString = unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        let (pf, wn, hn) = (CFNumber::new_i32(pixel_format as i32), CFNumber::new_i32(w), CFNumber::new_i32(h));
        let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        // SAFETY: reading immutable framework constants.
        let keys: [&CFString; 4] =
            unsafe { [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey] };
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&keys, &[pf.as_ref(), wn.as_ref(), hn.as_ref(), surface.as_ref()]);
        let shared = Box::new(Shared { out: Mutex::new(None) });
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call; the refcon points into `shared`, which the
        // returned `Session` keeps alive until the session is invalidated.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                codec,
                Some(spec.as_opaque()),
                Some(attrs.as_opaque()),
                None,
                Some(output_callback),
                &*shared as *const Shared as *mut c_void,
                NonNull::from(&mut out),
            )
        };
        let session = NonNull::new(out).filter(|_| status == 0).ok_or_else(|| format!("VTCompressionSessionCreate failed ({status})"))?;
        // SAFETY: a created session is returned retained (+1); `CFRetained` takes that reference over.
        let session = unsafe { CFRetained::from_raw(session) };
        let s = Session { session, pixel_format, width: width as usize, height: height as usize, next_pts: 0, shared };
        // SAFETY: reading immutable framework constants.
        let color_keys: [&CFString; 3] =
            unsafe { [kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix] };
        for (k, v) in color_keys.into_iter().zip(names) {
            s.set(k, v.as_ref())?;
        }
        if four444 {
            // without it the encoder codes the (opaque) alpha plane of every frame
            s.set(&CFString::from_static_str("kVTCodecPropertyKey_PreserveAlphaChannel"), CFBoolean::new(alpha).as_ref())?;
        }
        Ok(s)
    }

    fn set(&self, key: &CFString, value: &CFType) -> Result<(), String> {
        // SAFETY: the session is valid; key and value are valid CF objects VideoToolbox retains
        // as it needs.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            return Err(format!("VTSessionSetProperty {key} failed ({status})"));
        }
        Ok(())
    }

    /// The source pixel format (`S4AS` or `SV22`).
    pub(crate) fn pixel_format(&self) -> u32 {
        self.pixel_format
    }

    /// Encode one frame (synchronously): `fill` writes the picture into the planes of a source
    /// buffer (Y', Cb Cr[, A]); the result is the encoded ProRes frame.
    pub(crate) fn encode(&mut self, fill: &mut dyn FnMut(&mut [RowsMut<'_>]) -> Result<(), String>) -> Result<Vec<u8>, String> {
        // SAFETY: the session is valid; the pool comes back retained.
        let pool = unsafe { self.session.pixel_buffer_pool() }.ok_or("the session has no buffer pool")?;
        let mut pb: *mut CVImageBuffer = std::ptr::null_mut();
        // SAFETY: the pool is valid; the out pointer is a valid local.
        let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut pb)) };
        let pb = NonNull::new(pb).filter(|_| status == 0).ok_or_else(|| format!("CVPixelBufferPoolCreatePixelBuffer failed ({status})"))?;
        // SAFETY: a "Create" function returns the buffer retained (+1); `CFRetained` takes it over.
        let pb = unsafe { CFRetained::from_raw(pb) };
        let fmt = CVPixelBufferGetPixelFormatType(&pb);
        if fmt != self.pixel_format || (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb)) != (self.width, self.height) {
            return Err(format!("source buffer is {fmt:#x} {}x{}", CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb)));
        }
        {
            let mut lock = Locked::new(&pb)?;
            let mut planes = planes_mut(&pb, &mut lock)?;
            fill(&mut planes)?;
        }
        *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let pts = CMTime { value: self.next_pts, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 };
        self.next_pts = self.next_pts.saturating_add(1);
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        let mut info = VTEncodeInfoFlags(0);
        // SAFETY: the session and pixel buffer are valid (VideoToolbox retains the buffer for as
        // long as it needs it); no frame properties; the frame refcon is unused.
        let status = unsafe { self.session.encode_frame(&pb, pts, invalid, None, std::ptr::null_mut(), &mut info) };
        if status != 0 {
            return Err(format!("VTCompressionSessionEncodeFrame failed ({status})"));
        }
        // SAFETY: the session is valid; this returns once every pending frame was called back.
        let status = unsafe { self.session.complete_frames(invalid) };
        if status != 0 {
            return Err(format!("VTCompressionSessionCompleteFrames failed ({status})"));
        }
        self.shared.out.lock().unwrap_or_else(PoisonError::into_inner).take().unwrap_or_else(|| Err("VideoToolbox produced no frame".into()))
    }
}
