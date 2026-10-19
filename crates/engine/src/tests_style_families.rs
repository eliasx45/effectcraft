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
fn save_text_preset_checks_and_selects_the_layers_effects_and_animators() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Creating Text Presets", "width": 200, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
    let cid = s.active_comp_id().unwrap();
    let t = s.execute("layer.newText", json!({"text": "TESTING"})).unwrap()["layer"].as_u64().unwrap();
    let solid = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    assert!(s.execute("ml.saveTextPreset", json!({"layer": t, "name": "Slide In"})).is_err(), "nothing to save yet");
    crate::tests_text_presets::add_control(&mut s, cid, t, "ec.control.slider", "Travel Distance (px)", KV::Scalar(150.0), &[]);
    crate::tests_text_presets::add_fade(&mut s, cid, t, "Fade Up", 2, true, "100");
    assert!(s.execute("ml.saveTextPreset", json!({"layer": t, "name": "a/b"})).is_err(), "not a card name");
    assert!(s.execute("ml.saveTextPreset", json!({"layer": solid, "name": "Slide In"})).is_err(), "not text");
    let r = s.execute("ml.saveTextPreset", json!({"layer": t, "name": "Slide In"})).unwrap();
    assert_eq!(r["folder"], json!("Elias/Text animations"));
    assert_eq!((r["effects"].as_u64(), r["animators"].as_u64()), (Some(1), Some(1)), "{r:#}");
    assert_eq!(s.state.selected_layers, vec![LayerId(t)]);
    assert_eq!(s.state.selected_props.len(), 2, "the effect and the animator, for anim.savePreset");
    assert!(s.events.iter().any(|e| matches!(e, crate::Event::Frontend { command, params } if command == "ml.saveTextPreset" && params["kind"] == "preset")));
}

/// CX9: Bounce on a Range Selector's Offset keys adds a per-unit bounce animator (Expression
/// Selector), once; the second run updates it.
#[test]
fn unit_bounce_follows_the_reveal() {
    use effectcraft_keyframe::Keyframe;
    use effectcraft_time::Tick;
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "T", "width": 400, "height": 200, "frameRate": 30, "duration": 3})).unwrap();
    let l = s.execute("layer.newText", json!({"text": "Bounce"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.addTextAnimator", json!({"layer": l, "properties": ["opacity"]})).unwrap();
    // Offset 0 → 100 over 0.5..1.5 s (the reveal).
    let cid = s.active_comp_id().unwrap();
    let off = s
        .edit("keys", None, |proj, _| {
            let layer = crate::commands::layer_mut(proj, cid, LayerId(l))?;
            let pr = layer.props.prop_mut("text/animators/animator/selectors/rangeSelector/offset").unwrap();
            pr.keys = vec![Keyframe::new(Tick::from_seconds_f64(0.5), KV::Scalar(0.0)), Keyframe::new(Tick::from_seconds_f64(1.5), KV::Scalar(100.0))];
            Ok(pr.uid)
        })
        .unwrap();
    // No keyed reveal: a clear error.
    assert!(s.execute("text.unitBounce", json!({"layer": l, "selector": 1})).is_err());
    let r = s.execute("text.unitBounce", json!({"layer": l, "selector": off, "strength": 50, "frequency": 3, "decay": 6})).unwrap();
    assert_eq!(r["reused"], json!(false));
    assert!(r["message"].as_str().unwrap().contains("Expression Selector"));
    let comp = s.active_comp().unwrap();
    let layer = comp.layer(LayerId(l)).unwrap();
    let anims = layer.props.group("text/animators").unwrap();
    assert_eq!(anims.groups().count(), 2);
    let ub = anims.groups().find(|g| g.name == "Unit bounce").unwrap();
    let amount = ub.group("selectors/expressionSelector").unwrap().get("amount").unwrap();
    let text = &amount.expr.as_ref().unwrap().text;
    assert!(text.contains("t0 = 0.5000") && text.contains("t1 = 1.5000") && text.contains("Math.cos"));
    assert!(ub.group("selectors").unwrap().sub("rangeSelector").is_none(), "no 0–100 % range selector of its own");
    assert_eq!(ub.prop("properties/position").unwrap().value, KV::Vec3([0.0, -20.0, 0.0]));
    // Again with bounce + scale: the same animator, updated, one undo step each.
    let undo = s.history.undo.len();
    let r = s.execute("text.unitBounce", json!({"layer": l, "selector": off, "mode": "bounce", "moves": ["position", "scale"]})).unwrap();
    assert_eq!(r["reused"], json!(true));
    assert_eq!(s.history.undo.len(), undo + 1);
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    let anims = layer.props.group("text/animators").unwrap();
    assert_eq!(anims.groups().count(), 2);
    let ub = anims.groups().find(|g| g.name == "Unit bounce").unwrap();
    assert!(ub.prop("properties/scale").is_some());
    assert!(ub.group("selectors/expressionSelector").unwrap().get("amount").unwrap().expr.as_ref().unwrap().text.contains("Math.abs"));
}
