//! Branching history: undo then a new step keeps the undone line as a branch; the tree lists
//! every state; jumping to any state (on any branch) restores it and keeps everything else.

use std::sync::Arc;

use effectcraft_project::Project;
use serde_json::{Value, json};

use crate::Session;
use crate::history::{Branch, History};

fn names(s: &Session) -> Vec<String> {
    s.project.comps().next().map(|(_, c)| c.layers.iter().map(|l| l.name.clone()).collect()).unwrap_or_default()
}

fn solid(s: &mut Session, name: &str) {
    s.execute("layer.newSolid", json!({"name": name, "color": "#ff0000", "width": 10, "height": 10})).unwrap();
}

fn list(s: &mut Session) -> Vec<Value> {
    s.execute("edit.history.list", json!({})).unwrap()["states"].as_array().cloned().unwrap()
}

fn index_of(states: &[Value], label: &str, nth: usize) -> u64 {
    states.iter().filter(|n| n["label"] == label).nth(nth).unwrap_or_else(|| panic!("no state {label} in {states:?}"))["index"].as_u64().unwrap()
}

#[test]
fn undo_then_new_step_keeps_a_branch() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "H", "width": 64, "height": 64, "duration": 2})).unwrap();
    solid(&mut s, "A");
    solid(&mut s, "B");
    solid(&mut s, "C");
    // Linear so far: Original, New Composition, 3 solids.
    let st = list(&mut s);
    assert_eq!(st.len(), 5, "{st:?}");
    assert!(st.iter().all(|n| n["depth"] == 0));
    assert_eq!(st.last().unwrap()["current"], true);
    // Undo twice (B and C undone), then a new step: AE would forget B and C.
    assert!(s.undo() && s.undo());
    let st = list(&mut s);
    assert_eq!(st.iter().filter(|n| n["future"] == true).count(), 2);
    solid(&mut s, "D");
    assert!(s.history.redo.is_empty(), "the new step clears Redo, as in After Effects");
    assert_eq!(s.history.branches.len(), 1);
    assert_eq!(names(&s), ["D", "A"]);
    let st = list(&mut s);
    assert_eq!(st.len(), 6, "the undone states are still listed: {st:?}");
    let branch: Vec<&Value> = st.iter().filter(|n| n["depth"] == 1).collect();
    assert_eq!(branch.len(), 2, "B and C sit on a branch: {st:?}");
    // Jump to C on the old branch.
    let c = st.iter().rfind(|n| n["depth"] == 1).unwrap()["index"].as_u64().unwrap();
    let r = s.execute("edit.history.goto", json!({"index": c})).unwrap();
    assert_eq!(r["label"], "New Solid", "{r}");
    assert_eq!(names(&s), ["C", "B", "A"]);
    // Now D is the branch, and Undo walks the B/C line.
    assert_eq!(s.history.branches.len(), 1);
    assert_eq!(s.history.branches[0].steps.len(), 1);
    assert!(s.undo());
    assert_eq!(names(&s), ["B", "A"]);
    assert!(s.redo());
    // ...and back to D by id.
    let st = list(&mut s);
    let d = st.iter().find(|n| n["depth"] == 1).unwrap()["id"].as_str().unwrap().to_string();
    s.execute("edit.history.goto", json!({"id": d})).unwrap();
    assert_eq!(names(&s), ["D", "A"]);
    let st = list(&mut s);
    assert_eq!(st.len(), 6);
    assert_eq!(st.iter().filter(|n| n["current"] == true).count(), 1);
    // The original state.
    let root = index_of(&st, "Original", 0);
    s.execute("edit.history.goto", json!({"index": root})).unwrap();
    assert!(s.project.comps().next().is_none());
    assert!(s.history.undo.is_empty());
    assert_eq!(list(&mut s).len(), 6, "nothing is lost by jumping");
    // Relative steps along the line.
    s.execute("edit.history.goto", json!({"steps": 2})).unwrap();
    assert_eq!(names(&s), ["A"]);
    assert!(s.execute("edit.history.goto", json!({"id": "nope"})).is_err());
    assert!(s.execute("edit.history.goto", json!({})).is_err());
}

#[test]
fn branches_of_branches_and_purge() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "H", "width": 64, "height": 64, "duration": 2})).unwrap();
    solid(&mut s, "A");
    solid(&mut s, "B");
    s.undo();
    solid(&mut s, "C"); // branch 1: B
    s.undo();
    solid(&mut s, "D"); // branch 2: C
    let st = list(&mut s);
    assert_eq!(st.len(), 6, "{st:?}");
    // Go to B, undo to A, make E: D's line becomes a branch, and a branch of the new line.
    let b = st.iter().find(|n| n["depth"] == 1 && n["label"] == "New Solid").unwrap()["index"].as_u64().unwrap();
    s.execute("edit.history.goto", json!({"index": b})).unwrap();
    let which = names(&s);
    assert!(which == ["B", "A"] || which == ["C", "A"], "{which:?}");
    s.undo();
    solid(&mut s, "E");
    let st = list(&mut s);
    assert_eq!(st.len(), 7, "{st:?}");
    // Every state is reachable and restores its own layers.
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..st.len() {
        s.execute("edit.history.goto", json!({"index": i})).unwrap();
        seen.insert(names(&s).join(","));
        assert_eq!(list(&mut s).len(), 7);
    }
    for want in ["", "A", "B,A", "C,A", "D,A", "E,A"] {
        assert!(seen.contains(want), "{want:?} not reachable: {seen:?}");
    }
    s.execute("edit.purgeUndo", json!({})).unwrap();
    assert_eq!(list(&mut s).len(), 1);
}

#[test]
fn undo_levels_trim_branches() {
    let mut s = Session::default();
    s.prefs.general.undo_levels = 3;
    s.execute("comp.new", json!({"name": "H", "width": 64, "height": 64, "duration": 2})).unwrap();
    for n in ["A", "B", "C", "D", "E"] {
        solid(&mut s, n);
    }
    assert_eq!(s.history.undo.len(), 3);
    s.undo();
    s.undo();
    solid(&mut s, "F");
    // The branch (D, E) grew from a state still kept.
    assert_eq!(s.history.branch_states(), 2);
    for n in ["G", "H", "I"] {
        solid(&mut s, n);
    }
    // Its parent fell off the end of the undo levels: the branch goes too.
    assert_eq!(s.history.branch_states(), 0);
    assert_eq!(list(&mut s).len(), 4);
}

#[test]
fn modified_mark_follows_undo_and_no_op_edits_record_nothing() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "H", "width": 64, "height": 64, "duration": 2})).unwrap();
    solid(&mut s, "A");
    let path = std::env::temp_dir().join(format!("ec-dirty-{}.ecproj", std::process::id()));
    s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    assert!(!s.is_dirty());
    // Setting a value to what it already is: no undo step, still saved.
    let steps = s.history.undo.len();
    s.execute("prop.set", json!({"layer": "A", "path": "transform/opacity", "value": 100})).unwrap();
    assert_eq!(s.history.undo.len(), steps);
    assert!(!s.is_dirty());
    // A real change, then undo back to the saved state: not modified; redo: modified again.
    s.execute("prop.set", json!({"layer": "A", "path": "transform/opacity", "value": 50})).unwrap();
    assert!(s.is_dirty());
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!s.is_dirty(), "undo back to the saved state");
    s.execute("edit.redo", json!({})).unwrap();
    assert!(s.is_dirty());
    // History jumps: the saved state is the parent of the current (changed) one.
    let states = list(&mut s);
    let cur = states.iter().find(|n| n["current"] == true).unwrap();
    let (changed, saved) = (cur["index"].as_u64().unwrap(), cur["parent"].as_u64().unwrap());
    s.execute("edit.history.goto", json!({"index": saved})).unwrap();
    assert!(!s.is_dirty(), "jumped to the saved state");
    s.execute("edit.history.goto", json!({"index": changed})).unwrap();
    assert!(s.is_dirty());
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!s.is_dirty());
    let r = s.execute(
        "engine.batch",
        json!({"atomic": true, "steps": [
            {"command": "prop.set", "params": {"layer": "A", "path": "transform/opacity", "value": 10}},
            {"command": "no.suchCommand", "params": {}}
        ]}),
    );
    assert!(r.is_err());
    assert!(!s.is_dirty(), "rolled back");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_session_restored_with_unsaved_changes_stays_modified() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "H", "width": 64, "height": 64, "duration": 2})).unwrap();
    let p = (*s.project).clone();
    // The web app reopens the last visit's project, which had unsaved changes.
    s.replace_project(p, None);
    assert!(!s.is_dirty());
    s.mark_unsaved();
    assert!(s.is_dirty());
    solid(&mut s, "A");
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.is_dirty(), "undo doesn't reach a saved state");
}

// ---------------------------------------------------------------- agents and the user together

fn new_solid(s: &mut Session, name: &str, client: Option<&str>) -> u64 {
    let mut p = json!({"name": name, "color": "#ff0000", "width": 10, "height": 10});
    if let Some(c) = client {
        p["client"] = json!(c);
    }
    s.execute_checked("layer.newSolid", p).unwrap()["layer"].as_u64().unwrap()
}

/// A/B fix 6: a write outside the undo history (the footage check marking files missing after
/// open, a comp's Resolution) keeps who made the state, so the agent can still undo its step.
#[test]
fn a_write_outside_the_history_keeps_who_made_the_state() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "U", "width": 64, "height": 64})).unwrap();
    let comp = *s.project.comps().next().unwrap().0;
    new_solid(&mut s, "Agent's", Some("claude"));
    // In place with no bump (as footage_check's apply when nothing changed): the author map's
    // weak reference makes make_mut move the project.
    let _ = Arc::make_mut(&mut s.project);
    s.set_resolution(comp, effectcraft_project::Resolution::Half).unwrap();
    let current = list(&mut s).into_iter().find(|n| n["current"] == true).unwrap();
    assert_eq!(current["client"], "claude", "{current}");
    s.execute_checked("edit.undo", json!({"client": "claude"})).unwrap();
    assert!(names(&s).is_empty());
    // A history change is not carried over: the user's step stays the user's.
    new_solid(&mut s, "User's", None);
    let current = list(&mut s).into_iter().find(|n| n["current"] == true).unwrap();
    assert!(current.get("client").is_none(), "{current}");
}

#[test]
fn a_number_is_a_layer_id_never_a_stack_position() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Ids", "width": 64, "height": 64})).unwrap();
    let red = new_solid(&mut s, "Red", None);
    let blue = new_solid(&mut s, "Blue", None);
    // A number that is no layer's id is an error naming it, not the layer at that position.
    let missing = (1..).find(|n| *n != red && *n != blue).unwrap();
    let e = s.execute_checked("layer.rename", json!({"layer": missing, "name": "Oops"})).unwrap_err().to_string();
    assert!(e.contains(&format!("no layer {missing}")) && e.contains("\"#n\""), "{e}");
    assert_eq!(names(&s), ["Blue", "Red"]);
    // "#n" is the stack position; an id stays the same layer after a reorder.
    s.execute_checked("layer.rename", json!({"layer": "#2", "name": "Red 2"})).unwrap();
    s.execute_checked("layer.arrange", json!({"layers": [red], "to": "front"})).unwrap();
    s.execute_checked("layer.rename", json!({"layer": red, "name": "Red 3"})).unwrap();
    assert_eq!(names(&s), ["Red 3", "Blue"]);
    // A deleted layer's id hits nothing (not the layer now at that position).
    s.execute_checked("edit.clear", json!({"layers": [blue]})).unwrap();
    assert!(s.execute_checked("layer.rename", json!({"layer": blue, "name": "Stale"})).is_err());
    assert_eq!(names(&s), ["Red 3"]);
}

#[test]
fn ids_are_not_reused_after_undo() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Ids", "width": 64, "height": 64})).unwrap();
    let a = new_solid(&mut s, "Agent", Some("agent"));
    s.execute_checked("edit.undo", json!({"client": "agent"})).unwrap();
    let b = new_solid(&mut s, "User", None);
    assert_ne!(a, b, "an undone layer's id is never handed to another layer");
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.redo", json!({})).unwrap();
    assert!(s.active_comp().unwrap().layer(effectcraft_project::LayerId(b)).is_some());
}

#[test]
fn a_client_undoes_only_its_own_last_step() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "U", "width": 64, "height": 64})).unwrap();
    new_solid(&mut s, "Agent's", Some("agent-1"));
    new_solid(&mut s, "User's", None);
    // The user worked on another layer since: the agent's undo takes back only its own layer.
    let r = s.execute_checked("edit.undo", json!({"client": "agent-1"})).unwrap();
    assert_eq!(r["selective"], true, "{r}");
    assert_eq!(names(&s), ["User's"]);
    // When the user changed the agent's layer since, its undo is refused and changes nothing.
    let agents = new_solid(&mut s, "Agent's 2", Some("agent-1"));
    s.execute_checked("layer.rename", json!({"layer": agents, "name": "Renamed"})).unwrap();
    let rev = s.revision;
    let e = s.execute_checked("edit.undo", json!({"client": "agent-1"})).unwrap_err().to_string();
    assert!(e.contains("made by the user") && e.contains("undo without `client`"), "{e}");
    assert_eq!(s.revision, rev, "a refused undo changes nothing");
    assert_eq!(names(&s), ["Renamed", "User's"]);
    // Another client's step is refused too; the user's undo (no client) works as always.
    new_solid(&mut s, "Other's", Some("agent-2"));
    let e = s.execute_checked("edit.undo", json!({"client": "agent-1"})).unwrap_err().to_string();
    assert!(e.contains("client `agent-2`"), "{e}");
    s.execute_checked("edit.undo", json!({"client": "agent-2"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    s.execute_checked("edit.undo", json!({"client": "agent-1"})).unwrap();
    assert_eq!(names(&s), ["User's"]);
    // ⌘Z again skips the agent's own-step undo and takes the user's layer back.
    s.execute("edit.undo", json!({})).unwrap();
    assert!(names(&s).is_empty());
    // The History list names who made each state; batch steps inherit the batch's client.
    s.execute_checked("engine.batch", json!({"client": "agent-3", "steps": [{"command": "layer.newSolid", "params": {"color": "#00ff00"}}]})).unwrap();
    assert_eq!(list(&mut s).iter().find(|n| n["current"] == true).unwrap()["client"], "agent-3");
    s.execute_checked("edit.undo", json!({"client": "agent-3"})).unwrap();
    assert!(s.client.is_none(), "the client is only set while its command runs");
}

#[test]
fn expect_revision_refuses_a_changed_document() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "R", "width": 64, "height": 64})).unwrap();
    let rev = s.execute_checked("project.summary", json!({})).unwrap()["revision"].as_u64().unwrap();
    assert_eq!(s.execute_checked("editor.state", json!({})).unwrap()["revision"], rev);
    new_solid(&mut s, "Meanwhile", None);
    let e = s.execute_checked("layer.newSolid", json!({"color": "#0000ff", "expectRevision": rev})).unwrap_err().to_string();
    assert!(e.contains(&format!("document changed (revision {}, expected {rev})", s.revision)), "{e}");
    assert_eq!(names(&s), ["Meanwhile"], "nothing ran");
    let now = s.revision;
    s.execute_checked("layer.newSolid", json!({"color": "#0000ff", "expectRevision": now, "client": "agent"})).unwrap();
    assert_eq!(names(&s).len(), 2);
}

/// Multi-agent undo: ⌘Z undoes your own latest step, not an agent's that came after it; an
/// agent's undo likewise skips the user's later steps. Only what that step changed is reverted
/// (a new step), and it's refused, changing nothing, when someone changed the same thing since.
#[test]
fn undo_reverts_your_own_step_behind_others() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Mine", "width": 64, "height": 64})).unwrap();
    s.execute("comp.new", json!({"name": "Theirs", "width": 64, "height": 64})).unwrap();
    let theirs = s.active_comp_id().unwrap();
    s.execute("comp.new", json!({"name": "Mine 2", "width": 64, "height": 64})).unwrap();
    // The user renames a comp; then an agent adds a layer to another comp.
    let mine = s.project.find_by_name("Mine").unwrap().id;
    s.execute("project.rename", json!({"item": mine.0, "name": "Renamed"})).unwrap();
    s.execute_checked("layer.newSolid", json!({"comp": theirs.0, "name": "Agent's", "color": "#00ff00", "width": 8, "height": 8, "client": "agentA"})).unwrap();
    // ⌘Z: the user's rename goes, the agent's layer stays.
    let r = s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(r["selective"], true, "{r}");
    assert!(s.project.find_by_name("Mine").is_some() && s.project.find_by_name("Renamed").is_none());
    assert_eq!(s.project.comp(theirs).unwrap().layers.len(), 1, "the agent's work is untouched");
    // The agent undoes its own layer even though the user's undo came after it.
    s.execute_checked("edit.undo", json!({"client": "agentA"})).unwrap();
    assert_eq!(s.project.comp(theirs).unwrap().layers.len(), 0);
    // A clash: the agent adds a layer, the user then renames that same layer; the agent's undo
    // is refused and nothing changes.
    let a2 = s
        .execute_checked("layer.newSolid", json!({"comp": theirs.0, "name": "A2", "color": "#00ff00", "width": 8, "height": 8, "client": "agentA"}))
        .unwrap()["layer"]
        .clone();
    s.execute_checked("layer.rename", json!({"comp": theirs.0, "layer": a2, "name": "A2 (mine now)"})).unwrap();
    let rev = s.revision;
    let e = s.execute_checked("edit.undo", json!({"client": "agentA"})).unwrap_err().to_string();
    assert!(e.contains("made by the user") && e.contains("“A2 (mine now)” in “Theirs”"), "{e}");
    assert_eq!(s.revision, rev);
    // The Edit menu names the step ⌘Z undoes: the user's own latest, not the agent's last.
    assert_eq!(crate::selective_undo::own_undo_label(&s, None).as_deref(), Some("Rename Layer"));
    assert_eq!(crate::selective_undo::own_undo_label(&s, Some("nobody")), None);
    // `any: true` undoes the last step whoever made it.
    s.execute("edit.undo", json!({"any": true})).unwrap();
    assert_eq!(s.project.comp(theirs).unwrap().layers.first().map(|l| l.name.as_str()), Some("A2"));
}

/// Inside one comp, undo works layer by layer: the user's change to one layer is undone even
/// though an agent changed (or added) other layers of that comp since; the same layer clashes.
#[test]
fn undo_your_own_layer_change_while_an_agent_works_in_the_same_comp() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Shared", "width": 64, "height": 64})).unwrap();
    let comp = s.active_comp_id().unwrap().0;
    let solid = |s: &mut Session, name: &str, client: Option<&str>| {
        let mut p = json!({"comp": comp, "name": name, "color": "#00ff00", "width": 8, "height": 8});
        if let Some(c) = client {
            p["client"] = json!(c);
        }
        s.execute_checked("layer.newSolid", p).unwrap()["layer"].clone()
    };
    let names = |s: &Session| s.project.comp(effectcraft_project::ItemId(comp)).unwrap().layers.iter().map(|l| l.name.clone()).collect::<Vec<_>>();
    let mine = solid(&mut s, "Mine", None);
    let theirs = solid(&mut s, "Theirs", Some("agentA"));
    // The user renames their layer; the agent then renames its own layer and adds one.
    s.execute_checked("layer.rename", json!({"comp": comp, "layer": mine, "name": "Mine v2"})).unwrap();
    s.execute_checked("layer.rename", json!({"comp": comp, "layer": theirs, "name": "Theirs v2", "client": "agentA"})).unwrap();
    solid(&mut s, "Extra", Some("agentA"));
    assert_eq!(names(&s), ["Extra", "Theirs v2", "Mine v2"]);
    // ⌘Z: only the user's rename goes; the agent's layers and rename stay.
    let r = s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(r["selective"], true, "{r}");
    assert_eq!(names(&s), ["Extra", "Theirs v2", "Mine"]);
    // The agent deletes the user's layer: the user's next undo (their "Mine" solid) clashes.
    s.execute("edit.clear", json!({"comp": comp, "layers": [mine], "client": "agentA"})).unwrap();
    let rev = s.revision;
    let e = s.execute_checked("edit.undo", json!({})).unwrap_err().to_string();
    assert!(e.contains("“Mine” in “Shared”"), "{e}");
    assert_eq!(s.revision, rev);
    // The agent undoes its delete even though the user's undo came after: the layer is back in
    // its old place, under the agent's.
    s.execute_checked("edit.undo", json!({"client": "agentA"})).unwrap();
    assert_eq!(names(&s), ["Extra", "Theirs v2", "Mine"]);
    // And its added layer, while the user's work stays.
    s.execute_checked("edit.undo", json!({"client": "agentA"})).unwrap();
    assert_eq!(names(&s), ["Theirs v2", "Mine"]);
}

#[test]
fn dropped_redo_parent_does_not_discard_reachable_branches() {
    let state = || Arc::new(Project::default());
    let current = state();
    let futures: Vec<_> = (0..5).map(|_| state()).collect();
    let reachable = state();
    let mut history = History {
        redo: futures.iter().rev().map(|p| ("future".into(), p.clone())).collect(),
        branches: vec![
            Branch { parent: current.clone(), steps: vec![("reachable".into(), reachable.clone())] },
            Branch { parent: futures[4].clone(), steps: vec![("orphan1".into(), state()), ("orphan2".into(), state())] },
        ],
        ..Default::default()
    };
    history.trim(2, &current);
    assert_eq!(history.redo.len(), 2);
    assert_eq!(history.branches.len(), 1);
    assert!(Arc::ptr_eq(&history.branches[0].steps[0].1, &reachable));
    assert_eq!(history.branch_states(), 1);
    assert!(history.tree(&current).iter().any(|node| node.label == "reachable"));
}

#[test]
fn evicted_branch_parent_does_not_discard_other_reachable_branches() {
    let state = || Arc::new(Project::default());
    let current = state();
    let parent = state();
    let reachable = state();
    let mut history = History {
        branches: vec![
            Branch { parent: current.clone(), steps: vec![("parent".into(), parent.clone())] },
            Branch { parent: current.clone(), steps: vec![("reachable".into(), reachable.clone())] },
            Branch { parent, steps: vec![("orphan1".into(), state()), ("orphan2".into(), state())] },
        ],
        ..Default::default()
    };
    history.trim(2, &current);
    assert_eq!(history.branches.len(), 1);
    assert!(Arc::ptr_eq(&history.branches[0].steps[0].1, &reachable));
    assert_eq!(history.branch_states(), 1);
    assert_eq!(history.tree(&current).len(), 2);
}
