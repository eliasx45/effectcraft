//! Video frames the GPU reads straight from the hardware decoder's surface (HWDEC2: H.264 /
//! HEVC; Fix 2, 1 Nov 2026: ProRes 4:2:2 and 4:4:4 + alpha), instead of uploading the frame's CPU
//! planes.
//!
//! A platform layer (fork-only `effectcraft-platform`, macOS) installs a [`SurfaceImport`]: given
//! a [`SurfaceKey`] (the address of a decoded frame's luma samples and the frame's layout), it
//! returns the decoder's own surface for that frame as textures of this device (Y, interleaved
//! CbCr, and alpha when the frame has one), when it still holds it and the device can share it
//! (Metal). The native YUV decode reads them directly ([`direct`], `yuv_surface.wgsl`, Fix 3c), or
//! [`split`] splits them into the integer planes it reads for uploads (`yuv.wgsl`); either way
//! with exactly the samples the decoder copied for the CPU, so everything after (coefficients,
//! alpha, caching) is the same as for uploaded planes. Anything else (no importer,
//! another backend, a frame the decoder no longer holds, a layout the surface doesn't match)
//! returns `None` and the planes are uploaded as before. This crate stays free of `unsafe`: the
//! interop lives in the importer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::context::{Enc, buffer_init, init_resource};
use effectcraft_render::yuv::{Samples, View};

/// A decoder surface as textures of a device: `luma` (`R8Uint` / `R16Uint`), `chroma`
/// (`Rg8Uint` / `Rg16Uint`, interleaved Cb Cr at the frame's chroma subsampling) and `alpha`
/// (`R16Uint`, ProRes 4444 frames with alpha). Samples sit `shift` bits up (10-bit samples in the
/// high bits of 16-bit words: 6; ProRes 4444's 12-bit: 4); `round`: luma and chroma come down
/// rounded to the nearest code and clamped (VideoToolbox's ProRes output keeps more precision
/// than the coded depth) instead of shifted down; alpha is always shifted down. The picture
/// starts at `origin` (luma samples, a multiple of the subsampling) of the surface: a decoder may
/// output the coded size and leave the crop to us. `eight_from` (10 or 12; 0: not): the frame
/// asked for is the 8-bit rounding of the surface's samples at that depth (Decode Footage at 8
/// Bits; limited-range Y'CbCr rounded to nearest, full-range alpha scaled, exactly as the media
/// pool's `frame_to_8bit`).
pub struct ImportedPlanes {
    pub luma: wgpu::Texture,
    pub chroma: wgpu::Texture,
    pub alpha: Option<wgpu::Texture>,
    pub shift: u32,
    pub round: bool,
    pub eight_from: u32,
    pub origin: (u32, u32),
}

/// Which decoded frame's surface is asked for, and its layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceKey {
    /// Address of the frame's luma samples (never dereferenced).
    pub luma: usize,
    pub width: u32,
    pub height: u32,
    pub bits: u32,
    /// Chroma subsampling shifts: 4:4:4 `[0, 0]`, 4:2:2 `[1, 0]`, 4:2:0 `[1, 1]`.
    pub subsampling: [u32; 2],
    /// The frame has an alpha plane.
    pub alpha: bool,
}

/// Find the decoder surface of the frame `key` describes, as textures of `device`.
pub type SurfaceImport = fn(device: &wgpu::Device, key: SurfaceKey) -> Option<ImportedPlanes>;

static IMPORT: OnceLock<SurfaceImport> = OnceLock::new();
static IMPORTED: AtomicU64 = AtomicU64::new(0);
static DIRECT_READS: AtomicU64 = AtomicU64::new(0);
/// Fix 3c: the decode reads imported surfaces directly (`EFFECTCRAFT_SURFACE_DIRECT=0` starts
/// with it off: the split pass, as before).
static DIRECT: std::sync::LazyLock<AtomicBool> =
    std::sync::LazyLock::new(|| AtomicBool::new(std::env::var("EFFECTCRAFT_SURFACE_DIRECT").map_or(true, |v| !matches!(v.trim(), "0" | "off" | "false"))));

/// Install the platform's surface importer (once; later calls are ignored).
pub fn set_surface_import(f: SurfaceImport) {
    let _ = IMPORT.set(f);
}

/// Frames the GPU read from a decoder surface (no upload) so far.
pub fn imported_frames() -> u64 {
    IMPORTED.load(Ordering::Relaxed)
}

/// Of [`imported_frames`], the ones the decode read directly (no split pass).
pub fn direct_frames() -> u64 {
    DIRECT_READS.load(Ordering::Relaxed)
}

/// Whether the YUV decode reads imported surfaces directly ([`set_direct`]).
pub fn direct_enabled() -> bool {
    DIRECT.load(Ordering::Relaxed)
}

/// Read imported surfaces directly in the YUV decode (on by default), or split them into planes
/// first (as before Fix 3c; benches and tests compare the two).
pub fn set_direct(on: bool) {
    DIRECT.store(on, Ordering::Relaxed);
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
                source: wgpu::ShaderSource::Wgsl(concat!(include_str!("surface_samples.wgsl"), "\n", include_str!("surface.wgsl")).into()),
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
            wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(48) };
        let layout = init_resource(device, "surface split layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("surface split"),
                entries: &[
                    entry(0, uniform),
                    entry(1, input),
                    entry(2, input),
                    entry(3, output),
                    entry(4, output),
                    entry(5, output),
                    entry(6, input),
                    entry(7, output),
                ],
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

/// The direct decode pipeline (`yuv_surface.wgsl`) of each device it was made for.
static DIRECT_PIPE: Mutex<Vec<Arc<Split>>> = Mutex::new(Vec::new());

fn direct_pipeline(device: &wgpu::Device) -> Option<Arc<Split>> {
    let mut all = DIRECT_PIPE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(s) = all.iter().find(|s| s.device == *device) {
        return Some(s.clone());
    }
    let made = (|| -> Result<Split, String> {
        let module = init_resource(device, "surface decode shader", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("surface decode"),
                source: wgpu::ShaderSource::Wgsl(
                    concat!(
                        include_str!("ieee_add.wgsl"),
                        "\n",
                        include_str!("ieee_mul.wgsl"),
                        "\n",
                        include_str!("yuv_math.wgsl"),
                        "\n",
                        include_str!("surface_samples.wgsl"),
                        "\n",
                        include_str!("yuv_surface.wgsl")
                    )
                    .into(),
                ),
            })
        })?;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let input =
            wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Uint, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false };
        let uniform =
            |n| wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(n) };
        let output = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba32Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let layout = init_resource(device, "surface decode layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("surface decode"),
                entries: &[entry(0, uniform(64)), entry(1, input), entry(2, input), entry(3, input), entry(4, uniform(48)), entry(5, output)],
            })
        })?;
        let pipeline_layout = init_resource(device, "surface decode pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("surface decode"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            })
        })?;
        let pipeline = init_resource(device, "surface decode pipeline", || {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("surface decode"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("decode_surface"),
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
            log::info!("gpu: decoder surfaces are split into planes first ({e})");
            // (not asked again: a failed build would fail again)
            set_direct(false);
            None
        }
    }
}

/// The integer planes of a frame read from its decoder surface: Y, Cb, Cr, and alpha when the
/// surface had the frame's alpha (otherwise the caller uploads the CPU alpha plane).
pub(crate) struct SurfacePlanes {
    pub yuv: [wgpu::Texture; 3],
    pub alpha: Option<wgpu::Texture>,
}

/// A frame's decoder surface imported and checked against its layout ([`import`]).
pub(crate) struct Imported {
    planes: ImportedPlanes,
    /// The `Split` parameters (`surface_samples.wgsl`).
    params: [u32; 12],
    /// The surface's alpha is the frame's.
    alpha: bool,
    size: (u32, u32),
    chroma: (u32, u32),
}

impl Imported {
    /// The frame's alpha comes from the surface (or the frame has none).
    pub(crate) fn has_frame_alpha(&self, view: View<'_>) -> bool {
        view.alpha.is_none() || self.alpha
    }
}

/// The decoder surface of `view`, or `None` (the caller uploads the CPU planes). The decode reads
/// it [`direct`]ly or [`split`]s it into planes.
pub(crate) fn import(e: &mut Enc, view: View<'_>) -> Option<Imported> {
    let importer = IMPORT.get()?;
    let [sx, sy] = view.subsampling;
    if !matches!(view.subsampling, [0, 0] | [1, 0] | [1, 1]) || !(8..=16).contains(&view.bits) {
        return None;
    }
    // (a frame kept only as a surface names its key; others are found by their luma samples)
    let luma = view.surface.unwrap_or(match view.planes[0].samples {
        Samples::U8(v) => v.as_ptr() as usize,
        Samples::U16(v) => v.as_ptr() as usize,
    });
    let (w, h) = (view.width, view.height);
    let g = e.g;
    let key = SurfaceKey { luma, width: w, height: h, bits: view.bits, subsampling: view.subsampling, alpha: view.alpha.is_some() };
    let imported = importer(&g.device, key)?;
    let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
    let (ox, oy) = imported.origin;
    let fits = |t: &wgpu::Texture, x: u32, y: u32, w: u32, h: u32| {
        x.checked_add(w).is_some_and(|r| r <= t.width()) && y.checked_add(h).is_some_and(|b| b <= t.height())
    };
    let aligned = ox % (1 << sx) == 0 && oy % (1 << sy) == 0;
    let alpha_fits = imported.alpha.as_ref().is_none_or(|a| fits(a, ox, oy, w, h));
    if !aligned
        || !fits(&imported.luma, ox, oy, w, h)
        || !fits(&imported.chroma, ox >> sx, oy >> sy, cw, ch)
        || !alpha_fits
        || imported.shift > 8
        || !matches!(imported.eight_from, 0 | 9..=16)
        || (imported.eight_from != 0 && view.bits != 8)
    {
        return None;
    }
    // the surface's alpha only when the frame has one
    let alpha = imported.alpha.is_some() && view.alpha.is_some();
    let params = [imported.shift, w, h, u32::from(imported.round), ox, oy, cw, ch, sx, sy, u32::from(alpha), imported.eight_from];
    Some(Imported { planes: imported, params, alpha, size: (w, h), chroma: (cw, ch) })
}

/// The surface's alpha texture when it is the frame's, else its luma (bound, never read).
fn alpha_or_luma(imp: &Imported) -> &wgpu::Texture {
    imp.planes.alpha.as_ref().filter(|_| imp.alpha).unwrap_or(&imp.planes.luma)
}

/// Fix 3c: decode `imp` into `out` reading the surface directly, `coefficients` the decode's
/// uniform (`yuv_math.wgsl`'s `Params`); `false` when the direct pipeline isn't there (split
/// instead).
pub(crate) fn direct(e: &mut Enc, imp: &Imported, coefficients: &wgpu::Buffer, out: &wgpu::TextureView) -> bool {
    if !direct_enabled() {
        return false;
    }
    let g = e.g;
    let Some(pipe) = direct_pipeline(&g.device) else { return false };
    let uniform = buffer_init(&g.device, &g.queue, Some("surface decode"), bytemuck::cast_slice(&imp.params), wgpu::BufferUsages::UNIFORM);
    let (lv, cv) = (imp.planes.luma.create_view(&Default::default()), imp.planes.chroma.create_view(&Default::default()));
    let av = alpha_or_luma(imp).create_view(&Default::default());
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("surface decode"),
        layout: &pipe.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: coefficients.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&lv) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cv) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&av) },
            wgpu::BindGroupEntry { binding: 4, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(out) },
        ],
    });
    // (the imported textures stay alive with the bind group until the GPU is done, as for the
    // split)
    e.custom_pixels("yuv_decode_surface", &pipe.pipeline, &bg, imp.size);
    IMPORTED.fetch_add(1, Ordering::Relaxed);
    DIRECT_READS.fetch_add(1, Ordering::Relaxed);
    true
}

/// Split `imp` into the integer planes the decode of uploads reads (recorded on `e`; hand them to
/// [`Enc::stage`] once read).
pub(crate) fn split(e: &mut Enc, imp: Imported) -> Option<SurfacePlanes> {
    let g = e.g;
    let ((w, h), (cw, ch)) = (imp.size, imp.chroma);
    let split = split_pipeline(&g.device)?;
    let out = [g.storage_plane(w, h), g.storage_plane(cw, ch), g.storage_plane(cw, ch)];
    // (without alpha the shader writes nothing to it: a 1×1 stand-in)
    let alpha_out = if imp.alpha { g.storage_plane(w, h) } else { g.storage_plane(1, 1) };
    let uniform = buffer_init(&g.device, &g.queue, Some("surface split"), bytemuck::cast_slice(&imp.params), wgpu::BufferUsages::UNIFORM);
    let (lv, cv) = (imp.planes.luma.create_view(&Default::default()), imp.planes.chroma.create_view(&Default::default()));
    let av = alpha_or_luma(&imp).create_view(&Default::default());
    let ov = out.each_ref().map(|t| t.create_view(&Default::default()));
    let aov = alpha_out.create_view(&Default::default());
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
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&av) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&aov) },
        ],
    });
    // The imported textures are not staged (staged planes go back to the pool): the bind group
    // keeps them alive until the GPU is done with them, and the importer's own guard keeps the
    // surface from being reused before then.
    e.custom_pixels("surface_split", &split.pipeline, &bg, (w, h));
    IMPORTED.fetch_add(1, Ordering::Relaxed);
    let alpha = if imp.alpha {
        Some(alpha_out)
    } else {
        e.stage(alpha_out);
        None
    };
    Some(SurfacePlanes { yuv: out, alpha })
}
