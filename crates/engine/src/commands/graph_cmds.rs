//! Speed graphs on key pairs (batch 1, AX2/AX3): the Graph wheel's commands.
//!
//! - `graph.copy`: the selected move's graph (its first key's out side, its second key's in
//!   side, speeds normalised to the move) onto the graph clipboard.
//! - `graph.paste`: the clipboard's (or a given) graph onto every selected move.
//! - `graph.apply {item}`: one of Devin's graphs ([`GRAPHS`], Motion Library ▸ Devin Jatho ▸
//!   Graphs), a curve or custom influences onto the selected moves (or a property's moves on
//!   `layers`).
//! - `graph.save {name}`: the selected move's graph as a Motion Library curve (`[x1, y1, x2, y2]`);
//!   the app stores it in Motion Library ▸ Curves.
//! - `graph.relinkUpdate {from, to}`: linked graphs. A move carries the graph it was given (a
//!   Library graph's exact influences and speeds), so it stays linked to it through copy and
//!   paste; when the Library graph is edited, every move still carrying the old graph gets the
//!   new one, in every composition, in one undo step.
//! - `graph.links`: the moves of a comp that carry a named graph ("Fast In · linked").
//!
//! A "move" is a pair of neighbouring keys. From a selection: each property's neighbouring
//! selected keys, or with one key selected, the move it starts (or ends, for the last key).

use std::sync::{Mutex, PoisonError};

use effectcraft_keyframe::{Ease, Interp, Keyframe, Value as KValue, side_ease, spatial_segment_length};
use effectcraft_project::{ItemId, LayerId, Uid};
use serde_json::{Value, json};

use super::peak::{GRAPHS, Graph, apply_graph, graph_named};
use super::{CommandSpec, bad, comp_id, has_comp, layers_p, match_path_of, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd, query};

/// Most moves one call changes.
const MAX_PAIRS: usize = 4096;
/// Influences closer than this (0..1) are the same graph.
const INFLUENCE_EPS: f64 = 5e-4;
/// Normalised speeds closer than this share (of the larger) are the same graph.
const SPEED_EPS: f64 = 0.01;

/// What the graph clipboard holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clip {
    /// A linear move (no ease).
    Linear,
    Graph(Graph),
}

/// The graph clipboard, shared by every session of the process like the system clipboard.
// TODO(batch1-verify): move into EditorState (engine lib.rs) when that file is free; tests that
// copy and paste must not run in parallel with each other while it is a static.
static CLIP: Mutex<Option<Clip>> = Mutex::new(None);

fn clip_get() -> Option<Clip> {
    *CLIP.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clip_set(c: Clip) {
    *CLIP.lock().unwrap_or_else(PoisonError::into_inner) = Some(c);
}

/// Library graphs edited or saved in this session (name, graph as it is now): a move carrying
/// one is linked to it by name, like Devin's [`GRAPHS`].
// TODO(batch1-verify): kept per process; a move linked to an edited Library graph shows its tag
// again after a restart only once the Curves panel reports the graph (graph.relinkUpdate).
static LIBRARY: Mutex<Vec<(String, Graph)>> = Mutex::new(Vec::new());

/// Most Library graphs remembered.
const MAX_LIBRARY: usize = 256;

fn library_set(name: &str, g: Graph) {
    let mut lib = LIBRARY.lock().unwrap_or_else(PoisonError::into_inner);
    lib.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    if lib.len() >= MAX_LIBRARY {
        lib.remove(0);
    }
    lib.push((name.to_string(), g));
}

// ---------------------------------------------------------------- graph math

/// The move `i` → `i + 1`'s graph, speeds normalised (1 = the move's linear speed) as in
/// [`GRAPHS`]: `None` for a hold, equal values or a side that isn't Bezier.
pub fn pair_graph(keys: &[Keyframe], i: usize, spatial: bool) -> Option<Graph> {
    let a = keys.get(i)?;
    let b = keys.get(i.checked_add(1)?)?;
    if a.out_interp != Interp::Bezier || b.in_interp != Interp::Bezier || a.value == b.value {
        return None;
    }
    let dur = b.time.seconds() - a.time.seconds();
    if dur.is_nan() || dur <= 0.0 {
        return None;
    }
    let is_spatial = spatial && matches!(a.value, KValue::Vec2(_) | KValue::Vec3(_));
    // The dimension that moves most stands for the move.
    let (d, delta) = if is_spatial {
        (0, spatial_segment_length(keys, i))
    } else {
        let (ca, cb) = (a.value.components(), b.value.components());
        ca.iter().zip(&cb).map(|(x, y)| y - x).enumerate().max_by(|p, q| p.1.abs().total_cmp(&q.1.abs()))?
    };
    if !delta.is_finite() || delta.abs() < 1e-12 {
        return None;
    }
    let unit = delta / dur;
    let out = side_ease(keys, i, d, spatial, true)?;
    let inn = side_ease(keys, i + 1, d, spatial, false)?;
    let norm = |e: Ease| Ease { speed: e.speed / unit, influence: e.influence };
    Some(Graph { id: "custom", name: "Custom", out: norm(out), inn: norm(inn) })
}

fn same_ease(a: Ease, b: Ease) -> bool {
    let speed_ok = (a.speed - b.speed).abs() <= SPEED_EPS * a.speed.abs().max(b.speed.abs()).max(1.0);
    (a.influence - b.influence).abs() <= INFLUENCE_EPS && speed_ok
}

/// Whether two graphs are the same (influences and normalised speeds).
pub fn same_graph(a: &Graph, b: &Graph) -> bool {
    same_ease(a.out, b.out) && same_ease(a.inn, b.inn)
}

/// The named graph ([`GRAPHS`]) the move `i` → `i + 1` carries, if any: the Timeline's
/// "Fast In · linked" tag.
pub fn linked_graph(keys: &[Keyframe], i: usize, spatial: bool) -> Option<&'static Graph> {
    let g = pair_graph(keys, i, spatial)?;
    GRAPHS.iter().find(|n| same_graph(n, &g))
}

/// The name of the Library graph the move `i` → `i + 1` is linked to: one edited or saved in
/// this session, else one of Devin's [`GRAPHS`].
pub fn linked_name(keys: &[Keyframe], i: usize, spatial: bool) -> Option<String> {
    let g = pair_graph(keys, i, spatial)?;
    let lib = LIBRARY.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((n, _)) = lib.iter().rev().find(|(_, x)| same_graph(x, &g)) {
        return Some(n.clone());
    }
    drop(lib);
    GRAPHS.iter().find(|n| same_graph(n, &g)).map(|n| n.name.to_string())
}

/// A Motion Library curve `[x1, y1, x2, y2]` (normalised cubic Bezier, time and value 0 → 1) as
/// a graph: the first handle is the out side, the second the in side.
pub fn graph_from_curve(c: [f64; 4]) -> Option<Graph> {
    let [x1, y1, x2, y2] = c;
    if !c.iter().all(|v| v.is_finite()) || !(1e-6..=1.0).contains(&x1) || !(0.0..=1.0 - 1e-6).contains(&x2) {
        return None;
    }
    let out = Ease { speed: y1 / x1, influence: x1 };
    let inn = Ease { speed: (1.0 - y2) / (1.0 - x2), influence: 1.0 - x2 };
    Some(Graph { id: "custom", name: "Custom", out, inn })
}

/// A graph as a Motion Library curve `[x1, y1, x2, y2]`.
pub fn curve_of(g: &Graph) -> [f64; 4] {
    let (x1, x2) = (g.out.influence.clamp(0.0, 1.0), (1.0 - g.inn.influence).clamp(0.0, 1.0));
    [x1, g.out.speed * x1, x2, 1.0 - g.inn.speed * (1.0 - x2)]
}

fn r4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// A clip as agents see it: `{id, name, out: {speed, influence %}, in: {…}, curve}`.
pub fn clip_json(c: &Clip) -> Value {
    match c {
        Clip::Linear => json!({"id": "linear", "name": "Linear"}),
        Clip::Graph(g) => {
            let named = GRAPHS.iter().find(|n| same_graph(n, g));
            json!({
                "id": named.map_or(g.id, |n| n.id),
                "name": named.map_or(g.name, |n| n.name),
                "linked": named.is_some(),
                "out": {"speed": r4(g.out.speed), "influence": r4(g.out.influence * 100.0)},
                "in": {"speed": r4(g.inn.speed), "influence": r4(g.inn.influence * 100.0)},
                "curve": curve_of(g).map(r4),
            })
        }
    }
}

fn ease_p(v: &Value, side: &str, c: &str) -> Result<Ease> {
    let s = v.get(side).ok_or_else(|| bad(c, format!("a custom graph needs `{side}: {{speed, influence}}`")))?;
    let speed = s.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
    let influence = s.get("influence").and_then(Value::as_f64).ok_or_else(|| bad(c, format!("`{side}.influence` (percent) is missing")))?;
    if !speed.is_finite() || speed.abs() > 1e6 || !influence.is_finite() || !(0.1..=100.0).contains(&influence) {
        return Err(bad(c, format!("`{side}`: speed must be finite, influence 0.1 to 100 (percent)")));
    }
    Ok(Ease { speed, influence: influence / 100.0 })
}

/// A graph parameter: a name or id of [`GRAPHS`] ("Fast In", "fast-in", "smooth"), "linear", a
/// curve `[x1, y1, x2, y2]`, or `{out: {speed, influence}, in: {speed, influence}}` (normalised
/// speeds, influences in percent).
pub fn spec_p(v: &Value, c: &str) -> Result<Clip> {
    match v {
        Value::String(name) => {
            if name.trim().eq_ignore_ascii_case("linear") {
                return Ok(Clip::Linear);
            }
            graph_named(name).map(Clip::Graph).ok_or_else(|| {
                bad(
                    c,
                    format!(
                        "no graph {name:?}: use one of {} (Devin Jatho ▸ Graphs), \"linear\", a curve [x1, y1, x2, y2] or {{out: {{speed, influence}}, in: {{speed, influence}}}}",
                        GRAPHS.iter().map(|g| format!("\"{}\"", g.id)).collect::<Vec<_>>().join(", ")
                    ),
                )
            })
        }
        Value::Array(a) => {
            let n: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
            let curve: [f64; 4] = n.try_into().map_err(|_| bad(c, "a curve is four numbers [x1, y1, x2, y2]"))?;
            graph_from_curve(curve).map(Clip::Graph).ok_or_else(|| bad(c, "the curve's x1 must be 0–1 (above 0) and x2 0–1 (below 1)"))
        }
        Value::Object(o) if o.contains_key("curve") => spec_p(o.get("curve").unwrap_or(&Value::Null), c),
        Value::Object(_) => Ok(Clip::Graph(Graph { id: "custom", name: "Custom", out: ease_p(v, "out", c)?, inn: ease_p(v, "in", c)? })),
        _ => Err(bad(c, "a graph is a name, \"linear\", a curve [x1, y1, x2, y2] or {out, in}")),
    }
}

/// Puts `clip` on the move `i` → `i + 1`.
fn put(keys: &mut [Keyframe], i: usize, spatial: bool, clip: &Clip) -> Result<()> {
    match clip {
        Clip::Graph(g) => apply_graph(keys, i, spatial, g),
        Clip::Linear => {
            if let Some(a) = keys.get_mut(i) {
                (a.out_interp, a.auto_bezier, a.continuous) = (Interp::Linear, false, false);
            }
            if let Some(b) = keys.get_mut(i + 1) {
                (b.in_interp, b.auto_bezier, b.continuous) = (Interp::Linear, false, false);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- the moves

/// One move: a property's key pair `i` → `i + 1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pair {
    pub layer: LayerId,
    pub prop: Uid,
    pub key: usize,
}

/// A short property name ("position", "p", "opacity"…) or a path as a property path.
pub fn property_path(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "p" | "position" => "transform/position".into(),
        "o" | "opacity" => "transform/opacity".into(),
        "s" | "scale" => "transform/scale".into(),
        "r" | "rotation" => "transform/rotation".into(),
        "a" | "anchor" | "anchorpoint" => "transform/anchorPoint".into(),
        _ => name.trim().to_string(),
    }
}

/// The moves of the selected keys in comp `cid`.
fn selected_pairs(s: &Session, cid: ItemId) -> Vec<Pair> {
    let Some(comp) = s.project.comp(cid) else { return vec![] };
    let mut seen: Vec<(LayerId, Uid)> = vec![];
    let mut out: Vec<Pair> = vec![];
    for k in &s.state.selected_keys {
        if seen.contains(&(k.layer, k.prop)) || out.len() >= MAX_PAIRS {
            continue;
        }
        seen.push((k.layer, k.prop));
        let Some(pr) = comp.layer(k.layer).and_then(|l| l.props.find(k.prop)) else { continue };
        let mut idx: Vec<usize> = s
            .state
            .selected_keys
            .iter()
            .filter(|x| x.layer == k.layer && x.prop == k.prop)
            .filter_map(|x| pr.keys.iter().position(|kf| kf.time == x.time))
            .collect();
        idx.sort_unstable();
        idx.dedup();
        let pairs: Vec<usize> = idx.iter().copied().filter(|i| idx.contains(&(i + 1))).collect();
        if !pairs.is_empty() {
            out.extend(pairs.into_iter().map(|key| Pair { layer: k.layer, prop: k.prop, key }));
            continue;
        }
        // One key: the move it starts, or the one it ends.
        let Some(&first) = idx.first() else { continue };
        if first + 1 < pr.keys.len() {
            out.push(Pair { layer: k.layer, prop: k.prop, key: first });
        } else if let Some(prev) = first.checked_sub(1) {
            out.push(Pair { layer: k.layer, prop: k.prop, key: prev });
        }
    }
    out
}

/// Every move of `path` on `layers` (pairs with different values and no hold).
fn property_pairs(s: &Session, cid: ItemId, layers: &[LayerId], path: &str) -> Vec<Pair> {
    let Some(comp) = s.project.comp(cid) else { return vec![] };
    let mut out = vec![];
    for lid in layers {
        let Some(pr) = comp.layer(*lid).and_then(|l| l.props.prop(path)) else { continue };
        for (i, w) in pr.keys.windows(2).enumerate() {
            let [a, b] = w else { continue };
            if a.out_interp != Interp::Hold && a.value != b.value && out.len() < MAX_PAIRS {
                out.push(Pair { layer: *lid, prop: pr.uid, key: i });
            }
        }
    }
    out
}

/// The moves a command acts on: `layers` + `property` (every move of that property), else the
/// selected keys.
fn pairs_p(s: &Session, p: &Value, c: &str) -> Result<(ItemId, Vec<Pair>)> {
    let cid = comp_id(s, p)?;
    let pairs = if let Some(prop) = str_p(p, "property").or_else(|| str_p(p, "path")) {
        let (cid2, layers) = layers_p(s, p)?;
        if layers.is_empty() {
            return Err(bad(c, "`property` needs `layers` (or selected layers)"));
        }
        property_pairs(s, cid2, &layers, &property_path(prop))
    } else {
        selected_pairs(s, cid)
    };
    if pairs.is_empty() {
        return Err(bad(c, "select a keyframe of a move (two keys with different values), or pass `layers` and `property`"));
    }
    let ids: Vec<LayerId> = pairs.iter().map(|q| q.layer).collect();
    let open = unlocked(s, cid, ids, c)?;
    Ok((cid, pairs.into_iter().filter(|q| open.contains(&q.layer)).collect()))
}

fn put_all(s: &mut Session, label: &str, cid: ItemId, pairs: &[Pair], clip: &Clip) -> Result<Vec<Value>> {
    let pairs = pairs.to_vec();
    let clip = *clip;
    s.edit(label, None, move |proj, _| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut done = vec![];
        for q in &pairs {
            let Some(l) = comp.layer_mut(q.layer) else { continue };
            let path = match_path_of(&l.props, q.prop);
            let name = l.name.clone();
            let Some(pr) = l.props.find_mut(q.prop) else { continue };
            let spatial = pr.spatial;
            put(&mut pr.keys, q.key, spatial, &clip)?;
            done.push(json!({"layer": q.layer.0, "layerName": name, "property": pr.name, "path": path, "key": q.key}));
        }
        Ok(done)
    })
}

// ---------------------------------------------------------------- commands

fn copy(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "graph.copy";
    let (cid, pairs) = pairs_p(s, p, C)?;
    let q = pairs.first().ok_or_else(|| bad(C, "no move selected"))?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let pr = comp.layer(q.layer).and_then(|l| l.props.find(q.prop)).ok_or_else(|| bad(C, "no property"))?;
    let clip = match pair_graph(&pr.keys, q.key, pr.spatial) {
        Some(g) => Clip::Graph(GRAPHS.iter().copied().find(|n| same_graph(n, &g)).unwrap_or(g)),
        None => Clip::Linear,
    };
    clip_set(clip);
    Ok(json!({"graph": clip_json(&clip), "from": {"layer": q.layer.0, "property": pr.name, "key": q.key}}))
}

fn paste(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "graph.paste";
    let clip = match p.get("graph").filter(|v| !v.is_null()) {
        Some(v) => spec_p(v, C)?,
        None => clip_get().ok_or_else(|| bad(C, "the graph clipboard is empty: graph.copy first, or pass `graph`"))?,
    };
    let (cid, pairs) = pairs_p(s, p, C)?;
    let done = put_all(s, "Paste Graph", cid, &pairs, &clip)?;
    Ok(json!({"graph": clip_json(&clip), "moves": done}))
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "graph.apply";
    let item = p
        .get("item")
        .or_else(|| p.get("graph"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad(C, "missing `item`: a graph name (fast-in, quick-launch, smooth-emphasis, slow-launch, fast-out), a curve or {out, in}"))?;
    let clip = spec_p(item, C)?;
    let (cid, pairs) = pairs_p(s, p, C)?;
    let label = match &clip {
        Clip::Graph(g) => format!("Apply Graph {}", g.name),
        Clip::Linear => "Apply Graph Linear".into(),
    };
    let done = put_all(s, &label, cid, &pairs, &clip)?;
    Ok(json!({"graph": clip_json(&clip), "moves": done}))
}

fn save(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "graph.save";
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(C, "missing `name`"))?.to_string();
    if name.chars().count() > 200 {
        return Err(bad(C, "`name` is at most 200 characters"));
    }
    let (cid, pairs) = pairs_p(s, p, C)?;
    let q = pairs.first().ok_or_else(|| bad(C, "no move selected"))?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let pr = comp.layer(q.layer).and_then(|l| l.props.find(q.prop)).ok_or_else(|| bad(C, "no property"))?;
    let g = pair_graph(&pr.keys, q.key, pr.spatial).ok_or_else(|| bad(C, "the move is linear or holds: there is no graph to save"))?;
    library_set(&name, g);
    let mut v = clip_json(&Clip::Graph(g));
    v["name"] = json!(name);
    Ok(json!({
        "graph": v,
        "curve": curve_of(&g).map(r4),
        // The app stores it (Motion Library ▸ Curves); a headless session only reports it.
        "library": {"command": "motionLibrary.curve", "params": {"op": "save", "name": name, "curve": curve_of(&g).map(r4)}},
    }))
}

/// Every move of the project carrying `from`, as (comp, pair).
fn carrying(s: &Session, from: &Graph) -> Vec<(ItemId, Pair)> {
    let mut out = vec![];
    for item in s.project.items.values() {
        let Some(comp) = item.as_comp() else { continue };
        for l in &comp.layers {
            l.props.walk("", &mut |_, pr| {
                for i in 0..pr.keys.len().saturating_sub(1) {
                    if out.len() < MAX_PAIRS && pair_graph(&pr.keys, i, pr.spatial).is_some_and(|g| same_graph(&g, from)) {
                        out.push((item.id, Pair { layer: l.id, prop: pr.uid, key: i }));
                    }
                }
            });
        }
    }
    out
}

fn relink_update(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "graph.relinkUpdate";
    let from = match spec_p(p.get("from").ok_or_else(|| bad(C, "missing `from`: the Library graph as it was"))?, C)? {
        Clip::Graph(g) => g,
        Clip::Linear => return Err(bad(C, "`from` can't be linear: linear moves aren't linked to anything")),
    };
    let to = spec_p(p.get("to").ok_or_else(|| bad(C, "missing `to`: the Library graph as it is now"))?, C)?;
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty());
    let mut hits = carrying(s, &from);
    // The Library card's own name ("Fast In") also finds moves carrying Devin's original values,
    // which a card read back from its keys may not match exactly.
    if let Some(g) = name.and_then(graph_named).filter(|g| !same_graph(g, &from)) {
        for h in carrying(s, &g) {
            if !hits.contains(&h) {
                hits.push(h);
            }
        }
    }
    if let (Some(n), Clip::Graph(g)) = (name, &to) {
        library_set(n, *g);
    }
    let n = hits.len();
    let mut comps: Vec<u64> = hits.iter().map(|(c, _)| c.0).collect();
    comps.dedup();
    if n == 0 {
        return Ok(json!({"updated": 0, "comps": [], "note": "no move carries that graph"}));
    }
    let label = name.map_or_else(|| "Update Linked Graphs".to_string(), |nm| format!("Update Linked Graphs {nm}"));
    s.edit(&label, None, move |proj, _| {
        for (cid, q) in &hits {
            let Some(comp) = proj.comp_mut(*cid) else { continue };
            let Some(l) = comp.layer_mut(q.layer) else { continue };
            if l.switches.locked {
                continue;
            }
            let Some(pr) = l.props.find_mut(q.prop) else { continue };
            let spatial = pr.spatial;
            put(&mut pr.keys, q.key, spatial, &to)?;
        }
        Ok(())
    })?;
    Ok(json!({"updated": n, "comps": comps, "graph": clip_json(&to)}))
}

fn links(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut out = vec![];
    for l in &comp.layers {
        l.props.walk("", &mut |path, pr| {
            for i in 0..pr.keys.len().saturating_sub(1) {
                if let Some(name) = linked_name(&pr.keys, i, pr.spatial)
                    && out.len() < MAX_PAIRS
                {
                    let id = linked_graph(&pr.keys, i, pr.spatial).map(|g| g.id);
                    out.push(json!({"layer": l.id.0, "layerName": l.name, "property": pr.name, "path": path, "key": i, "graph": id, "graphName": name}));
                }
            }
        });
    }
    Ok(json!({"comp": cid.0, "links": out, "clipboard": clip_get().map(|c| clip_json(&c))}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "graph.copy",
            "Copy Graph",
            [],
            None,
            "{comp?, layers?, property? (position|opacity|scale|rotation or a path; default: the selected keys)} → {graph: {id, name, linked, out: {speed, influence}, in, curve}}: the first selected move's speed graph onto the graph clipboard (Graph wheel, hold G)",
            has_comp,
            copy
        ),
        cmd!(
            "graph.paste",
            "Paste Graph",
            [],
            None,
            "{graph? (default the graph clipboard; a name, \"linear\", a curve [x1, y1, x2, y2] or {out: {speed, influence %}, in}), comp?, layers?, property?} → {graph, moves: [{layer, property, path, key}]}: onto every selected move (each property's neighbouring selected keys, or the move a single selected key starts). One undo step; a named graph stays linked.",
            has_comp,
            paste
        ),
        cmd!(
            "graph.apply",
            "Apply Graph",
            [],
            None,
            "{item: fast-in | quick-launch | smooth-emphasis | slow-launch | fast-out (Devin Jatho ▸ Graphs) | linear | a curve [x1, y1, x2, y2] | {out, in}, comp?, layers?, property? (every move of that property on the layers; default the selected keys)} → {graph, moves}. One undo step; the moves stay linked to the named graph (graph.relinkUpdate).",
            has_comp,
            apply
        ),
        cmd!(
            "graph.save",
            "Save Graph to Library",
            [],
            None,
            "{name, comp?, layers?, property?} → {graph, curve: [x1, y1, x2, y2], library: {command, params}}: the first selected move's graph as a Motion Library curve; the app saves it in Motion Library ▸ Curves (headless: run `library.command` yourself)",
            has_comp,
            save
        ),
        cmd!(
            "graph.relinkUpdate",
            "Update Linked Graphs",
            [],
            None,
            "{from (the Library graph as it was: a name, a curve [x1, y1, x2, y2] or {out, in}), to (as it is now), name? (the Library graph's name: also finds moves carrying Devin's graph of that name; moves carrying `to` are then tagged with it)} → {updated, comps}: every move in the project still carrying `from` gets `to`, one undo step",
            super::always,
            relink_update
        ),
        query!(
            "graph.links",
            "Linked Graphs",
            "{comp?} → {links: [{layer, layerName, property, path, key, graph, graphName}], clipboard}: the moves that carry one of Devin's graphs",
            links
        ),
    ]
}
