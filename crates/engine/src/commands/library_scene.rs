//! `library.scene`: a Motion Library scene (Devin Jatho's scenes and any other comp card) by
//! name, placed at a time or on a word of the comp's speech map, with its texts and footage
//! swapped for this edit's in the same step. Agents rebuilt these scenes by hand because
//! `library.apply` needs a card id and leaves the swaps to them.

use std::collections::BTreeMap;

use effectcraft_project::{ItemId, ItemKind, LayerId, LayerSource, Project};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, f_p, str_p};
use crate::{EngineError, Result, Session, cmd};

const C: &str = "library.scene";
/// How many precomps deep the slots of a scene are looked for.
const MAX_DEPTH: usize = 6;
/// Most comps of a scene walked for slots (a scene that nests its comps in a loop stops here).
const MAX_COMPS: usize = 200;

/// Lower-case letters and digits only: "YT Zoom-Out" and "yt zoom out" are the same name.
fn loose(n: &str) -> String {
    n.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The comp card `scene` names (an id, else a name), with the other close names. Cards in a
/// folder named like `Scenes` win over others with the same score.
fn find_card(s: &mut Session, scene: &str, folder: Option<&str>) -> Result<(Value, Vec<String>)> {
    let mut q = json!({"query": scene, "kind": "comp", "limit": 50});
    if let Some(f) = folder {
        q["category"] = json!(f);
    }
    let hits = s.execute("library.search", q)?["entries"].as_array().cloned().unwrap_or_default();
    // An id lists nothing by name: look it up among everything.
    let by_id = if hits.iter().any(|h| h["id"] == scene) {
        None
    } else {
        let all = s.execute("library.search", json!({"query": "", "kind": "comp", "limit": 200}))?;
        all["entries"].as_array().and_then(|a| a.iter().find(|h| h["id"] == scene).cloned())
    };
    let name = |h: &Value| h["name"].as_str().unwrap_or("").to_string();
    let scenes = |h: &&Value| h["category"].as_str().is_some_and(|c| c.to_lowercase().contains("scene"));
    let want = loose(scene);
    let pick = by_id
        .or_else(|| hits.iter().find(|h| h["id"] == scene).cloned())
        .or_else(|| hits.iter().find(|h| loose(&name(h)) == want).cloned())
        .or_else(|| hits.iter().find(scenes).cloned())
        .or_else(|| hits.first().cloned());
    let Some(card) = pick else {
        let all = s.execute("library.search", json!({"query": "", "kind": "comp", "category": folder.unwrap_or("scene"), "limit": 40}))?;
        let names: Vec<String> = all["entries"].as_array().map(|a| a.iter().map(name).collect()).unwrap_or_default();
        return Err(bad(
            C,
            format!(
                "no Motion Library scene matches “{scene}”{}; scenes: {}",
                folder.map(|f| format!(" in {f}")).unwrap_or_default(),
                if names.is_empty() { "none found (library.search lists every card)".into() } else { names.join(", ") }
            ),
        ));
    };
    let others = hits.iter().filter(|h| h["id"] != card["id"]).take(5).map(name).collect();
    Ok((card, others))
}

/// The comp-space start of the `occurrence`-th (1-based) `word` of the comp's speech map.
fn word_time(s: &Session, cid: ItemId, word: &str, occurrence: usize) -> Result<f64> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let map = comp.speech.as_deref().filter(|m| !m.words.is_empty()).ok_or_else(|| {
        bad(C, "the comp has no speech map for `word` (make one with scripts/fork/speech-map, store it with speech.import), or give `at` in seconds")
    })?;
    let want = loose(word);
    let found: Vec<f64> = map.words.iter().filter(|w| loose(&w.word) == want).map(|w| w.start.seconds()).collect();
    found.get(occurrence.saturating_sub(1)).copied().ok_or_else(|| bad(C, format!("“{word}” is in the speech map {} time(s), not {occurrence}", found.len())))
}

/// A layer of the scene something can be swapped into.
struct Slot {
    comp: ItemId,
    layer: LayerId,
    name: String,
    kind: &'static str,
    /// The text of a text layer, the source's name of a footage or precomp layer.
    now: String,
}

/// The text, footage and precomp layers of `root` and the comps nested in it.
fn slots(project: &Project, root: ItemId) -> Vec<Slot> {
    let mut out = vec![];
    let mut queue = vec![(root, 0usize)];
    let mut seen = vec![];
    while let Some((cid, depth)) = queue.pop() {
        if seen.contains(&cid) || seen.len() >= MAX_COMPS {
            continue;
        }
        seen.push(cid);
        let Some(comp) = project.comp(cid) else { continue };
        for l in &comp.layers {
            let (kind, now) = match &l.source {
                LayerSource::Text => (
                    "text",
                    match l.props.prop("text/sourceText").map(|p| &p.value) {
                        Some(effectcraft_keyframe::Value::Text(d)) => d.text.clone(),
                        _ => String::new(),
                    },
                ),
                LayerSource::Footage { item } => ("footage", project.item(*item).map(|i| i.name.clone()).unwrap_or_default()),
                LayerSource::Comp { item } => {
                    if depth < MAX_DEPTH {
                        queue.push((*item, depth + 1));
                    }
                    ("comp", project.item(*item).map(|i| i.name.clone()).unwrap_or_default())
                }
                _ => continue,
            };
            out.push(Slot { comp: cid, layer: l.id, name: l.name.clone(), kind, now });
        }
    }
    out
}

/// The project item a footage swap names: an item id, an item name, else a file to import.
fn swap_item(s: &mut Session, v: &Value, key: &str) -> Result<ItemId> {
    if let Some(n) = v.as_u64() {
        return s.project.item(ItemId(n)).map(|i| i.id).ok_or_else(|| bad(C, format!("swap “{key}”: no item {n}")));
    }
    let v = v.as_str().ok_or_else(|| bad(C, format!("swap “{key}”: a file path, an item name or id, not {v}")))?;
    if let Some(i) = s.project.find_by_name(v).filter(|i| !matches!(i.kind, ItemKind::Folder)) {
        return Ok(i.id);
    }
    if !s.services.exists(v) {
        return Err(bad(C, format!("swap “{key}”: “{v}” is neither an item of the project nor a file on this machine")));
    }
    let r = s.execute("file.import", json!({"paths": [v]}))?;
    r["items"].as_array().and_then(|a| a.first()).and_then(Value::as_u64).map(ItemId).ok_or_else(|| {
        let why = r["errors"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("nothing was imported").to_string();
        bad(C, format!("swap “{key}”: {why}"))
    })
}

fn scene(s: &mut Session, p: &Value) -> Result<Value> {
    let folder = str_p(p, "folder").map(str::to_lowercase);
    let Some(name) = str_p(p, "scene").map(str::trim).filter(|n| !n.is_empty()) else {
        // No scene: the list to pick from.
        let r = s.execute("library.search", json!({"query": "", "kind": "comp", "category": folder.as_deref().unwrap_or("scene"), "limit": 200}))?;
        let list: Vec<Value> = r["entries"]
            .as_array()
            .map(|a| a.iter().map(|e| json!({"id": e["id"], "name": e["name"], "category": e["category"]})).collect())
            .unwrap_or_default();
        return Ok(json!({"scenes": list}));
    };
    let (card, others) = find_card(s, name, folder.as_deref())?;
    let id = card["id"].as_str().unwrap_or("").to_string();
    let cid = match p.get("comp").filter(|v| !v.is_null()) {
        Some(_) => super::comp_id(s, p)?,
        None => s.active_comp_id().ok_or_else(|| bad(C, "no comp open: give `comp`"))?,
    };
    let at = match (f_p(p, "at"), str_p(p, "word")) {
        (Some(t), _) if t.is_finite() => Some(t),
        (Some(t), _) => return Err(bad(C, format!("`at` must be a finite number of comp seconds, not {t}"))),
        (None, Some(w)) => {
            let n = p.get("occurrence").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            Some(word_time(s, cid, w, n)?)
        }
        (None, None) => None,
    };
    let swaps: BTreeMap<String, Value> = match p.get("swap").filter(|v| !v.is_null()) {
        None => BTreeMap::new(),
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Some(v) => return Err(bad(C, format!("`swap` is {{\"layer name\": text or file}}, not {v}"))),
    };

    let snap = s.project.clone();
    let history = s.history.clone();
    let result = (|| -> Result<Value> {
        let mut q = json!({"id": id, "comp": cid.0});
        if let Some(t) = at {
            q["time"] = json!(t);
        }
        let applied = s.execute("library.insert", q)?;
        let item = applied["item"].as_u64().map(ItemId).ok_or_else(|| bad(C, format!("“{}” added no composition", card["name"].as_str().unwrap_or(&id))))?;
        let found = slots(&s.project, item);
        let mut swapped = vec![];
        let mut unmatched = vec![];
        for (key, v) in &swaps {
            let k = loose(key);
            let hits: Vec<&Slot> = found.iter().filter(|sl| loose(&sl.name) == k || (sl.kind != "text" && loose(&sl.now) == k)).collect();
            if hits.is_empty() {
                unmatched.push(key.clone());
                continue;
            }
            for sl in hits {
                let how = if sl.kind == "text" {
                    let text = match v {
                        Value::String(t) => t.clone(),
                        other => other.to_string(),
                    };
                    s.execute("layer.setText", json!({"comp": sl.comp.0, "layer": sl.layer.0, "text": text}))?;
                    "text"
                } else {
                    let new = swap_item(s, v, key)?;
                    s.execute("layer.replaceSource", json!({"comp": sl.comp.0, "layer": sl.layer.0, "item": new.0}))?;
                    "source"
                };
                swapped.push(json!({"slot": key, "comp": sl.comp.0, "layer": sl.layer.0, "name": sl.name, "was": sl.now, "how": how}));
            }
        }
        if !unmatched.is_empty() && swapped.is_empty() && !swaps.is_empty() {
            return Err(bad(
                C,
                format!(
                    "no layer of “{}” is named {}; its slots: {}",
                    card["name"].as_str().unwrap_or(&id),
                    unmatched.iter().map(|u| format!("“{u}”")).collect::<Vec<_>>().join(", "),
                    found.iter().filter(|sl| sl.kind != "comp").map(|sl| format!("{} ({})", sl.name, sl.kind)).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        let list: Vec<Value> = slots(&s.project, item)
            .iter()
            .map(|sl| {
                json!({"comp": sl.comp.0, "compName": s.project.item(sl.comp).map(|i| i.name.as_str()).unwrap_or(""), "layer": sl.layer.0, "name": sl.name, "kind": sl.kind, "now": sl.now})
            })
            .collect();
        let mut notes = vec![];
        if !unmatched.is_empty() {
            notes.push(format!("no slot named {}: see `slots` for the names", unmatched.iter().map(|u| format!("“{u}”")).collect::<Vec<_>>().join(", ")));
        }
        if let Some(n) = applied["needsFootage"].as_array().filter(|a| !a.is_empty()) {
            notes.push(format!("{} footage item(s) are placeholders: swap them (swap {{\"layer name\": file}}) or relink with footage.relink", n.len()));
        }
        Ok(json!({
            "scene": {"id": id, "name": card["name"], "category": card["category"]},
            "comp": cid.0,
            "item": item.0,
            "layer": applied["layer"],
            "at": at,
            "slots": list,
            "swapped": swapped,
            "unmatched": unmatched,
            "alternatives": others,
            "needsFootage": applied.get("needsFootage").cloned().unwrap_or(Value::Null),
            "notes": notes,
        }))
    })();
    match result {
        Ok(r) => {
            super::batch::fold(s, &snap, &format!("Apply Scene {}", r["scene"]["name"].as_str().unwrap_or(&id)));
            Ok(r)
        }
        Err(e) => {
            s.project = snap;
            s.history = history;
            Err(e)
        }
    }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        journal: false,
        ..cmd!(
            "library.scene",
            "Apply Library Scene",
            [],
            None,
            "{scene? (a Motion Library comp card by name, loosely: \"yt zoom out\" finds “YT Zoom Out”; or its id; none = list the scenes), folder? (category filter, e.g. \"Devin Jatho\"; default: cards in a Scenes folder win), comp? (default the open one), at? (comp s) | word? (+ occurrence?, 1-based: the scene starts on that word of the comp's speech map); default the playhead, swap? ({\"layer name or footage name\": \"new text\" for a text layer | a file path, item name or id for a footage/precomp layer}: every layer of the scene and its precomps with that name)} → {scene {id, name, category}, comp, item (the scene's comp), layer, at, slots: [{comp, compName, layer, name, kind (text|footage|comp), now (text or source name)}], swapped: [{slot, comp, layer, name, was, how (text|source)}], unmatched, alternatives (other close names), needsFootage, notes}. Imports the scene like library.apply, then swaps; one undo step",
            always,
            scene
        )
    }]
}
