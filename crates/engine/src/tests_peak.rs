//! The speed-peak solver `keys.alignPeak`: each of Devin's graphs, each mode, precomps,
//! spatial and multi-dimensional properties, speech targets ("content" at frame 1281 in 10-10
//! MAIN), one undo step.

use effectcraft_project::{LayerId, ItemId};
use serde_json::{Value, json};

use crate::Session;
use crate::commands::peak::{GRAPHS, graph_peak};

/// A 320×180 comp at `fps`, `dur` s long, with a solid "Card"; returns (session, comp, layer).
fn comp(fps: f64, dur: f64) -> (Session, ItemId, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Talk", "width": 320, "height": 180, "frameRate": fps, "duration": dur})).unwrap();
    let cid = s.active_comp_id().unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Card", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    (s, cid, l)
}

fn key(s: &mut Session, l: u64, path: &str, t: f64, v: Value) {
    s.execute("prop.addKey", json!({"layer": l, "path": path, "value": v, "time": t, "timeBase": "comp"})).unwrap();
}

/// Scale 100 → 160 between `a` and `b` (comp s).
fn scale_move(s: &mut Session, l: u64, a: f64, b: f64) {
    key(s, l, "transform/scale", a, json!([100.0, 100.0, 100.0]));
    key(s, l, "transform/scale", b, json!([160.0, 130.0, 100.0]));
}

/// Easy Ease on both keys of `path`.
fn easy(s: &mut Session, cid: ItemId, l: u64, path: &str) {
    let pr = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap();
    let keys: Vec<Value> = pr.keys.iter().map(|k| json!({"layer": l, "prop": pr.uid, "time": k.time.seconds()})).collect();
    s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
    s.execute_checked("keys.easyEase", json!({})).unwrap();
}

fn key_times(s: &Session, cid: ItemId, l: u64, path: &str) -> Vec<f64> {
    s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap().keys.iter().map(|k| k.time.seconds()).collect()
}

fn values(s: &Session, cid: ItemId, l: u64, path: &str) -> Vec<Vec<f64>> {
    s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap().keys.iter().map(|k| k.value.components()).collect()
}

fn align(s: &mut Session, p: Value) -> Value {
    s.execute_checked("keys.alignPeak", p).unwrap()
}

fn first(r: &Value) -> &Value {
    &r["results"][0]
}

fn after(r: &Value) -> f64 {
    first(r)["peakAfter"].as_f64().unwrap()
}

#[test]
fn devins_graphs_peak_in_order_start_to_end() {
    let peaks: Vec<f64> = GRAPHS.iter().map(|g| graph_peak(g).unwrap()).collect();
    assert!(peaks.windows(2).all(|w| w[0] < w[1]), "{peaks:?}");
    assert!(peaks[0] < 0.02, "Fast In peaks at the start: {peaks:?}");
    assert!((peaks[1] - 0.26).abs() < 0.02, "Quick launch peaks early: {peaks:?}");
    assert!((peaks[2] - 0.5).abs() < 0.02, "Smooth emphasis peaks in the middle: {peaks:?}");
    assert!((peaks[3] - 0.6).abs() < 0.02, "Slow launch peaks late: {peaks:?}");
    assert!(peaks[4] > 0.98, "Fast out peaks at the end: {peaks:?}");
    let mut s = Session::default();
    let r = s.execute("keys.peakGraphs", json!({})).unwrap();
    assert_eq!(r["graphs"].as_array().unwrap().len(), 5);
    assert_eq!(r["graphs"][2]["name"], "Smooth emphasis");
}

#[test]
fn shift_lands_every_graph_on_the_frame_and_keeps_duration_and_values() {
    for g in GRAPHS {
        let (mut s, cid, l) = comp(25.0, 6.0);
        scale_move(&mut s, l, 1.0, 1.8);
        let vals = values(&s, cid, l, "transform/scale");
        let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 70, "graph": g.id}));
        assert!((after(&r) - 70.0).abs() <= 0.5, "{}: {r:#}", g.id);
        assert_eq!(first(&r)["graph"], g.id);
        let t = key_times(&s, cid, l, "transform/scale");
        assert!((t[1] - t[0] - 0.8).abs() < 1e-6, "{}: duration kept: {t:?}", g.id);
        // Whole frames.
        assert!(t.iter().all(|x| (x * 25.0 - (x * 25.0).round()).abs() < 1e-6), "{t:?}");
        assert_eq!(values(&s, cid, l, "transform/scale"), vals, "values kept");
        // The lint's finder agrees.
        let peaks = crate::lint::speed_peaks(s.project.comp(cid).unwrap(), 0.0, 6.0);
        assert!((peaks[0].time * 25.0 - 70.0).abs() <= 0.5, "{}: {peaks:?}", g.id);
    }
}

#[test]
fn the_graphs_influences_and_speeds_are_set_on_the_pair() {
    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 30, "graph": "Fast In"}));
    let keys = &s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/scale").unwrap().keys;
    let out = keys[0].out_ease.as_slice();
    assert_eq!(out.len(), 3, "one ease per dimension");
    assert!((out[0].influence - 0.005_926_6).abs() < 1e-6);
    // Normalised 23.75 × Δvalue / duration: x moves 60 in 1 s, y 30, z not at all.
    assert!((out[0].speed - 23.752_73 * 60.0).abs() < 0.01, "{out:?}");
    assert!((out[1].speed - 23.752_73 * 30.0).abs() < 0.01, "{out:?}");
    assert_eq!(out[2].speed, 0.0);
    assert!((keys[1].in_ease.as_slice()[0].influence - 0.929_21).abs() < 1e-4);
}

#[test]
fn without_a_graph_the_one_peaking_where_the_target_falls_is_picked() {
    for (frame, want) in [(26, "fast-in"), (32, "quick-launch"), (37, "smooth-emphasis"), (39, "slow-launch"), (50, "fast-out")] {
        let (mut s, _, l) = comp(25.0, 6.0);
        scale_move(&mut s, l, 1.0, 2.0);
        let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": frame}));
        assert_eq!(first(&r)["graph"], want, "frame {frame}: {r:#}");
        assert!((after(&r) - frame as f64).abs() <= 0.5, "{r:#}");
    }
}

#[test]
fn stretch_in_keeps_the_second_key_and_stretch_out_the_first() {
    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 45, "mode": "stretch-in", "graph": "smooth-emphasis"}));
    let t = key_times(&s, cid, l, "transform/scale");
    assert!((t[1] - 2.0).abs() < 1e-9, "{t:?}");
    assert!((t[0] - 1.6).abs() < 0.05, "{t:?}");
    assert!((after(&r) - 45.0).abs() <= 0.5, "{r:#}");

    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 33, "mode": "stretch-out", "graph": "slow-launch"}));
    let t = key_times(&s, cid, l, "transform/scale");
    assert!((t[0] - 1.0).abs() < 1e-9, "{t:?}");
    assert!((t[1] - 1.5).abs() < 0.06, "{t:?}");
    assert!((after(&r) - 33.0).abs() <= 0.5, "{r:#}");
    // The graph keeps its shape: the peak sits at the same share of the move.
    assert!((first(&r)["peakShare"].as_f64().unwrap() - 0.6).abs() < 0.02, "{r:#}");

    // A peak on the kept key can't move.
    let (mut s, _, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    assert!(s.execute_checked("keys.alignPeak", json!({"layer": l, "path": "transform/scale", "frame": 40, "mode": "stretch-in", "graph": "fast-out"})).is_err());
}

#[test]
fn ease_mode_only_changes_influences() {
    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 41, "mode": "ease", "graph": "smooth-emphasis"}));
    assert_eq!(key_times(&s, cid, l, "transform/scale"), vec![1.0, 2.0]);
    assert!((after(&r) - 41.0).abs() <= 0.5, "{r:#}");
    let inf = &first(&r)["influence"];
    assert!(inf["out"].as_f64().unwrap() > 70.3 && inf["in"].as_f64().unwrap() < 70.0, "later peak: more out influence, less in: {r:#}");
    // Out of the family's reach (the keys stay): an error that names the reachable frames.
    let e = s.execute_checked("keys.alignPeak", json!({"layer": l, "path": "transform/scale", "frame": 60, "mode": "ease"})).unwrap_err().to_string();
    assert!(e.contains("can only peak between"), "{e}");
}

#[test]
fn spatial_position_moves_along_its_path() {
    let (mut s, cid, l) = comp(30.0, 6.0);
    key(&mut s, l, "transform/position", 1.0, json!([40.0, 90.0, 0.0]));
    key(&mut s, l, "transform/position", 2.0, json!([280.0, 60.0, 0.0]));
    assert!(s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap().spatial);
    let r = align(&mut s, json!({"layer": l, "path": "transform/position", "frame": 72, "graph": "quick-launch"}));
    assert!((after(&r) - 72.0).abs() <= 0.5, "{r:#}");
    let keys = &s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap().keys;
    assert_eq!(keys[0].out_ease.len(), 1, "spatial: one ease");
    let t = key_times(&s, cid, l, "transform/position");
    assert!((t[1] - t[0] - 1.0).abs() < 1e-6, "{t:?}");
}

#[test]
fn several_properties_and_the_selected_pair() {
    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 1.6);
    key(&mut s, l, "transform/rotation", 1.0, json!(0.0));
    key(&mut s, l, "transform/rotation", 1.6, json!(90.0));
    let r = align(&mut s, json!({"layer": l, "paths": ["transform/scale", "transform/rotation"], "frame": 50, "graph": "smooth-emphasis"}));
    assert_eq!(r["results"].as_array().unwrap().len(), 2);
    assert!(r["results"].as_array().unwrap().iter().all(|x| (x["peakAfter"].as_f64().unwrap() - 50.0).abs() <= 0.5), "{r:#}");

    // The selected keyframe pair, no path.
    let uid = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/rotation").unwrap().uid;
    let t = key_times(&s, cid, l, "transform/rotation");
    let keys: Vec<Value> = t.iter().map(|t| json!({"layer": l, "prop": uid, "time": t})).collect();
    s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
    let r = align(&mut s, json!({"frame": 60}));
    assert_eq!(r["results"].as_array().unwrap().len(), 1);
    assert_eq!(first(&r)["property"], "Rotation");
    assert!((after(&r) - 60.0).abs() <= 0.5, "{r:#}");
    // The selection follows the moved keys.
    let now = key_times(&s, cid, l, "transform/rotation");
    assert!(s.state.selected_keys.iter().all(|k| now.contains(&k.time.seconds())), "{:?}", s.state.selected_keys);
}

#[test]
fn one_undo_step_restores_the_move() {
    let (mut s, cid, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    let before = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/scale").unwrap().keys.clone();
    align(&mut s, json!({"layer": l, "path": "transform/scale", "frame": 80, "graph": "fast-in"}));
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/scale").unwrap().keys, before);
}

#[test]
fn shift_never_crosses_a_neighbouring_key() {
    let (mut s, _, l) = comp(25.0, 8.0);
    key(&mut s, l, "transform/rotation", 1.0, json!(0.0));
    key(&mut s, l, "transform/rotation", 2.0, json!(90.0));
    key(&mut s, l, "transform/rotation", 2.5, json!(0.0));
    let e = s.execute_checked("keys.alignPeak", json!({"layer": l, "path": "transform/rotation", "key": 0, "frame": 60, "graph": "smooth-emphasis"})).unwrap_err().to_string();
    assert!(e.contains("crossing"), "{e}");
    // A linear move with the graph kept has no peak.
    let e = s.execute_checked("keys.alignPeak", json!({"layer": l, "path": "transform/rotation", "key": 0, "frame": 30, "graph": "keep"})).unwrap_err().to_string();
    assert!(e.contains("linear"), "{e}");
}

#[test]
fn keys_inside_a_stretched_offset_precomp_land_on_the_outer_frame() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Inner", "width": 64, "height": 64, "frameRate": 25, "duration": 4})).unwrap();
    let inner = s.active_comp_id().unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Dot", "width": 10, "height": 10, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap();
    scale_move(&mut s, l, 1.0, 2.0);
    s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 25, "duration": 12})).unwrap();
    let main = s.active_comp_id().unwrap();
    let nest = s.execute("layer.addItem", json!({"comp": main.0, "item": inner.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    {
        let p = std::sync::Arc::make_mut(&mut s.project);
        let nl = p.comp_mut(main).unwrap().layer_mut(LayerId(nest)).unwrap();
        nl.start_time = effectcraft_time::Tick::from_seconds_f64(3.0);
        nl.stretch = 200.0;
        nl.in_point = effectcraft_time::Tick::from_seconds_f64(3.0);
        nl.out_point = effectcraft_time::Tick::from_seconds_f64(11.0);
    }
    // Main frame 131 (5.24 s) = inner (5.24 − 3) / 2 = 1.12 s.
    let r = align(&mut s, json!({"comp": inner.0, "layer": l, "path": "transform/scale", "targetComp": main.0, "frame": 131, "graph": "smooth-emphasis"}));
    assert_eq!(r["via"][0]["layer"], nest);
    assert!((after(&r) - 131.0).abs() <= 0.5, "{r:#}");
    assert_eq!(r["frameScale"], 0.5);
    // The keys moved in the inner comp, duration kept.
    let t = key_times(&s, inner, l, "transform/scale");
    assert!((t[1] - t[0] - 1.0).abs() < 1e-6, "{t:?}");
    assert!((t[0] + 0.5 - 1.12).abs() < 0.03, "{t:?}");
    // Not nested in the target: an error.
    s.execute("comp.new", json!({"name": "Other", "width": 64, "height": 64, "frameRate": 25, "duration": 4})).unwrap();
    let other = s.active_comp_id().unwrap();
    assert!(s.execute_checked("keys.alignPeak", json!({"comp": inner.0, "layer": l, "path": "transform/scale", "targetComp": other.0, "frame": 10})).is_err());
}

/// 10-10 MAIN: "content" (onset 42.665, vowel 42.780 at 29.97 fps). Elias's hand-timed speed
/// peak is frame 1281; the speech map's accent (vowel − 1 frame) is frame 1281 too.
fn content_map() -> Value {
    json!({"engine": "test", "words": [
        {"word": "what", "start": 41.9, "end": 42.1, "syllables": [{"t": 41.9, "vowel": 41.95, "stress": 1}]},
        {"word": "content,", "start": 42.665, "end": 43.2, "syllables": [
            {"t": 42.665, "vowel": 42.780, "stress": 1},
            {"t": 42.95, "vowel": 43.0, "stress": 0}
        ]}
    ]})
}

#[test]
fn content_lands_on_frame_1281_in_10_10_main() {
    let (mut s, cid, l) = comp(29.97, 60.0);
    s.execute("speech.import", json!({"map": content_map()})).unwrap();
    // A pop-in that peaks too early (frame ~1268).
    scale_move(&mut s, l, 42.0, 42.6);
    easy(&mut s, cid, l, "transform/scale");
    let r = align(&mut s, json!({"layer": l, "path": "transform/scale", "speech": "content", "graph": "quick-launch"}));
    assert_eq!(r["target"]["speech"]["word"], "content,");
    assert_eq!(r["target"]["frame"].as_f64().unwrap().round(), 1281.0, "{r:#}");
    let peak = after(&r);
    assert!((peak - r["target"]["frame"].as_f64().unwrap()).abs() <= 0.5, "{r:#}");
    assert!((peak - 1281.0).abs() <= 0.6, "{r:#}");
    assert!(first(&r)["peakBefore"].as_f64().unwrap() < 1275.0, "{r:#}");
    // The off-accent check now calls it a hit.
    let a = s.execute("speech.accents", json!({"layer": l})).unwrap();
    assert_eq!(a["hits"], 1, "{a:#}");
}

#[test]
fn speech_target_in_the_parent_comp_reaches_a_precomp() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "S2 04 Common", "width": 64, "height": 64, "frameRate": 29.97, "duration": 10})).unwrap();
    let inner = s.active_comp_id().unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Phone", "width": 10, "height": 10, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap();
    scale_move(&mut s, l, 2.0, 2.5);
    s.execute("comp.new", json!({"name": "10-10 MAIN", "width": 64, "height": 64, "frameRate": 29.97, "duration": 60})).unwrap();
    let main = s.active_comp_id().unwrap();
    s.execute("speech.import", json!({"map": content_map()})).unwrap();
    s.execute("layer.addItem", json!({"comp": main.0, "item": inner.0, "time": 40.0})).unwrap();
    let first_key = key_times(&s, inner, l, "transform/scale")[0];
    // Without targetComp: the comp with the speech map that nests the precomp.
    let r = align(&mut s, json!({"comp": inner.0, "layer": l, "path": "transform/scale", "speech": {"word": "content"}, "mode": "stretch-out", "graph": "slow-launch"}));
    assert_eq!(r["targetComp"], main.0);
    assert!((after(&r) - r["target"]["frame"].as_f64().unwrap()).abs() <= 0.5, "{r:#}");
    assert_eq!(key_times(&s, inner, l, "transform/scale")[0], first_key, "stretch-out keeps the first key");
}

#[test]
fn bad_params_are_errors_not_crashes() {
    let (mut s, _, l) = comp(25.0, 6.0);
    scale_move(&mut s, l, 1.0, 2.0);
    for p in [
        json!({"layer": l, "path": "transform/scale"}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "time": 1.0}),
        json!({"layer": l, "path": "transform/scale", "frame": f64::MAX}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "graph": "wobble"}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "mode": "sideways"}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "key": 5}),
        json!({"layer": l, "path": "transform/scale", "speech": "content"}),
        json!({"layer": l, "path": "transform/opacity", "frame": 30}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "graph": {"out": {"influence": 500}}}),
        json!({"layer": l, "path": "transform/scale", "frame": 30, "via": [99]}),
        json!({"frame": 30}),
    ] {
        assert!(s.execute_checked("keys.alignPeak", p.clone()).is_err(), "{p}");
    }
}
