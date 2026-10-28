//! SX7: `script.get` fills itself from the speech map: lines cut on pauses and a word cap,
//! chapters from the comp markers, deep scene paths through nested precomps, gaps.

use serde_json::{Value, json};

use crate::Session;
use crate::script_lines::speech_lines;
use effectcraft_project::speech::SpeechWord;
use effectcraft_time::Tick;

fn word(w: &str, a: f64, b: f64) -> SpeechWord {
    SpeechWord { word: w.into(), start: Tick::from_seconds_f64(a), end: Tick::from_seconds_f64(b), ..Default::default() }
}

#[test]
fn speech_lines_split_on_pauses_and_after_eleven_words() {
    // 16 words, 0.25 s apart; a 0.5 s pause after the 4th.
    let mut ws = vec![];
    let mut t = 0.0;
    for i in 0..16 {
        ws.push(word(&format!("w{i}"), t, t + 0.2));
        t += if i == 3 { 0.7 } else { 0.25 };
    }
    assert_eq!(speech_lines(&ws, 0.32, 11), vec![(0, 3), (4, 14), (15, 15)]);
    // A pause just under the threshold doesn't split.
    assert_eq!(speech_lines(&ws, 0.6, 100), vec![(0, 15)]);
    assert!(speech_lines(&[], 0.32, 11).is_empty());
    // max_words 0 is treated as 1, not an endless line or a panic.
    assert_eq!(speech_lines(&ws[..2], 0.32, 0), vec![(0, 0), (1, 1)]);
}

/// MAIN (6 s, 25 fps) › "26-10 P1" › "S1 01" (0–2 s) and "S1 02" (3–5 s), a voice solid, chapter
/// markers HOOK at 0 and IDEA at 3 s, and three spoken lines: 0.5–1.2 s, 2.2–2.8 s (between the
/// scenes), 3.5–4.4 s.
fn nested() -> Session {
    let mut s = Session::default();
    let new_comp = |s: &mut Session, name: &str| {
        s.execute("comp.new", json!({"name": name, "width": 320, "height": 180, "frameRate": 25, "duration": 6.0})).unwrap();
    };
    let solid = |s: &mut Session, name: &str| -> u64 {
        s.execute("layer.newSolid", json!({"name": name, "width": 50, "height": 50, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap()
    };
    let add = |s: &mut Session, item: &str, a: f64, b: f64| -> u64 {
        let l = s.execute("layer.addItem", json!({"item": item, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.timing", json!({"layers": [l], "in": a, "out": b})).unwrap();
        l
    };
    new_comp(&mut s, "S1 01");
    solid(&mut s, "Card A");
    new_comp(&mut s, "S1 02");
    solid(&mut s, "Card B");
    new_comp(&mut s, "26-10 P1");
    add(&mut s, "S1 01", 0.0, 2.0);
    add(&mut s, "S1 02", 3.0, 5.0);
    new_comp(&mut s, "MAIN");
    let voice = solid(&mut s, "Voice");
    add(&mut s, "26-10 P1", 0.0, 6.0);
    s.execute("comp.addMarker", json!({"time": 0.0, "comment": "HOOK"})).unwrap();
    s.execute("comp.addMarker", json!({"time": 3.0, "comment": "IDEA · TikTokers got paid"})).unwrap();
    let words = json!([
        {"word": "this", "start": 0.5, "end": 0.8, "syllables": [{"t": 0.5, "vowel": 0.55, "stress": 0}]},
        {"word": "one", "start": 0.85, "end": 1.2, "syllables": [{"t": 0.85, "vowel": 0.9, "stress": 1}]},
        {"word": "huge", "start": 2.2, "end": 2.5},
        {"word": "problem", "start": 2.55, "end": 2.8},
        {"word": "every", "start": 3.5, "end": 3.9},
        {"word": "day", "start": 3.95, "end": 4.4},
    ]);
    s.execute("speech.import", json!({"layer": voice, "map": {"engine": "test", "words": words}})).unwrap();
    s
}

fn path(line: &Value) -> Vec<String> {
    line["scenePath"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[test]
fn script_get_builds_lines_from_the_speech_map_without_a_script() {
    let mut s = nested();
    let g = s.execute("script.get", json!({})).unwrap();
    assert_eq!(g["source"], "speech", "{g:#}");
    let lines = g["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3, "{g:#}");
    assert_eq!(lines[0]["text"], "this one");
    assert_eq!(lines[0]["firstWord"], 0);
    assert_eq!(lines[2]["lastWord"], 5);
    // Words carry their time and stress.
    let w = &lines[0]["words"][1];
    assert_eq!(w["index"], 1, "{w}");
    assert_eq!(w["stressed"], true, "{w}");
    assert!(w["stress"].is_i64(), "{w}");
    assert_eq!(lines[0]["words"][0]["stressed"], false);
    // Chapters are the comp markers.
    assert_eq!(g["chapters"].as_array().unwrap().len(), 2, "{g:#}");
    assert_eq!(lines[0]["chapter"], "HOOK");
    assert_eq!(lines[2]["chapter"], "IDEA · TikTokers got paid");
    // summary drops words and tokens.
    let g = s.execute("script.get", json!({"summary": true})).unwrap();
    assert!(g["lines"][0].get("words").is_none(), "{g:#}");
}

#[test]
fn script_get_finds_deep_scenes_and_gaps_through_nested_precomps() {
    let mut s = nested();
    let g = s.execute("script.get", json!({})).unwrap();
    let lines = g["lines"].as_array().unwrap();
    assert_eq!(path(&lines[0]), ["26-10 P1", "S1 01"], "{:#}", lines[0]);
    assert_eq!(lines[0]["gap"], false);
    // Between S1 01 and S1 02 only the part plays: a gap, and where it is.
    assert_eq!(lines[1]["gap"], true, "{:#}", lines[1]);
    assert!((lines[1]["gapSeconds"].as_f64().unwrap() - 0.6).abs() < 1e-6, "{:#}", lines[1]);
    assert!(lines[1]["scene"].is_null());
    assert_eq!(lines[1]["bare"]["path"], json!(["26-10 P1"]), "{:#}", lines[1]);
    assert_eq!(path(&lines[2]), ["26-10 P1", "S1 02"], "{:#}", lines[2]);
    // The scene's layers name the comp each one is in, for layer.select after comp.open.
    let layers = lines[2]["scene"]["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[1]["name"], "S1 02");
    let gaps = g["sceneGaps"].as_array().unwrap();
    assert_eq!(gaps.len(), 1, "{g:#}");
    assert_eq!(gaps[0]["line"], 2);
}

#[test]
fn script_get_without_words_offers_to_transcribe() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "MAIN", "width": 320, "height": 180, "frameRate": 25, "duration": 2.0})).unwrap();
    let g = s.execute("script.get", json!({})).unwrap();
    assert_eq!(g["source"], "none", "{g:#}");
    assert_eq!(g["needsTranscribe"], true);
    assert!(g["lines"].as_array().unwrap().is_empty());
}

#[test]
fn a_loaded_script_gets_chapters_and_scenes_too() {
    let mut s = nested();
    s.execute("script.load", json!({"text": "this one\nhuge problem\nevery day", "name": "t.txt"})).unwrap();
    let g = s.execute("script.get", json!({})).unwrap();
    assert_eq!(g["source"], "script", "{g:#}");
    assert_eq!(g["lines"][1]["gap"], true, "{g:#}");
    assert_eq!(path(&g["lines"][2]), ["26-10 P1", "S1 02"]);
    // source: speech ignores the script.
    let g = s.execute("script.get", json!({"source": "speech"})).unwrap();
    assert_eq!(g["source"], "speech");
}
