//! `context.packet` and `layer.related` (editing harness H1).

use std::sync::Arc;

use effectcraft_project::{Expression, ItemId, LayerId};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::context::expression_layer_refs;
use crate::{KeyRef, Session};

fn id(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap()
}

/// Comp "Main" with Phone (keyed scale, parent Null), Screen (matted by Phone), Shadow (expression
/// naming Phone) and Null; precomp "Inner" nested in Main.
fn setup() -> (Session, ItemId, [LayerId; 4]) {
    let mut s = Session::default();
    let inner = id(&s.execute("comp.new", json!({"name": "Inner", "duration": 5})).unwrap(), "comp");
    let main = id(&s.execute("comp.new", json!({"name": "Main", "duration": 10, "frameRate": 30})).unwrap(), "comp");
    let mk = |s: &mut Session, name: &str| LayerId(id(&s.execute("layer.newSolid", json!({"name": name})).unwrap(), "layer"));
    let null = mk(&mut s, "Null");
    let shadow = mk(&mut s, "Shadow");
    let screen = mk(&mut s, "Screen");
    let phone = mk(&mut s, "Phone");
    s.execute("layer.setTrackMatte", json!({"layer": screen.0, "matte": phone.0, "kind": "alpha"})).unwrap();
    s.execute("layer.setParent", json!({"layers": [phone.0], "parent": null.0})).unwrap();
    s.execute("prop.addKey", json!({"layer": phone.0, "path": "transform/scale", "time": 0.0, "value": [0, 0, 100]})).unwrap();
    s.execute("prop.addKey", json!({"layer": phone.0, "path": "transform/scale", "time": 0.5, "value": [100, 100, 100]})).unwrap();
    let cid = ItemId(main);
    let l = Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(shadow).unwrap();
    l.props.prop_mut("transform/position").unwrap().expr = Some(Expression { text: "thisComp.layer(\"Phone\").position".into(), enabled: true });
    let _ = inner;
    (s, cid, [phone, screen, shadow, null])
}

#[test]
fn expression_refs_find_names_and_indices() {
    assert_eq!(expression_layer_refs("thisComp.layer(\"Phone\").position + layer( 3 ).scale"), vec!["Phone", "#3"]);
    assert_eq!(expression_layer_refs("comp('A').layer('B')"), vec!["B"]);
    assert!(expression_layer_refs("sourceLayer(1); layer(index-1)").is_empty());
    assert!(expression_layer_refs("layer(\"unterminated").is_empty());
}

#[test]
fn related_lists_parent_matte_and_expression_links() {
    let (mut s, cid, [phone, screen, shadow, null]) = setup();
    let r = s.execute_checked("layer.related", json!({"comp": cid.0, "layer": phone.0})).unwrap();
    assert_eq!(r["parents"][0]["id"], null.0);
    assert_eq!(r["matteFor"][0]["layer"]["id"], screen.0);
    assert_eq!(r["expressionReferredBy"][0]["id"], shadow.0);
    let r = s.execute_checked("layer.related", json!({"comp": cid.0, "layer": shadow.0})).unwrap();
    assert_eq!(r["expressionRefersTo"][0]["id"], phone.0);
    let r = s.execute_checked("layer.related", json!({"comp": cid.0, "layer": null.0})).unwrap();
    assert_eq!(r["children"][0]["id"], phone.0);
}

#[test]
fn packet_carries_selected_keys_with_eases_and_text() {
    let (mut s, cid, [phone, ..]) = setup();
    let uid = s.project.comp(cid).unwrap().layer(phone).unwrap().props.prop("transform/scale").unwrap().uid;
    s.state.selected_layers = vec![phone];
    s.state.selected_keys = vec![KeyRef { layer: phone, prop: uid, time: Tick::from_seconds_f64(0.5) }];
    s.state.times.insert(cid, Tick::from_seconds_f64(0.25));
    let revision = s.revision;
    let p = s.execute_checked("context.packet", json!({"comp": cid.0})).unwrap();
    assert_eq!(p["comp"]["name"], "Main");
    assert_eq!(p["frame"], 8);
    let layer = &p["layers"][0];
    assert_eq!(layer["name"], "Phone");
    let prop = &layer["selectedProperties"][0];
    assert_eq!(prop["path"], "transform/scale");
    assert_eq!(prop["keyCount"], 2);
    assert_eq!(prop["keys"][1]["selected"], true);
    assert_eq!(prop["keys"][1]["frame"], 15);
    assert!(prop["keys"][0]["outEase"].is_array());
    assert_eq!(layer["related"]["matteFor"][0]["layer"]["name"], "Screen");
    let text = p["text"].as_str().unwrap();
    for needle in ["Comp \"Main\"", "Selected #", "transform/scale  2 keys", "▸ 0.500s (f15)", "is matte for", "used by expressions in", "Recent edits"] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    assert_eq!(s.revision, revision, "a query cannot edit the project");
}

#[test]
fn packet_without_selection_is_small() {
    let (mut s, cid, _) = setup();
    s.state.selected_layers.clear();
    s.state.selected_keys.clear();
    s.state.selected_props.clear();
    let p = s.execute_checked("context.packet", json!({"comp": cid.0})).unwrap();
    assert!(p["layers"].as_array().unwrap().is_empty());
    assert!(p["text"].as_str().unwrap().contains("Nothing selected."));
}
