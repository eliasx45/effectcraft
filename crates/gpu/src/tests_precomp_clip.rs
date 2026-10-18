//! Precomp layers whose nested comp reaches far outside the frame, drawn on the GPU compositor
//! with only the window of the nested frame that can show (`effectcraft_render::set_precomp_clip`):
//! the frames match the CPU oracle rendering the nested comps whole, with and without the layer
//! cache (the resident precomp path), with effects reaching across the window's edge and two
//! levels of nesting.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, LayerSource, Project, Solid};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer, set_precomp_clip};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Pattern, check, diff, gpu, tolerance};

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

/// A 4000×300 strip of tiles sliding through a 400×200 comp, scaled and rotated; `twice`: the
/// strip nested once more in a 4000×300 comp; `fx`: an effect on the outer precomp layer.
fn scene(depth: BitDepth, fx: Option<(&str, &[(&str, f64)])>, twice: bool) -> (Project, ItemId) {
    let mut p = Project::default();
    p.settings.bit_depth = depth;
    let mut strip = Comp::new(4000, 300, FrameRate::FPS_30, s(2.0));
    let mut layers = vec![];
    for i in 0..26 {
        let c = [(i as f32 * 0.37) % 1.0, 0.3 + (i % 3) as f32 * 0.2, 1.0 - (i as f32 * 0.11) % 1.0];
        let (w, h) = (130 + (i % 4) * 7, 240 - (i % 5) * 9);
        let sid = p.add_item("Tile", Label::Red, None, ItemKind::Solid(Solid { color: c, width: w, height: h, pixel_aspect: 1.0 }));
        let mut l = build::layer(&mut p, &strip, "Tile", LayerSource::Solid { item: sid }, (w, h), None);
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([80.0 + i as f64 * 153.3, 150.0 + (i % 3) as f64 * 11.0, 0.0]);
        l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar((i % 7) as f64 * 3.0 - 9.0);
        l.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(if i % 2 == 0 { 100.0 } else { 70.0 });
        layers.push(l);
    }
    strip.layers = layers.into();
    let mut item = p.add_item("Strip", Label::Sandstone, None, ItemKind::Comp(strip.into()));
    if twice {
        let mut mid = Comp::new(4000, 300, FrameRate::FPS_30, s(2.0));
        let mut l = build::layer(&mut p, &mid, "Strip", LayerSource::Comp { item }, (4000, 300), None);
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([2010.0, 155.0, 0.0]);
        l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(-2.0);
        mid.layers = vec![l].into();
        item = p.add_item("Middle", Label::Sandstone, None, ItemKind::Comp(mid.into()));
    }
    let outer = Comp::new(400, 200, FrameRate::FPS_30, s(2.0));
    let mut l = build::layer(&mut p, &outer, "Row", LayerSource::Comp { item }, (4000, 300), None);
    l.props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([1900.0, 100.0, 0.0])), Keyframe::new(s(1.0), Value::Vec3([-1500.0, 110.0, 0.0]))];
    l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(6.0);
    l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([90.0, 85.0, 100.0]);
    if let Some((id, params)) = fx {
        let mut next = p.next_id;
        let spec = effectcraft_effects::find(id).unwrap();
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [4000.0, 300.0]);
        for (k, v) in params {
            g.prop_mut(k).unwrap().value = Value::Scalar(*v);
        }
        p.next_id = next;
        l.props.sub_mut("effects").unwrap().children.push(g.into());
    }
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.into()));
    p.comp_mut(cid).unwrap().layers = vec![l].into();
    (p, cid)
}

/// GPU frames with windowed precomps match the CPU oracle rendering them whole at 8 and 32 bpc,
/// Full and Half, cached (resident precomps) and uncached.
#[test]
fn windowed_precomps_on_the_gpu_match_the_whole_cpu_render() {
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    let blur: &[(&str, f64)] = &[("blurriness", 40.0)];
    let glow: &[(&str, f64)] = &[("radius", 45.0)];
    for fx in [None, Some(("ec.blur.gaussian", blur)), Some(("ec.stylize.glow", glow))] {
        for twice in [false, true] {
            for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
                let (p, cid) = scene(depth, fx, twice);
                for scale in [1.0, 0.5] {
                    let opts = RenderOpts { scale, ..Default::default() };
                    for cached in [false, true] {
                        let cache = LayerCache::new(1 << 30);
                        for t in [0.0, 0.35, 0.7] {
                            set_precomp_clip(true);
                            let mut r = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts });
                            r.accel = Some(g);
                            r.cache = cached.then_some(&cache);
                            let img = g.render(&r, cid, s(t)).expect("the GPU renders the frame");
                            set_precomp_clip(false);
                            let cpu = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Cpu, ..opts }).comp_frame_cpu(cid, s(t));
                            set_precomp_clip(true);
                            assert!(cpu.data.iter().any(|p| p[3] > 0.5), "the row shows");
                            let mut d = diff(&cpu, &img, tolerance(depth));
                            d.quantized = depth != BitDepth::Bpc32;
                            let name = fx.map_or("none", |f| f.0);
                            check(&format!("precomp clip {name} twice {twice} {depth:?} scale {scale} cached {cached} t {t}"), Some(d), 0.0);
                        }
                    }
                }
            }
        }
    }
}
