//! Decoded VideoToolbox surfaces as wgpu textures without a copy (HWDEC2; FFI: `unsafe` code,
//! docs/adr/0001-platform-ffi.md, amendment of 2026-10-25b).
//!
//! A decoded `CVPixelBuffer` is IOSurface-backed (the session asks for Metal-compatible buffers);
//! each of its two planes (Y, interleaved CbCr) becomes an `MTLTexture` on the IOSurface itself
//! (`newTextureWithDescriptor:iosurface:plane:`, no copy) of wgpu's own `MTLDevice`, wrapped as a
//! wgpu-hal Metal texture and handed to wgpu (`create_texture_from_hal`). The texture's drop
//! callback holds the pixel buffer, so VideoToolbox cannot reuse the surface until wgpu has
//! destroyed the texture (after the GPU finished with it). Every failure is a `Result`: the GPU
//! then uploads the frame's CPU planes as before.

use std::ptr::NonNull;

use effectcraft_gpu::surface::ImportedPlanes;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVImageBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth};
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
}

const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const P010_VIDEO: u32 = u32::from_be_bytes(*b"x420");
const P010_FULL: u32 = u32::from_be_bytes(*b"xf20");

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

/// The Y and CbCr planes of `s` (holding a `width`×`height` 4:2:0 picture at `bits` bits from
/// `origin`) as textures of `device` (the whole surface), or why not.
pub(crate) fn import(device: &wgpu::Device, s: &Surface, width: u32, height: u32, bits: u32, origin: (u32, u32)) -> Result<ImportedPlanes, String> {
    let pb: &CVImageBuffer = &s.0;
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    let ((lm, lf), (cm, cf), shift) = match (fmt, bits) {
        (NV12_VIDEO | NV12_FULL, 8) => ((MTLPixelFormat::R8Uint, wgpu::TextureFormat::R8Uint), (MTLPixelFormat::RG8Uint, wgpu::TextureFormat::Rg8Uint), 0),
        (P010_VIDEO | P010_FULL, 10) => ((MTLPixelFormat::R16Uint, wgpu::TextureFormat::R16Uint), (MTLPixelFormat::RG16Uint, wgpu::TextureFormat::Rg16Uint), 6),
        _ => return Err(format!("decoded surface format {fmt:#x} at {bits} bits")),
    };
    let (bw, bh) = (u32::try_from(CVPixelBufferGetWidth(pb)).map_err(|_| "width")?, u32::try_from(CVPixelBufferGetHeight(pb)).map_err(|_| "height")?);
    let inside = origin.0.checked_add(width).is_some_and(|r| r <= bw) && origin.1.checked_add(height).is_some_and(|b| b <= bh);
    if !inside || width == 0 || height == 0 {
        return Err("the picture is not inside the decoded surface".into());
    }
    let io = CVPixelBufferGetIOSurface(Some(pb)).ok_or("the decoded picture is not IOSurface-backed")?;
    let mtl = metal_device(device).ok_or("not a Metal device")?;
    let luma = plane(device, &mtl, &io, s, 0, (bw, bh), (lm, lf))?;
    let chroma = plane(device, &mtl, &io, s, 1, (bw.div_ceil(2), bh.div_ceil(2)), (cm, cf))?;
    Ok(ImportedPlanes { luma, chroma, shift, origin })
}
