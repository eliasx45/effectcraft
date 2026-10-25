//! Mask feathers on the GPU (MASK-GPU1, [`effectcraft_render::Accelerator::mask_feather`]).
//!
//! The CPU still draws each mask's coverage (path fill, expansion, variable-width feather) into
//! its padded plane and combines the masks (modes, inversion, opacity); the feather blur, the
//! expensive part (a 177 px Gaussian feather on a full-HD layer: ~26 ms on the CPU), runs here.
//! The plane goes up as `R32Float`, is spread into a working image (`mask_expand`), blurred by
//! the very box passes the Gaussian / Box Blur effects use ([`crate::effects`]: the CPU's radii,
//! transparent edges; a Gaussian is 3 box passes per axis, the Linear falloff one), and only the
//! layer's window comes back, four values a texel (`mask_pack`). The result matches the CPU blur
//! to float rounding (the horizontal passes restart their running sums every 256 px, the CPU's
//! run along the whole row).

use effectcraft_render::masks::FeatherBlur;

use crate::context::{Enc, GpuContext, init_resource};
use crate::effects::{Precision, box_passes_owned, gaussian_blur_owned};

/// Pipelines (built on first use, kept in [`GpuContext`]).
pub(crate) struct Pipes {
    layout: wgpu::BindGroupLayout,
    expand: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
}

impl Pipes {
    fn new(device: &wgpu::Device) -> Pipes {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mask feather"),
            source: wgpu::ShaderSource::Wgsl(include_str!("masks_gpu.wgsl").into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mask feather"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(16) },
                ),
                entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(
                    2,
                    wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                ),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mask feather"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |name: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(name),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Pipes { expand: pipeline("mask_expand"), pack: pipeline("mask_pack"), layout }
    }
}

fn pipes(g: &GpuContext) -> Result<&Pipes, String> {
    g.masks
        .get_or_init(|| {
            let p = init_resource(&g.device, "mask feather pipelines", || Pipes::new(&g.device))?;
            g.static_pipeline_count.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
            Ok(p)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// One dispatch of `pipe` writing `out` (its size) from `src`, with the crop uniform.
fn run(e: &mut Enc, name: &str, pipe: &wgpu::ComputePipeline, p: &Pipes, src: &wgpu::TextureView, out: &crate::context::GpuImage, uniform: [u32; 4]) {
    let g = e.g;
    let buf = crate::context::buffer_init(&g.device, &g.queue, Some("mask feather"), bytemuck::cast_slice(&uniform), wgpu::BufferUsages::UNIFORM);
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(name),
        layout: &p.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&out.view) },
        ],
    });
    e.custom_pixels(name, pipe, &bg, (out.width, out.height));
}

/// `plane` (`width`×`height` coverage) blurred by `blur`, cropped to `crop` (x, y, width,
/// height). `None` (the CPU blurs it) when the device can't wait for a readback, the plane is
/// too big for it, a kernel is missing, or the inputs don't add up.
pub(crate) fn feather(g: &GpuContext, plane: &[f32], width: u32, height: u32, blur: FeatherBlur, crop: [u32; 4]) -> Option<Vec<f32>> {
    let [cx, cy, cw, ch] = crop;
    let n = (width as usize).checked_mul(height as usize)?;
    if plane.len() != n || cw == 0 || ch == 0 || cx.checked_add(cw)? > width || cy.checked_add(ch)? > height {
        return None;
    }
    let packed_w = cw.div_ceil(4);
    if !g.can_wait() || !g.fits(width, height) || !g.fits(packed_w, ch) {
        return None;
    }
    let p = pipes(g).map_err(|e| log::debug!(target: "effectcraft_gpu", "mask feathers stay on the CPU: {e}")).ok()?;
    let mut e = Enc::new(g);
    let texture = g.plane_texture(width, height, wgpu::TextureFormat::R32Float, "mask coverage plane");
    let bytes: &[u8] = bytemuck::cast_slice(plane);
    g.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        bytes,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width.checked_mul(4)?), rows_per_image: Some(height) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    g.transfers.count(true, bytes.len());
    let view = texture.create_view(&Default::default());
    let spread = e.scratch(width, height);
    run(&mut e, "mask_expand", &p.expand, p, &view, &spread, [0; 4]);
    e.stage(texture);
    let blurred = match blur {
        FeatherBlur::Box { rx, ry } => box_passes_owned(&mut e, "box_h", spread, &[rx], &[ry], false, Precision::F32),
        FeatherBlur::Gaussian { sx, sy } => gaussian_blur_owned(&mut e, spread, sx, sy, false, Precision::F32),
    };
    let packed = e.scratch(packed_w, ch);
    run(&mut e, "mask_pack", &p.pack, p, &blurred.view, &packed, [cx, cy, cw, ch]);
    drop(blurred);
    if e.take_missing() {
        return None;
    }
    let img = e.download(&packed)?;
    if e.take_missing() {
        return None;
    }
    let (cw, pw) = (cw as usize, packed_w as usize);
    let mut out = Vec::new();
    out.try_reserve_exact(cw.checked_mul(ch as usize)?).ok()?;
    for row in img.data.chunks_exact(pw.max(1)) {
        out.extend(row.iter().flatten().take(cw));
    }
    (out.len() == cw.checked_mul(ch as usize)?).then_some(out)
}
