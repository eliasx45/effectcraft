//! Large shape layers in small nested comps, rasterised only around the nested frame
//! (`effectcraft_render::set_shape_clip`), on the GPU compositor: the frames match the CPU oracle
//! drawing the shapes whole, and the shape uploads shrink.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, LayerSource, MaskMode, Project};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer, set_shape_clip};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Pattern, check, diff, gpu, tolerance};

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

/// A 2600×2000 shape (backdrop, a grid of tiles, a stroked frame; Gaussian blur, a feathered
/// expanded mask, animated position, rotation and scale) in a 120×90 comp, nested in a 200×150
/// comp; `outer_blur`: the precomp layer moves with motion blur.
fn scene(depth: BitDepth, outer_blur: bool) -> (Project, ItemId) {
    let mut p = Project::default();
    p.settings.bit_depth = depth;
    let inner = Comp::new(120, 90, FrameRate::FPS_30, s(2.0));
    let mut outer = Comp::new(200, 150, FrameRate::FPS_30, s(2.0));
    outer.enable_motion_blur = outer_blur;
    let mut l = build::layer(&mut p, &inner, "Shape", LayerSource::Shape, (inner.width, inner.height), None);
    let mut next = p.next_id;
    let mut ids = Ids(&mut next);
    let mut items = vec![build::shape_rect(&mut ids, [2600.0, 2000.0], [0.0, 0.0], 0.0)];
    items.push(build::shape_fill(&mut ids, [0.9, 0.6, 0.2, 1.0]));
    let mut tiles = vec![];
    for i in -6..=6 {
        for j in -5..=5 {
            tiles.push(build::shape_rect(&mut ids, [23.0 + (i * j % 7) as f64, 17.0], [i as f64 * 61.0 + 0.3, j as f64 * 47.0 - 0.6], 4.0));
        }
    }
    tiles.push(build::shape_fill(&mut ids, [0.1, 0.5, 0.9, 0.8]));
    // The frame's size animates: the content changes every frame (static content is kept whole).
    let mut rect = build::shape_rect(&mut ids, [150.0, 110.0], [10.0, -5.0], 12.0);
    rect.prop_mut("size").unwrap().keys = vec![Keyframe::new(Tick::ZERO, Value::Vec2([150.0, 110.0])), Keyframe::new(s(1.0), Value::Vec2([90.0, 300.0]))];
    let frame_items = vec![rect, build::shape_stroke(&mut ids, [1.0, 1.0, 1.0, 1.0], 7.0)];
    let groups =
        [build::shape_group(&mut ids, "Frame", frame_items), build::shape_group(&mut ids, "Tiles", tiles), build::shape_group(&mut ids, "Backdrop", items)];
    for g in groups {
        l.props.sub_mut("contents").unwrap().children.push(g.into());
    }
    let key = |a: Value, b: Value| vec![Keyframe::new(Tick::ZERO, a), Keyframe::new(s(1.0), b)];
    l.props.prop_mut("transform/position").unwrap().keys = key(Value::Vec3([-90.0, 20.0, 0.0]), Value::Vec3([210.0, 70.0, 0.0]));
    l.props.prop_mut("transform/rotation").unwrap().keys = key(Value::Scalar(0.0), Value::Scalar(30.0));
    l.props.prop_mut("transform/scale").unwrap().keys = key(Value::Vec3([100.0, 100.0, 100.0]), Value::Vec3([70.0, 85.0, 100.0]));
    let mut m = build::mask(&mut ids, "Mask 1", ShapePath::rect([100.0, 50.0], 900.0, 700.0), MaskMode::Add, [255, 255, 0]);
    m.prop_mut("feather").unwrap().value = Value::Vec2([30.0, 30.0]);
    m.prop_mut("expansion").unwrap().value = Value::Scalar(10.0);
    l.props.sub_mut("masks").unwrap().children.push(m.into());
    let spec = effectcraft_effects::find("ec.blur.gaussian").unwrap();
    let mut g = effectcraft_effects::instantiate(spec, &mut ids, spec.name, [120.0, 90.0]);
    g.prop_mut("blurriness").unwrap().value = Value::Scalar(25.0);
    l.props.sub_mut("effects").unwrap().children.push(g.into());
    p.next_id = next;
    let mut inner = inner;
    inner.layers.push(l);
    let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
    let mut pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (120, 90), None);
    pre.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(5.0);
    pre.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([120.0, 120.0, 100.0]);
    pre.props.prop_mut("transform/position").unwrap().value = Value::Vec3([100.0, 75.0, 0.0]);
    if outer_blur {
        pre.switches.motion_blur = true;
        pre.props.prop_mut("transform/position").unwrap().keys = key(Value::Vec3([60.0, 60.0, 0.0]), Value::Vec3([140.0, 90.0, 0.0]));
    }
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.into()));
    p.comp_mut(cid).unwrap().layers.push(pre);
    (p, cid)
}

/// GPU frames (layer cache on: the resident shape path) with clipping match the CPU oracle
/// drawing the shape whole at 8/16/32 bpc, Full and Half, with and without motion blur on the
/// precomp, and upload a fraction of the bytes.
#[test]
fn clipped_shapes_on_the_gpu_match_the_whole_cpu_raster() {
    let _serial = crate::tests_view_clip::CLIP_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        for outer_blur in [false, true] {
            let (p, cid) = scene(depth, outer_blur);
            for scale in [1.0, 0.5] {
                let opts = RenderOpts { motion_blur: true, scale, ..Default::default() };
                let mut bytes = [0u64; 2];
                for (i, clip) in [true, false].into_iter().enumerate() {
                    let cache = LayerCache::new(1 << 30);
                    g.ctx.clear_uploads();
                    let before = g.ctx.transfer_stats();
                    for t in [0.2, 0.5, 0.8] {
                        set_shape_clip(clip);
                        let mut r = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts });
                        r.accel = Some(g);
                        r.cache = Some(&cache);
                        let img = g.render(&r, cid, s(t)).expect("the GPU renders the frame");
                        set_shape_clip(false);
                        let cpu = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Cpu, ..opts }).comp_frame_cpu(cid, s(t));
                        set_shape_clip(true);
                        assert!(cpu.data.iter().any(|p| p[3] > 0.5), "the shape shows");
                        let mut d = diff(&cpu, &img, tolerance(depth));
                        d.quantized = depth != BitDepth::Bpc32;
                        check(&format!("shape clip={clip} {depth:?} outer blur {outer_blur} scale {scale} t {t}"), Some(d), 0.0);
                    }
                    bytes[i] = g.ctx.transfer_stats().upload_bytes - before.upload_bytes;
                }
                eprintln!("shape clip {depth:?} outer blur {outer_blur} scale {scale}: upload bytes {} clipped vs {} whole", bytes[0], bytes[1]);
                assert!(bytes[0] * 2 < bytes[1], "{depth:?} outer blur {outer_blur} scale {scale}: {} vs {} bytes", bytes[0], bytes[1]);
            }
        }
    }
}
