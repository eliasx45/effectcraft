//! `prop.set` takes `expression` (2 Nov 2026: `{path, expression}` failed with "missing `value`"
//! while `prop.setExpression` worked). Tests not run yet.

use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;

fn comp() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "E", "width": 64, "height": 36, "frameRate": 30, "duration": 2})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "S", "width": 8, "height": 8, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    (s, l)
}

fn expr(s: &Session, l: u64, path: &str) -> Option<String> {
    let pr = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap();
    pr.expr.as_ref().map(|e| e.text.clone())
}

#[test]
fn prop_set_with_only_an_expression_sets_it() {
    let (mut s, l) = comp();
    let r = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/rotation", "expression": "time * 90"})).unwrap();
    assert_eq!(r, json!("time * 90"));
    assert_eq!(expr(&s, l, "transform/rotation").as_deref(), Some("time * 90"));
    // The same as prop.setExpression: one undo step takes it off again.
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(expr(&s, l, "transform/rotation"), None);
}

#[test]
fn prop_set_with_a_value_and_an_expression_sets_both_in_one_step() {
    let (mut s, l) = comp();
    let r = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/opacity", "value": 40, "expression": "value / 2"})).unwrap();
    assert_eq!(r["expression"], json!("value / 2"));
    let ly = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    assert_eq!(ly.props.prop("transform/opacity").unwrap().value.as_f64(), 40.0);
    assert_eq!(expr(&s, l, "transform/opacity").as_deref(), Some("value / 2"));
    s.execute("edit.undo", json!({})).unwrap();
    let ly = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    assert_eq!(ly.props.prop("transform/opacity").unwrap().value.as_f64(), 100.0);
    assert_eq!(expr(&s, l, "transform/opacity"), None);
}

#[test]
fn prop_set_expression_errors_are_clear() {
    let (mut s, l) = comp();
    let e = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/rotation", "expression": 5})).unwrap_err().to_string();
    assert!(e.contains("`expression` must be a string"), "{e}");
    let e = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/nope", "expression": "1"})).unwrap_err().to_string();
    assert!(!e.contains("missing `value`"), "{e}");
    // Without either, the old message stays.
    let e = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/rotation"})).unwrap_err().to_string();
    assert!(e.contains("missing `value`"), "{e}");
}
