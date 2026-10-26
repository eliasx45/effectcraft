//! The handover gate (A/B round 3): a client's last `taste.check` and `render.lint` are
//! remembered, and its own edits after them to layers in the range they covered make
//! `agent.notices` say once that they are stale; `handover.check` runs both over the comp's
//! animated range and says whether the comp is ready to hand over.

use std::collections::HashMap;
use std::sync::Arc;

use effectcraft_project::{ItemId, LayerId, Project};
use serde_json::{Value, json};

use super::{CommandSpec, comp_id, f_p};
use crate::{Result, Session, query};

/// A check a client ran: what it covered and the project it judged.
#[derive(Clone)]
pub struct Check {
    revision: u64,
    epoch: u64,
    comp: ItemId,
    start: f64,
    end: f64,
    /// Its stale notice was given (once per check).
    noticed: bool,
}

/// A client's last taste check and lint.
#[derive(Clone, Default)]
pub struct Gate {
    taste: Option<Check>,
    lint: Option<Check>,
}

/// Each client's gate ([`Session::handover_gates`]).
pub type Gates = HashMap<String, Gate>;

/// Lists in a `handover.check` reply are cut to this many entries (counts stay whole).
const LIST_MAX: usize = 30;

/// Remember a check the calling client just ran (`Session::execute_inner` calls this after every
/// command; only `taste.check` and `render.lint` by a client count).
pub(crate) fn noted(s: &mut Session, id: &str, p: &Value, r: &Value) {
    let Some(me) = s.client.clone() else { return };
    let kind = match id {
        "taste.check" | "render.lint" => id,
        _ => return,
    };
    let Some(comp) = r.get("comp").and_then(Value::as_u64).map(ItemId) else { return };
    let duration = s.project.comp(comp).map_or(0.0, |c| c.duration.seconds());
    let (start, end) = if kind == "render.lint" {
        // The times it sampled.
        let times: Vec<f64> = r.get("times").and_then(Value::as_array).map(|t| t.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        (times.iter().copied().fold(f64::INFINITY, f64::min), times.iter().copied().fold(f64::NEG_INFINITY, f64::max))
    } else {
        (f_p(p, "start").unwrap_or(0.0), f_p(p, "end").unwrap_or(duration))
    };
    if !start.is_finite() || !end.is_finite() {
        return;
    }
    let check = Check { revision: s.revision, epoch: s.epoch, comp, start, end, noticed: false };
    let gate = s.handover_gates.entry(me).or_default();
    if kind == "taste.check" {
        gate.taste = Some(check);
    } else {
        gate.lint = Some(check);
    }
}

/// The layers `client` itself changed in `check`'s comp since it ran, that are on screen somewhere
/// in its range: (id, name).
fn changed_since(s: &Session, client: &str, check: &Check) -> Vec<(LayerId, String)> {
    if check.epoch != s.epoch || check.revision >= s.revision {
        return Vec::new();
    }
    let mut out: Vec<(LayerId, String)> = Vec::new();
    for e in s.feed.since(check.revision).filter(|e| e.client.as_deref() == Some(client)) {
        let (before, after) = e.projects();
        for (id, name) in changed_layers(before, after, check) {
            if !out.iter().any(|(l, _)| *l == id) {
                out.push((id, name));
            }
        }
    }
    out
}

/// Layers of `check.comp` that differ between `before` and `after` (added ones too) and overlap
/// the check's range.
fn changed_layers(before: &Arc<Project>, after: &Arc<Project>, check: &Check) -> Vec<(LayerId, String)> {
    let (Some(old), Some(new)) = (before.comp(check.comp), after.comp(check.comp)) else { return Vec::new() };
    if Arc::ptr_eq(before, after) {
        return Vec::new();
    }
    new.layers
        .iter()
        .filter(|l| old.layer(l.id) != Some(*l))
        .filter(|l| l.in_point.seconds() <= check.end && l.out_point.seconds() >= check.start)
        .map(|l| (l.id, l.name.clone()))
        .collect()
}

/// Whether `client`'s checks still hold: the layers it changed since its last taste check / lint
/// (empty: fresh, or no check yet).
fn stale_layers(s: &Session, client: &str) -> Vec<(LayerId, String)> {
    let Some(gate) = s.handover_gates.get(client) else { return Vec::new() };
    let mut out: Vec<(LayerId, String)> = Vec::new();
    for check in [&gate.taste, &gate.lint].into_iter().flatten() {
        for l in changed_since(s, client, check) {
            if !out.iter().any(|(id, _)| *id == l.0) {
                out.push(l);
            }
        }
    }
    out
}

/// For `agent.notices`: once per check, that the client changed layers its last taste check /
/// lint covered. None when nothing went stale or it was said already.
pub(crate) fn stale_notice(s: &mut Session, client: &str) -> Option<String> {
    let gate = s.handover_gates.get(client)?;
    let mut which = Vec::new();
    let mut layers: Vec<String> = Vec::new();
    let mut said = Vec::new();
    for (name, check) in [("taste_check", &gate.taste), ("lint", &gate.lint)] {
        let Some(check) = check.as_ref().filter(|c| !c.noticed) else { continue };
        let changed = changed_since(s, client, check);
        if changed.is_empty() {
            continue;
        }
        which.push(name);
        said.push(name);
        for (_, n) in changed {
            if !layers.contains(&n) {
                layers.push(n);
            }
        }
    }
    if which.is_empty() {
        return None;
    }
    if let Some(gate) = s.handover_gates.get_mut(client) {
        for (name, check) in [("taste_check", &mut gate.taste), ("lint", &mut gate.lint)] {
            if let Some(c) = check.as_mut().filter(|_| said.contains(&name)) {
                c.noticed = true;
            }
        }
    }
    let shown: Vec<&str> = layers.iter().take(8).map(String::as_str).collect();
    let more = layers.len().saturating_sub(shown.len());
    let more = if more > 0 { format!(" +{more} more") } else { String::new() };
    Some(format!("keys changed since your last {} (layers {}{more}); rerun before handover (handover_check)", which.join("/"), shown.join(", ")))
}

/// The comp's animated range: the first to the last keyframed move taste.check found, else the
/// work area.
fn animated_range(s: &Session, cid: ItemId, taste: &Value) -> (f64, f64) {
    let moves = taste.get("moves").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let t = |m: &Value, k: &str| m.get(k).and_then(|v| v.get("time")).and_then(Value::as_f64);
    let start = moves.iter().filter_map(|m| t(m, "from")).fold(f64::INFINITY, f64::min);
    let end = moves.iter().filter_map(|m| t(m, "to")).fold(f64::NEG_INFINITY, f64::max);
    if start.is_finite() && end.is_finite() && end >= start {
        return (start, end);
    }
    s.project.comp(cid).map_or((0.0, 0.0), |c| (c.work_area.0.seconds(), c.work_area.1.seconds()))
}

/// `handover.check`: taste.check + render.lint over the animated range in one call.
fn check(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let taste = s.execute("taste.check", json!({"comp": cid.0, "problemsOnly": true, "start": f_p(p, "start"), "end": f_p(p, "end")}))?;
    let (auto_start, auto_end) = animated_range(s, cid, &taste);
    let start = f_p(p, "start").unwrap_or(auto_start);
    let end = f_p(p, "end").unwrap_or(auto_end).max(start);
    let mut lint_p = json!({"comp": cid.0, "start": start, "end": end});
    if let Some(step) = f_p(p, "step") {
        lint_p["step"] = json!(step);
    }
    let lint = s.execute("render.lint", lint_p)?;
    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    for m in taste.get("moves").and_then(Value::as_array).into_iter().flatten() {
        let entry = json!({
            "source": "taste", "layer": m["layer"], "layerName": m["layerName"], "path": m["path"],
            "time": m["from"]["time"], "until": m["to"]["time"], "mark": m["mark"], "message": m["message"], "fix": m["fix"],
        });
        if m["blocking"] == true {
            blocking.push(entry);
        } else {
            warnings.push(entry);
        }
    }
    for pr in lint.get("problems").and_then(Value::as_array).into_iter().flatten() {
        let entry = json!({
            "source": "lint", "rule": pr["rule"], "layer": pr["layer"], "layerName": pr["layerName"],
            "time": pr["time"], "until": pr["until"], "message": pr["message"], "fix": pr.get("fix").cloned().unwrap_or(Value::Null),
        });
        match pr["severity"].as_str() {
            Some("error") => blocking.push(entry),
            Some("warning") => warnings.push(entry),
            _ => {}
        }
    }
    let stale: Vec<String> = s.client.clone().map(|me| stale_layers(s, &me).into_iter().map(|(_, n)| n).collect()).unwrap_or_default();
    let ready = blocking.is_empty() && stale.is_empty();
    let (nb, nw) = (blocking.len(), warnings.len());
    let text = if ready && nw == 0 {
        "ready: no blocking problems or warnings".to_string()
    } else if ready {
        format!("ready: no blocking problems; {nw} warning(s), each with a reason to leave it or a fix")
    } else {
        format!("not ready: {nb} blocking problem(s) to fix, {nw} warning(s)")
    };
    blocking.truncate(LIST_MAX);
    warnings.truncate(LIST_MAX);
    Ok(json!({
        "ready": ready,
        "comp": cid.0,
        "compName": s.project.item(cid).map(|i| i.name.as_str()).unwrap_or(""),
        "range": {"start": start, "end": end},
        "counts": {"blocking": nb, "warnings": nw},
        "blocking": blocking,
        "warnings": warnings,
        "taste": taste["summary"]["line"],
        "lint": lint["counts"],
        "stale": stale,
        "text": text,
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![query!(
        "handover.check",
        "Handover Check",
        "{comp?, start? (comp s), end? (comp s; default the animated range: first to last keyframed move, else the work area), step? (lint sampling, s)} → {ready, comp, compName, range {start, end}, counts {blocking, warnings}, blocking: [...], warnings: [...] (each {source: taste|lint, layer, layerName, time, until, message, fix?, path?|rule?}; at most 30 each), taste (summary line), lint (counts), stale (layers changed since the client's checks), text}. Runs taste.check and render.lint over the comp's animated range in one call: ready is false while a blocking taste ❌ or a lint error remains. Run it last, after the final edit, before handing a comp over.",
        check
    )]
}
