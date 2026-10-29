//! `speech.appearPlan` and the `off-word` lint (A/B round 3: appear timing): each layer's first
//! frame against the spoken word it shows.

use effectcraft_project::LayerId;
use serde_json::{Value, json};

use crate::Session;

/// A 320×180 comp, 4 s at 25 fps, with a speech map: "it's" (0.2 s), "value" (1.0 s, vowel
/// 1.08 s: accent frame 26), "gold" (2.0 s).
fn talk() -> Session {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Talk", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let map = json!({"engine": "test", "words": [
        {"word": "it's", "start": 0.2, "end": 0.4, "syllables": [{"t": 0.2, "vowel": 0.24, "stress": 1}]},
        {"word": "value", "start": 1.0, "end": 1.5, "syllables": [{"t": 1.0, "vowel": 1.08, "stress": 1}, {"t": 1.3, "vowel": 1.34, "stress": 0}]},
        {"word": "gold", "start": 2.0, "end": 2.4, "syllables": [{"t": 2.0, "vowel": 2.08, "stress": 1}]}
    ]});
    s.execute("speech.import", json!({"map": map})).unwrap();
    s
}

fn text(s: &mut Session, t: &str) -> u64 {
    s.execute("layer.newText", json!({"text": t})).unwrap()["layer"].as_u64().unwrap()
}

/// A scale pop from 0 between `a` and `b` (comp seconds), Easy Eased: its speed peak is midway.
fn pop(s: &mut Session, l: u64, a: f64, b: f64) {
    s.execute("prop.addKey", json!({"layer": l, "path": "transform/scale", "value": [0.0, 0.0, 100.0], "time": a, "timeBase": "comp"})).unwrap();
    s.execute("prop.addKey", json!({"layer": l, "path": "transform/scale", "value": [100.0, 100.0, 100.0], "time": b, "timeBase": "comp"})).unwrap();
    let uid = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("transform/scale").unwrap().uid;
    let keys: Vec<Value> = [a, b].iter().map(|t| json!({"layer": l, "prop": uid, "time": t})).collect();
    s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
    s.execute_checked("keys.easyEase", json!({})).unwrap();
}

fn entry(r: &Value, l: u64) -> Value {
    r["layers"].as_array().unwrap().iter().find(|e| e["layer"] == l).cloned().unwrap_or_else(|| panic!("layer {l} not planned: {r:#}"))
}

fn off_word(s: &mut Session) -> Vec<Value> {
    let r = s.execute("render.lint", json!({"start": 0, "end": 4, "rules": ["off-word"]})).unwrap();
    r["problems"].as_array().unwrap().iter().filter(|p| p["rule"] == "off-word").cloned().collect()
}

#[test]
fn a_pop_that_peaks_before_its_word_is_off_word_and_align_peak_fixes_it() {
    let mut s = talk();
    let l = text(&mut s, "VALUE");
    pop(&mut s, l, 0.6, 1.0); // peak at 0.8 s = frame 20; the accent is frame 26
    let r = s.execute("speech.appearPlan", json!({})).unwrap();
    let e = entry(&r, l);
    assert_eq!((e["word"].as_str(), e["from"].as_str(), e["measured"].as_str()), (Some("value"), Some("text"), Some("peak")), "{e:#}");
    assert_eq!((e["wordFrame"].as_i64(), e["accentFrame"].as_i64(), e["appearFrame"].as_i64()), (Some(25), Some(26), Some(15)), "{e:#}");
    assert!((e["offBy"].as_f64().unwrap() + 6.0).abs() < 0.6, "6 frames early: {e:#}");
    assert_eq!(e["suggestedFrame"], 21, "{e:#}");
    assert_eq!(e["fix"]["command"], "keys.alignPeak", "{e:#}");
    assert_eq!(r["off"], 1);

    let found = off_word(&mut s);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0]["message"].as_str().unwrap().contains("\"value\""), "{found:#?}");

    let fix = &e["fix"];
    s.execute_checked(fix["command"].as_str().unwrap(), fix["params"].clone()).unwrap();
    let e = entry(&s.execute("speech.appearPlan", json!({})).unwrap(), l);
    assert!(e["offBy"].as_f64().unwrap().abs() <= 1.0, "on its word after the fix: {e:#}");
    assert!(off_word(&mut s).is_empty());
}

#[test]
fn a_layer_that_cuts_in_starts_on_its_word() {
    let mut s = talk();
    let l = text(&mut s, "Gold");
    s.execute("layer.timing", json!({"layers": [l], "in": 2.0})).unwrap();
    let e = entry(&s.execute("speech.appearPlan", json!({})).unwrap(), l);
    assert_eq!((e["measured"].as_str(), e["offBy"].as_f64()), (Some("appear"), Some(0.0)), "{e:#}");
    assert!(off_word(&mut s).is_empty());

    s.execute("layer.timing", json!({"layers": [l], "in": 2.4})).unwrap();
    let e = entry(&s.execute("speech.appearPlan", json!({})).unwrap(), l);
    assert_eq!(e["offBy"], 10.0, "{e:#}");
    assert_eq!(e["fix"]["command"], "layer.timing", "{e:#}");
    assert_eq!(off_word(&mut s).len(), 1);
}

#[test]
fn layers_can_say_which_word_they_show_and_unsaid_text_is_listed_unmatched() {
    let mut s = talk();
    let card = s.execute("layer.newSolid", json!({"name": "Card", "width": 40, "height": 40, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    let other = text(&mut s, "−5");
    let r = s.execute("speech.appearPlan", json!({"layers": [{"layer": card, "words": "gold"}, other]})).unwrap();
    let e = entry(&r, card);
    assert_eq!((e["word"].as_str(), e["from"].as_str(), e["sure"].as_bool()), (Some("gold"), Some("given"), Some(true)), "{e:#}");
    assert!(entry(&r, other).get("unmatched").is_some(), "{r:#}");
    assert_eq!(r["unmatched"], 1);
    // Unasked, the card (its name isn't said) isn't planned.
    let r = s.execute("speech.appearPlan", json!({})).unwrap();
    assert!(r["layers"].as_array().unwrap().iter().all(|e| e["layer"] != card), "{r:#}");
    // Without a speech map: a clear error.
    s.execute("speech.clear", json!({})).unwrap();
    assert!(s.execute("speech.appearPlan", json!({})).unwrap_err().to_string().contains("speech map"));
}

/// Probe for Elias's finished Tyson 20-08 comps (the A/B answer key, local only): his own appear
/// times should mostly pass. Prints every layer's plan; run with
/// `cargo test -p effectcraft-engine --lib tests_appear::his_20_08_comps -- --ignored --nocapture`.
#[test]
#[ignore = "needs the A/B answer key in plan/editing-harness/ab (local only)"]
fn his_20_08_comps_mostly_appear_on_their_words() {
    let ab = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plan/editing-harness/ab");
    let path = ab.join("src/20-08-finished-20261025.ecproj");
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    // Comps that had no map in his project: the same maps the A/B gave both arms (comp seconds).
    for c in [300490, 301104, 303626] {
        let map = ab.join(format!("tasks/speech/{c}.json"));
        if map.exists() && s.execute("speech.get", json!({"comp": c})).is_err() {
            s.execute("speech.import", json!({"comp": c, "path": map})).unwrap();
        }
    }
    let (mut sure, mut on) = (0, 0);
    for c in [300306, 300333, 301104, 300490, 313971, 303626, 309234, 305155] {
        let r = match s.execute("speech.appearPlan", json!({"comp": c})) {
            Ok(r) => r,
            Err(e) => {
                println!("== {c}: {e}");
                continue;
            }
        };
        println!("== {c}: {} layers, off {}, unmatched {}", r["layers"].as_array().map_or(0, Vec::len), r["off"], r["unmatched"]);
        for e in r["layers"].as_array().into_iter().flatten() {
            match e.get("word") {
                None => println!("   ---  {:30} {:24} {}", e["name"], e["shows"], e["unmatched"]),
                Some(w) => {
                    let off = e["offBy"].as_f64().unwrap_or(0.0);
                    if e["sure"] == true {
                        sure += 1;
                        on += usize::from(off.abs() <= 3.0);
                    }
                    println!(
                        "   {} {:30} {:24} → {w} (f{}, accent f{}) appear f{} peak {} {} {off:+}",
                        if off.abs() > 3.0 { "OFF" } else { "ok " },
                        e["name"],
                        e["shows"],
                        e["wordFrame"],
                        e["accentFrame"],
                        e["appearFrame"],
                        e["entrance"]["peakFrame"],
                        e["measured"]
                    );
                }
            }
        }
    }
    println!("his layers on their word: {on} of {sure}");
    assert!(
        sure > 0 && on * 10 >= sure * 7,
        "most of his own layers should pass: {on} of {sure} (read the OFF lines: a pattern of his, like beat-staggered captions, or a matching problem)"
    );
}

/// 29 Oct 2026, Tapa 26-10 "full-time pill 17 f late": a pill showing "Full-time income" inside a
/// scene precomp peaked on IN-come, 17 frames after "full". `off-word` flagged it in the scene
/// comp but not in the part / MAIN lint the handover ran. It now looks into precomps that carry
/// their own speech map and reports the time in the comp linted.
#[test]
fn off_word_looks_inside_a_scene_precomp_with_its_own_speech_map() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Scene", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let scene = s.active_comp_id().unwrap();
    let words = |off: f64| {
        json!({"engine": "test", "words": [
            {"word": "full", "start": 1.0 + off, "end": 1.3 + off, "syllables": [{"t": 1.0 + off, "vowel": 1.08 + off, "stress": 1}]},
            {"word": "time", "start": 1.3 + off, "end": 1.6 + off, "syllables": [{"t": 1.3 + off, "vowel": 1.36 + off, "stress": 1}]},
            {"word": "income", "start": 1.68 + off, "end": 2.1 + off, "syllables": [{"t": 1.68 + off, "vowel": 1.72 + off, "stress": 1}]}
        ]})
    };
    s.execute("speech.import", json!({"comp": scene.0, "map": words(0.0)})).unwrap();
    let l = text(&mut s, "Full-time income");
    pop(&mut s, l, 1.48, 1.88); // peak 1.68 s (frame 42) on IN-come; "full" accent = frame 26
    // In the scene comp itself it was already flagged.
    assert_eq!(off_word(&mut s).len(), 1);

    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 25, "duration": 8.0})).unwrap();
    let main = s.active_comp_id().unwrap();
    let pre = s.execute("layer.addItem", json!({"comp": main.0, "item": scene.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    {
        let p = std::sync::Arc::make_mut(&mut s.project);
        let nl = p.comp_mut(main).unwrap().layer_mut(LayerId(pre)).unwrap();
        nl.start_time = effectcraft_time::Tick::from_seconds_f64(2.0);
        nl.in_point = effectcraft_time::Tick::from_seconds_f64(2.0);
        nl.out_point = effectcraft_time::Tick::from_seconds_f64(6.0);
    }
    s.execute("speech.import", json!({"comp": main.0, "map": words(2.0)})).unwrap();
    let r = s.execute("render.lint", json!({"comp": main.0, "start": 0, "end": 8, "rules": ["off-word"]})).unwrap();
    let found: Vec<Value> = r["problems"].as_array().unwrap().iter().filter(|p| p["rule"] == "off-word").cloned().collect();
    assert_eq!(found.len(), 1, "{r:#}");
    let f = &found[0];
    assert_eq!(f["layer"], pre, "reported on the precomp layer in Main: {f:#}");
    let msg = f["message"].as_str().unwrap();
    assert!(msg.contains("\"full\"") && msg.contains("in precomp \"Scene\""), "{msg}");
    assert!((f["time"].as_f64().unwrap() - 3.68).abs() < 0.03, "Main seconds (2 + 1.68): {f:#}");
    assert_eq!(f["fix"]["params"]["comp"], scene.0, "the fix runs in the scene comp: {f:#}");

    // Its fix lands it on "full", and Main's lint is clean.
    let fix = f["fix"].clone();
    s.execute_checked(fix["command"].as_str().unwrap(), fix["params"].clone()).unwrap();
    let r = s.execute("render.lint", json!({"comp": main.0, "start": 0, "end": 8, "rules": ["off-word"]})).unwrap();
    assert!(r["problems"].as_array().unwrap().iter().all(|p| p["rule"] != "off-word"), "{r:#}");
}
