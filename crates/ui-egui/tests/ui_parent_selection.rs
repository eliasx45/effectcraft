//! The Parent pick whip dragged from one of several selected layers parents all of them, as in
//! After Effects, each keeping its place on screen, as one undo step (19 Oct: only the dragged
//! layer got the Null). The parent itself and layers it descends from are skipped.

use effectcraft_engine::Session;
use effectcraft_engine::project::LayerId;
use effectcraft_ui_egui::EffectcraftApp;
use egui::{Event, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use serde_json::{Value, json};

fn center(h: &Harness<'_, EffectcraftApp>, id: &str) -> Pos2 {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}"));
    pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
}

fn drag(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: from, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    for k in 1..=8 {
        h.input_mut().events.push(Event::PointerMoved(from + (to - from) * (k as f32 / 8.0)));
        h.step();
    }
    h.input_mut().events.push(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn position(s: &mut Session, l: u64) -> Value {
    s.execute("prop.get", json!({"layer": l, "path": "transform/position"})).unwrap()["value"].clone()
}

#[test]
fn the_parent_pick_whip_parents_every_selected_layer() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "duration": 4, "fps": 24})).unwrap();
    let rig = s.execute("layer.newNull", json!({})).unwrap()["layer"].as_u64().unwrap();
    s.execute("prop.set", json!({"layer": rig, "path": "transform/position", "value": [40, 30]})).unwrap();
    s.execute("prop.set", json!({"layer": rig, "path": "transform/scale", "value": [200, 200]})).unwrap();
    let layers: Vec<u64> = (0..3)
        .map(|i| {
            let l = s.execute("layer.newSolid", json!({"name": format!("S{i}"), "color": "#406080"})).unwrap()["layer"].as_u64().unwrap();
            s.execute("prop.set", json!({"layer": l, "path": "transform/position", "value": [60 + 50 * i, 90 - 20 * i]})).unwrap();
            l
        })
        .collect();
    // Where each layer goes when parented on its own (the engine's compensation), then undone.
    let expected: Vec<Value> = layers
        .iter()
        .map(|&l| {
            s.execute("layer.setParent", json!({"layers": [l], "parent": rig})).unwrap();
            let p = position(&mut s, l);
            s.execute("edit.undo", json!({})).unwrap();
            p
        })
        .collect();
    assert_ne!(expected[0], json!([60.0, 90.0]), "parenting to the null re-expresses the position");
    s.execute("layer.select", json!({"layers": layers})).unwrap();
    let before = s.project.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(4);

    // Drag the pick whip of the middle layer onto the null's row.
    let (from, to) = (center(&h, &format!("timeline.layer.{}.pickWhip", layers[1])), center(&h, &format!("timeline.layer.{rig}.name")));
    drag(&mut h, from, to);
    for (l, want) in layers.iter().zip(&expected) {
        let comp = h.state().session.active_comp().unwrap();
        assert_eq!(comp.layer(LayerId(*l)).unwrap().parent, Some(LayerId(rig)), "layer {l} parented");
        assert_eq!(&position(&mut h.state_mut().session, *l), want, "layer {l} stays in place");
    }
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert!(h.state().session.project.same_content(&before), "one undo step");

    // With the null selected too, it is skipped rather than failing the edit.
    let s = &mut h.state_mut().session;
    let mut all = layers.clone();
    all.push(rig);
    s.execute("layer.setParent", json!({"layers": all, "parent": rig})).unwrap();
    let comp = s.active_comp().unwrap();
    assert!(layers.iter().all(|l| comp.layer(LayerId(*l)).unwrap().parent == Some(LayerId(rig))));
    assert_eq!(comp.layer(LayerId(rig)).unwrap().parent, None);
    // A single layer that would make a cycle is still an error.
    assert!(s.execute("layer.setParent", json!({"layers": [rig], "parent": layers[0]})).is_err());
}
