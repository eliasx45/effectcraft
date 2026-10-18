//! Library variants with situation cards ([`crate::library_variants`]): Elias's refined copies of
//! a library entry, remembered with the situation each was made for, and ranked for a new one.
//!
//! - `library.saveVariant`: the selected layers (or the selected precomp's comp) as a variant of
//!   an entry, with its card; the card's layer box, comp size and duration come from the document
//!   where the caller doesn't say.
//! - `library.variants`: an entry's variants (or every entry's counts).
//! - `library.matchVariant`: rank variants for a situation by a weighted feature distance.
//! - `library.updateCard`: change a variant's card (the "why" answers after a burst of fixes).
//! - `library.applyVariant`: bring a variant into a comp, one undo step.
//!
//! Saving writes the library folder (a project and the `variants.json` sidecar), never the open
//! project; applying is the only command that changes the document, and it is undoable.

use effectcraft_project::{ItemId, LayerId, LayerSource};
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, f_p, layers_p, str_p};
use crate::library_variants::{self as lv, SituationCard, Variant};
use crate::{EngineError, Result, Session, cmd, query};

/// The library folder to keep `parent`'s variants in, and the entry's name: the folder listing the
/// entry, else `library` (a configured folder), else the first configured folder. The entry must
/// be in a library folder or the Motion Library catalog.
fn home_of(s: &Session, c: &str, parent: &str, library: Option<&str>) -> Result<(String, String)> {
    let local = s.motion_library_local().get(parent).cloned();
    let name = match &local {
        Some(e) => e.name.clone(),
        None => s
            .motion_library
            .get()
            .and_then(|cl| cl.catalog())
            .and_then(|cat| cat.card(parent).map(|card| card.name.clone()))
            .ok_or_else(|| bad(c, format!("no library entry `{parent}` (library.search lists them)")))?,
    };
    if let Some(e) = local
        && library.is_none()
    {
        return Ok((e.folder, name));
    }
    let folder = match library {
        Some(l) => {
            let l = l.trim_end_matches('/');
            s.prefs.libraries.iter().find(|f| f.as_str() == l).cloned().ok_or_else(|| bad(c, format!("`{l}` isn't a library folder here (library.folders)")))?
        }
        None => s.prefs.libraries.first().cloned().ok_or_else(|| bad(c, "no library folder to keep variants in: add one with library.addFolder"))?,
    };
    Ok((folder, name))
}

/// What the document says about `ids` in comp `cid`: their box in the comp (the visible pixels'
/// union, else the layers' bounds), the comp's size and the layers' span.
fn doc_situation(s: &mut Session, cid: ItemId, ids: &[LayerId]) -> SituationCard {
    let mut card = SituationCard::default();
    let Some(comp) = s.project.comp(cid) else { return card };
    card.comp_size = Some([comp.width as f64, comp.height as f64]);
    let chosen = || comp.layers.iter().filter(|l| ids.contains(&l.id));
    if let (Some(a), Some(b)) = (chosen().map(|l| l.in_point).min(), chosen().map(|l| l.out_point).max()) {
        let d = (b - a).seconds();
        card.duration = (d.is_finite() && d > 0.0).then_some(d);
    }
    let ids_json: Vec<u64> = ids.iter().map(|l| l.0).collect();
    if let Ok(r) = s.execute("layer.bounds", json!({"comp": cid.0, "layers": ids_json, "pixels": true})) {
        let mut acc: Option<[f64; 4]> = None;
        for l in r["layers"].as_array().into_iter().flatten() {
            let b = if l["pixelBounds"].is_array() { &l["pixelBounds"] } else { &l["bounds"] };
            let Some(v) = b.as_array().filter(|a| a.len() == 4) else { continue };
            let [x, y, w, h] = [0, 1, 2, 3].map(|i| v.get(i).and_then(Value::as_f64).unwrap_or(f64::NAN));
            if ![x, y, w, h].iter().all(|n| n.is_finite()) || w <= 0.0 || h <= 0.0 {
                continue;
            }
            acc = Some(match acc {
                None => [x, y, w, h],
                Some([ax, ay, aw, ah]) => {
                    let (x0, y0) = (ax.min(x), ay.min(y));
                    let (x1, y1) = ((ax + aw).max(x + w), (ay + ah).max(y + h));
                    [x0, y0, x1 - x0, y1 - y0]
                }
            });
        }
        card.layer = acc;
    }
    card.fill_derived();
    card
}

fn card_p(c: &str, p: &Value, key: &str) -> Result<SituationCard> {
    SituationCard::from_input(p.get(key).unwrap_or(&Value::Null)).map_err(|e| bad(c, e))
}

fn save_variant(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.saveVariant";
    let parent = str_p(p, "parent").map(str::trim).filter(|x| !x.is_empty()).ok_or_else(|| bad(C, "missing `parent` (the library entry's id)"))?.to_string();
    // A junk card is refused before anything is written.
    let mut card = card_p(C, p, "card")?;
    let (folder, parent_name) = home_of(s, C, &parent, str_p(p, "library"))?;
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(C, "select the refined layers (or the precomp) to save as a variant"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    // One precomp layer (an applied library comp, refined): its comp is the variant.
    let nested = match (ids.as_slice(), b_p(p, "precomp")) {
        ([one], None | Some(true)) => comp.layer(*one).and_then(|l| match l.source {
            LayerSource::Comp { item } => Some(item),
            _ => None,
        }),
        _ => None,
    };
    if b_p(p, "precomp") == Some(true) && nested.is_none() {
        return Err(bad(C, "`precomp: true` needs one selected precomp layer"));
    }
    let names: Vec<String> = comp.layers.iter().filter(|l| ids.contains(&l.id)).map(|l| l.name.clone()).collect();
    let mut sides = lv::read(s, &folder).map_err(|e| bad(C, e))?;
    let now = lv::now_ms();
    let mut n = 0u64;
    let id = loop {
        let id = format!("v{now:x}{n:x}");
        if !sides.iter().any(|v| v.id == id) && lv::index(s).find(&id).is_none() {
            break id;
        }
        n += 1;
    };
    let rel = format!("{}/{}/{id}.ecproj", lv::FOLDER, lv::safe_name(&parent));
    let path = lv::resolve(&folder, &rel);
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(d) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(d).map_err(|e| bad(C, format!("cannot make {}: {e}", d.display())))?;
    }
    let (kind, comp_name) = match nested {
        Some(src) => {
            s.execute("file.exportComp", json!({"path": path, "comp": src.0}))?;
            ("comp", s.project.item(src).map(|i| i.name.clone()).unwrap_or_default())
        }
        None => {
            let ids_json: Vec<u64> = ids.iter().map(|l| l.0).collect();
            let r = s.execute("file.exportLayers", json!({"path": path, "comp": cid.0, "layers": ids_json}))?;
            ("layers", r["comp"].as_str().unwrap_or_default().to_string())
        }
    };
    card.fill_from(&doc_situation(s, cid, &ids));
    card.created = now;
    card.updated = now;
    let count = lv::index(s).count(&parent) + 1;
    let name = match str_p(p, "name").map(str::trim).filter(|x| !x.is_empty()) {
        Some(n) => n.chars().take(lv::NAME_LIMIT).collect(),
        None => format!("{parent_name} · variant {count}"),
    };
    let v = Variant { id, parent, name, file: rel, comp: comp_name, kind: kind.into(), layers: names, card, folder: folder.clone() };
    sides.push(v.clone());
    if let Err(e) = lv::write(s, &folder, &sides) {
        #[cfg(not(target_arch = "wasm32"))]
        let _ = std::fs::remove_file(&path);
        return Err(bad(C, e));
    }
    s.toast(format!("Saved “{}” as a variant of “{parent_name}”", v.name));
    let mut out = v.to_json();
    out["count"] = json!(count);
    Ok(out)
}

fn variants(s: &mut Session, p: &Value) -> Result<Value> {
    let idx = lv::index(s);
    let errors: Vec<Value> = idx.errors.iter().map(|(f, e)| json!({"library": f, "error": e})).collect();
    match str_p(p, "parent") {
        Some(parent) => {
            let list: Vec<Value> = idx.by_parent.get(parent).into_iter().flatten().map(Variant::to_json).collect();
            Ok(json!({"parent": parent, "count": list.len(), "variants": list, "errors": errors}))
        }
        None => {
            let counts: serde_json::Map<String, Value> = idx.by_parent.iter().map(|(k, v)| (k.clone(), json!(v.len()))).collect();
            Ok(json!({"total": idx.all().count(), "counts": counts, "errors": errors}))
        }
    }
}

fn match_variant(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.matchVariant";
    let mut query = card_p(C, p, "situation")?;
    if b_p(p, "fromSelection").unwrap_or(false) {
        let (cid, ids) = layers_p(s, p)?;
        if ids.is_empty() {
            return Err(bad(C, "`fromSelection` needs selected layers"));
        }
        let doc = doc_situation(s, cid, &ids);
        query.fill_from(&doc);
    }
    if !lv::has_features(&query) {
        return Err(bad(C, "describe the situation: shot, layer + compSize, aspect, word, duration or energy"));
    }
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(5).clamp(1, 100) as usize;
    let parent = str_p(p, "parent");
    let idx = lv::index(s);
    let pool: Vec<&Variant> = match parent {
        Some(id) => idx.by_parent.get(id).into_iter().flatten().collect(),
        None => idx.all().collect(),
    };
    let ranked = lv::rank(&query, pool.iter().copied());
    let matches: Vec<Value> = ranked
        .iter()
        .take(limit)
        .map(|(score, matched, each, v)| {
            let mut o = v.to_json();
            o["score"] = json!((score * 1000.0).round() / 1000.0);
            o["distance"] = json!(((1.0 - score) * 1000.0).round() / 1000.0);
            o["matched"] = json!(matched);
            o["features"] = json!(each);
            o
        })
        .collect();
    Ok(json!({"situation": query, "considered": ranked.len(), "weights": lv::WEIGHTS.iter().map(|(n, w)| json!({"feature": n, "weight": w})).collect::<Vec<_>>(), "matches": matches}))
}

fn update_card(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.updateCard";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id` (a variant id, from library.variants)"))?;
    let folder = lv::index(s).find(id).map(|v| v.folder.clone()).ok_or_else(|| bad(C, format!("no variant `{id}`")))?;
    let mut sides = lv::read(s, &folder).map_err(|e| bad(C, e))?;
    let v = sides.iter_mut().find(|v| v.id == id).ok_or_else(|| bad(C, format!("no variant `{id}`")))?;
    // The fields given replace the card's (null clears one); the rest stay.
    let mut merged = serde_json::to_value(&v.card).map_err(|e| EngineError::Other(e.to_string()))?;
    match p.get("card") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => {
            for (k, val) in o {
                merged[k] = val.clone();
            }
        }
        Some(other) => return Err(bad(C, format!("`card` is an object of the fields to change, not {other}"))),
    }
    // Derived values follow their sources unless given.
    for (derived, source) in [("aspect", "compSize"), ("wordCategory", "word")] {
        if p["card"].get(source).is_some() && p["card"].get(derived).is_none() {
            merged[derived] = Value::Null;
        }
    }
    let mut card = SituationCard::from_input(&merged).map_err(|e| bad(C, e))?;
    if let Some(add) = p.get("addWhy") {
        let tags = SituationCard::from_input(&json!({"why": add})).map_err(|e| bad(C, e))?.why;
        for t in tags {
            if !card.why.contains(&t) {
                card.why.push(t);
            }
        }
        card.check().map_err(|e| bad(C, e))?;
    }
    card.created = v.card.created;
    card.updated = lv::now_ms().max(v.card.created);
    v.card = card;
    if let Some(n) = str_p(p, "name").map(str::trim).filter(|x| !x.is_empty()) {
        v.name = n.chars().take(lv::NAME_LIMIT).collect();
    }
    let out = v.to_json();
    lv::write(s, &folder, &sides).map_err(|e| bad(C, e))?;
    Ok(out)
}

fn apply_variant(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.applyVariant";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id` (a variant id, from library.variants or library.matchVariant)"))?;
    let v = lv::index(s).find(id).cloned().ok_or_else(|| bad(C, format!("no variant `{id}`")))?;
    let path = v.path();
    if !s.services.exists(&path) {
        return Err(bad(C, format!("“{}”: its project is missing ({path})", v.name)));
    }
    let mut params = json!({"path": path});
    if let Some(c) = p.get("comp") {
        params["comp"] = c.clone();
    }
    if let Some(t) = f_p(p, "time") {
        params["at"] = json!(t);
    }
    let r = if v.kind == "comp" {
        // A precomp: import the project and nest its comp, one undo step.
        let target = match p.get("comp") {
            Some(_) => super::comp_id(s, p)?,
            None => s.active_comp_id().ok_or(EngineError::NoComp)?,
        };
        let snap = s.project.clone();
        let history = s.history.clone();
        let res = (|| -> Result<Value> {
            let r = s.execute("file.importProject", json!({"path": path}))?;
            let comps = r["comps"].as_array().cloned().unwrap_or_default();
            let item = comps
                .iter()
                .find(|c| c["name"] == v.comp.as_str())
                .and_then(|c| c["id"].as_u64())
                .or_else(|| r["primaryComp"].as_u64())
                .ok_or_else(|| bad(C, format!("“{}”: no composition `{}` in {path}", v.name, v.comp)))?;
            let mut add = json!({"comp": target.0, "item": item});
            if let Some(t) = f_p(p, "time") {
                add["time"] = json!(t);
            }
            let layer = s.execute("layer.addItem", add)?["layer"].clone();
            Ok(json!({"item": item, "layer": layer}))
        })();
        match res {
            Ok(r) => {
                super::batch::fold(s, &snap, &format!("Apply {}", v.name));
                r
            }
            Err(e) => {
                s.project = snap;
                s.history = history;
                s.sanitize_state();
                s.bump();
                return Err(e);
            }
        }
    } else {
        params["from"] = json!(v.comp);
        s.execute("file.importLayers", params)?
    };
    Ok(json!({"variant": v.to_json(), "result": r}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.saveVariant",
                "Save as Variant",
                [],
                None,
                "{parent (the library entry's id), card? (situation card: {text?, word?, wordCategory?, accentFrame? (frames from the start to the accent), layer? ([x, y, w, h] comp px), compSize? ([w, h]), aspect?, shot?: talkingHead|phone|fullScreen|overlay, duration? (s), energy? (0–1 or low/medium/high), ease?, client?, style?, why? ([tags]), note?}), name?, layers? (default selected), comp?, precomp? (one selected precomp layer saves its comp; default true for one, false = its layer), library? (a configured folder; default the entry's)} — saves the refined layers as a variant of the entry in its library folder (variants/<parent>/<id>.ecproj and variants.json beside library.json; the open project is unchanged). Layer box, comp size and duration are filled from the document where the card doesn't say. → {id, parent, name, kind, file, comp, layers, card, summary, library, count}",
                always,
                save_variant
            )
        },
        query!(
            "library.variants",
            "Library Variants",
            "{parent? (an entry id)} → with parent: {parent, count, variants: [{id, parent, name, kind, file, comp, layers, card, summary, library}], errors}; without: {total, counts: {entryId: n}, errors}",
            variants
        ),
        query!(
            "library.matchVariant",
            "Match Library Variant",
            "{situation? (a situation card: shot, layer + compSize, aspect, word or wordCategory, duration, energy…), fromSelection? (fill layer box, comp size and duration from the selected layers), layers?, comp?, parent? (only this entry's variants), limit? (default 5)} — ranks variants by a weighted feature distance (shot 3, size in frame 2, aspect 2, word category 1.5, duration 1, energy 1; a feature the variant doesn't record counts half) → {situation, considered, weights, matches: [{…variant, score (0–1), distance, matched: [features ≥ 0.75], features: {name: similarity|null}}]}",
            match_variant
        ),
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.updateCard",
                "Update Variant Card",
                [],
                None,
                "{id (a variant id), card? (the fields to change; null clears one), addWhy? ([tags] added to the card's why), name?} — rewrites the variant's situation card in variants.json → the variant",
                always,
                update_card
            )
        },
        cmd!(
            "library.applyVariant",
            "Apply Library Variant",
            [],
            None,
            "{id (a variant id), comp? (default the open composition), time? (s, default playhead)} — adds the variant's layers (or nests its comp) in the comp, one undo step → {variant, result}",
            always,
            apply_variant
        ),
    ]
}
