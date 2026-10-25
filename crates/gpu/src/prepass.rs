//! The GPU walk's pre-pass (`effectcraft_render::prepass`): before a layer list is composited,
//! the content its text, shape and solid layers need is produced on every core. Layer-styled
//! and other CPU-composited layers land in the layer cache; the sources of plain layers the walk
//! keeps resident (`walk::draw_ordinary_resident`, which runs their effects on the GPU) are held
//! here (by content key, comp, layer and time) until the walk takes them or the layer list is done.

use std::cell::RefCell;
use std::collections::HashMap;

use effectcraft_effects::Buf;
use effectcraft_project::Layer;
use effectcraft_render::cache::GpuStage;
use effectcraft_render::prepass::prefetch_slot;
use effectcraft_render::{ContentClip, EvalCtx, Renderer};

use crate::context::Enc;

thread_local! {
    /// Resident sources produced ahead of the walk on this thread, by
    /// `effectcraft_render::prepass::prefetch_slot`.
    static READY: RefCell<HashMap<u64, Buf>> = RefCell::new(HashMap::new());
}

/// The sources one layer list's pre-pass left for the walk: dropped (with what the walk didn't
/// take) when the list is drawn.
pub(crate) struct Prefetched {
    keys: Vec<u64>,
}

impl Drop for Prefetched {
    fn drop(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let _ = READY.try_with(|m| {
            if let Ok(mut m) = m.try_borrow_mut() {
                for k in &self.keys {
                    m.remove(k);
                }
            }
        });
    }
}

/// Run the pre-pass for `visible` (drawn into `canvas`). `resident`: the walk's ordinary
/// resident path is on (it still needs [`Enc::can_retain_resident`]).
pub(crate) fn run<'a>(e: &Enc, r: &Renderer<'a>, ctx: &EvalCtx<'a>, visible: &[&'a Layer], canvas: (u32, u32), resident: bool) -> Prefetched {
    let resident = resident && e.can_retain_resident();
    let held = |slot: u64| READY.try_with(|m| m.try_borrow().map(|m| m.contains_key(&slot)).unwrap_or(true)).unwrap_or(true);
    // Shape layers the walk rasterises on the GPU (`shapes_gpu`): no CPU raster ahead of it.
    let gpu_shapes: Vec<&Layer> =
        if crate::shapes_gpu::enabled() { visible.iter().copied().filter(|l| r.shape_raster_eligible(ctx, l)).collect() } else { Vec::new() };
    let on_gpu = |key: u64, slot: u64| gpu_shapes.iter().any(|l| prefetch_slot(ctx, l, key) == slot);
    let have = |key: u64, slot: u64| held(slot) || e.has_resident(key, GpuStage::Processed) || on_gpu(key, slot);
    let made = r.prefetch(ctx, visible, canvas, resident.then_some(&have as &dyn Fn(u64, u64) -> bool));
    let mut keys = Vec::with_capacity(made.len());
    let _ = READY.try_with(|m| {
        if let Ok(mut m) = m.try_borrow_mut() {
            for (k, b) in made {
                if let std::collections::hash_map::Entry::Vacant(v) = m.entry(k) {
                    v.insert(b);
                    keys.push(k);
                }
            }
        }
    });
    Prefetched { keys }
}

/// [`Renderer::resident_source`] of the layer whose content key is `key`: the pre-pass's buffer
/// when it made one (the same call, so the same pixels), else rendered now.
pub(crate) fn resident_source(r: &Renderer, ctx: &EvalCtx, layer: &Layer, key: u64, clip: ContentClip) -> Option<Buf> {
    let slot = prefetch_slot(ctx, layer, key);
    let ready = READY.try_with(|m| m.try_borrow_mut().ok().and_then(|mut m| m.remove(&slot))).ok().flatten();
    ready.or_else(|| r.resident_source(ctx, layer, clip))
}

#[cfg(test)]
mod tests {
    use effectcraft_color::Label;
    use effectcraft_keyframe::{Keyframe, ShapePath, TextDoc, Value};
    use effectcraft_project::build::{self, Ids};
    use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, LayerSource, MaskMode, MatteKind, Project, Solid, TrackMatte};
    use effectcraft_raster::Image;
    use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer, prepass};
    use effectcraft_time::{FrameRate, Tick};

    use crate::tests::{Pattern, gpu};

    fn s(t: f64) -> Tick {
        Tick::from_seconds_f64(t)
    }

    /// Text layers (moving, overlapping), shape layers (masked, layer-styled, blurred: the GPU
    /// runs that blur on the resident source) and a solid matted by a text layer, over a solid.
    fn scene(depth: BitDepth) -> (Project, ItemId) {
        let mut p = Project::default();
        p.settings.bit_depth = depth;
        let comp = Comp::new(320, 180, FrameRate::FPS_30, s(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let solid = |p: &mut Project, color: [f32; 3]| {
            let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color, width: 320, height: 180, pixel_aspect: 1.0 }));
            build::layer(p, &comp, "Solid", LayerSource::Solid { item: sid }, (320, 180), None)
        };
        let mut layers = vec![solid(&mut p, [0.2, 0.25, 0.3])];
        let key = |a: Value, b: Value| vec![Keyframe::new(Tick::ZERO, a), Keyframe::new(s(1.0), b)];
        for i in 0..6 {
            let mut l = build::layer(&mut p, &comp, "Text", LayerSource::Text, (320, 180), None);
            let doc = TextDoc {
                text: format!("Pre-pass {i} AVA"),
                size: 24.0 + i as f64 * 5.0,
                tracking: -60.0,
                fill: [0.9, 0.3 + i as f32 * 0.1, 0.2, 1.0],
                ..Default::default()
            };
            l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
            let y = 20.0 + i as f64 * 27.0;
            l.props.prop_mut("transform/position").unwrap().keys = key(Value::Vec3([10.0, y, 0.0]), Value::Vec3([60.0, y + 4.0, 0.0]));
            layers.push(l);
        }
        let mut next = p.next_id;
        let mut ids = Ids(&mut next);
        for i in 0..4 {
            let mut l = build::layer(&mut p, &comp, "Shape", LayerSource::Shape, (320, 180), None);
            let items = vec![
                build::shape_rect(&mut ids, [70.0, 40.0], [-30.0, 0.0], 8.0),
                build::shape_ellipse(&mut ids, [50.0, 50.0], [25.0, 5.0]),
                build::shape_fill(&mut ids, [0.2, 0.6 + i as f64 * 0.1, 0.9, 0.8]),
                build::shape_stroke(&mut ids, [1.0, 1.0, 1.0, 1.0], 3.0),
            ];
            l.props.sub_mut("contents").unwrap().children.push(build::shape_group(&mut ids, "Group", items).into());
            l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([70.0 + i as f64 * 60.0, 90.0 + i as f64 * 10.0, 0.0]);
            match i {
                1 => {
                    let mut m = build::mask(&mut ids, "Mask 1", ShapePath::ellipse([0.0, 0.0], 80.0, 50.0), MaskMode::Add, [255, 255, 0]);
                    m.prop_mut("feather").unwrap().value = Value::Vec2([8.0, 8.0]);
                    l.props.sub_mut("masks").unwrap().children.push(m.into());
                }
                2 => {
                    effectcraft_project::styles::add_style(&mut l, &mut ids, "dropShadow", &comp.global_light, true).unwrap();
                }
                3 => {
                    let spec = effectcraft_effects::find("ec.blur.gaussian").unwrap();
                    let mut g = effectcraft_effects::instantiate(spec, &mut ids, spec.name, [320.0, 180.0]);
                    g.prop_mut("blurriness").unwrap().value = Value::Scalar(6.0);
                    l.props.sub_mut("effects").unwrap().children.push(g.into());
                }
                _ => {}
            }
            layers.push(l);
        }
        p.next_id = next;
        // A solid seen through a text matte above it.
        let mut matte = build::layer(&mut p, &comp, "Matte", LayerSource::Text, (320, 180), None);
        let doc = TextDoc { text: "MATTE".into(), size: 70.0, ..Default::default() };
        matte.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
        matte.props.prop_mut("transform/position").unwrap().value = Value::Vec3([40.0, 150.0, 0.0]);
        let mut matted = solid(&mut p, [0.95, 0.85, 0.1]);
        matted.track_matte = Some(TrackMatte { layer: matte.id, kind: MatteKind::Alpha });
        layers.push(matted);
        layers.push(matte);
        let c = p.comp_mut(cid).unwrap();
        // Top first.
        layers.reverse();
        c.layers = layers.into();
        (p, cid)
    }

    /// GPU and CPU frames with the pre-pass are bit-identical to frames without it (each with a
    /// fresh layer cache, two times: produced, then partly reused), and the pre-pass did run.
    #[test]
    fn frames_with_the_prepass_match_frames_without() {
        let g = gpu();
        let bits = |i: &Image| i.data.iter().flat_map(|px| px.map(f32::to_bits)).collect::<Vec<_>>();
        for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
            let (p, cid) = scene(depth);
            let frames = |on: bool, gpu_backend: bool| {
                prepass::set_prefetch(on);
                let cache = LayerCache::new(1 << 30);
                let out: Vec<Option<Image>> = [0.0, 0.5, 0.5]
                    .into_iter()
                    .map(|t| {
                        let backend = if gpu_backend { Backend::Gpu } else { Backend::Cpu };
                        let mut r = Renderer::new(&p, &Pattern, RenderOpts { backend, ..Default::default() });
                        r.cache = Some(&cache);
                        match (gpu_backend, g) {
                            (true, Some(g)) => {
                                r.accel = Some(g);
                                g.render(&r, cid, s(t))
                            }
                            (true, None) => None,
                            (false, _) => Some(r.comp_frame_cpu(cid, s(t))),
                        }
                    })
                    .collect();
                prepass::set_prefetch(true);
                out
            };
            for gpu_backend in [false, true] {
                let before = prepass::prefetched();
                let on = frames(true, gpu_backend);
                assert!(prepass::prefetched() > before || (gpu_backend && g.is_none()), "{depth:?} gpu {gpu_backend}: the pre-pass produced layers");
                let off = frames(false, gpu_backend);
                for (i, (a, b)) in on.iter().zip(&off).enumerate() {
                    match (a, b) {
                        (Some(a), Some(b)) => {
                            assert!(a.data.iter().any(|px| px[3] > 0.5), "frame {i} shows");
                            assert!(bits(a) == bits(b), "{depth:?} gpu {gpu_backend} frame {i}: the pre-pass changed pixels");
                        }
                        (None, None) => {}
                        _ => panic!("{depth:?} gpu {gpu_backend} frame {i}: rendered only one way"),
                    }
                }
            }
        }
    }
}
