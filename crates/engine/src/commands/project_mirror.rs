//! A live mirror of the open project for another app editing it (FigCraft's hosted Timeline,
//! fork: `effectcraft-inproc`), over the control channel. The app stays the one source of truth:
//!
//! - **`project.mirror {client, reset?}`** brings the caller's replica to the current revision:
//!   `{revision, path, idFloor, unchanged: true}` when it has it, `{…, patch}` (a
//!   [`crate::remote_sync::Patch`] from the revision it was last sent) or `{…, project}` (the whole
//!   project's JSON, the first time or after `reset`). The app remembers per session and `client`
//!   what it sent, exactly like a cloud frame server's replica ([`crate::remote::Mirror`]).
//! - **`project.applyPatch {client, patch, label?, merge?}`** applies the caller's edit — a patch
//!   from the replica's revision to its edited project — as one undoable step of this session
//!   (`label` in Edit ▸ Undo and the History panel, authored by `client`; consecutive patches with
//!   the same `merge` key fold into one step, as a drag does here). Refused, changing nothing,
//!   when the document moved on since `patch.fromRev` (the caller re-syncs: this app wins).
//!   → `{revision}`. The caller then asks `project.mirror` for the result.
//!
//! Undo, redo and saving are this app's own commands (`edit.undo`, `file.save`), run as usual.

use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_project::Project;
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, str_p};
use crate::remote::{FrameMsg, Mirror};
use crate::remote_sync::{self, Patch, SyncCache};
use crate::{EngineError, Result, Session};

const MIRROR: &str = "project.mirror";
const APPLY: &str = "project.applyPatch";
/// Replicas remembered per session, and in all (each holds a project snapshot); the least
/// recently used goes.
const MAX_PER_SESSION: usize = 4;
const MAX_MIRRORS: usize = 32;
/// Longest undo label taken.
const MAX_LABEL: usize = 120;

/// What was last sent to each (session, client): its replica.
struct Entry {
    session: String,
    client: String,
    mirror: Mirror,
    used: u64,
}

static MIRRORS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn caller(s: &Session, cmd: &str) -> Result<String> {
    s.client.clone().filter(|c| !c.is_empty()).ok_or_else(|| bad(cmd, "`client` (the replica's name) is required"))
}

/// Run `f` on the replica record of (session, client), made when missing.
fn with_mirror<T>(session: &str, client: &str, f: impl FnOnce(&mut Mirror) -> T) -> T {
    let mut all = MIRRORS.lock().unwrap_or_else(PoisonError::into_inner);
    let now = CLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let at = match all.iter().position(|e| e.session == session && e.client == client) {
        Some(i) => i,
        None => {
            let mine = all.iter().filter(|e| e.session == session).count();
            let oldest =
                |all: &Vec<Entry>, own: bool| all.iter().enumerate().filter(|(_, e)| !own || e.session == session).min_by_key(|(_, e)| e.used).map(|(i, _)| i);
            let drop = if mine >= MAX_PER_SESSION {
                oldest(&all, true)
            } else if all.len() >= MAX_MIRRORS {
                oldest(&all, false)
            } else {
                None
            };
            if let Some(i) = drop {
                all.swap_remove(i);
            }
            all.push(Entry { session: session.to_string(), client: client.to_string(), mirror: Mirror::default(), used: now });
            all.len() - 1
        }
    };
    match all.get_mut(at) {
        Some(e) => {
            e.used = now;
            f(&mut e.mirror)
        }
        // (just found or pushed)
        None => f(&mut Mirror::default()),
    }
}

fn mirror(s: &mut Session, p: &Value) -> Result<Value> {
    let client = caller(s, MIRROR)?;
    let (revision, project) = (s.revision, s.project.clone());
    let reset = p.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let msg = with_mirror(&s.session_id, &client, |m| {
        if reset {
            m.reset();
        }
        m.sync(revision, &project)
    });
    let mut out = json!({"revision": revision, "path": s.path, "idFloor": s.id_floor.max(s.project.next_id)});
    match msg {
        None => out["unchanged"] = json!(true),
        Some(FrameMsg::Patch(patch)) => out["patch"] = serde_json::to_value(&patch).map_err(|e| EngineError::Other(e.to_string()))?,
        Some(FrameMsg::Project { project, .. }) if !project.is_empty() => out["project"] = json!(project),
        Some(_) => {
            // The project couldn't be written: the next ask sends it whole again.
            with_mirror(&s.session_id, &client, Mirror::reset);
            return Err(EngineError::Other("project.mirror: the project couldn't be serialized".into()));
        }
    }
    Ok(out)
}

/// `ops` applied to `p` (left as it was on an error: the edit is dropped).
fn apply_ops(p: &mut Project, ops: &[remote_sync::PatchOp]) -> std::result::Result<(), String> {
    let mut arc = Arc::new(std::mem::take(p));
    let r = remote_sync::apply(&mut arc, ops, &mut SyncCache::default());
    *p = Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone());
    r
}

fn apply_patch(s: &mut Session, p: &Value) -> Result<Value> {
    caller(s, APPLY)?;
    let patch: Patch = match p.get("patch") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| bad(APPLY, format!("`patch`: {e}")))?,
        None => return Err(bad(APPLY, "`patch` ({fromRev, toRev, ops}) is required")),
    };
    if patch.from_rev != s.revision {
        return Err(EngineError::Other(format!("document changed (revision {}, expected {}); read it again and retry", s.revision, patch.from_rev)));
    }
    let label: String = str_p(p, "label").filter(|l| !l.trim().is_empty()).unwrap_or("Edit in FigCraft").chars().take(MAX_LABEL).collect();
    let merge = match p.get("merge") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if !m.is_empty() => Some(m.clone()),
        Some(_) => return Err(bad(APPLY, "`merge` must be a non-empty string")),
    };
    if patch.ops.is_empty() {
        return Ok(json!({"revision": s.revision}));
    }
    s.edit(&label, merge.as_deref(), |proj, _| apply_ops(proj, &patch.ops).map_err(|e| EngineError::Other(format!("project.applyPatch: {e}"))))?;
    s.sanitize_state();
    Ok(json!({"revision": s.revision}))
}

const fn spec(id: &'static str, label: &'static str, params: &'static str, run: super::Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, alt_shortcuts: &[], params, enabled: always, run, journal }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            MIRROR,
            "Mirror Project",
            "{client (required), reset? (send the whole project)} → {revision, path, idFloor, unchanged: true | patch: {fromRev, toRev, ops} | project: \"<JSON>\"}: brings the caller's replica of the open project to this revision (a patch from what it was last sent). Another app editing this project live (FigCraft's hosted Timeline) keeps its copy with it",
            mirror,
            false,
        ),
        // Not journaled: a patch only applies at the revision it was made from.
        spec(
            APPLY,
            "Apply Project Patch",
            "{client (required), patch: {fromRev, toRev, ops} (from project.mirror's replica, edited), label? (undo label, default \"Edit in FigCraft\"), merge? (key: consecutive patches with the same key are one undo step)} → {revision}: one undoable edit; refused when the document changed since `fromRev`",
            apply_patch,
            false,
        ),
    ]
}

/// Replicas remembered for session `session` (tests).
#[cfg(test)]
pub(crate) fn mirrors_held(session: &str) -> usize {
    MIRRORS.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|e| e.session == session).count()
}
