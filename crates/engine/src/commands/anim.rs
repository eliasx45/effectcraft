//! Keyframe clipboard, selection, nudging, direct key/ease edits (Graph Editor handles) and key
//! inspection for the Keyframe Velocity / Keyframe Interpolation dialogs.

use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe, key_at, retime_roving, side_ease};
use effectcraft_project::{LayerId, Property, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::prop::{KeyTimeBase, key_time, key_timing, prop_ref};
use super::{CommandSpec, bad, comp_id, f_p, has_comp, has_keys, layers_p, merge_p, str_p, time_p};
use crate::{EngineError, GraphClip, KeyClip, KeyGraph, KeyRef, Result, Session, cmd, query};

fn has_key_clip(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if s.state.key_clipboard.is_empty() { Err("no keyframes have been copied".into()) } else { Ok(()) }
}

/// Copy the selected keyframes (Edit ▸ Copy with keys selected).
fn copy(s: &mut Session, _: &Value) -> Result<Value> {
    let comp = s.active_comp().ok_or(EngineError::NoComp)?;
    let mut groups: std::collections::BTreeMap<(LayerId, Uid), Vec<Tick>> = Default::default();
    for k in &s.state.selected_keys {
        groups.entry((k.layer, k.prop)).or_default().push(k.time);
    }
    // Earliest selected key in comp time: pasted keys keep their offsets from it.
    let mut earliest: Option<Tick> = None;
    for ((lid, _), times) in &groups {
        if let Some(l) = comp.layer(*lid) {
            for t in times {
                let ct = l.comp_time(*t);
                earliest = Some(earliest.map_or(ct, |e: Tick| e.min(ct)));
            }
        }
    }
    let earliest = earliest.ok_or_else(|| bad("keys.copy", "select keyframes first"))?;
    let mut clip = vec![];
    for ((lid, uid), times) in groups {
        let Some(l) = comp.layer(lid) else { continue };
        let Some(pr) = l.props.find(uid) else { continue };
        let Some(path) = l.props.match_path_of(uid) else { continue };
        let mut keys: Vec<Keyframe> = pr.keys.iter().filter(|k| times.contains(&k.time)).cloned().collect();
        for k in &mut keys {
            k.time = l.comp_time(k.time) - earliest;
        }
        clip.push(KeyClip { layer: lid, path, keys });
    }
    let n: usize = clip.iter().map(|c| c.keys.len()).sum();
    s.state.graph_clipboard = graph_clip(comp, &s.state.selected_keys);
    s.state.key_clipboard = clip;
    s.state.clip_is_keys = true;
    Ok(json!(n))
}

impl KeyGraph {
    fn of(k: &Keyframe) -> KeyGraph {
        KeyGraph {
            in_interp: k.in_interp,
            out_interp: k.out_interp,
            in_ease: k.in_ease.clone(),
            out_ease: k.out_ease.clone(),
            continuous: k.continuous,
            auto_bezier: k.auto_bezier,
        }
    }
}

/// `eases` for a value of `dims` dimensions: the same list, or the missing ones repeating the last.
fn fit_eases(eases: &Eases, dims: usize) -> Eases {
    (0..dims.max(1)).filter_map(|d| eases.get(d).or_else(|| eases.last()).copied()).collect()
}

/// What copying `selected` keeps as the graph to paste: the one graph they all share, else the
/// graphs of the last selected property's selected keys, in time order (one when those share it).
fn graph_clip(comp: &effectcraft_project::Comp, selected: &[KeyRef]) -> GraphClip {
    let key = |r: &KeyRef| comp.layer(r.layer).and_then(|l| l.props.find(r.prop)).and_then(|pr| pr.keys.iter().find(|k| k.time == r.time));
    let all: Vec<KeyGraph> = selected.iter().filter_map(|r| key(r).map(KeyGraph::of)).collect();
    let same = |g: &[KeyGraph]| g.windows(2).all(|w| w[0] == w[1]);
    let keys = if same(&all) {
        all.into_iter().take(1).collect()
    } else {
        let mut last: Vec<(Tick, KeyGraph)> = match selected.last() {
            Some(l) => selected.iter().filter(|r| r.layer == l.layer && r.prop == l.prop).filter_map(|r| key(r).map(|k| (r.time, KeyGraph::of(k)))).collect(),
            None => vec![],
        };
        last.sort_by_key(|(t, _)| *t);
        let graphs: Vec<KeyGraph> = last.into_iter().map(|(_, g)| g).collect();
        if same(&graphs) { graphs.into_iter().take(1).collect() } else { graphs }
    };
    GraphClip { keys, source: selected.to_vec() }
}

/// Whether Edit ▸ Paste puts the copied graph on the selected keyframes: keyframes were copied
/// and a different set of keyframes is selected now (with the copied ones still selected, Paste
/// pastes the keys themselves at the current time).
pub(crate) fn graph_paste_applies(s: &Session) -> bool {
    let (sel, src) = (&s.state.selected_keys, &s.state.graph_clipboard.source);
    s.state.clip_is_keys
        && s.state.value_clipboard.is_empty()
        && !s.state.graph_clipboard.keys.is_empty()
        && !sel.is_empty()
        && (sel.iter().any(|k| !src.contains(k)) || src.iter().any(|k| !sel.contains(k)))
}

fn has_graph_paste(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if s.state.graph_clipboard.keys.is_empty() { Err("no keyframes have been copied".into()) } else { Ok(()) }
}

/// Put the copied graph on the selected keyframes of every property: one graph for all of them
/// when the copied keys shared it; else the copied segments repeat over each property's selected
/// keys in time order (the copied keys' outgoing and incoming sides, segment by segment), so any
/// number of keys takes a graph copied from two.
fn paste_graph(s: &mut Session, p: &Value) -> Result<Value> {
    let clip = s.state.graph_clipboard.keys.clone();
    if clip.is_empty() {
        return Err(bad("keys.pasteGraph", "no keyframes have been copied"));
    }
    let cid = comp_id(s, p)?;
    let mut groups: std::collections::BTreeMap<(LayerId, Uid), Vec<Tick>> = Default::default();
    for k in &s.state.selected_keys {
        groups.entry((k.layer, k.prop)).or_default().push(k.time);
    }
    if groups.is_empty() {
        return Err(bad("keys.pasteGraph", "select keyframes first"));
    }
    s.edit("Paste Keyframe Graph", merge_p(p), |proj, _| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut n = 0;
        for ((lid, uid), times) in groups {
            let Some(pr) = comp.layer_mut(lid).and_then(|l| l.props.find_mut(uid)) else { continue };
            if pr.hold_only || pr.static_only {
                continue;
            }
            let idx: Vec<usize> = pr.keys.iter().enumerate().filter(|(_, k)| times.contains(&k.time)).map(|(i, _)| i).collect();
            let count = idx.len();
            for (j, i) in idx.into_iter().enumerate() {
                let Some(k) = pr.keys.get_mut(i) else { continue };
                let dims = k.value.dims();
                let one = |at: usize| clip.get(at.min(clip.len().saturating_sub(1)));
                let segs = clip.len().saturating_sub(1).max(1);
                if clip.len() == 1 || count == 1 {
                    let Some(g) = one(0) else { continue };
                    (k.in_interp, k.out_interp, k.continuous, k.auto_bezier) = (g.in_interp, g.out_interp, g.continuous, g.auto_bezier);
                    k.in_ease = fit_eases(&g.in_ease, dims);
                    k.out_ease = fit_eases(&g.out_ease, dims);
                } else {
                    // Key j leaves along segment j and arrives along segment j - 1 (the pattern's).
                    let out_at = if j + 1 < count { j % segs } else { (j.saturating_sub(1)) % segs + 1 };
                    let in_at = if j == 0 { 0 } else { (j - 1) % segs + 1 };
                    let (Some(go), Some(gi)) = (one(out_at), one(in_at)) else { continue };
                    k.out_interp = go.out_interp;
                    k.out_ease = fit_eases(&go.out_ease, dims);
                    k.in_interp = gi.in_interp;
                    k.in_ease = fit_eases(&gi.in_ease, dims);
                    k.auto_bezier = false;
                    k.continuous = k.continuous && k.in_ease.iter().zip(k.out_ease.iter()).all(|(a, b)| (a.speed - b.speed).abs() < 1e-9);
                }
                n += 1;
            }
        }
        Ok(json!(n))
    })
}

/// Values of the same kind (and dimensionality) can receive each other's keys.
fn compatible(a: &Property, b: &effectcraft_keyframe::Value) -> bool {
    a.value.kind_name() == b.kind_name() && a.value.components().len() == b.components().len()
}

/// Paste copied keyframes at the current time into the target layers' properties.
fn paste(s: &mut Session, p: &Value) -> Result<Value> {
    let clip = s.state.key_clipboard.clone();
    if clip.is_empty() {
        return Err(bad("keys.paste", "no keyframes have been copied"));
    }
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let t = time_p(s, p, Some(comp));
    let (_, mut targets) = layers_p(s, p)?;
    if targets.is_empty() {
        targets = clip.iter().map(|c| c.layer).collect();
        targets.dedup();
    }
    let explicit = p.get("prop").and_then(Value::as_u64).or_else(|| {
        let path = str_p(p, "path")?;
        comp.layer(*targets.first()?)?.props.prop(path).map(|pr| pr.uid)
    });
    // Keys copied from several layers, pasted onto as many layers: the first copied layer's keys
    // go to the first target and so on (else every target gets every copied property).
    let mut sources: Vec<LayerId> = clip.iter().map(|c| c.layer).collect();
    sources.dedup();
    let paired = sources.len() > 1 && sources.len() == targets.len();
    // (layer, prop uid, clip index)
    let mut plan: Vec<(LayerId, Uid, usize)> = vec![];
    for (ti, lid) in targets.iter().enumerate() {
        let Some(l) = comp.layer(*lid) else { continue };
        for (ci, c) in clip.iter().enumerate() {
            if paired && sources.get(ti) != Some(&c.layer) {
                continue;
            }
            let Some(sample) = c.keys.first().map(|k| &k.value) else { continue };
            let target = if clip.len() == 1 {
                explicit
                    .and_then(|u| l.props.find(u))
                    .or_else(|| {
                        s.state.selected_props.iter().filter(|(sl, _)| sl == lid).filter_map(|(_, u)| l.props.find(*u)).find(|pr| compatible(pr, sample))
                    })
                    .or_else(|| l.props.prop(&c.path))
            } else {
                l.props.prop(&c.path)
            };
            if let Some(pr) = target.filter(|pr| compatible(pr, sample) && !pr.static_only) {
                plan.push((*lid, pr.uid, ci));
            }
        }
    }
    if plan.is_empty() {
        return Err(bad("keys.paste", "no compatible property to paste into (select a property of the same type)"));
    }
    s.edit("Paste Keyframes", merge_p(p), |proj, st| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut sel = vec![];
        for (lid, uid, ci) in &plan {
            let Some(l) = comp.layer_mut(*lid) else { continue };
            let times: Vec<Tick> = clip[*ci].keys.iter().map(|k| l.layer_time(t + k.time)).collect();
            let Some(pr) = l.props.find_mut(*uid) else { continue };
            for (k, lt) in clip[*ci].keys.iter().zip(times) {
                let mut k = k.clone();
                k.time = lt;
                if let Some(v) = pr.value.coerce_json(&k.value.to_json()) {
                    k.value = v;
                }
                if pr.hold_only {
                    k = k.hold();
                }
                match pr.keys.binary_search_by(|x| x.time.cmp(&k.time)) {
                    Ok(i) => pr.keys[i] = k,
                    Err(i) => pr.keys.insert(i, k),
                }
                sel.push(KeyRef { layer: *lid, prop: *uid, time: lt });
            }
            retime_roving(&mut pr.keys, pr.spatial);
        }
        st.selected_keys = sel;
        Ok(json!(st.selected_keys.len()))
    })
}

/// Select every keyframe of the selected properties, or of the given / selected layers.
fn select_all(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, mut layers) = layers_p(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut sel = vec![];
    if let Some(visible) = super::visible_p(p) {
        // Ctrl+Alt+A in the Timeline: every key it shows (its revealed properties).
        for (lid, uid) in visible {
            if let Some(pr) = comp.layer(lid).and_then(|l| l.props.find(uid)) {
                sel.extend(pr.keys.iter().map(|k| KeyRef { layer: lid, prop: uid, time: k.time }));
            }
        }
    } else if p.get("layers").is_none() && p.get("layer").is_none() && !s.state.selected_props.is_empty() {
        for (lid, uid) in &s.state.selected_props {
            if let Some(pr) = comp.layer(*lid).and_then(|l| l.props.find(*uid)) {
                sel.extend(pr.keys.iter().map(|k| KeyRef { layer: *lid, prop: *uid, time: k.time }));
            }
        }
    } else {
        if layers.is_empty() {
            layers = comp.layers.iter().map(|l| l.id).collect();
        }
        for l in comp.layers.iter().filter(|l| layers.contains(&l.id)) {
            l.props.walk("", &mut |_, pr| sel.extend(pr.keys.iter().map(|k| KeyRef { layer: l.id, prop: pr.uid, time: k.time })));
        }
    }
    // Keys of locked layers can't be selected.
    sel.retain(|k| comp.layer(k.layer).is_some_and(|l| !l.switches.locked));
    s.state.selected_keys = sel;
    Ok(json!(s.state.selected_keys.len()))
}

/// The keyframe context menu's Select Equal / Previous / Following Keyframes: in every property
/// with a selected key, its keys with the same value as a selected one / before the first
/// selected one / after the last selected one (the selected keys stay selected).
fn select_related(s: &mut Session, which: &str) -> Result<Value> {
    let comp = s.active_comp().ok_or(EngineError::NoComp)?;
    let mut sel = s.state.selected_keys.clone();
    let mut props: Vec<(LayerId, Uid)> = sel.iter().map(|k| (k.layer, k.prop)).collect();
    props.sort();
    props.dedup();
    for (lid, uid) in props {
        let Some(pr) = comp.layer(lid).and_then(|l| l.props.find(uid)) else { continue };
        let picked: Vec<&Keyframe> = pr.keys.iter().filter(|k| sel.iter().any(|r| r.layer == lid && r.prop == uid && r.time == k.time)).collect();
        let (Some(first), Some(last)) = (picked.iter().map(|k| k.time).min(), picked.iter().map(|k| k.time).max()) else { continue };
        for k in &pr.keys {
            let hit = match which {
                "equal" => picked.iter().any(|p| p.value == k.value),
                "previous" => k.time < first,
                _ => k.time > last,
            };
            let r = KeyRef { layer: lid, prop: uid, time: k.time };
            if hit && !sel.contains(&r) {
                sel.push(r);
            }
        }
    }
    s.state.selected_keys = sel;
    Ok(json!(s.state.selected_keys.len()))
}

/// Move the selected keys by whole frames (Alt+→ / Alt+←, Shift for 10 frames).
fn nudge(s: &mut Session, p: &Value) -> Result<Value> {
    let frames = p.get("frames").and_then(Value::as_i64).ok_or_else(|| bad("keys.nudge", "missing `frames`"))?;
    let comp = s.active_comp().ok_or(EngineError::NoComp)?;
    let d = Tick(comp.frame_duration().0.saturating_mul(frames));
    super::prop::shift_keys(s, "Nudge Keyframes", merge_p(p), d)
}

/// Edit one key directly: move it in time and/or set its value (Graph Editor drags).
fn set_key(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = prop_ref(s, p, "keys.set")?;
    let base = KeyTimeBase::from_params(p, "keys.set")?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let layer = comp.layer(lid).ok_or(EngineError::NoComp)?;
    let t = key_time(layer, comp.frame_rate, p, "time", base, "keys.set", false)?.ok_or_else(|| bad("keys.set", "missing `time` (layer time by default)"))?;
    let new_t = key_time(layer, comp.frame_rate, p, "newTime", base, "keys.set", true)?;
    let value = p.get("value").cloned();
    let nt = s.edit("Edit Keyframe", merge_p(p), |proj, st| {
        let l = super::layer_mut(proj, cid, lid)?;
        let pr = l.props.find_mut(uid).ok_or_else(|| bad("keys.set", "no property"))?;
        let i = nearest_key(&pr.keys, t).ok_or_else(|| bad("keys.set", "the property has no keyframes"))?;
        let old_t = pr.keys[i].time;
        if let Some(v) = &value {
            let nv = pr.keys[i].value.coerce_json(v).ok_or_else(|| bad("keys.set", format!("can't use {v} here")))?;
            pr.keys[i].value = nv;
        }
        let mut nt = old_t;
        if let Some(n) = new_t {
            nt = n;
            // Keys can't pass their neighbours or share a time.
            let lo = if i > 0 { pr.keys[i - 1].time.0.saturating_add(1) } else { i64::MIN };
            let hi = if i + 1 < pr.keys.len() { pr.keys[i + 1].time.0.saturating_sub(1) } else { i64::MAX };
            nt = Tick(nt.0.clamp(lo, hi));
            pr.keys[i].time = nt;
        }
        retime_roving(&mut pr.keys, pr.spatial);
        let nt = pr.keys.get(i).map(|k| k.time).unwrap_or(nt);
        for k in st.selected_keys.iter_mut().filter(|k| k.layer == lid && k.prop == uid && k.time == old_t) {
            k.time = nt;
        }
        Ok(nt)
    })?;
    let layer = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or(EngineError::NoComp)?;
    Ok(key_timing(layer, nt))
}

fn nearest_key(keys: &[Keyframe], t: Tick) -> Option<usize> {
    if let Some(i) = key_at(keys, t) {
        return Some(i);
    }
    keys.iter().enumerate().min_by_key(|(_, k)| (k.time.0 - t.0).abs()).map(|(i, _)| i)
}

/// Set one side's temporal ease of a key (Graph Editor handle drags, scripting).
fn set_ease(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = prop_ref(s, p, "keys.setEase")?;
    let t = Tick::from_seconds_f64(f_p(p, "time").ok_or_else(|| bad("keys.setEase", "missing `time`"))?);
    let out = match str_p(p, "side") {
        Some("in") => false,
        Some("out") => true,
        _ => return Err(bad("keys.setEase", "`side` must be in or out")),
    };
    let dim = p.get("dim").and_then(Value::as_u64).map(|d| d as usize);
    let speed = f_p(p, "speed");
    let influence = f_p(p, "influence").map(|v| (v / 100.0).clamp(0.001, 1.0));
    s.edit("Keyframe Velocity", merge_p(p), |proj, _| {
        let l = super::layer_mut(proj, cid, lid)?;
        let pr = l.props.find_mut(uid).ok_or_else(|| bad("keys.setEase", "no property"))?;
        let i = nearest_key(&pr.keys, t).ok_or_else(|| bad("keys.setEase", "no keyframes"))?;
        let spatial = pr.spatial;
        let n = if spatial && matches!(pr.keys[i].value, effectcraft_keyframe::Value::Vec2(_) | effectcraft_keyframe::Value::Vec3(_)) {
            1
        } else {
            pr.keys[i].value.dims().max(1)
        };
        // Start from what the side presents now so untouched dimensions keep their shape.
        let cur: Vec<Ease> = (0..n).map(|d| side_ease(&pr.keys, i, d, spatial, out).unwrap_or_default()).collect();
        let k = &mut pr.keys[i];
        let list = if out { &mut k.out_ease } else { &mut k.in_ease };
        if list.len() != n {
            *list = cur.clone().into();
        }
        for (d, e) in list.iter_mut().enumerate() {
            if dim.is_some_and(|x| x != d) && n > 1 {
                continue;
            }
            if let Some(v) = speed {
                e.speed = v;
            }
            if let Some(v) = influence {
                e.influence = v;
            }
        }
        if out {
            k.out_interp = Interp::Bezier;
        } else {
            k.in_interp = Interp::Bezier;
        }
        k.auto_bezier = false;
        if let Some(continuous) = p.get("continuous").and_then(Value::as_bool) {
            k.continuous = continuous;
        }
        // Continuous Bezier keys keep matching speeds on both sides.
        if k.continuous {
            let src = if out { k.out_ease.clone() } else { k.in_ease.clone() };
            let other = if out { &mut k.in_ease } else { &mut k.out_ease };
            if other.len() != src.len() {
                *other = src.clone();
            }
            for (o, sv) in other.iter_mut().zip(&src) {
                o.speed = sv.speed;
            }
            k.in_interp = Interp::Bezier;
            k.out_interp = Interp::Bezier;
        }
        Ok(Value::Null)
    })
}

/// Speed units for a property (Keyframe Velocity dialog).
fn units(pr: &Property) -> &'static str {
    use effectcraft_project::ParamUi;
    if pr.spatial {
        return "pixels/sec";
    }
    match pr.ui {
        ParamUi::Angle => "degrees/sec",
        ParamUi::Percent => "%/sec",
        ParamUi::Slider { .. } if pr.match_id == "opacity" => "%/sec",
        _ if pr.match_id == "opacity" => "%/sec",
        ParamUi::Pixels => "pixels/sec",
        _ => "units/sec",
    }
}

fn interp_name(i: Interp) -> &'static str {
    match i {
        Interp::Linear => "linear",
        Interp::Bezier => "bezier",
        Interp::Hold => "hold",
    }
}

/// Describe the selected (or given) keys: interpolation, eases per side and dimension, units.
fn info(s: &mut Session, p: &Value) -> Result<Value> {
    let comp = s.active_comp().ok_or(EngineError::NoComp)?;
    let refs: Vec<KeyRef> = match p.get("keys") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|k| {
                let l = super::resolve_layer(comp, k.get("layer")?)?;
                Some(KeyRef { layer: l, prop: k.get("prop")?.as_u64()?, time: Tick::from_seconds_f64(k.get("time")?.as_f64()?) })
            })
            .collect(),
        _ => s.state.selected_keys.clone(),
    };
    let mut out = vec![];
    for r in refs {
        let Some(l) = comp.layer(r.layer) else { continue };
        let Some(pr) = l.props.find(r.prop) else { continue };
        let Some(i) = nearest_key(&pr.keys, r.time) else { continue };
        let k = &pr.keys[i];
        let spatial = pr.spatial && matches!(k.value, effectcraft_keyframe::Value::Vec2(_) | effectcraft_keyframe::Value::Vec3(_));
        let dims = if spatial { 1 } else { k.value.dims().max(1).min(if pr.shown_dims > 0 && !l.is_3d() { pr.shown_dims as usize } else { 4 }) };
        let side = |out: bool| -> Value {
            json!(
                (0..dims)
                    .map(|d| side_ease(&pr.keys, i, d, pr.spatial, out)
                        .map(|e| json!({"speed": e.speed, "influence": e.influence * 100.0}))
                        .unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            )
        };
        let temporal = if k.auto_bezier {
            "autoBezier"
        } else if k.continuous {
            "continuousBezier"
        } else if k.in_interp == k.out_interp {
            interp_name(k.in_interp)
        } else {
            "mixed"
        };
        let spatial_kind = if !spatial {
            Value::Null
        } else if k.spatial_auto {
            json!("autoBezier")
        } else if k.spatial_continuous {
            json!("continuousBezier")
        } else if k.spatial_in == [0.0; 3] && k.spatial_out == [0.0; 3] {
            json!("linear")
        } else {
            json!("bezier")
        };
        out.push(json!({
            "layer": l.id.0, "prop": pr.uid, "name": pr.name, "path": super::query::tree_path_of(&l.props, pr.uid),
            "matchPath": l.props.match_path_of(pr.uid), "time": k.time.seconds(), "compTime": l.comp_time(k.time).seconds(), "value": k.value.to_json(),
            "in": interp_name(k.in_interp), "out": interp_name(k.out_interp), "temporal": temporal,
            "autoBezier": k.auto_bezier, "continuous": k.continuous, "roving": k.roving, "spatial": spatial_kind,
            "dims": dims, "units": units(pr), "inEase": side(false), "outEase": side(true),
        }));
    }
    Ok(json!(out))
}

/// Keyframe Assistant ▸ Time-Reverse Keyframes: mirror the selected keys of each property.
fn time_reverse(s: &mut Session, _: &Value) -> Result<Value> {
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let sel = s.state.selected_keys.clone();
    s.edit("Time-Reverse Keyframes", None, |proj, st| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut groups: std::collections::BTreeMap<(LayerId, Uid), Vec<Tick>> = Default::default();
        for k in &sel {
            groups.entry((k.layer, k.prop)).or_default().push(k.time);
        }
        let mut new_sel = vec![];
        for ((lid, uid), times) in groups {
            let Some(pr) = comp.layer_mut(lid).and_then(|l| l.props.find_mut(uid)) else { continue };
            let (mut picked, rest): (Vec<Keyframe>, Vec<Keyframe>) = pr.keys.drain(..).partition(|k| times.contains(&k.time));
            effectcraft_keyframe::time_reverse(&mut picked);
            new_sel.extend(picked.iter().map(|k| KeyRef { layer: lid, prop: uid, time: k.time }));
            pr.keys = rest;
            for k in picked {
                match pr.keys.binary_search_by(|x| x.time.cmp(&k.time)) {
                    Ok(i) => pr.keys[i] = k,
                    Err(i) => pr.keys.insert(i, k),
                }
            }
        }
        st.selected_keys = new_sel;
        Ok(json!(st.selected_keys.len()))
    })
}

fn keys_or_layers(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!("keys.copy", "Copy Keyframes", [], None, "{}", has_keys, copy),
        cmd!("keys.paste", "Paste Keyframes", [], None, "{layers?, prop?|path?, time?}", has_key_clip, paste),
        cmd!(
            "keys.pasteGraph",
            "Paste Keyframe Graph",
            [],
            None,
            "{merge?} (the copied keys' graph onto the selected keys; Edit ▸ Paste does it when other keys are selected)",
            has_graph_paste,
            paste_graph
        ),
        cmd!(
            "keys.selectAll",
            "Select All Keyframes",
            [],
            Some("Cmd+Alt+A"),
            "{layers?, visible?: [{layer, prop}] (every key of these properties)}",
            keys_or_layers,
            select_all
        ),
        cmd!("keys.nudge", "Nudge Keyframes", [], None, "{frames, merge?}", has_keys, nudge),
        cmd!("keys.nudgeForward", "Move Keyframes 1 Frame Later", [], Some("Alt+ArrowRight"), "{}", has_keys, |s, _| nudge(s, &json!({"frames": 1}))),
        cmd!("keys.nudgeBackward", "Move Keyframes 1 Frame Earlier", [], Some("Alt+ArrowLeft"), "{}", has_keys, |s, _| nudge(s, &json!({"frames": -1}))),
        cmd!("keys.nudgeForward10", "Move Keyframes 10 Frames Later", [], Some("Alt+Shift+ArrowRight"), "{}", has_keys, |s, _| nudge(
            s,
            &json!({"frames": 10})
        )),
        cmd!("keys.nudgeBackward10", "Move Keyframes 10 Frames Earlier", [], Some("Alt+Shift+ArrowLeft"), "{}", has_keys, |s, _| nudge(
            s,
            &json!({"frames": -10})
        )),
        cmd!(
            "keys.set",
            "Edit Keyframe",
            [],
            None,
            "{layer?, path|prop, time (layer s by default), newTime? (same base), timeBase?: layer|comp, value?, merge?}",
            has_comp,
            set_key
        ),
        cmd!("keys.selectEqual", "Select Equal Keyframes", [], None, "{}", has_keys, |s, _| select_related(s, "equal")),
        cmd!("keys.selectPrevious", "Select Previous Keyframes", [], None, "{}", has_keys, |s, _| select_related(s, "previous")),
        cmd!("keys.selectFollowing", "Select Following Keyframes", [], None, "{}", has_keys, |s, _| select_related(s, "following")),
        cmd!(
            "keys.setEase",
            "Set Keyframe Ease",
            [],
            None,
            "{layer?, path|prop, time (layer s), side: in|out, dim?, speed?, influence? %, continuous?: bool, merge?}",
            has_comp,
            set_ease
        ),
        cmd!("keys.timeReverse", "Time-Reverse Keyframes", ["Animation", "Keyframe Assistant"], None, "{}", has_keys, time_reverse),
        query!("keys.info", "Keyframe Info", "{keys?: [{layer, prop, time}]}", info),
    ]
}

#[cfg(test)]
mod graph_paste_tests {
    use effectcraft_keyframe::Interp;
    use serde_json::json;

    use crate::Session;

    fn setup(keys: &[(f64, f64)]) -> (Session, u64, u64) {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "C", "width": 64, "height": 64, "frameRate": 30, "duration": 8})).unwrap();
        s.execute("layer.newSolid", json!({"name": "A", "width": 8, "height": 8})).unwrap();
        let l = s.active_comp().unwrap().layers[0].id.0;
        for (t, v) in keys {
            s.execute("prop.addKey", json!({"layer": l, "path": "transform/opacity", "time": t, "value": v})).unwrap();
        }
        let uid = s.active_comp().unwrap().layers[0].props.prop("transform/opacity").unwrap().uid;
        (s, l, uid)
    }

    fn select(s: &mut Session, l: u64, uid: u64, times: &[f64]) {
        let keys: Vec<_> = times.iter().map(|t| json!({"layer": l, "prop": uid, "time": t})).collect();
        s.execute("keys.select", json!({"keys": keys})).unwrap();
    }

    fn keys(s: &Session, l: u64) -> Vec<effectcraft_keyframe::Keyframe> {
        s.active_comp().unwrap().layers.iter().find(|x| x.id.0 == l).unwrap().props.prop("transform/opacity").unwrap().keys.to_vec()
    }

    /// Copy two keys with a custom graph, select four others, Paste: every selected key gets the
    /// graph, segment by segment; keys not selected stay; copying then pasting with the same keys
    /// selected still pastes the keys themselves.
    #[test]
    fn paste_puts_the_copied_graph_on_any_number_of_selected_keys() {
        let (mut s, l, uid) = setup(&[(0.0, 0.0), (1.0, 100.0), (2.0, 0.0), (3.0, 100.0), (4.0, 0.0), (5.0, 50.0)]);
        select(&mut s, l, uid, &[0.0, 1.0]);
        s.execute("keys.setEase", json!({"layer": l, "path": "transform/opacity", "time": 0.0, "side": "out", "influence": 80.0, "speed": 0.0})).unwrap();
        s.execute("keys.setEase", json!({"layer": l, "path": "transform/opacity", "time": 1.0, "side": "in", "influence": 20.0, "speed": 0.0})).unwrap();
        s.execute("edit.copy", json!({})).unwrap();
        // The copied keys still selected: Paste pastes keys (at the CTI), not the graph.
        assert!(!super::graph_paste_applies(&s));
        select(&mut s, l, uid, &[2.0, 3.0, 4.0, 5.0]);
        assert!(super::graph_paste_applies(&s));
        let undo = s.history.undo.len();
        s.execute("edit.paste", json!({})).unwrap();
        assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
        let k = keys(&s, l);
        assert_eq!(k.len(), 6, "no key added");
        for i in 2..5 {
            assert_eq!(k[i].out_interp, Interp::Bezier, "key {i} out");
            assert!((k[i].out_ease[0].influence - 0.8).abs() < 1e-9, "key {i} out: {:?}", k[i].out_ease);
        }
        for i in 3..6 {
            assert!((k[i].in_ease[0].influence - 0.2).abs() < 1e-9, "key {i} in: {:?}", k[i].in_ease);
        }
        assert_eq!(k[1].in_ease[0].influence, 0.2, "an unselected key keeps its own graph (here the copied one)");
    }

    /// Keys that all share one graph copy that graph: it goes on every selected key, both sides.
    #[test]
    fn one_shared_graph_goes_on_every_selected_key() {
        let (mut s, l, uid) = setup(&[(0.0, 0.0), (1.0, 100.0), (2.0, 0.0), (3.0, 100.0)]);
        select(&mut s, l, uid, &[0.0, 1.0]);
        s.execute("keys.easyEase", json!({})).unwrap();
        s.execute("edit.copy", json!({})).unwrap();
        select(&mut s, l, uid, &[2.0, 3.0]);
        s.execute("edit.paste", json!({})).unwrap();
        let k = keys(&s, l);
        for i in 2..4 {
            assert_eq!((k[i].in_interp, k[i].out_interp), (Interp::Bezier, Interp::Bezier), "key {i}");
            assert_eq!(k[i].in_ease, k[0].in_ease);
            assert_eq!(k[i].out_ease, k[0].out_ease);
        }
    }
}
