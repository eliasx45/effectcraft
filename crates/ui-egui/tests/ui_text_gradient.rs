//! Text Gradient Fill (EffectCraft extension): with the text layer selected, the animator's
//! Gradient Start / End are dots on the selected word in the Composition viewer
//! (`viewer.textGradient.<animator>.start|end`), where the animation draws it; a drag sets the
//! point in percent of the word's box in one undo step and snaps to its centre; the Properties
//! panel shows the colour stops bar, where a click under the bar adds a stop and Delete removes
//! the selected one.

use effectcraft_engine::Session;
use effectcraft_engine::keyframe::Value;
use effectcraft_engine::project::{Layer, LayerId};
use effectcraft_engine::render::EvalCtx;
use effectcraft_ui_egui::EffectcraftApp;
use effectcraft_ui_egui::dock::PanelKind;
use effectcraft_ui_egui::panels::viewer::comp_to_screen;
use egui::{Event, Modifiers, PointerButton, Pos2, pos2, vec2};
use egui_kittest::Harness;
use serde_json::json;

struct Ids {
    layer: u64,
    anim: u64,
    start: u64,
    end: u64,
    colors: u64,
}

fn layer(s: &Session, id: u64) -> Layer {
    s.active_comp().unwrap().layer(LayerId(id)).unwrap().clone()
}

/// "TESTING THIS ANIMATION" with a Gradient Fill on "THIS" (Based On Words, Index 1..2), the
/// layer selected; `prep` runs before the harness starts.
fn setup(prep: impl FnOnce(&mut Session, &Ids)) -> (Harness<'static, EffectcraftApp>, Ids) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 640, "height": 360, "duration": 4})).unwrap();
    let l = s.execute("layer.newText", json!({"text": "TESTING THIS ANIMATION", "size": 40, "fill": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    let anim = s.execute("layer.addTextAnimator", json!({"layer": l, "property": "gradientFill"})).unwrap()["animator"].as_u64().unwrap();
    let ly = layer(&s, l);
    let a = ly.props.find_group(anim).unwrap();
    let uid = |path: &str| a.prop(path).unwrap_or_else(|| panic!("no {path}")).uid;
    let ids = Ids { layer: l, anim, start: uid("properties/gradientStart"), end: uid("properties/gradientEnd"), colors: uid("properties/gradientFillColors") };
    for (path, v) in [
        ("selectors/#1/advanced/basedOn", json!(2)),
        ("selectors/#1/advanced/units", json!(1)),
        ("selectors/#1/start", json!(1)),
        ("selectors/#1/end", json!(2)),
    ] {
        s.execute("prop.set", json!({"layer": l, "prop": uid(path), "value": v})).unwrap();
    }
    prep(&mut s, &ids);
    s.state.selected_layers = vec![LayerId(l)];
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(4);
    (h, ids)
}

fn handle(h: &Harness<'_, EffectcraftApp>, id: &str) -> Option<Pos2> {
    h.state().auto.find(id).map(|e| pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
}

fn value2(h: &Harness<'_, EffectcraftApp>, ids: &Ids, uid: u64) -> [f64; 2] {
    layer(&h.state().session, ids.layer).props.find(uid).unwrap().value.as_vec2()
}

/// Where the renderer puts the group's start / end (screen points).
fn expected(h: &Harness<'_, EffectcraftApp>, ids: &Ids) -> (Pos2, Pos2) {
    let s = &h.state().session;
    let comp = s.active_comp().unwrap();
    let ctx = EvalCtx::new(&s.project, s.active_comp_id().unwrap(), comp, s.time());
    let l = comp.layer(LayerId(ids.layer)).unwrap();
    let geom = effectcraft_engine::render::text::text_geom(&ctx, l).unwrap();
    let g = geom.grad_groups.iter().find(|g| g.animator == ids.anim).expect("a gradient group");
    let glyph = geom.glyphs.iter().find(|x| x.char_index == g.char_index).unwrap();
    let lm = ctx.layer_to_comp(l).0;
    let m = lm * glyph.m2() * effectcraft_engine::geom::Mat3::translate(effectcraft_engine::geom::vec2(-glyph.rest[0], -glyph.rest[1]));
    let at = |p: [f64; 2]| {
        let v = m.apply(effectcraft_engine::geom::vec2(p[0], p[1]));
        comp_to_screen(&h.ctx, [v.x as f32, v.y as f32]).unwrap()
    };
    (at(g.start), at(g.end))
}

fn drag(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    h.event(Event::PointerMoved(from));
    h.step();
    h.event(Event::PointerButton { pos: from, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    for k in 1..=8 {
        h.event(Event::PointerMoved(from + (to - from) * (k as f32 / 8.0)));
        h.step();
    }
    h.event(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn click(h: &mut Harness<'_, EffectcraftApp>, pos: Pos2) {
    h.event(Event::PointerMoved(pos));
    h.step();
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

#[test]
fn handles_sit_on_the_selected_word_and_drag_in_one_undo_step() {
    let (mut h, ids) = setup(|_, _| {});
    let (sid, eid) = (format!("viewer.textGradient.{}.start", ids.anim), format!("viewer.textGradient.{}.end", ids.anim));
    let (a, b) = (handle(&h, &sid).expect("a Gradient Start handle"), handle(&h, &eid).expect("a Gradient End handle"));
    let (wa, wb) = expected(&h, &ids);
    assert!((a - wa).length() < 1.5 && (b - wb).length() < 1.5, "handles {a:?} {b:?} on the word's left and right middle {wa:?} {wb:?}");
    assert!(b.x - a.x > 40.0, "THIS is wide");
    // Drag End to the left by a quarter of the word (no snap there): x falls by about 25 %.
    let undo = h.state().session.history.undo.len();
    let to = b + vec2(-(b.x - a.x) * 0.27, 0.0) + vec2(0.0, 30.0);
    drag(&mut h, b, to);
    let end = value2(&h, &ids, ids.end);
    assert!((end[0] - 73.0).abs() < 3.0 && end[1] > 50.0, "End moved to the drop point in box percent: {end:?}");
    assert_eq!(h.state().session.history.undo.len(), undo + 1, "one undo step");
    assert_eq!(value2(&h, &ids, ids.start), [0.0, 50.0], "Start stays");
    // Near the box's centre (halfway between where Start and End began) the Start snaps to it.
    let centre = pos2((wa.x + wb.x) / 2.0, (wa.y + wb.y) / 2.0);
    drag(&mut h, a, centre + vec2(3.0, -2.0));
    assert_eq!(value2(&h, &ids, ids.start), [50.0, 50.0], "snapped to the centre");
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert_eq!(value2(&h, &ids, ids.start), [0.0, 50.0]);
}

#[test]
fn handles_follow_the_animated_word() {
    // A Position animator moving every character 60 px right and 40 down: the dots go along.
    let (h, ids) = setup(|s, ids| {
        let a = s.execute("layer.addTextAnimator", json!({"layer": ids.layer, "property": "position"})).unwrap()["animator"].as_u64().unwrap();
        let pos = layer(s, ids.layer).props.find_group(a).unwrap().prop("properties/position").unwrap().uid;
        s.execute("prop.set", json!({"layer": ids.layer, "prop": pos, "value": [60.0, 40.0, 0.0]})).unwrap();
    });
    let a = handle(&h, &format!("viewer.textGradient.{}.start", ids.anim)).unwrap();
    let (wa, _) = expected(&h, &ids);
    assert!((a - wa).length() < 1.5, "{a:?} vs {wa:?}");
    let z = comp_to_screen(&h.ctx, [100.0, 0.0]).unwrap().x - comp_to_screen(&h.ctx, [0.0, 0.0]).unwrap().x;
    let still = {
        let (h0, ids0) = setup(|_, _| {});
        handle(&h0, &format!("viewer.textGradient.{}.start", ids0.anim)).unwrap()
    };
    let d = a - still;
    assert!((d.x - 0.6 * z).abs() < 1.5 && (d.y - 0.4 * z).abs() < 1.5, "moved with the word: {d:?} (zoom {z})");
}

#[test]
fn stops_bar_adds_and_deletes_colour_stops() {
    let (mut h, ids) = setup(|_, _| {});
    h.state_mut().show_panel(PanelKind::Properties);
    h.run_steps(4);
    let prefix = format!("properties.gradientFill.{}.stops", ids.anim);
    let bar = h.state().auto.find(&format!("{prefix}.bar")).unwrap_or_else(|| panic!("no {prefix}.bar")).rect;
    let n = |h: &Harness<'_, EffectcraftApp>| match &layer(&h.state().session, ids.layer).props.find(ids.colors).unwrap().value {
        Value::Gradient(g) => g.colors.len(),
        _ => 0,
    };
    assert_eq!(n(&h), 3, "the default yellow → orange → red");
    // A click under the bar at 25 % adds a stop there.
    click(&mut h, pos2(bar[0] + bar[2] * 0.25, bar[1] + bar[3] + 8.0));
    assert_eq!(n(&h), 4);
    let g = match &layer(&h.state().session, ids.layer).props.find(ids.colors).unwrap().value {
        Value::Gradient(g) => (**g).clone(),
        _ => unreachable!(),
    };
    assert!((g.colors[1].0 - 0.25).abs() < 0.02, "{:?}", g.colors);
    // The new stop is selected: Delete removes it.
    let del = handle(&h, &format!("{prefix}.delete")).unwrap();
    click(&mut h, del);
    assert_eq!(n(&h), 3);
    assert!(handle(&h, &format!("{prefix}.stop.2")).is_some() && handle(&h, &format!("{prefix}.edit")).is_some());
}
