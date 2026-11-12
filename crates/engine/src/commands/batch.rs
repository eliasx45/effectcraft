//! `engine.batch`: run several commands in one call, as one undo step, with later steps able to
//! use what earlier ones returned (`"$1.layer"` is step 1's `layer` result). Agents build a
//! layer, animate it and style it in one round trip. Atomic batches accept only commands whose
//! document/editor changes can be restored; non-atomic failures retain completed-step receipts.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, batch_policy::atomic_supported, str_p};
use crate::{EngineError, Result, Session};

/// Nested batches and arbitrary script execution are unsupported even in non-atomic batches.
const NOT_IN_BATCH: &[&str] = &["engine.batch", "file.runScript", "script.run"];
const MAX_STEPS: usize = 1024;
const MAX_PARAM_DEPTH: usize = 64;

struct Step<'a> {
    id: &'a str,
    params: &'a Value,
    validation_error: Option<String>,
}

/// Validate every step before any command runs. References stay unresolved until their step.
fn preflight<'a>(steps: &'a [Value], atomic: bool, empty: &'a Value) -> Result<Vec<Step<'a>>> {
    if steps.len() > MAX_STEPS {
        return Err(bad("engine.batch", format!("at most {MAX_STEPS} steps may run in one batch")));
    }
    steps.iter().enumerate().map(|(i, step)| {
        let context = |why: String| bad("engine.batch", format!("step {}: {why}; no steps were run", i + 1));
        let obj = step.as_object().ok_or_else(|| context("each step must be an object".into()))?;
        if let Some(key) = obj.keys().find(|k| !matches!(k.as_str(), "command" | "id" | "params")) {
            return Err(context(format!("unknown step field `{key}`; expected command, id, params")));
        }
        if let (Some(command), Some(id)) = (obj.get("command"), obj.get("id"))
            && command != id
        {
            return Err(context("`command` and `id` disagree".into()));
        }
        let id = obj.get("command").or_else(|| obj.get("id")).and_then(Value::as_str)
            .filter(|id| !id.is_empty()).ok_or_else(|| context("each step needs a string `command` (or `id`)".into()))?;
        let spec = super::find(id);
        if NOT_IN_BATCH.contains(&id) {
            return Err(context(format!("`{id}` can't run inside a batch")));
        }
        if atomic && spec.is_none() {
            return Err(context(format!("unknown command `{id}`")));
        }
        if atomic && !atomic_supported(id) {
            return Err(context(format!("`{id}` is not supported in an atomic batch: external effects and unclassified commands cannot be rolled back; run it separately or use atomic:false")));
        }
        let params = obj.get("params").unwrap_or(empty);
        if !params.is_object() {
            return Err(context(format!("`{id}` params must be an object")));
        }
        if atomic && params.get("client").is_some() {
            return Err(context("set `client` on the batch, not individual atomic steps".into()));
        }
        let validation_error = match spec {
            Some(spec) => super::check_params(spec, params).err().map(|e| e.to_string()),
            None => Some(format!("unknown command `{id}`")),
        };
        if atomic && let Some(error) = &validation_error {
            return Err(context(error.clone()));
        }
        Ok(Step { id, params, validation_error })
    }).collect()
}

/// Resolve a `$N` / `$N.key.0.key` reference against the results so far.
fn reference(s: &str, results: &[Option<Value>]) -> Option<Result<Value>> {
    let rest = s.strip_prefix('$')?;
    let mut parts = rest.split('.');
    let n: usize = parts.next()?.parse().ok()?;
    let path: Vec<&str> = parts.collect();
    if path.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return None;
    }
    let Some(slot) = n.checked_sub(1).and_then(|i| results.get(i)) else {
        return Some(Err(bad("engine.batch", format!("`{s}` refers to step {n}, but only {} step(s) ran before", results.len()))));
    };
    let Some(mut v) = slot.as_ref() else {
        return Some(Err(bad("engine.batch", format!("`{s}` refers to failed step {n}, which has no result"))));
    };
    for p in &path {
        let next = match (v, p.parse::<usize>()) {
            (Value::Array(a), Ok(i)) => a.get(i),
            (Value::Object(o), _) => o.get(*p),
            _ => None,
        };
        match next {
            Some(x) => v = x,
            None => return Some(Err(bad("engine.batch", format!("`{s}`: step {n} returned {v}, which has no `{p}`")))),
        }
    }
    Some(Ok(v.clone()))
}

/// Replace every `$N…` string in `v` (recursively) with the referenced result.
fn substitute(v: &Value, results: &[Option<Value>]) -> Result<Value> {
    substitute_at(v, results, 0)
}

fn substitute_at(v: &Value, results: &[Option<Value>], depth: usize) -> Result<Value> {
    if depth >= MAX_PARAM_DEPTH {
        return Err(bad("engine.batch", format!("step parameters exceed {MAX_PARAM_DEPTH} levels of nesting")));
    }
    Ok(match v {
        Value::String(s) => match reference(s, results) {
            Some(r) => r?,
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute_at(x, results, depth + 1)).collect::<Result<_>>()?),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), substitute_at(x, results, depth + 1)?))).collect::<Result<_>>()?),
        _ => v.clone(),
    })
}

fn batch(s: &mut Session, p: &Value) -> Result<Value> {
    let steps = p.get("steps").and_then(Value::as_array).ok_or_else(|| bad("engine.batch", "missing `steps`: [{command, params?}, …]"))?;
    let label = str_p(p, "label").unwrap_or("Batch").to_string();
    if p.get("atomic").is_some_and(|v| !v.is_boolean()) {
        return Err(bad("engine.batch", "`atomic` must be a boolean; no steps were run"));
    }
    let atomic = p.get("atomic").and_then(Value::as_bool).unwrap_or(true);
    if p.get("stopOnError").is_some_and(|v| !v.is_boolean()) {
        return Err(bad("engine.batch", "`stopOnError` must be a boolean; no steps were run"));
    }
    let stop = p.get("stopOnError").and_then(Value::as_bool).unwrap_or(true);
    if atomic && !stop {
        return Err(bad("engine.batch", "`stopOnError:false` requires atomic:false; no steps were run"));
    }
    if p.get("groupUndo").is_some_and(|v| !v.is_boolean()) {
        return Err(bad("engine.batch", "`groupUndo` must be a boolean; no steps were run"));
    }
    let group_undo = p.get("groupUndo").and_then(Value::as_bool).unwrap_or(true);
    let empty = json!({});
    let steps = preflight(steps, atomic, &empty)?;
    let (project, history, state) = (s.project.clone(), s.history.clone(), s.state.clone());
    let (events, journal, learn) = (s.events.clone(), s.journal.clone(), s.learn.clone());
    let key_move = s.key_move.clone();
    let (warp_pending, camera_pending, roto_pending, analyses_checked) =
        (s.warp_pending.clone(), s.camera_pending.clone(), s.roto_pending.clone(), s.analyses_checked.clone());
    let mut results: Vec<Option<Value>> = Vec::with_capacity(steps.len());
    let mut receipts = Vec::with_capacity(steps.len());
    let mut execution_results = Vec::with_capacity(steps.len());
    let mut step_errors = Vec::new();
    let mut unknown_effects = false;
    for (i, step) in steps.iter().enumerate() {
        let mut started = false;
        let run = step.validation_error.as_ref().map_or_else(|| substitute(step.params, &results), |e| Err(EngineError::Other(e.clone()))).and_then(|params| {
            started = true;
            s.execute_checked(step.id, params)
        });
        match run {
            Ok(r) => {
                receipts.push(receipt(i, step.id, &r));
                execution_results.push(json!({"ok": true, "result": r}));
                results.push(Some(r));
            }
            Err(e) => {
                if atomic {
                    s.project = project;
                    s.history = history;
                    s.state = state;
                    s.events = events;
                    s.journal = journal;
                    s.learn = learn;
                    s.key_move = key_move;
                    s.warp_pending = warp_pending;
                    s.camera_pending = camera_pending;
                    s.roto_pending = roto_pending;
                    s.analyses_checked = analyses_checked;
                    s.sanitize_state();
                    s.bump();
                    return Err(EngineError::Other(format!("step {} ({}): {e}; the batch was rolled back", i + 1, step.id)));
                }
                let message = format!("step {} ({}): {e}", i + 1, step.id);
                execution_results.push(json!({"ok": false, "error": message}));
                step_errors.push(json!({"step": i + 1, "command": step.id, "error": message,
                    "outcome": if started { "unknown" } else { "not_run" }, "notRun": !started}));
                unknown_effects |= started;
                if stop {
                    break;
                }
                results.push(None);
            }
        }
    }
    if group_undo {
        fold(s, &project, &label, &history);
    }
    // Steps' warnings (clamped or coerced values), gathered so they aren't missed.
    let warnings: Vec<String> = results
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            let id = steps.get(i).map(|st| st.id).unwrap_or("?");
            r.as_ref().map(step_warnings).unwrap_or_default().into_iter().map(move |w| format!("step {} ({id}): {w}", i + 1))
        })
        .collect();
    let mut out = json!({"steps": results.len(), "results": results});
    if !atomic {
        out["completed"] = json!(receipts.len());
        out["failed"] = json!(step_errors.len());
        out["executionResults"] = json!(execution_results);
        if let Some(first) = step_errors.first() {
            out["ok"] = json!(false);
            out["error"] = first.get("error").cloned().unwrap_or(Value::Null);
            out["failedStep"] = first.get("step").cloned().unwrap_or(Value::Null);
            out["command"] = first.get("command").cloned().unwrap_or(Value::Null);
            out["failedOutcome"] = first.get("outcome").cloned().unwrap_or(Value::Null);
            out["notRun"] = first.get("notRun").cloned().unwrap_or(Value::Null);
        }
        out["stepErrors"] = json!(step_errors);
        out["atomic"] = json!(false);
        out["rolledBack"] = json!(false);
        out["appliedSteps"] = json!(receipts);
        add_file_summary(&mut out, unknown_effects);
    }
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

/// Returned paths are confirmed outputs only for these commands, never property/group paths.
fn receipt(i: usize, id: &str, result: &Value) -> Value {
    let key = match id {
        "file.save"
        | "file.saveAs"
        | "file.saveCopy"
        | "file.saveCopyAsXml"
        | "file.incrementAndSave"
        | "file.autoSave"
        | "file.exportLottie"
        | "file.exportLayers"
        | "file.exportComp"
        | "comp.saveFrameAs"
        | "comp.saveFrameAsPsd"
        | "comp.saveFrameAsExr"
        | "view.exportGuides"
        | "anim.savePreset"
        | "essential.exportTemplate"
        | "templates.saveAs"
        | "journal.export"
        | "help.exportRepro"
        | "shortcuts.export"
        | "render.saveCurrentPreview"
        | "render.frame"
        | "render.contactSheet" => Some("path"),
        "file.collectFiles" => Some("project"),
        _ => None,
    };
    let files: Vec<&str> = key.and_then(|k| result.get(k)).and_then(Value::as_str).into_iter().collect();
    json!({"step": i + 1, "command": id, "outcome": "completed", "files": files,
        "fileCoverage": if atomic_supported(id) { "no_external_writes" } else { "reported_paths_only" }})
}

fn add_file_summary(out: &mut Value, failed_unknown: bool) {
    let receipts = out.get("appliedSteps").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let files: Vec<Value> = receipts.iter().filter_map(|r| r.get("files").and_then(Value::as_array)).flatten().cloned().collect();
    let incomplete = failed_unknown || receipts.iter().any(|r| r.get("fileCoverage").and_then(Value::as_str) != Some("no_external_writes"));
    out["files"] = json!(files);
    out["fileCoverage"] = json!(if incomplete { "reported_paths_only" } else { "no_external_writes" });
    out["untrackedEffectsPossible"] = json!(incomplete);
}

/// The `warnings` of a step's result (also inside an array result, e.g. prop.set on a Position
/// with separated dimensions).
fn step_warnings(r: &Value) -> Vec<String> {
    match r {
        Value::Object(o) => {
            o.get("warnings").and_then(Value::as_array).map(|w| w.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        }
        Value::Array(a) => a.iter().flat_map(step_warnings).collect(),
        _ => vec![],
    }
}

/// Fold the undo steps recorded since the project was `snap` into one step called `label`.
/// `before` is the history when the group began: a group with more steps than Undo Levels
/// still ends as one step, with the older steps kept ([`crate::history::History::fold`]).
pub(crate) fn fold(s: &mut Session, snap: &Arc<effectcraft_project::Project>, label: &str, before: &crate::history::History) {
    let levels = s.prefs.general.undo_levels.max(1) as usize;
    let current = s.project.clone();
    s.history.fold(snap, label, before, &current, levels);
    s.history.set_author(&s.project, s.client.clone());
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "engine.batch",
        label: "Run Commands (Batch)",
        menu: &[],
        shortcut: None,
        alt_shortcuts: &[],
        params: "{steps: [{command, params?}], label? (undo step name, default Batch), atomic?: bool (default true: only approved document/editor commands; external effects rejected before any step), stopOnError?: bool (default true; false requires atomic:false), groupUndo?: bool (default true: one undo group; false preserves each command's undo step)} → {steps, results, warnings?}; atomic:false also returns executionResults, completed/failed, stepErrors, appliedSteps, files and fileCoverage (reported paths may not cover every external effect), or {ok:false, error, failedStep, failedOutcome, rolledBack:false, results, appliedSteps, files, fileCoverage}. At most 1024 steps. A string param \"$N\" or \"$N.key.0\" is step N's result (1-based), e.g. {\"layer\": \"$1.layer\"}; failed steps have null result slots and cannot be referenced.",
        enabled: always,
        run: batch,
        // The steps journal themselves.
        journal: false,
    }]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use crate::{Event, Services, Session};

    #[derive(Default)]
    struct Files {
        written: Mutex<BTreeMap<String, Vec<u8>>>,
        fail_after_write: bool,
    }

    impl Services for Files {
        fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
            self.written.lock().unwrap().get(path).cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, path.to_string()))
        }

        fn write_file(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
            self.written.lock().unwrap().insert(path.to_string(), bytes.to_vec());
            if self.fail_after_write { Err(std::io::Error::other("injected failure after writing bytes")) } else { Ok(()) }
        }
    }

    #[test]
    fn batch_refs_one_undo_step_and_rollback() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "B", "width": 100, "height": 100})).unwrap();
        let undo_before = s.history.undo.len();
        let r = s
            .execute_checked(
                "engine.batch",
                json!({"label": "Build", "steps": [
                    {"command": "layer.newSolid", "params": {"name": "Box", "color": "#ff0000"}},
                    {"command": "prop.set", "params": {"layer": "$1.layer", "path": "transform/opacity", "value": 40}},
                    {"command": "layer.rename", "params": {"layer": "$1.layer", "name": "Red Box"}},
                ]}),
            )
            .unwrap();
        assert_eq!(r["steps"], 3);
        let lid = r["results"][0]["layer"].as_u64().unwrap();
        let comp = s.active_comp().unwrap();
        let l = comp.layer(effectcraft_project::LayerId(lid)).unwrap();
        assert_eq!(l.name, "Red Box");
        assert_eq!(l.props.prop("transform/opacity").unwrap().value.as_f64(), 40.0);
        assert_eq!(s.history.undo.len(), undo_before + 1, "one undo step");
        assert_eq!(s.history.undo.last().unwrap().0, "Build");
        assert!(s.undo());
        assert!(s.active_comp().unwrap().layers.is_empty());
        assert!(s.redo());

        // A failing step rolls back the earlier ones and names the step.
        let n = s.active_comp().unwrap().layers.len();
        let e = s
            .execute_checked(
                "engine.batch",
                json!({"steps": [
                    {"command": "layer.newNull", "params": {}},
                    {"command": "prop.set", "params": {"layer": "$1.layer", "path": "transform/nope", "value": 1}},
                ]}),
            )
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("step 2 (prop.set)") && e.contains("rolled back"), "{e}");
        assert_eq!(s.active_comp().unwrap().layers.len(), n);
        // Bad references are reported.
        let e = s.execute_checked("engine.batch", json!({"steps": [{"command": "layer.rename", "params": {"layer": "$3.layer", "name": "x"}}]})).unwrap_err();
        assert!(e.to_string().contains("refers to step 3"), "{e}");
    }

    #[test]
    fn atomic_side_effects_are_rejected_before_any_step() {
        for command in [
            "file.save",
            "file.saveAs",
            "file.saveCopy",
            "file.open",
            "file.newProject",
            "file.exportComp",
            "comp.saveFrameAs",
            "render.frame",
            "renderQueue.render",
            "prefs.set",
            "host.release",
            "hosts",
        ] {
            let files = Arc::new(Files::default());
            let mut s = Session { services: files.clone(), ..Default::default() };
            s.execute("comp.new", json!({"name": "Before", "width": 16, "height": 16})).unwrap();
            let (project, revision, events, journal, undo) = (s.project.clone(), s.revision, s.events.clone(), s.journal.clone(), s.history.undo.len());
            let err = s
                .execute_checked(
                    "engine.batch",
                    json!({"steps": [
                        {"command": "layer.newNull"}, {"command": command, "params": {"path": "never-written.ecproj"}}
                    ]}),
                )
                .unwrap_err()
                .to_string();
            assert!(err.contains("no steps were run"), "{command}: {err}");
            assert!(Arc::ptr_eq(&project, &s.project));
            assert_eq!(s.revision, revision);
            assert_eq!(s.events, events);
            assert_eq!(s.journal, journal);
            assert_eq!(s.history.undo.len(), undo);
            assert!(files.written.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn every_step_shape_and_id_is_preflighted() {
        for atomic in [true, false] {
            for invalid in [
                json!(null),
                json!({}),
                json!({"command": "layer.rename", "params": 1}),
                json!({"command": "layer.rename", "id": "layer.newNull"}),
                json!({"command": "layer.newNull", "extra": true}),
            ] {
                let mut s = Session::default();
                s.execute("comp.new", json!({"name": "Before"})).unwrap();
                let project = s.project.clone();
                assert!(s.execute_checked("engine.batch", json!({"atomic": atomic, "steps": [{"command": "layer.newNull"}, invalid]})).is_err());
                assert!(Arc::ptr_eq(&project, &s.project));
            }
        }
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Before"})).unwrap();
        let revision = s.revision;
        assert!(
            s.execute_checked(
                "engine.batch",
                json!({"steps": [
                    {"command": "layer.newNull"}, {"command": "layer.newNull", "params": {"unknown": 1}}
                ]})
            )
            .is_err()
        );
        assert_eq!(s.revision, revision);
        assert!(s.active_comp().unwrap().layers.is_empty());
    }

    #[test]
    fn atomic_rollback_restores_events_journal_and_analysis_queues() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Before"})).unwrap();
        let (events, journal) = (s.events.clone(), s.journal.clone());
        let pending = (s.warp_pending.clone(), s.camera_pending.clone(), s.roto_pending.clone());
        let error = s
            .execute_checked(
                "engine.batch",
                json!({"steps": [
                    {"command": "comp.new", "params": {"name": "Transient"}},
                    {"command": "layer.rename", "params": {"layer": "Missing", "name": "Fail"}}
                ]}),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("the batch was rolled back"));
        assert!(!error.contains("nothing changed"));
        assert_eq!(s.project.comps().count(), 1);
        assert_eq!(s.journal, journal);
        assert_eq!(s.events.get(..events.len()), Some(events.as_slice()));
        assert_eq!(s.events.len(), events.len() + 1);
        assert!(matches!(s.events.last(), Some(Event::ProjectChanged { .. })));
        assert_eq!((s.warp_pending, s.camera_pending, s.roto_pending), pending);
    }

    #[test]
    fn non_atomic_failure_retains_applied_steps_and_file_receipts() {
        let files = Arc::new(Files::default());
        let mut s = Session { services: files.clone(), ..Default::default() };
        s.execute("comp.new", json!({"name": "Before"})).unwrap();
        let result = s
            .execute_checked(
                "engine.batch",
                json!({"atomic": false, "steps": [
                    {"command": "layer.newNull", "params": {"name": "Kept"}},
                    {"command": "file.saveAs", "params": {"path": "reported-output.ecproj"}},
                    {"command": "layer.rename", "params": {"layer": "Missing", "name": "Fail"}}
                ]}),
            )
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["rolledBack"], false);
        assert_eq!(result["failedStep"], 3);
        assert_eq!(result["failedOutcome"], "unknown");
        assert_eq!(result["steps"], 2);
        assert_eq!(result["appliedSteps"][1]["command"], "file.saveAs");
        assert_eq!(result["results"][1]["path"], "reported-output.ecproj");
        assert_eq!(result["files"], json!(["reported-output.ecproj"]));
        assert_eq!(result["fileCoverage"], "reported_paths_only");
        assert_eq!(result["untrackedEffectsPossible"], true);
        assert_eq!(s.active_comp().unwrap().layers.len(), 1);
        assert!(files.written.lock().unwrap().contains_key("reported-output.ecproj"));
    }

    #[test]
    fn a_failed_writer_does_not_claim_no_files_changed() {
        let files = Arc::new(Files { fail_after_write: true, ..Default::default() });
        let mut s = Session { services: files.clone(), ..Default::default() };
        let result = s
            .execute_checked(
                "engine.batch",
                json!({"atomic": false, "steps": [
                    {"command": "file.saveAs", "params": {"path": "partial-output.ecproj"}}
                ]}),
            )
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["appliedSteps"], json!([]));
        assert_eq!(result["files"], json!([]));
        assert_eq!(result["failedOutcome"], "unknown");
        assert_eq!(result["untrackedEffectsPossible"], true);
        assert!(files.written.lock().unwrap().contains_key("partial-output.ecproj"));
    }

    #[test]
    fn failed_reference_is_not_run_and_property_paths_are_not_files() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "B"})).unwrap();
        let result = s
            .execute_checked(
                "engine.batch",
                json!({"atomic": false, "steps": [
                    {"command": "layer.newShape"},
                    {"command": "layer.addShapeItem", "params": {"layer": "$1.layer", "kind": "trim"}},
                    {"command": "layer.rename", "params": {"layer": "$9.layer", "name": "Fail"}}
                ]}),
            )
            .unwrap();
        assert_eq!(result["notRun"], true);
        assert_eq!(result["failedOutcome"], "not_run");
        assert_eq!(result["files"], json!([]));
        assert_eq!(result["fileCoverage"], "no_external_writes");
        assert_eq!(result["untrackedEffectsPossible"], false);
    }

    #[test]
    fn parameter_substitution_is_depth_bounded() {
        let mut value = json!(1);
        for _ in 0..super::MAX_PARAM_DEPTH {
            value = json!([value]);
        }
        assert!(super::substitute(&value, &[]).is_err());
    }

    #[test]
    fn batch_limits_and_atomic_type_are_checked_before_mutation() {
        let mut s = Session::default();
        let before = s.project.clone();
        let steps = vec![json!({"command": "comp.new"}); super::MAX_STEPS + 1];
        assert!(s.execute_checked("engine.batch", json!({"steps": steps})).is_err());
        assert!(Arc::ptr_eq(&before, &s.project));
        assert!(s.execute_checked("engine.batch", json!({"atomic": "false", "steps": [{"command": "comp.new"}]})).is_err());
        assert!(Arc::ptr_eq(&before, &s.project));
    }

    #[test]
    fn non_atomic_continue_preserves_step_indices_and_failed_references() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "B"})).unwrap();
        let result = s
            .execute_checked(
                "engine.batch",
                json!({"atomic": false, "stopOnError": false, "steps": [
                    {"command": "no.such.command"},
                    {"command": "layer.newNull", "params": {"name": "Kept"}},
                    {"command": "prop.set", "params": {"layer": "$2.layer", "path": "transform/opacity", "value": 40}},
                    {"command": "layer.rename", "params": {"layer": "$1", "name": "Invalid reference"}},
                    {"command": "layer.rename", "params": {"layer": "$2.layer", "name": "Final"}}
                ]}),
            )
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["completed"], 3);
        assert_eq!(result["failed"], 2);
        assert_eq!(result["results"].as_array().unwrap().len(), 5);
        assert_eq!(result["results"][0], serde_json::Value::Null);
        assert_eq!(result["results"][3], serde_json::Value::Null);
        assert_eq!(result["executionResults"].as_array().unwrap().len(), 5);
        assert_eq!(result["executionResults"][0]["ok"], false);
        assert_eq!(result["executionResults"][1]["ok"], true);
        assert!(result["executionResults"][3]["error"].as_str().unwrap().contains("failed step 1"));
        assert_eq!(result["stepErrors"][1]["step"], 4);
        assert_eq!(result["appliedSteps"][2]["step"], 5);
        assert_eq!(result["fileCoverage"], "no_external_writes");
        assert_eq!(s.active_comp().unwrap().layers[0].name, "Final");
        assert_eq!(s.active_comp().unwrap().layers[0].props.prop("transform/opacity").unwrap().value.as_f64(), 40.0);
    }

    #[test]
    fn revision_guard_is_checked_once_for_the_whole_non_atomic_call() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "B"})).unwrap();
        let revision = s.revision;
        let undo_before = s.history.undo.len();
        let steps = json!([
            {"command": "layer.newNull", "params": {"name": "One"}},
            {"command": "layer.newNull", "params": {"name": "Two"}},
            {"command": "layer.rename", "params": {"layer": "$2.layer", "name": "Final"}}
        ]);
        let result = s
            .execute_checked("engine.batch", json!({"atomic": false, "stopOnError": false, "groupUndo": false, "expectRevision": revision, "steps": steps}))
            .unwrap();
        assert_eq!(result["completed"], 3);
        assert_eq!(result["failed"], 0);
        assert_eq!(s.history.undo.len(), undo_before + 3);
        assert!(s.revision > revision);
        let project = s.project.clone();
        assert!(s.execute_checked("engine.batch", json!({"atomic": false, "stopOnError": false, "expectRevision": revision, "steps": steps})).is_err());
        assert!(Arc::ptr_eq(&project, &s.project));
    }

    #[test]
    fn non_atomic_default_stops_at_validation_error_without_running_later_steps() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "B"})).unwrap();
        let result = s
            .execute_checked(
                "engine.batch",
                json!({"atomic": false, "steps": [
                    {"command": "no.such.command"}, {"command": "layer.newNull"}
                ]}),
            )
            .unwrap();
        assert_eq!(result["completed"], 0);
        assert_eq!(result["failed"], 1);
        assert_eq!(result["executionResults"].as_array().unwrap().len(), 1);
        assert_eq!(result["failedOutcome"], "not_run");
        assert!(s.active_comp().unwrap().layers.is_empty());
        let project = s.project.clone();
        assert!(s.execute_checked("engine.batch", json!({"stopOnError": false, "steps": [{"command": "layer.newNull"}]})).is_err());
        assert!(Arc::ptr_eq(&project, &s.project));
    }
}
