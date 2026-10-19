//! Claude's suggestions (batch 1, CX4): an agent proposes an edit instead of making it, and the
//! user keeps or drops it.
//!
//! - `suggestion.propose {label, steps, layers?, comp?, range?}`: for agents. `steps` is a command
//!   list (as `engine.batch`, without `$N` references), `layers` and `range` say what it touches (the Timeline draws those
//!   dashed). Nothing changes until it is accepted.
//! - `suggestion.list`: what is waiting.
//! - `suggestion.accept {id | all}`: runs it, each suggestion one undo step ("Suggestion 2: …").
//! - `suggestion.reject {id | all}`: drops it; nothing changes.
//!
//! [`preview`] applies a suggestion to a scratch copy of the document (everything is put back
//! afterwards: document, undo history, selection, journal), so the viewer can show it while the
//! pointer is over its pill.
//!
//! Suggestions belong to the running session, not the document: they are not saved with the
//! project.
// TODO(batch1-verify): they live in a process-wide table keyed by `Session::session_id`; a field on
// `Session` (lib.rs, not lane C's file) would be cleaner.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use effectcraft_project::Project;
use serde::Serialize;
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, str_p};
use crate::{EngineError, Result, Session};

/// One waiting suggestion.
#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    pub id: String,
    /// Its number on the pill ("SUGGESTION 2").
    pub n: u32,
    pub label: String,
    /// `engine.batch` steps: `[{command, params?}]`.
    pub steps: Vec<Value>,
    /// The layers it touches (ids), in `comp`.
    pub layers: Vec<u64>,
    pub comp: Option<u64>,
    /// Comp seconds it plays over (the preview loops it).
    pub range: Option<[f64; 2]>,
    /// The agent (`client`) that proposed it.
    pub by: Option<String>,
}

/// The agent side of a session: its suggestions and Claude's first version (CX5).
#[derive(Default)]
pub(crate) struct AgentSide {
    pub suggestions: Vec<Suggestion>,
    pub next: u32,
    /// Claude's first version: the document as an agent left it (CX5).
    pub claude: Option<(Arc<Project>, Option<String>)>,
}

static SIDES: Mutex<BTreeMap<String, AgentSide>> = Mutex::new(BTreeMap::new());

/// The agent side of session `s` (created on first use).
pub(crate) fn with_side<T>(s: &Session, f: impl FnOnce(&mut AgentSide) -> T) -> T {
    let mut g: MutexGuard<'_, BTreeMap<String, AgentSide>> = SIDES.lock().unwrap_or_else(PoisonError::into_inner);
    // Sessions come and go (tests, reopened windows): keep the table small.
    if g.len() > 64 && !g.contains_key(&s.session_id) {
        let first = g.keys().next().cloned();
        if let Some(k) = first {
            g.remove(&k);
        }
    }
    f(g.entry(s.session_id.clone()).or_default())
}

/// The waiting suggestions, oldest first.
pub fn list(s: &Session) -> Vec<Suggestion> {
    with_side(s, |a| a.suggestions.clone())
}

const MAX_SUGGESTIONS: usize = 32;
const MAX_STEPS: usize = 256;
/// Commands a suggestion may not run.
const NOT_IN_SUGGESTION: &[&str] = &["engine.batch", "file.runScript", "script.run", "suggestion.accept", "suggestion.propose"];

fn propose(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "suggestion.propose";
    let label = str_p(p, "label").map(str::trim).filter(|l| !l.is_empty()).ok_or_else(|| bad(cmd, "missing `label` (what the pill says, e.g. \"Card → Fast In, peak on WORKS\")"))?;
    if label.chars().count() > 120 {
        return Err(bad(cmd, "`label`: at most 120 characters"));
    }
    let steps = p.get("steps").and_then(Value::as_array).ok_or_else(|| bad(cmd, "missing `steps`: [{command, params?}] (as engine.batch)"))?;
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(bad(cmd, format!("`steps`: 1 to {MAX_STEPS} commands")));
    }
    for (i, st) in steps.iter().enumerate() {
        let id = st.get("command").or_else(|| st.get("id")).and_then(Value::as_str).ok_or_else(|| bad(cmd, format!("step {}: needs `command`", i + 1)))?;
        if super::find(id).is_none() {
            return Err(bad(cmd, format!("step {}: unknown command `{id}`", i + 1)));
        }
        if NOT_IN_SUGGESTION.contains(&id) {
            return Err(bad(cmd, format!("step {}: `{id}` can't run in a suggestion", i + 1)));
        }
    }
    let layers: Vec<u64> = p.get("layers").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).take(256).collect()).unwrap_or_default();
    let range = p.get("range").and_then(Value::as_array).and_then(|a| {
        let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
        (x.is_finite() && y.is_finite()).then(|| if x <= y { [x, y] } else { [y, x] })
    });
    let comp = match p.get("comp") {
        Some(v) if !v.is_null() => Some(s.resolve_comp(Some(v)).map_err(|e| bad(cmd, e.to_string()))?.0),
        _ => s.active_comp_id().map(|c| c.0),
    };
    let by = s.client.clone();
    let steps = steps.clone();
    let label = label.to_string();
    let out = with_side(s, |a| {
        if a.suggestions.len() >= MAX_SUGGESTIONS {
            return Err(bad(cmd, format!("{MAX_SUGGESTIONS} suggestions are waiting already; accept or reject some first")));
        }
        a.next = a.next.saturating_add(1);
        let n = a.next;
        let sg = Suggestion { id: format!("s{n}"), n, label, steps, layers, comp, range, by };
        let v = json!({"id": sg.id, "n": n, "waiting": a.suggestions.len() + 1});
        a.suggestions.push(sg);
        Ok(v)
    })?;
    s.events.push(crate::Event::ProjectChanged { revision: s.revision });
    Ok(out)
}

/// The suggestions an `{id | all}` parameter names (removed from the table).
fn take(s: &Session, p: &Value, cmd: &str) -> Result<Vec<Suggestion>> {
    let all = b_p(p, "all").unwrap_or(false) || str_p(p, "id") == Some("all");
    let id = str_p(p, "id");
    if !all && id.is_none() {
        return Err(bad(cmd, "pass `id` (from suggestion.list) or `all: true`"));
    }
    with_side(s, |a| {
        if all {
            return Ok(std::mem::take(&mut a.suggestions));
        }
        let id = id.unwrap_or_default();
        let i = a.suggestions.iter().position(|sg| sg.id == id).ok_or_else(|| bad(cmd, format!("no suggestion `{id}` (suggestion.list)")))?;
        Ok(vec![a.suggestions.remove(i)])
    })
}

/// Run one suggestion as one undo step.
fn run_one(s: &mut Session, sg: &Suggestion) -> Result<Value> {
    let label = format!("Suggestion {}: {}", sg.n, sg.label);
    if let Some(c) = sg.comp
        && s.project.comp(effectcraft_project::ItemId(c)).is_some()
        && s.active_comp_id().map(|a| a.0) != Some(c)
    {
        // Steps without a `comp` act on the active comp: the suggestion's comp is opened first.
        s.open_comp(effectcraft_project::ItemId(c));
    }
    // Any command (not only the atomic-batch ones), rolled back as a whole when one fails.
    super::link_paste::atomic(s, &label, |s| {
        let mut results = Vec::with_capacity(sg.steps.len());
        for (i, st) in sg.steps.iter().enumerate() {
            let id = st.get("command").or_else(|| st.get("id")).and_then(Value::as_str).unwrap_or_default();
            let params = st.get("params").cloned().unwrap_or_else(|| json!({}));
            let r = s.execute_checked(id, params).map_err(|e| EngineError::Other(format!("step {} ({id}): {e}", i + 1)))?;
            results.push(r);
        }
        Ok(json!({"results": results}))
    })
}

fn accept(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "suggestion.accept";
    let taken = take(s, p, cmd)?;
    let mut kept = vec![];
    let mut failed = vec![];
    for (i, sg) in taken.iter().enumerate() {
        match run_one(s, sg) {
            Ok(_) => kept.push(json!({"id": sg.id, "n": sg.n, "label": sg.label})),
            Err(e) => {
                failed.push(json!({"id": sg.id, "error": e.to_string()}));
                // The ones not run yet stay waiting; a failed one stays too.
                let rest: Vec<Suggestion> = taken.get(i..).map(<[Suggestion]>::to_vec).unwrap_or_default();
                with_side(s, |a| {
                    for (k, r) in rest.into_iter().enumerate() {
                        a.suggestions.insert(k.min(a.suggestions.len()), r);
                    }
                });
                break;
            }
        }
    }
    if kept.is_empty()
        && let Some(f) = failed.first()
    {
        return Err(EngineError::Other(format!("suggestion.accept: {}", f["error"].as_str().unwrap_or("failed"))));
    }
    Ok(json!({"kept": kept, "failed": failed, "waiting": list(s).len()}))
}

fn reject(s: &mut Session, p: &Value) -> Result<Value> {
    let taken = take(s, p, "suggestion.reject")?;
    s.events.push(crate::Event::ProjectChanged { revision: s.revision });
    Ok(json!({"dropped": taken.iter().map(|sg| json!({"id": sg.id, "n": sg.n})).collect::<Vec<_>>(), "waiting": list(s).len()}))
}

fn list_cmd(s: &mut Session, _p: &Value) -> Result<Value> {
    Ok(json!({"suggestions": list(s)}))
}

/// Everything a scratch run changes, to put back.
struct Saved {
    project: Arc<Project>,
    history: crate::history::History,
    state: crate::EditorState,
    events: Vec<crate::Event>,
    journal: Vec<(String, Value)>,
    learn: Option<crate::learn::Progress>,
    key_move: Option<crate::KeyMove>,
    warp_pending: Vec<(effectcraft_project::ItemId, effectcraft_project::LayerId, effectcraft_project::Uid)>,
    camera_pending: Vec<(effectcraft_project::ItemId, effectcraft_project::LayerId, effectcraft_project::Uid)>,
    roto_pending: Vec<(effectcraft_project::ItemId, effectcraft_project::LayerId, effectcraft_project::Uid)>,
    analyses_checked: std::sync::Weak<Project>,
    revision: u64,
}

impl Saved {
    fn take(s: &Session) -> Saved {
        Saved {
            project: s.project.clone(),
            history: s.history.clone(),
            state: s.state.clone(),
            events: s.events.clone(),
            journal: s.journal.clone(),
            learn: s.learn.clone(),
            key_move: s.key_move.clone(),
            warp_pending: s.warp_pending.clone(),
            camera_pending: s.camera_pending.clone(),
            roto_pending: s.roto_pending.clone(),
            analyses_checked: s.analyses_checked.clone(),
            revision: s.revision,
        }
    }

    fn put_back(self, s: &mut Session) {
        s.project = self.project;
        s.history = self.history;
        s.state = self.state;
        s.events = self.events;
        s.journal = self.journal;
        s.learn = self.learn;
        s.key_move = self.key_move;
        s.warp_pending = self.warp_pending;
        s.camera_pending = self.camera_pending;
        s.roto_pending = self.roto_pending;
        s.analyses_checked = self.analyses_checked;
        s.sanitize_state();
        // TODO(batch1-verify): the revision goes back too, so nothing (frames, loopd's watcher)
        // sees a change; check that `bump`'s other side effects (freeze runtime) need no undo.
        s.revision = self.revision;
    }
}

/// The document with suggestion `id` applied, on a scratch copy: the session is exactly as it was
/// afterwards. For the viewer's hover preview.
pub fn preview(s: &mut Session, id: &str) -> Result<Arc<Project>> {
    let sg = list(s).into_iter().find(|sg| sg.id == id).ok_or_else(|| bad("suggestion.preview", format!("no suggestion `{id}`")))?;
    let saved = Saved::take(s);
    let r = run_one(s, &sg);
    let scratch = s.project.clone();
    saved.put_back(s);
    r.map(|_| scratch)
}

const fn spec(id: &'static str, label: &'static str, params: &'static str, run: super::Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, alt_shortcuts: &[], params, enabled: always, run, journal }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        // Proposing changes nothing (the accept is the user's action and is journaled).
        spec(
            "suggestion.propose",
            "Propose a Suggestion",
            "{label, steps: [{command, params?}] (as engine.batch), layers? ([layer ids] it touches), comp?, range? ([start, end] comp seconds it plays over)} → {id, n, waiting}: shown as a pill in the Timeline; nothing changes until the user keeps it (suggestion.accept)",
            propose,
            false,
        ),
        spec("suggestion.list", "List Suggestions", "{} → {suggestions: [{id, n, label, steps, layers, comp, range, by}]}", list_cmd, false),
        spec(
            "suggestion.accept",
            "Keep Suggestion",
            "{id | all: true} → {kept, failed, waiting}: each suggestion runs as one undo step",
            accept,
            true,
        ),
        spec("suggestion.reject", "Drop Suggestion", "{id | all: true} → {dropped, waiting}: nothing changes", reject, true),
    ]
}
