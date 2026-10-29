//! Now graphs: the segments under the playhead through precomps, and their edits as commands
//! (`now_graphs.rs`, `commands/now_graphs.rs`).

use effectcraft_keyframe::Interp;
use effectcraft_project::{ItemId, LayerId};
use serde_json::{Value, json};

use crate::Session;

fn solid(s: &mut Session, name: &str) -> u64 {
    s.execute("layer.newSolid", json!({"name": name, "width": 40, "height": 40})).unwrap();
    s.active_comp().unwrap().layers.iter().find(|l| l.name == name).unwrap().id.0
}

fn key(s: &mut Session, comp: ItemId, l: u64, path: &str, t: f64, v: Value) {
    s.execute("prop.addKey", json!({"comp": comp.0, "layer": l, "path": path, "time": t, "value": v})).unwrap();
}

/// Main (10 s) holds Scene (a precomp starting at 2 s); Scene's "Pill" moves 0.5 → 1.5 s
/// (Main 2.5 → 3.5 s); Main's own "Title" fades 3 → 4 s and its "Late" layer moves at 8 → 9 s.
fn board() -> (Session, ItemId, ItemId, u64, u64, u64, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Scene", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let scene = s.active_comp_id().unwrap();
    let pill = solid(&mut s, "Pill");
    key(&mut s, scene, pill, "transform/position", 0.5, json!([0.0, 90.0]));
    key(&mut s, scene, pill, "transform/position", 1.5, json!([160.0, 90.0]));
    s.execute("comp.new", json!({"name": "MAIN", "width": 320, "height": 180, "frameRate": 25, "duration": 10.0})).unwrap();
    let main = s.active_comp_id().unwrap();
    let title = solid(&mut s, "Title");
    key(&mut s, main, title, "transform/opacity", 3.0, json!(0.0));
    key(&mut s, main, title, "transform/opacity", 4.0, json!(100.0));
    let late = solid(&mut s, "Late");
    key(&mut s, main, late, "transform/position", 8.0, json!([0.0, 0.0]));
    key(&mut s, main, late, "transform/position", 9.0, json!([100.0, 0.0]));
    let pre = s.execute("layer.addItem", json!({"comp": main.0, "item": scene.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    {
        let p = std::sync::Arc::make_mut(&mut s.project);
        let nl = p.comp_mut(main).unwrap().layer_mut(LayerId(pre)).unwrap();
        nl.start_time = effectcraft_time::Tick::from_seconds_f64(2.0);
        nl.in_point = effectcraft_time::Tick::from_seconds_f64(2.0);
        nl.out_point = effectcraft_time::Tick::from_seconds_f64(6.0);
    }
    (s, main, scene, pre, pill, title, late)
}

fn graphs(r: &Value) -> Vec<Value> {
    r["graphs"].as_array().cloned().unwrap_or_default()
}

fn keys_of(s: &Session, comp: ItemId, l: u64, path: &str) -> Vec<effectcraft_keyframe::Keyframe> {
    s.project.comp(comp).unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap().keys.clone()
}

#[test]
fn lists_every_segment_touching_the_playhead_through_precomps() {
    let (mut s, main, scene, pre, pill, title, late) = board();
    let r = s.execute("nowGraphs.list", json!({"comp": main.0, "time": 3.2})).unwrap();
    let g = graphs(&r);
    // The pill inside Scene (2.5 → 3.5 in MAIN) and the title (3 → 4); not Late (8 → 9).
    assert_eq!(g.len(), 2, "{r:#}");
    let p = g.iter().find(|x| x["layer"] == pill).expect("the nested pill is listed");
    assert_eq!(p["comp"], scene.0);
    assert_eq!(p["topLayer"], pre, "it shows through the precomp layer in MAIN");
    assert!((p["start"].as_f64().unwrap() - 2.5).abs() < 0.01 && (p["end"].as_f64().unwrap() - 3.5).abs() < 0.01, "MAIN seconds: {p:#}");
    assert_eq!(p["path"], "transform/position");
    let trail: Vec<&str> = p["trail"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(trail.first().copied(), Some("MAIN"));
    assert_eq!(trail.get(trail.len().saturating_sub(2)).copied(), Some("Pill"));
    assert_eq!(p["speed"].as_array().unwrap().len(), crate::now_graphs::SAMPLES + 1);
    assert!(g.iter().any(|x| x["layer"] == title));
    assert!(g.iter().all(|x| x["layer"] != late));
    // Two objects, each with a box; the pill's box is in MAIN's pixels (Scene sits at its centre).
    let objects = r["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
    assert!(objects.iter().all(|o| o["quad"].is_array()), "{r:#}");
    // The curves: his approved Graphs, then Linear last.
    let curves = r["curves"].as_array().unwrap();
    assert!(curves.iter().any(|c| c["id"] == "expo-out-1"));
    assert!(curves.iter().any(|c| c["id"] == "entrance-fade"));
    assert_eq!(curves.last().unwrap()["id"], "linear");
}

#[test]
fn a_segment_a_second_away_still_touches_but_further_does_not() {
    let (mut s, main, _, _, _, title, _) = board();
    // 4.9 s: the title's 3 → 4 fade ends 0.9 s before: listed. 5.2 s: 1.2 s away: not.
    let near = graphs(&s.execute("nowGraphs.list", json!({"comp": main.0, "time": 4.9})).unwrap());
    assert!(near.iter().any(|x| x["layer"] == title));
    let far = graphs(&s.execute("nowGraphs.list", json!({"comp": main.0, "time": 5.2})).unwrap());
    assert!(far.iter().all(|x| x["layer"] != title));
    // A wider window reaches it.
    let wide = graphs(&s.execute("nowGraphs.list", json!({"comp": main.0, "time": 5.2, "window": 2.0})).unwrap());
    assert!(wide.iter().any(|x| x["layer"] == title));
}

#[test]
fn set_influence_is_one_undo_step_per_drag_and_keeps_values() {
    let (mut s, main, _, _, _, title, _) = board();
    let before = keys_of(&s, main, title, "transform/opacity");
    for (i, out) in [40.0, 55.0, 70.0].iter().enumerate() {
        let r = s
            .execute(
                "nowGraphs.setInfluence",
                json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "out": out, "in": 90.0, "merge": "nowgraphs-drag-1"}),
            )
            .unwrap();
        assert_eq!(r["out"].as_f64(), Some(*out), "step {i}: {r:#}");
    }
    let k = keys_of(&s, main, title, "transform/opacity");
    assert_eq!(k[0].value, before[0].value);
    assert_eq!(k[1].value, before[1].value);
    assert_eq!(k[0].out_interp, Interp::Bezier);
    assert_eq!(k[1].in_interp, Interp::Bezier);
    assert!((k[0].out_ease.as_slice()[0].influence - 0.70).abs() < 1e-6);
    assert!((k[1].in_ease.as_slice()[0].influence - 0.90).abs() < 1e-6);
    // The whole drag is one undo step.
    s.execute("edit.undo", json!({})).unwrap();
    let k = keys_of(&s, main, title, "transform/opacity");
    assert_eq!(k[0].out_interp, before[0].out_interp);
    assert_eq!(k[0].out_ease, before[0].out_ease);
}

#[test]
fn set_influence_clamps_and_refuses_bad_keys() {
    let (mut s, main, _, _, _, title, _) = board();
    let r = s.execute("nowGraphs.setInfluence", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "out": 400.0})).unwrap();
    assert_eq!(r["out"].as_f64(), Some(100.0));
    let r = s.execute("nowGraphs.setInfluence", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "out": -3.0})).unwrap();
    assert_eq!(r["out"].as_f64(), Some(0.1));
    assert!(s.execute("nowGraphs.setInfluence", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 1, "out": 50.0})).is_err());
    assert!(s.execute("nowGraphs.setInfluence", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0})).is_err());
    assert!(s.execute("nowGraphs.setInfluence", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "out": f64::NAN})).is_err());
}

#[test]
fn apply_ease_puts_a_saved_graph_on_a_nested_segment_in_one_step() {
    let (mut s, main, scene, _, pill, _, _) = board();
    // The shelf runs it with the nested comp, as nowGraphs.list reports it.
    let g = graphs(&s.execute("nowGraphs.list", json!({"comp": main.0, "time": 3.0})).unwrap());
    let p = g.iter().find(|x| x["layer"] == pill).unwrap();
    let r = s
        .execute("nowGraphs.applyEase", json!({"comp": p["comp"], "layer": p["layer"], "path": p["path"], "key": p["key"], "curve": "Expo Out 1 (Elias)"}))
        .unwrap();
    assert_eq!(r["curve"], "expo-out-1", "{r:#}");
    let k = keys_of(&s, scene, pill, "transform/position");
    assert!((k[0].out_ease.as_slice()[0].influence - 0.001).abs() < 1e-6, "out 0.1 %");
    assert!((k[1].in_ease.as_slice()[0].influence - 1.0).abs() < 1e-6, "in 100 %");
    // Listed again, it matches the curve.
    let g = graphs(&s.execute("nowGraphs.list", json!({"comp": main.0, "time": 3.0})).unwrap());
    assert_eq!(g.iter().find(|x| x["layer"] == pill).unwrap()["curve"], "expo-out-1");
    s.execute("edit.undo", json!({})).unwrap();
    let k = keys_of(&s, scene, pill, "transform/position");
    assert_eq!(k[0].out_interp, Interp::Linear, "one undo takes it back");
}

#[test]
fn apply_ease_linear_and_unknown_curves() {
    let (mut s, main, _, _, _, title, _) = board();
    s.execute("nowGraphs.applyEase", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "curve": "entrance-fade"})).unwrap();
    assert_eq!(keys_of(&s, main, title, "transform/opacity")[0].out_interp, Interp::Bezier);
    let r = s.execute("nowGraphs.applyEase", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "curve": "Linear"})).unwrap();
    assert_eq!(r["curve"], "linear");
    let k = keys_of(&s, main, title, "transform/opacity");
    assert_eq!((k[0].out_interp, k[1].in_interp), (Interp::Linear, Interp::Linear));
    assert!(
        s.execute("nowGraphs.applyEase", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0, "curve": "no such curve"})).is_err()
    );
    assert!(s.execute("nowGraphs.applyEase", json!({"comp": main.0, "layer": title, "path": "transform/opacity", "key": 0})).is_err());
}

#[test]
fn junk_times_and_windows_never_panic() {
    let (mut s, main, ..) = board();
    for (t, w) in [(f64::NAN, 1.0), (1e300, 1.0), (-5.0, f64::INFINITY), (3.0, -1.0), (3.0, 1e9)] {
        let r = s.execute("nowGraphs.list", json!({"comp": main.0, "time": t, "window": w}));
        assert!(r.is_ok() || r.is_err());
    }
    let q = [[-50.0, -50.0], [-10.0, -50.0], [-10.0, -10.0], [-50.0, -10.0]];
    assert!(crate::now_graphs::off_frame(&q, 320.0, 180.0));
    let q = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
    assert!(!crate::now_graphs::off_frame(&q, 320.0, 180.0));
}
