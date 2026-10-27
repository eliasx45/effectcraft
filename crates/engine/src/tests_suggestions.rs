//! Claude's suggestions (batch 1, CX4): propose changes nothing, accept is one undo step, reject
//! drops, preview leaves the session as it was.

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
    let r =
        s.execute("suggestion.propose", json!({"label": "Rename it", "steps": steps, "layers": [a], "range": [0.5, 0.2], "client": "claude-test"})).unwrap();
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
    let steps =
        json!([{"command": "layer.rename", "params": {"layer": a, "name": "Half"}}, {"command": "layer.rename", "params": {"layer": 999999, "name": "x"}}]);
    let id = s.execute("suggestion.propose", json!({"label": "Broken", "steps": steps})).unwrap()["id"].as_str().unwrap().to_string();
    assert!(s.execute("suggestion.accept", json!({"id": id})).is_err());
    assert_ne!(name(&s, a), "Half");
    assert_eq!(suggestions::list(&s).len(), 1);
}

#[test]
fn claude_version_follows_agent_steps_and_stays_while_the_user_edits() {
    let (mut s, a) = session();
    assert_eq!(s.execute("agent.version", json!({})).unwrap()["has"], json!(false));
    s.execute("layer.rename", json!({"layer": a, "name": "Claude's", "client": "claude-test"})).unwrap();
    suggestions::observe(&s);
    let (v, by) = suggestions::claude_version(&s).unwrap();
    assert_eq!(by.as_deref(), Some("claude-test"));
    // The user's fix: Claude's version stays as the agent left it.
    s.execute("layer.rename", json!({"layer": a, "name": "Mine"})).unwrap();
    suggestions::observe(&s);
    let (v2, _) = suggestions::claude_version(&s).unwrap();
    assert!(std::sync::Arc::ptr_eq(&v, &v2));
    assert_eq!(v2.comp(s.active_comp_id().unwrap()).unwrap().layer(LayerId(a)).unwrap().name, "Claude's");
    // agent.snapshot marks the current state.
    s.execute("agent.snapshot", json!({"client": "claude-test"})).unwrap();
    assert_eq!(s.execute("agent.version", json!({})).unwrap()["same"], json!(true));
}

#[test]
fn why_answer_goes_to_the_app() {
    let (mut s, _) = session();
    assert!(s.execute("why.answer", json!({})).is_err());
    s.execute("why.answer", json!({"why": "too big/small", "what": "Scale"})).unwrap();
    assert!(
        s.events.iter().any(|e| matches!(e, crate::Event::Frontend { command, params } if command == "why.answer" && params["why"] == json!("too big/small")))
    );
}

#[test]
fn usage_lists_actions_not_selection() {
    let (mut s, a) = session();
    s.execute("layer.select", json!({"layers": [a]})).unwrap();
    s.execute("layer.rename", json!({"layer": a, "name": "Card"})).unwrap();
    let r = s.execute("history.usage", json!({})).unwrap();
    let cmds: Vec<&str> = r["actions"].as_array().unwrap().iter().filter_map(|x| x["command"].as_str()).collect();
    assert!(cmds.contains(&"layer.rename"));
    assert!(!cmds.contains(&"layer.select"));
    let rename = r["actions"].as_array().unwrap().iter().find(|x| x["command"] == json!("layer.rename")).unwrap();
    assert_eq!(rename["layers"][0]["name"], json!("Card"));
}

/// 27 Oct: hovering a loopd suggestion (Anchor Point to [0, 0, 0]) ran it on the scratch copy,
/// and the scratch run's step stayed in the edit feed as the user's change. The next real edit,
/// an agent's script deleting keys in another comp, then read as reverting it ("Delete Keyframes:
/// Anchor Point [0, 0, 0] → [380, 130, 0]"). A preview leaves the feed as it was: no phantom
/// change, and the agent's step describes only what it did.
#[test]
fn a_preview_leaves_no_change_in_the_feed() {
    use crate::feed;
    let (mut s, a) = session();
    let card = s.active_comp_id().unwrap().0;
    s.execute("prop.set", json!({"layer": a, "path": "transform/anchor", "value": [5, 5, 0]})).unwrap();
    let steps = json!([{"command": "prop.set", "params": {"layer": a, "path": "transform/anchor", "value": [0, 0, 0]}}]);
    let r = s.execute("suggestion.propose", json!({"label": "Anchor", "comp": card, "steps": steps, "client": "loopd"})).unwrap();
    let id = r["id"].as_str().unwrap().to_string();
    // The agent's comp, with a key its script removes.
    s.execute("comp.new", json!({"name": "Agent", "width": 100, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
    let other = s.active_comp_id().unwrap().0;
    let b = s.execute("layer.newSolid", json!({"color": "#000000"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("prop.addKey", json!({"layer": b, "path": "transform/opacity", "time": 0.5, "value": 50})).unwrap();
    let rev = s.revision;
    let latest = s.freeze_runtime.latest_source.lock().unwrap().1.clone();
    let scratch = suggestions::preview(&mut s, &id).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&scratch, &s.project));
    assert_eq!(s.revision, rev);
    assert_eq!(s.feed.since(rev).count(), 0, "the scratch run is no one's edit");
    assert!(std::sync::Arc::ptr_eq(&s.freeze_runtime.latest_source.lock().unwrap().1, &latest), "Smart Freeze keeps rendering the real project");
    // The agent's script (as `removeKey` runs it): only its own change, in its own comp.
    s.execute("keys.select", json!({"comp": other, "keys": [{"layer": b, "path": "transform/opacity", "time": 0.5}], "client": "agent"})).unwrap();
    s.execute("keys.delete", json!({"comp": other, "client": "agent"})).unwrap();
    let entries: Vec<_> = s.feed.since(rev).cloned().collect();
    assert!(!entries.is_empty());
    for e in &entries {
        assert_eq!(e.client.as_deref(), Some("agent"));
        let (before, after) = e.projects();
        assert_eq!(feed::changed_comps(before, after), vec![other], "{:?}", feed::describe(before, after));
    }
    // And the document never had the suggestion's value.
    let anchor = s.execute("prop.get", json!({"comp": card, "layer": a, "path": "transform/anchor"})).unwrap()["value"].clone();
    assert_eq!(anchor, json!([5.0, 5.0, 0.0]));
}
