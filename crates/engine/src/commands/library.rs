//! Libraries: folders of reusable motion (compositions, animation presets, footage and sound
//! effects) indexed by a `library.json`, searched and inserted by people and agents alike.
//!
//! ```json
//! {"format": "effectcraft-library", "version": 1, "name": "Studio",
//!  "entries": [{"id": "lower-third", "name": "Lower Third", "kind": "comp",
//!               "file": "projects/lower-third.ecproj", "comp": "Lower Third",
//!               "category": "Titles", "tags": ["name", "caption"], "description": "…",
//!               "duration": 4.0, "thumbnail": "thumbs/lower-third.png", "preview": "…mp4"}]}
//! ```
//!
//! Kinds: `comp` (an `.ecproj`, nested by its `comp` name), `preset` (an `.ecpreset` applied to
//! layers), `footage` and `audio` (media files). Relative paths resolve against the library
//! folder. Settings ▸ `libraries` lists the folders.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, f_p, str_p};
use crate::{EngineError, Result, Session, cmd, query};

pub const INDEX: &str = "library.json";
const FORMAT: &str = "effectcraft-library";
const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Index {
    format: String,
    version: u32,
    name: String,
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Entry {
    id: String,
    name: String,
    kind: String,
    file: String,
    /// The composition to nest (`comp` entries); else the project's only root comp.
    comp: String,
    category: String,
    tags: Vec<String>,
    description: String,
    duration: Option<f64>,
    thumbnail: String,
    preview: String,
    /// A folder of PNG frames played on hover (the Motion Library's MotionEssence cards).
    frames: String,
    /// The visible artwork inside a transparent thumbnail.
    crop: Option<effectcraft_motionlib::Crop>,
    /// When the card was made (Unix seconds; a number or a numeric string), when the index says.
    created_at: Option<Value>,
}

/// A library folder, read.
struct Library {
    folder: String,
    name: String,
    entries: Vec<Entry>,
}

fn resolve(folder: &str, path: &str) -> String {
    if path.is_empty() || std::path::Path::new(path).is_absolute() {
        path.to_string()
    } else {
        std::path::Path::new(folder).join(path).to_string_lossy().to_string()
    }
}

fn index_path(folder: &str) -> String {
    std::path::Path::new(folder).join(INDEX).to_string_lossy().to_string()
}

fn read_library(s: &Session, folder: &str) -> std::result::Result<Library, String> {
    let path = index_path(folder);
    let bytes = s.services.read_file(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let index: Index = serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))?;
    if index.format != FORMAT {
        return Err(format!("{path} is not an EffectCraft library index"));
    }
    if index.version > VERSION {
        return Err(format!("{path} was written for a newer EffectCraft (version {})", index.version));
    }
    let name =
        if index.name.is_empty() { std::path::Path::new(folder).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { index.name };
    Ok(Library { folder: folder.to_string(), name, entries: index.entries.into_iter().filter(|e| !e.id.is_empty()).collect() })
}

/// Every configured library that reads (the others are reported by `library.folders`).
fn libraries(s: &Session) -> Vec<Library> {
    s.prefs.libraries.iter().filter_map(|f| read_library(s, f).ok()).collect()
}

/// A library folder's entry with absolute paths. On the Mac that keeps the Motion Library, its
/// cards apply and preview from these files (same ids) instead of the bucket.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub category: String,
    pub file: String,
    pub preview: String,
    pub thumbnail: String,
    /// A folder of PNG frames to play on hover (the index's `frames`, else `Png` beside the
    /// thumbnail); may not exist.
    pub frames: String,
    /// The visible artwork inside its transparent thumbnail and frames.
    pub crop: Option<effectcraft_motionlib::Crop>,
    pub folder: String,
    /// When the card was made (the index's `created_at`, Unix seconds), when it says.
    pub created: Option<f64>,
}

/// Local entries by id (the first folder listing an id wins).
pub type LocalIndex = BTreeMap<String, LocalEntry>;

/// An index file's size and modification time (none on the web).
fn stamp(path: &str) -> Option<(u64, std::time::SystemTime)> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let m = std::fs::metadata(path).ok()?;
        Some((m.len(), m.modified().ok()?))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

type LocalKey = Vec<(String, Option<(u64, std::time::SystemTime)>)>;

/// Every configured folder's entries, read again only when a folder or its index changes (the
/// Motion Library panel asks every frame).
pub(crate) fn local_entries(s: &Session) -> Arc<LocalIndex> {
    static CACHE: Mutex<Option<(LocalKey, Arc<LocalIndex>)>> = Mutex::new(None);
    let key: LocalKey = s.prefs.libraries.iter().map(|f| (f.clone(), stamp(&index_path(f)))).collect();
    if let Ok(c) = CACHE.lock()
        && let Some((k, index)) = c.as_ref()
        && *k == key
    {
        return index.clone();
    }
    let mut index = LocalIndex::new();
    for lib in libraries(s) {
        for e in &lib.entries {
            index.entry(e.id.clone()).or_insert_with(|| LocalEntry {
                id: e.id.clone(),
                name: e.name.clone(),
                kind: e.kind.clone(),
                category: e.category.clone(),
                file: resolve(&lib.folder, &e.file),
                preview: resolve(&lib.folder, &e.preview),
                thumbnail: resolve(&lib.folder, &e.thumbnail),
                frames: if e.frames.is_empty() {
                    std::path::Path::new(&resolve(&lib.folder, &e.thumbnail)).parent().map(|d| d.join("Png").to_string_lossy().to_string()).unwrap_or_default()
                } else {
                    resolve(&lib.folder, &e.frames)
                },
                crop: e.crop.and_then(effectcraft_motionlib::Crop::valid),
                folder: lib.folder.clone(),
                created: e
                    .created_at
                    .as_ref()
                    .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok())))
                    .filter(|t: &f64| t.is_finite()),
            });
        }
    }
    let index = Arc::new(index);
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((key, index.clone()));
    }
    index
}

fn summary(lib: &Library, e: &Entry) -> Value {
    json!({
        "id": e.id,
        "name": e.name,
        "kind": e.kind,
        "category": e.category,
        "tags": e.tags,
        "description": e.description,
        "duration": e.duration,
        "thumbnail": resolve(&lib.folder, &e.thumbnail),
        "library": lib.name,
    })
}

fn detail(lib: &Library, e: &Entry) -> Value {
    let mut v = summary(lib, e);
    let file = resolve(&lib.folder, &e.file);
    v["file"] = json!(file);
    v["comp"] = json!(e.comp);
    v["preview"] = json!(resolve(&lib.folder, &e.preview));
    v["folder"] = json!(lib.folder);
    v
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// How well `e` matches every query word (0 = not every word matches): a word in the name counts
/// most, then tags, category and description; a whole-word or prefix hit beats a substring.
fn score(e: &Entry, query: &[String]) -> u32 {
    let fields: [(u32, Vec<String>); 4] =
        [(8, words(&e.name)), (4, e.tags.iter().flat_map(|t| words(t)).collect()), (2, words(&e.category)), (1, words(&e.description))];
    let mut total = 0;
    for q in query {
        let best = fields
            .iter()
            .map(|(w, ws)| {
                let hit = if ws.iter().any(|x| x == q) {
                    3
                } else if ws.iter().any(|x| x.starts_with(q.as_str())) {
                    2
                } else if ws.iter().any(|x| x.contains(q.as_str())) {
                    1
                } else {
                    0
                };
                w * hit
            })
            .max()
            .unwrap_or(0);
        if best == 0 {
            return 0;
        }
        total += best;
    }
    total.max(1)
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let query = words(str_p(p, "query").unwrap_or(""));
    let kind = str_p(p, "kind").map(str::to_lowercase);
    let category = str_p(p, "category").map(str::to_lowercase);
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 200) as usize;
    let offset = p.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let libs = libraries(s);
    let mut hits: Vec<(u32, &Library, &Entry)> = libs
        .iter()
        .flat_map(|lib| lib.entries.iter().map(move |e| (lib, e)))
        .filter(|(_, e)| kind.as_deref().is_none_or(|k| e.kind.eq_ignore_ascii_case(k)))
        .filter(|(_, e)| category.as_deref().is_none_or(|c| e.category.to_lowercase().contains(c)))
        .filter_map(|(lib, e)| {
            let sc = score(e, &query);
            (sc > 0).then_some((sc, lib, e))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.name.to_lowercase().cmp(&b.2.name.to_lowercase())));
    // Motion Library cards first (ranked as the Motion Library panel ranks them), then folders.
    let source = str_p(p, "source").unwrap_or("all");
    let mut all: Vec<Value> =
        if source == "folders" { vec![] } else { super::motion_library::search_hits(s, str_p(p, "query").unwrap_or(""), category.as_deref(), kind.as_deref()) };
    if source != "motion" {
        all.extend(hits.iter().map(|(_, lib, e)| summary(lib, e)));
    }
    let total = all.len();
    let entries: Vec<Value> = all.into_iter().skip(offset).take(limit).collect();
    Ok(json!({"total": total, "offset": offset, "entries": entries}))
}

fn find<'a>(libs: &'a [Library], id: &str) -> Option<(&'a Library, &'a Entry)> {
    libs.iter().find_map(|lib| lib.entries.iter().find(|e| e.id == id).map(|e| (lib, e)))
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    let id = str_p(p, "id").ok_or_else(|| bad("library.get", "missing `id`"))?;
    let libs = libraries(s);
    let (lib, e) = find(&libs, id).ok_or_else(|| bad("library.get", format!("no library entry `{id}`")))?;
    Ok(detail(lib, e))
}

fn insert(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.insert";
    let id = str_p(p, "id").ok_or_else(|| bad(C, "missing `id`"))?;
    let libs = libraries(s);
    let Some((lib, e)) = find(&libs, id) else {
        // A Motion Library card (library.search lists them too).
        if s.motion_library().ok().and_then(|c| c.catalog()).is_some_and(|cat| cat.card(id).is_some()) {
            return s.execute("library.apply", p.clone());
        }
        return Err(bad(C, format!("no library entry `{id}`")));
    };
    let snap = s.project.clone();
    let file = resolve(&lib.folder, &e.file);
    if file.is_empty() || !s.services.exists(&file) {
        return Err(bad(C, format!("“{}” has no file EffectCraft can use yet ({})", e.name, if file.is_empty() { "none listed" } else { &file })));
    }
    let (entry, wanted) = (detail(lib, e), e.comp.clone());
    let kind = e.kind.to_lowercase();
    if kind == "preset" {
        let mut params = json!({"path": file});
        if let Some(layers) = p.get("layers") {
            params["layers"] = layers.clone();
        }
        let r = s.execute("anim.applyPreset", params)?;
        return Ok(json!({"entry": entry, "applied": r}));
    }
    // Where the layer goes: `comp`, else the open composition (none: import only).
    let target = match p.get("comp") {
        Some(_) => Some(super::comp_id(s, p)?),
        None => s.active_comp_id(),
    };
    let (imported, item) = match kind.as_str() {
        "comp" => {
            let r = s.execute("file.importProject", json!({"path": file}))?;
            let comps = r["comps"].as_array().cloned().unwrap_or_default();
            let named = comps.iter().find(|c| !wanted.is_empty() && c["name"] == wanted.as_str()).and_then(|c| c["id"].as_u64());
            let item = named.or_else(|| r["primaryComp"].as_u64()).ok_or_else(|| {
                let names: Vec<&str> = comps.iter().filter_map(|c| c["name"].as_str()).collect();
                bad(C, format!("“{}”: no composition `{wanted}` in {file} (it has {})", e.name, names.join(", ")))
            })?;
            (r, item)
        }
        "footage" | "audio" => {
            let r = s.execute("file.import", json!({"paths": [file]}))?;
            let item = r["items"].as_array().and_then(|a| a.first()).and_then(Value::as_u64).ok_or_else(|| {
                let why = r["errors"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("nothing was imported").to_string();
                bad(C, format!("“{}”: {why}", e.name))
            })?;
            (r, item)
        }
        other => return Err(bad(C, format!("“{}” is a `{other}` entry, which EffectCraft can't insert yet", e.name))),
    };
    let mut layer = Value::Null;
    if let Some(cid) = target.filter(|c| s.project.comp(*c).is_some()) {
        let mut params = json!({"comp": cid.0, "item": item});
        if let Some(t) = f_p(p, "time") {
            params["time"] = json!(t);
        }
        layer = s.execute("layer.addItem", params)?["layer"].clone();
    }
    let mut r = json!({"entry": entry, "item": item, "layer": layer, "import": imported});
    report_problems(s, &snap, &mut r);
    Ok(r)
}

/// What went wrong bringing a library entry in, at the top of its reply instead of buried in
/// `import`: the import's `warnings` and missing media, and expressions that fail in the
/// compositions it added or changed (`expressionErrors`, e.g. one reading a marker or match name
/// the entry doesn't have), also toasted.
pub(crate) fn report_problems(s: &mut Session, before: &effectcraft_project::Project, r: &mut Value) {
    let mut warnings: Vec<String> = r["import"]["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    if let Some(missing) = r["import"]["missingMedia"].as_array().filter(|m| !m.is_empty()) {
        r["missingMedia"] = json!(missing);
    }
    if let Some(errors) = super::expr_tools::new_errors(s, before) {
        let count = errors["count"].as_u64().unwrap_or(0);
        let first = errors["errors"][0]["message"].as_str().unwrap_or_default().to_string();
        warnings.push(format!("{count} expression{} fail{} after applying ({first})", if count == 1 { "" } else { "s" }, if count == 1 { "s" } else { "" }));
        r["expressionErrors"] = errors;
    }
    if !warnings.is_empty() {
        s.toast(format!("Library: {}", warnings.join("; ")));
        r["warnings"] = json!(warnings);
    }
}

fn folders(s: &mut Session, _: &Value) -> Result<Value> {
    let list: Vec<Value> = s
        .prefs
        .libraries
        .clone()
        .iter()
        .map(|f| match read_library(s, f) {
            Ok(lib) => json!({"folder": f, "name": lib.name, "entries": lib.entries.len()}),
            Err(e) => json!({"folder": f, "error": e}),
        })
        .collect();
    Ok(json!({"libraries": list}))
}

fn add_folder(s: &mut Session, p: &Value) -> Result<Value> {
    let folder = str_p(p, "path").ok_or_else(|| bad("library.addFolder", "missing `path` (a folder with library.json)"))?.trim_end_matches('/').to_string();
    let lib = read_library(s, &folder).map_err(EngineError::Other)?;
    if !s.prefs.libraries.contains(&folder) {
        s.prefs.libraries.push(folder.clone());
        s.save_prefs();
    }
    Ok(json!({"folder": folder, "name": lib.name, "entries": lib.entries.len()}))
}

fn remove_folder(s: &mut Session, p: &Value) -> Result<Value> {
    let folder = str_p(p, "path").ok_or_else(|| bad("library.removeFolder", "missing `path`"))?.trim_end_matches('/').to_string();
    let before = s.prefs.libraries.len();
    s.prefs.libraries.retain(|f| f != &folder);
    let removed = s.prefs.libraries.len() != before;
    if removed {
        s.save_prefs();
    }
    Ok(json!({"removed": removed}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "library.search",
            "Search Libraries",
            "{query? (words; every word must match name, tags, category or description), kind?: comp|preset|footage|audio, category?, source?: all|motion|folders (default all: Motion Library cards first, then library folders), limit? (default 20, max 200), offset?} → {total, entries: [{id, name, kind, category, tags, description, duration, thumbnail, library, source?, needsFootage?}]} — Motion Library cards (source \"motion\") apply with library.apply {id}",
            search
        ),
        query!("library.get", "Library Entry", "{id} → the entry with its file, comp, preview and folder", get),
        cmd!(
            "library.insert",
            "Insert from Library",
            [],
            None,
            "{id, comp? (default: the open composition; none open = import only), time? (seconds), layers? (presets)} — comp: imports the .ecproj and nests the entry's composition; footage/audio: imports the file and adds a layer; preset: applies it to `layers` or the selected layers",
            always,
            insert
        ),
        query!("library.folders", "Library Folders", "{} → the configured library folders, with their entry counts or read errors", folders),
        cmd!("library.addFolder", "Add Library Folder", [], None, "{path (a folder with library.json)}", always, add_folder),
        cmd!("library.removeFolder", "Remove Library Folder", [], None, "{path}", always, remove_folder),
    ]
}
