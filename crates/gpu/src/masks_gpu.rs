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
//!
//! MASK-GPU2 ([`apply`]): for shape and text layers rasterised on the GPU (`shapes_gpu`) the
//! whole mask stack runs here, with no CPU plane and no readback: each mask's coverage is drawn
//! by the shape kernels from `effectcraft_render::masks::gpu_plan`'s scan edges, feathered by the
//! same box passes, and combined into the resident layer image. Variable-width feathers, mask
//! motion blur on animated paths and negative or distance-based expansions stay on the CPU (the
//! plan declines and the layer renders there). Other layers still mask on the CPU, with the
//! feather blurred here ([`feather`]).

use effectcraft_render::masks::FeatherBlur;
use effectcraft_render::masks::gpu_plan::{GpuMaskPlan, MaskDraw};
use effectcraft_render::shapes::gpu_plan::{PlanDraw, PlanPaint, RasterPlan};
use effectcraft_render::shapes::skia_scan::ScanEdge;

use crate::context::{Enc, GpuContext, init_resource};
use crate::effects::{Precision, box_passes_owned, gaussian_blur_owned};

/// Layers whose masks were applied here (tests check the GPU path ran).
#[cfg(test)]
pub(crate) static APPLIED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Pipelines (built on first use, kept in [`GpuContext`]).
pub(crate) struct Pipes {
    layout: wgpu::BindGroupLayout,
    expand: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    combine: wgpu::ComputePipeline,
    apply: wgpu::ComputePipeline,
}

/// Bytes of the shader's `MaskFeatherP` uniform.
const UNIFORM_WORDS: usize = 12;

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
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_WORDS as u64 * 4),
                    },
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
                entry(
                    3,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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
        Pipes { expand: pipeline("mask_expand"), pack: pipeline("mask_pack"), combine: pipeline("mask_combine"), apply: pipeline("mask_apply"), layout }
    }
}

fn pipes(g: &GpuContext) -> Result<&Pipes, String> {
    g.masks
        .get_or_init(|| {
            let p = init_resource(&g.device, "mask feather pipelines", || Pipes::new(&g.device))?;
            g.static_pipeline_count.fetch_add(4, std::sync::atomic::Ordering::Relaxed);
            Ok(p)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// One dispatch of `pipe` writing `out` (its size) from `src` (and `aux`), with the uniform.
#[allow(clippy::too_many_arguments)]
fn run(
    e: &mut Enc,
    name: &str,
    pipe: &wgpu::ComputePipeline,
    p: &Pipes,
    src: &wgpu::TextureView,
    aux: Option<&wgpu::TextureView>,
    out: &crate::context::GpuImage,
    uniform: [u32; UNIFORM_WORDS],
) {
    let g = e.g;
    let buf = crate::context::buffer_init(&g.device, &g.queue, Some("mask feather"), bytemuck::cast_slice(&uniform), wgpu::BufferUsages::UNIFORM);
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(name),
        layout: &p.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&out.view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(aux.unwrap_or(src)) },
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
    run(&mut e, "mask_expand", &p.expand, p, &view, None, &spread, [0; UNIFORM_WORDS]);
    e.stage(texture);
    let blurred = match blur {
        FeatherBlur::Box { rx, ry } => box_passes_owned(&mut e, "box_h", spread, &[rx], &[ry], false, Precision::F32),
        FeatherBlur::Gaussian { sx, sy } => gaussian_blur_owned(&mut e, spread, sx, sy, false, Precision::F32),
    };
    let packed = e.scratch(packed_w, ch);
    run(&mut e, "mask_pack", &p.pack, p, &blurred.view, None, &packed, words([cx, cy], [cw, ch], 0, false, 0.0, false, 0.0));
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

/// The `MaskFeatherP` words.
fn words(off: [u32; 2], size: [u32; 2], mode: u32, inverted: bool, opacity: f32, first: bool, init: f32) -> [u32; UNIFORM_WORDS] {
    let mut w = [0u32; UNIFORM_WORDS];
    w[..4].copy_from_slice(&[off[0], off[1], size[0], size[1]]);
    w[4..9].copy_from_slice(&[mode, u32::from(inverted), opacity.to_bits(), u32::from(first), init.to_bits()]);
    w
}

/// MASK-GPU2: the layer's masks ([`GpuMaskPlan`], [`effectcraft_render::masks::combined`]) drawn
/// and applied to `img` on the GPU: each mask's coverage is rasterised by the shape kernels
/// ([`crate::shapes_gpu::raster`]: tiny-skia's coverage, white at full opacity, so alpha is the
/// coverage; a positive expansion's ring is a second draw over it, `c + r − c r`), feathered by
/// the box passes, cropped, inverted, scaled by its opacity and combined by its mode into an
/// accumulator (`mask_combine`); `mask_apply` multiplies every channel by the result, as
/// `Image::mul_mask`. `None`: the kernels are missing or a plane doesn't fit (the CPU renders
/// the layer).
pub(crate) fn apply(e: &mut Enc, plan: &GpuMaskPlan, img: crate::context::GpuImage) -> Option<crate::context::GpuImage> {
    if plan.masks.is_empty() {
        return Some(img);
    }
    let (w, h) = (img.width, img.height);
    if (w, h) != (plan.width, plan.height) || !e.g.fits(w, h) {
        return None;
    }
    let p = pipes(e.g).map_err(|err| log::debug!(target: "effectcraft_gpu", "masks stay on the CPU: {err}")).ok()?;
    let white = |edges: &[ScanEdge], m: &MaskDraw| PlanDraw {
        rect: [0, 0, m.plane[0], m.plane[1]],
        even_odd: false,
        edges: edges.to_vec(),
        opacity: 1.0,
        paint: PlanPaint::Solid([1.0; 4]),
    };
    let mut acc: Option<crate::context::GpuImage> = None;
    for m in &plan.masks {
        if m.pad[0].checked_mul(2)?.checked_add(w)? != m.plane[0] || m.pad[1].checked_mul(2)?.checked_add(h)? != m.plane[1] {
            return None;
        }
        let draws = [&m.fill, &m.ring].into_iter().filter(|d| !d.is_empty()).map(|d| white(d, m)).collect();
        let raster = RasterPlan { width: m.plane[0], height: m.plane[1], offset: [0.0; 2], scale: 1.0, draws };
        let cov = crate::shapes_gpu::raster(e, &raster)?;
        let cov = match m.feather {
            Some(FeatherBlur::Box { rx, ry }) => box_passes_owned(e, "box_h", cov, &[rx], &[ry], false, Precision::F32),
            Some(FeatherBlur::Gaussian { sx, sy }) => gaussian_blur_owned(e, cov, sx, sy, false, Precision::F32),
            None => cov,
        };
        let next = e.scratch(w, h);
        let u = words(m.pad, [w, h], m.mode, m.inverted, m.opacity, acc.is_none(), plan.init);
        run(e, "mask_combine", &p.combine, p, &cov.view, acc.as_ref().map(|a| &a.view), &next, u);
        acc = Some(next);
    }
    let acc = acc?;
    let out = e.scratch(w, h);
    run(e, "mask_apply", &p.apply, p, &img.view, Some(&acc.view), &out, [0; UNIFORM_WORDS]);
    Some(out)
}
