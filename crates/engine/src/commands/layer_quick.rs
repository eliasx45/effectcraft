//! Quick layer moves (batch 1, AX4/AX5): the Layer wheel's and the quick row's own commands.
//!
//! - `layer.fadeExit {frames?}`: opacity from its value to 0 over the layer's last frames (8).
//! - `layer.fastIn {property?, frames?, distance?}`: Fast In (Devin ▸ Graphs) on a property. A
//!   property with keys gets the graph on its first move; one without gets a move from the
//!   layer's in point (position slides up `distance` px, opacity rises from 0, scale grows from
//!   60 %, rotation turns from −8°) with the graph on it.
//! - `layer.cascade {frames?}`: each selected layer starts `frames` (1) after the one above it.
//!
//! Bounce, Caption glow and Save to Library are Motion Library tools the app runs
//! (`motionLibrary.tool`, `motionLibrary.apply`, `motionLibrary.saveSelection`).

use effectcraft_keyframe::{Keyframe, Value as KValue};
use effectcraft_project::{ItemId, LayerId, Property};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::graph_cmds::property_path;
use super::peak::{apply_graph, graph_named};
use super::{CommandSpec, bad, f_p, has_comp, layers_p, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd};

/// Longest fade or move these commands make (frames).
const MAX_FRAMES: f64 = 10_000.0;

fn frames_p(p: &Value, default: f64, c: &str) -> Result<i64> {
    let f = f_p(p, "frames").unwrap_or(default);
    if !f.is_finite() || !(1.0..=MAX_FRAMES).contains(&f) {
        return Err(bad(c, format!("`frames` is 1 to {MAX_FRAMES}")));
    }
    Ok(f.round() as i64)
}

/// The selected (or given) unlocked layers.
fn targets(s: &Session, p: &Value, c: &str) -> Result<(ItemId, Vec<LayerId>)> {
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(c, "select a layer first (or pass `layers`)"));
    }
    let open = unlocked(s, cid, ids, c)?;
    Ok((cid, open))
}

/// Sets a key at `t` (layer time), replacing one already there; keys stay in time order.
fn set_key(pr: &mut Property, t: Tick, v: KValue) {
    match pr.keys.iter().position(|k| k.time >= t) {
        Some(i) if pr.keys.get(i).is_some_and(|k| k.time == t) => {
            if let Some(k) = pr.keys.get_mut(i) {
                k.value = v;
            }
        }
        Some(i) => pr.keys.insert(i, Keyframe::new(t, v)),
        None => pr.keys.push(Keyframe::new(t, v)),
    }
}

fn map(v: &KValue, f: impl Fn(usize, f64) -> f64) -> Option<KValue> {
    match v {
        KValue::Scalar(x) => Some(KValue::Scalar(f(0, *x))),
        KValue::Vec2(a) => Some(KValue::Vec2([f(0, a[0]), f(1, a[1])])),
        KValue::Vec3(a) => Some(KValue::Vec3([f(0, a[0]), f(1, a[1]), f(2, a[2])])),
        _ => None,
    }
}

fn fade_exit(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.fadeExit";
    let frames = frames_p(p, 8.0, C)?;
    let (cid, ids) = targets(s, p, C)?;
    let fd = s.project.comp(cid).ok_or(EngineError::NoComp)?.frame_rate.frame_duration();
    let done = s.edit("Fade Exit", None, |proj, _| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut done = vec![];
        for lid in &ids {
            let Some(l) = comp.layer_mut(*lid) else { continue };
            // The last frame shown, and `frames` before it (comp time → layer time).
            let end_c = l.out_point - fd;
            let start_c = end_c - Tick(fd.0.saturating_mul(frames));
            let (end, start) = (l.layer_time(end_c), l.layer_time(start_c.max(l.in_point)));
            let Some(pr) = l.props.prop_mut("transform/opacity") else { continue };
            let from = pr.value_at(start);
            set_key(pr, start, from);
            set_key(pr, end, KValue::Scalar(0.0));
            done.push(json!({"layer": lid.0, "from": start_c.seconds(), "to": end_c.seconds()}));
        }
        Ok(done)
    })?;
    if done.is_empty() {
        return Err(bad(C, "none of the layers has an opacity"));
    }
    Ok(json!({"frames": frames, "layers": done}))
}

fn fast_in(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.fastIn";
    let frames = frames_p(p, 12.0, C)?;
    let prop = str_p(p, "property").unwrap_or("position");
    let path = property_path(prop);
    let distance = f_p(p, "distance").unwrap_or(40.0);
    if !distance.is_finite() || distance.abs() > 1e5 {
        return Err(bad(C, "`distance` is pixels (finite)"));
    }
    let graph = graph_named("fast-in").ok_or_else(|| bad(C, "the Fast In graph is missing"))?;
    let (cid, ids) = targets(s, p, C)?;
    let fd = s.project.comp(cid).ok_or(EngineError::NoComp)?.frame_rate.frame_duration();
    let short = path.rsplit('/').next().unwrap_or("").to_string();
    let done = s.edit(&format!("Fast In ({short})"), None, |proj, _| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut done = vec![];
        for lid in &ids {
            let Some(l) = comp.layer_mut(*lid) else { continue };
            let t0c = l.in_point.max(Tick(0));
            let (t0, t1) = (l.layer_time(t0c), l.layer_time(t0c + Tick(fd.0.saturating_mul(frames))));
            let Some(pr) = l.props.prop_mut(&path) else { continue };
            if pr.static_only {
                continue;
            }
            let spatial = pr.spatial;
            let made = if pr.keys.len() >= 2 {
                // Keys already there: the graph on their first move.
                false
            } else {
                let end = pr.value_at(t1);
                let start = match short.as_str() {
                    "position" => map(&end, |d, v| if d == 1 { v + distance } else { v }),
                    "opacity" => Some(KValue::Scalar(0.0)),
                    "scale" => map(&end, |_, v| v * 0.6),
                    "rotation" => map(&end, |_, v| v - 8.0),
                    _ => None,
                };
                let Some(start) = start else { continue };
                let end = if short == "opacity" && end.components().first().is_some_and(|v| *v <= 0.0) { KValue::Scalar(100.0) } else { end };
                pr.keys.clear();
                set_key(pr, t0, start);
                set_key(pr, t1, end);
                true
            };
            apply_graph(&mut pr.keys, 0, spatial, &graph)?;
            done.push(json!({"layer": lid.0, "property": pr.name, "path": path, "madeKeys": made}));
        }
        Ok(done)
    })?;
    if done.is_empty() {
        return Err(bad(C, format!("none of the layers has an animatable `{path}`")));
    }
    Ok(json!({"graph": "fast-in", "frames": frames, "layers": done}))
}

fn cascade(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.cascade";
    let frames = frames_p(p, 1.0, C)?;
    let (cid, ids) = targets(s, p, C)?;
    if ids.len() < 2 {
        return Err(bad(C, "cascade needs two or more layers"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fd = comp.frame_rate.frame_duration();
    // Stack order, top first.
    let order: Vec<LayerId> = comp.layers.iter().map(|l| l.id).filter(|id| ids.contains(id)).collect();
    let done = s.edit("Cascade Layers", None, |proj, _| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut done = vec![];
        for (j, lid) in order.iter().enumerate() {
            let Some(l) = comp.layer_mut(*lid) else { continue };
            let d = Tick(fd.0.saturating_mul(frames.saturating_mul(j as i64)));
            l.start_time += d;
            l.in_point += d;
            l.out_point += d;
            done.push(json!({"layer": lid.0, "offsetFrames": frames.saturating_mul(j as i64)}));
        }
        Ok(done)
    })?;
    Ok(json!({"frames": frames, "layers": done}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "layer.fadeExit",
            "Fade Exit",
            [],
            None,
            "{layers?, frames? (default 8)} → {frames, layers: [{layer, from, to}]}: opacity keys from its value to 0 over the layer's last frames, one undo step",
            has_comp,
            fade_exit
        ),
        cmd!(
            "layer.fastIn",
            "Fast In",
            [],
            None,
            "{layers?, property?: position (default) | opacity | scale | rotation | a path, frames? (default 12), distance? (px a new position move slides up, default 40)} → {layers: [{layer, property, path, madeKeys}]}: Devin's Fast In graph on the property's first move; without keys, a move from the layer's in point first",
            has_comp,
            fast_in
        ),
        cmd!(
            "layer.cascade",
            "Cascade Layers",
            [],
            None,
            "{layers? (two or more), frames? (default 1)} → {layers: [{layer, offsetFrames}]}: each layer starts `frames` after the one above it (keys move with it)",
            has_comp,
            cascade
        ),
    ]
}
