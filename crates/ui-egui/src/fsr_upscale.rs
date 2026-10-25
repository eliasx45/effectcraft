//! UP1: Settings ▸ Previews ▸ Sharpen Low-Resolution Previews (FSR), beyond After Effects. A GPU
//! viewer frame rendered below Full (Half, Third, Quarter, or Adaptive Resolution While Playing)
//! and shown magnified goes through AMD FidelityFX Super Resolution 1.0 on the window's device
//! before egui draws it: EASU upscales it to the size it covers on screen, RCAS (0.2 stops)
//! sharpens it. Off, or at Full resolution, or shown at or under its own pixel size, the frame is
//! drawn as before: magnified pixels are blocks, as in After Effects (#882).
//!
//! The shaders are in `fsr1.wgsl` (AMD's MIT licence; NOTICE). The two output textures are kept
//! across frames and made again only when the display size changes. The pass costs about 0.4 ms
//! at 960×540 → 1920×1080 (M3 preview-upscale experiment, M1 Max). CPU frames are not upscaled.

use eframe::wgpu;

/// The most pixels an upscaled frame may have (a larger display size upscales less, and the
/// sampler magnifies the rest).
pub const MAX_PIXELS: f64 = 4096.0 * 4096.0;

/// Magnifications at or below this are left alone (the frame already about covers its pixels).
const MIN_GAIN: f64 = 1.01;

/// The size to upscale a `frame`-sized GPU frame rendered at `scale` (frame pixels per comp
/// pixel, the frame key's scale) to, when it is drawn at `screen` physical pixels per comp pixel
/// (viewer zoom × pixels per point): `None` when the pref is off, the frame is at Full, or it is
/// not magnified. The size is the frame's size on screen (the comp's Full size × the zoom for a
/// whole-comp frame), within [`MAX_PIXELS`] and `max_side` (the device's texture limit).
pub fn target_size(on: bool, frame: [u32; 2], scale: f64, screen: f64, max_side: u32) -> Option<[u32; 2]> {
    if !on || frame[0] == 0 || frame[1] == 0 || max_side == 0 {
        return None;
    }
    if !(scale.is_finite() && screen.is_finite() && scale > 0.0 && screen > 0.0) || scale >= 0.999 {
        return None;
    }
    let (w, h) = (f64::from(frame[0]), f64::from(frame[1]));
    let mut gain = screen / scale;
    gain = gain.min(f64::from(max_side) / w.max(h));
    if w * h * gain * gain > MAX_PIXELS {
        gain = (MAX_PIXELS / (w * h)).sqrt();
    }
    if !(gain.is_finite() && gain > MIN_GAIN) {
        return None;
    }
    let side = |v: f64| (v * gain).round().clamp(1.0, f64::from(max_side)) as u32;
    let out = [side(w), side(h)];
    (out[0] > frame[0] || out[1] > frame[1]).then_some(out)
}

/// The sampler the upscaled frame is drawn with: it already has about one texel per screen
/// pixel, so bilinear only smooths the rounding.
pub fn sampler() -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        label: Some("viewer frame (FSR)"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    }
}

struct Pipes {
    layout: wgpu::BindGroupLayout,
    easu: wgpu::ComputePipeline,
    rcas: wgpu::ComputePipeline,
}

struct Targets {
    size: [u32; 2],
    /// EASU's output, RCAS's input.
    mid: wgpu::Texture,
    out: wgpu::Texture,
}

/// The viewer's FSR1 pass (on [`crate::EffectcraftApp`]): its pipelines, made on first use, and
/// its output textures.
#[derive(Default)]
pub struct Upscaler {
    pipes: Option<Pipes>,
    targets: Option<Targets>,
    /// The device can't run it (no compute shaders, or a pipeline or pass failed): frames are
    /// drawn as they are from then on.
    failed: bool,
    /// The size the frame on screen was last shown for (`None`: not upscaled), so the viewer
    /// shows it again when the zoom or the pref asks for another.
    pub(crate) asked: Option<[u32; 2]>,
}

impl Upscaler {
    /// Upscale level 0 of `src` (an `Rgba8Unorm` viewer frame) to `size` on `device`: a view of
    /// the result, `None` when the pass can't run (draw `src` instead).
    pub fn run(
        &mut self,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        src: &wgpu::Texture,
        size: [u32; 2],
    ) -> Option<wgpu::TextureView> {
        if self.failed || src.format() != wgpu::TextureFormat::Rgba8Unorm || !src.usage().contains(wgpu::TextureUsages::TEXTURE_BINDING) {
            return None;
        }
        let ok = within_validation(device, || self.encode(adapter, device, queue, src, size));
        match ok {
            Some(Some(view)) => Some(view),
            _ => {
                log::warn!("viewer FSR upscale unavailable on this device: frames are drawn unscaled");
                self.failed = true;
                self.targets = None;
                None
            }
        }
    }

    fn encode(
        &mut self,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        src: &wgpu::Texture,
        size: [u32; 2],
    ) -> Option<wgpu::TextureView> {
        if self.pipes.is_none() {
            if !adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
                return None;
            }
            self.pipes = Some(pipes(device));
        }
        let pipes = self.pipes.as_ref()?;
        if self.targets.as_ref().is_none_or(|t| t.size != size) {
            self.targets = Some(Targets { size, mid: target(device, "viewer FSR (EASU)", size), out: target(device, "viewer FSR", size) });
        }
        let t = self.targets.as_ref()?;
        let src_view = src.create_view(&wgpu::TextureViewDescriptor { base_mip_level: 0, mip_level_count: Some(1), ..Default::default() });
        let mid_view = t.mid.create_view(&Default::default());
        let out_view = t.out.create_view(&Default::default());
        let bind = |label, a: &wgpu::TextureView, b: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &pipes.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(a) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(b) },
                ],
            })
        };
        let easu_bg = bind("viewer FSR EASU", &src_view, &mid_view);
        let rcas_bg = bind("viewer FSR RCAS", &mid_view, &out_view);
        let groups = (size[0].div_ceil(8), size[1].div_ceil(8));
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("viewer FSR") });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("viewer FSR"), timestamp_writes: None });
            pass.set_pipeline(&pipes.easu);
            pass.set_bind_group(0, &easu_bg, &[]);
            pass.dispatch_workgroups(groups.0, groups.1, 1);
            pass.set_pipeline(&pipes.rcas);
            pass.set_bind_group(0, &rcas_bg, &[]);
            pass.dispatch_workgroups(groups.0, groups.1, 1);
        }
        queue.submit([enc.finish()]);
        Some(out_view)
    }

    /// The size of the current output texture (tests, diagnostics).
    pub fn target_size(&self) -> Option<[u32; 2]> {
        self.targets.as_ref().map(|t| t.size)
    }
}

/// Run `f` inside a validation error scope (native): `None` when it raised a validation error.
/// The browser can't wait for the scope, so there `f` runs as it is.
fn within_validation<T>(device: &wgpu::Device, f: impl FnOnce() -> T) -> Option<T> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f();
        match pollster::block_on(scope.pop()) {
            None => Some(out),
            Some(e) => {
                log::error!("viewer FSR upscale: {e}");
                None
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = device;
        Some(f())
    }
}

fn pipes(device: &wgpu::Device) -> Pipes {
    let module = device
        .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("viewer FSR1"), source: wgpu::ShaderSource::Wgsl(include_str!("fsr1.wgsl").into()) });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewer FSR"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    });
    let pl =
        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("viewer FSR"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
    let pipe = |entry: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(&pl),
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        })
    };
    let (easu, rcas) = (pipe("easu"), pipe("rcas"));
    Pipes { layout, easu, rcas }
}

fn target(device: &wgpu::Device, label: &str, size: [u32; 2]) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}
