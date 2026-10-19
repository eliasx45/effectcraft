//! Claude's suggestions (batch 1, CX4): propose changes nothing, accept is one undo step, reject
//! drops, preview leaves the session as it was.
//! Not run yet (batch 1): TODO(batch1-verify) run `cargo test -p effectcraft-engine tests_suggestions`.

use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;
use crate::commands::suggestions;

fn session() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "S", "width": 200, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
    let a = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
    (s, a)
}

fn name(s: &Session, a: u64) -> String {
    s.active_comp().unwrap().layer(LayerId(a)).unwrap().name.clone()
}

#[test]
fn propose_accept_reject() {
    let (mut s, a) = session();
    let rev = s.revision;
    let undo = s.history.undo.len();
    let steps = json!([{"command": "layer.rename", "params": {"layer": a, "name": "Kept"}}]);
    let r = s.execute("suggestion.propose", json!({"label": "Rename it", "steps": steps, "layers": [a], "range": [0.5, 0.2], "client": "claude-test"})).unwrap();
    let id = r["id"].as_str().unwrap().to_string();
    assert_eq!(s.revision, rev, "proposing changes nothing");
    let list = suggestions::list(&s);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].range, Some([0.2, 0.5]));
    assert_eq!(list[0].by.as_deref(), Some("claude-test"));
    // Preview: the scratch copy has it, the session doesn't.
    let scratch = suggestions::preview(&mut s, &id).unwrap();
    assert_eq!(scratch.comp(s.active_comp_id().unwrap()).unwrap().layer(LayerId(a)).unwrap().name, "Kept");
    assert_ne!(name(&s, a), "Kept");
    assert_eq!(s.history.undo.len(), undo);
    assert_eq!(s.revision, rev);
    // Accept: one undo step.
    s.execute("suggestion.accept", json!({"id": id})).unwrap();
    assert_eq!(name(&s, a), "Kept");
    assert_eq!(s.history.undo.len(), undo + 1);
    assert!(s.history.undo.last().unwrap().0.starts_with("Suggestion 1"));
    assert!(suggestions::list(&s).is_empty());
    // Reject: nothing changes.
    s.execute("suggestion.propose", json!({"label": "Again", "steps": [{"command": "layer.rename", "params": {"layer": a, "name": "Dropped"}}]})).unwrap();
    s.execute("suggestion.reject", json!({"all": true})).unwrap();
    assert_eq!(name(&s, a), "Kept");
    assert!(suggestions::list(&s).is_empty());
}

#[test]
fn bad_proposals_and_failed_accepts() {
    let (mut s, a) = session();
    assert!(s.execute("suggestion.propose", json!({"label": "x", "steps": []})).is_err());
    assert!(s.execute("suggestion.propose", json!({"label": "x", "steps": [{"command": "no.such"}]})).is_err());
    assert!(s.execute("suggestion.propose", json!({"label": "x", "steps": [{"command": "engine.batch"}]})).is_err());
    // A step that fails rolls the whole suggestion back, and it stays waiting.
    let steps = json!([{"command": "layer.rename", "params": {"layer": a, "name": "Half"}}, {"command": "layer.rename", "params": {"layer": 999999, "name": "x"}}]);
    let id = s.execute("suggestion.propose", json!({"label": "Broken", "steps": steps})).unwrap()["id"].as_str().unwrap().to_string();
    assert!(s.execute("suggestion.accept", json!({"id": id})).is_err());
    assert_ne!(name(&s, a), "Half");
    assert_eq!(suggestions::list(&s).len(), 1);
}
