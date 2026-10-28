//! Batch 1 lane B (SX5): `script.load`, `script.get`, `script.copyRef`, `script.board`,
//! `word.apply`. Not compiled yet (batch 1).

use serde_json::json;

use crate::Session;

/// A comp with a solid as the voice layer and the words "this one really WORKS every single time".
fn talk() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "10-10 MAIN", "width": 320, "height": 180, "frameRate": 25, "duration": 6.0})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Voice", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    let words: Vec<serde_json::Value> = ["this", "one", "really", "WORKS", "every", "single", "time"]
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let a = 1.0 + i as f64 * 0.4;
            if *w == "WORKS" {
                json!({"word": w, "start": a, "end": a + 0.35, "syllables": [{"t": a, "vowel": a + 0.08, "stress": 1}]})
            } else {
                // An unstressed syllable each: a word without syllables in a map that has some gets
                // estimated ones with the first stressed (HS2), which would stress every word.
                json!({"word": w, "start": a, "end": a + 0.35, "syllables": [{"t": a, "vowel": a + 0.05, "stress": 0}]})
            }
        })
        .collect();
    s.execute("speech.import", json!({"layer": l, "map": {"engine": "test", "words": words}})).unwrap();
    (s, l)
}

#[test]
fn a_loaded_script_lines_up_and_survives_a_new_transcript() {
    let (mut s, l) = talk();
    s.execute("script.load", json!({"text": "But this one works every single time.\nHere's what these pages have in common.", "name": "10-10.txt"})).unwrap();
    let g = s.execute("script.get", json!({})).unwrap();
    assert_eq!(g["lines"][0]["notSaid"], 1);
    assert_eq!(g["lines"][0]["adlibs"], 1);
    assert_eq!(g["lines"][1]["cut"], true);
    assert_eq!(g["lines"][0]["firstWord"], 0);
    // A new transcript keeps the script.
    s.execute("speech.import", json!({"layer": l, "map": {"words": [{"word": "this", "start": 1.0, "end": 1.2}]}})).unwrap();
    assert!(s.execute("script.get", json!({})).is_ok());
}

#[test]
fn copy_ref_names_the_comp_line_frames_and_words() {
    let (mut s, _) = talk();
    s.execute("script.load", json!({"text": "But this one works every single time."})).unwrap();
    let r = s.execute("script.copyRef", json!({"from": 2, "to": 6})).unwrap();
    let text = r["text"].as_str().unwrap();
    assert!(text.starts_with("10-10 MAIN · script line 1"), "{text}");
    assert!(text.contains("really WORKS every single time"), "{text}");
    assert!(text.contains("f45"), "{text}"); // "really" starts at 1.8 s
}

#[test]
fn word_apply_pop_puts_its_peak_on_the_stressed_syllable_in_one_undo_step() {
    let (mut s, _) = talk();
    let before = s.active_comp().unwrap().layers.len();
    let r = s.execute("word.apply", json!({"index": 3, "item": "pop"})).unwrap();
    assert_eq!(r["onStress"], true);
    assert_eq!(s.active_comp().unwrap().layers.len(), before + 1);
    // The pop's speed peak, as speech.accents measures it, lands on WORKS's accent.
    let pop = r["layer"].as_u64().unwrap();
    let acc = s.execute("speech.accents", json!({"layer": pop})).unwrap();
    let list = acc["list"].as_array().unwrap();
    let m = list.iter().find(|m| m["word"] == "WORKS").unwrap_or_else(|| panic!("{acc:#}"));
    assert_eq!(m["hit"], true, "{m:#}");
    assert!((m["frame"].as_f64().unwrap() - r["peakFrame"].as_f64().unwrap()).abs() <= 1.0, "{m:#} {r:#}");
    s.undo();
    assert_eq!(s.active_comp().unwrap().layers.len(), before, "one undo step");
}

#[test]
fn word_apply_rejects_unknown_items_and_zoom_without_layers() {
    let (mut s, _) = talk();
    assert!(s.execute("word.apply", json!({"index": 3, "item": "ask"})).is_err());
    s.execute("layer.select", json!({"layers": []})).unwrap();
    assert!(s.execute("word.apply", json!({"index": 3, "item": "zoom"})).is_err());
}

#[test]
fn board_splits_a_line_around_its_strongest_word() {
    let (mut s, _) = talk();
    s.execute("script.load", json!({"text": "this one really works every single time"})).unwrap();
    let b = s.execute("script.board", json!({"lines": [1], "send": false})).unwrap();
    assert!(b["frames"].as_array().unwrap().len() >= 2);
}

#[test]
fn gaps_stay_inside_the_comp() {
    // BUILD2 SX5: words after the comp's end made a gap past it (12 s..573 s in a 12 s comp).
    let (mut s, l) = talk();
    let words = json!([{"word": "this", "start": 1.0, "end": 1.3}, {"word": "later", "start": 5.0, "end": 5.4}]);
    s.execute("speech.import", json!({"layer": l, "map": {"words": words}})).unwrap();
    s.execute("script.load", json!({"text": "this later", "name": "t.txt"})).unwrap();
    s.execute("comp.settings", json!({"duration": 3.0})).unwrap();
    let g = s.execute("script.get", json!({"gapSeconds": 1.0})).unwrap();
    let gaps = g["gaps"].as_array().unwrap();
    assert!(!gaps.is_empty(), "{g}");
    for gap in gaps {
        assert!(gap["end"].as_f64().unwrap() <= 3.0, "{gap}");
    }
}
