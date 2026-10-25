//! Dragging layers to a new place in the Timeline's stack (egui_kittest, real pointer drags):
//! an adjustment layer dragged down several rows lands where the line shows; the release always
//! ends the drag (also with the dragged row scrolled out of view, or outside the list); and the
//! pointer held at the bottom edge scrolls the layers, as in After Effects.

use effectcraft_engine::Session;
use effectcraft_ui_egui::EffectcraftApp;
use egui::{Event, Modifiers, Pos2, Rect, pos2, vec2};
use egui_kittest::Harness;
use serde_json::json;

/// Comp "Main" with `solids` solids S0 (bottom) … and adjustment layer "Adjust" on top.
fn harness(solids: usize) -> Harness<'static, EffectcraftApp> {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    for i in 0..solids {
        s.execute("layer.newSolid", json!({"name": format!("S{i}"), "color": "#406080"})).unwrap();
    }
    s.execute("layer.newAdjustment", json!({"name": "Adjust"})).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(3);
    h
}

fn rect(h: &Harness<'_, EffectcraftApp>, id: &str) -> Rect {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}")).clone();
    Rect::from_min_size(pos2(e.rect[0], e.rect[1]), vec2(e.rect[2], e.rect[3]))
}

fn names(h: &Harness<'_, EffectcraftApp>) -> Vec<String> {
    h.state().session.active_comp().unwrap().layers.iter().map(|l| l.name.clone()).collect()
}

fn layer_id(h: &Harness<'_, EffectcraftApp>, name: &str) -> u64 {
    h.state().session.active_comp().unwrap().layers.iter().find(|l| l.name == name).unwrap().id.0
}

/// Where to grab a layer's row: on its name.
fn grab(h: &Harness<'_, EffectcraftApp>, name: &str) -> Pos2 {
    rect(h, &format!("timeline.layer.{}.name", layer_id(h, name))).center()
}

fn row(h: &Harness<'_, EffectcraftApp>, name: &str) -> Rect {
    rect(h, &format!("timeline.layer.{}.row", layer_id(h, name)))
}

/// The layers being dragged, while a reorder drag is on.
fn dragging(h: &Harness<'_, EffectcraftApp>) -> Option<Vec<u64>> {
    h.ctx.data(|d| d.get_temp::<Vec<u64>>(egui::Id::new("tl-layer-drag")))
}

fn press(h: &mut Harness<'_, EffectcraftApp>, at: Pos2) {
    h.event(Event::PointerMoved(at));
    h.step();
    h.event(Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
}

/// Move the held pointer from `from` to `to` in steps.
fn move_to(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    for k in 1..=10 {
        h.event(Event::PointerMoved(from + (to - from) * (k as f32 / 10.0)));
        h.step();
    }
}

fn release(h: &mut Harness<'_, EffectcraftApp>, at: Pos2) {
    h.event(Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn drag(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    press(h, from);
    move_to(h, from, to);
    release(h, to);
}

#[test]
fn an_adjustment_layer_dragged_down_several_rows_lands_there() {
    let mut h = harness(6);
    assert_eq!(names(&h), ["Adjust", "S5", "S4", "S3", "S2", "S1", "S0"]);
    let steps = h.state().session.history.undo.len();
    // Below S2's middle: between S2 and S1.
    let s2 = row(&h, "S2");
    let from = grab(&h, "Adjust");
    drag(&mut h, from, pos2(from.x, s2.center().y + s2.height() * 0.4));
    assert_eq!(names(&h), ["S5", "S4", "S3", "S2", "Adjust", "S1", "S0"]);
    assert_eq!(h.state().session.history.undo.len(), steps + 1, "one undo step");
    assert!(dragging(&h).is_none(), "the release ends the drag");
    // Down again, below the last row: to the bottom of the stack.
    let s0 = row(&h, "S0");
    let from = grab(&h, "Adjust");
    drag(&mut h, from, pos2(from.x, s0.max.y + 4.0));
    assert_eq!(names(&h), ["S5", "S4", "S3", "S2", "S1", "S0", "Adjust"]);
    // Other layer types move the same way: a solid from the top to between S1 and S0.
    let s0 = row(&h, "S0");
    let from = grab(&h, "S5");
    drag(&mut h, from, pos2(from.x, s0.min.y + 2.0));
    assert_eq!(names(&h), ["S4", "S3", "S2", "S1", "S5", "S0", "Adjust"]);
    assert!(dragging(&h).is_none());
}

/// The release ends the drag wherever it happens: with the dragged row scrolled out of view
/// (before, the drop waited for that row's own release and the drag never ended), and outside
/// the layer list. A later drag works as usual.
#[test]
fn releasing_always_ends_a_layer_drag() {
    let mut h = harness(60);
    let from = grab(&h, "Adjust");
    press(&mut h, from);
    let s55 = row(&h, "S55");
    let to = pos2(from.x, s55.center().y);
    move_to(&mut h, from, to);
    assert!(dragging(&h).is_some(), "dragging");
    // Scroll the dragged row away (a trackpad scroll), then let go over the list.
    h.state_mut().ui.timeline.scroll_y = 300.0;
    h.run_steps(2);
    assert!(h.state().auto.find(&format!("timeline.layer.{}.row", layer_id(&h, "Adjust"))).is_none(), "the dragged row is out of view");
    let list = rect(&h, "timeline.layerMarquee");
    let at = pos2(from.x, list.center().y);
    h.event(Event::PointerMoved(at));
    h.step();
    release(&mut h, at);
    assert!(dragging(&h).is_none(), "the release ends the drag");
    let moved = names(&h).iter().position(|n| n == "Adjust").unwrap();
    assert!(moved > 5, "the layer dropped where the line was: index {moved}");

    // Released outside the list (over the viewer): the drag ends there too.
    h.state_mut().ui.timeline.scroll_y = 0.0;
    h.run_steps(3);
    let from = grab(&h, "S59");
    press(&mut h, from);
    move_to(&mut h, from, pos2(from.x, from.y + 40.0));
    assert!(dragging(&h).is_some());
    let outside = pos2(from.x, 150.0);
    move_to(&mut h, pos2(from.x, from.y + 40.0), outside);
    release(&mut h, outside);
    assert!(dragging(&h).is_none(), "a release outside the list ends the drag");

    // And a plain drag afterwards still reorders.
    h.state_mut().ui.timeline.scroll_y = 0.0;
    h.run_steps(3);
    let top = names(&h)[0].clone();
    let second = row(&h, &names(&h)[1]);
    let from = grab(&h, &top);
    drag(&mut h, from, pos2(from.x, second.center().y + second.height() * 0.4));
    assert_eq!(names(&h)[1], top, "the next drag moved the layer one row down");
    assert!(dragging(&h).is_none());
}

/// Holding a dragged layer at (or past) the bottom edge of the list scrolls the layers, faster
/// past the edge; dropped there it lands below the rows that scrolled into view.
#[test]
fn holding_a_layer_drag_at_the_bottom_edge_scrolls_the_list() {
    let mut h = harness(60);
    assert_eq!(h.state().ui.timeline.scroll_y, 0.0);
    let list = rect(&h, "timeline.layerMarquee");
    let from = grab(&h, "Adjust");
    press(&mut h, from);
    let edge = pos2(from.x, list.max.y - 3.0);
    move_to(&mut h, from, edge);
    // Held still at the edge: the list keeps scrolling.
    let before = h.state().ui.timeline.scroll_y;
    h.run_steps(3);
    let at_edge = h.state().ui.timeline.scroll_y - before;
    assert!(at_edge > 0.0, "the list scrolls with the pointer held at the bottom edge");
    // Past the edge it scrolls faster.
    let past = pos2(from.x, list.max.y + 40.0);
    h.event(Event::PointerMoved(past));
    h.step();
    let before = h.state().ui.timeline.scroll_y;
    h.run_steps(3);
    let past_edge = h.state().ui.timeline.scroll_y - before;
    assert!(past_edge > at_edge, "faster past the edge: {past_edge} vs {at_edge}");
    // Back inside, away from the edges: it stops.
    let mid = pos2(from.x, list.center().y);
    h.event(Event::PointerMoved(mid));
    h.step();
    let before = h.state().ui.timeline.scroll_y;
    h.run_steps(3);
    assert_eq!(h.state().ui.timeline.scroll_y, before, "no scrolling away from the edges");
    assert!(before > 100.0, "scrolled: {before}");
    release(&mut h, mid);
    assert!(dragging(&h).is_none());
    let moved = names(&h).iter().position(|n| n == "Adjust").unwrap();
    assert!(moved > 10, "dropped among the rows scrolled into view: index {moved}");
}
