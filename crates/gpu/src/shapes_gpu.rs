//! Shape layers rasterised on the GPU (layer content on the GPU, milestone 1). The CPU's
//! `layer_raster` of a shape layer (tiny-skia coverage masks, then `shapes::paint`) cost 16–33 ms
//! a frame in heavy comps; here it becomes a plan on the CPU and three kernels
//! (`shaders/shapes.wgsl`), and the result is the layer's resident source image directly (no
//! upload, no readback).
//!
//! Why our own rasteriser and not vello (0.11, on the same wgpu 30, as FigCraft's canvas): the
//! CPU renderer is the pixel reference, and its coverage is tiny-skia's 4 × 4-sample scan
//! converter, which vello's analytic area coverage misses by up to ~30 levels on every edge
//! pixel (the GPU-vs-CPU tests allow 4). vello would also submit outside [`Enc`]'s encoder,
//! render into an 8-bit unpremultiplied texture (wrong for 16/32 bpc, lossy at 8), and bring its
//! own gradient model (After Effects' midpoints, diamond and highlight gradients don't map).
//! Here the CPU builds the very edges tiny-skia walks (`effectcraft_render::shapes::skia_scan`:
//! fixed-point lines, curves by its forward differencing) and the kernels count the same
//! samples, so the masks are the CPU's to the bit; colour, gradients and compositing follow
//! `shapes::paint`. Glyph outlines (text, milestone 2) are fills too and reuse all of it.
//!
//! Per draw: `sh_accum` (one invocation per edge chunk of ≤ 32 sub-scanlines) adds windings at
//! sample columns with atomics; `sh_row` (one workgroup per row) prefix-sums them, counts covered
//! samples into tiny-skia's 0–255 coverage, paints solid or gradient colour onto the layer image
//! and clears what it read. `sh_store` writes the image.
//!
//! Fallbacks: the plan declines (the CPU renders the layer as before) for masks, motion-blurred
//! shape sub-frames, non-Normal draw blend modes, Taper / Wave strokes, paths tiny-skia would clip
//! or tile, runaway edge counts; this module declines for buffers past the device's
//! storage-buffer limits or when the kernels didn't build. `EFFECTCRAFT_GPU_SHAPES=0` turns it off
//! (A/B).

use std::sync::{Arc, Mutex};

use effectcraft_project::Layer;
use effectcraft_render::shapes::gpu_plan::{PlanDraw, PlanPaint, RasterPlan};
use effectcraft_render::{ContentClip, EvalCtx, Renderer};

use crate::context::{Enc, GpuContext, GpuImage};

/// Bytes of the shader's `Draw` uniform (11 × vec4).
const DRAW_BYTES: usize = 176;
/// Edge chunks per `sh_accum` workgroup.
const ACCUM_GROUP: u32 = 64;

/// Whether shape layers rasterise here (`EFFECTCRAFT_GPU_SHAPES=0`: on the CPU, for A/B).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_GPU_SHAPES").as_deref() != Ok("0"))
}

/// Shape layers rasterised here (tests check the GPU path ran).
#[cfg(test)]
pub(crate) static RASTERISED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) struct Pipes {
    bgl: wgpu::BindGroupLayout,
    accum: wgpu::ComputePipeline,
    row: wgpu::ComputePipeline,
    store: wgpu::ComputePipeline,
}

/// The kernels per device (few devices live at once: the app's, a test's).
static PIPES: Mutex<Vec<(wgpu::Device, Arc<Result<Pipes, String>>)>> = Mutex::new(Vec::new());
const DEVICES_KEPT: usize = 4;

fn pipes(g: &GpuContext) -> Option<Arc<Result<Pipes, String>>> {
    let mut all = PIPES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, p)) = all.iter().find(|(d, _)| *d == g.device) {
        return Some(p.clone());
    }
    let built = Arc::new(build(g));
    if let Err(e) = built.as_ref() {
        log::warn!("gpu: shape kernels: {e}");
    } else {
        g.static_pipeline_count.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
    }
    if all.len() >= DEVICES_KEPT {
        all.remove(0);
    }
    all.push((g.device.clone(), built.clone()));
    Some(built)
}

fn build(g: &GpuContext) -> Result<Pipes, String> {
    crate::context::init_resource(&g.device, "shape raster pipelines", || {
        let d = &g.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("effectcraft shapes"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shapes.wgsl").into()),
        });
        let buffer = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("effectcraft shapes"),
            entries: &[
                buffer(0, wgpu::BufferBindingType::Uniform),
                buffer(1, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer(2, wgpu::BufferBindingType::Storage { read_only: false }),
                buffer(3, wgpu::BufferBindingType::Storage { read_only: false }),
                buffer(4, wgpu::BufferBindingType::Storage { read_only: true }),
                crate::context::storage_tex_entry(5, crate::context::FORMAT),
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("effectcraft shapes"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipe = |entry: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Pipes { accum: pipe("sh_accum"), row: pipe("sh_row"), store: pipe("sh_store"), bgl }
    })
}

/// The `Draw` uniform of one draw (or of the final store, `draw: None`).
fn uniform(plan: &RasterPlan, draw: Option<&PlanDraw>, edge_at: u32, grad_at: u32) -> [u8; DRAW_BYTES] {
    let mut u = [[0u32; 4]; 3];
    let mut f = [[0f32; 4]; 8];
    u[2][0] = plan.width;
    u[2][1] = plan.height;
    if let Some(d) = draw {
        let [x0, y0, w, h] = d.rect;
        u[0] = [x0, y0, w, h];
        let gradient = matches!(d.paint, PlanPaint::Gradient(_));
        u[1] = [0, d.edges.len() as u32, edge_at, u32::from(d.even_odd) | (u32::from(gradient) << 1)];
        u[2][2] = grad_at;
        f[1][0] = d.opacity;
        match &d.paint {
            PlanPaint::Solid(c) => f[0] = *c,
            PlanPaint::Gradient(g) => {
                u[2][3] = g.kind;
                f[1][1] = g.len2 as f32;
                f[1][2] = if g.affine.is_some() { 1.0 } else { 0.0 };
                f[1][3] = if g.focal.is_some() { 1.0 } else { 0.0 };
                let l = g.to_local;
                f[2] = [l[0] as f32, l[1] as f32, l[2] as f32, 0.0];
                f[3] = [l[3] as f32, l[4] as f32, l[5] as f32, 0.0];
                if let Some(a) = g.affine {
                    f[4] = [a[0] as f32, a[1] as f32, a[2] as f32, 0.0];
                    f[5] = [a[3] as f32, a[4] as f32, a[5] as f32, 0.0];
                }
                f[6] = [g.start[0] as f32, g.start[1] as f32, g.dir[0] as f32, g.dir[1] as f32];
                if let Some(p) = g.focal {
                    f[7] = [p[0] as f32, p[1] as f32, 0.0, 0.0];
                }
            }
        }
    }
    let mut out = [0u8; DRAW_BYTES];
    let words = u.iter().flatten().copied().chain(f.iter().flatten().map(|x| x.to_bits()));
    for (dst, x) in out.chunks_exact_mut(4).zip(words) {
        dst.copy_from_slice(&x.to_le_bytes());
    }
    out
}

/// A gradient's words for the shader's `gradient_at` (see `shapes.wgsl`).
fn gradient_words(paint: &PlanPaint, out: &mut Vec<f32>) {
    let PlanPaint::Gradient(g) = paint else { return };
    out.extend([g.colors.len() as f32, g.opacities.len() as f32]);
    for (p, c, m) in &g.colors {
        out.extend([*p, c[0], c[1], c[2], c[3], *m]);
    }
    for (p, a, m) in &g.opacities {
        out.extend([*p, *a, *m]);
    }
}

/// Rasterise `plan` into a new image of its size. `None`: the kernels are missing, or the
/// buffers would pass the device's limits (the CPU renders the layer then).
pub(crate) fn raster(e: &mut Enc, plan: &RasterPlan) -> Option<GpuImage> {
    let (w, h) = (plan.width, plan.height);
    if w == 0 || h == 0 || !e.g.fits(w, h) {
        return None;
    }
    if plan.draws.is_empty() {
        return Some(e.image(w, h));
    }
    let pipes = pipes(e.g)?;
    let Ok(p) = pipes.as_ref() else { return None };
    let limits = e.g.device.limits();
    let max_bind = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
    let max_groups = u64::from(limits.max_compute_workgroups_per_dimension);
    let img_bytes = u64::from(w) * u64::from(h) * 16;
    // Four packed sub-scanlines per mask pixel.
    let acc_bytes = plan.draws.iter().map(|d| u64::from(d.rect[2]) * u64::from(d.rect[3]) * 16).max().unwrap_or(16).max(16);
    let edge_count = plan.edge_count() as u64;
    if img_bytes > max_bind
        || acc_bytes > max_bind
        || edge_count.saturating_mul(16) > max_bind
        || u64::from(h) > max_groups
        || plan.draws.iter().any(|d| (d.edges.len() as u64).div_ceil(u64::from(ACCUM_GROUP)) > max_groups)
    {
        return None;
    }
    // Edges and gradient stops of every draw, each draw's offsets into them.
    let mut edges: Vec<i32> = Vec::with_capacity(plan.edge_count() * 4);
    let mut grad: Vec<f32> = vec![0.0; 4];
    let mut offsets = Vec::with_capacity(plan.draws.len());
    for d in &plan.draws {
        let at = (edges.len() / 4) as u32;
        edges.extend(d.edges.iter().flatten());
        let g_at = grad.len() as u32;
        gradient_words(&d.paint, &mut grad);
        offsets.push((at, g_at));
    }
    if edges.is_empty() {
        edges.extend([0; 4]);
    }
    let g = e.g;
    let storage = wgpu::BufferUsages::STORAGE;
    let edge_buf = g.buffer_init(Some("shape edges"), bytemuck::cast_slice(&edges), storage);
    let grad_buf = g.buffer_init(Some("shape gradients"), bytemuck::cast_slice(&grad), storage);
    // New buffers start zeroed; `sh_row` zeroes what it reads, so the draws share one.
    let acc = g.device.create_buffer(&wgpu::BufferDescriptor { label: Some("shape coverage"), size: acc_bytes, usage: storage, mapped_at_creation: false });
    let img = g.device.create_buffer(&wgpu::BufferDescriptor { label: Some("shape layer"), size: img_bytes, usage: storage, mapped_at_creation: false });
    let out = e.scratch(w, h);
    let bind = |e: &mut Enc, draw: Option<&PlanDraw>, edge_at: u32, grad_at: u32| {
        let u = e.g.buffer_init(Some("shape draw"), &uniform(plan, draw, edge_at, grad_at), wgpu::BufferUsages::UNIFORM);
        e.g.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("effectcraft shapes"),
            layout: &p.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: edge_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: acc.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: img.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: grad_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&out.view) },
            ],
        })
    };
    for (d, &(edge_at, grad_at)) in plan.draws.iter().zip(&offsets) {
        if d.edges.is_empty() || d.rect[2] == 0 || d.rect[3] == 0 {
            continue;
        }
        let bg = bind(e, Some(d), edge_at, grad_at);
        let groups = (d.edges.len() as u32).div_ceil(ACCUM_GROUP);
        // `custom_pixels` dispatches size / 16 workgroups along each axis.
        e.custom_pixels("sh_accum", &p.accum, &bg, (groups.saturating_mul(16), 1));
        e.custom_pixels("sh_row", &p.row, &bg, (1, d.rect[3].saturating_mul(16)));
    }
    let bg = bind(e, None, 0, 0);
    e.custom_pixels("sh_store", &p.store, &bg, (w, h));
    #[cfg(test)]
    RASTERISED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}

/// [`Renderer::resident_source`] of a shape layer made here: its image, offset and scale (effect
/// space), converted to the working space and quantised as the CPU's `masked_source`. `None`:
/// render it on the CPU.
pub(crate) fn resident_source(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer, clip: ContentClip) -> Option<(GpuImage, [f64; 2], f64)> {
    if !enabled() || !r.shape_raster_eligible(ctx, layer) {
        return None;
    }
    let _profile = effectcraft_render::profile::span("layer_raster_gpu");
    let plan = {
        let _profile = effectcraft_render::profile::span("layer_raster_plan");
        r.shape_raster_plan(ctx, layer, clip)?
    };
    let mut img = raster(e, &plan)?;
    let pipe = r.pipe();
    if let Some(c) = pipe.authored_in() {
        img = crate::ops::convert(e, &img, &c);
    }
    if let Some(l) = pipe.levels {
        img = crate::ops::quantize(e, &img, l);
    }
    Some((img, plan.offset, plan.scale))
}
