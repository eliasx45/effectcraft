//! Text and shape layers in the top comp, partly off the frame, drawn only where they show
//! (`Renderer::shape_clip` for the source, `Renderer::fx_view` for the comp-sized effect canvas;
//! the CPU's own test against the whole layer is in effectcraft-render's tests_view_clip). GPU
//! frames with the layer cache on (the resident path) and clipping match the CPU drawing them
//! whole: 10-10's captions (Deep Glow, two Drop Shadows, Bevel Alpha), Gradient Ramp, CC Light
//! Sweep and a blurred backdrop, still and animated, Full and Half.

use std::sync::Mutex;

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, TextDoc, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer, set_shape_clip};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Pattern, check, diff, gpu, tolerance};

/// The clip tests flip the process-wide switch: one at a time.
pub(crate) static CLIP_SERIAL: Mutex<()> = Mutex::new(());

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
        l.switches.collapse = true;
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
        l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(7.0);
    }
    // Drifting left across the right edge.
    let at = if text { [900.3, 400.7] } else { [1300.0, 600.0] };
    l.props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([at[0], at[1], 0.0])), Keyframe::new(s(1.0), Value::Vec3([at[0] - 120.0, at[1], 0.0]))];
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

#[test]
fn top_level_text_and_shapes_drawn_where_they_show_match_the_whole_cpu_frame() {
    let _g = CLIP_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let g = gpu();
    assert!(g.is_some() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for kind in [Kind::Caption, Kind::Ramp, Kind::Sweep, Kind::Backdrop] {
        for animated in [false, true] {
            let (p, cid) = scene(kind, animated);
            for scale in [1.0, 0.5] {
                let opts = RenderOpts { scale, ..Default::default() };
                let cache = LayerCache::new(1 << 30);
                for f in 0..4 {
                    let t = s(f as f64 / 30.0);
                    set_shape_clip(true);
                    let mut r = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts });
                    r.accel = Some(g);
                    r.cache = Some(&cache);
                    let img = g.render(&r, cid, t).expect("the GPU renders the frame");
                    set_shape_clip(false);
                    let cpu = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Cpu, ..opts }).comp_frame_cpu(cid, t);
                    set_shape_clip(true);
                    let mut d = diff(&cpu, &img, tolerance(p.settings.bit_depth));
                    d.quantized = true;
                    // Deep Glow's Downsample and Dither follow its canvas (two 8-bit steps on a
                    // few pixels).
                    let allow = if kind == Kind::Caption { 0.01 } else { 0.0 };
                    check(&format!("{kind:?} animated {animated} scale {scale} frame {f}"), Some(d), allow);
                }
            }
        }
    }
}
