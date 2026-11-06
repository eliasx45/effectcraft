//! VideoToolbox H.264 compression sessions (FFI: `unsafe` code, docs/adr/0001-platform-ffi.md,
//! amendment 2026-11-06: hardware H.264 encoding for fast exports).
//!
//! Rules (as for the ProRes sessions in `videotoolbox_enc`): every `unsafe` block has a
//! `// SAFETY:` comment; no panic may unwind into VideoToolbox (the output callback runs under
//! `catch_unwind`); every failure is a `Result` the caller ([`crate::h264_enc`], then the movie
//! exporter) answers by encoding in software or restarting the session.
//!
//! A session is made for one size, frame rate, average bitrate and key-frame interval, High
//! profile, BT.709, with the hardware encoder required and frame reordering off (no B-frames:
//! samples come out in the order pictures go in, so the MP4 needs no composition offsets).
//! Pictures go in as biplanar 4:2:0 8-bit video range (`420v`) from the session's own
//! IOSurface-backed pool, filled by safe code ([`crate::prores_pack::pack_nv12`]); encoding is
//! asynchronous, the callback collecting each sample's bytes (4-byte length-prefixed NAL units),
//! its picture index (the frame refcon, not a pointer) and, on key frames, the parameter sets.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMVideoCodecType_H264};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferPool, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTProfileLevel_H264_High_AutoLevel, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::prores_pack::RowsMut;
use crate::videotoolbox_enc::{Locked, color_names, planes_mut};

/// Biplanar Y' + interleaved Cb Cr, 4:2:0, 8-bit video range: the session's source format.
pub(crate) const NV12: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

/// One coded sample from the callback.
pub(crate) struct Coded {
    pub data: Vec<u8>,
    /// The picture index it went in with.
    pub index: u64,
    /// SPS and PPS (on key frames).
    pub ps: Option<(Vec<u8>, Vec<u8>)>,
}

/// State shared with the output callback (its `outputCallbackRefCon`). Boxed by the session and
/// kept alive until the session is invalidated.
struct Shared {
    out: Mutex<Vec<Result<Coded, String>>>,
}

/// What a session encodes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bits_per_second: i64,
    pub keyint: u32,
}

/// A hardware H.264 compression session. Dropping it invalidates the session before the callback
/// state is freed.
pub(crate) struct Session {
    session: CFRetained<VTCompressionSession>,
    params: Params,
    /// Pointed to by the session's callback refcon: must outlive `session`.
    shared: Box<Shared>,
}

// SAFETY: a VTCompressionSession and the CoreFoundation objects it uses may be used from any
// thread (Apple: "thread safe"); a `Session` is used by one thread at a time (`&mut self` to
// encode), and the callback state is behind a `Mutex`.
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
    frame_refcon: *mut c_void,
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
    // the frame refcon is not a pointer: it carries the picture index
    let index = frame_refcon as usize as u64;
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Err(format!("VideoToolbox encode error {status}"));
        }
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Err("VideoToolbox dropped a frame".into());
        }
        let Some(sample) = NonNull::new(sample) else {
            return Err("VideoToolbox gave no sample".into());
        };
        // SAFETY: VideoToolbox passes a valid sample buffer that stays alive for the duration of
        // the callback; we only borrow it here.
        let sample = unsafe { sample.as_ref() };
        let data = copy_sample(sample)?;
        let ps = if effectcraft_media::h264_hw::has_idr(&data) { Some(parameter_sets(sample)?) } else { None };
        Ok(Coded { data, index, ps })
    }));
    let r = r.unwrap_or_else(|_| Err("panic while reading an encoded frame".into()));
    shared.out.lock().unwrap_or_else(PoisonError::into_inner).push(r);
}

/// The bytes of an encoded sample.
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

/// The SPS and PPS of a sample's format description (4-byte NAL lengths checked).
fn parameter_sets(sample: &CMSampleBuffer) -> Result<(Vec<u8>, Vec<u8>), String> {
    // SAFETY: `sample` is valid; the description comes back retained.
    let desc = unsafe { sample.format_description() }.ok_or("encoded sample has no format description")?;
    let sps = parameter_set(&desc, 0)?;
    let pps = parameter_set(&desc, 1)?;
    Ok((sps.0, pps.0))
}

/// Parameter set `i` of an H.264 format description (a copy) and the NAL length size.
fn parameter_set(desc: &CMFormatDescription, i: usize) -> Result<(Vec<u8>, i32), String> {
    let (mut ptr, mut size, mut count, mut nal_len): (*const u8, usize, usize, i32) = (std::ptr::null(), 0, 0, 0);
    // SAFETY: `desc` is a valid, retained format description; every out pointer is a valid local.
    let status = unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, i, &mut ptr, &mut size, &mut count, &mut nal_len) };
    if status != 0 || ptr.is_null() || size == 0 || size > 1 << 16 {
        return Err(format!("no H.264 parameter set {i} ({status})"));
    }
    if nal_len != 4 {
        return Err(format!("{nal_len}-byte NAL lengths"));
    }
    // SAFETY: CoreMedia returns `size` readable bytes at `ptr`, owned by `desc`, which is retained
    // (borrowed) for this call; they are copied out before it returns.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    Ok((bytes, nal_len))
}

impl Session {
    /// A hardware H.264 session (High profile, BT.709, no frame reordering).
    pub(crate) fn new(p: Params) -> Result<Session, String> {
        let names = color_names(1, 1, 1).ok_or("no BT.709 names")?;
        let (w, h) = (i32::try_from(p.width).map_err(|_| "width")?, i32::try_from(p.height).map_err(|_| "height")?);
        // SAFETY: reading an immutable framework constant.
        let key: &CFString = unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        let (pf, wn, hn) = (CFNumber::new_i32(NV12 as i32), CFNumber::new_i32(w), CFNumber::new_i32(h));
        let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        // SAFETY: reading immutable framework constants.
        let keys: [&CFString; 4] =
            unsafe { [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey] };
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&keys, &[pf.as_ref(), wn.as_ref(), hn.as_ref(), surface.as_ref()]);
        let shared = Box::new(Shared { out: Mutex::new(Vec::new()) });
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call; the refcon points into `shared`, which the
        // returned `Session` keeps alive until the session is invalidated.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                kCMVideoCodecType_H264,
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
        let s = Session { session, params: p, shared };
        let fps = f64::from(p.fps_num) / f64::from(p.fps_den.max(1));
        // SAFETY: reading immutable framework constants.
        let (k_profile, profile, k_rate, k_key, k_reorder, k_real, k_fps) = unsafe {
            (
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_ExpectedFrameRate,
            )
        };
        s.set(k_profile, profile.as_ref())?;
        s.set(k_rate, CFNumber::new_i64(p.bits_per_second.max(100_000)).as_ref())?;
        s.set(k_key, CFNumber::new_i32(i32::try_from(p.keyint.max(1)).unwrap_or(i32::MAX)).as_ref())?;
        s.set(k_reorder, CFBoolean::new(false).as_ref())?;
        s.set(k_real, CFBoolean::new(false).as_ref())?;
        s.set(k_fps, CFNumber::new_f64(fps).as_ref())?;
        // SAFETY: reading immutable framework constants.
        let color_keys: [&CFString; 3] = unsafe {
            [
                objc2_video_toolbox::kVTCompressionPropertyKey_ColorPrimaries,
                objc2_video_toolbox::kVTCompressionPropertyKey_TransferFunction,
                objc2_video_toolbox::kVTCompressionPropertyKey_YCbCrMatrix,
            ]
        };
        for (k, v) in color_keys.into_iter().zip(names) {
            s.set(k, v.as_ref())?;
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

    /// Hand picture `index` in: `fill` writes it into the planes of a source buffer (Y', Cb Cr).
    /// Its sample arrives through the callback ([`Session::take`]), possibly after this returns.
    pub(crate) fn submit(&mut self, index: u64, fill: &mut dyn FnMut(&mut [RowsMut<'_>]) -> Result<(), String>) -> Result<(), String> {
        // SAFETY: the session is valid; the pool comes back retained.
        let pool = unsafe { self.session.pixel_buffer_pool() }.ok_or("the session has no buffer pool")?;
        let mut pb: *mut CVImageBuffer = std::ptr::null_mut();
        // SAFETY: the pool is valid; the out pointer is a valid local.
        let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut pb)) };
        let pb = NonNull::new(pb).filter(|_| status == 0).ok_or_else(|| format!("CVPixelBufferPoolCreatePixelBuffer failed ({status})"))?;
        // SAFETY: a "Create" function returns the buffer retained (+1); `CFRetained` takes it over.
        let pb = unsafe { CFRetained::from_raw(pb) };
        let (w, h) = (self.params.width as usize, self.params.height as usize);
        if CVPixelBufferGetPixelFormatType(&pb) != NV12 || (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb)) != (w, h) {
            return Err(format!("source buffer is {:#x} {}x{}", CVPixelBufferGetPixelFormatType(&pb), CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb)));
        }
        {
            let mut lock = Locked::new(&pb)?;
            let mut planes = planes_mut(&pb, &mut lock)?;
            fill(&mut planes)?;
        }
        let den = i64::from(self.params.fps_den.max(1));
        let scale = i32::try_from(self.params.fps_num.max(1)).unwrap_or(i32::MAX);
        let value = i64::try_from(index).unwrap_or(i64::MAX).saturating_mul(den);
        let pts = CMTime { value, timescale: scale, flags: CMTimeFlags::Valid, epoch: 0 };
        let duration = CMTime { value: den, timescale: scale, flags: CMTimeFlags::Valid, epoch: 0 };
        let refcon = usize::try_from(index).map_err(|_| "frame index")? as *mut c_void;
        let mut info = VTEncodeInfoFlags(0);
        // SAFETY: the session and pixel buffer are valid (VideoToolbox retains the buffer for as
        // long as it needs it); no frame properties; the frame refcon is not a pointer, it carries
        // the picture index back to the callback.
        let status = unsafe { self.session.encode_frame(&pb, pts, duration, None, refcon, &mut info) };
        if status != 0 {
            return Err(format!("VTCompressionSessionEncodeFrame failed ({status})"));
        }
        Ok(())
    }

    /// Wait until every picture handed in has come out.
    pub(crate) fn complete(&mut self) -> Result<(), String> {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        // SAFETY: the session is valid; this returns once every pending frame was called back.
        let status = unsafe { self.session.complete_frames(invalid) };
        if status != 0 {
            return Err(format!("VTCompressionSessionCompleteFrames failed ({status})"));
        }
        Ok(())
    }

    /// The samples the callback produced so far, in the order it produced them.
    pub(crate) fn take(&self) -> Vec<Result<Coded, String>> {
        std::mem::take(&mut *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
