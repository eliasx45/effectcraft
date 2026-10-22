//! Half-float working images (RGBA16F) for effect intermediates (preview memory).
//!
//! A multi-pass effect on a large canvas holds several full-size working images at once: a
//! Bevel and Emboss on a 40 Mpx layer canvas used to hold nine 656 MB RGBA f32 images. The
//! scratch images such effects only read back themselves (blur passes, glow parts, alpha
//! planes, the bevel's height profile) keep 8-bit-visible precision at half the bytes, so the
//! viewer's display renders ([`crate::Gpu::render_display`]) take them as RGBA16F when
//! [`crate::GpuContext::set_half_intermediates`] is on. Everything an effect hands on (its
//! output, the layer buffers, caches) stays RGBA f32, and so does every render that is not a
//! viewer display render (exports, readbacks, tests): those keep their exact results.
//!
//! A kernel writes its `out` through a storage texture of one format, so the kernels that may
//! write a half image are built a second time with `out` declared `rgba16float` (on first use:
//! the bank costs nothing until a display render asks for it). Reading needs no variant
//! (`texture_2d<f32>` reads either format). A kernel left out of [`KERNELS`] that is handed a
//! half `out` marks the effect missing ([`crate::context::Enc::take_missing`]) and it renders on
//! the CPU: never a wrong image.

use std::collections::HashMap;

use crate::context::{init_resource, storage_tex_entry, tex_entry};

/// The half working-image format.
pub(crate) const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Kernels built with a half `out` (every other kernel writes RGBA f32 only).
pub(crate) const KERNELS: &[&str] = &[
    // Zeroing / test NaN fill of a pooled image (`Enc::image_as`, `Enc::scratch_as`).
    "fill",
    // Gaussian blur passes (`effects::box_passes_owned`).
    "box_h",
    "box_v",
    // Glow (`effects::glow`): the bright pass and its A&B colouring.
    "glow_bright",
    "glow_ab",
    // Deep Glow (`fx_deepglow`): the weighted pass sums.
    "fdg_sum",
    // Layer styles (`fx_styles`): alpha planes, packed planes, the bevel's height profile and
    // its packed (height, region). The distance field (squared distances up to 65 536) stays f32.
    "sty_alpha",
    "sty_pack",
    "sty_bev_height",
    "sty_bev_pack",
];

/// The `out` declaration in `kernels.wgsl` that the half module swaps.
const OUT_F32: &str = "@group(0) @binding(3) var out: texture_storage_2d<rgba32float, write>;";
const OUT_F16: &str = "@group(0) @binding(3) var out: texture_storage_2d<rgba16float, write>;";

/// The half kernels: their bind group layout (the kernels' one, `out` RGBA16F) and pipelines.
pub(crate) struct Bank {
    pub(crate) bgl: wgpu::BindGroupLayout,
    pipelines: HashMap<&'static str, wgpu::ComputePipeline>,
}

impl Bank {
    /// Build the half kernels from the compositor's kernel source (`source`). `metal` builds
    /// `box_h` from `box_h_serial`, as the f32 bank does (fast math reassociates the staged one).
    /// A kernel that fails to build is left out (its effects then render on the CPU when the
    /// half path asks for it); no kernel at all is an error.
    pub(crate) fn new(device: &wgpu::Device, source: &str, metal: bool) -> Result<Bank, String> {
        if cfg!(target_arch = "wasm32") {
            // Browser error scopes are asynchronous (`init_resource`): no lazy builds mid-frame.
            return Err("half working images are native only".into());
        }
        if !source.contains(OUT_F32) {
            return Err("kernel source has no rgba32float `out` to swap".into());
        }
        let src = source.replacen(OUT_F32, OUT_F16, 1);
        let module = init_resource(device, "half kernels shader module", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("effectcraft half kernels"), source: wgpu::ShaderSource::Wgsl(src.into()) })
        })?;
        let bgl = init_resource(device, "half kernels bind-group layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("effectcraft half kernels"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                        count: None,
                    },
                    tex_entry(1),
                    tex_entry(2),
                    storage_tex_entry(3, FORMAT),
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            })
        })?;
        let layout = init_resource(device, "half kernels pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("effectcraft half kernels"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            })
        })?;
        let mut pipelines = HashMap::new();
        for &e in KERNELS {
            let entry = if metal && e == "box_h" { "box_h_serial" } else { e };
            let built = init_resource(device, e, || {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(e),
                    layout: Some(&layout),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            });
            match built {
                Ok(p) => {
                    pipelines.insert(e, p);
                }
                Err(why) => log::warn!("gpu: half kernel {e} unavailable (its effects keep f32 working images): {why}"),
            }
        }
        if pipelines.is_empty() {
            return Err("no half kernel built".into());
        }
        log::info!("gpu: {} half-float effect kernels built", pipelines.len());
        Ok(Bank { bgl, pipelines })
    }

    pub(crate) fn get(&self, entry: &str) -> Option<&wgpu::ComputePipeline> {
        self.pipelines.get(entry)
    }

    /// Whether every kernel in `entries` has a half variant (an effect asks before it takes
    /// half images, so it never ends up half-way).
    pub(crate) fn has_all(&self, entries: &[&str]) -> bool {
        entries.iter().all(|e| self.pipelines.contains_key(e))
    }
}
