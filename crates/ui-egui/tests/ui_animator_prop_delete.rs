//! PROP-REMOVE: a text animator's property (Fill Color…) can be deleted on its own from the
//! Timeline: its row's context menu has Delete (`timeline.prop.<uid>.menu.delete`), and the
//! Delete key with it selected removes it, not the layer. Fixed properties have no Delete.

use effectcraft_engine::Session;
use effectcraft_engine::project::{LayerId, Node, PropGroup};
use effectcraft_ui_egui::EffectcraftApp;
use effectcraft_ui_egui::dock::PanelKind;
use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use serde_json::json;

fn harness() -> (Harness<'static, EffectcraftApp>, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "duration": 4})).unwrap();
    let t = s.execute("layer.newText", json!({"text": "ABCD"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.addTextAnimator", json!({"layer": t, "properties": ["fillColor", "opacity"]})).unwrap();
    // Twirl open the layer and every group of its text, down to Animator 1's properties.
    let mut groups = vec![];
    if let Some(text) = s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.sub("text") {
        collect_groups(text, &mut groups);
    }
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.state_mut().ui.timeline.open_layers.insert(t);
    h.state_mut().ui.timeline.open_groups.extend(groups);
    h.run_steps(4);
    (h, t)
}

fn collect_groups(g: &PropGroup, out: &mut Vec<u64>) {
    out.push(g.uid);
    for c in &g.children {
        if let Node::Group(x) = c {
            collect_groups(x, out);
        }
    }
}

fn kinds(h: &Harness<'_, EffectcraftApp>, t: u64) -> Vec<String> {
    let l = h.state().session.active_comp().unwrap().layer(LayerId(t)).unwrap().clone();
    l.props.group("text/animators/#1/properties").unwrap().props().map(|p| p.match_id.clone()).collect()
}

fn prop_uid(h: &Harness<'_, EffectcraftApp>, t: u64, path: &str) -> u64 {
    h.state().session.active_comp().unwrap().layer(LayerId(t)).unwrap().props.prop(path).unwrap().uid
}

fn center(h: &Harness<'_, EffectcraftApp>, id: &str) -> Pos2 {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}"));
    pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
}

fn click(h: &mut Harness<'_, EffectcraftApp>, p: Pos2, button: PointerButton) {
    h.input_mut().events.push(Event::PointerMoved(p));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

// TODO(verify-tonight): not run; relies on the animator's property rows showing once every
// group of the text is in `open_groups`, and on the context menu registering its items.
#[test]
fn the_context_menu_deletes_a_text_animator_property() {
    let (mut h, t) = harness();
    let fill = prop_uid(&h, t, "text/animators/#1/properties/fillColor");
    let p = center(&h, &format!("timeline.prop.{fill}.name"));
    click(&mut h, p, PointerButton::Secondary);
    let d = center(&h, &format!("timeline.prop.{fill}.menu.delete"));
    click(&mut h, d, PointerButton::Primary);
    assert_eq!(kinds(&h, t), ["opacity"], "Delete removed Fill Color");
    assert_eq!(h.state().session.active_comp().unwrap().layers.len(), 1, "the layer stays");
    h.input_mut().events.push(Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
    // A fixed property's menu has no Delete.
    let pos = prop_uid(&h, t, "transform/position");
    let transform = h.state().session.active_comp().unwrap().layer(LayerId(t)).unwrap().props.sub("transform").unwrap().uid;
    h.state_mut().ui.timeline.open_groups.insert(transform);
    h.run_steps(3);
    let p = center(&h, &format!("timeline.prop.{pos}.name"));
    click(&mut h, p, PointerButton::Secondary);
    assert!(h.state().auto.find(&format!("timeline.prop.{pos}.menu.reset")).is_some(), "Position's menu is open");
    assert!(h.state().auto.find(&format!("timeline.prop.{pos}.menu.delete")).is_none(), "no Delete on a fixed property");
}

// TODO(verify-tonight): not run; assumes Delete reaches edit.clear with the Timeline focused.
#[test]
fn the_delete_key_deletes_the_selected_animator_property() {
    let (mut h, t) = harness();
    let fill = prop_uid(&h, t, "text/animators/#1/properties/fillColor");
    {
        let s = &mut h.state_mut().session;
        s.execute("layer.select", json!({"layers": [t]})).unwrap();
        s.execute("prop.select", json!({"layer": t, "prop": fill, "selectKeys": false})).unwrap();
    }
    h.state_mut().ui.focused = PanelKind::Timeline;
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: Key::Delete, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.input_mut().events.push(Event::Key { key: Key::Delete, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
    assert_eq!(kinds(&h, t), ["opacity"]);
    assert_eq!(h.state().session.active_comp().unwrap().layers.len(), 1, "the layer stays");
}
