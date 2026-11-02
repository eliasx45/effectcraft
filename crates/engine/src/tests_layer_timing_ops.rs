//! `layer.timing` ops take an explicit time and refuse params they don't use (2 Nov 2026: agents'
//! `{op: "trimInToTime", in: 1}` trimmed to the current time instead, `trimOutToTime` with `out`
//! cut the layer to one frame, and an unknown op was a silent no-op). Tests not run yet.

use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;

fn comp() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "T", "width": 64, "height": 36, "frameRate": 30, "duration": 5})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "S", "width": 8, "height": 8, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    (s, l)
}

fn in_out(s: &Session, l: u64) -> (f64, f64) {
    let ly = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    (ly.in_point.seconds(), ly.out_point.seconds())
}

#[test]
fn trim_ops_use_the_given_in_and_out() {
    let (mut s, l) = comp();
    s.execute_checked("layer.timing", json!({"layers": [l], "op": "trimInToTime", "in": 1.0})).unwrap();
    s.execute_checked("layer.timing", json!({"layers": [l], "op": "trimOutToTime", "out": 3.0})).unwrap();
    let (i, o) = in_out(&s, l);
    assert!((i - 1.0).abs() < 1e-6 && (o - 3.0).abs() < 1e-6, "{i} {o}");
    // `time` works for every …ToTime op; the start stays put for trims.
    s.execute_checked("layer.timing", json!({"layers": [l], "op": "trimInToTime", "time": 2.0})).unwrap();
    assert!((in_out(&s, l).0 - 2.0).abs() < 1e-6);
    s.execute_checked("layer.timing", json!({"layers": [l], "op": "moveInToTime", "time": 0.5})).unwrap();
    let (i, o) = in_out(&s, l);
    assert!((i - 0.5).abs() < 1e-6 && (o - 1.5).abs() < 1e-6, "{i} {o}");
}

#[test]
fn trim_without_a_time_still_uses_the_current_time() {
    let (mut s, l) = comp();
    s.execute("time.set", json!({"time": 2.0})).unwrap();
    s.execute_checked("layer.timing", json!({"layers": [l], "op": "trimOutToTime"})).unwrap();
    assert!((in_out(&s, l).1 - 2.0).abs() < 1e-6);
}

#[test]
fn unknown_ops_and_unused_params_are_errors() {
    let (mut s, l) = comp();
    let err = |s: &mut Session, p| s.execute_checked("layer.timing", p).unwrap_err().to_string();
    assert!(err(&mut s, json!({"layers": [l], "op": "trimIn", "in": 1.0})).contains("unknown op `trimIn`"));
    assert!(err(&mut s, json!({"layers": [l], "op": "trimInToTime", "out": 1.0})).contains("doesn't use `out`"));
    assert!(err(&mut s, json!({"layers": [l], "op": "moveInToStart", "start": 1.0})).contains("doesn't use `start`"));
    assert!(err(&mut s, json!({"layers": [l], "time": 1.0})).contains("doesn't use `time`"));
    assert!(err(&mut s, json!({"layers": [l], "op": "trimInToTime", "in": 1.0, "time": 2.0})).contains("not both"));
    assert!(err(&mut s, json!({"layers": [l]})).contains("nothing to change"));
    assert!(err(&mut s, json!({"layers": [l], "in": "1s"})).contains("must be a number"));
    // Nothing changed.
    assert_eq!(in_out(&s, l), (0.0, 5.0));
}

#[test]
fn plain_in_out_and_the_menu_ops_still_work() {
    let (mut s, l) = comp();
    s.execute_checked("layer.timing", json!({"layers": [l], "in": 1.0, "out": 4.0})).unwrap();
    assert_eq!(in_out(&s, l), (1.0, 4.0));
    s.execute_checked("layer.moveInToStart", json!({"layers": [l]})).unwrap();
    assert_eq!(in_out(&s, l), (0.0, 3.0));
}
