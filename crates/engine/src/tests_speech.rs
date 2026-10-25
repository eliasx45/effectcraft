//! Speech maps (`speech.import` / `speech.get` / `speech.accents` / `speech.clear`), speed-graph
//! peaks and the `off-accent` lint.

use effectcraft_project::LayerId;
use effectcraft_project::speech::Syllable;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;
use crate::lint;

/// A 320×180 comp, 4 s at 25 fps, with a 20 px solid "Logo".
fn comp() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Talk", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Logo", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    (s, l)
}

/// Scale keys at `a` and `b` (comp seconds), eased (Easy Ease) unless `linear`.
fn scale_keys(s: &mut Session, l: u64, a: f64, b: f64, linear: bool) {
    s.execute("prop.addKey", json!({"layer": l, "path": "transform/scale", "value": [100.0, 100.0, 100.0], "time": a, "timeBase": "comp"})).unwrap();
    s.execute("prop.addKey", json!({"layer": l, "path": "transform/scale", "value": [160.0, 160.0, 100.0], "time": b, "timeBase": "comp"})).unwrap();
    if !linear {
        let uid = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("transform/scale").unwrap().uid;
        let keys: Vec<Value> = [a, b].iter().map(|t| json!({"layer": l, "prop": uid, "time": t})).collect();
        s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
        s.execute_checked("keys.easyEase", json!({})).unwrap();
    }
}

/// A one-word map: "content", stressed first syllable at `t` / `vowel`, unstressed second.
fn map(t: f64, vowel: f64) -> Value {
    json!({"engine": "test", "words": [
        {"word": "content", "start": t, "end": vowel + 0.4, "syllables": [
            {"t": t, "vowel": vowel, "stress": 1},
            {"t": vowel + 0.2, "vowel": vowel + 0.25, "stress": 0}
        ]}
    ]})
}

fn off_accent(s: &mut Session) -> Vec<Value> {
    let r = s.execute("render.lint", json!({"start": 0, "end": 4, "rules": ["off-accent"]})).unwrap();
    r["problems"].as_array().unwrap().clone()
}

#[test]
fn speed_peak_of_an_eased_segment_sits_in_its_middle() {
    let (mut s, l) = comp();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    let peaks = lint::speed_peaks(s.active_comp().unwrap(), 0.0, 4.0);
    assert_eq!(peaks.len(), 1, "{peaks:#?}");
    assert!((peaks[0].time - 1.5).abs() < 0.002, "{peaks:#?}");
    assert_eq!((peaks[0].layer, peaks[0].property.as_str()), (LayerId(l), "Scale"));
    // Outside the range: none.
    assert!(lint::speed_peaks(s.active_comp().unwrap(), 2.5, 4.0).is_empty());
    // Trimmed away (the layer starts after the peak): nothing shows, so no peak.
    s.execute("layer.timing", json!({"layers": [l], "in": 1.6})).unwrap();
    assert!(lint::speed_peaks(s.active_comp().unwrap(), 0.0, 4.0).is_empty());
    // Linear keys move at a constant speed: no peak.
    let (mut s, l) = comp();
    scale_keys(&mut s, l, 1.0, 2.0, true);
    assert!(lint::speed_peaks(s.active_comp().unwrap(), 0.0, 4.0).is_empty());
}

#[test]
fn accent_is_one_frame_before_the_vowel_never_before_the_onset() {
    let syl = |t: f64, v: f64| Syllable { t: Tick::from_seconds_f64(t), vowel: Tick::from_seconds_f64(v), stress: 1 };
    // Elias's "CONtent" in 10-10 MAIN: k burst 42.665, vowel 42.780 → frame 1281 at 29.97 fps.
    let fps = 30000.0 / 1001.0;
    assert_eq!((lint::accent_time(&syl(42.665, 42.78), fps, 1.0) * fps).round(), 1281.0);
    assert!((lint::accent_time(&syl(1.0, 1.01), 25.0, 1.0) - 1.0).abs() < 1e-9, "clamped to the onset");
    assert!((lint::accent_time(&syl(1.0, 1.2), 25.0, 0.0) - 1.2).abs() < 1e-9, "lead 0 = the vowel");
}

#[test]
fn off_accent_flags_near_misses_only() {
    // Peak at 1.5 s. Accent = vowel − 1 frame (0.04 s).
    for (vowel, flagged, words) in
        [(1.54, false, "hit"), (1.62, true, "2 frames before"), (1.48, true, "1.5 frames after"), (1.76, false, "5.5 frames away: outside the window")]
    {
        let (mut s, l) = comp();
        scale_keys(&mut s, l, 1.0, 2.0, false);
        s.execute("speech.import", json!({"map": map(vowel - 0.1, vowel)})).unwrap();
        let found = off_accent(&mut s);
        assert_eq!(!found.is_empty(), flagged, "{words}: {found:#?}");
        if flagged {
            let m = found[0]["message"].as_str().unwrap();
            assert!(m.contains(words) && m.contains("\"content\"") && m.contains("Scale"), "{m}");
            assert_eq!((found[0]["severity"].as_str(), found[0]["layer"].as_u64()), (Some("warning"), Some(l)));
        }
    }
    // A wider window catches the 5.5-frame miss.
    let (mut s, l) = comp();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    s.execute("speech.import", json!({"map": map(1.66, 1.76)})).unwrap();
    let r = s.execute("render.lint", json!({"start": 0, "end": 4, "rules": ["off-accent"], "accentWindow": 6})).unwrap();
    assert_eq!(r["problems"].as_array().unwrap().len(), 1, "{r:#}");
    // A longer lead moves the accent towards the onset: vowel 1.66 − 4 frames = 1.50 hits.
    s.execute("speech.import", json!({"map": map(1.40, 1.66)})).unwrap();
    assert_eq!(off_accent(&mut s).len(), 1, "default accent 1.62: 3 frames late");
    let r = s.execute("render.lint", json!({"start": 0, "end": 4, "rules": ["off-accent"], "accentLead": 4})).unwrap();
    assert!(r["problems"].as_array().unwrap().is_empty(), "{r:#}");
    assert!(s.execute_checked("render.lint", json!({"rules": ["off-accent"], "accentWindow": 0})).is_err());
}

#[test]
fn off_accent_without_a_map_is_skipped_with_a_note() {
    let (mut s, l) = comp();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    let r = s.execute("render.lint", json!({"start": 0, "end": 4, "rules": ["off-accent"]})).unwrap();
    assert!(r["problems"].as_array().unwrap().is_empty());
    assert!(r["notes"].to_string().contains("no speech map"), "{r}");
}

#[test]
fn import_maps_media_time_through_the_layer() {
    let (mut s, _) = comp();
    let voice = s.execute("layer.newSolid", json!({"name": "Voice", "width": 10, "height": 10, "color": "#000000"})).unwrap()["layer"].as_u64().unwrap();
    // The voice layer starts at 1 s and is trimmed to 1.6–3 s; the map's times are media + 10.
    s.execute("layer.timing", json!({"layers": [voice], "start": 1.0})).unwrap();
    s.execute("layer.timing", json!({"layers": [voice], "in": 1.6, "out": 3.0})).unwrap();
    let v = s.active_comp().unwrap().layer(LayerId(voice)).unwrap();
    assert_eq!((v.start_time.seconds(), v.in_point.seconds(), v.out_point.seconds()), (1.0, 1.6, 3.0));
    let doc = json!({"offset": 10.0, "engine": "test", "media": "/tmp/voice.wav", "words": [
        {"word": "before", "start": 10.1, "end": 10.3, "syllables": [{"t": 10.1, "vowel": 10.15, "stress": 1}]},
        {"word": "content", "start": 11.0, "end": 11.4, "syllables": [{"t": 11.0, "vowel": 11.1, "stress": 1}, {"t": 11.2, "vowel": 11.25, "stress": 0}]},
        {"word": "after", "start": 12.5, "end": 12.8, "syllables": [{"t": 12.5, "vowel": 12.55, "stress": 1}]}
    ]});
    let r = s.execute("speech.import", json!({"map": doc, "layer": voice})).unwrap();
    assert_eq!((r["words"].as_u64(), r["dropped"].as_u64(), r["stressed"].as_u64(), r["layer"].as_u64()), (Some(1), Some(2), Some(1), Some(voice)), "{r}");
    let g = s.execute("speech.get", json!({"stressed": true})).unwrap();
    let a = &g["accents"][0];
    // Media 1.0 s → comp 2.0 s; accent = vowel 2.1 − 1 frame = 2.06 (frame 51.5 → 52).
    assert_eq!(
        (a["word"].as_str(), a["onset"].as_f64(), a["vowel"].as_f64(), a["accent"].as_f64()),
        (Some("content"), Some(2.0), Some(2.1), Some(2.06)),
        "{g}"
    );
    assert_eq!(a["onsetFrame"].as_f64(), Some(50.0));
    let w = s.execute("speech.get", json!({})).unwrap();
    assert_eq!(w["list"][0]["syllables"].as_array().unwrap().len(), 2, "{w}");
    // Undo removes it; the map draws nothing.
    let before = s.active_comp().unwrap().clone();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active_comp().unwrap().speech.is_none());
    assert!(before.same_pixels(s.active_comp().unwrap()));
    assert!(matches!(before.pixel_form(), std::borrow::Cow::Owned(c) if c.speech.is_none()));
}

#[test]
fn bad_imports_are_errors() {
    let (mut s, l) = comp();
    for p in [
        json!({}),
        json!({"map": {"words": "no"}}),
        json!({"map": {"words": [{"word": "x", "start": "a", "end": 1}]}}),
        json!({"map": {"words": [{"word": "x", "start": 9.0, "end": 9.5}]}, "layer": l}),
        json!({"map": map(1.0, 1.1), "path": "/nope.json"}),
        json!({"path": "/definitely/not/here.json"}),
        json!({"map": map(1.0, 1.1), "layer": 999}),
    ] {
        assert!(s.execute_checked("speech.import", p.clone()).is_err(), "{p}");
    }
    assert!(s.execute_checked("speech.get", json!({})).is_err(), "no map yet");
    assert_eq!(s.execute("speech.clear", json!({})).unwrap()["removed"], false);
}

#[test]
fn accents_list_every_peak_with_its_offset_and_the_map_saves() {
    let (mut s, l) = comp();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    s.execute("speech.import", json!({"map": map(1.48, 1.62)})).unwrap();
    let r = s.execute("speech.accents", json!({})).unwrap();
    assert_eq!((r["peaks"].as_u64(), r["hits"].as_u64()), (Some(1), Some(0)), "{r}");
    let p = &r["list"][0];
    // Accent 1.58, peak 1.5: 2 frames early.
    assert_eq!(
        (p["word"].as_str(), p["accent"].as_f64(), p["offsetFrames"].as_f64(), p["hit"].as_bool()),
        (Some("content"), Some(1.58), Some(-2.0), Some(false)),
        "{p}"
    );
    assert_eq!(s.execute("speech.accents", json!({"layer": l, "start": 3.0})).unwrap()["peaks"], 0);
    // Saved with the comp, and only when there is one.
    let comp = s.active_comp().unwrap().clone();
    let text = serde_json::to_string(&comp).unwrap();
    let back: effectcraft_project::Comp = serde_json::from_str(&text).unwrap();
    assert_eq!(back.speech, comp.speech);
    assert_eq!(s.execute("speech.clear", json!({})).unwrap()["removed"], true);
    assert!(!serde_json::to_string(s.active_comp().unwrap()).unwrap().contains("\"speech\""));
}

/// `inner` placed in a new 25 fps comp `name` (12 s) at `start` with `stretch` %, trimmed to
/// `in_out` (comp seconds): (the new comp, the precomp layer).
fn nest(s: &mut Session, inner: effectcraft_project::ItemId, name: &str, start: f64, stretch: f64, in_out: (f64, f64)) -> (effectcraft_project::ItemId, u64) {
    s.execute("comp.new", json!({"name": name, "width": 320, "height": 180, "frameRate": 25, "duration": 12.0})).unwrap();
    let outer = s.active_comp_id().unwrap();
    let l = s.execute("layer.addItem", json!({"comp": outer.0, "item": inner.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    let p = std::sync::Arc::make_mut(&mut s.project);
    let nl = p.comp_mut(outer).unwrap().layer_mut(LayerId(l)).unwrap();
    nl.start_time = Tick::from_seconds_f64(start);
    nl.stretch = stretch;
    (nl.in_point, nl.out_point) = (Tick::from_seconds_f64(in_out.0), Tick::from_seconds_f64(in_out.1));
    (outer, l)
}

#[test]
fn a_peak_inside_a_stretched_offset_precomp_is_found_at_the_parent_frame() {
    // The move peaks at 1.5 s inside "Talk"; Main shows Talk from 3 s at 200 %: 3 + 1.5 × 2 = 6 s.
    let (mut s, l) = comp();
    let inner = s.active_comp_id().unwrap();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    let (main, pre) = nest(&mut s, inner, "Main", 3.0, 200.0, (3.0, 11.0));
    let deep = lint::speed_peaks_deep(&s.project, main, 0.0, 12.0);
    assert_eq!(deep.peaks.len(), 1, "{deep:#?}");
    let pk = &deep.peaks[0];
    assert!((pk.time - 6.0).abs() < 0.004, "{pk:#?}");
    assert_eq!((pk.layer, pk.comp, pk.via.len(), pk.via[0].layer), (LayerId(l), Some(inner), 1, LayerId(pre)));
    // The comp's own layers only: nothing.
    assert!(lint::speed_peaks(s.project.comp(main).unwrap(), 0.0, 12.0).is_empty());

    // Accent = vowel 6.12 − 1 frame = 6.08: the peak is 2 frames early.
    s.execute("speech.import", json!({"map": map(6.0, 6.12)})).unwrap();
    let r = s.execute("render.lint", json!({"start": 0, "end": 12, "rules": ["off-accent"]})).unwrap();
    let found = r["problems"].as_array().unwrap();
    assert_eq!(found.len(), 1, "{r:#}");
    assert_eq!(found[0]["layer"].as_u64(), Some(pre), "on the precomp layer in Main");
    let m = found[0]["message"].as_str().unwrap();
    assert!(m.contains("Logo") && m.contains("\"Talk\"") && m.contains("2 frames before") && m.contains(&format!("comp: {}, layer: {l}", inner.0)), "{m}");

    let a = s.execute("speech.accents", json!({})).unwrap();
    let p = &a["list"][0];
    assert_eq!((p["comp"].as_u64(), p["layer"].as_u64(), p["via"][0].as_u64()), (Some(inner.0), Some(l), Some(pre)), "{a:#}");
    assert_eq!((p["frame"].as_f64().map(f64::round), p["offsetFrames"].as_f64()), (Some(150.0), Some(-2.0)), "{p:#}");
    // Filtering by the precomp layer keeps what is inside it.
    assert_eq!(s.execute("speech.accents", json!({"layer": pre})).unwrap()["peaks"], 1);
    // The finding's alignPeak params land the nested peak on the accent.
    let mut params = p["alignPeak"].clone();
    params["graph"] = json!("keep");
    s.execute_checked("keys.alignPeak", params).unwrap();
    let a = s.execute("speech.accents", json!({})).unwrap();
    assert_eq!(a["hits"], 1, "{a:#}");
}

#[test]
fn a_peak_two_precomps_deep_maps_through_each_start_and_stretch() {
    // Talk (peak 1.5 s) in Middle from 1 s at 100 % → 2.5 s; Middle in Main from 2 s at 50 % → 3.25 s.
    let (mut s, l) = comp();
    let inner = s.active_comp_id().unwrap();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    let (middle, _) = nest(&mut s, inner, "Middle", 1.0, 100.0, (1.0, 5.0));
    let (main, top) = nest(&mut s, middle, "Main", 2.0, 50.0, (2.0, 8.0));
    let deep = lint::speed_peaks_deep(&s.project, main, 0.0, 12.0);
    assert_eq!(deep.peaks.len(), 1, "{deep:#?}");
    assert!((deep.peaks[0].time - 3.25).abs() < 0.004, "{deep:#?}");
    assert_eq!((deep.peaks[0].comp, deep.peaks[0].via.len(), deep.peaks[0].via[0].layer), (Some(inner), 2, LayerId(top)));
    // A window that ends before it: none.
    assert!(lint::speed_peaks_deep(&s.project, main, 0.0, 3.0).peaks.is_empty());
}

#[test]
fn trimmed_away_nested_peaks_are_dropped() {
    let (mut s, l) = comp();
    let inner = s.active_comp_id().unwrap();
    scale_keys(&mut s, l, 1.0, 2.0, false);
    // Main shows Talk (peak at 3 + 1.5 = 4.5 s) only from 5 s: the peak is trimmed away.
    let (main, _) = nest(&mut s, inner, "Main", 3.0, 100.0, (5.0, 7.0));
    assert!(lint::speed_peaks_deep(&s.project, main, 0.0, 12.0).peaks.is_empty());
    // Out before it: dropped too.
    let (main, _) = nest(&mut s, inner, "Main 2", 3.0, 100.0, (3.0, 4.2));
    assert!(lint::speed_peaks_deep(&s.project, main, 0.0, 12.0).peaks.is_empty());
    // Two deep, trimmed in the middle: Middle's precomp layer ends before the peak (2.5 s there).
    let (middle, _) = nest(&mut s, inner, "Middle", 1.0, 100.0, (1.0, 2.2));
    let (main, _) = nest(&mut s, middle, "Main 3", 0.0, 100.0, (0.0, 10.0));
    assert!(lint::speed_peaks_deep(&s.project, main, 0.0, 12.0).peaks.is_empty());
    // Shown: found.
    let (main, _) = nest(&mut s, inner, "Main 4", 3.0, 100.0, (4.0, 5.0));
    assert_eq!(lint::speed_peaks_deep(&s.project, main, 0.0, 12.0).peaks.len(), 1);
}
