//! `render.lint`: each design rule on a tiny synthetic comp, a clean comp, merging occurrences
//! over a range, sampling, and the beat matching behind `off-beat`.

use effectcraft_project::LayerId;
use serde_json::{Value, json};

use crate::Session;
use crate::lint::{self, KeyMoment, Severity};

/// A 320×180 comp, black background, `dur` seconds at 25 fps.
fn comp(dur: f64) -> Session {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Lint", "width": 320, "height": 180, "frameRate": 25, "duration": dur})).unwrap();
    s
}

fn solid(s: &mut Session, name: &str, w: u32, h: u32, color: &str) -> u64 {
    s.execute("layer.newSolid", json!({"name": name, "width": w, "height": h, "color": color})).unwrap()["layer"].as_u64().unwrap()
}

fn text(s: &mut Session, name: &str, t: &str, size: f64, fill: &str, pos: [f64; 2]) -> u64 {
    s.execute("layer.newText", json!({"name": name, "text": t, "size": size, "fill": fill, "position": pos})).unwrap()["layer"].as_u64().unwrap()
}

fn set(s: &mut Session, layer: u64, path: &str, value: Value) {
    s.execute("prop.set", json!({"layer": layer, "path": path, "value": value})).unwrap();
}

fn lint(s: &mut Session, p: Value) -> Value {
    s.execute("render.lint", p).unwrap()
}

/// The problems of `rule`.
fn of(r: &Value, rule: &str) -> Vec<Value> {
    r["problems"].as_array().unwrap().iter().filter(|p| p["rule"] == rule).cloned().collect()
}

/// The one problem of `rule` (fails when there are none or several).
fn one(r: &Value, rule: &str) -> Value {
    let v = of(r, rule);
    assert_eq!(v.len(), 1, "{rule}: {r:#}");
    v[0].clone()
}

#[test]
fn clean_comp_has_no_problems() {
    let mut s = comp(2.0);
    solid(&mut s, "BG", 320, 180, "#102040");
    text(&mut s, "Title", "Hello", 24.0, "#ffffff", [160.0, 90.0]);
    let r = lint(&mut s, json!({"time": 0.5}));
    assert_eq!(r["problems"], json!([]), "{r:#}");
    assert_eq!(r["clean"], true);
    assert_eq!(r["counts"], json!({"error": 0, "warning": 0, "info": 0}));
    assert_eq!(r["compName"], "Lint");
    assert_eq!(r["times"], json!([0.5]));
    assert_eq!(r["truncated"], false);
    assert!(r["notes"][0].as_str().unwrap().contains("no audible layers"), "{r}");
}

#[test]
fn text_too_small_warns_then_errors() {
    let mut s = comp(2.0);
    // 180 px tall: the minimum is 6 px.
    let t = text(&mut s, "Small", "Hello", 5.0, "#ffffff", [160.0, 90.0]);
    let r = lint(&mut s, json!({"time": 0, "rules": ["text-too-small"]}));
    let p = one(&r, "text-too-small");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64(), p["time"].as_f64()), (Some("warning"), Some(t), Some(0.0)), "{p}");
    assert_eq!(p["layerName"], "Small");
    assert!(p["message"].as_str().unwrap().contains("5.0 px"), "{p}");
    assert_eq!(r["clean"], false);
    // Under half the minimum: an error. The layer's scale counts (font size 20 at 10 % = 2 px).
    s.execute("layer.setText", json!({"layer": t, "size": 20})).unwrap();
    set(&mut s, t, "transform/scale", json!([10.0, 10.0, 100.0]));
    let p = one(&lint(&mut s, json!({"time": 0, "rules": ["text-too-small"]})), "text-too-small");
    assert_eq!(p["severity"], "error", "{p}");
    assert!(p["message"].as_str().unwrap().contains("font size 20"), "{p}");
    // minTextPx overrides the threshold.
    set(&mut s, t, "transform/scale", json!([100.0, 100.0, 100.0]));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["text-too-small"]})), "text-too-small").is_empty());
    let p = one(&lint(&mut s, json!({"time": 0, "rules": ["text-too-small"], "minTextPx": 30})), "text-too-small");
    assert_eq!(p["severity"], "warning", "{p}");
}

#[test]
fn low_contrast_measures_what_is_under_the_text() {
    let mut s = comp(2.0);
    solid(&mut s, "Grey", 320, 180, "#808080");
    let t = text(&mut s, "Faint", "Hello", 30.0, "#8a8a8a", [160.0, 90.0]);
    let r = lint(&mut s, json!({"time": 0, "rules": ["low-contrast"]}));
    let p = one(&r, "low-contrast");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("error"), Some(t)), "{p}");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("#8a8a8a") && m.contains("#808080") && m.contains(":1"), "{m}");
    // #c0c0c0 on #808080 is about 2.2:1: a warning.
    s.execute("layer.setText", json!({"layer": t, "fill": "#c0c0c0"})).unwrap();
    assert_eq!(one(&lint(&mut s, json!({"time": 0, "rules": ["low-contrast"]})), "low-contrast")["severity"], "warning");
    // White on mid grey is about 4:1: fine.
    s.execute("layer.setText", json!({"layer": t, "fill": "#ffffff"})).unwrap();
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["low-contrast"]})), "low-contrast").is_empty());
    // The layer's opacity blends the text into the background.
    set(&mut s, t, "transform/opacity", json!(20.0));
    assert_eq!(one(&lint(&mut s, json!({"time": 0, "rules": ["low-contrast"]})), "low-contrast")["severity"], "error");
}

#[test]
fn safe_areas_and_cut_off() {
    let mut s = comp(2.0);
    // A title 20 px from the left edge: on screen but outside title safe (32 px per side).
    let t = text(&mut s, "Edge", "Hi", 24.0, "#ffffff", [20.0, 90.0]);
    let r = lint(&mut s, json!({"time": 0}));
    let p = one(&r, "outside-title-safe");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("warning"), Some(t)), "{r:#}");
    assert!(p["message"].as_str().unwrap().contains("on the left"), "{p}");
    assert!(of(&r, "cut-off").is_empty() && of(&r, "outside-action-safe").is_empty(), "{r:#}");
    s.execute("edit.clear", json!({"layers": [t]})).unwrap();

    // A box touching the left edge: outside action safe (16 px), not cut off.
    let b = solid(&mut s, "Box", 40, 40, "#ff0000");
    set(&mut s, b, "transform/position", json!([20.0, 90.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0}));
    let p = one(&r, "outside-action-safe");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("info"), Some(b)), "{r:#}");
    assert!(of(&r, "cut-off").is_empty(), "{r:#}");
    // Half off the left edge: cut off (info for a solid).
    set(&mut s, b, "transform/position", json!([0.0, 90.0, 0.0]));
    let p = one(&lint(&mut s, json!({"time": 0})), "cut-off");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("info"), Some(b)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("50% of it is outside") && p["message"].as_str().unwrap().contains("left"), "{p}");
    // Cut-off text is a warning.
    let t = text(&mut s, "Wide", "Hello", 24.0, "#ffffff", [310.0, 90.0]);
    let r = lint(&mut s, json!({"time": 0, "rules": ["cut-off"]}));
    let p = of(&r, "cut-off").into_iter().find(|p| p["layer"] == t).unwrap();
    assert_eq!(p["severity"], "warning", "{p}");
    assert_eq!(r["rules"], json!(["cut-off"]));
    // A full-frame background is neither cut off nor outside the safe areas.
    let mut s = comp(2.0);
    let bg = solid(&mut s, "BG", 400, 300, "#204060");
    let r = lint(&mut s, json!({"time": 0}));
    assert!(r["problems"].as_array().unwrap().iter().all(|p| p["layer"] != bg), "{r:#}");
}

#[test]
fn text_overlap_names_both_layers() {
    let mut s = comp(2.0);
    let a = text(&mut s, "One", "Hello", 24.0, "#ffffff", [160.0, 90.0]);
    let b = text(&mut s, "Two", "World", 24.0, "#ffffff", [165.0, 95.0]);
    let r = lint(&mut s, json!({"time": 0}));
    let p = one(&r, "text-overlap");
    // The upper layer (created last) first, the other as `other`.
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64(), p["other"].as_u64()), (Some("warning"), Some(b), Some(a)), "{p}");
    assert_eq!(p["otherName"], "One");
    set(&mut s, b, "transform/position", json!([160.0, 150.0, 0.0]));
    assert!(of(&lint(&mut s, json!({"time": 0})), "text-overlap").is_empty());
}

#[test]
fn hidden_visible_reasons() {
    let mut s = comp(2.0);
    let off = solid(&mut s, "Away", 40, 40, "#ff0000");
    set(&mut s, off, "transform/position", json!([1000.0, 90.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["hidden-visible"]}));
    let p = one(&r, "hidden-visible");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("info"), Some(off)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("entirely outside"), "{p}");
    // Info doesn't make the comp unclean.
    assert_eq!(r["clean"], true);
    s.execute("edit.clear", json!({"layers": [off]})).unwrap();

    let faded = solid(&mut s, "Faded", 40, 40, "#ff0000");
    set(&mut s, faded, "transform/opacity", json!(0.0));
    let p = one(&lint(&mut s, json!({"time": 0, "rules": ["hidden-visible"]})), "hidden-visible");
    assert!(p["layer"] == faded && p["message"].as_str().unwrap().contains("opacity is 0"), "{p}");
    s.execute("edit.clear", json!({"layers": [faded]})).unwrap();

    let under = solid(&mut s, "Under", 40, 40, "#ff0000");
    let cover = solid(&mut s, "Cover", 320, 180, "#000000");
    let p = one(&lint(&mut s, json!({"time": 0, "rules": ["hidden-visible"]})), "hidden-visible");
    assert_eq!((p["layer"].as_u64(), p["other"].as_u64()), (Some(under), Some(cover)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("covered by “Cover”"), "{p}");
    // A half-transparent cover hides nothing.
    set(&mut s, cover, "transform/opacity", json!(50.0));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["hidden-visible"]})), "hidden-visible").is_empty());
}

#[test]
fn range_merges_occurrences() {
    let mut s = comp(2.0);
    let b = solid(&mut s, "Mover", 40, 40, "#ff0000");
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/position", "value": [160.0, 90.0, 0.0], "time": 0.0})).unwrap();
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/position", "value": [320.0, 90.0, 0.0], "time": 1.0})).unwrap();
    let r = lint(&mut s, json!({"start": 0, "end": 2, "rules": ["cut-off"]}));
    // Every 0.5 s, the end clamped to the last frame (1.96 s), keyframes at 0 and 1 s.
    assert_eq!(r["times"], json!([0.0, 0.5, 1.0, 1.5, 1.96]), "{r}");
    let p = one(&r, "cut-off");
    assert_eq!((p["layer"].as_u64(), p["time"].as_f64(), p["until"].as_f64()), (Some(b), Some(1.0), Some(1.96)), "{p}");
    // Keyframes add sample times between the steps.
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/opacity", "value": 50.0, "time": 0.2})).unwrap();
    let r = lint(&mut s, json!({"start": 0, "end": 1, "rules": ["cut-off"]}));
    assert_eq!(r["times"], json!([0.0, 0.2, 0.5, 1.0]), "{r}");
    // Long ranges are thinned to 120 samples.
    let mut s = comp(10.0);
    solid(&mut s, "Box", 40, 40, "#ff0000");
    let r = lint(&mut s, json!({"start": 0, "end": 10, "step": 0.04, "rules": ["cut-off"]}));
    assert_eq!((r["truncated"].as_bool(), r["times"].as_array().unwrap().len()), (Some(true), 120), "{r}");
    assert_eq!(r["times"][0], 0.0);
    assert_eq!(r["times"][119], 9.96);
}

#[test]
fn bad_params_are_errors() {
    let mut s = comp(2.0);
    for p in [
        json!({"rules": ["nope"]}),
        json!({"time": 1, "start": 0}),
        json!({"start": 1, "end": 0}),
        json!({"start": 0, "step": 0}),
        json!({"minTextPx": -1}),
        json!({"minContrast": 30}),
        json!({"colour": 1}),
    ] {
        assert!(s.execute_checked("render.lint", p.clone()).is_err(), "{p}");
    }
    assert!(s.execute_checked("render.lint", json!({"start": 0, "end": 1, "step": 0.5, "rules": ["cut-off"], "minTextPx": 9, "minContrast": 4.5})).is_ok());
    let e = s.execute("render.lint", json!({"rules": ["nope"]})).unwrap_err().to_string();
    assert!(e.contains("text-too-small"), "{e}");
}

#[test]
fn off_beat_matches_key_moments_to_beats() {
    let mut s = comp(2.0);
    let l = solid(&mut s, "Logo", 10, 10, "#ffffff");
    let comp = s.project.comp(s.active_comp_id().unwrap()).unwrap().clone();
    let beats = [0.0, 0.5, 1.0, 1.5, 2.0];
    let m = |t: f64| KeyMoment { layer: LayerId(l), time: t, what: "Position keyframe".into() };
    // On the beat, one frame off (allowed), 3 frames late, exactly between beats, 2.5 frames early.
    let found = lint::off_beat(&comp, &[m(1.0), m(1.04), m(1.12), m(1.25), m(0.9)], &beats, 25.0);
    let got: Vec<(f64, Severity, bool)> = found.iter().map(|(t, f)| (t.seconds(), f.severity, f.message.contains("after"))).collect();
    assert_eq!(got.len(), 2, "{found:#?}");
    assert!((got[0].0 - 1.12).abs() < 1e-6 && got[0].2, "{found:#?}");
    assert!((got[1].0 - 0.9).abs() < 1e-6 && !got[1].2, "{found:#?}");
    assert!(found[0].1.message.contains("3 frames after the beat at 1.000 s"), "{}", found[0].1.message);
    assert!(found[1].1.message.contains("2.5 frames before"), "{}", found[1].1.message);
    assert!(lint::off_beat(&comp, &[m(1.12)], &[1.0], 25.0).is_empty(), "one beat is no grid");
}

#[test]
fn beats_of_a_click_track() {
    // 0.02 s bursts every 0.5 s from 0.25 s, 6 s at 12 kHz.
    let rate = 12_000u32;
    let mut mono = vec![0.0f32; 6 * rate as usize];
    for k in 0..12 {
        let at = (0.25 + 0.5 * k as f64) * rate as f64;
        for i in 0..240 {
            if let Some(v) = mono.get_mut(at as usize + i) {
                *v = 0.5 * (i as f32 * 0.5).sin();
            }
        }
    }
    let beats = lint::detect_beats(&mono, rate);
    assert_eq!(beats.len(), 12, "{beats:?}");
    for (k, b) in beats.iter().enumerate() {
        assert!((b - (0.25 + 0.5 * k as f64)).abs() <= 0.006, "{beats:?}");
    }
    // Silence and a steady tone have no beat.
    assert!(lint::detect_beats(&vec![0.0; 12_000], rate).is_empty());
    let tone: Vec<f32> = (0..36_000).map(|i| 0.5 * (i as f32 * 0.5).sin()).collect();
    assert!(lint::detect_beats(&tone, rate).is_empty());
}

#[test]
fn off_beat_on_a_comp_with_clicks() {
    let mut s = comp(4.0);
    // Clicks: one-frame 1 kHz Tone solids every 12 frames (0.48 s, 125 BPM).
    for k in 0..8 {
        let c = solid(&mut s, &format!("Click {k}"), 10, 10, "#000000");
        s.execute("effect.apply", json!({"layers": [c], "effect": "Tone"})).unwrap();
        for (p, v) in [("frequency1", 1000.0), ("frequency2", 0.0), ("frequency3", 0.0), ("frequency4", 0.0), ("frequency5", 0.0), ("level", 50.0)] {
            set(&mut s, c, &format!("effects/#1/{p}"), json!(v));
        }
        s.execute("layer.timing", json!({"layers": [c], "in": 0.48 * k as f64, "out": 0.48 * k as f64 + 0.04})).unwrap();
    }
    // A logo keyed on the beat at 0.96 s and 3 frames after it.
    let logo = solid(&mut s, "Logo", 20, 20, "#ffffff");
    s.execute("prop.addKey", json!({"layer": logo, "path": "transform/position", "value": [100.0, 90.0, 0.0], "time": 0.96})).unwrap();
    s.execute("prop.addKey", json!({"layer": logo, "path": "transform/position", "value": [200.0, 90.0, 0.0], "time": 1.08})).unwrap();
    let r = lint(&mut s, json!({"start": 0, "end": 4, "rules": ["off-beat"]}));
    let p = one(&r, "off-beat");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("warning"), Some(logo)), "{r:#}");
    assert!((p["time"].as_f64().unwrap() - 1.08).abs() < 1e-6 && p["until"] == p["time"], "{p}");
    assert!(p["message"].as_str().unwrap().contains("3 frames after the beat at 0.960 s"), "{p}");
    assert!(r["notes"][0].as_str().unwrap().contains("125 BPM"), "{r}");
    // One time checks the key moments on that frame only.
    assert_eq!(of(&lint(&mut s, json!({"time": 1.08, "rules": ["off-beat"]})), "off-beat").len(), 1);
    assert!(of(&lint(&mut s, json!({"time": 2.0, "rules": ["off-beat"]})), "off-beat").is_empty());
}

/// The centre of a layer's box in comp pixels (`layer.bounds`).
fn centre(s: &mut Session, layer: u64) -> [f64; 2] {
    let r = s.execute("layer.bounds", json!({"layers": [layer]})).unwrap();
    let c = &r["layers"][0]["center"];
    [c[0].as_f64().unwrap(), c[1].as_f64().unwrap()]
}

#[test]
fn text_off_centre_in_its_card() {
    let mut s = comp(2.0);
    solid(&mut s, "BG", 320, 180, "#102040");
    let pill = solid(&mut s, "Pill", 120, 40, "#2060c0");
    let t = s
        .execute("layer.newText", json!({"name": "Price", "text": "Hello", "size": 20, "fill": "#ffffff", "position": [160.0, 96.0], "justify": "left"}))
        .unwrap()["layer"]
        .as_u64()
        .unwrap();
    let c = centre(&mut s, t);
    // The pill's centre 5 px below the text's: the text is 5 px above it.
    set(&mut s, pill, "transform/position", json!([c[0] + 10.0, c[1] + 5.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]}));
    let p = one(&r, "text-off-centre");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64(), p["other"].as_u64()), (Some("warning"), Some(t), Some(pill)), "{p}");
    let m = p["message"].as_str().unwrap();
    // Left-justified text isn't checked across: only the 5 px up/down.
    assert!(m.contains("5.0 px above the centre of its card “Pill”") && m.contains("5.0 px down") && !m.contains("left"), "{m}");
    // Centre-justified text is checked across too (the card is 10 px right of it).
    s.execute("layer.setText", json!({"layer": t, "justify": "center"})).unwrap();
    let c = centre(&mut s, t);
    set(&mut s, pill, "transform/position", json!([c[0] + 10.0, c[1] + 5.0, 0.0]));
    let m = one(&lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]})), "text-off-centre")["message"].as_str().unwrap().to_string();
    assert!(m.contains("5.0 px above and 10.0 px left of the centre") && m.contains("5.0 px down and 10.0 px right"), "{m}");
    // Within 1.5 px: centred. The full-frame background is never the card.
    set(&mut s, pill, "transform/position", json!([c[0] + 1.0, c[1] - 1.0, 0.0]));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]})), "text-off-centre").is_empty());
    set(&mut s, pill, "transform/position", json!([60.0, 30.0, 0.0]));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]})), "text-off-centre").is_empty());
}

#[test]
fn text_off_centre_follows_a_rotated_card_and_skips_tall_cards() {
    let mut s = comp(2.0);
    let card = solid(&mut s, "Card", 140, 36, "#2060c0");
    let t = text(&mut s, "Label", "Hi", 20.0, "#ffffff", [160.0, 96.0]);
    // Card and text both turned 90° clockwise: the card's "down" is comp left.
    set(&mut s, card, "transform/rotation", json!(90.0));
    set(&mut s, t, "transform/rotation", json!(90.0));
    let c = centre(&mut s, t);
    set(&mut s, card, "transform/position", json!([c[0] - 4.0, c[1], 0.0]));
    let m = one(&lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]})), "text-off-centre")["message"].as_str().unwrap().to_string();
    assert!(m.contains("4.0 px above"), "{m}");

    // A card more than 3 text heights tall: where the text sits in it is left alone.
    let mut s = comp(2.0);
    let tall = solid(&mut s, "Tall", 140, 170, "#2060c0");
    let t = text(&mut s, "Label", "Hi", 20.0, "#ffffff", [160.0, 96.0]);
    let c = centre(&mut s, t);
    set(&mut s, tall, "transform/position", json!([c[0], c[1] + 20.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]}));
    assert!(of(&r, "text-off-centre").is_empty(), "{r:#}");
}

#[test]
fn nested_layers_are_measured_in_the_final_frame() {
    let mut s = comp(2.0);
    let t = text(&mut s, "Inside", "Hello", 24.0, "#ffffff", [250.0, 90.0]);
    let r = s.execute("layer.precompose", json!({"layers": [t], "name": "Card", "mode": "move"})).unwrap();
    let pre = r["layer"].as_u64().unwrap_or_else(|| panic!("{r}"));
    // Inside the precomp the text is whole; the precomp layer shifted 60 px right cuts it off here.
    let r = lint(&mut s, json!({"time": 0, "rules": ["nested-cut-off"]}));
    assert!(of(&r, "nested-cut-off").is_empty(), "{r:#}");
    set(&mut s, pre, "transform/position", json!([220.0, 90.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["nested-cut-off", "cut-off"]}));
    let p = one(&r, "nested-cut-off");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64(), p["inner"]["layer"].as_u64()), (Some("warning"), Some(pre), Some(t)), "{p}");
    assert_eq!(p["inner"]["path"], "Card › Inside", "{p}");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("“Card › Inside”") && m.contains("right frame edge"), "{m}");
    // Back in the frame but scaled to 20 %: its 24 px font is about 5 px here (minimum 6).
    set(&mut s, pre, "transform/position", json!([160.0, 90.0, 0.0]));
    set(&mut s, pre, "transform/scale", json!([20.0, 20.0, 100.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["nested-cut-off", "text-too-small"]}));
    assert!(of(&r, "nested-cut-off").is_empty(), "{r:#}");
    let p = one(&r, "text-too-small");
    assert_eq!((p["layer"].as_u64(), p["inner"]["layer"].as_u64()), (Some(pre), Some(t)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("font size 24 inside the precomp"), "{p}");
}

#[test]
fn nested_layer_clipped_by_its_precomp_edge() {
    let mut s = comp(2.0);
    let t = text(&mut s, "Edge", "Hello", 24.0, "#ffffff", [160.0, 90.0]);
    let r = s.execute("layer.precompose", json!({"layers": [t], "name": "Phone", "mode": "move"})).unwrap();
    let pre = r["layer"].as_u64().unwrap();
    let inner_comp = r["comp"].as_u64().unwrap_or(0);
    // Inside the precomp, the text runs past its right edge; the precomp layer is scaled down, so
    // the text is well inside this comp's frame but still clipped at the precomp's edge.
    let inner = s.project.comp(effectcraft_project::ItemId(inner_comp)).map(|c| c.layers[0].id.0).unwrap_or(t);
    s.execute("prop.set", json!({"comp": inner_comp, "layer": inner, "path": "transform/position", "value": [310.0, 90.0, 0.0]})).unwrap();
    set(&mut s, pre, "transform/scale", json!([50.0, 50.0, 100.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["nested-cut-off"]}));
    let p = one(&r, "nested-cut-off");
    assert_eq!(p["layer"].as_u64(), Some(pre), "{p}");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("cut off by the edge of the precomp “Phone” (320×180)") && m.contains("Collapse Transformations"), "{m}");
    // Collapsed: the precomp doesn't clip it.
    s.execute("layer.setSwitch", json!({"layers": [pre], "switch": "collapse", "value": true})).unwrap();
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["nested-cut-off"]})), "nested-cut-off").is_empty());
}

#[test]
fn exposed_edge_strip_under_a_full_screen_shot() {
    let mut s = comp(2.0);
    // 10 px short of the frame: a strip along the bottom shows the background.
    let shot = solid(&mut s, "Shot", 320, 170, "#808080");
    set(&mut s, shot, "transform/position", json!([160.0, 85.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["exposed-edge"]}));
    let p = one(&r, "exposed-edge");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64()), (Some("warning"), Some(shot)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("10 px along the bottom edge"), "{p}");
    // Covering the frame: clean. An overlay that never fills the frame isn't checked.
    set(&mut s, shot, "transform/position", json!([160.0, 90.0, 0.0]));
    set(&mut s, shot, "transform/scale", json!([100.0, 110.0, 100.0]));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["exposed-edge"]})), "exposed-edge").is_empty());
    set(&mut s, shot, "transform/scale", json!([30.0, 30.0, 100.0]));
    assert!(of(&lint(&mut s, json!({"time": 0, "rules": ["exposed-edge"]})), "exposed-edge").is_empty());
}

#[test]
fn exposed_edge_gap_in_a_transition() {
    let mut s = comp(2.0);
    let a = solid(&mut s, "Out", 320, 180, "#804040");
    let b = solid(&mut s, "In", 320, 180, "#408040");
    // "Out" slides off to the left while "In" slides in from the right, 40 px behind it.
    s.execute("prop.addKey", json!({"layer": a, "path": "transform/position", "value": [160.0, 90.0, 0.0], "time": 0.0})).unwrap();
    s.execute("prop.addKey", json!({"layer": a, "path": "transform/position", "value": [-160.0, 90.0, 0.0], "time": 1.0})).unwrap();
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/position", "value": [520.0, 90.0, 0.0], "time": 0.0})).unwrap();
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/position", "value": [160.0, 90.0, 0.0], "time": 1.0})).unwrap();
    let r = lint(&mut s, json!({"start": 0, "end": 1, "step": 0.25, "rules": ["exposed-edge"]}));
    let p = one(&r, "exposed-edge");
    assert_eq!((p["time"].as_f64(), p["until"].as_f64(), p["layer"].as_u64(), p["other"].as_u64()), (Some(0.25), Some(0.75), Some(a), Some(b)), "{p}");
    assert!(p["message"].as_str().unwrap().contains("gap in the transition"), "{p}");
    // Overlapping shots: no gap.
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/position", "value": [480.0, 90.0, 0.0], "time": 0.0})).unwrap();
    assert!(of(&lint(&mut s, json!({"start": 0, "end": 1, "step": 0.25, "rules": ["exposed-edge"]})), "exposed-edge").is_empty());
}

#[test]
fn coverage_measures_edge_strips() {
    let mut img = effectcraft_raster::Image::new(20, 10);
    for (i, p) in img.data.iter_mut().enumerate() {
        // Opaque except the last 2 rows and the first column.
        let (x, y) = (i % 20, i / 20);
        if y < 8 && x > 0 {
            *p = [1.0, 1.0, 1.0, 1.0];
        }
    }
    let c = lint::coverage(&img, 0.5);
    // At half scale each line is 2 comp px: one empty column on the left, two empty rows at the bottom.
    assert_eq!(c.strips, [2.0, 0.0, 0.0, 4.0], "{c:?}");
    assert!((c.empty - 48.0 / 200.0).abs() < 1e-9, "{c:?}");
}

#[test]
fn texts_on_one_card_are_centred_as_a_block() {
    let mut s = comp(2.0);
    let bar = solid(&mut s, "Bar", 200, 70, "#203040");
    let name = text(&mut s, "Name", "Ada", 20.0, "#ffffff", [160.0, 84.0]);
    let role = text(&mut s, "Role", "Maths", 14.0, "#ffffff", [160.0, 104.0]);
    let (a, b) = (centre(&mut s, name), centre(&mut s, role));
    // The bar centred on the two lines together: clean, though each line alone is off-centre.
    set(&mut s, bar, "transform/position", json!([160.0, (a[1] + b[1]) / 2.0, 0.0]));
    let r = s.execute("layer.bounds", json!({"layers": [name, role]})).unwrap();
    let top = r["layers"][0]["bounds"][1].as_f64().unwrap();
    let bottom = r["layers"][1]["bounds"][1].as_f64().unwrap() + r["layers"][1]["bounds"][3].as_f64().unwrap();
    set(&mut s, bar, "transform/position", json!([160.0, (top + bottom) / 2.0, 0.0]));
    let r = lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]}));
    assert!(of(&r, "text-off-centre").is_empty(), "{r:#}");
    // The block 6 px low: one problem naming both.
    set(&mut s, bar, "transform/position", json!([160.0, (top + bottom) / 2.0 - 6.0, 0.0]));
    let p = one(&lint(&mut s, json!({"time": 0, "rules": ["text-off-centre"]})), "text-off-centre");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("“Role” and “Name” (together) are 6.0 px below") || m.contains("“Name” and “Role” (together) are 6.0 px below"), "{m}");
}

fn key(s: &mut Session, layer: u64, path: &str, time: f64, value: Value) {
    s.execute("prop.addKey", json!({"layer": layer, "path": path, "time": time, "timeBase": "comp", "value": value})).unwrap();
}

/// Talking-head footage with sound for the whole comp: the shot a cutaway cuts back to.
fn base_shot(s: &mut Session) -> u64 {
    use effectcraft_project::{Footage, FootageKind, ItemKind};
    let p = std::sync::Arc::make_mut(&mut s.project);
    let f =
        Footage { path: "/nowhere/main.mov".into(), kind: FootageKind::Still, width: 320, height: 180, has_video: true, has_audio: true, ..Default::default() };
    let item = p.add_item("MAIN", Default::default(), None, ItemKind::Footage(f)).0;
    let comp = s.active_comp_id().unwrap().0;
    s.execute("layer.addItem", json!({"comp": comp, "item": item})).unwrap()["layer"].as_u64().unwrap()
}

#[test]
fn exit_at_cut_full_screen_cutaway_holds_to_the_end() {
    // 2 s at 25 fps: frames 0–49. A background over the footage, with a title on it.
    let mut s = comp(2.0);
    let main = base_shot(&mut s);
    let bg = solid(&mut s, "BG", 320, 180, "#102040");
    let title = text(&mut s, "Title", "Hello", 24.0, "#ffffff", [160.0, 90.0]);
    let rules = json!({"start": 0, "end": 2, "rules": ["exit-at-cut"]});
    let r = lint(&mut s, rules.clone());
    assert!(of(&r, "exit-at-cut").is_empty(), "{r:#}");
    assert!(r["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("a full-screen cutaway")), "{r:#}");
    // Both fade out over the last 8 frames, into the cut: one warning on the background.
    for l in [bg, title] {
        key(&mut s, l, "transform/opacity", 1.64, json!(100.0));
        key(&mut s, l, "transform/opacity", 1.96, json!(0.0));
    }
    let p = one(&lint(&mut s, rules.clone()), "exit-at-cut");
    assert_eq!((p["severity"].as_str(), p["layer"].as_u64(), p["other"].as_u64()), (Some("warning"), Some(bg), Some(main)), "{p}");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("“Title”, “BG” fade or move out from frame 41 to 49") && m.contains("hold to the end and hard-cut"), "{m}");

    // Leaving early while the comp runs on (cut at 1.2 s, frame 30): a warning too.
    let mut s = comp(2.0);
    base_shot(&mut s);
    let bg = solid(&mut s, "BG", 320, 180, "#102040");
    s.execute("layer.timing", json!({"layers": [bg], "out": 1.2})).unwrap();
    let p = one(&lint(&mut s, rules.clone()), "exit-at-cut");
    assert_eq!((p["severity"].as_str(), p["time"].as_f64()), (Some("warning"), Some(1.2)), "{p}");
    assert!(
        p["message"].as_str().unwrap().contains("gone from frame 30") && p["message"].as_str().unwrap().contains("runs 20 more frames, to frame 49"),
        "{p}"
    );
    // A hard cut one frame before the end is a cut, not an exit.
    s.execute("layer.timing", json!({"layers": [bg], "out": 1.96})).unwrap();
    assert!(of(&lint(&mut s, rules), "exit-at-cut").is_empty());
}

#[test]
fn exit_at_cut_lets_overlays_fade_into_the_end() {
    // A card over the footage (an overlay): fading into the cut is right.
    let mut s = comp(2.0);
    base_shot(&mut s);
    let card = solid(&mut s, "Card", 120, 40, "#2060c0");
    key(&mut s, card, "transform/opacity", 1.64, json!(100.0));
    key(&mut s, card, "transform/opacity", 1.96, json!(0.0));
    let rules = json!({"start": 0, "end": 2, "rules": ["exit-at-cut"]});
    let r = lint(&mut s, rules.clone());
    assert!(of(&r, "exit-at-cut").is_empty(), "{r:#}");
    assert!(r["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("an overlay")), "{r:#}");
    // Gone well before the end: only a note (info), the tail may be footage on purpose.
    let mut s = comp(2.0);
    base_shot(&mut s);
    let card = solid(&mut s, "Card", 120, 40, "#2060c0");
    key(&mut s, card, "transform/opacity", 1.0, json!(100.0));
    key(&mut s, card, "transform/opacity", 1.2, json!(0.0));
    let p = one(&lint(&mut s, rules), "exit-at-cut");
    assert_eq!(p["severity"], "info", "{p}");
}

#[test]
fn text_off_centre_is_measured_at_rest_with_a_fix_in_its_own_units() {
    let mut s = comp(2.0);
    // A 60×20 pill shown at 200 % (120×40 on screen), popping in 0 → 220 → 200 % over 0.4 s.
    let pill = solid(&mut s, "Pill", 60, 20, "#2060c0");
    let t = text(&mut s, "Label", "Hi", 12.0, "#ffffff", [160.0, 90.0]);
    // The pill centred on the text, then the text parented to it.
    let c = centre(&mut s, t);
    set(&mut s, pill, "transform/position", json!([c[0], c[1], 0.0]));
    s.execute("layer.setParent", json!({"layers": [t], "parent": pill})).unwrap();
    let pos = s.execute("prop.get", json!({"layer": t, "path": "transform/position"})).unwrap()["value"].clone();
    let (x, y) = (pos[0].as_f64().unwrap(), pos[1].as_f64().unwrap());
    // At 200 % the text sits 3 pill units (6 comp px) low.
    set(&mut s, pill, "transform/scale", json!([200.0, 200.0, 100.0]));
    set(&mut s, t, "transform/position", json!([x, y + 3.0, 0.0]));
    for (time, v) in [(0.0, 0.0), (0.2, 220.0), (0.4, 200.0)] {
        key(&mut s, pill, "transform/scale", time, json!([v, v, 100.0]));
    }
    // Linted mid-pop (0.1 s): measured at the rest pose (0.4 s), 6 px, a 3-unit fix.
    let r = lint(&mut s, json!({"time": 0.1, "rules": ["text-off-centre"]}));
    let p = one(&r, "text-off-centre");
    assert_eq!(p["time"].as_f64(), Some(0.4), "{p}");
    let m = p["message"].as_str().unwrap();
    assert!(m.contains("6.0 px below") && m.contains("in “Pill”'s layer space") && m.contains("rest pose, 0.400 s"), "{m}");
    let fix = &p["fix"];
    assert_eq!(
        (fix["command"].as_str(), fix["params"]["layer"].as_u64(), fix["params"]["path"].as_str()),
        (Some("prop.set"), Some(t), Some("transform/position")),
        "{p}"
    );
    let to = fix["params"]["value"].as_array().unwrap();
    assert!((to[0].as_f64().unwrap() - x).abs() < 0.02 && (to[1].as_f64().unwrap() - y).abs() < 0.02, "{fix}");
    // Running the fix centres it.
    s.execute(fix["command"].as_str().unwrap(), fix["params"].clone()).unwrap();
    let r = lint(&mut s, json!({"start": 0, "end": 2, "rules": ["text-off-centre"]}));
    assert!(of(&r, "text-off-centre").is_empty(), "{r:#}");
}

#[test]
fn rest_poses_skip_moving_keys() {
    let mut s = comp(2.0);
    let l = solid(&mut s, "Pill", 60, 20, "#2060c0");
    // Pop in (0 → 0.4 s), hold, fade out (1.52 → 1.8 s).
    for (time, v) in [(0.0, 0.0), (0.2, 110.0), (0.4, 100.0)] {
        key(&mut s, l, "transform/scale", time, json!([v, v, 100.0]));
    }
    key(&mut s, l, "transform/opacity", 1.52, json!(100.0));
    key(&mut s, l, "transform/opacity", 1.8, json!(0.0));
    let c = s.active_comp().unwrap();
    let poses: Vec<f64> = lint::rest_poses(c, c.layer(LayerId(l)).unwrap()).iter().map(|t| (t.seconds() * 100.0).round() / 100.0).collect();
    // 0.2 s (the overshoot) is mid-move; 1.8 s and 1.52 s hold on one side; 0.4 s and 0 too.
    assert_eq!(poses, vec![1.8, 1.52, 0.4, 0.0], "{poses:?}");
}
