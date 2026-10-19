//! Timeline controls used on one of several selected layers set every selected layer, as in
//! After Effects, as one undo step (19 Oct, after PARENT-SEL: switches, Mode, Track Matte, label
//! and dropped effects changed only the layer they were used on). On an unselected layer they
//! set that layer alone.

use effectcraft_engine::Session;
use effectcraft_engine::project::LayerId;
use effectcraft_ui_egui::EffectcraftApp;
use egui::{Event, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use serde_json::json;

fn center(h: &Harness<'_, EffectcraftApp>, id: &str) -> Pos2 {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}"));
    pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
}

fn click(h: &mut Harness<'_, EffectcraftApp>, at: Pos2) {
    h.event(Event::PointerMoved(at));
    h.step();
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn three_d(h: &Harness<'_, EffectcraftApp>, ids: &[u64]) -> Vec<bool> {
    let comp = h.state().session.active_comp().unwrap();
    ids.iter().map(|id| comp.layer(LayerId(*id)).unwrap().switches.three_d).collect()
}

#[test]
fn a_switch_on_a_selected_layer_sets_every_selected_layer() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    // Solids A (top) to D (bottom).
    let mut ids: Vec<u64> = ["D", "C", "B", "A"]
        .iter()
        .map(|name| s.execute("layer.newSolid", json!({"name": name, "color": "#406080"})).unwrap()["layer"].as_u64().unwrap())
        .collect();
    ids.reverse();
    s.execute("layer.select", json!({"layers": [ids[0], ids[1], ids[2]]})).unwrap();
    let before = s.project.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(4);

    // The 3D switch of B, one of the selected A–C: all three go 3D, D (unselected) stays 2D.
    let at = center(&h, &format!("timeline.layer.{}.switch.threeD", ids[1]));
    click(&mut h, at);
    assert_eq!(three_d(&h, &ids), [true, true, true, false]);
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert!(h.state().session.project.same_content(&before), "one undo step");

    // On the unselected D it sets D alone.
    let at = center(&h, &format!("timeline.layer.{}.switch.threeD", ids[3]));
    click(&mut h, at);
    assert_eq!(three_d(&h, &ids), [false, false, false, true]);

    // `layer.setTrackMatte` with `layers` (what the Track Matte menu sends for a selection):
    // each gets the matte, the matte layer itself is left out, in one undo step.
    let s = &mut h.state_mut().session;
    let steps = s.history.undo.len();
    s.execute("layer.setTrackMatte", json!({"layers": [ids[0], ids[1], ids[2]], "matte": ids[0], "kind": "luma"})).unwrap();
    assert_eq!(s.history.undo.len(), steps + 1, "one undo step");
    let comp = s.active_comp().unwrap();
    assert!(comp.layer(LayerId(ids[0])).unwrap().track_matte.is_none(), "the matte doesn't matte itself");
    for l in [ids[1], ids[2]] {
        assert_eq!(comp.layer(LayerId(l)).unwrap().track_matte.map(|m| m.layer), Some(LayerId(ids[0])));
    }
    assert!(!comp.layer(LayerId(ids[0])).unwrap().switches.video, "the matte layer is hidden");
    // The matte alone is still an error.
    assert!(s.execute("layer.setTrackMatte", json!({"layers": [ids[0]], "matte": ids[0]})).is_err());
    assert!(s.execute("layer.setTrackMatte", json!({"layer": ids[0], "matte": ids[0]})).is_err());
}
