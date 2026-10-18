//! Precomp layers whose nested comp reaches far outside the frame render only the window of it
//! that can show (see `clip.rs`), with the frames identical to rendering the nested comp whole:
//! also with blurs, glows, shadows and feathered masks reaching across the window's edge, two
//! levels of nesting, and the layer cache.

use std::sync::Mutex;

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, Layer, LayerSource, MaskMode, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{Image, LayerCache, NoFootage, RenderOpts, Renderer};

/// The tests flip the process-wide switch: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fx {
    None,
    Blur,
    Glow,
    Shadow,
    Mask,
}

/// A 4000×300 strip of tiles (a row of photos) nested scaled and rotated in a 400×200 comp,
/// sliding across it; `twice`: the strip sits in a 4000×300 middle comp that is itself nested.
/// `fx` goes on the precomp layer of the outer comp. Returns the project and the outer comp.
fn scene(depth: BitDepth, fx: Fx, twice: bool) -> (Project, ItemId) {
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
    let mut item = p.add_item("Strip", Label::Sandstone, None, ItemKind::Comp(strip.clone().into()));
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
    let pos = l.props.prop_mut("transform/position").unwrap();
    pos.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3([1900.0, 100.0, 0.0])), Keyframe::new(s(1.0), Value::Vec3([-1500.0, 110.0, 0.0]))];
    l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(6.0);
    l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([90.0, 85.0, 100.0]);
    let mut next = p.next_id;
    let mut ids = Ids(&mut next);
    let effect = |ids: &mut Ids, id: &str, params: &[(&str, f64)]| {
        let spec = effectcraft_effects::find(id).unwrap();
        let mut g = effectcraft_effects::instantiate(spec, ids, spec.name, [4000.0, 300.0]);
        for (k, v) in params {
            g.prop_mut(k).unwrap().value = Value::Scalar(*v);
        }
        g
    };
    match fx {
        Fx::None => {}
        Fx::Blur => l.props.sub_mut("effects").unwrap().children.push(effect(&mut ids, "ec.blur.gaussian", &[("blurriness", 40.0)]).into()),
        Fx::Glow => l.props.sub_mut("effects").unwrap().children.push(effect(&mut ids, "ec.stylize.glow", &[("radius", 45.0)]).into()),
        Fx::Shadow => {
            l.props.sub_mut("effects").unwrap().children.push(effect(&mut ids, "ec.perspective.dropshadow", &[("distance", 60.0), ("softness", 40.0)]).into())
        }
        Fx::Mask => {
            let mut m = build::mask(&mut ids, "Mask 1", ShapePath::rect([2050.0, 150.0], 3500.0, 260.0), MaskMode::Add, [255, 255, 0]);
            m.prop_mut("feather").unwrap().value = Value::Vec2([80.0, 40.0]);
            m.prop_mut("expansion").unwrap().value = Value::Scalar(-15.0);
            l.props.sub_mut("masks").unwrap().children.push(m.into());
        }
    }
    p.next_id = next;
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.into()));
    p.comp_mut(cid).unwrap().layers = vec![l].into();
    (p, cid)
}

fn frame(p: &Project, cid: ItemId, scale: f64, t: Tick, clip: bool, cache: Option<&LayerCache>) -> Image {
    crate::set_precomp_clip(clip);
    let mut r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
    r.cache = cache;
    let img = r.comp_frame(cid, t);
    crate::set_precomp_clip(true);
    img
}

/// Pixels of the outer precomp layer's buffer (after masks and effects).
fn buf_pixels(p: &Project, cid: ItemId, t: Tick, clip: bool) -> u64 {
    crate::set_precomp_clip(clip);
    let r = Renderer::new(p, &NoFootage, RenderOpts::default());
    let c = p.comp(cid).unwrap();
    let l: &Layer = c.layers.iter().next().unwrap();
    let b = r.content_buf(&r.ctx(cid, c, t), l).unwrap();
    crate::set_precomp_clip(true);
    b.img.width as u64 * b.img.height as u64
}

fn max_diff(a: &Image, b: &Image) -> f32 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.data.iter().zip(&b.data).flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs())).fold(0.0, f32::max)
}

/// Largest difference allowed: without effects the pixels are identical; an effect run on a
/// window sums its kernel at other (whole-pixel) coordinates, which changes values by float
/// rounding only (under 1e-5 at 32 bpc) and can tip a value quantised after the effect by one
/// code value at 8 bpc.
fn tolerance(depth: BitDepth, fx: Fx) -> f32 {
    match (fx, depth) {
        (Fx::None, _) => 0.0,
        (_, BitDepth::Bpc8) => 1.01 / 255.0,
        (_, BitDepth::Bpc16) => 1.01 / 32768.0,
        (_, BitDepth::Bpc32) => 1e-5,
    }
}

fn nonblank(img: &Image) -> bool {
    img.data.iter().any(|p| p[3] > 0.5)
}

/// The frames are the whole render's at 8 and 32 bpc, Full and Half, with each effect reaching
/// across the window's edge, for one and two levels of nesting, while the row slides through;
/// and the precomp's buffer is a fraction of the whole nested frame.
#[test]
fn windowed_precomp_frames_match_the_whole_render() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for fx in [Fx::None, Fx::Blur, Fx::Glow, Fx::Shadow, Fx::Mask] {
        for twice in [false, true] {
            for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
                let (p, cid) = scene(depth, fx, twice);
                for scale in [1.0, 0.5] {
                    for t in [0.0, 0.35, 0.7, 1.0] {
                        let a = frame(&p, cid, scale, s(t), true, None);
                        let b = frame(&p, cid, scale, s(t), false, None);
                        assert!(nonblank(&b), "{fx:?} twice {twice}: the row shows at {t}");
                        let d = max_diff(&a, &b);
                        assert!(d <= tolerance(depth, fx), "{fx:?} twice {twice} {depth:?} scale {scale} t {t}: {d}");
                    }
                }
                let (clipped, whole) = (buf_pixels(&p, cid, s(0.35), true), buf_pixels(&p, cid, s(0.35), false));
                assert!(clipped * 2 < whole, "{fx:?} twice {twice} {depth:?}: {clipped} of {whole} pixels");
            }
        }
    }
}

/// An effect whose reach is not known (Turbulent Displace) keeps the nested comp whole.
#[test]
fn unknown_effects_render_the_precomp_whole() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (mut p, cid) = scene(BitDepth::Bpc8, Fx::None, false);
    let mut next = p.next_id;
    let spec = effectcraft_effects::find("ec.distort.turbulentdisplace").unwrap();
    let g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [4000.0, 300.0]);
    p.next_id = next;
    let c = p.comp_mut(cid).unwrap();
    c.layers.iter_mut().next().unwrap().props.sub_mut("effects").unwrap().children.push(g.into());
    assert_eq!(buf_pixels(&p, cid, s(0.35), true), buf_pixels(&p, cid, s(0.35), false));
    assert!(max_diff(&frame(&p, cid, 1.0, s(0.35), true, None), &frame(&p, cid, 1.0, s(0.35), false, None)) == 0.0);
}

/// With the layer cache, the window is part of the key: every frame of the slide matches the
/// uncached whole render (no stale window served), and the static row goes whole once it held
/// (drawn once, as before windowing).
#[test]
fn precomp_windows_are_part_of_the_cache_key() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (p, cid) = scene(BitDepth::Bpc8, Fx::Blur, true);
    let cache = LayerCache::new(1 << 30);
    for f in 0..12 {
        let t = s(f as f64 / 30.0);
        let a = frame(&p, cid, 1.0, t, true, Some(&cache));
        let b = frame(&p, cid, 1.0, t, false, None);
        let d = max_diff(&a, &b);
        assert!(d <= tolerance(BitDepth::Bpc8, Fx::Blur), "frame {f}: {d}");
    }
    assert!(cache.stats().hits > 0);
}

/// Another layer's effect with an empty layer parameter (as Deep Glow's Mask Layer at None)
/// reads no layer: the row is still windowed. Pointed at the row, the row is drawn whole (that
/// effect may read any of its pixels). The frames match the whole render either way.
#[test]
fn layer_parameters_naming_the_precomp_keep_it_whole() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (mut p, cid) = scene(BitDepth::Bpc8, Fx::None, false);
    let row = p.comp(cid).unwrap().layers.iter().next().unwrap().id;
    let sid = p.add_item("Dot", Label::Red, None, ItemKind::Solid(Solid { color: [1.0, 1.0, 1.0], width: 20, height: 20, pixel_aspect: 1.0 }));
    let outer = p.comp(cid).unwrap().clone();
    let mut dot = build::layer(&mut p, &outer, "Dot", LayerSource::Solid { item: sid }, (20, 20), None);
    let mut next = p.next_id;
    let spec = effectcraft_effects::find("ec.control.layer").unwrap();
    let g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [20.0, 20.0]);
    p.next_id = next;
    dot.props.sub_mut("effects").unwrap().children.push(g.into());
    p.comp_mut(cid).unwrap().layers.insert(0, dot);
    let row_pixels = |p: &Project, clip: bool| {
        crate::set_precomp_clip(clip);
        let r = Renderer::new(p, &NoFootage, RenderOpts::default());
        let c = p.comp(cid).unwrap();
        let l = c.layers.iter().find(|l| l.id == row).unwrap();
        let b = r.content_buf(&r.ctx(cid, c, s(0.35)), l).unwrap();
        crate::set_precomp_clip(true);
        b.img.width as u64 * b.img.height as u64
    };
    for named in [false, true] {
        if named {
            let c = p.comp_mut(cid).unwrap();
            let fx = c.layers.iter_mut().next().unwrap().props.sub_mut("effects").unwrap();
            let Some(effectcraft_project::Node::Group(g)) = fx.children.first_mut() else { panic!("effect") };
            g.prop_mut("layer").unwrap().value = Value::Layer(Some(row.0));
        }
        let (clipped, whole) = (row_pixels(&p, true), row_pixels(&p, false));
        assert_eq!(clipped < whole, !named, "named {named}: {clipped} of {whole}");
        let d = max_diff(&frame(&p, cid, 1.0, s(0.35), true, None), &frame(&p, cid, 1.0, s(0.35), false, None));
        assert!(d == 0.0, "named {named}: {d}");
    }
}
