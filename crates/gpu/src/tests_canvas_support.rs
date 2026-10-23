//! Drop Shadow, Deep Glow, Bevel Alpha and Fill on a text or shape layer run on the part of the
//! comp-sized effect canvas they can reach (`Renderer::canvas_support`; the CPU test against the
//! whole canvas is in effectcraft-render's tests_styles). The GPU's trimmed canvas must give the
//! CPU's frame: each effect alone and stacked as on 10-10's captions, 2D and a 3D Continuously
//! Rasterized layer scaled up, at Full and Half.

use effectcraft_color::Label;
use effectcraft_keyframe::{TextDoc, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Comp, ItemKind, LayerSource, Project};
use effectcraft_render::RenderOpts;
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Scene, check, compare_at, n, set};

fn scene(text: bool, three_d: bool, stack: &[(&str, Vec<(&str, Value)>)]) -> Scene {
    let mut p = Project::default();
    let comp = Comp::new(400, 200, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let mut s = Scene { p, cid, comp };
    let src = if text { LayerSource::Text } else { LayerSource::Shape };
    let mut l = build::layer(&mut s.p, &s.comp, "L", src, (400, 200), None);
    if text {
        set(&mut l, "text/sourceText", Value::Text(Box::new(TextDoc { text: "Ok".into(), size: 30.0, ..Default::default() })));
    } else {
        let mut next = s.p.next_id;
        let mut ids = Ids(&mut next);
        let items = vec![build::shape_rect(&mut ids, [40.0, 26.0], [0.0, 0.0], 6.0), build::shape_fill(&mut ids, [0.9, 0.6, 0.2, 1.0])];
        let g = build::shape_group(&mut ids, "R", items);
        s.p.next_id = next;
        l.props.sub_mut("contents").unwrap().children.push(g.into());
    }
    set(&mut l, "transform/position", Value::Vec3([150.7, 90.3, 0.0]));
    if three_d {
        l.switches.three_d = true;
        l.switches.collapse = true;
        set(&mut l, "transform/scale", Value::Vec3([180.0, 180.0, 100.0]));
    }
    for (id, vals) in stack {
        s.effect(&mut l, id, vals);
    }
    s.push(l);
    s
}

#[test]
fn shadows_deep_glow_and_bevel_on_a_trimmed_canvas_match_the_cpu() {
    let shadow = |d: f64, sf: f64| ("ec.perspective.dropshadow", vec![("distance", n(d)), ("softness", n(sf))]);
    let glow = ("ec.stylize.deepglow", vec![("PEDG-0001", n(24.0))]);
    let bevel = ("ec.perspective.bevelalpha", vec![("edgeThickness", n(3.0))]);
    let fill = ("ec.generate.fill", vec![("color", Value::Color([0.2, 0.9, 0.4, 1.0]))]);
    let stacks = [vec![shadow(12.0, 20.0)], vec![glow.clone()], vec![bevel.clone(), fill], vec![glow, shadow(4.0, 9.0), shadow(0.0, 30.0), bevel]];
    for (k, stack) in stacks.iter().enumerate() {
        for (text, three_d) in [(true, false), (true, true), (false, false), (false, true)] {
            let s = scene(text, three_d, stack);
            let label = format!("stack {k}, text {text}, 3D {three_d}");
            check(&label, compare_at(&s, RenderOpts::default(), Tick::ZERO), 0.0);
            check(&format!("{label} half"), compare_at(&s, RenderOpts { scale: 0.5, ..Default::default() }, Tick::ZERO), 0.0);
        }
    }
}
