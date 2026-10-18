//! A project file remembers the comps open in the Timeline/Composition tabs, the one in front and
//! each one's current time, and opening it reopens them (as After Effects does).

use std::path::PathBuf;

use effectcraft_project::ItemId;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

fn tmp(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("ec-open-comps-{}-{}-{name}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn comp(s: &mut Session, name: &str) -> ItemId {
    ItemId(s.execute("comp.new", json!({"name": name, "width": 64, "height": 36, "frameRate": 25, "duration": 10})).unwrap()["comp"].as_u64().unwrap())
}

/// Comps A, B and C open (in that tab order, C last), with their own current times.
fn three_open() -> (Session, [ItemId; 3]) {
    let mut s = Session::default();
    let ids = [comp(&mut s, "A"), comp(&mut s, "B"), comp(&mut s, "C")];
    for (id, t) in ids.iter().zip([1.0, 2.0, 3.0]) {
        s.execute("comp.open", json!({"comp": id.0})).unwrap();
        s.execute("time.set", json!({"time": t})).unwrap();
    }
    (s, ids)
}

fn secs(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

#[test]
fn reopening_a_project_restores_its_open_comps_front_comp_and_times() {
    let d = tmp("restore");
    let path = d.join("P.ecproj");
    let (mut s, [a, b, c]) = three_open();
    // B in front.
    s.execute("comp.open", json!({"comp": b.0})).unwrap();
    s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["openComps"]["comps"].as_array().map(Vec::len), Some(3), "{}", v["openComps"]);
    assert_eq!(v["openComps"]["active"], json!(b.0));

    let mut o = Session::default();
    o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(o.state.open_comps, vec![a, b, c]);
    assert_eq!(o.active_comp_id(), Some(b));
    assert_eq!((o.time_of(a), o.time_of(b), o.time_of(c)), (secs(1.0), secs(2.0), secs(3.0)));
    assert!(!o.is_dirty());
    assert!(o.history.undo.is_empty());
    // The viewer is told which comp is in front.
    assert!(o.drain_events().iter().any(|e| matches!(e, crate::Event::OpenComp(id) if *id == b)));
    // A closed comp stays closed.
    o.execute("comp.close", json!({"comp": a.0})).unwrap();
    o.execute("file.save", json!({})).unwrap();
    o.execute("file.newProject", json!({})).unwrap();
    o.execute("file.openRecent", json!({"index": 0})).unwrap();
    assert_eq!(o.state.open_comps, vec![b, c]);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn deleted_or_unknown_comps_are_skipped_and_an_old_file_opens_as_before() {
    let d = tmp("stale");
    let path = d.join("P.ecproj");
    let (mut s, [a, b, c]) = three_open();
    s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut v: Value = serde_json::from_str(&text).unwrap();
    // A comp that's gone, a repeat and a time past the end; the front comp is the gone one.
    v["openComps"] = json!({"comps": [{"comp": 9999, "time": 0}, {"comp": c.0, "time": secs(99.0).0}, {"comp": a.0}, {"comp": c.0}], "active": 9999});
    std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    let mut o = Session::default();
    o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(o.state.open_comps, vec![c, a]);
    assert_eq!(o.active_comp_id(), Some(c), "the first comp left comes to the front");
    let cc = o.project.comp(c).unwrap();
    assert_eq!(o.time_of(c), cc.duration - cc.frame_duration(), "clamped to the last frame");
    assert_eq!(o.time_of(a), Tick::ZERO);
    assert!(!o.is_dirty());

    // Nothing usable (or malformed): today's behaviour, the first comp opens.
    for open_comps in [json!({"comps": [{"comp": 9999}]}), json!({"comps": []}), json!("garbage")] {
        v["openComps"] = open_comps;
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let mut o = Session::default();
        o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        assert_eq!(o.state.open_comps, vec![a], "{}", v["openComps"]);
        assert_eq!(o.active_comp_id(), Some(a));
    }
    // A file from before `openComps` (or another app) opens as before.
    v.as_object_mut().unwrap().remove("openComps");
    std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    let mut o = Session::default();
    o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(o.state.open_comps, vec![a]);
    assert_eq!(o.active_comp_id(), Some(a));
    let _ = b;
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn changing_tabs_or_the_time_does_not_modify_the_project_but_saves_the_current_view() {
    let d = tmp("dirty");
    let path = d.join("P.ecproj");
    let (mut s, [a, _, c]) = three_open();
    s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    assert!(!s.is_dirty());
    s.execute("comp.open", json!({"comp": a.0})).unwrap();
    s.execute("time.set", json!({"time": 4.0})).unwrap();
    assert!(!s.is_dirty(), "switching tabs and moving the CTI leave the project unmodified");
    let undo = s.history.undo.len();
    // Saving now writes the view as it is now.
    s.execute("file.save", json!({})).unwrap();
    assert_eq!(s.history.undo.len(), undo);
    let mut o = Session::default();
    o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(o.active_comp_id(), Some(a));
    assert_eq!(o.time_of(a), secs(4.0));
    assert_eq!(o.time_of(c), secs(3.0));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn an_agent_saving_writes_the_users_view_not_its_own() {
    let d = tmp("agent");
    let path = d.join("P.ecproj");
    let (mut s, [a, b, c]) = three_open();
    // The user is on C at 3 s; the agent moves to A at 7 s in its own cursor, then saves.
    s.execute_checked("comp.open", json!({"comp": a.0, "client": "bot"})).unwrap();
    s.execute_checked("time.set", json!({"time": 7.0, "client": "bot"})).unwrap();
    s.execute_checked("file.saveAs", json!({"path": path.to_string_lossy(), "client": "bot"})).unwrap();
    let mut o = Session::default();
    o.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(o.state.open_comps, vec![a, b, c]);
    assert_eq!(o.active_comp_id(), Some(c));
    assert_eq!(o.time_of(a), secs(1.0));
    assert_eq!(o.time_of(c), secs(3.0));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn auto_saves_and_xml_copies_carry_the_open_comps() {
    let d = tmp("autosave");
    let (mut s, [a, b, _]) = three_open();
    s.execute("comp.open", json!({"comp": b.0})).unwrap();
    s.prefs.auto_save.location = "custom".into();
    s.prefs.auto_save.folder = d.to_string_lossy().into_owned();
    let auto = s.autosave_now().unwrap();
    let xml = d.join("P.ecprojx");
    s.execute("file.saveCopyAsXml", json!({"path": xml.to_string_lossy()})).unwrap();
    for p in [auto, xml.to_string_lossy().into_owned()] {
        let mut o = Session::default();
        o.execute("file.open", json!({"path": p})).unwrap();
        assert_eq!(o.state.open_comps.len(), 3, "{p}");
        assert_eq!(o.active_comp_id(), Some(b), "{p}");
        assert_eq!(o.time_of(a), secs(1.0), "{p}");
    }
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn move_tab_reorders_the_open_comps_and_keeps_the_front_comp() {
    let (mut s, [a, b, c]) = three_open();
    s.execute("comp.moveTab", json!({"comp": c.0, "before": a.0})).unwrap();
    assert_eq!(s.state.open_comps, vec![c, a, b]);
    s.execute("comp.moveTab", json!({"comp": c.0})).unwrap();
    assert_eq!(s.state.open_comps, vec![a, b, c]);
    // Before itself: nothing moves.
    s.execute("comp.moveTab", json!({"comp": b.0, "before": b.0})).unwrap();
    assert_eq!(s.state.open_comps, vec![a, b, c]);
    assert_eq!(s.active_comp_id(), Some(c));
    // A comp with no tab is an error, not a new tab.
    let d = comp(&mut s, "D");
    s.execute("comp.close", json!({"comp": d.0})).unwrap();
    assert!(s.execute("comp.moveTab", json!({"comp": d.0})).is_err());
    assert_eq!(s.state.open_comps, vec![a, b, c]);
}
