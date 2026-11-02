//! `prop.set` on a shape Path takes a plain `{vertices, inTangents, outTangents, closed}` object
//! like After Effects' Shape (2 Nov 2026: only the stored `{t: "Path", v: {…}}` / full snake_case
//! form worked). Tests not run yet.

use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;

const P: &str = "contents/group/contents/path/path";

fn shape() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "P", "width": 64, "height": 36, "frameRate": 30, "duration": 2})).unwrap();
    let r = s.execute("shape.newPath", json!({"vertices": [[0, 0], [10, 0], [10, 10]], "closed": true, "fill": "#ff0000", "strokeWidth": 0})).unwrap();
    (s, r["layer"].as_u64().unwrap())
}

#[test]
fn prop_set_takes_an_ae_style_shape_object() {
    let (mut s, l) = shape();
    s.execute_checked(
        "prop.set",
        json!({"layer": l, "path": P, "value": {"vertices": [[0, 0], [20, 0], [20, 20]], "inTangents": [[0, 0], [0, -4], [0, 0]], "outTangents": [[0, 0], [0, 4], [0, 0]], "closed": false}}),
    )
    .unwrap();
    let ly = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    let p = ly.props.prop(P).unwrap().value.as_path().unwrap().clone();
    assert_eq!(p.vertices, vec![[0.0, 0.0], [20.0, 0.0], [20.0, 20.0]]);
    assert_eq!(p.in_tangents[1], [0.0, -4.0]);
    assert_eq!(p.out_tangents[1], [0.0, 4.0]);
    assert!(!p.closed);
    // Only vertices: corners, closed.
    s.execute_checked("prop.set", json!({"layer": l, "path": P, "value": {"vertices": [[1, 1], [2, 2], [3, 1]]}})).unwrap();
    let ly = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    let p = ly.props.prop(P).unwrap().value.as_path().unwrap().clone();
    assert!(p.closed && p.in_tangents.iter().all(|t| *t == [0.0, 0.0]));
}

#[test]
fn a_bad_path_names_the_shape_it_wants() {
    let (mut s, l) = shape();
    let e =
        s.execute_checked("prop.set", json!({"layer": l, "path": P, "value": {"vertices": [[0, 0], [1, 1]], "inTangents": [[0, 0]]}})).unwrap_err().to_string();
    assert!(e.contains("inTangents") && e.contains("vertices"), "{e}");
}
