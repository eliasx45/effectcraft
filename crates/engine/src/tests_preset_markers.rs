//! Animation presets carry the source layer's markers (PRESET-MARKERS): Elias's text presets are
//! timed by an "In" and an "Out" marker that their expressions read
//! (`thisLayer.marker.key("In").time`). A marker near the keys moves with them (from the playhead);
//! one near the out point keeps its distance from the target layer's out point; durations stay;
//! applying again replaces same-comment markers instead of stacking them.

use effectcraft_keyframe::{Keyframe, Value as KV};
use effectcraft_project::{LayerId, Marker};
use effectcraft_time::Tick;
use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("ec-preset-markers-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn secs(s: f64) -> Tick {
    Tick::from_seconds_f64(s)
}

/// Sets layer `l`'s start, in and out points (comp seconds) and its markers (layer seconds).
fn shape_layer(s: &mut Session, l: u64, start: f64, out: f64, markers: Vec<Marker>) {
    let cid = s.active_comp_id().unwrap();
    s.edit("fixture", None, |proj, _| {
        let layer = crate::commands::layer_mut(proj, cid, LayerId(l))?;
        layer.start_time = secs(start);
        layer.in_point = secs(start);
        layer.out_point = secs(out);
        layer.markers = markers;
        Ok(())
    })
    .unwrap();
}

fn marker(t: f64, dur: f64, comment: &str) -> Marker {
    Marker { time: secs(t), duration: secs(dur), comment: comment.into(), ..Default::default() }
}

/// Layer `l`'s markers as (comp seconds, duration seconds, comment).
fn markers(s: &Session, l: u64) -> Vec<(f64, f64, String)> {
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    layer.markers.iter().map(|m| (layer.comp_time(m.time).seconds(), m.duration.seconds(), m.comment.clone())).collect()
}

fn close(got: &[(f64, f64, String)], want: &[(f64, f64, &str)]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g.0 - w.0).abs() < 1e-6 && (g.1 - w.1).abs() < 1e-6 && g.2 == w.2)
}

fn evaluated(s: &mut Session, l: u64, path: &str) -> f64 {
    let v = s.execute("prop.get", json!({"layer": l, "path": path})).unwrap();
    assert!(v.get("expressionError").is_none(), "{v:#}");
    v["evaluated"].as_f64().unwrap_or_else(|| panic!("{v:#}"))
}

#[test]
fn markers_round_trip_with_the_keys_and_the_out_point() {
    let path = temp_dir("roundtrip").join("Title In Out.ecpreset").to_string_lossy().to_string();
    let mut s = Session { expr: Some(std::sync::Arc::new(effectcraft_expr::Expressions)), ..Session::default() };
    s.execute("comp.new", json!({"name": "Workshop", "width": 320, "height": 180, "frameRate": 25, "duration": 6.0})).unwrap();

    // The source: 0–5 s; Opacity keyed 1 → 1.5 s; "In" at 1 s (0.5 s long, at the first key),
    // "Mid" at 2 s (nearer the keys), "Out" at 4.4 s (0.4 s long, 0.6 s before the out point).
    let a = s.execute("layer.newText", json!({"text": "Title"})).unwrap()["layer"].as_u64().unwrap();
    shape_layer(&mut s, a, 0.0, 5.0, vec![marker(1.0, 0.5, "In"), marker(2.0, 0.0, "Mid"), marker(4.4, 0.4, "Out")]);
    s.execute("prop.addKey", json!({"layer": a, "path": "transform/opacity", "time": 1.0, "value": 0})).unwrap();
    s.execute("prop.addKey", json!({"layer": a, "path": "transform/opacity", "time": 1.5, "value": 100})).unwrap();
    s.execute("prop.setExpression", json!({"layer": a, "path": "transform/rotation", "expression": "thisLayer.marker.key(\"In\").time"})).unwrap();
    s.execute(
        "prop.setExpression",
        json!({"layer": a, "path": "transform/anchor", "expression": "[thisLayer.marker.key(\"Out\").time, thisLayer.marker.key(\"Out\").duration]"}),
    )
    .unwrap();
    let layer = s.active_comp().unwrap().layer(LayerId(a)).unwrap();
    let uids: Vec<_> = ["transform/opacity", "transform/rotation", "transform/anchor"].iter().map(|p| (LayerId(a), layer.props.prop(p).unwrap().uid)).collect();
    s.state.selected_props = uids;
    let r = s.execute("anim.savePreset", json!({"path": path})).unwrap();
    assert_eq!(r["properties"], json!(3), "{r:#}");
    assert_eq!(r["markers"], json!(3), "{r:#}");
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let anchors: Vec<&str> = doc["markers"].as_array().unwrap().iter().map(|m| m["anchor"].as_str().unwrap()).collect();
    assert_eq!(anchors, ["keys", "keys", "out"], "{doc:#}");

    // A fresh text layer of another length (0.2–3.5 s, so its layer time is not comp time), with a
    // marker of its own; the playhead at 0.6 s.
    let b = s.execute("layer.newText", json!({"text": "Fresh"})).unwrap()["layer"].as_u64().unwrap();
    shape_layer(&mut s, b, 0.2, 3.5, vec![marker(1.0, 0.0, "Beat")]);
    s.execute("time.set", json!({"time": 0.6})).unwrap();
    let r = s.execute("anim.applyPreset", json!({"path": path, "layers": [b]})).unwrap();
    assert_eq!(r["skipped"], json!([]), "{r:#}");
    assert_eq!(r["markers"], json!(3), "{r:#}");
    // "In" lands at the playhead like the first key, "Mid" 1 s after it, "Out" 0.6 s before the
    // new out point; durations as saved; "Beat" stays.
    let want = [(0.6, 0.5, "In"), (1.2, 0.0, "Beat"), (1.6, 0.0, "Mid"), (2.9, 0.4, "Out")];
    assert!(close(&markers(&s, b), &want), "{:?}", markers(&s, b));
    let opacity = s.active_comp().unwrap().layer(LayerId(b)).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
    let first_key = s.active_comp().unwrap().layer(LayerId(b)).unwrap().comp_time(opacity[0].time).seconds();
    assert!((first_key - 0.6).abs() < 1e-6, "the keys start at the playhead too: {first_key}");

    // The expressions read the new times.
    assert!((evaluated(&mut s, b, "transform/rotation") - 0.6).abs() < 1e-6);
    let ap = s.execute("prop.get", json!({"layer": b, "path": "transform/anchor"})).unwrap();
    let ap: Vec<f64> = ap["evaluated"].as_array().unwrap().iter().filter_map(|v| v.as_f64()).collect();
    assert!((ap[0] - 2.9).abs() < 1e-6 && (ap[1] - 0.4).abs() < 1e-6, "{ap:?}");

    // Applying again (elsewhere) replaces the preset's markers instead of stacking them.
    s.execute("anim.applyPreset", json!({"path": path, "layers": [b], "at": 0.8})).unwrap();
    let want = [(0.8, 0.5, "In"), (1.2, 0.0, "Beat"), (1.8, 0.0, "Mid"), (2.9, 0.4, "Out")];
    assert!(close(&markers(&s, b), &want), "{:?}", markers(&s, b));
    assert!((evaluated(&mut s, b, "transform/rotation") - 0.8).abs() < 1e-6);

    // A solid gets them too.
    let solid = s.execute("layer.newSolid", json!({"name": "S", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    shape_layer(&mut s, solid, 0.0, 4.0, vec![]);
    let r = s.execute("anim.applyPreset", json!({"path": path, "layers": [solid], "at": 1.0})).unwrap();
    assert_eq!(r["markers"], json!(3), "{r:#}");
    assert!(close(&markers(&s, solid), &[(1.0, 0.5, "In"), (2.0, 0.0, "Mid"), (3.4, 0.4, "Out")]), "{:?}", markers(&s, solid));
}

#[test]
fn presets_without_markers_leave_the_layers_markers_alone() {
    let path = temp_dir("old").join("old.ecpreset").to_string_lossy().to_string();
    let key = Keyframe::new(Tick::ZERO, KV::Scalar(50.0));
    let preset = json!({"effectcraftAnimationPreset": 2, "name": "old", "effects": [],
        "properties": [{"path": "transform/opacity", "value": KV::Scalar(50.0), "keys": [key], "expression": null}]});
    std::fs::write(&path, preset.to_string()).unwrap();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "C", "width": 64, "height": 64, "frameRate": 25, "duration": 2.0})).unwrap();
    let a = s.execute("layer.newText", json!({"text": "x"})).unwrap()["layer"].as_u64().unwrap();
    shape_layer(&mut s, a, 0.0, 2.0, vec![marker(1.0, 0.25, "In")]);
    let r = s.execute("anim.applyPreset", json!({"path": path, "layers": [a]})).unwrap();
    assert_eq!((r["applied"].clone(), r["markers"].clone()), (json!(1), json!(0)), "{r:#}");
    assert!(close(&markers(&s, a), &[(1.0, 0.25, "In")]));
}
