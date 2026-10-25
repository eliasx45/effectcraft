//! Text and shape layers in the top comp, partly off the frame: their comp-sized effect canvas is
//! filled only where it can show (`Renderer::fx_view`) and a large shape is rasterised only
//! around the frame (`Renderer::shape_clip`), with the frames identical to drawing them whole.
//! 10-10's captions (Deep Glow, two Drop Shadows, Bevel Alpha), Gradient Ramp, CC Light Sweep
//! and a blurred backdrop; still and animated content, cached and uncached, Full and Half.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, TextDoc, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Comp, ItemId, ItemKind, Layer, LayerSource, Project};
use effectcraft_time::{FrameRate, Tick};

use crate::tests_clip::SERIAL;
use crate::{EvalCtx, Image, LayerCache, NoFootage, RenderOpts, Renderer};

const W: u32 = 1280;
const H: u32 = 720;

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Caption,
    Ramp,
    Sweep,
    Backdrop,
}

/// One layer of `kind` partly off the right edge, over a solid; `animated` keys its content.
fn scene(kind: Kind, animated: bool) -> (Project, ItemId) {
    let mut p = Project::default();
    let comp = Comp::new(W, H, FrameRate::FPS_30, s(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let text = kind != Kind::Backdrop;
    let src = if text { LayerSource::Text } else { LayerSource::Shape };
    let mut l = build::layer(&mut p, &comp, "L", src, (W, H), None);
    let mut next = p.next_id;
    if text {
        l.props.prop_mut("text/sourceText").unwrap().value =
            Value::Text(Box::new(TextDoc { text: "Followers and more".into(), size: 120.0, ..Default::default() }));
        // Continuously Rasterized at 300 %: a caption far wider than the frame, most of it (and of
        // its 3840×2160 effect canvas) off the right edge.
        l.switches.collapse = true;
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([900.3, 400.7, 0.0]);
        l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([300.0, 300.0, 100.0]);
    } else {
        let mut ids = Ids(&mut next);
        let mut rect = build::shape_rect(&mut ids, [1800.0, 1200.0], [0.0, 0.0], 0.0);
        if animated {
            rect.prop_mut("size").unwrap().keys =
                vec![Keyframe::new(Tick::ZERO, Value::Vec2([1800.0, 1200.0])), Keyframe::new(s(1.0), Value::Vec2([1500.0, 1400.0]))];
        }
        let mut items = vec![rect, build::shape_fill(&mut ids, [0.9, 0.6, 0.2, 1.0])];
        for i in -5..=5 {
            items.insert(0, build::shape_rect(&mut ids, [40.0, 30.0 + i as f64], [i as f64 * 131.0 + 0.4, i as f64 * 77.0 - 0.3], 6.0));
        }
        items.insert(0, build::shape_fill(&mut ids, [0.1, 0.4, 0.9, 1.0]));
        let g = build::shape_group(&mut ids, "Backdrop", items);
        l.props.sub_mut("contents").unwrap().children.push(g.into());
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([1300.0, 600.0, 0.0]);
        l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(7.0);
    }
    let stack: Vec<(&str, Vec<(&str, Value)>)> = match kind {
        Kind::Caption => vec![
            ("ec.stylize.deepglow", vec![("PEDG-0001", Value::Scalar(20.0))]),
            ("ec.perspective.dropshadow", vec![("distance", Value::Scalar(5.0)), ("softness", Value::Scalar(10.0))]),
            ("ec.perspective.dropshadow", vec![("distance", Value::Scalar(0.0)), ("softness", Value::Scalar(30.0))]),
            ("ec.perspective.bevelalpha", vec![("edgeThickness", Value::Scalar(3.0))]),
        ],
        Kind::Ramp => vec![(
            "ec.generate.gradientramp",
            vec![("start", Value::Vec2([0.0, 0.0])), ("end", Value::Vec2([1280.0, 720.0])), ("startColor", Value::Color([1.0, 0.2, 0.1, 1.0]))],
        )],
        Kind::Sweep => vec![(
            "ec.generate.cclightsweep",
            vec![("center", Value::Vec2([600.0, 300.0])), ("edgeThickness", Value::Scalar(4.0)), ("edgeIntensity", Value::Scalar(80.0))],
        )],
        Kind::Backdrop => vec![("ec.blur.gaussian", vec![("blurriness", Value::Scalar(6.0))])],
    };
    for (i, (id, set)) in stack.into_iter().enumerate() {
        let spec = effectcraft_effects::find(id).unwrap();
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [W as f64, H as f64]);
        for (k, v) in set {
            let prop = g.prop_mut(k).unwrap();
            // The first effect's first value moves when the content is animated.
            if animated && text && i == 0 && matches!(v, Value::Scalar(_) | Value::Vec2(_)) && prop.keys.is_empty() {
                let to = match &v {
                    Value::Scalar(x) => Value::Scalar(x * 1.5),
                    Value::Vec2([x, y]) => Value::Vec2([x + 40.0, y + 20.0]),
                    _ => v.clone(),
                };
                prop.keys = vec![Keyframe::new(Tick::ZERO, v.clone()), Keyframe::new(s(1.0), to)];
            }
            prop.value = v;
        }
        l.props.sub_mut("effects").unwrap().children.push(g.into());
    }
    p.next_id = next;
    let sid = p.add_item(
        "Solid",
        Label::Sandstone,
        None,
        ItemKind::Solid(effectcraft_project::Solid { color: [0.2, 0.25, 0.3], width: W, height: H, pixel_aspect: 1.0 }),
    );
    let bg = build::layer(&mut p, &comp, "BG", LayerSource::Solid { item: sid }, (W, H), None);
    p.comp_mut(cid).unwrap().layers = vec![l, bg].into();
    (p, cid)
}

fn frame(p: &Project, cid: ItemId, scale: f64, t: Tick, clip: bool, cache: Option<&LayerCache>) -> Image {
    crate::set_shape_clip(clip);
    let mut r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
    r.cache = cache;
    let img = r.comp_frame(cid, t);
    crate::set_shape_clip(true);
    img
}

/// Pixels of the layer's content buffer (after effects), clipped or whole.
fn content_px(p: &Project, cid: ItemId, scale: f64, clip: bool) -> u64 {
    crate::set_shape_clip(clip);
    let r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
    let c = p.comp(cid).unwrap();
    let l: &Layer = c.layers.iter().next().unwrap();
    let b = r.content_buf(&EvalCtx::new(p, cid, c, Tick::ZERO), l).unwrap();
    crate::set_shape_clip(true);
    b.img.width as u64 * b.img.height as u64
}

/// One 8-bit step at most on a few channels (float sums round either way); Deep Glow's
/// Downsample and Dither follow its canvas, so two steps on a few more.
fn same(kind: Kind, a: &Image, b: &Image, at: &str) {
    assert_eq!((a.width, a.height), (b.width, b.height), "{at}");
    let (steps, share) = if kind == Kind::Caption { (2.0, 100) } else { (1.0, 1000) };
    let diffs: Vec<f32> = a.data.iter().zip(&b.data).flat_map(|(x, y)| (0..4).map(move |i| (x[i] - y[i]).abs())).collect();
    let worst = diffs.iter().copied().fold(0.0f32, f32::max);
    let off = diffs.iter().filter(|d| **d > 1e-6).count();
    assert!(worst <= steps / 255.0 + 1e-6 && off * share <= diffs.len(), "{at}: differs by {worst} on {off} of {} channels", diffs.len());
}

#[test]
fn top_level_text_and_shapes_draw_only_what_shows_with_the_same_frames() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for kind in [Kind::Caption, Kind::Ramp, Kind::Sweep, Kind::Backdrop] {
        for animated in [false, true] {
            let (mut p, cid) = scene(kind, animated);
            // The layer drifts left (content still or animated).
            let l = p.comp_mut(cid).unwrap().layers.iter_mut().next().unwrap();
            let pos = l.props.prop_mut("transform/position").unwrap();
            let Value::Vec3(at) = pos.value.clone() else { panic!("a layer's position is a Vec3") };
            pos.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3(at)), Keyframe::new(s(1.0), Value::Vec3([at[0] - 120.0, at[1], 0.0]))];
            // (At Half the buffers are under `clip::MIN_PIXELS`: drawn whole.)
            let (part, whole) = (content_px(&p, cid, 1.0, true), content_px(&p, cid, 1.0, false));
            assert!(part * 4 < whole * 3, "{kind:?} animated {animated}: drew {part} of {whole} px");
            // Cached frames against uncached whole ones: a moving Continuously Rasterized layer's
            // Gradient Ramp and Light Sweep stay put in the comp (its key follows the transform).
            let (cache, whole_cache) = (LayerCache::new(1 << 30), LayerCache::new(1 << 30));
            for f in 0..6 {
                let t = s(f as f64 / 30.0);
                let whole = frame(&p, cid, 1.0, t, false, None);
                let at = format!("{kind:?} animated {animated} frame {f}");
                same(kind, &frame(&p, cid, 1.0, t, true, Some(&cache)), &whole, &format!("{at} cached"));
                same(kind, &frame(&p, cid, 1.0, t, false, Some(&whole_cache)), &whole, &format!("{at} cached whole"));
                same(kind, &frame(&p, cid, 1.0, t, true, None), &whole, &at);
            }
            let t = s(0.2);
            same(kind, &frame(&p, cid, 0.5, t, true, None), &frame(&p, cid, 0.5, t, false, None), &format!("{kind:?} animated {animated} half"));
        }
    }
}
