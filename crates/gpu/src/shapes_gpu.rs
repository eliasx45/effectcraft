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
//! Every draw of a layer at once, in three dispatches (TEXT-GPU2; one pair per draw cost more to
//! encode than the CPU took for a motion-blurred line of text): `sh_accum` (one invocation per
//! edge chunk of ≤ 32 sub-scanlines) adds windings at sample columns with atomics, each draw in
//! its own region; `sh_cov` (one workgroup per row of a draw) prefix-sums them and counts
//! covered samples into tiny-skia's 0–255 coverage; `sh_compose` (one invocation per pixel)
//! paints solid or gradient colour of the draws covering its 16 × 16 tile in order, each group
//! of draws (a motion-blur sub-sample) on its own and the groups summed with their weights. A
//! layer whose coverage regions pass [`ACC_BUDGET`] goes in batches, the pixels carried between.
//!
//! Solid layers (TEXT-GPU2) are filled here too instead of uploading a frame of one colour.
//!
//! Masks (MASK-GPU2) are drawn and applied here too (`masks_gpu::apply`) when
//! `effectcraft_render::masks::gpu_plan` can plan them.
//!
//! Fallbacks: the plan declines (the CPU renders the layer as before) for masks it can't plan, motion-blurred
//! shape sub-frames (text sub-frames are drawn here), non-Normal draw blend modes, Taper / Wave strokes, masks tiny-skia would tile
//! (over 8191 px), runaway edge counts; this module declines for buffers past the device's
//! storage-buffer limits or when the kernels didn't build. `EFFECTCRAFT_GPU_SHAPES=0` turns it off
//! (A/B).
//!
//! Text layers (TEXT-GPU1) come the same way: `effectcraft_render::text::gpu_plan` makes the
//! glyph runs' fills and round-joined strokes into draws of solid colour, Normal in drawing
//! order, as `text::render` draws them; a motion-blurred text layer whose characters animate
//! (TEXT-GPU2) is one group of draws per shutter sub-sample, as `blurred_source` averages them.
//! `EFFECTCRAFT_GPU_TEXT=0` turns that off.

use std::sync::{Arc, Mutex};

use effectcraft_project::{Layer, LayerSource};
use effectcraft_render::shapes::gpu_plan::{PlanDraw, PlanPaint, RasterPlan};
use effectcraft_render::{ContentClip, EvalCtx, Renderer};

use crate::context::{Enc, GpuContext, GpuImage};

/// Bytes of the shader's `DrawRec` (12 × vec4) and `Batch` uniform (3 × vec4).
const DRAW_BYTES: usize = 192;
const BATCH_BYTES: usize = 48;
/// Edge chunks per `sh_accum` workgroup.
const ACCUM_GROUP: u32 = 64;
/// Workgroups along x in `sh_accum` / `sh_cov` (the rest go along y).
const GROUPS_ACROSS: u32 = 16384;
/// Coverage regions (bytes) one batch draws at most.
const ACC_BUDGET: u64 = 256 << 20;
/// Pixels per side of a compose tile.
const TILE: u32 = 16;

/// Whether shape layers rasterise here (`EFFECTCRAFT_GPU_SHAPES=0`: on the CPU, for A/B).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_GPU_SHAPES").as_deref() != Ok("0"))
}

/// Whether text layers rasterise here (`EFFECTCRAFT_GPU_TEXT=0`: on the CPU, for A/B).
pub(crate) fn text_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_GPU_TEXT").as_deref() != Ok("0"))
}

/// Whether the walk may rasterise this layer here (then the CPU pre-pass leaves it alone; the
/// plan can still decline, and the walk then renders it on the CPU).
pub(crate) fn eligible(r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> bool {
    match layer.source {
        LayerSource::Shape => enabled() && r.shape_raster_eligible(ctx, layer),
        LayerSource::Text => text_enabled() && r.text_raster_eligible(ctx, layer),
        LayerSource::Solid { .. } => enabled() && r.solid_fill_eligible(layer),
        _ => false,
    }
}

/// Why a shape, text or solid layer the walk asked [`resident_source`] for renders on the CPU
/// instead (RI1): the reason its plan left ([`effectcraft_render::cpu_why::take`]), else why it
/// wasn't eligible. Called only when it declined.
pub(crate) fn why_cpu(r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> effectcraft_render::CpuWhy {
    use effectcraft_render::cpu_why as why;
    if let Some(w) = why::take() {
        return w;
    }
    let off = match layer.source {
        LayerSource::Text => !text_enabled(),
        _ => !enabled(),
    };
    if off {
        return why::GPU_SHAPES_OFF;
    }
    if !effectcraft_render::masks::gpu_plan::gpu_eligible(layer) {
        return why::MASK_FEATHER;
    }
    match layer.source {
        // Its masks could be drawn: its contents' motion blur sub-frames weren't.
        LayerSource::Shape if !r.shape_raster_eligible(ctx, layer) => why::SHAPE_MOTION_BLUR,
        LayerSource::Text if !r.text_raster_eligible(ctx, layer) => why::TEXT_ANIMATORS,
        _ => why::RASTER_DECLINED,
    }
}

/// Shape layers rasterised here (tests check the GPU path ran).
#[cfg(test)]
pub(crate) static RASTERISED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Text layers rasterised here.
#[cfg(test)]
pub(crate) static RASTERISED_TEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) struct Pipes {
    bgl: wgpu::BindGroupLayout,
    accum: wgpu::ComputePipeline,
    cov: wgpu::ComputePipeline,
    compose: wgpu::ComputePipeline,
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
        let ro = wgpu::BufferBindingType::Storage { read_only: true };
        let rw = wgpu::BufferBindingType::Storage { read_only: false };
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("effectcraft shapes"),
            entries: &[
                buffer(0, wgpu::BufferBindingType::Uniform),
                buffer(1, ro),
                buffer(2, rw),
                buffer(3, ro),
                buffer(4, ro),
                buffer(5, ro),
                buffer(6, rw),
                crate::context::storage_tex_entry(7, crate::context::FORMAT),
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
        Pipes { accum: pipe("sh_accum"), cov: pipe("sh_cov"), compose: pipe("sh_compose"), bgl }
    })
}

/// A draw's `DrawRec`: its coverage region starts at word `acc_base` of its batch's buffer.
fn draw_record(d: &PlanDraw, acc_base: u32, grad_at: u32) -> [u8; DRAW_BYTES] {
    let mut u = [[0u32; 4]; 3];
    let mut f = [[0f32; 4]; 9];
    u[0] = d.rect;
    let gradient = matches!(d.paint, PlanPaint::Gradient(_));
    u[1] = [acc_base, 0, u32::from(d.even_odd) | (u32::from(gradient) << 1), grad_at];
    u[2][1] = d.group;
    f[1][0] = d.opacity;
    f[8][0] = d.weight;
    match &d.paint {
        PlanPaint::Solid(c) => f[0] = *c,
        PlanPaint::Gradient(g) => {
            u[2][0] = g.kind;
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
    let mut out = [0u8; DRAW_BYTES];
    let words = u.iter().flatten().copied().chain(f.iter().flatten().map(|x| x.to_bits()));
    for (dst, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
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

/// Bytes of a draw's coverage region (4 packed sub-scanlines per pixel).
fn acc_bytes(d: &PlanDraw) -> u64 {
    u64::from(d.rect[2]) * u64::from(d.rect[3]) * 16
}

/// Consecutive draws drawn in one pass of the kernels, `[first, end)` into the drawn draws.
/// With groups a batch holds whole groups (each composited on its own before it is summed).
fn batches(draws: &[&PlanDraw], groups: bool, budget: u64) -> Option<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let (mut first, mut bytes) = (0usize, 0u64);
    let mut i = 0;
    while i < draws.len() {
        // The next unit: one draw, or a whole group.
        let mut j = i + 1;
        if groups {
            while draws.get(j).is_some_and(|d| draws.get(i).is_some_and(|a| a.group == d.group)) {
                j += 1;
            }
        }
        let unit: u64 = draws.get(i..j)?.iter().map(|d| acc_bytes(d)).sum();
        if unit > budget {
            return None;
        }
        if bytes + unit > budget && i > first {
            out.push((first, i));
            first = i;
            bytes = 0;
        }
        bytes += unit;
        i = j;
    }
    if draws.len() > first {
        out.push((first, draws.len()));
    }
    Some(out)
}

/// Rasterise `plan` into a new image of its size. `None`: the kernels are missing, or the
/// buffers would pass the device's limits (the CPU renders the layer then).
pub(crate) fn raster(e: &mut Enc, plan: &RasterPlan) -> Option<GpuImage> {
    let (w, h) = (plan.width, plan.height);
    if w == 0 || h == 0 || !e.g.fits(w, h) {
        effectcraft_render::cpu_why::declined(effectcraft_render::cpu_why::GPU_BUFFER_LIMITS);
        return None;
    }
    let drawn: Vec<&PlanDraw> = plan.draws.iter().filter(|d| !d.edges.is_empty() && d.rect[2] > 0 && d.rect[3] > 0).collect();
    if drawn.is_empty() {
        return Some(e.image(w, h));
    }
    let pipes = pipes(e.g)?;
    let Ok(p) = pipes.as_ref() else { return None };
    let limits = e.g.device.limits();
    let max_bind = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
    let max_groups = u64::from(limits.max_compute_workgroups_per_dimension);
    if limits.max_storage_buffers_per_shader_stage < 6 || u64::from(w) * u64::from(h) * 16 > max_bind {
        effectcraft_render::cpu_why::declined(effectcraft_render::cpu_why::GPU_BUFFER_LIMITS);
        return None;
    }
    let groups = drawn.iter().any(|d| d.group != drawn[0].group || d.weight != 1.0);
    let batches = batches(&drawn, groups, ACC_BUDGET.min(max_bind))?;
    let edge_count: u64 = drawn.iter().map(|d| d.edges.len() as u64).sum();
    if edge_count.saturating_mul(16) > max_bind || drawn.len() >= 1 << 24 {
        effectcraft_render::cpu_why::declined(effectcraft_render::cpu_why::GPU_BUFFER_LIMITS);
        return None;
    }
    // Edges (tagged with their draw), draw records and gradient stops of every draw; each draw's
    // coverage region within its batch.
    let mut edges: Vec<i32> = Vec::with_capacity(edge_count as usize * 4);
    let mut records: Vec<u8> = Vec::with_capacity(drawn.len() * DRAW_BYTES);
    let mut grad: Vec<f32> = vec![0.0; 4];
    let mut edge_ranges = Vec::with_capacity(batches.len());
    for &(first, end) in &batches {
        let e0 = edges.len() / 4;
        let mut base = 0u64;
        for (i, d) in drawn.iter().enumerate().take(end).skip(first) {
            let tag = (i as i32) << 7;
            edges.extend(d.edges.iter().flat_map(|x| [x[0], x[1] | tag, x[2], x[3]]));
            let g_at = grad.len() as u32;
            gradient_words(&d.paint, &mut grad);
            records.extend(draw_record(d, u32::try_from(base / 4).ok()?, g_at));
            base += acc_bytes(d);
        }
        edge_ranges.push((e0 as u32, (edges.len() / 4 - e0) as u32, base));
    }
    let g = e.g;
    let storage = wgpu::BufferUsages::STORAGE;
    let edge_buf = g.buffer_init(Some("shape edges"), bytemuck::cast_slice(&edges), storage);
    let draw_buf = g.buffer_init(Some("shape draws"), &records, storage);
    let grad_buf = g.buffer_init(Some("shape gradients"), bytemuck::cast_slice(&grad), storage);
    // The pixels between batches; new buffers start zeroed.
    let state = g.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shape layer"),
        size: u64::from(w) * u64::from(h) * 16,
        usage: storage,
        mapped_at_creation: false,
    });
    let out = e.scratch(w, h);
    let tiles = [w.div_ceil(TILE), h.div_ceil(TILE)];
    let n_tiles = tiles[0] as usize * tiles[1] as usize;
    let last = batches.len().saturating_sub(1);
    for (bi, (&(first, end), &(e0, en, acc))) in batches.iter().zip(&edge_ranges).enumerate() {
        // Rows: (draw, row within it); tile ranges, then each tile's draws in order.
        let mut index: Vec<u32> = Vec::new();
        let mut lists: Vec<Vec<u32>> = vec![Vec::new(); n_tiles];
        for (i, d) in drawn.iter().enumerate().take(end).skip(first) {
            let [x0, y0, dw, dh] = d.rect;
            for y in 0..dh {
                index.extend([i as u32, y]);
            }
            let (tx0, ty0) = (x0 / TILE, y0 / TILE);
            let tx1 = (x0.saturating_add(dw).min(w)).div_ceil(TILE).min(tiles[0]);
            let ty1 = (y0.saturating_add(dh).min(h)).div_ceil(TILE).min(tiles[1]);
            for ty in ty0..ty1 {
                for tx in tx0..tx1 {
                    if let Some(l) = lists.get_mut(ty as usize * tiles[0] as usize + tx as usize) {
                        l.push(i as u32);
                    }
                }
            }
        }
        let rows = (index.len() / 2) as u32;
        let tile_at = index.len() as u32;
        let mut at = 0u32;
        index.push(0);
        for l in &lists {
            at = at.checked_add(l.len() as u32)?;
            index.push(at);
        }
        let list_at = index.len() as u32;
        index.extend(lists.into_iter().flatten());
        if (index.len() as u64).saturating_mul(4) > max_bind {
            return None;
        }
        let accum_groups = en.div_ceil(ACCUM_GROUP).max(1);
        let accum_across = accum_groups.min(GROUPS_ACROSS);
        let cov_across = rows.clamp(1, GROUPS_ACROSS);
        if u64::from(accum_groups.div_ceil(accum_across)) > max_groups || u64::from(rows.max(1).div_ceil(cov_across)) > max_groups {
            return None;
        }
        let flags = u32::from(groups) | (u32::from(bi == last) << 1);
        let words: [u32; 12] = [w, h, tiles[0], rows, e0, en, cov_across, flags, tile_at, list_at, accum_across, 0];
        let mut uniform = [0u8; BATCH_BYTES];
        for (dst, x) in uniform.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            dst.copy_from_slice(&x.to_le_bytes());
        }
        let u = g.buffer_init(Some("shape batch"), &uniform, wgpu::BufferUsages::UNIFORM);
        let index_buf = g.buffer_init(Some("shape index"), bytemuck::cast_slice(&index), storage);
        let acc_buf =
            g.device.create_buffer(&wgpu::BufferDescriptor { label: Some("shape coverage"), size: acc.max(16), usage: storage, mapped_at_creation: false });
        let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("effectcraft shapes"),
            layout: &p.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: edge_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: acc_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: draw_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: grad_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: index_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: state.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&out.view) },
            ],
        });
        // `custom_pixels` dispatches size / 16 workgroups along each axis.
        let wg = |across: u32, total: u32| (across.saturating_mul(16), total.div_ceil(across).saturating_mul(16));
        e.custom_pixels("sh_accum", &p.accum, &bg, wg(accum_across, accum_groups));
        e.custom_pixels("sh_cov", &p.cov, &bg, wg(cov_across, rows.max(1)));
        e.custom_pixels("sh_compose", &p.compose, &bg, (w, h));
    }
    #[cfg(test)]
    RASTERISED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}

/// [`Renderer::resident_source`] of a shape or text layer made here: its image, offset and
/// scale (effect space), converted to the working space and quantised as the CPU's
/// `masked_source`. `None`: render it on the CPU.
pub(crate) fn resident_source(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer, clip: ContentClip) -> Option<(GpuImage, [f64; 2], f64)> {
    if !eligible(r, ctx, layer) {
        return None;
    }
    if matches!(layer.source, LayerSource::Solid { .. }) {
        return solid_source(e, r, ctx, layer);
    }
    let text = matches!(layer.source, LayerSource::Text);
    let _profile = effectcraft_render::profile::span(if text { "text_raster_gpu" } else { "layer_raster_gpu" });
    let plan = {
        let _profile = effectcraft_render::profile::span(if text { "text_raster_plan" } else { "layer_raster_plan" });
        if text { r.text_raster_plan(ctx, layer, clip)? } else { r.shape_raster_plan(ctx, layer, clip)? }
    };
    // Masks as the CPU's `masked_source` applies them (after the conversion, before quantising),
    // planned first: a mask the GPU can't draw sends the whole layer to the CPU.
    let masks = if layer.masks().is_some_and(|m| !m.children.is_empty()) {
        let _profile = effectcraft_render::profile::span("mask_plan");
        Some(r.mask_gpu_plan(ctx, layer, plan.width, plan.height, plan.offset, plan.scale)?)
    } else {
        None
    };
    let mut img = raster(e, &plan)?;
    #[cfg(test)]
    if text {
        RASTERISED_TEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let pipe = r.pipe();
    if let Some(c) = pipe.authored_in() {
        img = crate::ops::convert(e, &img, &c);
    }
    if let Some(masks) = &masks {
        img = crate::masks_gpu::apply(e, masks, img)?;
        #[cfg(test)]
        crate::masks_gpu::APPLIED.fetch_add(usize::from(!masks.masks.is_empty()), std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(l) = pipe.levels {
        img = crate::ops::quantize(e, &img, l);
    }
    Some((img, plan.offset, plan.scale))
}

/// A solid layer's resident source filled here (its colour already in the working space and
/// quantised, as `Renderer::source` fills it), masks applied as above. `None`: on the CPU.
fn solid_source(e: &mut Enc, r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> Option<(GpuImage, [f64; 2], f64)> {
    let f = r.solid_fill(ctx, layer)?;
    if !e.g.fits(f.width, f.height) {
        return None;
    }
    let masks =
        if layer.masks().is_some_and(|m| !m.children.is_empty()) { Some(r.mask_gpu_plan(ctx, layer, f.width, f.height, f.offset, f.scale)?) } else { None };
    let mut img = e.scratch(f.width, f.height);
    let mut p = crate::context::Params::default();
    p.f[0] = f.px;
    let dummy = e.g.dummy.clone();
    e.pixels("fill", &p, &dummy, None, &img, None);
    // `masked_source` quantises a solid only when masks took it off the grid.
    if let Some(masks) = masks.filter(|m| !m.masks.is_empty()) {
        img = crate::masks_gpu::apply(e, &masks, img)?;
        if let Some(l) = r.pipe().levels {
            img = crate::ops::quantize(e, &img, l);
        }
    }
    Some((img, f.offset, f.scale))
}
