//! Style families (batch 1, CX3): link, change size once for every linked layer, unlink.
//! Not run yet (batch 1): TODO(batch1-verify) run `cargo test -p effectcraft-engine tests_style_families`.

use effectcraft_keyframe::Value as KV;
use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;

fn scale_x(s: &Session, layer: u64) -> f64 {
    let l = s.active_comp().unwrap().layer(LayerId(layer)).unwrap();
    match &l.props.prop("transform/scale").unwrap().value {
        KV::Vec2(v) => v[0],
        KV::Vec3(v) => v[0],
        other => panic!("{other:?}"),
    }
}

#[test]
fn changing_the_family_size_scales_every_linked_layer_once() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "F", "width": 200, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
    let a = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    let b = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    let c = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.setComment", json!({"layer": a, "comment": "hand-made"})).unwrap();
    s.execute("style.family.link", json!({"family": "Captions", "layers": [a, b]})).unwrap();
    let undo = s.history.undo.len();
    let r = s.execute("style.family.set", json!({"family": "Captions", "size": 133})).unwrap();
    assert_eq!(r["layers"], json!(2));
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    assert!((scale_x(&s, a) - 133.0).abs() < 1e-6);
    assert!((scale_x(&s, b) - 133.0).abs() < 1e-6);
    assert!((scale_x(&s, c) - 100.0).abs() < 1e-6, "not linked");
    // Relative: back to 100 % undoes the scale; the user's comment stays.
    s.execute("style.family.set", json!({"family": "Captions", "size": 100})).unwrap();
    assert!((scale_x(&s, a) - 100.0).abs() < 1e-6);
    let comment = s.active_comp().unwrap().layer(LayerId(a)).unwrap().comment.clone();
    assert!(comment.starts_with("hand-made"), "{comment}");
    assert!(comment.contains("[family: Captions @100%]"), "{comment}");
    let list = s.execute("style.family.list", json!({})).unwrap();
    assert_eq!(list["families"][0]["count"], json!(2));
    s.execute("style.family.unlink", json!({"layers": [a]})).unwrap();
    assert_eq!(s.active_comp().unwrap().layer(LayerId(a)).unwrap().comment, "hand-made");
    assert!(s.execute("style.family.set", json!({"family": "Nope", "size": 120})).is_err());
}

#[test]
fn save_text_preset_checks_and_selects_the_workshop_layer() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Creating Text Presets", "width": 200, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
    let t = s.execute("layer.newText", json!({"text": "TESTING"})).unwrap()["layer"].as_u64().unwrap();
    let solid = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    assert!(s.execute("ml.saveTextPreset", json!({"layer": t, "name": "Pop"})).is_err(), "not a preset name");
    assert!(s.execute("ml.saveTextPreset", json!({"layer": solid, "name": "T3b Pop · mid"})).is_err(), "not text");
    let r = s.execute("ml.saveTextPreset", json!({"layer": t, "name": "T3b Pop · mid"})).unwrap();
    assert_eq!(r["folder"], json!("Elias/Text animations"));
    assert_eq!(s.state.selected_layers, vec![LayerId(t)]);
    assert!(s.events.iter().any(|e| matches!(e, crate::Event::Frontend { command, .. } if command == "ml.saveTextPreset")));
}
