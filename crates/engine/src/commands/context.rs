//! Context for agents in one call (editing harness H1): `context.packet` returns what is selected
//! with the keyframes, eases and related layers an edit needs, plus a short `text` an agent hook
//! can pass along with the user's message; `layer.related` is the relationship graph of one layer.
//! Read-only (not journaled).

use std::collections::BTreeMap;

use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Property, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, comp_id, layer_p};
use crate::{EngineError, Result, Session};

/// Keys listed per property; a property with more keys lists the ones nearest the selected keys.
const MAX_KEYS: usize = 12;
/// Undo labels shown as recent history.
const HISTORY: usize = 5;
/// Parent links followed before giving up (a cycle would be a project bug, but never loop).
const MAX_PARENT_DEPTH: usize = 64;

fn layer_name(c: &Comp, id: LayerId) -> String {
    c.layer(id).map(|l| l.name.clone()).unwrap_or_else(|| format!("layer {}", id.0))
}

fn layer_ref(c: &Comp, id: LayerId) -> Value {
    json!({"id": id.0, "index": c.index_of(id), "name": layer_name(c, id)})
}

fn frame_of(c: &Comp, t: Tick) -> i64 {
    (t.seconds() * c.frame_rate.as_f64()).round() as i64
}

/// Layers an expression names with `layer("Name")` or `layer(n)` (any comp; dynamic lookups are
/// not resolved). Returns names and 1-based indices as written.
pub(crate) fn expression_layer_refs(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find("layer(") {
        let after = rest.get(at + "layer(".len()..).unwrap_or("");
        // Skip identifiers that only end in `layer(` (e.g. `sourceLayer(`): the char before must not be alphanumeric.
        let prev_ok = rest.get(..at).and_then(|s| s.chars().last()).is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        let arg = after.trim_start();
        if prev_ok {
            let quote = arg.chars().next().filter(|q| *q == '"' || *q == '\'');
            if let Some(q) = quote {
                let body = arg.get(1..).unwrap_or("");
                if let Some(end) = body.find(q) {
                    out.push(body.get(..end).unwrap_or("").to_string());
                }
            } else {
                let digits: String = arg.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() {
                    out.push(format!("#{digits}"));
                }
            }
        }
        rest = after;
    }
    out.dedup();
    out
}

/// True when the expression reference `r` (from [`expression_layer_refs`]) names layer `l` of `c`.
fn names_layer(c: &Comp, r: &str, l: &Layer) -> bool {
    match r.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => c.index_of(l.id) == Some(n),
        None => r == l.name,
    }
}

fn expressions_of(l: &Layer) -> Vec<String> {
    let mut v = Vec::new();
    l.props.walk("", &mut |_, p: &Property| {
        if let Some(e) = p.expr.as_ref().filter(|e| e.enabled) {
            v.push(e.text.clone());
        }
    });
    v
}

/// The relationship graph of layer `lid` in comp `cid`.
fn related(s: &Session, cid: ItemId, lid: LayerId) -> Result<Value> {
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = c.layer(lid).ok_or(EngineError::NoComp)?;
    let mut parents = Vec::new();
    let mut next = l.parent;
    while let Some(p) = next {
        if parents.len() >= MAX_PARENT_DEPTH || p == lid {
            break;
        }
        parents.push(layer_ref(c, p));
        next = c.layer(p).and_then(|pl| pl.parent);
    }
    let children: Vec<Value> = c.layers.iter().filter(|o| o.parent == Some(lid)).map(|o| layer_ref(c, o.id)).collect();
    let matte = l.track_matte.map(|m| json!({"layer": layer_ref(c, m.layer), "kind": m.kind.label()}));
    let matte_for: Vec<Value> = c
        .layers
        .iter()
        .filter_map(|o| o.track_matte.filter(|m| m.layer == lid).map(|m| json!({"layer": layer_ref(c, o.id), "kind": m.kind.label()})))
        .collect();
    let mut refers_to = Vec::new();
    for e in expressions_of(l) {
        for r in expression_layer_refs(&e) {
            if let Some(o) = c.layers.iter().find(|o| names_layer(c, &r, o)) {
                let v = layer_ref(c, o.id);
                if !refers_to.contains(&v) {
                    refers_to.push(v);
                }
            }
        }
    }
    let referred_by: Vec<Value> = c
        .layers
        .iter()
        .filter(|o| o.id != lid && expressions_of(o).iter().any(|e| expression_layer_refs(e).iter().any(|r| names_layer(c, r, l))))
        .map(|o| layer_ref(c, o.id))
        .collect();
    let item_name = |id: ItemId| s.project.item(id).map(|i| i.name.clone()).unwrap_or_default();
    let precomp = match &l.source {
        LayerSource::Comp { item } => {
            let n = s.project.comp(*item).map(|pc| pc.layers.len()).unwrap_or(0);
            Some(json!({"comp": item.0, "name": item_name(*item), "layers": n}))
        }
        _ => None,
    };
    let used_in: Vec<Value> =
        s.project.item_users(cid).into_iter().map(|(u, n)| json!({"comp": u.0, "name": item_name(u), "layers": n})).collect();
    Ok(json!({
        "layer": layer_ref(c, lid), "comp": cid.0,
        "parents": parents, "children": children,
        "trackMatte": matte, "matteFor": matte_for,
        "expressionRefersTo": refers_to, "expressionReferredBy": referred_by,
        "precomp": precomp, "compUsedIn": used_in,
    }))
}

fn layer_related(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid) = layer_p(s, p, "layer.related")?;
    related(s, cid, lid)
}

/// One keyframe as the packet lists it: comp time, frame, value, interpolation and eases.
fn key_json(c: &Comp, l: &Layer, k: &effectcraft_keyframe::Keyframe, selected: bool) -> Value {
    let t = l.comp_time(k.time);
    let mut o = json!({
        "time": t.seconds(), "frame": frame_of(c, t), "layerTime": k.time.seconds(), "value": k.value.to_json(),
        "in": k.in_interp.label(), "out": k.out_interp.label(),
        "inEase": serde_json::to_value(&k.in_ease).unwrap_or_default(), "outEase": serde_json::to_value(&k.out_ease).unwrap_or_default(),
    });
    if selected {
        o["selected"] = json!(true);
    }
    o
}

/// The keys of `prop` to list: all of them, or a window of [`MAX_KEYS`] around the selected ones.
fn key_window(prop: &Property, selected: &[Tick]) -> std::ops::Range<usize> {
    let n = prop.keys.len();
    if n <= MAX_KEYS {
        return 0..n;
    }
    let first = prop.keys.iter().position(|k| selected.contains(&k.time)).unwrap_or(0);
    let start = first.saturating_sub(2).min(n.saturating_sub(MAX_KEYS));
    start..(start + MAX_KEYS).min(n)
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Number(n) => n.as_f64().map(|f| format!("{}", (f * 1000.0).round() / 1000.0)).unwrap_or_else(|| n.to_string()),
        Value::Array(a) => format!("[{}]", a.iter().map(fmt_value).collect::<Vec<_>>().join(", ")),
        other => {
            let s = other.to_string();
            if s.len() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
        }
    }
}

fn ease_text(k: &Value) -> String {
    let side = |interp: &Value, ease: &Value| -> String {
        let i = interp.as_str().unwrap_or("");
        let infl = ease.get(0).and_then(|e| e.get("influence")).and_then(Value::as_f64);
        match (i, infl) {
            ("Bezier", Some(f)) => format!("bezier {}%", (f * 100.0).round()),
            (i, _) => i.to_ascii_lowercase(),
        }
    };
    let a = side(&k["in"], &k["inEase"]);
    let b = side(&k["out"], &k["outEase"]);
    if a == b { a } else { format!("in {a} / out {b}") }
}

fn packet(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    // Claims as `editor.state` reports them (expired ones pruned); needs `&mut`, so before borrowing the comp.
    let mut extras = serde_json::Map::new();
    crate::agents::state_extras(s, &mut extras);
    let claims = extras.remove("claims").unwrap_or_else(|| json!([]));
    let t = s.time_of(cid);
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let comp_name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    let mut text = vec![format!(
        "Comp \"{comp_name}\" (id {}) {}x{} @ {}fps, {:.3}s · playhead {:.3}s (frame {})",
        cid.0,
        c.width,
        c.height,
        (c.frame_rate.as_f64() * 1000.0).round() / 1000.0,
        c.duration.seconds(),
        t.seconds(),
        frame_of(c, t)
    )];

    // Selected keys grouped by property, and selected properties without selected keys.
    let mut props: BTreeMap<(LayerId, Uid), Vec<Tick>> = BTreeMap::new();
    for k in &s.state.selected_keys {
        props.entry((k.layer, k.prop)).or_default().push(k.time);
    }
    for (l, u) in &s.state.selected_props {
        props.entry((*l, *u)).or_default();
    }
    let mut layer_ids: Vec<LayerId> = s.state.selected_layers.iter().copied().filter(|l| c.layer(*l).is_some()).collect();
    for (l, _) in props.keys() {
        if !layer_ids.contains(l) && c.layer(*l).is_some() {
            layer_ids.push(*l);
        }
    }

    let mut layers = Vec::new();
    for lid in &layer_ids {
        let Some(l) = c.layer(*lid) else { continue };
        let rel = related(s, cid, *lid)?;
        let mut keyed = Vec::new();
        l.props.walk("", &mut |_, pr: &Property| {
            if !pr.keys.is_empty() || pr.expr.as_ref().is_some_and(|e| e.enabled) {
                let path = l.props.match_path_of(pr.uid).unwrap_or_default();
                keyed.push(json!({"path": path, "keys": pr.keys.len(), "expression": pr.expr.as_ref().is_some_and(|e| e.enabled)}));
            }
        });
        let mut sel_props = Vec::new();
        for ((pl, uid), times) in props.range((*lid, u64::MIN)..=(*lid, u64::MAX)) {
            if pl != lid {
                continue;
            }
            let Some(pr) = l.props.find(*uid) else { continue };
            let path = l.props.match_path_of(*uid).unwrap_or_default();
            let win = key_window(pr, times);
            let keys: Vec<Value> =
                pr.keys.get(win.clone()).unwrap_or(&[]).iter().map(|k| key_json(c, l, k, times.contains(&k.time))).collect();
            let mut o = json!({"path": path, "name": pr.name, "keyCount": pr.keys.len(), "keys": keys,
                "value": pr.value_at(l.layer_time(t)).to_json()});
            if let Some(e) = &pr.expr {
                o["expression"] = json!({"text": e.text, "enabled": e.enabled});
            }
            if win.len() < pr.keys.len() {
                o["keysShown"] = json!([win.start, win.end]);
            }
            sel_props.push(o);
        }
        let idx = c.index_of(*lid).unwrap_or(0);
        text.push(format!(
            "Selected #{idx} {} ({}) in {:.3}s–{:.3}s{}",
            l.name,
            l.source.type_name(),
            l.in_point.seconds(),
            l.out_point.seconds(),
            l.parent.map(|p| format!(", parent {}", layer_name(c, p))).unwrap_or_default()
        ));
        for sp in &sel_props {
            let keys = sp["keys"].as_array().cloned().unwrap_or_default();
            text.push(format!("  {}  {} keys{}", sp["path"].as_str().unwrap_or(""), sp["keyCount"], if keys.is_empty() { format!(", value {}", fmt_value(&sp["value"])) } else { String::new() }));
            for k in &keys {
                text.push(format!(
                    "    {}{:.3}s (f{})  {}  {}",
                    if k.get("selected").is_some() { "▸ " } else { "  " },
                    k["time"].as_f64().unwrap_or(0.0),
                    k["frame"],
                    fmt_value(&k["value"]),
                    ease_text(k)
                ));
            }
        }
        if sel_props.is_empty() && !keyed.is_empty() {
            let list: Vec<String> = keyed.iter().map(|k| format!("{} ({} keys)", k["path"].as_str().unwrap_or(""), k["keys"])).collect();
            text.push(format!("  animated: {}", list.join(", ")));
        }
        let names = |v: &Value| -> Vec<String> {
            v.as_array().map(|a| a.iter().map(|x| format!("#{} {}", x["index"], x["name"].as_str().unwrap_or(""))).collect()).unwrap_or_default()
        };
        let mut links = Vec::new();
        if let Some(m) = rel["trackMatte"].as_object() {
            links.push(format!("matte: #{} {} ({})", m["layer"]["index"], m["layer"]["name"].as_str().unwrap_or(""), m["kind"].as_str().unwrap_or("")));
        }
        for (key, label) in [("matteFor", "is matte for"), ("children", "children"), ("expressionRefersTo", "expressions use"), ("expressionReferredBy", "used by expressions in")] {
            let v = if key == "matteFor" {
                Value::Array(rel[key].as_array().map(|a| a.iter().map(|x| x["layer"].clone()).collect()).unwrap_or_default())
            } else {
                rel[key].clone()
            };
            let n = names(&v);
            if !n.is_empty() {
                links.push(format!("{label}: {}", n.join(", ")));
            }
        }
        if let Some(pc) = rel["precomp"].as_object() {
            links.push(format!("precomp \"{}\" ({} layers)", pc["name"].as_str().unwrap_or(""), pc["layers"]));
        }
        if !links.is_empty() {
            text.push(format!("  related: {}", links.join("; ")));
        }
        let mut o = layer_ref(c, *lid);
        o["type"] = json!(l.source.type_name());
        o["in"] = json!(l.in_point.seconds());
        o["out"] = json!(l.out_point.seconds());
        o["animated"] = json!(keyed);
        o["selectedProperties"] = json!(sel_props);
        o["related"] = rel;
        layers.push(o);
    }
    if layers.is_empty() {
        text.push("Nothing selected.".into());
    }

    let history: Vec<String> = s.history.undo.iter().rev().take(HISTORY).map(|(label, _)| label.clone()).collect();
    if !history.is_empty() {
        text.push(format!("Recent edits (newest first): {}", history.join(" · ")));
    }
    let used_in: Vec<String> = s.project.item_users(cid).into_iter().filter_map(|(u, _)| s.project.item(u).map(|i| i.name.clone())).collect();
    if !used_in.is_empty() {
        text.push(format!("This comp is used in: {}", used_in.join(", ")));
    }
    if claims.as_array().is_some_and(|a| !a.is_empty()) {
        text.push(format!("Claims by other agents: {claims}"));
    }
    if s.is_dirty() {
        text.push("Project has unsaved changes.".into());
    }
    Ok(json!({
        "comp": {"id": cid.0, "name": comp_name, "width": c.width, "height": c.height, "frameRate": c.frame_rate.as_f64(),
            "duration": c.duration.seconds(), "workArea": [c.work_area.0.seconds(), c.work_area.1.seconds()],
            "usedIn": used_in},
        "time": t.seconds(), "frame": frame_of(c, t), "timeBase": "comp",
        "project": {"path": s.path, "dirty": s.is_dirty()},
        "revision": s.revision,
        "layers": layers,
        "history": history,
        "claims": claims,
        "text": text.join("\n"),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "context.packet",
            "Context Packet",
            "{comp?} → what is selected in one call: comp, playhead (comp s + frame), selected layers with their animated properties, selected keys (comp time, frame, value, interpolation, eases; neighbours included), related layers (parent, children, matte, expression links, precomp, comps using this one), recent undo labels, claims, dirty; `text` is a short summary to pass to an agent",
            packet
        ),
        query!(
            "layer.related",
            "Related Layers",
            "{layer?, comp?} → parent chain, children, track matte and layers using it as matte, layers its expressions name and layers whose expressions name it, precomp source, comps using this comp",
            layer_related
        ),
    ]
}
