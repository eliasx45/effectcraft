//! Batch 1 lane B: words follow their voice layer (SX1), `layer.words`, `speech.find` (SX4),
//! `speech.transcribe` with a words file (SX2). Not compiled yet (batch 1).

use effectcraft_project::LayerId;
use serde_json::{Value, json};

use crate::Session;

/// A 320×180 comp, 4 s at 25 fps, with a solid standing in for the voice layer (`speech.import`
/// maps media seconds through any layer) and a map of three words, "works" stressed at 1.2 s.
fn talk() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Talk", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Voice", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("speech.import", json!({"layer": l, "map": map()})).unwrap();
    (s, l)
}

fn map() -> Value {
    json!({"engine": "test", "words": [
        {"word": "this", "start": 0.4, "end": 0.6},
        {"word": "really", "start": 0.7, "end": 1.0},
        {"word": "WORKS", "start": 1.1, "end": 1.5, "syllables": [{"t": 1.1, "vowel": 1.2, "stress": 1}]}
    ]})
}

fn first_start(s: &Session) -> f64 {
    s.active_comp().unwrap().speech.as_ref().unwrap().words[0].start.seconds()
}

#[test]
fn words_move_with_their_layer_and_undo_back() {
    let (mut s, l) = talk();
    assert!((first_start(&s) - 0.4).abs() < 1e-6);
    // The Timeline's bar drag runs layer.timing {layers, delta} (seconds).
    s.execute("layer.timing", json!({"layers": [l], "delta": 1.0})).unwrap();
    assert!((first_start(&s) - 1.4).abs() < 1e-6, "the words moved with the layer");
    s.undo();
    assert!((first_start(&s) - 0.4).abs() < 1e-6);
}

#[test]
fn layer_words_lists_frames_and_stress() {
    let (mut s, l) = talk();
    let r = s.execute("layer.words", json!({"layer": l})).unwrap();
    assert_eq!(r["count"], 3);
    let works = &r["words"][2];
    assert_eq!(works["word"], "WORKS");
    assert_eq!(works["frames"], json!([28, 38]));
    // Vowel at 1.2 s = frame 30, one frame of lead: 29.
    assert_eq!(works["stress"], 29);
}

#[test]
fn trimming_hides_words_outside_the_layer() {
    let (mut s, l) = talk();
    s.edit("Trim", None, |proj, _| {
        let cid = proj.comps().next().map(|(id, _)| *id).unwrap();
        proj.comp_mut(cid).unwrap().layer_mut(LayerId(l)).unwrap().out_point = effectcraft_time::Tick::from_seconds_f64(1.05);
        Ok(())
    })
    .unwrap();
    let r = s.execute("layer.words", json!({"layer": l})).unwrap();
    assert_eq!(r["count"], 2, "WORKS is trimmed away");
    assert_eq!(s.active_comp().unwrap().speech.as_ref().unwrap().words.len(), 3, "but kept for trimming back");
}

#[test]
fn speech_find_moves_the_playhead_to_the_stressed_syllable() {
    let (mut s, _) = talk();
    let r = s.execute("speech.find", json!({"word": "works"})).unwrap();
    assert_eq!(r["frame"], 29);
    assert_eq!(r["stressed"], true);
    assert_eq!(s.active_comp().unwrap().frame_rate.frame_at(s.time()), 29);
    let phrase = s.execute("speech.find", json!({"word": "this really", "go": false})).unwrap();
    assert_eq!(phrase["index"], 0);
    assert!(s.execute("speech.find", json!({"word": "nope"})).is_err());
    assert!(s.execute("speech.find", json!({"word": "works", "occurrence": 2})).is_err());
}

#[test]
fn transcribe_needs_a_layer_with_sound() {
    let (mut s, l) = talk();
    // A solid has no sound.
    assert!(s.execute("speech.transcribe", json!({"layer": l})).is_err());
}
