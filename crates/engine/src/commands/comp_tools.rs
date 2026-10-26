//! Comp tools of the Motion Library panel's quick-action bar, as the library's After Effects
//! panel has them: Smart Precomp, Multi Precomp, Decomp, Deep Duplicate, Split Masks and Gradient
//! Lock. Each is one undo step, and each refuses what it can't keep exact, saying why.

use std::collections::{BTreeMap, BTreeSet};

use effectcraft_color::Label;
use effectcraft_keyframe::Value as KV;
use effectcraft_project::{Comp, Expression, GroupKind, ItemId, ItemKind, Layer, LayerId, LayerSource, MaskMode, PropGroup};
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, has_comp, has_layers, layers_p, str_p};
use crate::{EngineError, Result, Session, cmd};

/// Property trees deeper than this are refused (they come from files).
const MAX_DEPTH: usize = 64;

fn has_expression(g: &PropGroup) -> bool {
    let mut found = false;
    g.walk("", &mut |_, p| found |= p.has_expression());
    found
}

/// A property is at `want` and stays there (no keyframes, no expression).
fn fixed(l: &Layer, path: &str, want: &[f64]) -> bool {
    let Some(p) = l.props.prop(path) else { return false };
    if p.is_animated() || p.has_expression() {
        return false;
    }
    let got: Vec<f64> = match &p.value {
        KV::Scalar(v) => vec![*v],
        KV::Vec2(v) => v.to_vec(),
        KV::Vec3(v) => v.to_vec(),
        _ => return false,
    };
    want.iter().enumerate().all(|(i, w)| got.get(i).is_some_and(|g| (g - w).abs() < 1e-6))
}

/// The selected layers in stacking order, checked for precomposing as one group: consecutive,
/// unlocked, parents and track mattes inside the group with their layers.
fn check_group(comp: &Comp, ids: &[LayerId], c: &str) -> Result<Vec<usize>> {
    let mut idx: Vec<usize> = ids.iter().filter_map(|id| comp.layers.iter().position(|l| l.id == *id)).collect();
    idx.sort_unstable();
    for w in idx.windows(2) {
        if let [a, b] = w
            && *b != a + 1
        {
            return Err(bad(c, "select consecutive layers to keep the stacking order"));
        }
    }
    let inside = |id: LayerId| ids.contains(&id);
    for l in comp.layers.iter() {
        if inside(l.id) && l.switches.locked {
            return Err(bad(c, format!("unlock “{}” first", l.name)));
        }
        if let Some(p) = l.parent
            && inside(l.id) != inside(p)
        {
            return Err(bad(c, format!("keep parents and children together: “{}”", l.name)));
        }
        if let Some(m) = l.track_matte
            && inside(l.id) != inside(m.layer)
        {
            return Err(bad(c, format!("keep each track matte with its layer: “{}”", l.name)));
        }
    }
    Ok(idx)
}

/// A project item name not taken yet (`Precomp`, `Precomp 2`…).
fn free_name(s: &Session, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Precomp" } else { base };
    let taken: BTreeSet<&str> = s.project.items.values().map(|i| i.name.as_str()).collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..10_000).map(|k| format!("{base} {k}")).find(|n| !taken.contains(n.as_str())).unwrap_or_else(|| base.to_string())
}

fn precompose_group(s: &mut Session, cid: ItemId, ids: &[LayerId], name: Option<&str>, label: Option<Label>) -> Result<Value> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let base = match (name, ids) {
        (Some(n), _) => n.to_string(),
        (None, [one]) => format!("{} Precomp", comp.layer(*one).map(|l| l.name.as_str()).unwrap_or("Layer")),
        _ => "Precomp".into(),
    };
    let name = free_name(s, &base);
    let r = s.execute(
        "layer.precompose",
        json!({"comp": cid.0, "layers": ids.iter().map(|l| l.0).collect::<Vec<_>>(), "name": name, "mode": "move", "adjustDuration": true}),
    )?;
    if let Some(label) = label {
        let (item, layer) = (r["comp"].as_u64(), r["layer"].as_u64());
        s.edit("Label", None, |proj, _| {
            if let Some(it) = item.and_then(|i| proj.item_mut(ItemId(i))) {
                it.label = label;
            }
            if let Some(lid) = layer
                && let Some(c) = proj.comp_mut(cid)
                && let Some(l) = c.layers.iter_mut_where(move |x| x.id.0 == lid).next()
            {
                l.label = label;
            }
            Ok(())
        })?;
    }
    Ok(json!({"comp": r["comp"], "layer": r["layer"], "name": name}))
}

fn label_p(p: &Value, c: &str) -> Result<Option<Label>> {
    match p.get("label") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Label::from_name(s).map(Some).ok_or_else(|| bad(c, format!("unknown label `{s}`"))),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| Label::ALL.get(i as usize).copied()).map(Some).ok_or_else(|| bad(c, "label index 0–16")),
        _ => Err(bad(c, "`label` is a label name or 0–16")),
    }
}

/// Smart Precomp: the selected (consecutive) layers into one precomp trimmed to their span.
fn smart_precompose(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.smartPrecompose";
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(C, "select timeline layers first"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    check_group(comp, &ids, C)?;
    let label = label_p(p, C)?;
    let snap = s.project.clone();
    let r = precompose_group(s, cid, &ids, str_p(p, "name"), label);
    super::batch::fold(s, &snap, "Smart Precomp");
    r
}

/// Multi Precomp: the selected layers into precomps of `count` consecutive layers each (one per
/// layer by default), or per run of one label (`byLabel`).
fn multi_precompose(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.multiPrecompose";
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(C, "select timeline layers first"));
    }
    let size = p.get("count").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=10_000).contains(&size) {
        return Err(bad(C, "`count` (layers per precomp) is 1–10000"));
    }
    let by_label = b_p(p, "byLabel").unwrap_or(false);
    let label = label_p(p, C)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut picked: Vec<(usize, &Layer)> = comp.layers.iter().enumerate().filter(|(_, l)| ids.contains(&l.id)).collect();
    picked.sort_by_key(|(i, _)| *i);
    // Runs of consecutive layers, at most `size` long (and of one label with `byLabel`).
    let mut groups: Vec<Vec<LayerId>> = vec![];
    let mut last: Option<(usize, Label)> = None;
    for (i, l) in picked {
        let split = match (groups.last(), last) {
            (Some(g), Some((li, ll))) => g.len() as u64 >= size || i != li + 1 || (by_label && l.label != ll),
            _ => true,
        };
        if split {
            groups.push(vec![]);
        }
        if let Some(g) = groups.last_mut() {
            g.push(l.id);
        }
        last = Some((i, l.label));
    }
    for g in &groups {
        check_group(comp, g, C)?;
    }
    let snap = s.project.clone();
    let mut made = vec![];
    for g in &groups {
        match precompose_group(s, cid, g, str_p(p, "name"), label) {
            Ok(r) => made.push(r),
            Err(e) => {
                super::batch::fold(s, &snap, "Multi Precomp");
                return Err(e);
            }
        }
    }
    super::batch::fold(s, &snap, "Multi Precomp");
    Ok(json!({"precomps": made}))
}

/// Why the precomp layer `w` can't be expanded in `comp` exactly (None: it can).
fn decomp_problem(s: &Session, comp: &Comp, w: &Layer) -> Option<String> {
    let LayerSource::Comp { item } = &w.source else { return Some(format!("“{}” isn't a precomp layer", w.name)) };
    let Some(ItemKind::Comp(nc)) = s.project.item(*item).map(|i| &i.kind) else { return Some(format!("“{}”: its composition is missing", w.name)) };
    let n = &w.name;
    if w.switches.locked || w.parent.is_some() || w.track_matte.is_some() {
        return Some(format!("unlock and detach “{n}” (parent, track matte) first"));
    }
    if comp.layers.iter().any(|l| l.parent == Some(w.id) || l.track_matte.is_some_and(|m| m.layer == w.id)) {
        return Some(format!("another layer depends on “{n}” (parent or track matte)"));
    }
    if w.props.get("timeRemap").is_some() || (w.stretch - 100.0).abs() > 1e-9 {
        return Some(format!("“{n}”: time remapping or stretch can't be flattened"));
    }
    let count = |g: &str| w.props.sub(g).map_or(0, |g| g.children.len());
    if count("effects") > 0 || count("masks") > 0 {
        return Some(format!("“{n}”: effects or masks on the precomp can't be flattened"));
    }
    let (cw, ch) = (comp.width as f64, comp.height as f64);
    let (nw, nh) = (nc.width as f64, nc.height as f64);
    let plain = fixed(w, "transform/anchor", &[nw / 2.0, nh / 2.0, 0.0])
        && fixed(w, "transform/position", &[cw / 2.0, ch / 2.0, 0.0])
        && fixed(w, "transform/scale", &[100.0, 100.0, 100.0])
        && fixed(w, "transform/rotation", &[0.0])
        && fixed(w, "transform/opacity", &[100.0]);
    if !plain || w.switches.three_d {
        return Some(format!("“{n}”: Decomp keeps an unchanged 2D precomp transform only"));
    }
    if nc.width != comp.width || nc.height != comp.height || nc.frame_rate != comp.frame_rate || (nc.pixel_aspect - comp.pixel_aspect).abs() > 1e-9 {
        return Some(format!("“{n}”: size, pixel aspect and frame rate must match this comp"));
    }
    if !w.switches.video {
        return Some(format!("“{n}”: switch its video on first"));
    }
    if nc.layers.iter().any(|l| has_expression(&l.props) || l.props.get("timeRemap").is_some()) {
        return Some(format!("“{n}”: its layers use expressions or time remapping (keep it nested)"));
    }
    None
}

/// Decomp: each selected precomp layer replaced by copies of its composition's layers, in place
/// and in time; the composition stays in the project.
fn decompose(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.decompose";
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(C, "select a precomp layer first"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut plans: Vec<(LayerId, Vec<Layer>)> = vec![];
    for id in &ids {
        let w = comp.layer(*id).ok_or(EngineError::NoComp)?;
        if let Some(why) = decomp_problem(s, comp, w) {
            return Err(bad(C, why));
        }
        let LayerSource::Comp { item } = &w.source else { continue };
        let Some(ItemKind::Comp(nc)) = s.project.item(*item).map(|i| &i.kind) else { continue };
        plans.push((*id, nc.layers.iter().cloned().collect()));
    }
    let mut expanded = 0usize;
    let created = s.edit("Decomp", None, |proj, st| {
        let mut next = proj.next_id;
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut created = vec![];
        for (wid, inner) in plans {
            let Some(at) = c.layers.iter().position(|l| l.id == wid) else { continue };
            let Some(w) = c.layers.get(at).cloned() else { continue };
            let mut map: BTreeMap<LayerId, LayerId> = BTreeMap::new();
            let mut copies = Vec::with_capacity(inner.len());
            for mut l in inner {
                let old = l.id;
                super::edit::reid(&mut l, &mut next);
                map.insert(old, l.id);
                l.start_time += w.start_time;
                let (lo, hi) = ((l.in_point + w.start_time).max(w.in_point), (l.out_point + w.start_time).min(w.out_point));
                if hi > lo {
                    l.in_point = lo;
                    l.out_point = hi;
                } else {
                    // Outside the precomp layer's span: kept, switched off.
                    l.in_point += w.start_time;
                    l.out_point += w.start_time;
                    l.switches.video = false;
                    l.switches.audio = false;
                }
                copies.push(l);
            }
            for l in &mut copies {
                l.parent = l.parent.and_then(|p| map.get(&p).copied());
                l.track_matte = l.track_matte.and_then(|m| map.get(&m.layer).map(|layer| effectcraft_project::TrackMatte { layer: *layer, ..m }));
            }
            let _ = c.layers.remove(at);
            expanded += copies.len();
            for (k, l) in copies.into_iter().enumerate() {
                created.push(l.id);
                c.layers.insert((at + k).min(c.layers.len()), l);
            }
        }
        proj.next_id = next;
        st.selected_layers = created.clone();
        Ok(created)
    })?;
    Ok(json!({"layers": created.iter().map(|l| l.0).collect::<Vec<_>>(), "expanded": expanded}))
}

/// `comp("Name")` / `comp('Name')` in expressions renamed by `names` (old → new).
fn rename_comp_refs(text: &str, names: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (old, new) in names {
        for q in ['"', '\''] {
            let esc = |n: &str| n.replace('\\', "\\\\").replace(q, &format!("\\{q}"));
            out = out.replace(&format!("comp({q}{}{q})", esc(old)), &format!("comp({q}{}{q})", esc(new)));
        }
    }
    out
}

/// Deep Duplicate: the composition and every composition nested in it (at any depth) copied, the
/// copies using each other; literal `comp("…")` references in their expressions follow.
fn deep_duplicate(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "project.deepDuplicate";
    let root = match p.get("comp").and_then(Value::as_u64) {
        Some(id) => ItemId(id),
        None => s
            .state
            .project_selection
            .iter()
            .copied()
            .find(|i| s.project.comp(*i).is_some())
            .or_else(|| s.active_comp_id())
            .ok_or_else(|| bad(C, "select or open a composition"))?,
    };
    s.project.comp(root).ok_or_else(|| bad(C, "not a composition"))?;
    // Every comp reachable through precomp layers, root first.
    let mut graph: Vec<ItemId> = vec![];
    let mut todo = vec![(root, 0usize)];
    while let Some((id, depth)) = todo.pop() {
        if graph.contains(&id) {
            continue;
        }
        if depth > MAX_DEPTH || graph.len() > 10_000 {
            return Err(bad(C, "the nesting is too deep"));
        }
        let Some(comp) = s.project.comp(id) else { continue };
        graph.push(id);
        for l in comp.layers.iter() {
            if let LayerSource::Comp { item } = &l.source {
                todo.push((*item, depth + 1));
            }
        }
    }
    let created = s.edit("Deep Duplicate", None, |proj, st| {
        let mut items: BTreeMap<ItemId, ItemId> = BTreeMap::new();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for id in &graph {
            let it = proj.item(*id).ok_or_else(|| bad(C, "a composition vanished"))?.clone();
            let name = proj.numbered_item_name(&it.name, |_| false);
            names.insert(it.name.clone(), name.clone());
            let nid = proj.add_item(&name, it.label, it.parent, it.kind.clone());
            items.insert(*id, nid);
        }
        for nid in items.values() {
            let mut next = proj.next_id;
            let Some(ItemKind::Comp(comp)) = proj.item_mut(*nid).map(|i| &mut i.kind) else { continue };
            let comp = std::sync::Arc::make_mut(comp);
            let mut map: BTreeMap<LayerId, LayerId> = BTreeMap::new();
            for l in comp.layers.iter_mut() {
                let old = l.id;
                super::edit::reid(l, &mut next);
                map.insert(old, l.id);
                if let LayerSource::Comp { item } = &mut l.source
                    && let Some(n) = items.get(item)
                {
                    *item = *n;
                }
                l.props.walk_mut(&mut |pr| {
                    if let Some(e) = &mut pr.expr {
                        let renamed = rename_comp_refs(&e.text, &names);
                        if renamed != e.text {
                            *e = Expression { text: renamed, enabled: e.enabled };
                        }
                    }
                });
            }
            for l in comp.layers.iter_mut() {
                l.parent = l.parent.and_then(|p| map.get(&p).copied());
                if let Some(m) = &mut l.track_matte {
                    m.layer = map.get(&m.layer).copied().unwrap_or(m.layer);
                }
            }
            proj.next_id = next;
        }
        let made: Vec<ItemId> = graph.iter().filter_map(|i| items.get(i).copied()).collect();
        st.project_selection = made.first().copied().into_iter().collect();
        Ok(made)
    })?;
    Ok(json!({"comp": created.first().map(|i| i.0), "items": created.iter().map(|i| i.0).collect::<Vec<_>>()}))
}

/// A mask path's bounds (vertices and tangent handles) in layer space.
fn path_bounds(path: &effectcraft_keyframe::ShapePath) -> Option<[f64; 4]> {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for (i, v) in path.vertices.iter().enumerate() {
        let tin = path.in_tangents.get(i).copied().unwrap_or_default();
        let tout = path.out_tangents.get(i).copied().unwrap_or_default();
        for [x, y] in [*v, [v[0] + tin[0], v[1] + tin[1]], [v[0] + tout[0], v[1] + tout[1]]] {
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
    }
    b.iter().all(|v| v.is_finite()).then_some(b)
}

/// Move the layer's anchor to `center` (layer space) without moving the artwork: unanimated 2D
/// transforms only. Whether it moved.
fn pivot(l: &mut Layer, center: [f64; 2]) -> bool {
    if l.switches.three_d {
        return false;
    }
    let Some(tr) = l.props.sub_mut("transform") else { return false };
    let still = |m: &str| tr.get(m).is_some_and(|p| !p.is_animated() && !p.has_expression());
    if !(still("anchor") && still("position") && still("scale") && still("rotation")) {
        return false;
    }
    let (Some(KV::Vec3(a)), Some(KV::Vec3(pos)), Some(KV::Vec3(sc)), Some(KV::Scalar(rot))) = (
        tr.get("anchor").map(|p| p.value.clone()),
        tr.get("position").map(|p| p.value.clone()),
        tr.get("scale").map(|p| p.value.clone()),
        tr.get("rotation").map(|p| p.value.clone()),
    ) else {
        return false;
    };
    let r = rot.to_radians();
    let (dx, dy) = ((center[0] - a[0]) * sc[0] / 100.0, (center[1] - a[1]) * sc[1] / 100.0);
    let moved = [pos[0] + dx * r.cos() - dy * r.sin(), pos[1] + dx * r.sin() + dy * r.cos(), pos[2]];
    if let Some(p) = tr.get_mut("anchor") {
        p.value = KV::Vec3([center[0], center[1], a[2]]);
    }
    if let Some(p) = tr.get_mut("position") {
        p.value = KV::Vec3(moved);
    }
    true
}

/// Split Masks: one copy of each selected layer per mask, keeping only that mask, named after it
/// and pivoting on its center; the original is switched off, never deleted.
fn split_masks(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.splitMasks";
    let (cid, ids) = layers_p(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let t = s.time_of(cid);
    let sources: Vec<LayerId> =
        ids.iter().copied().filter(|id| comp.layer(*id).and_then(|l| l.props.sub("masks")).is_some_and(|m| !m.children.is_empty())).collect();
    if sources.is_empty() {
        return Err(bad(C, if ids.is_empty() { "select a layer with the masks you drew" } else { "the selected layers have no masks: draw masks first" }));
    }
    let mut centered = 0usize;
    let created = s.edit("Split Masks", None, |proj, st| {
        let mut next = proj.next_id;
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut created = vec![];
        for sid in &sources {
            let Some(at) = c.layers.iter().position(|l| l.id == *sid) else { continue };
            let Some(src) = c.layers.get(at).cloned() else { continue };
            let count = src.props.sub("masks").map_or(0, |m| m.children.len());
            let lt = src.layer_time(t);
            for k in 0..count {
                let mut l = src.clone();
                super::edit::reid(&mut l, &mut next);
                let Some(masks) = l.props.sub_mut("masks") else { continue };
                let Some(keep) = masks.children.get(k).cloned() else { continue };
                masks.children = vec![keep];
                let mut label = String::new();
                let mut bounds = None;
                if let Some(g) = masks.children.first_mut().and_then(|n| n.as_group_mut()) {
                    label = g.name.clone();
                    if let GroupKind::Mask { mode, .. } = &mut g.kind
                        && matches!(mode, MaskMode::None | MaskMode::Subtract)
                    {
                        *mode = MaskMode::Add;
                    }
                    if let Some(KV::Path(path)) = g.get("path").map(|p| p.value_at(lt)) {
                        bounds = path_bounds(&path);
                    }
                }
                l.name = format!("{} · {label}", src.name).chars().take(120).collect();
                l.switches.video = true;
                if let Some(b) = bounds
                    && pivot(&mut l, [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0])
                {
                    centered += 1;
                }
                created.push(l.id);
                // Mask 1's copy on top, the original below the copies.
                c.layers.insert((at + k).min(c.layers.len()), l);
            }
            if let Some(o) = c.layers.iter_mut_where(|l| l.id == *sid).next() {
                o.switches.video = false;
            }
        }
        proj.next_id = next;
        st.selected_layers = created.clone();
        Ok(created)
    })?;
    Ok(json!({"layers": created.iter().map(|l| l.0).collect::<Vec<_>>(), "centered": centered}))
}

const GRADIENT_TAG: &str = "// Motion Library: Gradient Lock v1\n";

/// Gradient Lock in one Contents group (and the groups inside it): each gradient fill or stroke
/// next to exactly one rectangle, ellipse or polystar follows that path's Position.
fn lock_gradients(contents: &mut PropGroup, layer: &str, t: effectcraft_time::Tick, depth: usize, out: &mut (usize, usize, Vec<String>)) {
    if depth > MAX_DEPTH {
        return;
    }
    for n in contents.children.iter_mut() {
        if let Some(g) = n.as_group_mut()
            && g.match_id == "group"
            && let Some(inner) = g.sub_mut("contents")
        {
            lock_gradients(inner, layer, t, depth + 1, out);
        }
    }
    let paths: Vec<(String, Option<KV>)> = contents
        .groups()
        .filter(|g| matches!(g.match_id.as_str(), "rect" | "ellipse" | "star" | "path"))
        .map(|g| (g.name.clone(), (g.match_id != "path").then(|| g.get("position").map(|p| p.value_at(t))).flatten()))
        .collect();
    let same_name = |name: &str| contents.children.iter().filter(|c| c.name() == name).count();
    let target = match paths.as_slice() {
        [(name, Some(KV::Vec2(origin)))] if same_name(name) == 1 => Some((name.clone(), *origin)),
        _ => None,
    };
    for n in contents.children.iter_mut() {
        let Some(g) = n.as_group_mut().filter(|g| matches!(g.match_id.as_str(), "gfill" | "gstroke")) else { continue };
        let label = format!("{layer} / {}", g.name);
        let exprs: Vec<Option<String>> =
            ["start", "end"].iter().map(|m| g.get(m).and_then(|p| p.expr.as_ref().filter(|e| !e.text.trim().is_empty()).map(|e| e.text.clone()))).collect();
        if exprs.iter().all(|e| e.as_deref().is_some_and(|t| t.starts_with(GRADIENT_TAG))) {
            out.1 += 1;
            continue;
        }
        if exprs.iter().any(Option::is_some) {
            out.2.push(format!("{label}: existing expression kept"));
            continue;
        }
        let Some((name, origin)) = &target else {
            out.2.push(format!("{label}: needs one rectangle, ellipse or polystar in the same group"));
            continue;
        };
        let quoted = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
        let text = format!("{GRADIENT_TAG}var p=thisProperty.propertyGroup(2)({quoted})(\"Position\");\nvalue + p - [{}, {}];", origin[0], origin[1]);
        for m in ["start", "end"] {
            if let Some(p) = g.get_mut(m) {
                p.expr = Some(Expression { text: text.clone(), enabled: true });
            }
        }
        out.0 += 1;
    }
}

/// Gradient Lock: shape gradients keep their place on their rectangle, ellipse or polystar when
/// it moves (an expression on Start and End Point).
fn gradient_lock(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shape.gradientLock";
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(C, "select a shape layer with a gradient first"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let t = s.time_of(cid);
    let mut plan = (0usize, 0usize, vec![]);
    // Work it out on copies first: nothing changes when nothing locks.
    let mut done: Vec<Layer> = vec![];
    for id in &ids {
        let Some(l) = comp.layer(*id) else { continue };
        let mut l = l.clone();
        if l.switches.locked {
            plan.2.push(format!("{}: layer is locked", l.name));
            continue;
        }
        let lt = l.layer_time(t);
        let name = l.name.clone();
        match l.props.sub_mut("contents") {
            Some(contents) => lock_gradients(contents, &name, lt, 0, &mut plan),
            None => plan.2.push(format!("{name}: not a shape layer")),
        }
        done.push(l);
    }
    let (locked, already, skipped) = plan;
    if locked == 0 && already == 0 {
        let why = if skipped.is_empty() { "select a shape layer with a gradient fill or stroke".to_string() } else { skipped.join("; ") };
        return Err(bad(C, format!("no gradients locked: {why}")));
    }
    if locked > 0 {
        s.edit("Gradient Lock", None, |proj, _| {
            let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
            for l in done {
                if let Some(slot) = c.layers.iter_mut_where(|x| x.id == l.id).next() {
                    slot.props = l.props;
                }
            }
            Ok(())
        })?;
    }
    Ok(json!({"locked": locked, "already": already, "skipped": skipped}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "layer.smartPrecompose",
            "Smart Precomp",
            [],
            None,
            "{layers? (default: selected; consecutive, parents and mattes inside), name?, label?} — one precomp trimmed to the layers' span → {comp, layer, name}",
            has_layers,
            smart_precompose
        ),
        cmd!(
            "layer.multiPrecompose",
            "Multi Precomp",
            [],
            None,
            "{layers?, count? (layers per precomp, default 1), byLabel? (split where the label changes), name?, label?} — runs of consecutive layers into their own precomps, one undo step → {precomps: [{comp, layer, name}]}",
            has_layers,
            multi_precompose
        ),
        cmd!(
            "layer.decompose",
            "Decomp",
            [],
            None,
            "{layers? (precomp layers, default: selected)} — replace each precomp layer by copies of its layers in place and time (unchanged 2D transform, no effects/masks/time remap, same size and rate); the comp stays in the project → {layers, expanded}",
            has_layers,
            decompose
        ),
        cmd!(
            "project.deepDuplicate",
            "Deep Duplicate",
            [],
            None,
            "{comp? (default: selected or open comp)} — copy the comp and every comp nested in it, the copies using each other (literal comp(\"…\") expression references follow) → {comp, items}",
            has_comp,
            deep_duplicate
        ),
        cmd!(
            "layer.splitMasks",
            "Split Masks",
            [],
            None,
            "{layers? (default: selected)} — one copy per mask, keeping only it, named after it, anchored on its center (unanimated 2D); the original is switched off → {layers, centered}",
            has_layers,
            split_masks
        ),
        cmd!(
            "shape.gradientLock",
            "Gradient Lock",
            [],
            None,
            "{layers? (shape layers, default: selected)} — gradient fills/strokes follow the one rectangle, ellipse or polystar in their group (expression on Start and End Point) → {locked, already, skipped}",
            has_layers,
            gradient_lock
        ),
    ]
}
