//! The ease taste check ([`crate::taste`]) as commands: `taste.check` judges a comp's moves
//! against Elias's profile (✅ approved curve · 🟡 in his usual range · ❌ off), `taste.apply`
//! puts one of his curves on a move (the one-call fix every finding carries), `taste.graph`
//! draws a property's value and speed graphs as a PNG, `taste.profile` lists the curves and
//! rules, `taste.learn` finds curves in his comps the profile doesn't know, `taste.propose`
//! files a ⏳ candidate, and `taste.approve` (Elias only, never an agent client) approves or
//! rejects one. `taste.rebuild` marks a comp as a rebuild of a reference and stores the reference
//! editor's fitted ease per move (rebuild mode, [`crate::taste`]); its fits become ⏳ candidates
//! tagged `reference <name>`. The lint rule `off-taste` (`render.lint`) runs the same check.

use std::sync::Arc;

use effectcraft_project::rebuild as taste_rebuild;
use effectcraft_project::{ItemId, LayerId};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::peak::{Graph, apply_graph};
use super::prop::prop_ref;
use super::{CommandSpec, b_p, bad, comp_id, f_p, has_comp, layers_p, str_p, unlocked};
use crate::readout::short_timecode;
use crate::taste::{self, Curve, Judged, MoveType, Profile, Side, Status, Stored, Verdict};
use crate::{EngineError, Result, Session, cmd, query};

/// Most comps `taste.learn` reads.
const MAX_LEARN_COMPS: usize = 400;
/// Most curve groups `taste.learn` returns.
const MAX_GROUPS: usize = 60;

fn profile(s: &Session) -> (Profile, Stored) {
    Profile::load(s.config.as_deref())
}

fn save(s: &Session, stored: &Stored, cmd: &str) -> Result<()> {
    let store = s.config.as_deref().ok_or_else(|| bad(cmd, "no settings store in this session (headless run): the profile can't be saved"))?;
    let text = serde_json::to_string_pretty(stored).map_err(|e| bad(cmd, e.to_string()))?;
    store.write(taste::PROFILE_FILE, &text).map_err(|e| bad(cmd, format!("couldn't save the taste profile: {e}")))
}

fn side_json(s: Side) -> Value {
    json!({"speed": (s.speed * 10_000.0).round() / 10_000.0, "influence": (s.influence * 1000.0).round() / 1000.0})
}

fn curve_json(p: &Profile, c: &Curve) -> Value {
    let mut o = json!({
        "id": c.id, "name": c.name, "status": c.status.id(), "mark": c.status.mark(),
        "out": side_json(c.out), "in": side_json(c.inn),
        "roles": c.roles.iter().map(|r| r.id()).collect::<Vec<_>>(),
        "source": c.source,
    });
    if let Some(f) = &c.family {
        o["family"] = json!(f);
    }
    if !c.note.is_empty() {
        o["note"] = json!(c.note);
    }
    if !c.said.is_empty() {
        o["said"] = json!(c.said);
    }
    let _ = p;
    o
}

/// The `taste.apply` call that puts curve `id` on move `m`.
fn fix_call(cid: ItemId, m: &Judged, id: &str) -> Value {
    json!({"command": "taste.apply", "params": {"comp": cid.0, "layer": m.at.layer.0, "path": m.path, "key": m.at.key, "curve": id}})
}

pub(crate) fn move_json(s: &Session, cid: ItemId, m: &Judged) -> Value {
    let comp = s.project.comp(cid);
    let fps = comp.map(|c| c.frame_rate.as_f64()).unwrap_or(30.0);
    let tc = |t: f64| comp.map(|c| short_timecode(c, effectcraft_time::Tick::from_seconds_f64(t))).unwrap_or_default();
    let mut o = json!({
        "layer": m.at.layer.0, "layerName": m.layer_name, "path": m.path, "property": m.property,
        "key": m.at.key,
        "from": {"time": (m.t0 * 1000.0).round() / 1000.0, "tc": tc(m.t0), "frame": (m.t0 * fps).round()},
        "to": {"time": (m.t1 * 1000.0).round() / 1000.0, "tc": tc(m.t1), "frame": (m.t1 * fps).round()},
        "frames": m.frames,
        "moveType": m.kind.id(),
        "verdict": m.verdict.id(), "mark": m.verdict.mark(),
        "blocking": m.verdict == Verdict::Off && m.blocking,
        "message": m.message,
    });
    if let Some((out, inn)) = m.ease {
        o["ease"] = json!({"out": side_json(out), "in": side_json(inn)});
    }
    if let Some(sh) = m.shape {
        o["criteria"] = json!({
            "peakPosition": (sh.peak_pos * 10.0).round() / 10.0,
            "sharpness": (sh.sharpness * 100.0).round() / 100.0,
            "tail10Frames": sh.tail10,
        });
        if let Some(px) = m.px_per_frame {
            o["criteria"]["pxPerFrame"] = json!(px);
        }
        if let Some(pct) = m.frame_pct {
            o["criteria"]["framePerFrame"] = json!(pct);
        }
    }
    if let Some(c) = &m.curve {
        o["curve"] = json!(c);
    }
    if let Some((id, name, d)) = &m.nearest {
        o["nearest"] = json!({"id": id, "name": name, "gap": d});
        if m.verdict != Verdict::Approved && m.reference.is_none() {
            o["fix"] = fix_call(cid, m, id);
        }
    }
    if let Some(r) = &m.reference {
        let mut j = json!({
            "name": r.name, "curve": r.curve,
            "ease": {"out": side_json(r.ease.0), "in": side_json(r.ease.1)},
            "gap": r.gap, "tolerance": r.tolerance, "evidence": r.evidence,
            "profile": {"verdict": r.profile.id(), "mark": r.profile.mark(), "problems": r.profile_problems, "message": r.profile_message},
        });
        if let Some(f) = &r.fit {
            j["fit"] = serde_json::to_value(f).unwrap_or(Value::Null);
        }
        if let Some(p) = r.fit_pct {
            j["fitPct"] = json!(p);
        }
        o["reference"] = j;
        if m.verdict == Verdict::Off {
            o["fix"] = fix_call(cid, m, "reference");
        }
    }
    if let Some(e) = &m.ease_source {
        o["referenceEase"] = match &e.curve {
            Some((c, gap)) => json!({
                "why": e.why, "status": c.status.id(), "mark": c.status.mark(),
                "id": c.id, "name": c.name, "source": c.source, "gap": gap,
            }),
            None => json!({"why": e.why, "status": "none", "mark": "", "note": "no profile curve has this shape yet"}),
        };
    }
    if !m.problems.is_empty() {
        o["problems"] = json!(m.problems);
    }
    o
}

/// A rebuild comp's state for replies: its reference and how its evidence was used by `moves`.
fn rebuild_json(rb: &taste_rebuild::Rebuild, moves: &[Judged]) -> Value {
    let judged: Vec<&Judged> = moves.iter().filter(|m| m.reference.is_some()).collect();
    let n = |v: Verdict| judged.iter().filter(|m| m.verdict == v).count();
    let unused: Vec<Value> = taste::unused_evidence(rb, moves)
        .into_iter()
        .filter_map(|i| rb.moves.get(i).map(|m| json!({"evidence": i, "layer": m.layer, "path": m.path, "key": m.key, "frame": m.frame})))
        .collect();
    json!({
        "reference": rb.reference, "name": rb.label(), "tolerance": rb.tolerance,
        "evidence": rb.moves.len(), "judged": judged.len(),
        "matched": n(Verdict::Approved), "loose": n(Verdict::Close), "drifted": n(Verdict::Off),
        "unmatched": unused,
    })
}

/// The comp range of `p` (`start`/`end` comp seconds; default the whole comp).
fn span(s: &Session, p: &Value, cid: ItemId) -> Result<(f64, f64)> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let start = f_p(p, "start").unwrap_or(f64::MIN);
    let end = f_p(p, "end").unwrap_or(f64::MAX);
    let _ = comp;
    Ok((start, end))
}

/// The layers to check: `layers` / `layer` when given, else every layer.
fn only_p(s: &Session, p: &Value) -> Result<Option<Vec<LayerId>>> {
    if p.get("layers").is_some_and(|v| !v.is_null()) || p.get("layer").is_some_and(|v| !v.is_null()) {
        let mut q = p.clone();
        if let Some(l) = p.get("layer").filter(|v| !v.is_null()) {
            q["layers"] = json!([l]);
        }
        return Ok(Some(layers_p(s, &q)?.1));
    }
    Ok(None)
}

fn check(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let (prof, _) = profile(s);
    let (start, end) = span(s, p, cid)?;
    let only = only_p(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (mut moves, truncated) =
        taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), only.as_deref(), start, end, b_p(p, "all").unwrap_or(false));
    let (ok, close, off, blocking) = taste::summary(&moves);
    let rebuild = comp.rebuild.as_deref().map(|rb| rebuild_json(rb, &moves));
    if b_p(p, "problemsOnly").unwrap_or(false) {
        moves.retain(|m| m.verdict != Verdict::Approved);
    }
    let per_type: Vec<Value> = MoveType::ALL
        .iter()
        .filter(|m| **m != MoveType::Other)
        .map(|m| json!({"moveType": m.id(), "approvedCurves": prof.approved_for(*m).map(|c| c.name.clone()).collect::<Vec<_>>(), "blocks": prof.blocks(*m)}))
        .collect();
    let list: Vec<Value> = moves.iter().map(|m| move_json(s, cid, m)).collect();
    Ok(json!({
        "comp": cid.0,
        "compName": s.project.item(cid).map(|i| i.name.as_str()).unwrap_or(""),
        "summary": {"approved": ok, "close": close, "off": off, "blocking": blocking,
            "line": format!("✅ {ok} · 🟡 {close} · ❌ {off} ({blocking} blocking)")},
        "clean": blocking == 0,
        "truncated": truncated,
        "moves": list,
        "moveTypes": per_type,
        "blockAt": prof.rules.block_at,
        "rebuild": rebuild,
    }))
}

pub(crate) fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.apply";
    let (prof, _) = profile(s);
    let (cid, lid, uid) = prop_ref(s, p, C)?;
    let open = unlocked(s, cid, vec![lid], C)?;
    if open.is_empty() {
        return Err(bad(C, "the layer is locked"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let pr = l.props.find(uid).ok_or_else(|| bad(C, "no property"))?;
    let n = pr.keys.len();
    if n < 2 {
        return Err(bad(C, format!("{} has fewer than two keyframes", pr.name)));
    }
    let key = match p.get("key").filter(|v| !v.is_null()) {
        Some(v) => {
            v.as_u64().map(|k| k as usize).filter(|k| k + 1 < n).ok_or_else(|| bad(C, format!("`key` is the move's first key, 0-based, below {}", n - 1)))?
        }
        None if n == 2 => 0,
        None => return Err(bad(C, format!("{} has {n} keys: pass `key` (the move's first key, 0-based)", pr.name))),
    };
    let secs = (pr.keys.get(key + 1).map(|k| k.time.seconds()).unwrap_or(0.0) - pr.keys.get(key).map(|k| k.time.seconds()).unwrap_or(0.0)).abs()
        * l.stretch.abs()
        / 100.0;
    let want = str_p(p, "curve").unwrap_or("nearest");
    let curve: Curve = if taste::squash(want) == "reference" {
        // Rebuild mode: the reference editor's fitted ease for this move.
        let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), Some(&[lid]), f64::MIN, f64::MAX, true);
        let m = moves
            .iter()
            .find(|m| m.at.uid == uid && m.at.key == key)
            .ok_or_else(|| bad(C, "that key pair isn't a move (its value doesn't change, or it holds)"))?;
        let r =
            m.reference.as_ref().ok_or_else(|| bad(C, "no reference fit for this move: the comp isn't a rebuild, or taste.rebuild has no evidence for it"))?;
        Curve {
            id: "reference".into(),
            name: format!("reference {}: {}", r.name, r.curve),
            out: r.ease.0,
            inn: r.ease.1,
            roles: vec![m.kind],
            status: Status::Approved,
            family: None,
            source: format!("reference {}", r.name),
            note: String::new(),
            said: String::new(),
        }
    } else if taste::squash(want) == "nearest" {
        let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), Some(&[lid]), f64::MIN, f64::MAX, true);
        let m = moves
            .iter()
            .find(|m| m.at.uid == uid && m.at.key == key)
            .ok_or_else(|| bad(C, "that key pair isn't a move (its value doesn't change, or it holds)"))?;
        let id = m.nearest.as_ref().map(|n| n.0.clone()).ok_or_else(|| bad(C, "no approved curve for this move type: pass `curve`"))?;
        prof.find(&id).cloned().ok_or_else(|| bad(C, "nearest curve vanished"))?
    } else {
        prof.find(want).cloned().ok_or_else(|| {
            bad(
                C,
                format!(
                    "no curve {want:?}: use one of {} (taste.profile lists them)",
                    prof.curves.iter().map(|c| format!("\"{}\"", c.id)).collect::<Vec<_>>().join(", ")
                ),
            )
        })?
    };
    if curve.status == Status::Rejected {
        return Err(bad(C, format!("Elias rejected {}", curve.name)));
    }
    let (out, inn) = curve.at(secs);
    let g = Graph {
        id: "taste",
        name: "taste",
        out: effectcraft_keyframe::Ease { speed: out.speed, influence: (out.influence / 100.0).clamp(0.001, 1.0) },
        inn: effectcraft_keyframe::Ease { speed: inn.speed, influence: (inn.influence / 100.0).clamp(0.001, 1.0) },
    };
    let mut keys = pr.keys.clone();
    let spatial = pr.spatial;
    apply_graph(&mut keys, key, spatial, &g)?;
    let (name, path) = (pr.name.clone(), crate::commands::match_path_of(&l.props, uid).unwrap_or_default());
    let label = format!("Apply Ease {}", curve.name);
    s.edit(&label, None, |proj, _| {
        let l = proj.comp_mut(cid).ok_or(EngineError::NoComp)?.layer_mut(lid).ok_or_else(|| bad(C, "no layer"))?;
        let pr = l.props.find_mut(uid).ok_or_else(|| bad(C, "no property"))?;
        pr.keys = keys;
        Ok(())
    })?;
    let note = if curve.status == Status::Candidate { " (⏳ not approved by Elias yet)" } else { "" };
    let after = s.project.comp(cid).and_then(|c| {
        let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, c, Tick::ZERO), Some(&[lid]), f64::MIN, f64::MAX, true);
        moves.into_iter().find(|m| m.at.uid == uid && m.at.key == key)
    });
    Ok(json!({
        "comp": cid.0, "layer": lid.0, "path": path, "property": name, "key": key,
        "curve": curve.id, "curveName": curve.name, "status": curve.status.id(),
        "out": side_json(out), "in": side_json(inn),
        "message": format!("{} on {name} keys {key}–{}{note}", curve.name, key + 1),
        "after": after.map(|m| move_json(s, cid, &m)),
    }))
}

fn profile_cmd(s: &mut Session, _: &Value) -> Result<Value> {
    let (prof, stored) = profile(s);
    let types: Vec<Value> = MoveType::ALL
        .iter()
        .filter(|m| **m != MoveType::Other)
        .map(|m| json!({"moveType": m.id(), "words": m.words(), "approved": prof.approved_for(*m).count(), "blocks": prof.blocks(*m)}))
        .collect();
    Ok(json!({
        "rules": serde_json::to_value(&prof.rules).unwrap_or(Value::Null),
        "curves": prof.curves.iter().map(|c| curve_json(&prof, c)).collect::<Vec<_>>(),
        "moveTypes": types,
        "stored": !stored.curves.is_empty() || stored.rules.is_some(),
        "file": taste::PROFILE_FILE,
    }))
}

fn roles_p(p: &Value, cmd: &str) -> Result<Vec<MoveType>> {
    match p.get("roles").filter(|v| !v.is_null()) {
        None => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|r| {
                r.as_str()
                    .and_then(MoveType::parse)
                    .ok_or_else(|| bad(cmd, format!("unknown move type {r}: {}", MoveType::ALL.iter().map(|m| m.id()).collect::<Vec<_>>().join(", "))))
            })
            .collect(),
        Some(v) => Err(bad(cmd, format!("`roles` is a list of move types, not {v}"))),
    }
}

fn side_p(p: &Value, k: &str, cmd: &str) -> Result<Side> {
    let v = p.get(k).ok_or_else(|| bad(cmd, format!("missing `{k}: {{speed, influence}}`")))?;
    let speed = v.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
    let influence = v.get("influence").and_then(Value::as_f64).filter(|x| x.is_finite() && *x > 0.0 && *x <= 100.0);
    let influence = influence.ok_or_else(|| bad(cmd, format!("`{k}.influence` is a percent (0 to 100]")))?;
    if !speed.is_finite() {
        return Err(bad(cmd, format!("`{k}.speed` is a number")));
    }
    Ok(Side { speed, influence })
}

fn propose(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.propose";
    let (prof, mut stored) = profile(s);
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(C, "missing `name`"))?.to_string();
    let mut roles = roles_p(p, C)?;
    let (out, inn, source) = if p.get("out").is_some() {
        let source = match str_p(p, "reference").map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => format!("reference {r}"),
            None => str_p(p, "source").unwrap_or("proposed").to_string(),
        };
        (side_p(p, "out", C)?, side_p(p, "in", C)?, source)
    } else {
        let (cid, lid, uid) = prop_ref(s, p, C)?;
        let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
        let key = p.get("key").and_then(Value::as_u64).unwrap_or(0) as usize;
        let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), Some(&[lid]), f64::MIN, f64::MAX, true);
        let m = moves.iter().find(|m| m.at.uid == uid && m.at.key == key).ok_or_else(|| bad(C, "that key pair isn't an eased move"))?;
        let (o, i) = m.ease.ok_or_else(|| bad(C, "that move has no ease to read (linear?)"))?;
        if roles.is_empty() {
            roles.push(m.kind);
        }
        let comp_name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
        let at = format!("{comp_name} › {} › {} @ {:.2} s", m.layer_name, m.path, m.t0);
        let source = match str_p(p, "reference").map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => format!("reference {r} ({at})"),
            None => at,
        };
        (o, i, source)
    };
    if roles.is_empty() {
        return Err(bad(C, "pass `roles` (the move types it is for)"));
    }
    let id = taste::squash(&name);
    if id.is_empty() {
        return Err(bad(C, "`name` needs letters or digits"));
    }
    if prof.curves.iter().any(|c| c.id == id || c.name == name) {
        return Err(bad(C, format!("a curve {name:?} already exists")));
    }
    let near = prof.curves.iter().map(|c| (c, taste::distance((out, inn), c.at(1.0)))).min_by(|a, b| a.1.total_cmp(&b.1));
    let note = str_p(p, "note").unwrap_or("").to_string();
    let c = Curve { id: id.clone(), name: name.clone(), out, inn, roles, status: Status::Candidate, family: None, source, note, said: String::new() };
    stored.curves.push(c.clone());
    save(s, &stored, C)?;
    Ok(json!({
        "curve": curve_json(&prof, &c),
        "nearest": near.map(|(c, d)| json!({"id": c.id, "name": c.name, "gap": (d * 10.0).round() / 10.0, "status": c.status.id()})),
        "message": format!("⏳ {name} saved as a candidate: it joins the profile only after Elias approves it (taste.approve, from him)"),
    }))
}

fn approve(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.approve";
    if let Some(c) = &s.client {
        return Err(bad(C, format!("only Elias approves taste: `{c}` is an agent client. Ask him, show him the move (taste.graph), and let him run it")));
    }
    let said = str_p(p, "said").map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| bad(C, "missing `said`: Elias's words approving or rejecting it"))?;
    let status = match str_p(p, "status").unwrap_or("approved") {
        "approved" => Status::Approved,
        "rejected" => Status::Rejected,
        "candidate" => Status::Candidate,
        x => return Err(bad(C, format!("`status` is approved, rejected or candidate, not {x:?}"))),
    };
    let (prof, mut stored) = profile(s);
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id`"))?;
    let mut c = prof.find(id).cloned().ok_or_else(|| bad(C, format!("no curve {id:?}")))?;
    c.status = status;
    c.said = said.to_string();
    let roles = roles_p(p, C)?;
    if !roles.is_empty() {
        c.roles = roles;
    }
    match stored.curves.iter_mut().find(|x| x.id == c.id) {
        Some(x) => *x = c.clone(),
        None => stored.curves.push(c.clone()),
    }
    save(s, &stored, C)?;
    let prof = Profile::with(&stored);
    Ok(json!({
        "curve": curve_json(&prof, &c),
        "blocks": c.roles.iter().map(|r| json!({"moveType": r.id(), "approved": prof.approved_for(*r).count(), "blocks": prof.blocks(*r)})).collect::<Vec<_>>(),
    }))
}

fn learn(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.learn";
    let (prof, mut stored) = profile(s);
    let min_uses = p.get("minUses").and_then(Value::as_u64).unwrap_or(3).max(1) as usize;
    let ids: Vec<ItemId> = match p.get("comps").filter(|v| !v.is_null()) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_u64).map(ItemId).collect(),
        Some(v) => return Err(bad(C, format!("`comps` is a list of comp ids, not {v}"))),
        None => s
            .project
            .items
            .iter()
            .filter(|(_, i)| matches!(i.kind, effectcraft_project::ItemKind::Comp(_)) && !i.name.contains("(Claude)"))
            .map(|(id, _)| *id)
            .collect(),
    };
    let comps: Vec<(&str, EvalCtx)> = ids
        .iter()
        .take(MAX_LEARN_COMPS)
        .filter_map(|id| Some((s.project.item(*id)?.name.as_str(), EvalCtx::new(&s.project, *id, s.project.comp(*id)?, Tick::ZERO))))
        .collect();
    let found = taste::unmatched_curves(&prof, &comps, MAX_GROUPS);
    let keep: Vec<&taste::Found> = found.iter().filter(|f| f.uses >= min_uses).collect();
    let mut saved = vec![];
    if b_p(p, "save").unwrap_or(false) {
        for (n, f) in keep.iter().enumerate() {
            let name = format!("Found {} {:.0}/{:.0}", f.kinds.first().map(|k| k.id()).unwrap_or("move"), f.ease.0.influence, f.ease.1.influence);
            let id = format!("found-{}-{}", taste::squash(&name), n);
            if prof.curves.iter().chain(stored.curves.iter()).any(|c| c.id == id) {
                continue;
            }
            stored.curves.push(Curve {
                id: id.clone(),
                name,
                out: f.ease.0,
                inn: f.ease.1,
                roles: f.kinds.clone(),
                status: Status::Candidate,
                family: None,
                source: f.examples.first().cloned().unwrap_or_default(),
                note: format!("used {}× in his comps (taste.learn)", f.uses),
                said: String::new(),
            });
            saved.push(id);
        }
        if !saved.is_empty() {
            save(s, &stored, C)?;
        }
    }
    Ok(json!({
        "comps": comps.len(),
        "groups": keep.iter().map(|f| json!({
            "out": side_json(f.ease.0), "in": side_json(f.ease.1),
            "moveTypes": f.kinds.iter().map(|k| k.id()).collect::<Vec<_>>(),
            "uses": f.uses, "examples": f.examples,
            "nearest": f.nearest.as_ref().map(|(n, d)| json!({"name": n, "gap": d})),
        })).collect::<Vec<_>>(),
        "saved": saved,
        "note": "curves used in these comps that no profile curve matches; save:true files them as ⏳ candidates (Elias approves)",
    }))
}

/// Whether two evidence entries name the same move (a newer one replaces the older).
fn same_move(a: &taste_rebuild::RefMove, b: &taste_rebuild::RefMove) -> bool {
    a.layer.trim().eq_ignore_ascii_case(b.layer.trim())
        && taste::squash(a.path.trim_start_matches("layer/")) == taste::squash(b.path.trim_start_matches("layer/"))
        && a.key == b.key
        && a.frame == b.frame
}

fn rebuild(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.rebuild";
    let cid = comp_id(s, p)?;
    let old = s.project.comp(cid).ok_or(EngineError::NoComp)?.rebuild.clone();
    if b_p(p, "clear").unwrap_or(false) {
        if old.is_some() {
            s.edit("Clear Rebuild", None, |proj, _| {
                proj.comp_mut(cid).ok_or(EngineError::NoComp)?.rebuild = None;
                Ok(())
            })?;
        }
        return Ok(json!({"comp": cid.0, "rebuild": null, "message": "not a rebuild: every move is judged against Elias's profile"}));
    }
    // The evidence: `file` (the same JSON), then the params on top.
    let mut doc = match str_p(p, "file") {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| bad(C, format!("can't read {path}: {e}")))?;
            let v: Value = serde_json::from_str(&text).map_err(|e| bad(C, format!("{path} is not rebuild JSON: {e}")))?;
            if !v.is_object() {
                return Err(bad(C, format!("{path} holds a JSON object {{reference, name?, tolerance?, moves}}")));
            }
            v
        }
        None => json!({}),
    };
    for k in ["reference", "name", "tolerance", "moves"] {
        if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
            doc[k] = v.clone();
        }
    }
    let given = doc.as_object().is_some_and(|o| !o.is_empty());
    let (prof, _) = profile(s);
    let mut rb: taste_rebuild::Rebuild = match (&old, given) {
        (None, false) => return Err(bad(C, "the comp isn't a rebuild yet: pass `reference` and `moves` (or `file`)")),
        (Some(o), _) => (**o).clone(),
        (None, true) => taste_rebuild::Rebuild::default(),
    };
    if b_p(p, "replace").unwrap_or(false) {
        rb.moves.clear();
    }
    if let Some(r) = doc.get("reference").filter(|v| !v.is_null()) {
        rb.reference = r.as_str().map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| bad(C, "`reference` is the reference's path or URL"))?.to_string();
    }
    if let Some(n) = doc.get("name").filter(|v| !v.is_null()) {
        rb.name = n.as_str().ok_or_else(|| bad(C, "`name` is text"))?.trim().to_string();
    }
    if let Some(t) = doc.get("tolerance").filter(|v| !v.is_null()) {
        let t =
            t.as_f64().filter(|t| t.is_finite() && *t > 0.0 && *t <= 50.0).ok_or_else(|| bad(C, "`tolerance` is a percent of the move's progress (0, 50]"))?;
        rb.tolerance = Some(t);
    }
    if let Some(m) = doc.get("moves").filter(|v| !v.is_null()) {
        let incoming: Vec<taste_rebuild::RefMove> = serde_json::from_value(m.clone())
            .map_err(|e| bad(C, format!("`moves` is a list of {{layer, path, key?|frame?, graph|ease, fit?, samples?, note?}}: {e}")))?;
        for m in incoming {
            rb.moves.retain(|x| !same_move(x, &m));
            rb.moves.push(m);
        }
    }
    if rb.reference.trim().is_empty() {
        return Err(bad(C, "missing `reference` (the reference's path or URL)"));
    }
    if rb.moves.len() > taste_rebuild::MAX_MOVES {
        return Err(bad(C, format!("at most {} moves", taste_rebuild::MAX_MOVES)));
    }
    let wrong: Vec<String> = rb
        .moves
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if m.layer.trim().is_empty() || m.path.trim().is_empty() {
                return Some(format!("moves[{i}]: needs `layer` (its name) and `path` (transform/position)"));
            }
            taste::reference_ease(&prof, m, 1.0).err().map(|e| format!("moves[{i}] ({} {}): {e}", m.layer, m.path))
        })
        .take(8)
        .collect();
    if !wrong.is_empty() {
        return Err(bad(C, wrong.join("; ")));
    }
    if given && old.as_deref() != Some(&rb) {
        let stored = Arc::new(rb.clone());
        s.edit("Set Rebuild Evidence", None, |proj, _| {
            proj.comp_mut(cid).ok_or(EngineError::NoComp)?.rebuild = Some(stored);
            Ok(())
        })?;
    }
    // Judge the comp against it (every animated property: evidence may name any).
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), None, f64::MIN, f64::MAX, true);
    let comp_name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    // Its fits become ⏳ candidates tagged `reference <name>` (Elias approves them).
    let label = rb.label();
    let (mut saved, mut known, mut skipped) = (vec![], vec![], Value::Null);
    if b_p(p, "candidates").unwrap_or(true) {
        let (_, mut stored) = profile(s);
        let mut cur = Profile::with(&stored);
        let mut added = false;
        for m in moves.iter() {
            let Some(r) = &m.reference else { continue };
            let secs = (m.t1 - m.t0).abs().max(1e-9);
            let near = cur.curves.iter().map(|c| (c, taste::distance(r.ease, c.at(secs)))).min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((c, d)) = near.filter(|(_, d)| *d <= cur.rules.match_within) {
                known.push(json!({"layerName": m.layer_name, "path": m.path, "key": m.at.key, "id": c.id, "name": c.name, "status": c.status.id(), "gap": (d * 10.0).round() / 10.0}));
                continue;
            }
            let (o, i) = r.ease;
            let base = format!("ref-{}-{}-{:.0}-{:.0}", taste::squash(&label), m.kind.id(), o.influence, i.influence);
            let id =
                (0..100).map(|n| if n == 0 { base.clone() } else { format!("{base}-{n}") }).find(|id| !cur.curves.iter().any(|c| &c.id == id)).unwrap_or(base);
            let c = Curve {
                id: id.clone(),
                name: format!("Ref {label}: {} {}", m.kind.id(), r.curve),
                out: o,
                inn: i,
                roles: vec![m.kind],
                status: Status::Candidate,
                family: None,
                source: format!("reference {label}"),
                note: format!(
                    "fitted from {} for {comp_name} › {} › {} @ {:.2} s{}",
                    rb.reference,
                    m.layer_name,
                    m.path,
                    m.t0,
                    r.fit.as_ref().map(|f| format!(", fit RMS {:.1} {}", f.rms, f.unit)).unwrap_or_default()
                ),
                said: String::new(),
            };
            saved.push(curve_json(&cur, &c));
            stored.curves.push(c.clone());
            cur.curves.push(c);
            added = true;
        }
        if added && let Err(e) = save(s, &stored, C) {
            skipped = json!(e.to_string());
            saved.clear();
        }
    }
    let (ok, close, off, blocking) = taste::summary(&moves);
    let rebuild = rebuild_json(&rb, &moves);
    let list: Vec<Value> = moves.iter().filter(|m| m.reference.is_some()).map(|m| move_json(s, cid, m)).collect();
    let message = format!(
        "{comp_name} is a rebuild of {label}: {} of {} evidence moves judged against their fit (✅ {} · 🟡 {} · ❌ {}){}; {} ⏳ candidate(s) filed",
        rebuild["judged"],
        rb.moves.len(),
        rebuild["matched"],
        rebuild["loose"],
        rebuild["drifted"],
        if rebuild["unmatched"].as_array().is_some_and(|u| !u.is_empty()) { ", some evidence matches no keyed move (see unmatched)" } else { "" },
        saved.len()
    );
    Ok(json!({
        "comp": cid.0,
        "compName": comp_name,
        "rebuild": rebuild,
        "moves": list,
        "candidates": {"saved": saved, "known": known, "skipped": skipped},
        "summary": {"approved": ok, "close": close, "off": off, "blocking": blocking, "line": format!("✅ {ok} · 🟡 {close} · ❌ {off} ({blocking} blocking)")},
        "message": message,
    }))
}

fn graph(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "taste.graph";
    let out = str_p(p, "out").ok_or_else(|| bad(C, "missing `out` (a .png path)"))?;
    if !out.to_ascii_lowercase().ends_with(".png") {
        return Err(bad(C, "`out` must end in .png"));
    }
    let (cid, lid, uid) = prop_ref(s, p, C)?;
    let (prof, _) = profile(s);
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let pr = l.props.find(uid).ok_or_else(|| bad(C, "no property"))?;
    let key_t: Vec<f64> = pr.keys.iter().map(|k| l.comp_time(k.time).seconds()).collect();
    let first = key_t.iter().copied().fold(f64::MAX, f64::min);
    let last = key_t.iter().copied().fold(f64::MIN, f64::max);
    let fps = comp.frame_rate.as_f64().max(1.0);
    let (k0, k1) = if first <= last { (first, last) } else { (0.0, comp.duration.seconds()) };
    let pad = ((k1 - k0) * 0.08).max(4.0 / fps);
    let t0 = f_p(p, "start").unwrap_or(k0 - pad).max(0.0);
    let t1 = f_p(p, "end").unwrap_or(k1 + pad).min(comp.duration.seconds().max(t0 + 1.0 / fps));
    if t1 <= t0 {
        return Err(bad(C, "`end` must be after `start`"));
    }
    let w = p.get("width").and_then(Value::as_u64).unwrap_or(1280).min(u32::MAX as u64) as u32;
    let h = p.get("height").and_then(Value::as_u64).unwrap_or(720).min(u32::MAX as u64) as u32;
    let (moves, _) = taste::check(&prof, &EvalCtx::new(&s.project, cid, comp, Tick::ZERO), Some(&[lid]), t0, t1, true);
    let mine: Vec<&Judged> = moves.iter().filter(|m| m.at.uid == uid).collect();
    let path = crate::commands::match_path_of(&l.props, uid).unwrap_or_else(|| pr.name.clone());
    let comp_name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    let img = crate::taste_graph::draw(l, pr, &path, &comp_name, t0, t1, fps, &mine, w, h);
    if let Some(dir) = std::path::Path::new(out).parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| bad(C, format!("can't create {}: {e}", dir.display())))?;
    }
    img.save(out).map_err(|e| bad(C, format!("can't write {out}: {e}")))?;
    Ok(json!({
        "path": out, "width": img.width(), "height": img.height(),
        "comp": cid.0, "layer": lid.0, "property": pr.name, "propertyPath": path,
        "start": t0, "end": t1,
        "moves": mine.iter().map(|m| move_json(s, cid, m)).collect::<Vec<_>>(),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "taste.check",
            "Taste Check",
            "{comp?, layers?|layer? (default every layer), start? (comp s), end? (comp s), all? (also other animated properties: hard fails only; default position, scale, rotation, opacity), problemsOnly? (leave out ✅ moves)} → {comp, compName, summary {approved, close, off, blocking, line}, clean (no blocking ❌), truncated, moves: [{layer, layerName, path, property, key (the move's first key, 0-based), from/to {time, tc, frame}, frames, moveType (entrance|exit|move|fade-in|fade-out|scale-pop|scrub|camera (a camera or its null rig: on → on criteria, no speed cap, warn only until camera curves are approved)|continuous (keys past the layer's first or last frame on screen, covering all of it: an orbit, a drift; hard fails + speed cap only, warn only)|other (other properties, and layers with no pixels: hard fails only)), verdict (approved|close|off), mark (✅|🟡|❌), blocking, message, ease {out, in} (normalised speed, influence %), criteria {peakPosition %, sharpness (fastest frame ÷ average), tail10Frames (frames on the last 10 % of the travel), pxPerFrame?, framePerFrame? (% of the comp width per frame)}, curve? (the approved curve it matches), nearest? {id, name, gap %}, fix? {command: taste.apply, params}, problems?, reference? (rebuild mode: {name, curve, ease, gap %, tolerance, evidence, fit?, fitPct?, profile {verdict, mark, problems, message}: what Elias's profile alone says, info only}), referenceEase? ({why, status: approved|candidate|rejected|none, mark, id?, name?, source?, gap?}: for layers or comps whose comment carries the `reference ease` tag, and moves with rebuild evidence: the profile curve with this shape and its status)}], moveTypes: [{moveType, approvedCurves, blocks}], blockAt, rebuild? ({reference, name, tolerance, evidence, judged, matched, loose, drifted, unmatched: evidence no move in the range used})}. Judges every keyframed move against Elias's ease profile: ❌ = linear, Easy Ease 33/33, a speed jump at a key, baked one-key-per-frame, or outside his learned ranges (peak position per move type, peak sharpness for its length, speed cap in % of the frame per frame, entrance landing length; a shape he approved for the move type passes the peak and landing ranges); 🟡 = in range but not one of his approved curves; ✅ = an approved curve for that move type. A ❌ blocks handover only when its move type has blockAt approved curves. Rebuild mode (a comp marked with taste.rebuild): a move with reference evidence is ✅ when its ease matches the reference fit within the rebuild's tolerance, 🟡 when it matches a loose fit (RMS > 10 % of the travel), ❌ blocking when it drifted (fix: taste.apply curve reference); moves without evidence are judged as usual.",
            check
        ),
        cmd!(
            "taste.apply",
            "Apply Taste Curve",
            [],
            None,
            "{comp?, layer, path|prop, key? (the move's first key, 0-based; default when the property has 2 keys), curve? (a curve id or name from taste.profile, nearest (default): the nearest approved curve for its move type, or reference: the move's reference fit in a rebuild comp)} → {comp, layer, path, property, key, curve, curveName, status, out, in, message, after (the move judged again)}. Puts the curve on keys key → key + 1 (out of the first, in of the second), keeping values and times; Expo In-Out's influences follow the move's length. One undo step.",
            has_comp,
            apply
        ),
        cmd!(
            "taste.rebuild",
            "Mark as Rebuild of a Reference",
            [],
            None,
            "{comp?, reference (the reference's path or URL), name? (short: \"kunal TR\"; default its file name), tolerance? (largest progress gap %, default the profile's matchWithin 5), moves: [{layer (name, or #id), path (transform/position; layer/… and position also match), key? (the move's first key, 0-based) | frame? (comp frame of its first key, ±1) (neither: every move of the property), graph (a profile curve name or id, fit_graph.py's free:<out>:<in> with influences as fractions, or linear) | ease {out {speed, influence}, in {…}} (normalised speed, influence %), fit? {rms, worst?, unit: px (the property's units, as fit_graph.py prints) | % (of the travel)}, samples? (the measured samples' file), note?}], file? (a JSON file with the same fields; params win), replace? (drop the stored moves first; default merge: a move for the same layer, path and key/frame replaces the old one), candidates? (default true: file each fit no profile curve has as a ⏳ candidate tagged source `reference <name>`), clear? (true: no longer a rebuild)} → {comp, compName, rebuild {reference, name, tolerance, evidence, judged, matched, loose, drifted, unmatched}, moves (the moves with evidence, as taste.check), candidates {saved, known, skipped}, summary, message}. Marks the comp as a rebuild of a reference: taste.check, the off-taste lint and handover.check then judge each move with evidence against its reference fit instead of Elias's profile (his rules become info). Saved with the comp (survives save/load and copying the comp). With only `comp`, reports the stored state. One undo step.",
            has_comp,
            rebuild
        ),
        query!(
            "taste.profile",
            "Taste Profile",
            "{} → {rules {blockAt, entrancePeakMax, exitPeakMin, movePeak, fadeInPeakMax, sharpA, sharpB, speedCap (% of width per frame), entranceTailMin (% of frames on the last 10 %), matchWithin}, curves: [{id, name, status (approved|candidate|rejected), mark, out, in, roles, source, family?, note?, said?}], moveTypes: [{moveType, words, approved, blocks}], stored, file}: Elias's ease profile (built-in Graphs and approved curves, plus approvals and ⏳ candidates saved in the settings)",
            profile_cmd
        ),
        query!(
            "taste.propose",
            "Propose Taste Curve",
            "{name, roles? ([entrance|exit|move|fade-in|fade-out|scale-pop]), out? {speed, influence}, in? {speed, influence} (normalised speed, influence %), source?, note? | comp?, layer, path|prop, key? (read the curve from that move; roles default to its move type), reference? (the reference's name: tags it source `reference <name>`)} → {curve, nearest, message}. Files a ⏳ candidate: it is not used for ✅ until Elias approves it.",
            propose
        ),
        query!(
            "taste.approve",
            "Approve Taste Curve",
            "{id, status? (approved (default) | rejected | candidate), said (Elias's words), roles?} → {curve, blocks}. Elias only: refused for agent clients.",
            approve
        ),
        query!(
            "taste.learn",
            "Learn Taste Curves",
            "{comps? (comp ids; default every comp without \"(Claude)\" in its name), minUses? (default 3), save? (file them as ⏳ candidates)} → {comps, groups: [{out, in, moveTypes, uses, examples, nearest {name, gap}}], saved, note}: the curves used in those comps that no profile curve matches",
            learn
        ),
        query!(
            "taste.graph",
            "Taste Graph",
            "{comp?, layer, path|prop, out (.png path), start? (comp s; default the keys with a margin), end?, width? (px, default 1280), height? (default 720)} → {path, width, height, comp, layer, property, propertyPath, start, end, moves}. Draws the property's value graph with its speed graph under it, keys marked, each move's speed coloured by its taste verdict (green approved, amber close, red off) and labelled with its curve.",
            graph
        ),
    ]
}
