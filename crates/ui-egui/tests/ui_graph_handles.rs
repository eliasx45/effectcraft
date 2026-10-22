//! Graph Editor influence / velocity handles stay grabbable on any graph, as in After Effects:
//! a selected key's handle whose influence is tiny (it would sit under the key) or whose value
//! lies far off the visible range (a speed spike) is still drawn on screen at a usable length,
//! and a press on it drags the handle, not the key or the transform box. The owner could not
//! grab the in handle of a camera Position key whose speed spiked to ~7300 px/s right before it.

use effectcraft_engine::Session;
use effectcraft_engine::project::LayerId;
use effectcraft_ui_egui::EffectcraftApp;
use egui::{Event, Pos2, Rect, pos2, vec2};
use egui_kittest::Harness;
use serde_json::json;

/// A 2 s, 30 fps 3D comp with a camera whose Position goes from key 0 at 0 s to key 1 at 23
/// frames; key 0 eases out slowly and key 1's in influence is 0.5 % at speed 0, so the speed
/// graph creeps up and shoots to a spike right before key 1. Key 1 is selected; the Graph
/// Editor shows `mode` with Show Selected, Snap and Transform Box on (the owner's setup).
fn harness(mode: &str) -> (Harness<'static, EffectcraftApp>, LayerId, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cam", "width": 1920, "height": 1080, "frameRate": 30, "duration": 2})).unwrap();
    let id = LayerId(s.execute("layer.newCamera", json!({})).unwrap()["layer"].as_u64().unwrap());
    let t1 = 23.0 / 30.0;
    for (t, v) in [(0.0, [960.0, 540.0, -1500.0]), (t1, [1328.0, 287.0, -1243.9])] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/position", "time": t, "value": v})).unwrap();
    }
    let uid = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/position").unwrap().uid;
    s.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": 0.0, "side": "out", "speed": 0.0, "influence": 60.0})).unwrap();
    s.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": t1, "side": "in", "speed": 0.0, "influence": 0.5})).unwrap();
    s.execute("layer.select", json!({"layers": [id.0]})).unwrap();
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": t1}]})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    let tl = &mut h.state_mut().ui.timeline;
    tl.graph_mode = mode.into();
    tl.graph_show_selected = true;
    tl.graph_snap = true;
    tl.graph_transform_box = true;
    tl.graph_editor = true;
    h.run_steps(5);
    // The whole segment in view (opening the editor fits the lone selected key, zoomed right in):
    // at 600 px/s the 0.5 % influence is about 2 px, under the key.
    let tl = &mut h.state_mut().ui.timeline;
    tl.pps = Some(600.0);
    tl.start = 0.0;
    h.run_steps(3);
    (h, id, uid)
}

fn rect(h: &Harness<'_, EffectcraftApp>, id: &str) -> Rect {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no element {id}")).clone();
    Rect::from_min_size(pos2(e.rect[0], e.rect[1]), vec2(e.rect[2], e.rect[3]))
}

/// Key 1's (in influence %, in speed, time s).
fn key1(h: &Harness<'_, EffectcraftApp>, id: LayerId) -> (f64, f64, f64) {
    let l = h.state().session.active_comp().unwrap().layer(id).unwrap().clone();
    let k = l.props.prop("transform/position").unwrap().keys[1].clone();
    let e = k.in_ease.first().copied().unwrap_or_default();
    (e.influence * 100.0, e.speed, k.time.seconds())
}

/// Press at `from`, move to `to` in small steps (a real drag), release.
fn drag(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    for i in 1..=24 {
        h.input_mut().events.push(Event::PointerMoved(from + (to - from) * (i as f32 / 24.0)));
        h.step();
    }
    h.input_mut().events.push(Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.run_steps(2);
}

#[test]
fn a_speed_spike_keys_in_handle_is_grabbable_and_drags_its_influence() {
    let (mut h, id, uid) = harness("speed");
    let plot = rect(&h, "timeline.graph");
    let key = rect(&h, &format!("timeline.graph.key.{uid}.0.1")).center();
    let hr = rect(&h, &format!("timeline.graph.handle.{uid}.0.1.in"));
    let hp = hr.center();
    assert!(plot.contains(hp), "the handle is on the plot: {hp:?} in {plot:?}");
    assert!(key.x - hp.x >= 10.0, "the handle stands clear of its key: {hp:?} vs key {key:?}");
    let (inf0, _, t0) = key1(&h, id);
    // Left by a third of the segment (the keys are 23 frames apart): influence goes up.
    let px_per_s = key.x - rect(&h, &format!("timeline.graph.key.{uid}.0.0")).center().x;
    drag(&mut h, hp, hp - vec2(px_per_s * 23.0 / 30.0 / 3.0, 0.0));
    let (inf1, _, t1) = key1(&h, id);
    assert!((t1 - t0).abs() < 1e-9, "the key stayed put: {t0} → {t1}");
    assert!(inf1 > inf0 + 20.0, "the in influence followed the handle: {inf0} → {inf1}");
}

#[test]
fn a_handle_off_the_visible_range_stays_on_screen_and_drags() {
    let (mut h, id, uid) = harness("speed");
    let t1 = 23.0 / 30.0;
    // A fast arrival (5000 px/s) with a 30 % influence, and the graph zoomed to 0–100 px/s: the
    // handle's speed is far above the top of the plot.
    h.state_mut().session.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": t1, "side": "in", "speed": 5000.0, "influence": 30.0})).unwrap();
    let tl = &mut h.state_mut().ui.timeline;
    tl.graph_auto_zoom = false;
    tl.graph_range = Some((0.0, 100.0));
    h.run_steps(3);
    let plot = rect(&h, "timeline.graph");
    let hp = rect(&h, &format!("timeline.graph.handle.{uid}.0.1.in")).center();
    assert!(plot.contains(hp), "the handle is pinned inside the plot: {hp:?} in {plot:?}");
    let (inf0, sp0, _) = key1(&h, id);
    // Down: the speed drops (from the 5000 it had, by the pointer's travel in graph units).
    drag(&mut h, hp, hp + vec2(0.0, plot.height() * 0.25));
    let (inf1, sp1, _) = key1(&h, id);
    assert!(sp1 < sp0 - 10.0, "the speed followed the handle: {sp0} → {sp1}");
    assert!((inf1 - inf0).abs() < 1.0, "a vertical drag keeps the influence: {inf0} → {inf1}");
}

#[test]
fn a_value_graph_handle_on_top_of_its_key_is_grabbable() {
    let (mut h, _, _) = harness("value");
    // Value graph: Position is spatial (no handles there); a solid's Opacity with a 0.2 % in
    // influence puts key 1's handle under the key.
    let t1: f64 = 23.0 / 30.0;
    let s = &mut h.state_mut().session;
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 80, "height": 80})).unwrap();
    let id = s.active_comp().unwrap().layers.iter().find(|l| l.name == "Box").unwrap().id;
    for (t, v) in [(0.0, 0.0), (t1, 100.0)] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/opacity", "time": t, "value": v})).unwrap();
    }
    let op = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().uid;
    s.execute("keys.setEase", json!({"layer": id.0, "prop": op, "time": t1, "side": "in", "speed": 0.0, "influence": 0.2})).unwrap();
    s.execute("layer.select", json!({"layers": [id.0]})).unwrap();
    s.execute("prop.select", json!({"layer": id.0, "prop": op})).unwrap();
    s.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": op, "time": t1}]})).unwrap();
    h.run_steps(4);
    let key = rect(&h, &format!("timeline.graph.key.{op}.0.1")).center();
    let hp = rect(&h, &format!("timeline.graph.handle.{op}.0.1.in")).center();
    assert!(key.x - hp.x >= 10.0, "the handle stands clear of its key: {hp:?} vs key {key:?}");
    let px_per_s = key.x - rect(&h, &format!("timeline.graph.key.{op}.0.0")).center().x;
    drag(&mut h, hp, hp - vec2(px_per_s * t1 as f32 / 3.0, 0.0));
    let l = h.state().session.active_comp().unwrap().layer(id).unwrap().clone();
    let k = l.props.prop("transform/opacity").unwrap().keys[1].clone();
    assert!((k.time.seconds() - t1).abs() < 1e-9, "the key stayed put: {}", k.time.seconds());
    assert!(k.in_ease[0].influence > 0.2, "the in influence followed the handle: {}", k.in_ease[0].influence);
}

#[test]
fn auto_height_expands_for_a_handle_drag_beyond_the_plot() {
    let (mut h, id, uid) = harness("speed");
    h.state_mut().ui.timeline.graph_auto_zoom = true;
    h.run_steps(3);
    let plot = rect(&h, "timeline.graph");
    let before = h.state().ui.timeline.graph_range.unwrap();
    let from = rect(&h, &format!("timeline.graph.handle.{uid}.0.1.in")).center();
    drag(&mut h, from, pos2(from.x, plot.min.y - 80.0));
    let after = h.state().ui.timeline.graph_range.unwrap();
    assert!(after.1 > before.1, "range expands: {before:?} → {after:?}");
    assert!(key1(&h, id).1 > 0.0);
    assert!(h.state().ui.timeline.graph_auto_zoom);
}

/// After Effects draws a handle at 100 % influence halfway across its segment, so two handles at
/// 100 % meet in the middle and never cross; dragging one to the middle sets 100 %.
#[test]
fn a_full_influence_handle_reaches_halfway_and_drags_to_100_percent() {
    let (mut h, id, uid) = harness("speed");
    let t1 = 23.0 / 30.0;
    h.state_mut().session.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": t1, "side": "in", "speed": 0.0, "influence": 100.0})).unwrap();
    h.run_steps(3);
    let k0 = rect(&h, &format!("timeline.graph.key.{uid}.0.0")).center();
    let k1 = rect(&h, &format!("timeline.graph.key.{uid}.0.1")).center();
    let hp = rect(&h, &format!("timeline.graph.handle.{uid}.0.1.in")).center();
    let middle = (k0.x + k1.x) * 0.5;
    assert!((hp.x - middle).abs() < 2.0, "100 % sits halfway: {} vs {middle}", hp.x);
    h.state_mut().session.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": t1, "side": "in", "speed": 0.0, "influence": 25.0})).unwrap();
    h.run_steps(3);
    let hp = rect(&h, &format!("timeline.graph.handle.{uid}.0.1.in")).center();
    // Past the middle and on: it stops at 100 %.
    drag(&mut h, hp, pos2(k0.x + 4.0, hp.y));
    assert!((key1(&h, id).0 - 100.0).abs() < 1e-6, "dragged to the far key: {}", key1(&h, id).0);
}

/// Zooming in on time keeps the whole curve's height in view with Auto-zoom (After Effects): a
/// speed spike outside the visible span still sets the top.
#[test]
fn auto_height_keeps_a_peak_out_of_view() {
    let (mut h, _, _) = harness("speed");
    h.state_mut().ui.timeline.graph_auto_zoom = true;
    h.run_steps(3);
    let whole = h.state().ui.timeline.graph_range.unwrap();
    // Only the slow start in view: the spike before key 1 is off to the right.
    let tl = &mut h.state_mut().ui.timeline;
    tl.pps = Some(6000.0);
    tl.start = 0.0;
    h.run_steps(3);
    let zoomed = h.state().ui.timeline.graph_range.unwrap();
    assert!(zoomed.1 >= whole.1 * 0.99, "the top stays at the peak: {whole:?} → {zoomed:?}");
}

/// A 2D solid whose Scale goes from 100 % to (120, 150) % over 1 s with Easy Ease, selected, in
/// graph `mode`. Returns the layer and Scale's uid.
fn scale_harness(mode: &str) -> (Harness<'static, EffectcraftApp>, LayerId, u64) {
    let (mut h, _, _) = harness(mode);
    let s = &mut h.state_mut().session;
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 80, "height": 80})).unwrap();
    let id = s.active_comp().unwrap().layers.iter().find(|l| l.name == "Box").unwrap().id;
    for (t, v) in [(0.0, [100.0, 100.0, 100.0]), (1.0, [120.0, 150.0, 100.0])] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/scale", "time": t, "value": v})).unwrap();
    }
    let sc = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/scale").unwrap().uid;
    s.execute("layer.select", json!({"layers": [id.0]})).unwrap();
    s.execute("prop.select", json!({"layer": id.0, "prop": sc})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    let tl = &mut h.state_mut().ui.timeline;
    tl.pps = Some(600.0);
    tl.start = 0.0;
    tl.graph_auto_zoom = true;
    h.run_steps(4);
    (h, id, sc)
}

/// After Effects shows Scale as one speed curve and a handle drag eases X and Y together: the
/// same influence, and each dimension its own share of the speed. Only X used to change.
#[test]
fn scale_is_one_speed_curve_and_a_handle_drag_eases_every_dimension() {
    let (mut h, id, sc) = scale_harness("speed");
    // One curve, led by Y (it moves most): no X or Z handles.
    assert!(h.state().auto.find(&format!("timeline.graph.handle.{sc}.0.1.in")).is_none(), "no separate X curve");
    assert!(h.state().auto.find(&format!("timeline.graph.handle.{sc}.2.1.in")).is_none(), "no Z curve");
    let hp = rect(&h, &format!("timeline.graph.handle.{sc}.1.1.in")).center();
    let plot = rect(&h, "timeline.graph");
    drag(&mut h, hp, hp - vec2(40.0, plot.height() * 0.2));
    let l = h.state().session.active_comp().unwrap().layer(id).unwrap().clone();
    let k = l.props.prop("transform/scale").unwrap().keys[1].clone();
    let (x, y) = (k.in_ease[0], k.in_ease[1]);
    assert!(y.influence > 1.0 / 3.0 + 0.01, "the influence followed the handle: {}", y.influence);
    assert!((x.influence - y.influence).abs() < 1e-9, "X and Y share the influence: {} vs {}", x.influence, y.influence);
    assert!(y.speed > 0.0, "the speed followed the handle: {}", y.speed);
    assert!((x.speed / y.speed - 20.0 / 50.0).abs() < 1e-6, "X takes its share of the speed: {} vs {}", x.speed, y.speed);
}

/// A 2D layer's Scale shows X and Y in the value graph, never a flat Z (After Effects).
#[test]
fn a_2d_layers_scale_shows_no_z_in_the_value_graph() {
    let (h, _, sc) = scale_harness("value");
    assert!(h.state().auto.find(&format!("timeline.graph.key.{sc}.0.0")).is_some(), "X shows");
    assert!(h.state().auto.find(&format!("timeline.graph.key.{sc}.1.0")).is_some(), "Y shows");
    assert!(h.state().auto.find(&format!("timeline.graph.key.{sc}.2.0")).is_none(), "no Z");
}
