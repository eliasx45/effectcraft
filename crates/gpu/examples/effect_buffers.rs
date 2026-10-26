//! `cargo run --release -p effectcraft-gpu --example effect_buffers -- [--frames N] [--sizes 1080p,4k,40mp]
//! [--effects blur,glow,deepglow,bevel] [--depth 8|16|32]`
//!
//! Effect working-buffer benchmark (plan/memory/effect-buffers.md): renders a comp with one
//! large solid carrying Gaussian Blur, Glow, Deep Glow or a Bevel and Emboss layer style through
//! the viewer's display path (`Gpu::render_display`, no layer cache, so every frame runs the
//! effect) with effect intermediates in RGBA f32 and then in RGBA16F
//! (`GpuContext::set_half_intermediates`), at 1920 × 1080, 3840 × 2160 and a 40 Mpx canvas
//! (7744 × 5168, the Neon/Bevel layer at raster scale 2.83). Prints per case the median time
//! per frame (GPU work waited for) and the peak bytes of pooled working textures handed out
//! during a frame (`GpuContext::leased_bytes`, the frame's comp canvas and layer upload
//! included; they are the same in both modes, so the difference is the effect's scratch).
//!
//! Leave `EFFECTCRAFT_HALF_FX` unset: it overrides the mode this bench switches.

use std::sync::Arc;
use std::time::Instant;

use effectcraft_color::Label;
use effectcraft_gpu::Gpu;
use effectcraft_keyframe::Value;
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, Footage, ItemId, ItemKind, Layer, LayerSource, Project, Solid};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, FootageSource, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

/// No footage: the bench comps hold solids only.
struct NoFootage;

impl FootageSource for NoFootage {
    fn frame(&self, _item: ItemId, _f: &Footage, _t: Tick) -> Option<Arc<Image>> {
        None
    }
}

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

fn set(l: &mut Layer, path: &str, v: Value) -> Result<(), String> {
    l.props.prop_mut(path).ok_or_else(|| format!("no property {path}"))?.value = v;
    Ok(())
}

fn effect(p: &mut Project, comp: &Comp, l: &mut Layer, id: &str, vals: &[(&str, f64)]) -> Result<(), String> {
    let spec = effectcraft_effects::find(id).ok_or_else(|| format!("no effect {id}"))?;
    let size = effectcraft_render::effect_bounds(p, comp, l).0;
    let mut next = p.next_id;
    let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, size);
    p.next_id = next;
    for (k, v) in vals {
        g.prop_mut(k).ok_or_else(|| format!("{id}: no {k}"))?.value = Value::Scalar(*v);
    }
    l.props.sub_mut("effects").ok_or("layer has no effects group")?.children.push(g.into());
    Ok(())
}

/// A `w × h` comp with one solid (90 % of the comp, centred) carrying `fx`.
fn scene(fx: &str, w: u32, h: u32, depth: BitDepth) -> Result<(Project, ItemId), String> {
    let mut p = Project::default();
    p.settings.bit_depth = depth;
    let comp = Comp::new(w, h, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Bench", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let (sw, sh) = (w * 9 / 10, h * 9 / 10);
    let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color: [0.9, 0.45, 0.15], width: sw, height: sh, pixel_aspect: 1.0 }));
    let mut l = build::layer(&mut p, &comp, "Solid", LayerSource::Solid { item: sid }, (sw, sh), None);
    // Effect sizes scale with the canvas (a raster-scaled layer's effects scale with it).
    let k = f64::from(h) / 1080.0;
    match fx {
        "blur" => effect(&mut p, &comp, &mut l, "ec.blur.gaussian", &[("blurriness", 20.0 * k)])?,
        "glow" => effect(&mut p, &comp, &mut l, "ec.stylize.glow", &[("threshold", 30.0), ("radius", 30.0 * k), ("intensity", 1.5)])?,
        "deepglow" => effect(&mut p, &comp, &mut l, "ec.stylize.deepglow", &[("PEDG-0001", 40.0 * k), ("PEDG-0004", 30.0)])?,
        "bevel" => {
            let mut next = p.next_id;
            effectcraft_project::styles::add_style(&mut l, &mut Ids(&mut next), "bevelEmboss", &comp.global_light, true).ok_or("no Bevel and Emboss style")?;
            p.next_id = next;
            let group = effectcraft_project::styles::GROUP;
            // At most `styles::GPU_BEVEL_REACH` (256) buffer pixels, or it renders on the CPU.
            set(&mut l, &format!("{group}/bevelEmboss/size"), Value::Scalar((12.0 * k).min(60.0)))?;
            set(&mut l, &format!("{group}/bevelEmboss/soften"), Value::Scalar(2.0 * k))?;
        }
        other => return Err(format!("unknown effect {other} (blur, glow, deepglow, bevel)")),
    }
    p.comp_mut(cid).ok_or("no comp")?.layers.insert(0, l);
    Ok((p, cid))
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// (median ms per frame, peak working-texture bytes during a frame).
fn run(g: &Gpu, p: &Project, cid: ItemId, frames: usize) -> Result<(f64, usize), String> {
    let mut r = Renderer::new(p, &NoFootage, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(g);
    let ctx = g.context();
    let mut ms = vec![];
    let mut peak = 0usize;
    // One warm-up frame (pipelines, pool), then the measured ones.
    for i in 0..=frames {
        ctx.reset_vram_peak();
        let (before, _) = ctx.leased_bytes();
        let t0 = Instant::now();
        let frame = g.within_memory(|| g.render_display(&r, cid, Tick::ZERO))?.ok_or("the frame did not render on the GPU")?;
        g.wait_submission(&ctx.fence())?;
        let took = t0.elapsed().as_secs_f64() * 1e3;
        drop(frame);
        let (_, high) = ctx.leased_bytes();
        if i > 0 {
            ms.push(took);
            peak = peak.max(high.saturating_sub(before));
        }
    }
    Ok((median(&mut ms), peak))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if std::env::var_os("EFFECTCRAFT_HALF_FX").is_some() {
        eprintln!("note: EFFECTCRAFT_HALF_FX is set and overrides the f32/f16 switch; unset it for an A/B");
    }
    let frames = arg(&args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(5usize).max(1);
    let depth = match arg(&args, "--depth").as_deref() {
        Some("16") => BitDepth::Bpc16,
        Some("32") => BitDepth::Bpc32,
        _ => BitDepth::Bpc8,
    };
    let sizes_arg = arg(&args, "--sizes").unwrap_or_else(|| "1080p,4k,40mp".into());
    let effects_arg = arg(&args, "--effects").unwrap_or_else(|| "blur,glow,deepglow,bevel".into());
    let g = Gpu::try_headless()?;
    println!("adapter: {}  depth: {depth:?}  frames: {frames}", effectcraft_render::Accelerator::name(&g));
    println!("{:<9} {:<6} {:>11} {:>11} {:>13} {:>13} {:>8}", "effect", "size", "f32 ms", "f16 ms", "f32 peak MB", "f16 peak MB", "saved");
    for fx in effects_arg.split(',') {
        for size in sizes_arg.split(',') {
            let (w, h) = match size {
                "1080p" => (1920, 1080),
                "4k" => (3840, 2160),
                "40mp" => (7744, 5168),
                other => return Err(format!("unknown size {other} (1080p, 4k, 40mp)")),
            };
            let (p, cid) = scene(fx, w, h, depth)?;
            g.context().set_half_intermediates(false);
            let (ms32, peak32) = run(&g, &p, cid, frames)?;
            g.context().set_half_intermediates(true);
            let (ms16, peak16) = run(&g, &p, cid, frames)?;
            g.context().set_half_intermediates(false);
            let mb = |b: usize| b as f64 / (1u64 << 20) as f64;
            let saved = if peak32 > 0 { 100.0 * (1.0 - peak16 as f64 / peak32 as f64) } else { 0.0 };
            println!("{fx:<9} {size:<6} {ms32:>11.1} {ms16:>11.1} {:>13.0} {:>13.0} {saved:>7.0}%", mb(peak32), mb(peak16));
        }
    }
    Ok(())
}
