//! Animation presets carry whole text animators (CX8d): anim.savePreset → anim.applyPreset on a
//! fresh text layer gives the same animators (names, selectors, Based On, expressions), with the
//! keys from the playhead; applying again replaces them; a non-text layer skips them with a note.
//! Not run yet (batch 1): TODO(batch1-verify) run `cargo test -p effectcraft-engine tests_preset_animators`.

use effectcraft_keyframe::{Keyframe, Value as KV};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Expression, LayerId, PropGroup};
use effectcraft_time::Tick;
use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("ec-preset-animators-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A "Fade Up" animator (Range Selector Based On Words, Ramp Up, Offset keyed 1 → 3 s eased,
/// Amount expression) and a "Bounce" animator (an Expression Selector only, Based On
/// Characters), pushed onto text layer `l` of the active comp.
fn add_animators(s: &mut Session, l: u64) {
    let cid = s.active_comp_id().unwrap();
    s.edit("fixture", None, |proj, _| {
        let mut next = proj.next_id;
        let mut ids = Ids(&mut next);
        let mut opacity = build::text_anim_props(&mut ids, "opacity", false);
        if let Some(o) = opacity.first_mut() {
            o.value = KV::Scalar(0.0);
        }
        let mut fade = build::text_animator(&mut ids, "Fade Up", opacity);
        let rs = fade.group_mut("selectors/rangeSelector").unwrap();
        rs.prop_mut("advanced/basedOn").unwrap().value = KV::Enum(2);
        rs.prop_mut("advanced/shape").unwrap().value = KV::Enum(1);
        rs.prop_mut("advanced/amount").unwrap().expr = Some(Expression { text: "effect(\"Animate By\")(\"Menu\")==2?100:0".into(), enabled: true });
        rs.prop_mut("offset").unwrap().keys =
            vec![Keyframe::new(Tick::from_seconds_f64(1.0), KV::Scalar(-100.0)).eased(), Keyframe::new(Tick::from_seconds_f64(3.0), KV::Scalar(100.0)).eased()];
        let pos = build::text_anim_props(&mut ids, "position", false);
        let mut bounce = build::text_animator(&mut ids, "Bounce", pos);
        let sels = bounce.sub_mut("selectors").unwrap();
        sels.children.clear();
        let mut es = build::expression_selector(&mut ids, "Expression Selector 1");
        es.prop_mut("basedOn").unwrap().value = KV::Enum(0);
        es.prop_mut("amount").unwrap().expr =
            Some(Expression { text: "var o=text.animator(\"Fade Up\").selector(\"Range Selector 1\").offset; [o.numKeys,0,0]".into(), enabled: true });
        sels.children.push(es.into());
        proj.next_id = next;
        let layer = crate::commands::layer_mut(proj, cid, LayerId(l))?;
        let anims = layer.props.group_mut("text/animators").unwrap();
        anims.children.push(fade.into());
        anims.children.push(bounce.into());
        Ok(())
    })
    .unwrap();
}

/// The animators of layer `l` without uids, for comparing.
fn animators(s: &Session, l: u64) -> Vec<PropGroup> {
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    layer
        .props
        .group("text/animators")
        .unwrap()
        .groups()
        .cloned()
        .map(|mut g| {
            let mut zero = 0;
            g.reassign_uids(&mut zero);
            g
        })
        .collect()
}

fn shifted(mut gs: Vec<PropGroup>, by: f64) -> Vec<PropGroup> {
    for g in &mut gs {
        g.walk_mut(&mut |pr| {
            for k in &mut pr.keys {
                k.time += Tick::from_seconds_f64(by);
            }
        });
    }
    gs
}

#[test]
fn text_animators_round_trip_through_a_preset_from_the_playhead() {
    let path = temp_dir("roundtrip").join("Slide In.ecpreset").to_string_lossy().to_string();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Workshop", "width": 320, "height": 180, "frameRate": 25, "duration": 6.0})).unwrap();
    let a = s.execute("layer.newText", json!({"text": "Slide in"})).unwrap()["layer"].as_u64().unwrap();
    add_animators(&mut s, a);
    // The Animators group selected: both animators, whole.
    let group = s.active_comp().unwrap().layer(LayerId(a)).unwrap().props.group("text/animators").unwrap().uid;
    s.state.selected_props = vec![(LayerId(a), group)];
    let r = s.execute("anim.savePreset", json!({"path": path})).unwrap();
    assert_eq!(r["animators"], json!(2), "{r:#}");
    assert_eq!(r["properties"], json!(0), "the animators' properties travel with them, not as loose paths: {r:#}");
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc[crate::commands::animation::PRESET_TAG], json!(crate::commands::animation::PRESET_VERSION));

    // A fresh text layer, the playhead at 2 s: the earliest key (1 s in the source) lands there.
    let b = s.execute("layer.newText", json!({"text": "Fresh"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("time.set", json!({"time": 2.0})).unwrap();
    let r = s.execute("anim.applyPreset", json!({"path": path, "layers": [b]})).unwrap();
    assert_eq!(r["skipped"], json!([]), "{r:#}");
    let want = shifted(animators(&s, a), 1.0);
    let got = animators(&s, b);
    // TODO(batch1-verify): PropGroup survives the serde round trip field for field (nothing skipped on save).
    assert_eq!(got, want, "same names, selectors, Based On, expressions and eases; keys from the playhead");
    let names: Vec<&str> = got.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["Fade Up", "Bounce"]);
    let offset = got[0].prop("selectors/rangeSelector/offset").unwrap();
    assert!((offset.keys[0].time.seconds() - 2.0).abs() < 1e-6 && (offset.keys[1].time.seconds() - 4.0).abs() < 1e-6);
    assert_eq!(got[0].prop("selectors/rangeSelector/advanced/basedOn").unwrap().value, KV::Enum(2));

    // Again: the same-named animators are replaced, not stacked.
    s.execute("anim.applyPreset", json!({"path": path, "layers": [b]})).unwrap();
    assert_eq!(animators(&s, b).len(), 2);
}

#[test]
fn a_layer_without_text_skips_the_animators_with_a_note() {
    let path = temp_dir("solid").join("Fade.ecpreset").to_string_lossy().to_string();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Workshop", "width": 320, "height": 180, "frameRate": 25, "duration": 6.0})).unwrap();
    let a = s.execute("layer.newText", json!({"text": "Fade"})).unwrap()["layer"].as_u64().unwrap();
    add_animators(&mut s, a);
    let first = s.active_comp().unwrap().layer(LayerId(a)).unwrap().props.group("text/animators").unwrap().groups().next().unwrap().uid;
    s.state.selected_props = vec![(LayerId(a), first)];
    assert_eq!(s.execute("anim.savePreset", json!({"path": path})).unwrap()["animators"], json!(1));
    let solid = s.execute("layer.newSolid", json!({"name": "S", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    let r = s.execute("anim.applyPreset", json!({"path": path, "layers": [solid]})).unwrap();
    assert!(r["skipped"].as_array().unwrap().iter().any(|v| v.as_str().is_some_and(|t| t.contains("not a text layer"))), "{r:#}");
}

#[test]
fn version_1_presets_still_apply() {
    let path = temp_dir("v1").join("old.ecpreset").to_string_lossy().to_string();
    std::fs::write(&path, json!({"effectcraftAnimationPreset": 1, "name": "old", "properties": [], "effects": []}).to_string()).unwrap();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "C", "width": 64, "height": 64, "frameRate": 25, "duration": 2.0})).unwrap();
    let a = s.execute("layer.newText", json!({"text": "x"})).unwrap()["layer"].as_u64().unwrap();
    assert!(s.execute("anim.applyPreset", json!({"path": path, "layers": [a]})).is_ok());
}
