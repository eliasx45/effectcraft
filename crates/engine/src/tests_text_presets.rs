//! Text presets (batch 1b, DX1): text.presetFamilies / text.apply.
//! Not run yet (batch 1): TODO(batch1-verify) run `cargo test -p effectcraft-engine tests_text_presets`.

use effectcraft_project::{ItemId, LayerId};
use serde_json::{Value, json};

use crate::Session;
use crate::commands::text_presets::{Distance, parse_for_tests};

/// The workshop comp with "T3b Pop · mid" (position up 40 px over 0.5 s, Easy Ease; opacity
/// 0 → 100) and a 25 fps "Main" comp with a speech map ("works" stressed at 1.2 s) and a text
/// layer "Caption". Returns (session, workshop, main, caption layer).
fn studio() -> (Session, ItemId, ItemId, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Creating Text Presets", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let ws = s.active_comp_id().unwrap();
    let t = s.execute("layer.newText", json!({"text": "Pop", "name": "T3b Pop · mid"})).unwrap()["layer"].as_u64().unwrap();
    key(&mut s, t, "transform/position", 0.0, json!([160.0, 130.0, 0.0]));
    key(&mut s, t, "transform/position", 0.5, json!([160.0, 90.0, 0.0]));
    key(&mut s, t, "transform/opacity", 0.0, json!(0.0));
    key(&mut s, t, "transform/opacity", 0.2, json!(100.0));
    let pr = s.project.comp(ws).unwrap().layer(LayerId(t)).unwrap().props.prop("transform/position").unwrap().clone();
    let keys: Vec<Value> = pr.keys.iter().map(|k| json!({"layer": t, "prop": pr.uid, "time": k.time.seconds()})).collect();
    s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
    s.execute_checked("keys.easyEase", json!({})).unwrap();

    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let main = s.active_comp_id().unwrap();
    let voice = s.execute("layer.newSolid", json!({"name": "Voice", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    s.execute(
        "speech.import",
        json!({"layer": voice, "map": {"engine": "test", "words": [
            {"word": "this", "start": 0.4, "end": 0.6},
            {"word": "really", "start": 0.7, "end": 1.0},
            {"word": "WORKS", "start": 1.1, "end": 1.5, "syllables": [{"t": 1.1, "vowel": 1.2, "stress": 1}]}
        ]}}),
    )
    .unwrap();
    let cap = s.execute("layer.newText", json!({"text": "Caption", "name": "Caption"})).unwrap()["layer"].as_u64().unwrap();
    (s, ws, main, cap)
}

fn key(s: &mut Session, l: u64, path: &str, t: f64, v: Value) {
    s.execute("prop.addKey", json!({"layer": l, "path": path, "value": v, "time": t, "timeBase": "comp"})).unwrap();
}

fn keys(s: &Session, cid: ItemId, l: u64, path: &str) -> Vec<(f64, Vec<f64>)> {
    s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap().keys.iter().map(|k| (k.time.seconds(), k.value.components())).collect()
}

#[test]
fn names_read_family_variant_title_and_distance() {
    let n = parse_for_tests("T3b Pop · mid").unwrap();
    assert_eq!((n.family, n.variant, n.title.as_str(), n.distance), (3, Some('b'), "Pop", Some(Distance::Mid)));
    assert_eq!(n.short(), "T3b");
    let n = parse_for_tests("t12c Slide up - far").unwrap();
    assert_eq!((n.family, n.variant, n.title.as_str(), n.distance), (12, Some('c'), "Slide up", Some(Distance::Far)));
    let n = parse_for_tests("T1").unwrap();
    assert_eq!((n.family, n.variant, n.distance), (1, None, None));
    // The letter alone says the distance.
    assert_eq!(parse_for_tests("T2a Slide").unwrap().distance, Some(Distance::Near));
    for not in ["Text 1", "Title", "T3x2", "T", "T3bounce", ""] {
        assert!(parse_for_tests(not).is_none(), "{not}");
    }
}

#[test]
fn families_list_the_workshop_layers() {
    let (mut s, ws, _, _) = studio();
    let r = s.execute("text.presetFamilies", json!({})).unwrap();
    assert_eq!(r["workshop"], json!(ws.0));
    let fam = &r["families"][0];
    assert_eq!(fam["family"], "T3");
    assert_eq!(fam["name"], "Pop");
    assert_eq!(fam["variants"][0]["short"], "T3b");
    assert_eq!(fam["variants"][0]["distance"], "mid");
    assert_eq!(fam["variants"][0]["source"], "project");
}

#[test]
fn apply_moves_the_animation_onto_the_layer_relative_to_its_place_in_one_undo_step() {
    let (mut s, _, main, cap) = studio();
    let before = s.history.undo.len();
    let rest = s.project.comp(main).unwrap().layer(LayerId(cap)).unwrap().props.prop("transform/position").unwrap().value.components();
    let r = s.execute_checked("text.apply", json!({"preset": "T3b", "layers": [cap]})).unwrap();
    assert_eq!(r["preset"]["short"], "T3b");
    assert_eq!(s.history.undo.len(), before + 1, "one undo step: {r:#}");
    let k = keys(&s, main, cap, "transform/position");
    assert_eq!(k.len(), 2);
    // Starts at the layer's in point (0), settles on its own position, travels 40 px up.
    assert!(k[0].0.abs() < 1e-6 && (k[1].0 - 0.5).abs() < 1e-6, "{k:?}");
    assert!((k[1].1[1] - rest[1]).abs() < 1e-6 && (k[0].1[1] - (rest[1] + 40.0)).abs() < 1e-6, "{k:?} {rest:?}");
    assert_eq!(keys(&s, main, cap, "transform/opacity").len(), 2);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(keys(&s, main, cap, "transform/position").is_empty());
}

#[test]
fn distance_without_its_variant_scales_the_travel() {
    let (mut s, _, main, cap) = studio();
    let r = s.execute_checked("text.apply", json!({"preset": "T3", "layers": [cap], "distance": "far"})).unwrap();
    assert!((r["preset"]["travelScale"].as_f64().unwrap() - 1.6).abs() < 1e-9, "{r:#}");
    let k = keys(&s, main, cap, "transform/position");
    assert!(((k[0].1[1] - k[1].1[1]) - 64.0).abs() < 1e-6, "{k:?}");
    // Opacity stays absolute.
    let o = keys(&s, main, cap, "transform/opacity");
    assert_eq!((o[0].1[0], o[1].1[0]), (0.0, 100.0));
}

#[test]
fn sync_makes_a_layer_for_the_words_with_its_peak_on_the_stressed_syllable() {
    let (mut s, _, main, _) = studio();
    let r = s.execute_checked("text.apply", json!({"preset": "T3b", "words": "works", "sync": true})).unwrap();
    let layer = &r["layers"][0];
    assert_eq!(layer["created"], json!(true), "{r:#}");
    let lid = layer["layer"].as_u64().unwrap();
    // The accent: the vowel (1.2 s) − 1 frame = frame 29 at 25 fps.
    let pr = s.project.comp(main).unwrap().layer(LayerId(lid)).unwrap().props.prop("transform/position").unwrap().clone();
    let (peak, _) = crate::lint::segment_peak(&pr.keys, 0, pr.spatial).unwrap();
    assert!((peak * 25.0 - 29.0).abs() <= 0.6, "peak at frame {} ({r:#})", peak * 25.0);
    assert!(layer["sync"]["offsetFrames"].as_f64().unwrap().abs() <= 0.6);
    // Its text is the word as said, and it shows while the word is said.
    let l = s.project.comp(main).unwrap().layer(LayerId(lid)).unwrap().clone();
    assert_eq!(l.name, "WORKS");
    assert!(l.in_point.seconds() <= 1.1 + 1e-6 && l.out_point.seconds() >= 1.5, "{} {}", l.in_point.seconds(), l.out_point.seconds());
}

#[test]
fn words_as_a_speech_map_range_work_like_the_phrase() {
    let (mut s, _, _, _) = studio();
    let r = s.execute_checked("text.apply", json!({"preset": "T3b", "words": [1, 2], "word": "works", "sync": true, "distance": ""})).unwrap();
    assert_eq!(r["layers"][0]["name"], "really WORKS", "{r:#}");
    assert_eq!(r["layers"][0]["sync"]["word"], "WORKS");
}

#[test]
fn unknown_presets_and_bad_params_are_errors_not_crashes() {
    let (mut s, _, _, cap) = studio();
    let e = s.execute_checked("text.apply", json!({"preset": "T9", "layers": [cap]})).unwrap_err().to_string();
    assert!(e.contains("T3b"), "lists what there is: {e}");
    assert!(s.execute_checked("text.apply", json!({"preset": "T3b", "layers": [cap], "distance": "miles"})).is_err());
    assert!(s.execute_checked("text.apply", json!({"preset": "T3b", "words": [99, 120]})).is_err());
    assert!(s.execute_checked("text.apply", json!({"layers": [cap]})).is_err());
}
