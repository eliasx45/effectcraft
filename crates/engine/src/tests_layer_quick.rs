//! Batch 1 quick layer commands (AX4): layer.fadeExit, layer.fastIn, layer.cascade.
//! Not run yet (batch 1).

use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;
use crate::commands::graph_cmds::linked_graph;

fn comp() -> (Session, u64, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Q", "width": 320, "height": 180, "frameRate": 25, "duration": 4})).unwrap();
    let a = s.execute("layer.newSolid", json!({"name": "A", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    let b = s.execute("layer.newSolid", json!({"name": "B", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    (s, a, b)
}

#[test]
fn fade_exit_keys_the_last_eight_frames() {
    let (mut s, a, _) = comp();
    let r = s.execute_checked("layer.fadeExit", json!({"layers": [a]})).unwrap();
    assert_eq!(r["frames"], 8);
    let l = s.active_comp().unwrap().layer(LayerId(a)).unwrap();
    let keys = &l.props.prop("transform/opacity").unwrap().keys;
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[1].value.components(), vec![0.0]);
    assert!(((keys[1].time.seconds() - keys[0].time.seconds()) * 25.0 - 8.0).abs() < 1e-6);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active_comp().unwrap().layer(LayerId(a)).unwrap().props.prop("transform/opacity").unwrap().keys.is_empty());
}

#[test]
fn fast_in_makes_a_linked_move_on_each_property() {
    for prop in ["position", "opacity", "scale", "rotation"] {
        let (mut s, a, _) = comp();
        s.execute_checked("layer.fastIn", json!({"layers": [a], "property": prop})).unwrap();
        let path = crate::commands::graph_cmds::property_path(prop);
        let pr = s.active_comp().unwrap().layer(LayerId(a)).unwrap().props.prop(&path).unwrap().clone();
        assert_eq!(pr.keys.len(), 2, "{prop}");
        assert_eq!(linked_graph(&pr.keys, 0, pr.spatial).map(|g| g.id), Some("fast-in"), "{prop}");
    }
    let (mut s, _, _) = comp();
    assert!(s.execute_checked("layer.fastIn", json!({"layers": [], "property": "position"})).is_err());
    assert!(s.execute_checked("layer.fastIn", json!({"frames": -3})).is_err());
}

#[test]
fn cascade_staggers_by_stack_order() {
    let (mut s, a, b) = comp();
    let r = s.execute_checked("layer.cascade", json!({"layers": [a, b], "frames": 2})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 2);
    let c = s.active_comp().unwrap();
    let (la, lb) = (c.layer(LayerId(a)).unwrap(), c.layer(LayerId(b)).unwrap());
    // The newest layer (B) is on top: it stays, A starts 2 frames later.
    assert_eq!(lb.in_point.seconds(), 0.0);
    assert!((la.in_point.seconds() * 25.0 - 2.0).abs() < 1e-6);
    assert!(s.execute_checked("layer.cascade", json!({"layers": [a]})).is_err());
}
