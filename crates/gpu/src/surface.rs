//! Video frames the GPU reads straight from the hardware decoder's surface (HWDEC2), instead of
//! uploading the frame's CPU planes.
//!
//! A platform layer (fork-only `effectcraft-platform`, macOS) installs a [`SurfaceImport`]: given
//! the address of a decoded frame's luma samples, it returns the decoder's own surface for that
//! frame as two textures of this device (Y, and interleaved CbCr), when it still holds it and the
//! device can share it (Metal). [`planes`] splits them into the three integer planes the native
//! YUV decode reads (`yuv.wgsl`), so everything after (coefficients, alpha, caching) is the same
//! as for uploaded planes. Anything else (no importer, another backend, a frame the decoder no
//! longer holds, a layout the surface doesn't match) returns `None` and the planes are uploaded
//! as before. This crate stays free of `unsafe`: the interop lives in the importer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::context::{Enc, buffer_init, init_resource};
use effectcraft_render::yuv::{Samples, View};

/// A decoder surface as textures of a device: `luma` (`R8Uint` / `R16Uint`) and `chroma`
/// (`Rg8Uint` / `Rg16Uint`, interleaved Cb Cr at half size), samples shifted left by `shift` bits
/// (10-bit samples in the high bits of 16-bit words: 6). The picture starts at `origin` (luma
/// samples, even) of the surface: a decoder may output the coded size and leave the crop to us.
pub struct ImportedPlanes {
    pub luma: wgpu::Texture,
    pub chroma: wgpu::Texture,
    pub shift: u32,
    pub origin: (u32, u32),
}

/// Find the decoder surface of the frame whose luma samples start at `luma` (an address, never
/// dereferenced), `width`×`height` 4:2:0 at `bits` bits, as textures of `device`.
pub type SurfaceImport = fn(device: &wgpu::Device, luma: usize, width: u32, height: u32, bits: u32) -> Option<ImportedPlanes>;

static IMPORT: OnceLock<SurfaceImport> = OnceLock::new();
static IMPORTED: AtomicU64 = AtomicU64::new(0);

/// Install the platform's surface importer (once; later calls are ignored).
pub fn set_surface_import(f: SurfaceImport) {
    let _ = IMPORT.set(f);
}

/// Frames the GPU read from a decoder surface (no upload) so far.
pub fn imported_frames() -> u64 {
    IMPORTED.load(Ordering::Relaxed)
}

struct Split {
    device: wgpu::Device,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

/// The split pipeline of each device it was made for (a handful at most: adapters change rarely).
static SPLIT: Mutex<Vec<Arc<Split>>> = Mutex::new(Vec::new());

fn split_pipeline(device: &wgpu::Device) -> Option<Arc<Split>> {
    let mut all = SPLIT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(s) = all.iter().find(|s| s.device == *device) {
        return Some(s.clone());
    }
    let made = (|| -> Result<Split, String> {
        let module = init_resource(device, "surface split shader", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("surface split"),
                source: wgpu::ShaderSource::Wgsl(include_str!("surface.wgsl").into()),
            })
        })?;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let input =
            wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Uint, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false };
        let output = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::R32Uint,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let uniform =
            wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(32) };
        let layout = init_resource(device, "surface split layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("surface split"),
                entries: &[entry(0, uniform), entry(1, input), entry(2, input), entry(3, output), entry(4, output), entry(5, output)],
            })
        })?;
        let pipeline_layout = init_resource(device, "surface split pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("surface split"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            })
        })?;
        let pipeline = init_resource(device, "surface split pipeline", || {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("surface split"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("split"),
                compilation_options: Default::default(),
                cache: None,
            })
        })?;
        Ok(Split { device: device.clone(), pipeline, layout })
    })();
    match made {
        Ok(s) => {
            let s = Arc::new(s);
            all.push(s.clone());
            Some(s)
        }
        Err(e) => {
            log::info!("gpu: decoder surfaces are uploaded instead ({e})");
            None
        }
    }
}

fn plane(device: &wgpu::Device, w: u32, h: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("decoder surface plane"),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// The Y, Cb and Cr planes of `view` from its decoder surface (recorded on `e`), or `None` (the
/// caller uploads the CPU planes).
pub(crate) fn planes(e: &mut Enc, view: View<'_>) -> Option<[wgpu::Texture; 3]> {
    let import = IMPORT.get()?;
    // what the hardware decoder outputs: 4:2:0 without alpha
    if view.alpha.is_some() || view.subsampling != [1, 1] || !matches!(view.bits, 8 | 10) {
        return None;
    }
    let luma = match view.planes[0].samples {
        Samples::U8(v) => v.as_ptr() as usize,
        Samples::U16(v) => v.as_ptr() as usize,
    };
    let (w, h) = (view.width, view.height);
    let g = e.g;
    let imported = import(&g.device, luma, w, h, view.bits)?;
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ox, oy) = imported.origin;
    let fits = |t: &wgpu::Texture, x: u32, y: u32, w: u32, h: u32| {
        x.checked_add(w).is_some_and(|r| r <= t.width()) && y.checked_add(h).is_some_and(|b| b <= t.height())
    };
    if ox % 2 != 0 || oy % 2 != 0 || !fits(&imported.luma, ox, oy, w, h) || !fits(&imported.chroma, ox / 2, oy / 2, cw, ch) || imported.shift > 8 {
        return None;
    }
    let split = split_pipeline(&g.device)?;
    let out = [plane(&g.device, w, h), plane(&g.device, cw, ch), plane(&g.device, cw, ch)];
    let params = [imported.shift, w, h, 0, ox, oy, 0, 0];
    let uniform = buffer_init(&g.device, &g.queue, Some("surface split"), bytemuck::cast_slice(&params), wgpu::BufferUsages::UNIFORM);
    let (lv, cv) = (imported.luma.create_view(&Default::default()), imported.chroma.create_view(&Default::default()));
    let ov = out.each_ref().map(|t| t.create_view(&Default::default()));
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("surface split"),
        layout: &split.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&lv) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cv) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&ov[0]) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&ov[1]) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&ov[2]) },
        ],
    });
    // The imported textures are not staged (staged planes go back to the upload pool): the bind
    // group keeps them alive until the GPU is done with them, and the importer's own guard keeps
    // the surface from being reused before then.
    e.custom_pixels("surface_split", &split.pipeline, &bg, (w, h));
    IMPORTED.fetch_add(1, Ordering::Relaxed);
    Some(out)
}
