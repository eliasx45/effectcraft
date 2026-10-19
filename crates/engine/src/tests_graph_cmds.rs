//! Batch 1 graph commands (AX2/AX3): graph.apply / copy / paste / save / relinkUpdate / links.
//! Not run yet (batch 1): written against the command docs.

use effectcraft_project::{ItemId, LayerId};
use serde_json::{Value, json};

use crate::Session;
use crate::commands::graph_cmds::{curve_of, graph_from_curve, linked_graph, pair_graph, same_graph};
use crate::commands::peak::{GRAPHS, Graph, graph_named};

fn comp() -> (Session, ItemId) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "G", "width": 320, "height": 180, "frameRate": 25, "duration": 6})).unwrap();
    let cid = s.active_comp_id().unwrap();
    (s, cid)
}

fn solid(s: &mut Session, name: &str) -> u64 {
    s.execute("layer.newSolid", json!({"name": name, "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap()
}

/// Position 0,0 → 100,50 between 1 s and 2 s.
fn mv(s: &mut Session, l: u64) {
    for (t, v) in [(1.0, json!([0.0, 0.0])), (2.0, json!([100.0, 50.0]))] {
        s.execute("prop.addKey", json!({"layer": l, "path": "transform/position", "value": v, "time": t, "timeBase": "comp"})).unwrap();
    }
}

fn select_first_key(s: &mut Session, cid: ItemId, l: u64) {
    let pr = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap();
    let k = json!({"layer": l, "prop": pr.uid, "time": pr.keys[0].time.seconds()});
    s.execute_checked("keys.select", json!({"keys": [k]})).unwrap();
}

fn graph_of(s: &Session, cid: ItemId, l: u64) -> Option<Graph> {
    let pr = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap();
    pair_graph(&pr.keys, 0, pr.spatial)
}

#[test]
fn curves_and_graphs_round_trip() {
    for g in GRAPHS {
        let back = graph_from_curve(curve_of(&g)).unwrap();
        assert!(same_graph(&g, &back), "{}: {back:?}", g.id);
    }
    assert!(graph_from_curve([0.0, 0.0, 0.5, 1.0]).is_none(), "x1 = 0 has no out side");
}

#[test]
fn apply_links_and_relink_updates_every_carrier() {
    let (mut s, cid) = comp();
    let a = solid(&mut s, "A");
    let b = solid(&mut s, "B");
    mv(&mut s, a);
    mv(&mut s, b);
    // Linear: no graph, no link.
    assert!(graph_of(&s, cid, a).is_none());
    for l in [a, b] {
        s.execute_checked("graph.apply", json!({"item": "Fast In", "layers": [l], "property": "position"})).unwrap();
    }
    let fast_in = graph_named("fast-in").unwrap();
    assert!(same_graph(&graph_of(&s, cid, a).unwrap(), &fast_in));
    let pr = s.project.comp(cid).unwrap().layer(LayerId(a)).unwrap().props.prop("transform/position").unwrap();
    assert_eq!(linked_graph(&pr.keys, 0, pr.spatial).map(|g| g.id), Some("fast-in"));
    let links = s.execute("graph.links", json!({})).unwrap();
    assert_eq!(links["links"].as_array().unwrap().len(), 2, "{links:#}");
    // The Library's Fast In changes: both moves follow, one undo step.
    let r = s
        .execute_checked(
            "graph.relinkUpdate",
            json!({"from": "fast-in", "to": {"out": {"speed": 10.0, "influence": 2.0}, "in": {"speed": 0.0, "influence": 90.0}}}),
        )
        .unwrap();
    assert_eq!(r["updated"], 2, "{r:#}");
    for l in [a, b] {
        let g = graph_of(&s, cid, l).unwrap();
        assert!((g.out.influence - 0.02).abs() < 1e-6, "{g:?}");
        assert!((g.inn.influence - 0.9).abs() < 1e-6, "{g:?}");
    }
    s.execute("edit.undo", json!({})).unwrap();
    assert!(same_graph(&graph_of(&s, cid, b).unwrap(), &fast_in), "undo puts Fast In back on both");
    assert!(same_graph(&graph_of(&s, cid, a).unwrap(), &fast_in));
}

#[test]
fn copy_paste_save_and_bad_input() {
    let (mut s, cid) = comp();
    let a = solid(&mut s, "A");
    let b = solid(&mut s, "B");
    mv(&mut s, a);
    mv(&mut s, b);
    s.execute_checked("graph.apply", json!({"item": "smooth", "layers": [a], "property": "position"})).unwrap();
    select_first_key(&mut s, cid, a);
    let c = s.execute_checked("graph.copy", json!({})).unwrap();
    assert_eq!(c["graph"]["id"], "smooth-emphasis", "{c:#}");
    assert_eq!(c["graph"]["linked"], true);
    select_first_key(&mut s, cid, b);
    let r = s.execute_checked("graph.paste", json!({})).unwrap();
    assert_eq!(r["moves"].as_array().unwrap().len(), 1, "{r:#}");
    assert!(same_graph(&graph_of(&s, cid, b).unwrap(), &graph_named("smooth").unwrap()), "pasted and still linked");
    // Save: the curve to store in the Library.
    let v = s.execute_checked("graph.save", json!({"name": "Mine"})).unwrap();
    assert_eq!(v["curve"].as_array().unwrap().len(), 4, "{v:#}");
    assert_eq!(v["library"]["params"]["op"], "save");
    // Bad input is an error, never a panic.
    for bad in [json!({"item": "nope"}), json!({"item": [1, 2]}), json!({"item": {"out": {"influence": 500}}}), json!({})] {
        assert!(s.execute_checked("graph.apply", bad.clone()).is_err(), "{bad}");
    }
    assert!(s.execute_checked("graph.save", json!({})).is_err());
    assert!(s.execute_checked("graph.relinkUpdate", json!({"from": "linear", "to": "fast-in"})).is_err());
    let _: Value = s.execute("graph.links", json!({})).unwrap();
}
