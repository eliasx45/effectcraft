//! Bending a motion-path segment between two keys by their spatial tangents (Pen drag on the
//! path in the viewer): no key is added.

use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::prop::prop_ref;
use super::{CommandSpec, bad, f_p, has_comp, merge_p};
use crate::{Result, Session, cmd};

fn v3(v: Option<&Value>) -> Option<[f64; 3]> {
    let a = v?.as_array()?;
    Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2).and_then(Value::as_f64).unwrap_or(0.0)])
}

/// Bend one motion-path segment without adding a key: the point at Bezier parameter `u` (or the
/// one nearest `point`, parent space) moves by `offset`, changing only the out tangent of the key
/// at or before `time` and the in tangent of the next key (Pen drag on a motion path). Times,
/// values and temporal ease are kept.
fn bend_segment(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "keys.bendSpatialSegment";
    let (cid, lid, uid) = prop_ref(s, p, ID)?;
    let t = Tick::from_seconds_f64(f_p(p, "time").filter(|t| t.is_finite()).ok_or_else(|| bad(ID, "missing finite `time` (layer s)"))?);
    let offset = v3(p.get("offset")).ok_or_else(|| bad(ID, "missing `offset` [dx, dy, dz?] (parent space)"))?;
    let (u, point) = (f_p(p, "u"), v3(p.get("point")));
    s.edit("Bend Motion Path", merge_p(p), |proj, _| {
        let l = super::layer_mut(proj, cid, lid)?;
        let pr = l.props.find_mut(uid).ok_or_else(|| bad(ID, "no property"))?;
        if !pr.spatial {
            return Err(bad(ID, "not a spatial property"));
        }
        let i = pr.keys.partition_point(|k| k.time <= t).checked_sub(1).ok_or_else(|| bad(ID, "Choose a time between two motion-path keys"))?;
        let u = match (u, point) {
            (Some(u), _) => u,
            (None, Some(q)) => {
                effectcraft_keyframe::spatial_segment_param(&pr.keys, i, [q[0], q[1]]).ok_or_else(|| bad(ID, "Choose a time between two motion-path keys"))?
            }
            (None, None) => return Err(bad(ID, "pass `u` (0–1) or `point` [x, y]")),
        };
        pr.keys = effectcraft_keyframe::bend_spatial_segment(&pr.keys, i, u, offset).map_err(|reason| bad(ID, reason))?;
        effectcraft_keyframe::retime_roving(&mut pr.keys, true);
        Ok(json!({"u": u}))
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "keys.bendSpatialSegment",
        "Bend Motion Path",
        [],
        None,
        "{layer?, path|prop, time (layer s, in the segment), u? (0–1) | point? [x, y] (parent space), offset: [dx, dy, dz?], merge?}",
        has_comp,
        bend_segment
    )]
}
