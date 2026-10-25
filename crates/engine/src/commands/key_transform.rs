//! Keyframe geometry edits from the viewer and the Graph Editor: the Graph Editor transform box
//! (scale / move selected keys in time and value; Alt-drag of a key group in the timeline) and
//! spatial Bezier tangents dragged on a motion path.

use effectcraft_keyframe::{Ease, Interp, Keyframe, side_ease};
use effectcraft_project::{LayerId, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::prop::prop_ref;
use super::{CommandSpec, b_p, bad, f_p, has_comp, has_keys, merge_p};
use crate::{EngineError, KeyRef, Result, Session, cmd};

fn v3(v: Option<&Value>) -> Option<[f64; 3]> {
    let a = v?.as_array()?;
    Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2).and_then(Value::as_f64).unwrap_or(0.0)])
}

/// Transform the selected keys: times scale about `timeAnchor` (comp seconds, default the first
/// selected key) by `timeScale` and move by `timeOffset`; values scale about `valueAnchor` by
/// `valueScale` and move by `valueOffset` (every dimension, or only `dim`). Times land on frames.
/// With `fromStart` (drags), each step of one merged gesture gives the whole transform since the
/// drag started and applies to the keys as they were then: keys squeezed onto one frame on the
/// way, or rounded steps, are never lost; only the final position counts.
fn transform(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let merge = merge_p(p);
    let from_start = b_p(p, "fromStart").unwrap_or(false) && merge.is_some();
    let gesture = from_start && merge.is_some_and(|m| s.history.merge_key.as_deref() == Some(m)) && s.key_move.as_ref().map(|g| g.merge.as_str()) == merge;
    let (sel, base) = match (&s.key_move, s.history.undo.last()) {
        (Some(g), Some((_, b))) if gesture => (g.from.clone(), Some(b.clone())),
        _ => (s.state.selected_keys.clone(), None),
    };
    if from_start {
        s.key_move = merge.map(|m| crate::KeyMove { merge: m.to_string(), from: sel.clone(), total: Tick::ZERO });
    }
    // The comp as it was when the drag started (its keys are what the transform applies to).
    let start = base.clone().unwrap_or_else(|| s.project.clone());
    let comp = start.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let ts = f_p(p, "timeScale").unwrap_or(1.0);
    let toff = f_p(p, "timeOffset").unwrap_or(0.0);
    let vs = f_p(p, "valueScale").unwrap_or(1.0);
    let voff = f_p(p, "valueOffset").unwrap_or(0.0);
    let va = f_p(p, "valueAnchor").unwrap_or(0.0);
    // `dim` (one dimension) or `dims` (several); none = every dimension.
    let dims: Option<Vec<usize>> = match (p.get("dim").and_then(Value::as_u64), p.get("dims").and_then(Value::as_array)) {
        (Some(d), _) => Some(vec![d as usize]),
        (None, Some(a)) => Some(a.iter().filter_map(Value::as_u64).map(|d| d as usize).collect()),
        _ => None,
    };
    if !ts.is_finite() || ts <= 0.0 {
        return Err(bad("keys.transform", "`timeScale` must be positive"));
    }
    // Default time anchor: the earliest selected key (comp time).
    let first = sel.iter().filter_map(|k| comp.layer(k.layer).map(|l| l.comp_time(k.time))).min().unwrap_or(Tick::ZERO);
    let ta = f_p(p, "timeAnchor").map(Tick::from_seconds_f64).unwrap_or(first);
    // Keys don't move before the comp start, but one already there (a layer starting earlier)
    // stays where it is: the anchor of an Alt-drag scale must not jump to 0.
    let map_t = |ct: Tick| -> Tick {
        if ct == ta && toff == 0.0 {
            return ct;
        }
        let x = ta.seconds() + (ct.seconds() - ta.seconds()) * ts + toff;
        fr.snap_nearest(Tick::from_seconds_f64(x.max(ct.seconds().min(0.0))))
    };
    let touch_values = vs != 1.0 || voff != 0.0;
    // `speedOffset` (speed graph): the speed entering and leaving each key changes by this much
    // (units/s, faster away from 0), its influence kept, as dragging a key up or down there.
    let soff = f_p(p, "speedOffset").filter(|v| v.is_finite()).unwrap_or(0.0);
    // `speedRef` (the dragged key, {layer, prop, time}): the offset is in that key's units, so
    // every selected side changes by the same share of its own segment's average speed (Position
    // in px/s and Scale in %/s move together, as in After Effects). Without it, plain units/s.
    let speed_ref = p.get("speedRef").and_then(|r| {
        let lid = LayerId(r.get("layer")?.as_u64()?);
        let uid = r.get("prop")?.as_u64()?;
        let t = Tick::from_seconds_f64(r.get("time")?.as_f64()?);
        let pr = comp.layer(lid)?.props.find(uid)?;
        let i = pr.keys.iter().position(|k| k.time == t)?;
        let avg = [false, true].iter().filter_map(|&out| seg_speed(&pr.keys, i, 0, pr.spatial, out)).fold(0.0_f64, f64::max);
        (avg > 1e-9).then_some(avg)
    });
    s.edit("Transform Keyframes", merge, |proj, st| {
        if let Some(b) = base {
            *proj = (*b).clone();
        }
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut groups: std::collections::BTreeMap<(LayerId, Uid), Vec<Tick>> = Default::default();
        for k in &sel {
            groups.entry((k.layer, k.prop)).or_default().push(k.time);
        }
        let mut new_sel = vec![];
        for ((lid, uid), times) in groups {
            let Some(layer) = comp.layer_mut(lid) else { continue };
            let (start, stretch) = (layer.start_time, layer.stretch);
            let to_comp = |lt: Tick| if (stretch - 100.0).abs() < 1e-9 { start + lt } else { start + Tick((lt.0 as f64 * stretch / 100.0) as i64) };
            let to_layer = |ct: Tick| {
                let d = ct - start;
                if (stretch - 100.0).abs() < 1e-9 { d } else { Tick((d.0 as f64 * 100.0 / stretch) as i64) }
            };
            let Some(pr) = layer.props.find_mut(uid) else { continue };
            if soff != 0.0 {
                // Every dimension: linked Scale X/Y stay together (separated dimensions are
                // properties of their own).
                speed_offset(&mut pr.keys, pr.spatial, &times, soff, speed_ref, None);
            }
            let (mut moved, mut kept): (Vec<Keyframe>, Vec<Keyframe>) = pr.keys.drain(..).partition(|k| times.contains(&k.time));
            for k in &mut moved {
                k.time = to_layer(map_t(to_comp(k.time)));
                if touch_values {
                    let mut c = k.value.components();
                    for (d, v) in c.iter_mut().enumerate() {
                        if dims.as_ref().is_none_or(|x| x.contains(&d)) {
                            *v = va + (*v - va) * vs + voff;
                        }
                    }
                    k.value = k.value.with_components(&c);
                }
            }
            // Transformed keys win over unselected keys at the same time.
            kept.retain(|k| !moved.iter().any(|m| m.time == k.time));
            moved.sort_by_key(|k| k.time);
            moved.dedup_by_key(|k| k.time);
            for k in &moved {
                new_sel.push(KeyRef { layer: lid, prop: uid, time: k.time });
            }
            kept.extend(moved);
            kept.sort_by_key(|k| k.time);
            pr.keys = kept;
            effectcraft_keyframe::retime_roving(&mut pr.keys, pr.spatial);
        }
        st.selected_keys = new_sel;
        Ok(json!({"keys": st.selected_keys.len()}))
    })
}

/// Add `off` to the speed magnitude of both sides of the keys at `times` (the dimensions `dims`,
/// or all), keeping each side's influence; the sides become Bezier. A side at speed 0 speeds up
/// the way its segment moves, so pulling it up never reverses the motion.
fn speed_offset(keys: &mut [Keyframe], spatial: bool, times: &[Tick], off: f64, reference: Option<f64>, dims: Option<&[usize]>) {
    let n_keys = keys.len();
    let mut updates = vec![];
    for (i, k) in keys.iter().enumerate().filter(|(_, k)| times.contains(&k.time)) {
        let n = if spatial { 1 } else { k.value.dims().max(1) };
        for out in [false, true] {
            let Some(j) = (if out { i.checked_add(1).filter(|j| *j < n_keys) } else { i.checked_sub(1) }) else { continue };
            let eases: Vec<Ease> = (0..n)
                .map(|d| {
                    let e = side_ease(keys, i, d, spatial, out).unwrap_or_default();
                    if n > 1 && dims.is_some_and(|x| !x.contains(&d)) {
                        return e;
                    }
                    let dir = if e.speed != 0.0 {
                        e.speed.signum()
                    } else if spatial {
                        1.0
                    } else {
                        let at = |kk: &Keyframe| kk.value.components().get(d).copied().unwrap_or(0.0);
                        let other = keys.get(j).map_or(at(k), at);
                        let rising = if out { other >= at(k) } else { at(k) >= other };
                        if rising { 1.0 } else { -1.0 }
                    };
                    let off = match reference {
                        Some(r) => off / r * seg_speed(keys, i, d, spatial, out).unwrap_or(r),
                        None => off,
                    };
                    Ease { speed: dir * (e.speed.abs() + off).max(0.0), influence: e.influence }
                })
                .collect();
            updates.push((i, out, eases));
        }
    }
    for (i, out, eases) in updates {
        let Some(k) = keys.get_mut(i) else { continue };
        if out {
            k.out_ease = eases.into();
            k.out_interp = Interp::Bezier;
        } else {
            k.in_ease = eases.into();
            k.in_interp = Interp::Bezier;
        }
        k.auto_bezier = false;
    }
}

/// Average speed (units/s) of the segment on one side of key `i`: path distance for spatial
/// properties, dimension `d` otherwise. `None` at the ends or for a zero-length segment.
fn seg_speed(keys: &[Keyframe], i: usize, d: usize, spatial: bool, out: bool) -> Option<f64> {
    let j = if out { i.checked_add(1)? } else { i.checked_sub(1)? };
    let (a, b) = (keys.get(i)?, keys.get(j)?);
    let dt = (b.time.seconds() - a.time.seconds()).abs();
    if dt <= 0.0 {
        return None;
    }
    let (ca, cb) = (a.value.components(), b.value.components());
    let dv = if spatial {
        ca.iter().zip(&cb).map(|(x, y)| (y - x).powi(2)).sum::<f64>().sqrt()
    } else {
        (cb.get(d).copied().unwrap_or(0.0) - ca.get(d).copied().unwrap_or(0.0)).abs()
    };
    (dv > 0.0).then_some(dv / dt)
}

/// Spatial tangents of one key of a spatial property (motion path handles), relative to the key
/// value. Setting one side only mirrors it to the other unless `break` is true; the key stops
/// being auto Bezier.
fn spatial_tangents(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = prop_ref(s, p, "keys.setSpatialTangents")?;
    let t = Tick::from_seconds_f64(f_p(p, "time").ok_or_else(|| bad("keys.setSpatialTangents", "missing `time` (layer s)"))?);
    let (tin, tout) = (v3(p.get("in")), v3(p.get("out")));
    if tin.is_none() && tout.is_none() {
        return Err(bad("keys.setSpatialTangents", "pass `in` and/or `out` [dx, dy, dz?]"));
    }
    let brk = b_p(p, "break").unwrap_or(false);
    s.edit("Edit Spatial Tangents", merge_p(p), |proj, _| {
        let l = super::layer_mut(proj, cid, lid)?;
        let pr = l.props.find_mut(uid).ok_or_else(|| bad("keys.setSpatialTangents", "no property"))?;
        if !pr.spatial {
            return Err(bad("keys.setSpatialTangents", "not a spatial property"));
        }
        let i = pr.keys.iter().position(|k| k.time == t).or_else(|| pr.keys.iter().enumerate().min_by_key(|(_, k)| (k.time.0 - t.0).abs()).map(|(i, _)| i));
        let i = i.ok_or_else(|| bad("keys.setSpatialTangents", "the property has no keyframes"))?;
        let (cur_in, cur_out) = effectcraft_keyframe::spatial_tangents(&pr.keys, i);
        let k = &mut pr.keys[i];
        let neg = |v: [f64; 3]| [-v[0], -v[1], -v[2]];
        let (ni, no) = match (tin, tout) {
            (Some(a), Some(b)) => (a, b),
            (Some(a), None) => (a, if brk { cur_out } else { neg(a) }),
            (None, Some(b)) => (if brk { cur_in } else { neg(b) }, b),
            (None, None) => (cur_in, cur_out),
        };
        k.spatial_auto = false;
        k.spatial_continuous = !brk && tin.is_some() != tout.is_some();
        k.spatial_in = ni;
        k.spatial_out = no;
        effectcraft_keyframe::retime_roving(&mut pr.keys, true);
        Ok(json!({"in": ni, "out": no}))
    })
}

fn insert_spatial(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = prop_ref(s, p, "keys.insertSpatial")?;
    let seconds = f_p(p, "time").filter(|t| t.is_finite()).ok_or_else(|| bad("keys.insertSpatial", "missing finite `time` (layer s)"))?;
    let raw = Tick::from_seconds_f64(seconds);
    s.edit("Insert Motion Path Vertex", None, |proj, st| {
        let fr = proj.comp(cid).ok_or(EngineError::NoComp)?.frame_rate;
        let layer = super::layer_mut(proj, cid, lid)?;
        let (start, stretch) = (layer.start_time, layer.stretch);
        let pr = layer.props.find_mut(uid).ok_or_else(|| bad("keys.insertSpatial", "no property"))?;
        // Keys sit on frames, as in After Effects (a motion path's dots are its frames): the
        // nearest comp frame strictly between the two keys around the click.
        let to_comp = |lt: Tick| start + Tick((lt.0 as f64 * stretch / 100.0) as i64);
        let to_layer = |ct: Tick| Tick(((ct - start).0 as f64 * 100.0 / stretch) as i64);
        let next = pr.keys.partition_point(|k| k.time <= raw);
        let (lo, hi) = match (next.checked_sub(1).and_then(|i| pr.keys.get(i)), pr.keys.get(next)) {
            (Some(a), Some(b)) => (to_comp(a.time), to_comp(b.time)),
            _ => return Err(bad("keys.insertSpatial", "Choose a time strictly between two motion-path keys")),
        };
        let near = fr.snap_nearest(to_comp(raw));
        if near == lo || near == hi {
            return Err(bad("keys.insertSpatial", "A keyframe already exists at this frame"));
        }
        let candidates = [near, fr.snap_nearest(near + fr.frame_duration()), fr.snap_nearest(near - fr.frame_duration())];
        let ct = candidates
            .into_iter()
            .filter(|c| *c > lo && *c < hi)
            .min_by_key(|c| (c.0 - to_comp(raw).0).abs())
            .ok_or_else(|| bad("keys.insertSpatial", "no frame between these two keyframes"))?;
        let time = to_layer(ct);
        if !pr.spatial || pr.has_expression() {
            return Err(bad("keys.insertSpatial", "requires a spatial property without an enabled expression"));
        }
        pr.keys = effectcraft_keyframe::insert_spatial(&pr.keys, time).map_err(|reason| bad("keys.insertSpatial", reason))?;
        st.selected_keys = vec![KeyRef { layer: lid, prop: uid, time }];
        Ok(json!({"time":time.seconds()}))
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!("keys.insertSpatial", "Insert Motion Path Vertex", [], None, "{layer?, path|prop, time (layer s)}", has_comp, insert_spatial),
        cmd!(
            "keys.transform",
            "Transform Keyframes",
            [],
            None,
            "{timeScale?, timeAnchor? (comp s), timeOffset? (s), speedOffset? (units/s: the speed graph's vertical drag), speedRef?: {layer, prop, time} (the dragged key: speedOffset is in its units, shared out by speed), valueScale?, valueAnchor?, valueOffset?, dim? | dims?: [d…], merge?, fromStart?: bool (with merge: values are the whole transform since the drag started)}",
            has_keys,
            transform
        ),
        cmd!(
            "keys.setSpatialTangents",
            "Edit Spatial Tangents",
            [],
            None,
            "{layer?, path|prop, time (layer s), in?: [dx,dy,dz?], out?, break?, merge?}",
            has_comp,
            spatial_tangents
        ),
    ]
}
