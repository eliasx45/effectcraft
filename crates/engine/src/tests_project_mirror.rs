//! `project.mirror` / `project.applyPatch`: another app (FigCraft's hosted Timeline) keeps a
//! replica of the open project, edits it, and its edits land here as undo steps; this session
//! stays the one source of truth.

use std::sync::Arc;

use effectcraft_project::{ItemId, LayerId, Project};
use serde_json::{Value, json};

use crate::Session;
use crate::remote_sync::{self, Patch, SyncCache};

const CLIENT: &str = "figcraft-test";

/// The app: comp "Main" with layer L. Returns (app, comp, layer).
fn app() -> (Session, u64, u64) {
    let mut s = Session::default();
    let main = s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64})).unwrap()["comp"].as_u64().unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "L", "color": "#ff0000", "width": 10, "height": 10})).unwrap()["layer"].as_u64().unwrap();
    (s, main, l)
}

fn pull(app: &mut Session, reset: bool) -> Value {
    app.execute("project.mirror", json!({"client": CLIENT, "reset": reset})).unwrap()
}

/// The replica a `project.mirror` reply makes of `base` (None: start from the whole project).
fn replica(base: Option<Arc<Project>>, reply: &Value) -> Arc<Project> {
    if let Some(p) = reply.get("project").and_then(Value::as_str) {
        return Arc::new(remote_sync::read_project(p.as_bytes()).unwrap());
    }
    let mut base = base.unwrap();
    if let Some(patch) = reply.get("patch") {
        let patch: Patch = serde_json::from_value(patch.clone()).unwrap();
        remote_sync::apply(&mut base, &patch.ops, &mut SyncCache::default()).unwrap();
    }
    base
}

fn opacity(p: &Project, comp: u64, l: u64) -> f64 {
    p.comp(ItemId(comp)).unwrap().layer(LayerId(l)).unwrap().props.prop("transform/opacity").unwrap().value.as_f64()
}

/// A FigCraft-side edit of `base`: opacity set to `v` in a session holding the replica. Returns
/// the patch (from `from_rev`) and the edited project.
fn edit(base: &Arc<Project>, from_rev: u64, comp: u64, l: u64, v: f64) -> (Value, Arc<Project>) {
    let mut fig = Session { project: base.clone(), ..Session::default() };
    fig.open_comp(ItemId(comp));
    fig.execute("prop.set", json!({"comp": comp, "layer": l, "path": "transform/opacity", "value": v})).unwrap();
    let ops = remote_sync::diff_projects(base, &fig.project, &mut SyncCache::default()).unwrap();
    let patch = Patch { from_rev, to_rev: from_rev + 1, ops };
    (serde_json::to_value(&patch).unwrap(), fig.project.clone())
}

#[test]
fn the_mirror_sends_the_project_then_patches_then_nothing() {
    let (mut s, main, l) = app();
    let first = pull(&mut s, true);
    assert!(first.get("project").is_some(), "the first ask gets the whole project");
    assert_eq!(first["revision"].as_u64(), Some(s.revision));
    let base = replica(None, &first);
    assert!(base.same_content(&s.project));
    assert_eq!(pull(&mut s, false)["unchanged"], json!(true));
    s.execute("prop.set", json!({"comp": main, "layer": l, "path": "transform/opacity", "value": 25})).unwrap();
    let next = pull(&mut s, false);
    assert!(next.get("patch").is_some(), "an edit comes as a patch: {next}");
    let base = replica(Some(base), &next);
    assert_eq!(opacity(&base, main, l), 25.0);
    assert!(base.same_content(&s.project));
    // `reset`: the whole project again.
    assert!(pull(&mut s, true).get("project").is_some());
}

#[test]
fn a_replica_edit_lands_as_one_undo_step_and_undoes_here() {
    let (mut s, main, l) = app();
    let base = replica(None, &pull(&mut s, true));
    let rev = s.revision;
    let (patch, edited) = edit(&base, rev, main, l, 40.0);
    let r = s.execute("project.applyPatch", json!({"client": CLIENT, "patch": patch, "label": "Change Opacity", "merge": "fig-1"})).unwrap();
    assert!(r["revision"].as_u64().unwrap() > rev);
    assert_eq!(opacity(&s.project, main, l), 40.0);
    assert!(s.project.same_content(&edited), "the app holds exactly the replica's edit");
    assert!(s.is_dirty());
    assert_eq!(s.history.undo.last().map(|(l, _)| l.as_str()), Some("Change Opacity"));
    // The replica catches up from the app (a patch, its own edit included).
    let base = replica(Some(base), &pull(&mut s, false));
    assert!(base.same_content(&s.project));
    // A drag's next step with the same key folds into that step.
    let steps = s.history.undo.len();
    let (patch, _) = edit(&base, s.revision, main, l, 55.0);
    s.execute("project.applyPatch", json!({"client": CLIENT, "patch": patch, "label": "Change Opacity", "merge": "fig-1"})).unwrap();
    assert_eq!(s.history.undo.len(), steps, "merged");
    assert_eq!(opacity(&s.project, main, l), 55.0);
    // The app's undo takes the whole gesture back.
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(opacity(&s.project, main, l), 100.0);
}

#[test]
fn a_patch_from_an_old_revision_is_refused_and_changes_nothing() {
    let (mut s, main, l) = app();
    let base = replica(None, &pull(&mut s, true));
    let rev = s.revision;
    // The user edits in the app meanwhile.
    s.execute("prop.set", json!({"comp": main, "layer": l, "path": "transform/opacity", "value": 10})).unwrap();
    let (patch, _) = edit(&base, rev, main, l, 70.0);
    let before = s.project.clone();
    let e = s.execute("project.applyPatch", json!({"client": CLIENT, "patch": patch})).unwrap_err().to_string();
    assert!(e.contains("document changed"), "{e}");
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(opacity(&s.project, main, l), 10.0, "the app wins");
}

#[test]
fn junk_is_refused_never_panics() {
    let (mut s, main, l) = app();
    assert!(s.execute("project.mirror", json!({})).is_err(), "no client");
    assert!(s.execute("project.applyPatch", json!({"patch": {"fromRev": s.revision, "toRev": 0, "ops": []}})).is_err(), "no client");
    let rev = s.revision;
    for p in [
        json!({"client": CLIENT}),
        json!({"client": CLIENT, "patch": 3}),
        json!({"client": CLIENT, "patch": {"fromRev": rev, "toRev": 0, "ops": [{"op": "remove", "id": 987654}]}}),
        json!({"client": CLIENT, "patch": {"fromRev": rev, "toRev": 0, "ops": [{"op": "comp", "id": main, "hash": 1}]}}),
        json!({"client": CLIENT, "patch": {"fromRev": rev, "toRev": 0, "ops": [{"op": "item", "item": {"nope": 1}, "hash": 1}]}}),
        json!({"client": CLIENT, "patch": {"fromRev": rev, "toRev": 0, "ops": []}, "merge": 5}),
    ] {
        assert!(s.execute("project.applyPatch", p.clone()).is_err(), "{p}");
        assert_eq!(s.revision, rev, "{p} changed nothing");
    }
    assert_eq!(opacity(&s.project, main, l), 100.0);
    // An empty patch is fine and no step.
    let steps = s.history.undo.len();
    s.execute("project.applyPatch", json!({"client": CLIENT, "patch": {"fromRev": rev, "toRev": 0, "ops": []}})).unwrap();
    assert_eq!(s.history.undo.len(), steps);
    // Replicas stay bounded.
    for i in 0..10 {
        s.execute("project.mirror", json!({"client": format!("c{i}")})).unwrap();
    }
    assert!(crate::commands::project_mirror::mirrors_held(&s.session_id) <= 4, "bounded");
}
