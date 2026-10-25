//! Motion Library commands: the card catalog kept in a remote bucket ([`effectcraft_motionlib`]),
//! for the Motion Library panel and agents alike.
//!
//! - `library.status` / `library.sync`: the catalog version, the "Library updated" chip's update,
//!   errors (offline keeps the cached catalog).
//! - `library.catalog` / `library.card`: browse and inspect cards (`library.search` also finds
//!   them).
//! - `library.apply`: download what a card needs, import its project and nest its composition at
//!   the playhead, one undo step. Footage that was not uploaded comes in as missing items to
//!   relink (`footage.relink`).
//! - `library.configure`: where the catalog is and the token (kept in the settings folder).
//! - `library.publish` (desktop): upload a library folder's chosen folders.

use std::collections::BTreeMap;
use std::sync::Arc;

use effectcraft_motionlib::{Card, Client, Prepared};
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, f_p, str_p};
use crate::{EngineError, Result, Session, cmd, query};

/// How long agents' calls wait for downloads by default (seconds).
#[cfg(not(target_arch = "wasm32"))]
const WAIT: f64 = 120.0;

fn client(s: &Session, c: &str) -> Result<Client> {
    s.motion_library().map_err(|e| bad(c, e))
}

/// Wait for downloads (desktop agents) unless `wait: false` (the panel polls instead).
fn waits(p: &Value) -> bool {
    !cfg!(target_arch = "wasm32") && b_p(p, "wait").unwrap_or(true)
}

#[cfg(not(target_arch = "wasm32"))]
fn timeout(p: &Value) -> std::time::Duration {
    std::time::Duration::from_secs_f64(f_p(p, "timeout").filter(|t| t.is_finite() && *t > 0.0).unwrap_or(WAIT).min(3600.0))
}

/// A card as lists show it, `local` when this machine has its files (a library folder: it
/// applies from them, so it isn't "on the Mac" here).
pub fn card_summary(c: &Card, local: bool) -> Value {
    let description: String = c.description.chars().take(240).collect();
    json!({
        "id": c.id,
        "name": c.name,
        "kind": c.kind,
        "kindLabel": c.subtitle().split(" · ").next().unwrap_or_default(),
        "category": c.category,
        "location": if c.location.is_empty() { &c.category } else { &c.location },
        "description": description,
        "duration": c.duration,
        "needsFootage": c.needs_footage(),
        "onMac": c.on_mac && !local,
        "local": local,
        "source": "motion",
        "library": "Motion Library",
    })
}

fn card_detail(c: &Card, local: bool) -> Value {
    let mut v = card_summary(c, local);
    v["description"] = json!(c.description);
    v["comp"] = json!(c.comp);
    v["tags"] = json!(c.tags);
    v["aliases"] = json!(c.aliases);
    v["files"] = json!(c.files.iter().map(|f| json!({"name": f.blob.name, "size": f.blob.size})).collect::<Vec<_>>());
    v["placeholders"] = json!(c.placeholders);
    v["hasPreview"] = json!(c.preview.is_some());
    v["downloadBytes"] = json!(c.apply_blobs().iter().map(|b| b.size).sum::<u64>());
    v
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    let cfg = s.motion_library_config();
    match s.motion_library() {
        Ok(c) => {
            let mut v = serde_json::to_value(c.status()).map_err(|e| EngineError::Other(e.to_string()))?;
            v["configured"] = json!(true);
            v["tokenSet"] = json!(!cfg.token.is_empty());
            Ok(v)
        }
        Err(e) => Ok(json!({"configured": false, "error": e})),
    }
}

fn sync(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.sync";
    let c = client(s, C)?;
    if waits(p) {
        #[cfg(not(target_arch = "wasm32"))]
        c.refresh_blocking(timeout(p));
    } else {
        c.refresh();
    }
    let adopted = b_p(p, "adopt").unwrap_or(false) && c.adopt_update();
    let mut v = serde_json::to_value(c.status()).map_err(|e| EngineError::Other(e.to_string()))?;
    v["adopted"] = json!(adopted);
    Ok(v)
}

fn update(s: &mut Session, _: &Value) -> Result<Value> {
    let c = client(s, "library.update")?;
    let adopted = c.adopt_update();
    let mut v = serde_json::to_value(c.status()).map_err(|e| EngineError::Other(e.to_string()))?;
    v["adopted"] = json!(adopted);
    Ok(v)
}

/// The catalog, refreshed first when none is cached yet (agents' first call).
fn catalog_of(s: &Session, c: &str, p: &Value) -> Result<(Client, Arc<effectcraft_motionlib::Manifest>)> {
    let client = client(s, c)?;
    if client.catalog().is_none() && waits(p) {
        #[cfg(not(target_arch = "wasm32"))]
        client.refresh_blocking(timeout(p));
    }
    let cat =
        client.catalog().ok_or_else(|| bad(c, client.status().error.unwrap_or_else(|| "the Motion Library catalog isn't loaded yet (library.sync)".into())))?;
    Ok((client, cat))
}

fn catalog(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.catalog";
    let (client, cat) = catalog_of(s, C, p)?;
    let folder = str_p(p, "folder").unwrap_or("").trim_matches('/');
    let query = str_p(p, "query").unwrap_or("");
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 500) as usize;
    let offset = p.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let hits = effectcraft_motionlib::search(&cat.cards, query, folder);
    // In the library's order, with each folder's depth and whether it has an icon.
    let folders: Vec<Value> = cat.tree().into_iter().map(|r| json!({"folder": r.path, "cards": r.cards, "depth": r.depth, "icon": r.icon.is_some()})).collect();
    Ok(json!({
        "version": client.status().version,
        "name": cat.name,
        "folders": folders,
        "total": hits.len(),
        "offset": offset,
        "cards": hits.iter().skip(offset).take(limit).map(|c| card_summary(c, s.motion_library_local_entry(&c.id).is_some())).collect::<Vec<_>>(),
    }))
}

fn card(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.card";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id`"))?;
    let (_, cat) = catalog_of(s, C, p)?;
    let c = cat.card(id).ok_or_else(|| bad(C, format!("no Motion Library card `{id}`")))?;
    Ok(card_detail(c, s.motion_library_local_entry(id).is_some()))
}

/// Motion Library hits for `library.search` (best first), when the library is set up.
pub fn search_hits(s: &Session, query: &str, category: Option<&str>, kind: Option<&str>) -> Vec<Value> {
    let Ok(client) = s.motion_library() else { return vec![] };
    let Some(cat) = client.catalog() else { return vec![] };
    effectcraft_motionlib::search(&cat.cards, query, "")
        .into_iter()
        .filter(|c| kind.is_none_or(|k| c.kind.eq_ignore_ascii_case(k)))
        .filter(|c| category.is_none_or(|q| c.category.to_lowercase().contains(q)))
        // Insertable cards only: on-the-Mac cards are listed by library.catalog, unless this
        // machine has their files.
        .filter_map(|c| {
            let local = s.motion_library_local_entry(&c.id).is_some();
            (c.applicable() || local).then(|| card_summary(c, local))
        })
        .collect()
}

/// A cached file's path for the importer: the store's own file, else written through the
/// session's services (the browser keeps it in its storage).
fn materialize(s: &Session, client: &Client, key: &str, bytes: impl FnOnce() -> Option<Vec<u8>>) -> std::result::Result<String, String> {
    if let Some(p) = client.store().path(key) {
        return Ok(p.to_string_lossy().to_string());
    }
    let path = format!("/motion-library/{key}");
    let data = bytes().ok_or_else(|| format!("{key} is not cached"))?;
    s.services.store_file(&path, &data).map_err(|e| format!("{path}: {e}"))?;
    Ok(path)
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.apply";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id` (a Motion Library card id, from library.catalog or library.search)"))?.to_string();
    // The Mac that keeps the library applies the card's own files: nothing to download, and
    // its footage stays linked (`remote: true` uses the bucket anyway).
    if !b_p(p, "remote").unwrap_or(false)
        && let Some(e) = s.motion_library_local_entry(&id)
    {
        return apply_local(s, p, &e);
    }
    let (client, _) = catalog_of(s, C, p)?;
    #[cfg(not(target_arch = "wasm32"))]
    let prepared = if waits(p) { client.prepare_blocking(&id, timeout(p)) } else { client.prepare(&id) };
    #[cfg(target_arch = "wasm32")]
    let prepared = client.prepare(&id);
    let (card, project, files) = match prepared {
        Prepared::Loading { done, total } => return Ok(json!({"pending": true, "done": done, "total": total})),
        Prepared::Failed(e) => return Err(bad(C, e)),
        Prepared::Ready { card, project, files } => (card, project, files),
    };
    // Where the layer goes: `comp`, else the open composition (none: import only).
    let target = match p.get("comp") {
        Some(_) => Some(super::comp_id(s, p)?),
        None => s.active_comp_id(),
    };
    let blob = card.project.clone().unwrap_or_default();
    // The project names the publishing Mac's files: point it at the cached copies.
    let mut paths = BTreeMap::new();
    for (original, b, _) in &files {
        let local = materialize(s, &client, &b.local_key(), || client.blob_bytes(b)).map_err(|e| bad(C, e))?;
        paths.insert(original.clone(), local);
    }
    let snap = s.project.clone();
    let history = s.history.clone();
    let result = (|| -> Result<Value> {
        let (imported, item) = if card.kind == "footage" {
            let path = materialize(s, &client, &blob.local_key(), || Some(project.clone())).map_err(|e| bad(C, e))?;
            let r = s.execute("file.import", json!({"paths": [path]}))?;
            let item = r["items"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_u64)
                .ok_or_else(|| bad(C, format!("“{}”: nothing was imported", card.name)))?;
            (r, item)
        } else {
            let rewritten = effectcraft_motionlib::rewrite_paths(&project, &paths).map_err(|e| bad(C, e))?;
            // Named after the card: the Project panel's folder reads as the card.
            let name: String = card.name.chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' }).take(100).collect();
            let key = format!("applied/{}/{}.ecproj", card.rev.get(..16).unwrap_or("card"), if name.trim().is_empty() { "card" } else { name.trim() });
            client.store().put(&key, &rewritten).map_err(|e| bad(C, e))?;
            let path = materialize(s, &client, &key, || Some(rewritten.clone())).map_err(|e| bad(C, e))?;
            let r = s.execute("file.importProject", json!({"path": path}))?;
            let comps = r["comps"].as_array().cloned().unwrap_or_default();
            let named = comps.iter().find(|c| !card.comp.is_empty() && c["name"] == card.comp.as_str()).and_then(|c| c["id"].as_u64());
            let item = named.or_else(|| r["primaryComp"].as_u64()).ok_or_else(|| {
                let names: Vec<&str> = comps.iter().filter_map(|c| c["name"].as_str()).collect();
                bad(C, format!("“{}”: no composition `{}` in the card (it has {})", card.name, card.comp, names.join(", ")))
            })?;
            (r, item)
        };
        let mut layer = Value::Null;
        if let Some(cid) = target.filter(|c| s.project.comp(*c).is_some()) {
            let mut params = json!({"comp": cid.0, "item": item});
            if let Some(t) = f_p(p, "time") {
                params["time"] = json!(t);
            }
            layer = s.execute("layer.addItem", params)?["layer"].clone();
        }
        Ok(json!({"item": item, "layer": layer, "comp": target.map(|c| c.0), "import": imported}))
    })();
    let mut r = match result {
        Ok(r) => r,
        Err(e) => {
            // Leave the project as it was.
            s.project = snap;
            s.history = history;
            s.sanitize_state();
            s.bump();
            return Err(e);
        }
    };
    super::batch::fold(s, &snap, &format!("Apply {}", card.name));
    // Footage left out of the upload: the imported items that are missing, to relink.
    let imported: Vec<u64> = r["import"]["items"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let needs: Vec<Value> = card
        .placeholders
        .iter()
        .map(|ph| {
            let item = s
                .project
                .items
                .values()
                .find(|i| imported.contains(&i.id.0) && matches!(&i.kind, effectcraft_project::ItemKind::Footage(f) if f.path == ph.path));
            json!({"item": item.map(|i| i.id.0), "name": ph.name, "path": ph.path, "reason": ph.reason, "size": ph.size,
                   "duration": ph.duration, "fps": ph.fps, "width": ph.width, "height": ph.height,
                   "missing": !s.services.exists(&ph.path)})
        })
        .collect();
    r["card"] = card_summary(&card, false);
    r["source"] = json!("remote");
    super::library::report_problems(s, &snap, &mut r);
    if !needs.is_empty() {
        r["needsFootage"] = json!(needs);
        r["hint"] = json!("Relink each missing item to this video's footage: footage.relink {item, path}");
        s.toast(format!("Applied “{}”: {} footage item{} to relink", card.name, needs.len(), if needs.len() == 1 { "" } else { "s" }));
    }
    Ok(r)
}

/// `library.reveal`: open the folder holding a card's file on this machine (a library folder).
fn reveal(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.reveal";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id` (a Motion Library card id)"))?;
    let e = s.motion_library_local_entry(id).ok_or_else(|| bad(C, format!("`{id}` has no file on this machine (it comes from the Motion Library bucket)")))?;
    let url = super::layer_menu::folder_url(&e.file);
    s.events.push(crate::Event::OpenUrl(url.clone()));
    Ok(json!({"url": url, "file": e.file}))
}

/// `library.apply` from a library folder (`library.insert`), one undo step.
fn apply_local(s: &mut Session, p: &Value, e: &crate::motion_library::LocalEntry) -> Result<Value> {
    let snap = s.project.clone();
    let history = s.history.clone();
    match s.execute("library.insert", p.clone()) {
        Ok(mut r) => {
            super::batch::fold(s, &snap, &format!("Apply {}", e.name));
            r["source"] = json!("local");
            r["file"] = json!(e.file);
            Ok(r)
        }
        Err(err) => {
            s.project = snap;
            s.history = history;
            s.sanitize_state();
            s.bump();
            Err(err)
        }
    }
}

fn configure(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.configure";
    let store = s.config.clone().ok_or_else(|| bad(C, "this session has no settings folder to keep the configuration"))?;
    let mut cfg: crate::motion_library::Config = store.read(crate::motion_library::CONFIG).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    for (k, slot) in [("remote", &mut cfg.remote), ("token", &mut cfg.token), ("cache", &mut cfg.cache)] {
        if let Some(v) = str_p(p, k) {
            *slot = v.trim().to_string();
        }
    }
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| EngineError::Other(e.to_string()))?;
    store.write(crate::motion_library::CONFIG, &text).map_err(|e| bad(C, format!("cannot save {}: {e}", crate::motion_library::CONFIG)))?;
    // A new location means a new client (the old one keeps nothing the new one needs).
    s.motion_library.set(None);
    let eff = s.motion_library_config();
    Ok(json!({"remote": eff.remote, "tokenSet": !eff.token.is_empty(), "cache": eff.cache}))
}

#[cfg(not(target_arch = "wasm32"))]
fn publish(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.publish";
    let library = str_p(p, "library").ok_or_else(|| bad(C, "missing `library` (the folder with library.json)"))?;
    let folders: Vec<String> = match p.get("folders") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(f)) => f.split(',').map(|x| x.trim().to_string()).collect(),
        _ => vec!["Devin Jatho".into(), "SceneYard".into()],
    };
    let mut opts = effectcraft_motionlib::publish::Options::new(library, folders);
    if let Some(mb) = f_p(p, "maxFileMB").filter(|m| m.is_finite() && *m > 0.0) {
        opts.max_file_bytes = (mb * 1e6) as u64;
    }
    if let Some(mb) = f_p(p, "maxPreviewMB").filter(|m| m.is_finite() && *m >= 0.0) {
        opts.max_preview_bytes = (mb * 1e6) as u64;
    }
    if let Some(map) = p.get("folderMaxMB").and_then(Value::as_object) {
        opts.folder_max_bytes =
            map.iter().filter_map(|(f, mb)| Some((f.clone(), (mb.as_f64().filter(|m| m.is_finite() && *m >= 0.0)? * 1e6) as u64))).collect();
    }
    opts.include_video = b_p(p, "includeVideo").unwrap_or(false);
    opts.catalog_all = b_p(p, "catalogAll").unwrap_or(false);
    opts.dry_run = b_p(p, "dryRun").unwrap_or(false);
    opts.force = b_p(p, "force").unwrap_or(false);
    let remote = match str_p(p, "remote") {
        Some(r) => effectcraft_motionlib::remote_for(r, Some(&s.motion_library_config().token)).map_err(|e| bad(C, e))?,
        None => {
            let cfg = s.motion_library_config();
            effectcraft_motionlib::remote_for(&cfg.remote, Some(&cfg.token)).map_err(|e| bad(C, e))?
        }
    };
    let mut log = vec![];
    let report = effectcraft_motionlib::publish::publish(remote.as_ref(), &opts, &mut |line| {
        log::info!("library.publish: {line}");
        log.push(line);
    })
    .map_err(|e| bad(C, e))?;
    let mut v = serde_json::to_value(&report).map_err(|e| EngineError::Other(e.to_string()))?;
    // The skipped list can be long: the first ones, and how many there are.
    if let Some(list) = v.get_mut("skipped").and_then(Value::as_array_mut) {
        let n = list.len();
        list.truncate(40);
        v["skippedCount"] = json!(n);
    }
    v["log"] = json!(log);
    Ok(v)
}

/// Uploads above this many MB wait for `confirm: true` (Elias's internet is metered).
const ASK_ABOVE_MB: f64 = 50.0;

/// `library.saveCard`: upload one card from this Mac's library folder to the bucket (the
/// catalog's own folders and limits; video and big files stay home), in the background. A dry
/// run first: over `askAboveMB` it stops with `needsConfirm` and the size.
#[cfg(not(target_arch = "wasm32"))]
fn save_card(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.saveCard";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id` (a card in this Mac's library folder)"))?.to_string();
    let e = s
        .motion_library_local_entry(&id)
        .ok_or_else(|| bad(C, format!("`{id}` isn't in a library folder on this machine: cards are saved from the Mac that keeps the library")))?;
    let cfg = s.motion_library_config();
    if cfg.remote.is_empty() || cfg.token.is_empty() {
        return Err(bad(C, "the Motion Library isn't set up for saving here (no remote or token in motion-library.json)"));
    }
    let remote = effectcraft_motionlib::remote_for(&cfg.remote, Some(&cfg.token)).map_err(|e| bad(C, e))?;
    let client = s.motion_library().ok();
    // The published catalog's settings, so the save changes nothing else.
    let cat = client.as_ref().and_then(Client::catalog);
    let folders = cat.as_ref().map(|c| c.folders.clone()).filter(|f| !f.is_empty()).unwrap_or_else(|| vec!["Devin Jatho".into(), "SceneYard".into()]);
    let mut opts = effectcraft_motionlib::publish::Options::new(&e.folder, folders);
    opts.ids = vec![id.clone()];
    opts.catalog_all = cat.as_ref().is_some_and(|c| c.cards.iter().any(|c| c.on_mac));
    // The Mac publish script's limits (option C): small previews, SceneYard files up to 20 MB.
    opts.max_preview_bytes = (f_p(p, "maxPreviewMB").filter(|m| m.is_finite() && *m >= 0.0).unwrap_or(2.0) * 1e6) as u64;
    opts.folder_max_bytes = match p.get("folderMaxMB").and_then(Value::as_object) {
        Some(map) => map.iter().filter_map(|(f, mb)| Some((f.clone(), (mb.as_f64().filter(|m| m.is_finite() && *m >= 0.0)? * 1e6) as u64))).collect(),
        None => vec![("SceneYard".into(), 20_000_000)],
    };
    let dry = b_p(p, "dryRun").unwrap_or(false);
    let confirm = b_p(p, "confirm").unwrap_or(false);
    let ask = f_p(p, "askAboveMB").filter(|m| m.is_finite() && *m >= 0.0).unwrap_or(ASK_ABOVE_MB);
    let name = e.name.clone();
    s.spawn_task("librarySave", format!("Saving “{name}” to the library"), b_p(p, "wait").unwrap_or(false), move |ctl| {
        ctl.message("working out what to upload");
        let mut plan = opts.clone();
        plan.dry_run = true;
        let r = effectcraft_motionlib::publish::publish(remote.as_ref(), &plan, &mut |line| ctl.message(line))?;
        let mb = r.upload_bytes as f64 / 1e6;
        // What stays home (video, too large), for this card.
        let skipped: Vec<Value> = r
            .skipped
            .iter()
            .filter(|k| k.card == name)
            .map(|k| json!({"file": k.path.rsplit('/').next().unwrap_or(&k.path), "size": k.size, "reason": k.reason}))
            .collect();
        let skipped_text = if skipped.is_empty() {
            String::new()
        } else {
            let names: Vec<String> =
                skipped.iter().take(3).map(|k| format!("{} ({})", k["file"].as_str().unwrap_or(""), k["reason"].as_str().unwrap_or(""))).collect();
            format!("; left at home: {}{}", names.join(", "), if skipped.len() > 3 { format!(" and {} more", skipped.len() - 3) } else { String::new() })
        };
        let base = json!({"id": id, "name": name, "uploadMB": (mb * 10.0).round() / 10.0, "uploads": r.uploads, "skipped": skipped});
        if dry || (mb > ask && !confirm) {
            let mut out = base;
            out["dryRun"] = json!(dry);
            out["needsConfirm"] = json!(!dry);
            out["toast"] = json!(if dry {
                format!("Saving “{name}” would upload {mb:.1} MB{skipped_text}")
            } else {
                format!("“{name}” is {mb:.1} MB to upload: confirm to save it{skipped_text}")
            });
            return Ok(Box::new(move |_: &mut Session| Ok(out)) as super::super::jobs::Apply);
        }
        ctl.message(format!("uploading {mb:.1} MB"));
        let done = effectcraft_motionlib::publish::publish(remote.as_ref(), &opts, &mut |line| ctl.message(line))?;
        // This session sees the saved card right away.
        if let Some(c) = &client {
            c.refresh_blocking(std::time::Duration::from_secs(60));
            c.adopt_update();
        }
        let mut out = base;
        out["version"] = json!(done.version);
        out["toast"] = json!(format!("Saved “{name}” to the library ({mb:.1} MB uploaded, version {}){skipped_text}", done.version));
        Ok(Box::new(move |_: &mut Session| Ok(out)) as super::super::jobs::Apply)
    })
}

#[cfg(target_arch = "wasm32")]
fn save_card(_: &mut Session, _: &Value) -> Result<Value> {
    Err(bad("library.saveCard", "cards are saved from the Mac that keeps the library"))
}

#[cfg(target_arch = "wasm32")]
fn publish(_: &mut Session, _: &Value) -> Result<Value> {
    Err(bad("library.publish", "publishing runs on the Mac that keeps the library"))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "library.status",
            "Motion Library Status",
            "{} → {configured, remote, cache, version, cards, needsFootage, update: {version, new, changed, removed, label}|null, refreshing, checked, error, downloading, failed, tokenSet}",
            status
        ),
        query!(
            "library.sync",
            "Check Motion Library",
            "{wait? (default true on desktop), timeout? (s, default 120), adopt? (also switch to a newer catalog)} — checks the remote for a newer catalog (the first one is adopted at once; later ones wait for library.update, the panel's “Library updated” chip). Offline keeps the cached catalog. → library.status + {adopted}",
            sync
        ),
        query!("library.update", "Update Motion Library", "{} — switch to the newer catalog library.sync found → library.status + {adopted}", update),
        query!(
            "library.catalog",
            "Motion Library Catalog",
            "{folder? (e.g. \"Devin Jatho\" or \"Devin Jatho/UI\"; subfolders included), query? (words, as library.search), limit? (default 50, max 500), offset?} → {version, name, folders: [{folder, cards}], total, cards: [{id, name, kind, category, description, duration, needsFootage}]}",
            catalog
        ),
        query!(
            "library.card",
            "Motion Library Card",
            "{id} → the card with its files, placeholders (footage not uploaded: name, path, size, duration, fps, reason) and download size",
            card
        ),
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.reveal",
                "Reveal Motion Library Card in Finder",
                [],
                None,
                "{id} → {url, file}: opens the folder of the card's file on this machine",
                always,
                reveal
            )
        },
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.apply",
                "Apply Motion Library Card",
                [],
                None,
                "{id, comp? (default: the open composition; none open = import only), time? (s, default playhead), wait? (default true on desktop: download first; false returns {pending, done, total} until ready), timeout? (s), remote? (true: from the bucket even when a library folder has the card)} — imports the card's project and nests its composition, one undo step; a card a library folder lists applies from those files (source \"local\") → {item, layer, comp, card, source, import, needsFootage?: [{item, name, path, reason, duration, fps, missing}], hint?}. Relink needsFootage items with footage.relink {item, path}",
                always,
                apply
            )
        },
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.configure",
                "Configure Motion Library",
                [],
                None,
                "{remote? (the Worker URL or a folder), token? (kept in the settings folder, never shown), cache? (folder)} → {remote, tokenSet, cache}",
                always,
                configure
            )
        },
        query!(
            "library.publish",
            "Publish Motion Library",
            "{library (folder with library.json), folders? (default [\"Devin Jatho\", \"SceneYard\"]), maxFileMB? (default 90: the Worker takes uploads up to 100 MB), maxPreviewMB?, folderMaxMB? ({\"SceneYard\": 5}: per-folder file limit), includeVideo? (default false), catalogAll? (also list every other entry with its thumbnail only: applied on the Mac), dryRun?, force? (upload every blob again), remote? (default the configured one)} — desktop: uploads only new files, then the manifest and a bumped version → {version, previous, unchanged, cards, needsFootage, onMac, uploads, uploadBytes, blobs, totalBytes, new, changed, removed, skipped, skippedCount, errors, log}",
            publish
        ),
        CommandSpec {
            journal: false,
            ..cmd!(
                "library.saveCard",
                "Save Card to Motion Library",
                [],
                None,
                "{id (a card in this Mac's library folder), confirm? (upload even above askAboveMB), askAboveMB? (default 50), dryRun?, wait?, maxPreviewMB? (default 2), folderMaxMB? (default {\"SceneYard\": 20})} — desktop, in the background: uploads the card's files (video and big files stay home) and a new catalog version; it stays saved in later publishes → {job}; the job's result: {id, name, uploadMB, uploads, skipped: [{file, size, reason}], needsConfirm?, version?}",
                always,
                save_card
            )
        },
    ]
}
