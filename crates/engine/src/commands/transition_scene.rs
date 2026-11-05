//! The scene cuts of `transition.apply` (Animation ▸ Transitions ▸ Scene Cut): Elias's 18 approved
//! scene changes where the shots themselves move, open or dissolve (push-slide, vertical-push,
//! elastic-push, squeeze, circle-iris, diamond-iris, diagonal-split, card-flip-3d, crossfade,
//! blur-crossfade, focus-pull, color-dip, staggered-blocks, horizontal-blinds, vertical-blinds, and from
//! hf-keyframes-demo linear-wipe, point-iris, split-handoff). A
//! port of motion-kit's shelf part `scene-cut` (`shelf/transitions/scene-cut.jsx`, verified against
//! the approved renders at 24/25/29.97/59.94 and every `dir`); every number comes from the same data
//! table, shipped here as `presets/scene-cut.kinds.json` (re-copied when the shelf table changes, see
//! its sidecar).
//!
//! The table has the FX cuts' schema ([`super::transition_fx`] builds both) plus: `dir` (a 2×2
//! matrix per direction that maps every point, offset, pivot and mask vertex of the kind, and swaps
//! `…Width` / `…Height` params and strip sizes when it swaps the axes), `stack: new-over-old` (the new
//! shot goes right above the old one when it is below: it slides or fades in over it), Transform
//! pivots, `hold` keys and straight paths, polygon mask paths, copies of a shot with their own trims
//! or turned in 3-D (card-flip-3d, through the comp's camera), sized solids (the blinds' strips), a
//! new shot that starts after the cut (color-dip) and the dark #08080b backing where the approved
//! renders show it (squeeze, card-flip-3d, blur-crossfade, color-dip). `stack: old-over-new` (linear-wipe,
//! point-iris: a copy of the new shot uncovered over the old one) moves the old shot right above the
//! new one; `centre` (point-iris) moves its circle and grows its last radius to still cover the frame;
//! every copy of a shot must have picture over its whole span (split-handoff's copies carry both
//! shots for 1.9 s). The shots' transforms are
//! never touched; everything added is named `SceneCut <kind> …` and tagged `SceneCut <old>-><new>`.

use std::sync::OnceLock;

use effectcraft_project::{ItemId, LayerId};
use serde_json::{Map, Value, json};

use super::bad;
use super::transition::{Applied, C, Removed, arr, pt};
use super::transition_fx::{Family, apply_family};
use crate::{EngineError, Result, Session};

/// The scene-cut kinds: a copy of motion-kit's `shelf/transitions/scene-cut.kinds.json`.
pub const SCENE_CUT_JSON: &str = include_str!("../../presets/scene-cut.kinds.json");

/// Everything a scene cut adds is named with this prefix (effects, layers, precomps).
pub(super) const PREFIX: &str = "SceneCut ";
/// How deep a kind's specs nest (a comp overlay's layers, a matte).
const MAX_DEPTH: usize = 8;

pub(super) fn table() -> Result<&'static Value> {
    static T: OnceLock<std::result::Result<Value, String>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(SCENE_CUT_JSON).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| EngineError::Other(format!("{C}: the scene-cut table doesn't parse: {e}")))
}

/// The scene-cut kinds in menu order.
pub(super) fn kinds() -> Vec<String> {
    table()
        .ok()
        .and_then(|t| t.get("order"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub(super) fn is_kind(kind: &str) -> bool {
    table().ok().and_then(|t| t.get("kinds")).and_then(|k| k.get(kind)).is_some()
}

fn kind_spec(kind: &str) -> Result<&'static Value> {
    table()?.get("kinds").and_then(|k| k.get(kind)).ok_or_else(|| bad(C, format!("unknown scene cut `{kind}`; kinds: {}", kinds().join(", "))))
}

/// A kind's directions: its default first, then the others in table order (none: it takes no `dir`).
pub(super) fn dirs(k: &Value) -> Vec<String> {
    let Some(d) = k.get("dir") else { return vec![] };
    let default = d.get("default").and_then(Value::as_str);
    let mut out: Vec<String> = default.map(String::from).into_iter().collect();
    if let Some(Value::Object(maps)) = d.get("maps") {
        out.extend(maps.keys().filter(|m| Some(m.as_str()) != default).cloned());
    }
    out
}

/// `transition.kinds`' scene-cut rows.
pub(super) fn list() -> Vec<Value> {
    let Ok(t) = table() else { return vec![] };
    let default = t.get("default").and_then(Value::as_str).unwrap_or("push-slide");
    kinds()
        .iter()
        .filter_map(|id| {
            let k = t.get("kinds")?.get(id)?;
            Some(json!({
                "kind": id,
                "family": "scene-cut",
                "default": id == default,
                "title": k.get("title"),
                "energy": k.get("energy"),
                "window": k.get("window"),
                "land": k.get("land"),
                "looks": k.get("looks"),
                "use": k.get("use"),
                "dir": k.get("dir").and_then(|d| d.get("default")),
                "dirs": dirs(k),
            }))
        })
        .collect()
}

/// `dir` for a scene cut: `None` = the kind as built (no `dir`, or its default); else the 2×2
/// matrix. A `dir` on a kind without one, or one it doesn't have, is an error.
pub(super) fn dir_matrix(kind: &str, p: &Value) -> Result<Option<[[f64; 2]; 2]>> {
    let Some(v) = p.get("dir").filter(|v| !v.is_null()) else { return Ok(None) };
    let k = kind_spec(kind)?;
    let ds = dirs(k);
    let Some(d) = k.get("dir") else { return Err(bad(C, format!("{kind} takes no `dir` (scene cuts with one: {})", with_dirs().join(", ")))) };
    let name = v.as_str().ok_or_else(|| bad(C, format!("`dir` is one of {} for {kind}", ds.join(", "))))?;
    let m = d.get("maps").and_then(|m| m.get(name)).ok_or_else(|| bad(C, format!("{kind} has no dir `{name}` (dirs: {})", ds.join(", "))))?;
    if d.get("default").and_then(Value::as_str) == Some(name) {
        return Ok(None);
    }
    let row = |r: Option<&Value>| pt(r).ok_or_else(|| bad(C, format!("{kind}: dir `{name}` isn't a 2×2 matrix in the table")));
    let rows = m.as_array().filter(|a| a.len() == 2).ok_or_else(|| bad(C, format!("{kind}: dir `{name}` isn't a 2×2 matrix in the table")))?;
    Ok(Some([row(rows.first())?, row(rows.get(1))?]))
}

/// The kinds that take a `dir`.
fn with_dirs() -> Vec<String> {
    let Ok(t) = table() else { return vec![] };
    kinds().into_iter().filter(|id| t.get("kinds").and_then(|k| k.get(id)).is_some_and(|k| k.get("dir").is_some())).collect()
}

/// A kind's data mapped to another direction (the shelf's `scMapKind_`): every geometric value by
/// the matrix `m` in centred, normalised frame coords (u = x / 1920 − 0.5, v = y / 1080 − 0.5).
struct Dir {
    m: [[f64; 2]; 2],
    /// The matrix swaps the axes: `…Width` ↔ `…Height` params, strip sizes [w, h] → [h, w].
    swap: bool,
}

impl Dir {
    fn new(m: [[f64; 2]; 2]) -> Dir {
        Dir { m, swap: m[0][0] == 0.0 }
    }

    /// An absolute point (px of 1920×1080).
    fn point(&self, v: &Value, kind: &str) -> Result<Value> {
        let p = pt(Some(v)).ok_or_else(|| bad(C, format!("{kind}: a point to map is [x, y], not {v}")))?;
        let (u, w) = (p[0] / 1920.0 - 0.5, p[1] / 1080.0 - 0.5);
        let m = self.m;
        Ok(json!([(m[0][0] * u + m[0][1] * w + 0.5) * 1920.0, (m[1][0] * u + m[1][1] * w + 0.5) * 1080.0]))
    }

    /// An offset [dx, dy] (px of 1920×1080).
    fn offset(&self, v: &Value, kind: &str) -> Result<Value> {
        let p = pt(Some(v)).ok_or_else(|| bad(C, format!("{kind}: an offset to map is [dx, dy], not {v}")))?;
        let (u, w) = (p[0] / 1920.0, p[1] / 1080.0);
        let m = self.m;
        Ok(json!([(m[0][0] * u + m[0][1] * w) * 1920.0, (m[1][0] * u + m[1][1] * w) * 1080.0]))
    }

    /// A parameter's name: `…Width` ↔ `…Height` when the axes swap.
    fn param(&self, n: &str) -> String {
        if !self.swap {
            return n.to_string();
        }
        if let Some(stem) = n.strip_suffix("Width") {
            format!("{stem}Height")
        } else if let Some(stem) = n.strip_suffix("Height") {
            format!("{stem}Width")
        } else {
            n.to_string()
        }
    }

    /// A track on `path`: position values (absolute, or offsets with `offset`), mask path polygons.
    fn track(&self, path: &str, tr: &Value, kind: &str) -> Result<Value> {
        let mut t = tr.clone();
        let last = path.rsplit('/').next().unwrap_or(path);
        let offset = tr.get("offset").and_then(Value::as_bool).unwrap_or(false);
        if let Some(Value::Array(vs)) = t.get_mut("v") {
            for v in vs.iter_mut() {
                match last {
                    "position" => *v = if offset { self.offset(v, kind)? } else { self.point(v, kind)? },
                    "path" => {
                        let pts = v.as_array().ok_or_else(|| bad(C, format!("{kind}: a mask path value is a list of points")))?;
                        *v = Value::Array(pts.iter().map(|p| self.point(p, kind)).collect::<Result<_>>()?);
                    }
                    _ => {}
                }
            }
        }
        Ok(t)
    }

    /// `{param: track}` with the params renamed (`rename`) and the tracks mapped.
    fn tracks(&self, keys: &Value, rename: bool, kind: &str) -> Result<Value> {
        let Value::Object(m) = keys else { return Ok(keys.clone()) };
        let mut out = Map::new();
        for (p, tr) in m {
            out.insert(if rename { self.param(p) } else { p.clone() }, self.track(p, tr, kind)?);
        }
        Ok(Value::Object(out))
    }

    fn effects(&self, list: &[Value], kind: &str) -> Result<Value> {
        let mut out = vec![];
        for e in list {
            let mut e2 = e.clone();
            if let Some(o) = e2.as_object_mut() {
                if let Some(Value::Object(set)) = e.get("set") {
                    o.insert("set".into(), Value::Object(set.iter().map(|(p, v)| (self.param(p), v.clone())).collect()));
                }
                if let Some(keys) = e.get("keys") {
                    o.insert("keys".into(), self.tracks(keys, true, kind)?);
                }
                if let Some(pv) = e.get("pivot") {
                    o.insert("pivot".into(), self.point(pv, kind)?);
                }
            }
            out.push(e2);
        }
        Ok(Value::Array(out))
    }

    /// One layer / overlay spec (its effects, keys, position, strip size, mask, matte, child layers).
    fn spec(&self, s: &Value, kind: &str, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(bad(C, format!("{kind}: the table nests too deep")));
        }
        let mut o = s.clone();
        let Some(om) = o.as_object_mut() else { return Ok(o) };
        if let Some(Value::Array(effs)) = s.get("effects") {
            om.insert("effects".into(), self.effects(effs, kind)?);
        }
        if let Some(keys) = s.get("keys") {
            om.insert("keys".into(), self.tracks(keys, false, kind)?);
        }
        if let Some(p) = s.get("position") {
            om.insert("position".into(), self.point(p, kind)?);
        }
        if self.swap
            && let Some(sz) = pt(s.get("size"))
        {
            om.insert("size".into(), json!([sz[1] * 1920.0 / 1080.0, sz[0] * 1080.0 / 1920.0]));
        }
        if let Some(m) = s.get("mask").filter(|m| m.is_object()) {
            if m.get("rect").is_some() {
                return Err(bad(C, format!("{kind}: a dir can't map a rect mask")));
            }
            let mut m2 = m.clone();
            if let Some(mo) = m2.as_object_mut() {
                if let Some(c) = m.get("circle") {
                    mo.insert("circle".into(), self.point(c, kind)?);
                }
                if let Some(tr) = m.get("keys").and_then(|k| k.get("path"))
                    && let Some(Value::Object(ks)) = mo.get_mut("keys")
                {
                    ks.insert("path".into(), self.track("path", tr, kind)?);
                }
            }
            om.insert("mask".into(), m2);
        }
        if let Some(m) = s.get("matte").filter(|m| m.is_object()) {
            om.insert("matte".into(), self.spec(m, kind, depth.saturating_add(1))?);
        }
        if let Some(Value::Array(ls)) = s.get("layers") {
            om.insert("layers".into(), Value::Array(ls.iter().map(|l| self.spec(l, kind, depth.saturating_add(1))).collect::<Result<_>>()?));
        }
        Ok(o)
    }

    fn kind(&self, k: &Value, kind: &str) -> Result<Value> {
        let mut m = k.clone();
        let Some(mo) = m.as_object_mut() else { return Ok(m) };
        if let Some(Value::Object(roles)) = k.get("layers") {
            let mut out = Map::new();
            for (r, sp) in roles {
                out.insert(r.clone(), self.spec(sp, kind, 0)?);
            }
            mo.insert("layers".into(), Value::Object(out));
        }
        if k.get("overlays").is_some() {
            let ovs = arr(k, "overlays").iter().map(|o| self.spec(o, kind, 0)).collect::<Result<Vec<_>>>()?;
            mo.insert("overlays".into(), Value::Array(ovs));
        }
        Ok(m)
    }
}

/// Whether a scene cut takes a `centre` (the table's `centre`: point-iris).
pub(super) fn takes_centre(kind: &str) -> bool {
    kind_spec(kind).is_ok_and(|k| k.get("centre").is_some())
}

/// A scene cut's `centre`: [x, y] px of 1920×1080 (none: the kind as built).
pub(super) fn centre_p(p: &Value) -> Result<Option<[f64; 2]>> {
    match p.get("centre").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => {
            pt(Some(v)).filter(|c| c[0].abs() <= 100_000.0 && c[1].abs() <= 100_000.0).map(Some).ok_or_else(|| bad(C, "`centre` is [x, y] (px of 1920×1080)"))
        }
    }
}

/// `centre` (the shelf's `scCentre_`): the kind's named overlay's mask circle goes to `c` (px of
/// 1920×1080), and its last radius grows to the farthest frame corner + the table's margin when
/// the table's would no longer cover the frame from there.
fn centre_kind(kind: &str, k: &Value, c: [f64; 2]) -> Result<Value> {
    let spec = k.get("centre").ok_or_else(|| bad(C, format!("{kind} takes no `centre`")))?;
    let target = spec.get("overlay").and_then(Value::as_str).ok_or_else(|| bad(C, format!("{kind}: the table's centre names no overlay")))?;
    let margin = spec.get("margin").and_then(Value::as_f64).filter(|m| m.is_finite()).unwrap_or(0.0);
    let far = [[0.0, 0.0], [1920.0, 0.0], [0.0, 1080.0], [1920.0, 1080.0]].iter().map(|p: &[f64; 2]| (p[0] - c[0]).hypot(p[1] - c[1])).fold(0.0, f64::max);
    let mut m = k.clone();
    let mut done = false;
    if let Some(Value::Array(ovs)) = m.get_mut("overlays") {
        for ov in ovs.iter_mut() {
            if ov.get("name").and_then(Value::as_str) != Some(target) {
                continue;
            }
            let Some(mask) = ov.get_mut("mask").filter(|m| m.get("circle").is_some()) else { continue };
            mask["circle"] = json!([c[0], c[1]]);
            if let Some(Value::Array(vs)) = mask.get_mut("keys").and_then(|k| k.get_mut("radius")).and_then(|r| r.get_mut("v"))
                && let Some(last) = vs.last_mut()
            {
                let r = last.as_f64().unwrap_or(0.0);
                *last = json!(r.max((far + margin).ceil()));
            }
            done = true;
        }
    }
    if !done {
        return Err(bad(C, format!("{kind}: no circle overlay “{target}” for the centre")));
    }
    Ok(m)
}

/// A scene cut on the pair (the previous transition is gone already; `dir` checked already).
pub(super) fn apply(s: &mut Session, p: &Value, kind: &str, cid: ItemId, old: LayerId, new: LayerId, removed: Removed) -> Result<Applied> {
    let k = kind_spec(kind)?;
    // The dir first, then the centre (as the shelf).
    let mut k = match dir_matrix(kind, p)? {
        Some(m) => Dir::new(m).kind(k, kind)?,
        None => k.clone(),
    };
    if let Some(c) = centre_p(p)? {
        k = centre_kind(kind, &k, c)?;
    }
    let mut a = apply_family(s, Family::Scene, &k, p, kind, cid, old, new, removed)?;
    let dir = p.get("dir").filter(|v| !v.is_null()).cloned().or_else(|| k.get("dir").and_then(|d| d.get("default")).cloned()).unwrap_or(Value::Null);
    let centre =
        k.get("overlays").and_then(Value::as_array).zip(k.get("centre").and_then(|c| c.get("overlay")).and_then(Value::as_str)).and_then(|(ovs, name)| {
            ovs.iter().find(|o| o.get("name").and_then(Value::as_str) == Some(name)).and_then(|o| o.get("mask")).and_then(|m| m.get("circle")).cloned()
        });
    if let Some(o) = a.reply.as_object_mut() {
        o.insert("dir".into(), dir);
        if let Some(c) = centre {
            o.insert("centre".into(), c);
        }
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_maps_points_offsets_and_swaps_axes() {
        // up: [[0, 1], [1, 0]] (push-slide's) swaps the axes.
        let d = Dir::new([[0.0, 1.0], [1.0, 0.0]]);
        assert!(d.swap);
        assert_eq!(d.offset(&json!([-1920, 0]), "t").ok(), Some(json!([0.0, -1080.0])));
        assert_eq!(d.point(&json!([0, 540]), "t").ok(), Some(json!([960.0, 0.0])));
        assert_eq!(d.param("scaleWidth"), "scaleHeight");
        assert_eq!(d.param("outputHeight"), "outputWidth");
        assert_eq!(d.param("opacity"), "opacity");
        let sp = d.spec(&json!({"kind": "solid", "size": [1920, 135]}), "t", 0).ok().and_then(|s| pt(s.get("size")));
        assert_eq!(sp, Some([240.0, 1080.0]));
        // right: a mirror, no swap.
        let d = Dir::new([[-1.0, 0.0], [0.0, 1.0]]);
        assert!(!d.swap);
        assert_eq!(d.point(&json!([-960, 540]), "t").ok(), Some(json!([2880.0, 540.0])));
        assert!(d.spec(&json!({"mask": {"rect": [0, 0, 10, 10]}}), "t", 0).is_err(), "a rect mask can't be mapped");
    }
}
