//! A Bezier handle dragged with several keys selected (Graph Editor, the Timeline's speed rows):
//! every selected key's handle follows, as in After Effects.

use effectcraft_keyframe::{Ease, Interp, Keyframe, key_at, side_ease, spatial_segment_length};
use effectcraft_project::{Project, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::prop::prop_ref;
use super::{CommandSpec, bad, f_p, has_comp, merge_p, str_p};
use crate::{EngineError, KeyRef, Result, Session, cmd};

const ID: &str = "keys.setEaseSelected";

/// Whether the key at `i` presents its handles as one value (a spatial property's path speed).
fn one_curve(k: &Keyframe, spatial: bool) -> bool {
    spatial && matches!(k.value, effectcraft_keyframe::Value::Vec2(_) | effectcraft_keyframe::Value::Vec3(_))
}

/// Average velocity (units/s, signed the way the value moves) of the segment on one side of key
/// `i`, dimension `d`; the path speed for a spatial property. `None` without a neighbour there.
fn seg_velocity(keys: &[Keyframe], i: usize, d: usize, spatial: bool, out: bool) -> Option<f64> {
    let j = if out { i.checked_add(1)? } else { i.checked_sub(1)? };
    let (k, n) = (keys.get(i)?, keys.get(j)?);
    let dur = (n.time.seconds() - k.time.seconds()).abs();
    if dur <= 0.0 {
        return None;
    }
    if one_curve(k, spatial) {
        return Some(spatial_segment_length(keys, i.min(j)) / dur);
    }
    let at = |kk: &Keyframe| kk.value.components().get(d).copied().unwrap_or(0.0);
    Some(if out { at(n) - at(k) } else { at(k) - at(n) } / dur)
}

/// The side of key `i` that follows a drag of an `out` (or in) handle: the same side, or, for a
/// key without a segment there (the last key for an out handle), the other one, so dragging the
/// first key's out handle of a two-key move also eases the second key in (the curve stays
/// symmetrical, After Effects).
fn follow_side(keys: &[Keyframe], i: usize, out: bool) -> Option<bool> {
    let has = |o: bool| if o { i.checked_add(1).is_some_and(|j| j < keys.len()) } else { i > 0 };
    if has(out) {
        Some(out)
    } else if has(!out) {
        Some(!out)
    } else {
        None
    }
}

/// Writes `eases` (one per presented dimension) to one side of `k`, making it Bezier; a
/// continuous key keeps matching speeds on both sides.
fn write_side(k: &mut Keyframe, out: bool, eases: &[Ease]) {
    let list = if out { &mut k.out_ease } else { &mut k.in_ease };
    *list = eases.to_vec().into();
    if out {
        k.out_interp = Interp::Bezier;
    } else {
        k.in_interp = Interp::Bezier;
    }
    k.auto_bezier = false;
    if k.continuous {
        let src = if out { k.out_ease.clone() } else { k.in_ease.clone() };
        let other = if out { &mut k.in_ease } else { &mut k.out_ease };
        if other.len() != src.len() {
            *other = src.clone();
        }
        for (o, s) in other.iter_mut().zip(&src) {
            o.speed = s.speed;
        }
        k.in_interp = Interp::Bezier;
        k.out_interp = Interp::Bezier;
    }
}

/// How the grabbed handle moved, applied to the other handles: the influence change (0..1) and
/// the speed change as a share of the grabbed segment's average velocity (so Blurriness and
/// Brightness, or Position in px/s and Scale in %/s, ease alike), or, when that segment does not
/// move, in the grabbed property's own units.
#[derive(Clone, Copy)]
struct Change {
    influence: f64,
    speed: SpeedChange,
}

#[derive(Clone, Copy)]
enum SpeedChange {
    None,
    Share(f64),
    Units(Uid, f64),
}

/// The new eases of one side of key `i` of `pr`'s keys (`dims`: the dimensions that change; the
/// others keep theirs). `lead` (the grabbed dimension, its speed and influence) is set exactly.
fn eases_for(
    keys: &[Keyframe],
    i: usize,
    spatial: bool,
    out: bool,
    uid: Uid,
    dims: Option<&[usize]>,
    ch: Change,
    lead: Option<(usize, Option<f64>, Option<f64>)>,
) -> Option<Vec<Ease>> {
    let k = keys.get(i)?;
    let n = if one_curve(k, spatial) { 1 } else { k.value.dims().max(1) };
    // Hold sides and keys of values that don't interpolate have no handle.
    side_ease(keys, i, 0, spatial, out)?;
    Some(
        (0..n)
            .map(|d| {
                let e = side_ease(keys, i, d, spatial, out).unwrap_or_default();
                if n > 1 && dims.is_some_and(|x| !x.contains(&d)) {
                    return e;
                }
                if let Some((_, sp, inf)) = lead.filter(|l| l.0 == d || n == 1) {
                    return Ease { speed: sp.unwrap_or(e.speed), influence: inf.unwrap_or(e.influence).clamp(0.001, 1.0) };
                }
                let speed = match ch.speed {
                    SpeedChange::None => e.speed,
                    SpeedChange::Share(r) => e.speed + r * seg_velocity(keys, i, d, spatial, out).unwrap_or(0.0),
                    SpeedChange::Units(u, dv) if u == uid => e.speed + dv,
                    SpeedChange::Units(..) => e.speed,
                };
                Ease { speed, influence: (e.influence + ch.influence).clamp(0.001, 1.0) }
            })
            .collect(),
    )
}

/// `keys.setEaseSelected`: put the grabbed side (`layer`, `prop`, `time`, `side`, `dim`) at
/// `speed` / `influence` as `keys.setEase` does, and move the matching side of every other
/// selected key (or `keys`) by as much: the same influence change, the same share of its own
/// segment's speed. With `merge`, each step of the drag applies to the keys as they were when it
/// started, so the handles never drift.
fn set_ease_selected(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = prop_ref(s, p, ID)?;
    let t = Tick::from_seconds_f64(f_p(p, "time").ok_or_else(|| bad(ID, "missing `time`"))?);
    let out = match str_p(p, "side") {
        Some("in") => false,
        Some("out") => true,
        _ => return Err(bad(ID, "`side` must be in or out")),
    };
    let dim = p.get("dim").and_then(Value::as_u64).map(|d| d as usize);
    // The grabbed property's dimensions that move (a linked curve: all of them; default `dim`).
    let dims: Option<Vec<usize>> = match p.get("dims").and_then(Value::as_array) {
        Some(a) => Some(a.iter().filter_map(Value::as_u64).map(|d| d as usize).collect()),
        None => dim.map(|d| vec![d]),
    };
    let speed = f_p(p, "speed").filter(|v| v.is_finite());
    let influence = f_p(p, "influence").filter(|v| v.is_finite()).map(|v| (v / 100.0).clamp(0.001, 1.0));
    let continuous = p.get("continuous").and_then(Value::as_bool);
    // Only the selected keys of other properties follow, on the same side (the Timeline's speed
    // rows: the other key of the grabbed segment must not mirror the drag).
    let respective = p.get("respective").and_then(Value::as_bool).unwrap_or(false);
    let merge = merge_p(p);
    let keys: Vec<KeyRef> = match p.get("keys").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(|k| {
                Some(KeyRef {
                    layer: effectcraft_project::LayerId(k.get("layer")?.as_u64()?),
                    prop: k.get("prop")?.as_u64()?,
                    time: Tick::from_seconds_f64(k.get("time")?.as_f64()?),
                })
            })
            .collect(),
        None => s.state.selected_keys.clone(),
    };
    // Within one drag, every step starts from the keys as they were at the press.
    let in_gesture = merge.is_some() && s.history.merge_key.as_deref() == merge;
    let base: Option<std::sync::Arc<Project>> = if in_gesture { s.history.undo.last().map(|(_, b)| b.clone()) } else { None };
    let start = base.clone().unwrap_or_else(|| s.project.clone());
    let layer = start.comp(cid).and_then(|c| c.layer(lid)).ok_or(EngineError::NoComp)?;
    let pr = layer.props.find(uid).ok_or_else(|| bad(ID, "no property"))?;
    let gi = key_at(&pr.keys, t).ok_or_else(|| bad(ID, "no keyframe at `time`"))?;
    let gk = pr.keys.get(gi).ok_or_else(|| bad(ID, "no keyframe at `time`"))?;
    let lead_dim = if one_curve(gk, pr.spatial) { 0 } else { dim.unwrap_or(0) };
    let e0 = side_ease(&pr.keys, gi, lead_dim, pr.spatial, out).ok_or_else(|| bad(ID, "that side has no handle (hold, or no neighbouring key)"))?;
    let speed_change = match speed {
        None => SpeedChange::None,
        Some(sp) => match seg_velocity(&pr.keys, gi, lead_dim, pr.spatial, out).filter(|v| v.abs() > 1e-9) {
            Some(v) => SpeedChange::Share((sp - e0.speed) / v),
            None => SpeedChange::Units(uid, sp - e0.speed),
        },
    };
    let ch = Change { influence: influence.map_or(0.0, |i| i - e0.influence), speed: speed_change };
    let grabbed = KeyRef { layer: lid, prop: uid, time: gk.time };
    // The grabbed key's handle drags the others only when it is part of the selection.
    let others: Vec<KeyRef> =
        if keys.contains(&grabbed) { keys.into_iter().filter(|k| *k != grabbed && !(respective && k.layer == lid && k.prop == uid)).collect() } else { vec![] };
    let mut moved = 1usize;
    let result = s.edit("Keyframe Velocity", merge, |proj, _| {
        if let Some(b) = &base {
            *proj = (**b).clone();
        }
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        {
            let l = comp.layer_mut(lid).ok_or(EngineError::NoComp)?;
            let pr = l.props.find_mut(uid).ok_or_else(|| bad(ID, "no property"))?;
            let lead = Some((lead_dim, speed, influence));
            let eases = eases_for(&pr.keys, gi, pr.spatial, out, uid, dims.as_deref(), ch, lead).ok_or_else(|| bad(ID, "that side has no handle"))?;
            let k = pr.keys.get_mut(gi).ok_or_else(|| bad(ID, "no keyframe"))?;
            if let Some(c) = continuous {
                k.continuous = c;
            }
            write_side(k, out, &eases);
        }
        for kr in &others {
            let Some(l) = comp.layer_mut(kr.layer) else { continue };
            let Some(pr) = l.props.find_mut(kr.prop) else { continue };
            let Some(i) = key_at(&pr.keys, kr.time) else { continue };
            let Some(side) = follow_side(&pr.keys, i, out).filter(|side| !respective || *side == out) else { continue };
            // On the grabbed property the same dimensions move; other properties move whole.
            let d = if kr.prop == uid && kr.layer == lid { dims.as_deref() } else { None };
            let Some(eases) = eases_for(&pr.keys, i, pr.spatial, side, kr.prop, d, ch, None) else { continue };
            let Some(k) = pr.keys.get_mut(i) else { continue };
            write_side(k, side, &eases);
            moved += 1;
        }
        Ok(())
    });
    result.map(|()| json!({"keys": moved}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "keys.setEaseSelected",
        "Set Ease of Selected Keyframes",
        [],
        None,
        "{layer?, path|prop, time (layer s), side: in|out, dim?, dims?: [d…] (the grabbed property's dimensions that move), speed?, influence? %, continuous?: bool, keys?: [{layer, prop, time}] (default: the selection), respective?: bool (only other properties' keys follow, never flipping side), merge?} — the grabbed side gets speed/influence; the matching side of every other selected key moves by as much (same influence change, same share of its segment's speed); one undo step",
        has_comp,
        set_ease_selected
    )]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    fn ease(s: &Session, layer: u64, path: &str, i: usize, out: bool) -> effectcraft_keyframe::Ease {
        let comp = s.active_comp().unwrap();
        let pr = comp.layer(effectcraft_project::LayerId(layer)).unwrap().props.prop(path).unwrap();
        effectcraft_keyframe::side_ease(&pr.keys, i, 0, pr.spatial, out).unwrap()
    }

    /// Two properties with two eased keys each, all four selected (Elias's Blurriness and
    /// Brightness, 22 Oct 2026): dragging one handle eases every key, in one undo step.
    #[test]
    fn a_handle_drag_eases_every_selected_key() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "C", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap();
        s.execute("layer.newSolid", json!({"name": "A", "width": 8, "height": 8})).unwrap();
        let l = s.active_comp().unwrap().layers[0].id.0;
        for (path, a, b) in [("transform/opacity", 0.0, 100.0), ("transform/rotation", 0.0, -30.0)] {
            s.execute("prop.addKey", json!({"layer": l, "path": path, "time": 0.0, "value": a})).unwrap();
            s.execute("prop.addKey", json!({"layer": l, "path": path, "time": 1.0, "value": b})).unwrap();
        }
        s.execute("keys.selectAll", json!({})).unwrap();
        s.execute("keys.easyEase", json!({})).unwrap();
        s.execute("keys.selectAll", json!({})).unwrap();
        assert_eq!(s.state.selected_keys.len(), 4);
        let undo_before = s.history.undo.len();
        // Two drag steps of one gesture on Opacity's first key, out handle: 33 % → 80 %, speed up.
        for (inf, sp) in [(60.0, 20.0), (80.0, 50.0)] {
            s.execute(
                "keys.setEaseSelected",
                json!({"layer": l, "path": "transform/opacity", "time": 0.0, "side": "out", "influence": inf, "speed": sp, "merge": "drag"}),
            )
            .unwrap();
        }
        s.history.merge_key = None;
        assert_eq!(s.history.undo.len(), undo_before + 1, "one undo step");
        let o_out = ease(&s, l, "transform/opacity", 0, true);
        assert!((o_out.influence - 0.8).abs() < 1e-9 && (o_out.speed - 50.0).abs() < 1e-9);
        // The second key's in handle mirrors it: same influence, same share of the speed.
        let o_in = ease(&s, l, "transform/opacity", 1, false);
        assert!((o_in.influence - 0.8).abs() < 1e-6, "{o_in:?}");
        assert!((o_in.speed - 50.0).abs() < 1e-6, "{o_in:?}");
        // Rotation (other units, falling): same influence, half the opacity's 100 → 30 ° share,
        // signed the way it moves.
        for i in [0, 1] {
            let e = ease(&s, l, "transform/rotation", i, i == 0);
            assert!((e.influence - 0.8).abs() < 1e-6, "{e:?}");
            assert!((e.speed + 15.0).abs() < 1e-6, "{e:?}");
        }
        assert!(s.undo());
        assert!((ease(&s, l, "transform/rotation", 0, true).influence - 1.0 / 3.0).abs() < 1e-3);
        // `respective` (the Timeline's speed rows, Elias 23 Oct 2026): Opacity's second key's in
        // handle; Rotation's second key follows on its in side, Opacity's first key doesn't
        // mirror it and Rotation's first key (no in side) stays.
        s.execute("keys.setEaseSelected", json!({"layer": l, "path": "transform/opacity", "time": 1.0, "side": "in", "influence": 70.0, "respective": true}))
            .unwrap();
        assert!((ease(&s, l, "transform/opacity", 1, false).influence - 0.7).abs() < 1e-6);
        assert!((ease(&s, l, "transform/rotation", 1, false).influence - 0.7).abs() < 1e-6, "Rotation's respective key follows");
        assert!((ease(&s, l, "transform/opacity", 0, true).influence - 1.0 / 3.0).abs() < 1e-3, "the segment's other key stays");
        assert!((ease(&s, l, "transform/rotation", 0, true).influence - 1.0 / 3.0).abs() < 1e-3, "no side flips");
    }

    /// With only the grabbed key selected (or none), the other keys stay.
    #[test]
    fn unselected_keys_keep_their_handles() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "C", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap();
        s.execute("layer.newSolid", json!({"name": "A", "width": 8, "height": 8})).unwrap();
        let l = s.active_comp().unwrap().layers[0].id.0;
        for t in [0.0, 1.0] {
            s.execute("prop.addKey", json!({"layer": l, "path": "transform/opacity", "time": t, "value": t * 100.0})).unwrap();
        }
        s.execute("keys.selectAll", json!({})).unwrap();
        s.execute("keys.easyEase", json!({})).unwrap();
        s.execute("keys.select", json!({"keys": []})).unwrap();
        s.execute("keys.setEaseSelected", json!({"layer": l, "path": "transform/opacity", "time": 1.0, "side": "in", "influence": 90.0})).unwrap();
        assert!((ease(&s, l, "transform/opacity", 1, false).influence - 0.9).abs() < 1e-9);
        assert!((ease(&s, l, "transform/opacity", 0, true).influence - 1.0 / 3.0).abs() < 1e-3);
    }
}
