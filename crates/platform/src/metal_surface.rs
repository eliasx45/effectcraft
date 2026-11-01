//! Decoded VideoToolbox surfaces as wgpu textures without a copy (HWDEC2, and ProRes since Fix 2;
//! FFI: `unsafe` code, docs/adr/0001-platform-ffi.md, amendments of 2026-10-25b and 2026-11-01).
//!
//! A decoded `CVPixelBuffer` is IOSurface-backed (the sessions ask for Metal-compatible buffers);
//! each of its planes (Y, interleaved CbCr, and ProRes 4444's alpha) becomes an `MTLTexture` on the IOSurface itself
//! (`newTextureWithDescriptor:iosurface:plane:`, no copy) of wgpu's own `MTLDevice`, wrapped as a
//! wgpu-hal Metal texture and handed to wgpu (`create_texture_from_hal`). The texture's drop
//! callback holds the pixel buffer, so VideoToolbox cannot reuse the surface until wgpu has
//! destroyed the texture (after the GPU finished with it). Every failure is a `Result`: the GPU
//! then uploads the frame's CPU planes as before.

use std::ptr::NonNull;

use effectcraft_gpu::surface::{ImportedPlanes, SurfaceKey};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
};
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

/// A decoded picture's pixel buffer, kept alive (retained) while the GPU may read it.
#[derive(Clone)]
pub(crate) struct Surface(CFRetained<CVImageBuffer>);

// SAFETY: CoreVideo pixel buffers are reference-counted CF objects that may be retained, released
// and read from any thread; this wrapper never writes to the buffer.
unsafe impl Send for Surface {}
// SAFETY: as above (shared references only read immutable properties).
unsafe impl Sync for Surface {}

impl Surface {
    /// Retain `pb` (a buffer VideoToolbox handed to the output callback).
    pub(crate) fn retain(pb: &CVImageBuffer) -> Surface {
        // SAFETY: `pb` is a valid CoreVideo buffer for the duration of this call; retaining it
        // adds a reference that `CFRetained` releases when dropped.
        Surface(unsafe { CFRetained::retain(NonNull::from(pb)) })
    }

    /// The bytes of its planes (what holding it costs).
    pub(crate) fn bytes(&self) -> usize {
        let pb: &CVImageBuffer = &self.0;
        let planar: usize =
            (0..CVPixelBufferGetPlaneCount(pb)).map(|i| CVPixelBufferGetWidthOfPlane(pb, i).saturating_mul(CVPixelBufferGetHeightOfPlane(pb, i))).sum();
        // (the data size covers row padding; a plane sample is at most 4 bytes)
        CVPixelBufferGetDataSize(pb).max(planar)
    }
}

const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const P010_VIDEO: u32 = u32::from_be_bytes(*b"x420");
const P010_FULL: u32 = u32::from_be_bytes(*b"xf20");
// ProRes: `x422` (Y + CbCr, 10 bits in the high bits of 16-bit words) and `s4as` (Y, CbCr, A,
// 16-bit words; the 12-bit Y'CbCr and alpha in the high bits).
use crate::videotoolbox::{S4AS, X422};

type MetalDevice = Retained<ProtocolObject<dyn MTLDevice>>;

/// wgpu's `MTLDevice`, when `device` runs on Metal.
fn metal_device(device: &wgpu::Device) -> Option<MetalDevice> {
    // SAFETY: the hal device is only used to read (and retain) its raw MTLDevice; the guard is
    // dropped before this returns, so no wgpu call happens while it is held.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
    Some(hal.raw_device().clone())
}

/// Whether `device` runs on Metal (surfaces can be shared with it).
pub(crate) fn is_metal(device: &wgpu::Device) -> bool {
    metal_device(device).is_some()
}

/// One plane of `surface` as a wgpu texture of `device`.
fn plane(
    device: &wgpu::Device,
    mtl: &MetalDevice,
    io: &IOSurfaceRef,
    keep: &Surface,
    index: usize,
    (w, h): (u32, u32),
    (metal_format, format): (MTLPixelFormat, wgpu::TextureFormat),
) -> Result<wgpu::Texture, String> {
    // SAFETY: a class method returning a new descriptor for a plain 2D texture (sizes are
    // positive, checked by the caller).
    let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(metal_format, w as usize, h as usize, false) };
    desc.setUsage(MTLTextureUsage::ShaderRead);
    desc.setStorageMode(if mtl.hasUnifiedMemory() { MTLStorageMode::Shared } else { MTLStorageMode::Managed });
    let raw = mtl.newTextureWithDescriptor_iosurface_plane(&desc, io, index).ok_or("Metal refused the decoded surface")?;
    let guard = keep.clone();
    let on_drop: wgpu::hal::DropCallback = Box::new(move || drop(guard));
    // SAFETY: `raw` is a 2D texture of this device's MTLDevice, `w`×`h`, one layer and one mip
    // level, in the Metal format matching `format`; the drop callback keeps the pixel buffer (and
    // its IOSurface) alive as long as wgpu holds the texture.
    let hal = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
            Some(on_drop),
        )
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("decoded surface plane"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // SAFETY: `hal` was made from this device's MTLDevice with exactly `desc`'s size, format,
    // dimension, layers and levels, and Metal shader-read usage covering TEXTURE_BINDING.
    Ok(unsafe { device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal, &desc, wgpu::TextureUses::RESOURCE) })
}

/// The planes of `s` (holding the picture `key` describes, from `origin`; `eight_from`: the
/// picture is the surface's `eight_from`-bit samples rounded to 8 bits, 0: its own samples) as
/// textures of `device` (the whole surface), or why not. Each layout is read the way the decoder's CPU copy
/// reads it (`videotoolbox_nal` for H.264 / HEVC: shifted down; `convert` for ProRes: luma and
/// chroma rounded and clamped, alpha shifted down), so the GPU gets the same samples.
pub(crate) fn import(device: &wgpu::Device, s: &Surface, key: SurfaceKey, origin: (u32, u32), eight_from: u32) -> Result<ImportedPlanes, String> {
    let pb: &CVImageBuffer = &s.0;
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    let r8 = (MTLPixelFormat::R8Uint, wgpu::TextureFormat::R8Uint);
    let rg8 = (MTLPixelFormat::RG8Uint, wgpu::TextureFormat::Rg8Uint);
    let r16 = (MTLPixelFormat::R16Uint, wgpu::TextureFormat::R16Uint);
    let rg16 = (MTLPixelFormat::RG16Uint, wgpu::TextureFormat::Rg16Uint);
    // (luma, chroma, alpha plane, shift, round)
    let bits = match (eight_from, key.bits) {
        (0, b) => b,
        (from, 8) => from,
        _ => return Err("an 8-bit rounding of a picture that is not 8-bit".into()),
    };
    let (lf, cf, alpha_plane, shift, round) = match (fmt, bits, key.subsampling, key.alpha) {
        (NV12_VIDEO | NV12_FULL, 8, [1, 1], false) => (r8, rg8, false, 0, false),
        (P010_VIDEO | P010_FULL, 10, [1, 1], false) => (r16, rg16, false, 6, false),
        (X422, 10, [1, 0], false) => (r16, rg16, false, 6, true),
        (S4AS, 12, [0, 0], alpha) => (r16, rg16, alpha, 4, true),
        _ => return Err(format!("decoded surface format {fmt:#x} for a {bits}-bit {:?} picture (alpha {})", key.subsampling, key.alpha)),
    };
    let planes = CVPixelBufferGetPlaneCount(pb);
    if planes < if alpha_plane { 3 } else { 2 } {
        return Err(format!("decoded surface has {planes} planes"));
    }
    let (width, height) = (key.width, key.height);
    let (bw, bh) = (u32::try_from(CVPixelBufferGetWidth(pb)).map_err(|_| "width")?, u32::try_from(CVPixelBufferGetHeight(pb)).map_err(|_| "height")?);
    let inside = origin.0.checked_add(width).is_some_and(|r| r <= bw) && origin.1.checked_add(height).is_some_and(|b| b <= bh);
    if !inside || width == 0 || height == 0 {
        return Err("the picture is not inside the decoded surface".into());
    }
    let size = |i: usize| -> Result<(u32, u32), String> {
        let (w, h) = (CVPixelBufferGetWidthOfPlane(pb, i), CVPixelBufferGetHeightOfPlane(pb, i));
        match (u32::try_from(w), u32::try_from(h)) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err(format!("decoded surface plane {i} is {w}x{h}")),
        }
    };
    let io = CVPixelBufferGetIOSurface(Some(pb)).ok_or("the decoded picture is not IOSurface-backed")?;
    let mtl = metal_device(device).ok_or("not a Metal device")?;
    let luma = plane(device, &mtl, &io, s, 0, size(0)?, lf)?;
    let chroma = plane(device, &mtl, &io, s, 1, size(1)?, cf)?;
    let alpha = if alpha_plane { Some(plane(device, &mtl, &io, s, 2, size(2)?, r16)?) } else { None };
    Ok(ImportedPlanes { luma, chroma, alpha, shift, round, eight_from, origin })
}
