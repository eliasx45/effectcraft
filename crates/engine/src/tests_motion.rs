//! `comp.motion` / `layer.motion`: motion summaries of keyframed layers on tiny comps.

use serde_json::{Value, json};

use crate::Session;

/// A 320×180 comp, 24 fps, 5 s.
fn comp() -> Session {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "M", "width": 320, "height": 180, "frameRate": 24, "duration": 5})).unwrap();
    s
}

fn solid(s: &mut Session, name: &str, w: u32, h: u32) -> u64 {
    s.execute("layer.newSolid", json!({"name": name, "width": w, "height": h, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap()
}

fn key(s: &mut Session, layer: u64, path: &str, time: f64, value: Value) {
    s.execute("prop.addKey", json!({"layer": layer, "path": path, "time": time, "value": value})).unwrap();
}

fn motion_of(s: &mut Session, layer: u64) -> Value {
    let r = s.execute("layer.motion", json!({"layer": layer})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 1, "{r}");
    r["layers"][0].clone()
}

fn segs<'a>(l: &'a Value, kind: &str) -> Vec<&'a Value> {
    l["segments"].as_array().unwrap().iter().filter(|g| g["kind"] == kind).collect()
}

fn near(v: &Value, want: f64, tol: f64) {
    let got = v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"));
    assert!((got - want).abs() <= tol, "got {got}, want {want}");
}

#[test]
fn fly_in_from_the_left_then_hold() {
    let mut s = comp();
    let l = solid(&mut s, "Logo", 80, 44);
    key(&mut s, l, "transform/position", 0.0, json!([-60.0, 90.0]));
    key(&mut s, l, "transform/position", 1.0, json!([160.0, 90.0]));
    let m = motion_of(&mut s, l);
    let enter = segs(&m, "enter");
    assert_eq!(enter.len(), 1, "{m}");
    assert_eq!(enter[0]["edge"], "left");
    assert_eq!(enter[0]["direction"], "right");
    near(&enter[0]["start"], 0.0, 1e-9);
    near(&enter[0]["end"], 1.0, 1e-9);
    assert_eq!(enter[0]["to"], json!([160.0, 90.0]));
    let hold = segs(&m, "hold");
    assert_eq!(hold.len(), 1, "{m}");
    near(&hold[0]["start"], 1.0, 1e-9);
    near(&hold[0]["end"], 5.0, 1e-9);
    assert!(segs(&m, "move").is_empty() && segs(&m, "exit").is_empty(), "{m}");
    // Off screen until its right edge crosses x = 0 (the fourth frame).
    let off = m["offscreen"].as_array().unwrap();
    assert_eq!(off.len(), 1, "{m}");
    near(&off[0]["start"], 0.0, 1e-9);
    near(&off[0]["end"], 3.0 / 24.0, 1e-3);
    let summary = m["summary"].as_str().unwrap();
    assert!(summary.contains("from the left 0–1s") && summary.contains("holds 1–5s"), "{summary}");
    // A key between grid samples (every 0.2 s here) is sampled exactly: the enter ends and the
    // hold starts on it.
    let o = solid(&mut s, "Odd", 80, 44);
    key(&mut s, o, "transform/position", 0.0, json!([-60.0, 90.0]));
    key(&mut s, o, "transform/position", 0.5, json!([160.0, 90.0]));
    let m = s.execute("layer.motion", json!({"layer": o, "step": 0.2})).unwrap()["layers"][0].clone();
    assert_eq!(segs(&m, "enter")[0]["end"], 0.5, "{m}");
    assert_eq!(segs(&m, "hold")[0]["start"], 0.5, "{m}");
}

#[test]
fn fade_scale_and_rotate() {
    let mut s = comp();
    let l = solid(&mut s, "Card", 60, 60);
    key(&mut s, l, "transform/opacity", 4.0, json!(100.0));
    key(&mut s, l, "transform/opacity", 5.0, json!(0.0));
    key(&mut s, l, "transform/scale", 0.0, json!([50.0, 50.0]));
    key(&mut s, l, "transform/scale", 2.0, json!([150.0, 150.0]));
    key(&mut s, l, "transform/rotation", 2.0, json!(0.0));
    key(&mut s, l, "transform/rotation", 3.0, json!(180.0));
    let m = motion_of(&mut s, l);
    let fade = segs(&m, "fade-out");
    assert_eq!(fade.len(), 1, "{m}");
    near(&fade[0]["start"], 4.0, 1e-9);
    near(&fade[0]["end"], 5.0, 1e-9);
    assert_eq!((fade[0]["from"].clone(), fade[0]["to"].clone()), (json!(100.0), json!(0.0)));
    let scale = segs(&m, "scale");
    assert_eq!(scale.len(), 1, "{m}");
    assert_eq!((scale[0]["from"].clone(), scale[0]["to"].clone(), scale[0]["end"].clone()), (json!(50.0), json!(150.0), json!(2.0)));
    let rot = segs(&m, "rotate");
    assert_eq!(rot.len(), 1, "{m}");
    assert_eq!((rot[0]["start"].clone(), rot[0]["end"].clone(), rot[0]["to"].clone()), (json!(2.0), json!(3.0), json!(180.0)));
    // A centred layer that only scales and rotates doesn't move.
    assert!(segs(&m, "move").is_empty(), "{m}");
    // Holds between the changes: 3–4 s.
    let hold = segs(&m, "hold");
    assert_eq!(hold.len(), 1, "{m}");
    assert_eq!((hold[0]["start"].clone(), hold[0]["end"].clone()), (json!(3.0), json!(4.0)));
    let summary = m["summary"].as_str().unwrap();
    for part in ["scales up 50→150% 0–2s", "rotates 180° clockwise 2–3s", "holds 3–4s", "fades out 4–5s"] {
        assert!(summary.contains(part), "{part} not in {summary}");
    }
    // A fade-in is the other way round.
    let f = solid(&mut s, "In", 20, 20);
    key(&mut s, f, "transform/opacity", 0.0, json!(0.0));
    key(&mut s, f, "transform/opacity", 0.5, json!(100.0));
    let m = motion_of(&mut s, f);
    assert_eq!(segs(&m, "fade-in").len(), 1, "{m}");
    assert!(m["summary"].as_str().unwrap().contains("fades in 0–0.5s"), "{m}");
}

#[test]
fn exit_move_and_offscreen() {
    let mut s = comp();
    let l = solid(&mut s, "Box", 40, 40);
    // Straight motion paths: with After Effects' auto-Bezier tangents ((next - previous) / 6)
    // the path between the two equal keys at 1 s and 3 s would loop instead of holding.
    s.prefs.general.default_spatial_linear = true;
    // Moves up within the frame 0–1 s, then leaves through the right edge 3–4 s and stays out.
    key(&mut s, l, "transform/position", 0.0, json!([160.0, 120.0]));
    key(&mut s, l, "transform/position", 1.0, json!([160.0, 60.0]));
    key(&mut s, l, "transform/position", 3.0, json!([160.0, 60.0]));
    key(&mut s, l, "transform/position", 4.0, json!([400.0, 60.0]));
    let m = motion_of(&mut s, l);
    let mv = segs(&m, "move");
    assert_eq!(mv.len(), 1, "{m}");
    assert_eq!(mv[0]["direction"], "up");
    assert_eq!((mv[0]["from"].clone(), mv[0]["to"].clone()), (json!([160.0, 120.0]), json!([160.0, 60.0])));
    let exit = segs(&m, "exit");
    assert_eq!(exit.len(), 1, "{m}");
    assert_eq!(exit[0]["edge"], "right");
    near(&exit[0]["end"], 4.0, 1e-9);
    let off = m["offscreen"].as_array().unwrap();
    assert_eq!(off.len(), 1, "{m}");
    near(&off[0]["end"], 5.0, 1e-9);
    assert!(off[0]["start"].as_f64().unwrap() > 3.5 && off[0]["start"].as_f64().unwrap() <= 4.0, "{m}");
    let summary = m["summary"].as_str().unwrap();
    assert!(summary.contains("moves up 0–1s") && summary.contains("flies out to the right") && summary.contains("stays off screen 4–5s"), "{summary}");
    // Flying across splits into enter, move and exit.
    let x = solid(&mut s, "Across", 40, 40);
    key(&mut s, x, "transform/position", 0.0, json!([-40.0, 90.0]));
    key(&mut s, x, "transform/position", 2.0, json!([360.0, 90.0]));
    let m = motion_of(&mut s, x);
    let kinds: Vec<&str> = m["segments"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["enter", "move", "exit", "hold"], "{m}");
    assert_eq!((segs(&m, "enter")[0]["edge"].clone(), segs(&m, "exit")[0]["edge"].clone()), (json!("left"), json!("right")));
}

#[test]
fn overlap_between_two_moving_layers() {
    let mut s = comp();
    let bg = solid(&mut s, "BG", 320, 180);
    let a = solid(&mut s, "A", 60, 60);
    let b = solid(&mut s, "B", 60, 60);
    key(&mut s, a, "transform/position", 0.0, json!([60.0, 90.0]));
    key(&mut s, a, "transform/position", 2.0, json!([260.0, 90.0]));
    key(&mut s, b, "transform/position", 0.0, json!([260.0, 90.0]));
    key(&mut s, b, "transform/position", 2.0, json!([60.0, 90.0]));
    let r = s.execute("comp.motion", json!({})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 3, "{r}");
    assert_eq!(r["compName"], "M");
    assert_eq!(r["truncated"], false);
    // The full-frame background overlaps nothing; A and B cross around 1 s.
    let o = r["overlaps"].as_array().unwrap();
    assert_eq!(o.len(), 1, "{r}");
    let ids = [o[0]["a"].as_u64().unwrap(), o[0]["b"].as_u64().unwrap()];
    assert!(ids.contains(&a) && ids.contains(&b) && !ids.contains(&bg), "{r}");
    near(&o[0]["start"], 0.75, 1e-3);
    near(&o[0]["end"], 31.0 / 24.0, 1e-3);
    near(&o[0]["maxFraction"], 1.0, 1e-6);
    assert!(o[0]["aName"].is_string() && o[0]["bName"].is_string());
    // The background holds the whole time.
    let bgm = r["layers"].as_array().unwrap().iter().find(|l| l["layer"] == bg).unwrap().clone();
    assert_eq!(bgm["summary"], "BG holds the whole time.");
    // layer.motion: one layer, so no overlaps.
    let r = s.execute("layer.motion", json!({"layer": "A"})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 1);
    assert_eq!(r["overlaps"], json!([]));
    assert_eq!(segs(&r["layers"][0], "move")[0]["direction"], "right");
    // A range: start / end / step are honoured.
    let r = s.execute("comp.motion", json!({"layers": ["A", "#1"], "start": 1, "end": 2, "step": 0.25})).unwrap();
    assert_eq!((r["start"].clone(), r["end"].clone(), r["step"].clone()), (json!(1.0), json!(2.0), json!(0.25)));
    assert_eq!(r["layers"].as_array().unwrap().len(), 2);
}

#[test]
fn static_comp_holds_and_skips_layers_without_pictures() {
    let mut s = comp();
    solid(&mut s, "Background", 320, 180);
    solid(&mut s, "Title", 100, 30);
    let n = s.execute("layer.newNull", json!({"name": "Ctrl"})).unwrap()["layer"].as_u64().unwrap();
    let r = s.execute("comp.motion", json!({})).unwrap();
    for l in r["layers"].as_array().unwrap() {
        if l["layer"] == n {
            assert_eq!(l["segments"], json!([]));
            assert!(l["summary"].as_str().unwrap().contains("no picture"), "{l}");
            continue;
        }
        let kinds: Vec<&str> = l["segments"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["hold"], "{l}");
        assert!(l["summary"].as_str().unwrap().ends_with("holds the whole time."), "{l}");
        assert_eq!(l["offscreen"], json!([]));
    }
    assert_eq!(r["overlaps"], json!([]), "{r}");
    // Default range: the whole comp, one frame per sample.
    assert_eq!((r["start"].clone(), r["end"].clone()), (json!(0.0), json!(5.0)));
    near(&r["step"], 1.0 / 24.0, 1e-4);
    // A layer that starts later "appears".
    let t = solid(&mut s, "Late", 20, 20);
    s.execute("layer.timing", json!({"layers": [t], "in": 2.0, "out": 4.0})).unwrap();
    let m = motion_of(&mut s, t);
    let summary = m["summary"].as_str().unwrap();
    assert!(summary.contains("appears at 2s") && summary.contains("holds 2–4s") && summary.contains("ends at 4s"), "{summary}");
}

#[test]
fn samples_are_capped_and_bad_input_is_an_error() {
    let mut s = comp();
    for i in 0..5 {
        solid(&mut s, &format!("L{i}"), 10, 10);
    }
    // 5 layers × 5001 samples is over the 20 000-sample budget: the range is cut short.
    let r = s.execute("comp.motion", json!({"step": 0.001})).unwrap();
    assert_eq!(r["truncated"], true, "{}", r["notes"]);
    let end = r["end"].as_f64().unwrap();
    assert!(end < 5.0 && end > 3.9, "{end}");
    assert!(r["notes"].as_array().is_some_and(|n| !n.is_empty()));
    // A long range at the default step stays within 600 samples per layer.
    let r = s.execute("comp.motion", json!({"layers": ["L0"]})).unwrap();
    assert_eq!(r["truncated"], false);
    assert!(s.execute("comp.motion", json!({"start": 3, "end": 2})).is_err());
    assert!(s.execute("comp.motion", json!({"step": 0})).is_err());
    assert!(s.execute("comp.motion", json!({"layers": ["Nope"]})).is_err());
    assert!(s.execute("layer.motion", json!({})).is_err());
    assert!(s.execute_checked("comp.motion", json!({"bogus": 1})).is_err());
}

#[test]
fn default_step_is_coarser_for_long_comps() {
    use effectcraft_time::Tick;
    let frame = Tick::from_seconds_f64(1.0 / 24.0);
    assert_eq!(crate::motion::default_step(Tick::from_seconds_f64(5.0), frame), frame);
    let long = crate::motion::default_step(Tick::from_seconds_f64(60.0), frame);
    assert!((long.seconds() - 0.1).abs() < 1e-6, "{}", long.seconds());
    assert_eq!(crate::motion::fmt_secs(2.5), "2.5");
    assert_eq!(crate::motion::fmt_secs(1.0), "1");
}

/// Lab G1 (20-08 "10 Everything Is Copy"): a rig null carries the keys; get_motion said "null
/// layer: no picture" for it and never told the agent the child's move came from the null.
#[test]
fn a_rig_null_is_described_by_what_it_moves_and_children_name_it() {
    let mut s = comp();
    let rig = s.execute("layer.newNull", json!({"name": "Row RIG"})).unwrap()["layer"].as_u64().unwrap();
    let card = solid(&mut s, "Row card", 60, 30);
    let text = solid(&mut s, "Row text", 40, 10);
    s.execute("layer.setParent", json!({"layers": [card, text], "parent": rig})).unwrap();
    key(&mut s, rig, "transform/position", 1.0, json!([-200.0, 90.0]));
    key(&mut s, rig, "transform/position", 2.0, json!([160.0, 90.0]));
    let n = motion_of(&mut s, rig);
    let summary = n["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("Row RIG is a null that moves its 2 layers (") && summary.contains("Row card") && summary.contains("(its keys are on this layer)"),
        "{summary}"
    );
    assert!(summary.contains("flies in from the left"), "{summary}");
    assert!(!segs(&n, "enter").is_empty(), "{n:#}");
    assert_eq!(n["moves"].as_array().unwrap().len(), 2, "{n:#}");
    assert!(n["describedBy"]["layer"] == card || n["describedBy"]["layer"] == text, "{n:#}");
    // The child names the rig as the layer to key.
    let c = motion_of(&mut s, card);
    assert_eq!(c["movedBy"][0]["layer"], rig, "{c:#}");
    let cs = c["summary"].as_str().unwrap();
    assert!(cs.contains("flies in from the left") && cs.contains("key “Row RIG”"), "{cs}");
    // A null without keys still says it has no picture, and that it parents layers.
    let still = s.execute("layer.newNull", json!({"name": "Still"})).unwrap()["layer"].as_u64().unwrap();
    let t2 = solid(&mut s, "Kid", 20, 20);
    s.execute("layer.setParent", json!({"layers": [t2], "parent": still})).unwrap();
    let m = motion_of(&mut s, still);
    assert!(m["summary"].as_str().unwrap().contains("no picture") && m["summary"].as_str().unwrap().contains("parents 1 layer"), "{m:#}");
    assert!(motion_of(&mut s, t2).get("movedBy").is_none());
}

/// Probe on a saved project (not part of the suite): `MOTION_PROJECT=… MOTION_COMP=… MOTION_LAYERS=a|b cargo test -p
/// effectcraft-engine --lib tests_motion::real_rig -- --ignored --nocapture`.
#[test]
#[ignore]
fn real_rig() {
    let (Ok(path), Ok(comp)) = (std::env::var("MOTION_PROJECT"), std::env::var("MOTION_COMP")) else { return };
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    let names = std::env::var("MOTION_LAYERS").unwrap_or_default();
    for n in names.split('|') {
        let r = s.execute("layer.motion", json!({"comp": comp, "layer": n})).unwrap();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"summary": r["layers"][0]["summary"], "movedBy": r["layers"][0]["movedBy"], "describedBy": r["layers"][0]["describedBy"]})
            )
            .unwrap()
        );
    }
}
