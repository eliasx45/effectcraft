//! The Motion Library panel's comp tools: Smart / Multi Precomp, Decomp, Deep Duplicate, Split
//! Masks and Gradient Lock, each one undo step, refusing what they can't keep exact.

use effectcraft_project::{ItemKind, LayerSource, PropGroup};
use serde_json::{Value, json};

use crate::Session;

fn comp_with_solids(names: &[&str]) -> (Session, u64, Vec<u64>) {
    let mut s = Session::default();
    let c = s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    let cid = c["comp"].as_u64().or_else(|| c["id"].as_u64()).unwrap_or_else(|| s.active_comp_id().unwrap().0);
    let ids = names.iter().map(|n| s.execute("layer.newSolid", json!({"name": n, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap()).collect();
    (s, cid, ids)
}

fn layer_names(s: &Session) -> Vec<String> {
    s.active_comp().unwrap().layers.iter().map(|l| l.name.clone()).collect()
}

#[test]
fn smart_and_multi_precomp_are_one_undo_step() {
    let (mut s, _, ids) = comp_with_solids(&["A", "B", "C"]);
    // Newest on top: C, B, A.
    assert_eq!(layer_names(&s), ["C", "B", "A"]);
    let err = s.execute("layer.smartPrecompose", json!({"layers": [ids[0], ids[2]]})).unwrap_err().to_string();
    assert!(err.contains("consecutive"), "{err}");
    let undo = s.history.undo.len();
    let r = s.execute("layer.smartPrecompose", json!({"layers": [ids[1], ids[2]], "label": "Blue"})).unwrap();
    assert_eq!(r["name"], "Precomp");
    assert_eq!(layer_names(&s), ["Precomp", "A"]);
    assert_eq!(s.active_comp().unwrap().layers.iter().next().unwrap().label, effectcraft_color::Label::Blue);
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(layer_names(&s), ["C", "B", "A"]);

    let r = s.execute("layer.multiPrecompose", json!({"layers": ids})).unwrap();
    assert_eq!(r["precomps"].as_array().unwrap().len(), 3);
    assert_eq!(layer_names(&s), ["C Precomp", "B Precomp", "A Precomp"]);
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(layer_names(&s), ["C", "B", "A"]);
    let r = s.execute("layer.multiPrecompose", json!({"layers": ids, "count": 2})).unwrap();
    assert_eq!(r["precomps"].as_array().unwrap().len(), 2);
}

#[test]
fn decomp_expands_a_plain_precomp_and_refuses_a_moved_one() {
    let (mut s, cid, ids) = comp_with_solids(&["A", "B", "C"]);
    let r = s.execute("layer.precompose", json!({"layers": [ids[1], ids[2]], "name": "Inner", "mode": "move"})).unwrap();
    let wrapper = r["layer"].as_u64().unwrap();
    s.execute("layer.moveTime", json!({"layers": [wrapper], "time": 1.0})).ok();
    assert_eq!(layer_names(&s), ["Inner", "A"]);
    let start = s.active_comp().unwrap().layer(effectcraft_project::LayerId(wrapper)).unwrap().start_time;
    let r = s.execute("layer.decompose", json!({"layers": [wrapper]})).unwrap();
    assert_eq!(r["expanded"], 2);
    assert_eq!(layer_names(&s), ["C", "B", "A"]);
    let c = s.active_comp().unwrap();
    assert!(c.layers.iter().take(2).all(|l| l.start_time == start), "copies keep the precomp layer's timing");
    // The nested comp stays in the project.
    assert!(s.project.items.values().any(|i| i.name == "Inner"));
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(layer_names(&s), ["Inner", "A"]);
    s.execute("prop.set", json!({"comp": cid, "layer": wrapper, "path": "transform/scale", "value": [50, 50, 100]})).unwrap();
    let err = s.execute("layer.decompose", json!({"layers": [wrapper]})).unwrap_err().to_string();
    assert!(err.contains("unchanged 2D precomp transform"), "{err}");
    let err = s.execute("layer.decompose", json!({"layers": [ids[0]]})).unwrap_err().to_string();
    assert!(err.contains("isn't a precomp layer"), "{err}");
}

#[test]
fn deep_duplicate_copies_nested_comps_and_links_the_copies() {
    let (mut s, cid, ids) = comp_with_solids(&["A", "B"]);
    let r = s.execute("layer.precompose", json!({"layers": [ids[1]], "name": "Inner", "mode": "move"})).unwrap();
    let inner = r["comp"].as_u64().unwrap();
    let r = s.execute("project.deepDuplicate", json!({"comp": cid})).unwrap();
    let items: Vec<u64> = r["items"].as_array().unwrap().iter().filter_map(Value::as_u64).collect();
    assert_eq!(items.len(), 2);
    let top = s.project.comp(effectcraft_project::ItemId(items[0])).unwrap();
    let nested: Vec<u64> = top.layers.iter().filter_map(|l| if let LayerSource::Comp { item } = &l.source { Some(item.0) } else { None }).collect();
    assert_eq!(nested, [items[1]], "the copy uses the copied precomp");
    assert_ne!(items[1], inner);
    assert!(matches!(s.project.item(effectcraft_project::ItemId(items[1])).map(|i| &i.kind), Some(ItemKind::Comp(_))));
    assert_eq!(s.project.item(effectcraft_project::ItemId(items[1])).unwrap().name, "Inner 2");
}

fn find_group<'a>(g: &'a PropGroup, m: &str) -> Option<&'a PropGroup> {
    if g.match_id == m {
        return Some(g);
    }
    g.groups().find_map(|c| find_group(c, m))
}

#[test]
fn split_masks_makes_one_layer_per_mask() {
    let (mut s, _, ids) = comp_with_solids(&["Card"]);
    let l = ids[0];
    s.execute("layer.addMask", json!({"layer": l, "shape": "rect", "rect": [10, 10, 40, 20]})).unwrap();
    s.execute("layer.addMask", json!({"layer": l, "shape": "ellipse", "rect": [100, 50, 60, 60], "mode": "subtract"})).unwrap();
    let r = s.execute("layer.splitMasks", json!({"layers": [l]})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 2);
    assert_eq!(r["centered"], 2);
    assert_eq!(layer_names(&s), ["Card · Mask 1", "Card · Mask 2", "Card"]);
    let c = s.active_comp().unwrap();
    let first = c.layers.iter().next().unwrap();
    assert_eq!(first.props.sub("masks").unwrap().children.len(), 1);
    assert_eq!(first.props.prop("transform/anchor").unwrap().value, effectcraft_project::Value::Vec3([30.0, 20.0, 0.0]));
    let second = c.layers.iter().nth(1).unwrap();
    let mask = second.props.sub("masks").unwrap().children[0].as_group().unwrap();
    assert!(matches!(mask.kind, effectcraft_project::GroupKind::Mask { mode: effectcraft_project::MaskMode::Add, .. }), "subtract becomes add");
    assert!(!c.layers.iter().nth(2).unwrap().switches.video, "the original is switched off");
    let (mut s, _, _) = comp_with_solids(&["Plain"]);
    assert!(s.execute("layer.splitMasks", json!({})).unwrap_err().to_string().contains("no masks"));
}

#[test]
fn gradient_lock_keeps_the_gradient_on_its_rectangle() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    let l = s.execute("layer.newShape", json!({"kind": "rect"})).unwrap()["layer"].as_u64().unwrap();
    let layer = s.active_comp().unwrap().layer(effectcraft_project::LayerId(l)).unwrap();
    let group = find_group(&layer.props, "group").map(|g| g.uid);
    let r = s.execute("layer.addShapeItem", json!({"layer": l, "kind": "gfill", "group": group})).unwrap();
    assert!(r["uid"].as_u64().is_some(), "{r}");
    let r = s.execute("shape.gradientLock", json!({"layers": [l]})).unwrap();
    assert_eq!(r["locked"], 1, "{r}");
    let again = s.execute("shape.gradientLock", json!({"layers": [l]})).unwrap();
    assert_eq!((again["locked"].as_u64(), again["already"].as_u64()), (Some(0), Some(1)));
    // Start and End Point follow the rectangle (evaluated in the host tests, which have expressions).
    let layer = s.active_comp().unwrap().layer(effectcraft_project::LayerId(l)).unwrap();
    let gfill = find_group(&layer.props, "gfill").unwrap();
    for m in ["start", "end"] {
        let e = gfill.get(m).unwrap().expr.as_ref().unwrap();
        assert!(e.enabled && e.text.contains("propertyGroup(2)(\"Rectangle Path 1\")(\"Position\")"), "{}", e.text);
    }
    // A solid isn't a shape layer.
    let (mut s, _, ids) = comp_with_solids(&["Solid"]);
    let err = s.execute("shape.gradientLock", json!({"layers": ids})).unwrap_err().to_string();
    assert!(err.contains("not a shape layer"), "{err}");
}
