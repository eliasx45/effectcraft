//! Now graphs ([`crate::now_graphs`]) as commands: `nowGraphs.list` lists the keyframed segments
//! under the playhead through every precomp (with each object's box for the viewer outline),
//! `nowGraphs.setInfluence` drags a segment's two influence handles (one undo step per drag with
//! `merge`), `nowGraphs.applyEase` puts one of Elias's saved Graphs on it (or Linear), one
//! undo step, and `nowGraphs.moveKey` drags one of its keys in time (retimes the move: lengthen or
//! shorten it), snapped to the searched comp's frames, one undo step per drag with `merge`. FigCraft's paused EffectCraft card shows them as its "Now graphs" shelf; agents run
//! the same commands over the control channel.

use effectcraft_keyframe::{Ease, Interp};
use effectcraft_project::{ItemId, LayerId, LayerSource};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::peak::{Graph, apply_graph};
use super::prop::prop_ref;
use super::{CommandSpec, bad, comp_id, f_p, has_comp, merge_p, str_p, unlocked};
use crate::now_graphs::{self, WINDOW};
use crate::precomp_time::{self, Hop};
use crate::taste::{self, Profile, Side, Status};
use crate::{EngineError, Result, Session, cmd, query};

/// Smallest and largest influence (%), as After Effects allows.
const MIN_INFLUENCE: f64 = 0.1;
const MAX_INFLUENCE: f64 = 100.0;

fn r3(v: f64) -> f64 {
    if v.is_finite() { (v * 1000.0).round() / 1000.0 } else { 0.0 }
}

/// The curves the ease chip lists: Elias's approved curves (his Graphs cards and saved Motion
/// Library curves, `taste.profile`), then Linear.
pub fn ease_menu(profile: &Profile) -> Vec<Value> {
    let mut out: Vec<Value> = profile
        .curves
        .iter()
        .filter(|c| c.status == Status::Approved)
        .map(|c| {
            json!({
                "id": c.id, "name": c.name,
                "out": {"speed": r3(c.out.speed), "influence": r3(c.out.influence)},
                "in": {"speed": r3(c.inn.speed), "influence": r3(c.inn.influence)},
                "family": c.family,
            })
        })
        .collect();
    out.push(json!({"id": "linear", "name": "Linear"}));
    out
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (w, h) = (f64::from(comp.width), f64::from(comp.height));
    let t = f_p(p, "time").unwrap_or_else(|| s.time_of(cid).seconds());
    let window = f_p(p, "window").unwrap_or(WINDOW);
    let (profile, _) = Profile::load(s.config.as_deref());
    let found = now_graphs::under_playhead(&s.project, cid, t, window, Some(&profile));
    let objects: Vec<Value> = found
        .objects
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let quad = now_graphs::object_quad(&s.project, cid, t, o);
            json!({
                "object": i, "comp": o.comp.0, "layer": o.layer.0, "name": o.name,
                "via": o.via.iter().map(|h| json!({"comp": h.comp.0, "layer": h.layer.0})).collect::<Vec<_>>(),
                "quad": quad.map(|q| q.iter().map(|p| json!([r3(p[0]), r3(p[1])])).collect::<Vec<_>>()),
                "offFrame": quad.is_some_and(|q| now_graphs::off_frame(&q, w, h)),
            })
        })
        .collect();
    Ok(json!({
        "comp": cid.0,
        "compName": s.project.item(cid).map(|i| i.name.as_str()).unwrap_or(""),
        "time": r3(t),
        "window": r3(window),
        "width": comp.width, "height": comp.height,
        "frameRate": r3(comp.frame_rate.as_f64()),
        "graphs": found.graphs.iter().map(|g| g.to_json()).collect::<Vec<_>>(),
        "objects": objects,
        "curves": ease_menu(&profile),
        "truncated": found.truncated,
        "skipped": found.skipped,
    }))
}

/// `key`: the segment's first key, below the property's last.
fn key_p(p: &Value, n: usize, cmd: &str) -> Result<usize> {
    p.get("key")
        .and_then(Value::as_u64)
        .and_then(|k| usize::try_from(k).ok())
        .filter(|k| k.checked_add(1).is_some_and(|j| j < n))
        .ok_or_else(|| bad(cmd, format!("`key` is the segment's first key, 0-based, below {}", n.saturating_sub(1))))
}

fn influence_p(p: &Value, k: &str, cmd: &str) -> Result<Option<f64>> {
    match p.get(k).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(|x| Some(x.clamp(MIN_INFLUENCE, MAX_INFLUENCE)))
            .ok_or_else(|| bad(cmd, format!("`{k}` is an influence in percent ({MIN_INFLUENCE} to {MAX_INFLUENCE})"))),
    }
}

fn set_influence(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nowGraphs.setInfluence";
    let (cid, lid, uid) = prop_ref(s, p, C)?;
    if unlocked(s, cid, vec![lid], C)?.is_empty() {
        return Err(bad(C, "the layer is locked"));
    }
    let (out_inf, in_inf) = (influence_p(p, "out", C)?, influence_p(p, "in", C)?);
    if out_inf.is_none() && in_inf.is_none() {
        return Err(bad(C, "pass `out` and/or `in` (influence %)"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let pr = l.props.find(uid).ok_or_else(|| bad(C, "no property"))?;
    let key = key_p(p, pr.keys.len(), C)?;
    // The segment's eases as they are (normalised), the influences replaced: the speeds stay.
    let (out, inn) = match taste::normalised(pr, key) {
        Some((o, n, _)) => (o, n),
        None => return Err(bad(C, format!("{} doesn't change over keys {key}–{}: no speed graph to ease", pr.name, key + 1))),
    };
    let lin = |i: Interp| i == Interp::Linear;
    let (a_out, b_in) = (pr.keys.get(key).map(|k| k.out_interp), pr.keys.get(key + 1).map(|k| k.in_interp));
    // A linear side has no influence of its own: it starts from 0 speed, as AE's handles do.
    let base = |s: Side, linear: bool| if linear { Side { speed: 0.0, influence: s.influence } } else { s };
    let out = base(out, a_out.is_some_and(lin));
    let inn = base(inn, b_in.is_some_and(lin));
    let new_out = out_inf.unwrap_or(out.influence).clamp(MIN_INFLUENCE, MAX_INFLUENCE);
    let new_in = in_inf.unwrap_or(inn.influence).clamp(MIN_INFLUENCE, MAX_INFLUENCE);
    let g = Graph {
        id: "nowgraphs",
        name: "Now graphs",
        out: Ease { speed: out.speed, influence: new_out / 100.0 },
        inn: Ease { speed: inn.speed, influence: new_in / 100.0 },
    };
    let mut keys = pr.keys.clone();
    apply_graph(&mut keys, key, pr.spatial, &g)?;
    let (name, path) = (pr.name.clone(), crate::commands::match_path_of(&l.props, uid).unwrap_or_default());
    s.edit("Keyframe Influence", merge_p(p), |proj, _| {
        let pr = super::layer_mut(proj, cid, lid)?.props.find_mut(uid).ok_or_else(|| bad(C, "no property"))?;
        pr.keys = keys;
        Ok(())
    })?;
    Ok(json!({
        "comp": cid.0, "layer": lid.0, "path": path, "property": name, "key": key,
        "out": r3(new_out), "in": r3(new_in),
    }))
}

fn apply_ease(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nowGraphs.applyEase";
    let want = str_p(p, "curve")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| bad(C, "missing `curve` (an id or name from nowGraphs.list's curves, or linear)"))?;
    let (cid, lid, uid) = prop_ref(s, p, C)?;
    if unlocked(s, cid, vec![lid], C)?.is_empty() {
        return Err(bad(C, "the layer is locked"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let pr = l.props.find(uid).ok_or_else(|| bad(C, "no property"))?;
    let key = key_p(p, pr.keys.len(), C)?;
    if taste::squash(want) == "linear" {
        let (name, path) = (pr.name.clone(), crate::commands::match_path_of(&l.props, uid).unwrap_or_default());
        s.edit("Apply Ease Linear", None, |proj, _| {
            let pr = super::layer_mut(proj, cid, lid)?.props.find_mut(uid).ok_or_else(|| bad(C, "no property"))?;
            if let Some(a) = pr.keys.get_mut(key) {
                (a.out_interp, a.auto_bezier, a.continuous) = (Interp::Linear, false, false);
            }
            if let Some(b) = pr.keys.get_mut(key + 1) {
                (b.in_interp, b.auto_bezier, b.continuous) = (Interp::Linear, false, false);
            }
            Ok(())
        })?;
        return Ok(json!({
            "comp": cid.0, "layer": lid.0, "path": path, "property": name, "key": key,
            "curve": "linear", "curveName": "Linear", "message": format!("Linear on {name} keys {key}–{}", key + 1),
        }));
    }
    // One of his curves: taste.apply (its Expo In-Out follows the segment's length).
    super::taste::apply(s, &json!({"comp": cid.0, "layer": lid.0, "prop": uid, "key": key, "curve": want}))
}

/// `via` ([{comp, layer}], outermost first, as `nowGraphs.list` gives it) as hops from `top` down
/// to `inner`. Empty: `inner` is `top`.
fn hops_p(s: &Session, p: &Value, top: ItemId, inner: ItemId, cmd: &str) -> Result<Vec<Hop>> {
    let via = match p.get("via").filter(|v| !v.is_null()) {
        None => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(bad(cmd, "`via` is the precomp layers from `top` down, [{comp, layer}] as nowGraphs.list lists them")),
    };
    if via.len() > precomp_time::MAX_DEPTH {
        return Err(bad(cmd, "`via` is deeper than precomps are followed"));
    }
    let mut hops = Vec::with_capacity(via.len());
    let mut at = top;
    for h in &via {
        let (Some(c), Some(l)) = (h.get("comp").and_then(Value::as_u64), h.get("layer").and_then(Value::as_u64)) else {
            return Err(bad(cmd, "each `via` step is {comp, layer}"));
        };
        if c != at.0 {
            return Err(bad(cmd, format!("`via` doesn't lead down from comp {} (a step in comp {c})", at.0)));
        }
        let layer = s.project.comp(ItemId(c)).and_then(|cc| cc.layer(LayerId(l))).ok_or_else(|| bad(cmd, format!("no layer {l} in comp {c}")))?;
        let LayerSource::Comp { item } = layer.source else { return Err(bad(cmd, format!("{} isn't a precomp layer", layer.name))) };
        hops.push(precomp_time::hop(ItemId(c), layer).map_err(|e| bad(cmd, e))?);
        at = item;
    }
    if at != inner {
        return Err(bad(cmd, format!("`via` ends in comp {}, not the layer's comp {}", at.0, inner.0)));
    }
    Ok(hops)
}

/// Drag key `key` of a segment in time (Elias, 29 Oct: "extend keyframes, not just the graph").
fn move_key(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nowGraphs.moveKey";
    let (cid, lid, uid) = prop_ref(s, p, C)?;
    if unlocked(s, cid, vec![lid], C)?.is_empty() {
        return Err(bad(C, "the layer is locked"));
    }
    let top = match p.get("top").filter(|v| !v.is_null()) {
        None => cid,
        Some(v) => v.as_u64().map(ItemId).filter(|t| s.project.comp(*t).is_some()).ok_or_else(|| bad(C, format!("`top`: no comp {v}")))?,
    };
    let hops = hops_p(s, p, top, cid, C)?;
    let fr = s.project.comp(top).ok_or(EngineError::NoComp)?.frame_rate;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let pr = l.props.find(uid).ok_or_else(|| bad(C, "no property"))?;
    let n = pr.keys.len();
    let key = p
        .get("key")
        .and_then(Value::as_u64)
        .and_then(|k| usize::try_from(k).ok())
        .filter(|k| *k < n)
        .ok_or_else(|| bad(C, format!("`key` is the key to move, 0-based, below {n}")))?;
    // Key times in `top`'s seconds, and back to the layer's.
    let up = |t: Tick| precomp_time::up(&hops, l.comp_time(t).seconds());
    let down = |t: f64| l.layer_time(Tick::from_seconds_f64(precomp_time::down(&hops, t)));
    let want = match (f_p(p, "time"), f_p(p, "layerTime")) {
        (Some(t), _) => t,
        (None, Some(lt)) => up(Tick::from_seconds_f64(lt)),
        (None, None) => return Err(bad(C, "pass `time` (seconds of `top`, the comp nowGraphs.list searched) or `layerTime`")),
    };
    if !want.is_finite() || want.abs() > 1e7 {
        return Err(bad(C, "`time` is a time in seconds"));
    }
    let k = pr.keys.get(key).ok_or_else(|| bad(C, "no such key"))?;
    let now = up(k.time);
    // Snapped to `top`'s frames, and kept a frame clear of the neighbouring keys (it never passes
    // or lands on one), whichever way the precomps' stretch runs.
    let frame = fr.frame_duration().seconds();
    let mut t = fr.snap_nearest(Tick::from_seconds_f64(want)).seconds();
    let (mut lo, mut hi) = (f64::NEG_INFINITY, f64::INFINITY);
    for j in [key.checked_sub(1), key.checked_add(1)].into_iter().flatten() {
        let Some(nb) = pr.keys.get(j) else { continue };
        let nt = up(nb.time);
        if nt < now { lo = lo.max(nt + frame) } else { hi = hi.min(nt - frame) }
    }
    if lo > hi + 1e-9 {
        return Err(bad(C, "the neighbouring keys are a frame apart: no room to move this key"));
    }
    if t < lo - 1e-9 || t > hi + 1e-9 {
        let c = t.clamp(lo, hi);
        // Back on a frame if that stays inside, else right at the limit.
        let snapped = fr.snap_nearest(Tick::from_seconds_f64(c)).seconds();
        t = if snapped >= lo - 1e-9 && snapped <= hi + 1e-9 { snapped } else { c };
    }
    let to = down(t);
    // The eases either side keep their shape (normalised speed and influence): a longer move
    // keeps his Graph, it doesn't get steeper or flatter.
    let keep = |i: usize| -> Option<(usize, Graph)> {
        let (a, b) = (pr.keys.get(i)?, pr.keys.get(i + 1)?);
        if a.out_interp != Interp::Bezier || b.in_interp != Interp::Bezier || a.auto_bezier || b.auto_bezier {
            return None;
        }
        let (o, n, _) = taste::normalised(pr, i)?;
        Some((
            i,
            Graph {
                id: "nowgraphs",
                name: "Now graphs",
                out: Ease { speed: o.speed, influence: o.influence / 100.0 },
                inn: Ease { speed: n.speed, influence: n.influence / 100.0 },
            },
        ))
    };
    let shapes: Vec<(usize, Graph)> = [key.checked_sub(1), Some(key)].into_iter().flatten().filter_map(keep).collect();
    let mut keys = pr.keys.clone();
    if let Some(k) = keys.get_mut(key) {
        k.time = to;
    }
    for (i, g) in &shapes {
        apply_graph(&mut keys, *i, pr.spatial, g)?;
    }
    let (name, path) = (pr.name.clone(), crate::commands::match_path_of(&l.props, uid).unwrap_or_default());
    let from_layer = k.time.seconds();
    s.edit("Move Keyframe", merge_p(p), |proj, _| {
        let pr = super::layer_mut(proj, cid, lid)?.props.find_mut(uid).ok_or_else(|| bad(C, "no property"))?;
        pr.keys = keys;
        Ok(())
    })?;
    Ok(json!({
        "comp": cid.0, "layer": lid.0, "path": path, "property": name, "key": key,
        "time": r3(t), "layerTime": r3(to.seconds()), "from": r3(now), "fromLayerTime": r3(from_layer),
        "frames": r3((t - now) / frame.max(1e-9)),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "nowGraphs.list",
            "Now Graphs",
            "{comp? (default the active comp), time? (comp s; default the comp's current time), window? (s either side, default 1)} → {comp, compName, time, window, width, height, graphs: [{comp (the comp the layer is in), compName, via: [{comp, layer}] (precomp layers, outermost first), trail (names: comp › precomp layer… › layer › property), layer, layerName, topLayer (the layer in `comp` it shows through), prop, path, property, key (the segment's first key), start, end (comp s), layerTimes, interp {out, in}, ease? {out, in} (normalised speed, influence %), speed (49 samples, 0..1 of maxSpeed), maxSpeed, object, curve? (the saved curve it matches)}], objects: [{object, comp, layer, name, via, quad? (box corners in the comp's pixels), offFrame}], curves: [{id, name, out?, in?, family?}] (his approved Graphs, then Linear), truncated, skipped}. Every keyframed segment touching the playhead ± window in the comp and in every precomp it shows, one per property (the nearest).",
            list
        ),
        cmd!(
            "nowGraphs.setInfluence",
            "Set Segment Influence",
            [],
            None,
            "{comp?, layer, path|prop, key (the segment's first key, 0-based), out? (influence % of the first key's out side), in? (of the second key's in side), merge? (same key: one undo step, a drag)} → {comp, layer, path, property, key, out, in}. Both sides become Bezier; the speeds stay.",
            has_comp,
            set_influence
        ),
        cmd!(
            "nowGraphs.applyEase",
            "Apply Saved Ease",
            [],
            None,
            "{comp?, layer, path|prop, key (the segment's first key, 0-based), curve (an id or name from nowGraphs.list's curves, or linear)} → {comp, layer, path, property, key, curve, curveName, message, …}. Puts one of Elias's saved Graphs on keys key → key + 1 (taste.apply), or makes the segment linear. One undo step.",
            has_comp,
            apply_ease
        ),
        cmd!(
            "nowGraphs.moveKey",
            "Move Segment Key",
            [],
            None,
            "{comp?, layer, path|prop, key (the key to move, 0-based: a segment's first key or key + 1), time (seconds of `top`) | layerTime, top? (the comp nowGraphs.list searched; default `comp`), via? ([{comp, layer}] from top down to comp, as listed), merge? (one undo step per drag)} → {comp, layer, path, property, key, time, layerTime, from, fromLayerTime, frames}. Moves the key in time (lengthens or shortens the move), snapped to top's frames and kept at least a frame from its neighbours; Bezier eases either side keep their shape.",
            has_comp,
            move_key
        ),
    ]
}
