//! Gradient Lock (Motion Library comp tools) with expressions running: the gradient follows its
//! rectangle when the rectangle moves.

use effectcraft_engine::project::{LayerId, PropGroup, Value};
use serde_json::json;

fn find_group<'a>(g: &'a PropGroup, m: &str) -> Option<&'a PropGroup> {
    if g.match_id == m {
        return Some(g);
    }
    g.groups().find_map(|c| find_group(c, m))
}

#[test]
fn gradient_lock_follows_the_rectangle() {
    let mut s = effectcraft_host::session();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    let l = s.execute("layer.newShape", json!({"kind": "rect"})).unwrap()["layer"].as_u64().unwrap();
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    let group = find_group(&layer.props, "group").map(|g| g.uid);
    s.execute("layer.addShapeItem", json!({"layer": l, "kind": "gfill", "group": group})).unwrap();
    assert_eq!(s.execute("shape.gradientLock", json!({"layers": [l]})).unwrap()["locked"], 1);
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    let rect = find_group(&layer.props, "rect").unwrap().get("position").unwrap().uid;
    let start = find_group(&layer.props, "gfill").unwrap().get("start").unwrap();
    let (start_uid, Value::Vec2(before)) = (start.uid, start.value.clone()) else { panic!("2D start point") };
    let pos = s.execute("prop.get", json!({"layer": l, "path": format!("@{rect}")})).unwrap()["value"].clone();
    let moved = json!([pos[0].as_f64().unwrap() + 25.0, pos[1].as_f64().unwrap() - 10.0]);
    s.execute("prop.set", json!({"layer": l, "path": format!("@{rect}"), "value": moved})).unwrap();
    let got = s.execute("prop.get", json!({"layer": l, "path": format!("@{start_uid}")})).unwrap();
    assert!(got["expressionError"].is_null(), "{got}");
    let v = &got["evaluatedValue"];
    assert!((v[0].as_f64().unwrap() - (before[0] + 25.0)).abs() < 1e-6 && (v[1].as_f64().unwrap() - (before[1] - 10.0)).abs() < 1e-6, "{got}");
}
