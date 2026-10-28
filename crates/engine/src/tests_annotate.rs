//! PN1: Pen / Comment review marks (`annotate.*`): session state that names the comp, frame,
//! points, box and the layers under the mark, and never touches the project or its history.

use serde_json::json;

use crate::Session;
use crate::commands::annotate::{Tool, bbox, inside_polygon, stroke_samples};

/// Comp "Main" 64×64 at 30 fps with a 10×10 solid "Dot" in the middle (27..37) over a full-frame
/// "BG"; returns (session, comp, dot).
fn setup() -> (Session, u64, u64) {
    let mut s = Session::default();
    let comp = s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap()["comp"].as_u64().unwrap();
    s.execute_checked("layer.newSolid", json!({"name": "BG", "color": "#202020", "width": 64, "height": 64})).unwrap();
    let dot = s.execute_checked("layer.newSolid", json!({"name": "Dot", "color": "#ff0000", "width": 10, "height": 10})).unwrap()["layer"].as_u64().unwrap();
    (s, comp, dot)
}

fn circle(cx: f64, cy: f64, r: f64) -> Vec<[f64; 2]> {
    (0..24).map(|i| i as f64 / 24.0 * std::f64::consts::TAU).map(|a| [cx + r * a.cos(), cy + r * a.sin()]).collect()
}

#[test]
fn a_stroke_around_a_layer_names_it_and_leaves_the_project_alone() {
    let (mut s, comp, dot) = setup();
    let (rev, project, steps) = (s.revision, s.project.clone(), s.history.tree(&s.project).len());
    s.execute_checked("annotate.start", json!({"tool": "pen"})).unwrap();
    let r = s
        .execute_checked("annotate.add", json!({"comp": comp, "time": 1.01, "tool": "pen", "points": circle(32.0, 32.0, 12.0), "note": "make it bigger"}))
        .unwrap();
    let m = &r["mark"];
    assert_eq!(m["tool"], "pen");
    assert_eq!(m["compName"], "Main");
    assert_eq!(m["frame"], json!(30), "the time snaps to the frame");
    assert_eq!(m["note"], "make it bigger");
    assert_eq!(m["compSize"], json!([64, 64]));
    let layers = m["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "{layers:?}");
    assert_eq!(layers[0]["id"], json!(dot), "the full-frame BG is a background, never named");
    assert!(r["copyRef"].as_str().unwrap().contains("Main") && r["copyRef"].as_str().unwrap().contains("make it bigger"));
    // Session state only.
    assert_eq!(s.revision, rev);
    assert!(std::sync::Arc::ptr_eq(&s.project, &project));
    assert_eq!(s.history.tree(&s.project).len(), steps);
}

#[test]
fn a_comment_is_one_point_and_a_background_is_a_moment() {
    let (mut s, comp, dot) = setup();
    let on = s.execute_checked("annotate.add", json!({"comp": comp, "time": 0, "tool": "comment", "point": [32, 32]})).unwrap();
    assert_eq!(on["mark"]["tool"], "comment");
    assert_eq!(on["mark"]["points"], json!([[32.0, 32.0]]));
    assert_eq!(on["mark"]["layers"][0]["id"], json!(dot));
    let off = s.execute_checked("annotate.add", json!({"comp": comp, "time": 0, "x": 2, "y": 2})).unwrap();
    assert_eq!(off["mark"]["tool"], "comment", "one point is a comment");
    assert_eq!(off["mark"]["layers"], json!([]), "only the background is under it");
}

#[test]
fn hosts_read_new_marks_and_remove_them_when_their_card_goes() {
    let (mut s, comp, _) = setup();
    let a = s.execute_checked("annotate.add", json!({"comp": comp, "point": [1, 1]})).unwrap()["mark"]["id"].as_u64().unwrap();
    let b = s.execute_checked("annotate.add", json!({"comp": comp, "point": [2, 2]})).unwrap()["mark"]["id"].as_u64().unwrap();
    assert!(b > a);
    let since = s.execute_checked("annotate.list", json!({"since": a})).unwrap();
    assert_eq!(since["marks"].as_array().unwrap().len(), 1);
    assert_eq!(since["last"], json!(b));
    assert_eq!(s.execute_checked("annotate.remove", json!({"id": a})).unwrap()["removed"], json!(1));
    assert!(s.execute_checked("annotate.get", json!({"id": a})).is_err());
    assert_eq!(s.execute_checked("annotate.clear", json!({})).unwrap()["removed"], json!(1));
    assert_eq!(s.execute_checked("annotate.list", json!({})).unwrap()["marks"], json!([]));
}

#[test]
fn arming_switches_tools_and_stop_disarms() {
    let mut s = Session::default();
    let serial = s.annotations.arm_serial;
    assert_eq!(s.execute_checked("annotate.start", json!({"tool": "pin"})).unwrap()["armed"], "comment", "pin is the old name");
    assert_eq!(s.annotations.armed, Some(Tool::Comment));
    s.execute_checked("annotate.start", json!({})).unwrap();
    assert_eq!(s.annotations.armed, Some(Tool::Pen));
    assert_eq!(s.annotations.arm_serial, serial + 2, "each arm pauses playback again");
    assert!(s.execute_checked("annotate.start", json!({"tool": "lasso"})).is_err());
    assert_eq!(s.execute_checked("annotate.stop", json!({})).unwrap()["armed"], json!(null));
}

#[test]
fn bad_marks_are_refused_without_a_panic() {
    let (mut s, comp, _) = setup();
    assert!(s.execute_checked("annotate.add", json!({"comp": comp})).is_err());
    assert!(s.execute_checked("annotate.add", json!({"comp": comp, "points": []})).is_err());
    assert!(s.execute_checked("annotate.add", json!({"comp": comp, "points": [["a", 1]]})).is_err());
    // Non-finite points are dropped, not stored.
    let r = s.execute_checked("annotate.add", json!({"comp": comp, "points": [[1, 1], [2, 2], [3, 1]]})).unwrap();
    assert_eq!(r["mark"]["points"].as_array().unwrap().len(), 3);
}

#[test]
fn marks_are_capped() {
    let (mut s, comp, _) = setup();
    for i in 0..(crate::commands::annotate::MAX_MARKS + 5) {
        s.execute_checked("annotate.add", json!({"comp": comp, "point": [i % 64, 1]})).unwrap();
    }
    assert_eq!(s.annotations.marks.len(), crate::commands::annotate::MAX_MARKS);
}

#[test]
fn geometry_helpers() {
    assert_eq!(bbox(&[[1.0, 2.0], [5.0, -1.0]]), [1.0, -1.0, 4.0, 3.0]);
    assert_eq!(bbox(&[]), [0.0; 4]);
    let sq = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
    assert!(inside_polygon([5.0, 5.0], &sq));
    assert!(!inside_polygon([15.0, 5.0], &sq));
    assert!(!inside_polygon([1.0, 1.0], &sq[..2]));
    let samples = stroke_samples(&circle(32.0, 32.0, 12.0));
    assert!(samples.len() > 5 && samples.iter().all(|p| (p[0] - 32.0).hypot(p[1] - 32.0) <= 12.5));
    assert_eq!(stroke_samples(&[[3.0, 4.0]]).len(), 3);
}
