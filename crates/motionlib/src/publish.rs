//! Publishing (the Mac that manages the library): read a library folder's `library.json`, hash
//! every file the chosen folders' cards need, upload only blobs the remote doesn't have yet,
//! then the manifest and a new `head.json` with the version bumped.
//!
//! Big footage stays home: video (unless `include_video`) and files over `max_file_bytes` become
//! [`Placeholder`]s; the card's project still names them, so applying it imports them as missing
//! footage to relink.
//!
//! An entry's variants (`variants.json` beside `library.json`) go up with its card, each with its
//! project and footage, so a changed or new variant changes the card.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::sha256_hex;
use crate::manifest::{BlobRef, Card, CardFile, CardVariant, Collection, Crop, HEAD_KEY, Head, Manifest, Placeholder, decode_blob, encode_blob};
use crate::remote::{Remote, get_blocking};

/// Ticks per second (`effectcraft_time::Tick`), to report placeholder durations in seconds.
const TICKS_PER_SECOND: f64 = 254_016_000_000.0;
const TIMEOUT: Duration = Duration::from_secs(120);
/// The hash cache next to `library.json` (path, size and mtime → SHA-256).
const HASH_CACHE: &str = ".motion-library-hashes.json";
/// The variants sidecar next to `library.json` (written by EffectCraft's `library.saveVariant`).
const VARIANTS: &str = "variants.json";

/// Video containers left out unless `include_video`.
const VIDEO: &[&str] = &["mov", "mp4", "m4v", "mkv", "webm", "avi", "mxf", "mpg", "mpeg", "mts", "m2ts", "wmv", "flv", "3gp", "ogv", "r3d", "braw"];

#[derive(Clone, Debug)]
pub struct Options {
    /// The folder with `library.json` (e.g. `~/Documents/Elias Motion Library/effectcraft`).
    pub library: PathBuf,
    /// Folders to publish (a card is in when its category is one of them or below one).
    pub folders: Vec<String>,
    /// Files bigger than this stay home as placeholders.
    pub max_file_bytes: u64,
    /// Previews bigger than this are left out (the card shows its thumbnail on hover).
    pub max_preview_bytes: u64,
    /// Per folder (and its subfolders): files bigger than this stay home, instead of
    /// `max_file_bytes` (e.g. `SceneYard` = 5 MB keeps its big footage images home).
    pub folder_max_bytes: Vec<(String, u64)>,
    pub include_video: bool,
    /// List every other library entry too, with its thumbnail only ([`Card::on_mac`]): entries
    /// outside `folders`, and the index's `catalog_only` ones (what EffectCraft can't apply).
    pub catalog_all: bool,
    /// Entries published in full wherever they are (a card saved from the panel). Cards a
    /// previous publish had in full stay so ("saved" cards keep their files).
    pub ids: Vec<String>,
    /// Work everything out, upload nothing.
    pub dry_run: bool,
    /// Upload every blob again (a remote that lost files).
    pub force: bool,
}

impl Options {
    pub fn new(library: impl Into<PathBuf>, folders: Vec<String>) -> Options {
        Options {
            library: library.into(),
            folders,
            max_file_bytes: 90 * 1000 * 1000,
            max_preview_bytes: 90 * 1000 * 1000,
            folder_max_bytes: vec![],
            include_video: false,
            catalog_all: false,
            ids: vec![],
            dry_run: false,
            force: false,
        }
    }
}

/// A file left out of the upload, for the report.
#[derive(Clone, Debug, Serialize)]
pub struct Skipped {
    pub card: String,
    pub path: String,
    pub size: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub version: u64,
    pub previous: u64,
    /// Nothing changed: the version stays.
    pub unchanged: bool,
    #[serde(rename = "dryRun")]
    pub dry_run: bool,
    pub cards: usize,
    #[serde(rename = "needsFootage")]
    pub needs_footage: usize,
    /// Cards listed with their thumbnail only (applied on the Mac).
    #[serde(rename = "onMac")]
    pub on_mac: usize,
    /// Blobs uploaded (or to upload, dry run) and the bytes sent (gzipped blobs compressed).
    pub uploads: usize,
    #[serde(rename = "uploadBytes")]
    pub upload_bytes: u64,
    /// Every blob the catalog references and their bytes before compression.
    pub blobs: usize,
    #[serde(rename = "totalBytes")]
    pub total_bytes: u64,
    /// The cards changed relative to the previous catalog.
    pub new: usize,
    pub changed: usize,
    pub removed: usize,
    pub skipped: Vec<Skipped>,
    /// Entries that could not be published (missing project, unreadable file), with why.
    pub errors: Vec<String>,
    /// How each of [`Options::ids`] differs from its published copy.
    pub diffs: Vec<CardDiff>,
}

/// How a card differs from its published copy.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CardDiff {
    pub id: String,
    /// `new` (not in the catalog, or listed there with its thumbnail only), `changed` or `same`.
    pub status: String,
    /// The card fields that differ (`name`, `description`, `tags`, `project`, `variants`…).
    pub fields: Vec<String>,
    /// Variant names added, changed and removed.
    #[serde(rename = "variantsAdded")]
    pub variants_added: Vec<String>,
    #[serde(rename = "variantsChanged")]
    pub variants_changed: Vec<String>,
    #[serde(rename = "variantsRemoved")]
    pub variants_removed: Vec<String>,
}

/// How `now` differs from `old`, the card in the published catalog.
pub fn diff_card(old: Option<&Card>, now: &Card) -> CardDiff {
    let mut d = CardDiff { id: now.id.clone(), ..CardDiff::default() };
    let old = old.filter(|o| !o.on_mac || now.on_mac);
    let empty = Card::default();
    let fields = |c: &Card| match serde_json::to_value(c) {
        Ok(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    let (a, b) = (fields(old.unwrap_or(&empty)), fields(now));
    let mut keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.remove(&"rev".to_string());
    d.fields = keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect();
    let was: Vec<&CardVariant> = old.map(|o| o.variants.iter().collect()).unwrap_or_default();
    for v in &now.variants {
        match was.iter().find(|w| w.id == v.id) {
            None => d.variants_added.push(v.name.clone()),
            Some(w) if *w != v => d.variants_changed.push(v.name.clone()),
            Some(_) => {}
        }
    }
    d.variants_removed = was.iter().filter(|w| !now.variants.iter().any(|v| v.id == w.id)).map(|w| w.name.clone()).collect();
    d.status = match old {
        None => "new",
        Some(_) if d.fields.is_empty() => "same",
        Some(_) => "changed",
    }
    .into();
    d
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct HashEntry {
    size: u64,
    mtime: u128,
    hash: String,
}

/// Hashes of files by path, size and modification time: a re-publish only reads changed files.
struct Hasher {
    file: PathBuf,
    cache: BTreeMap<String, HashEntry>,
    dirty: bool,
}

impl Hasher {
    fn open(library: &Path) -> Hasher {
        let file = library.join(HASH_CACHE);
        let cache = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Hasher { file, cache, dirty: false }
    }
    fn stat(path: &Path) -> Option<(u64, u128)> {
        let m = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        Some((m.len(), t))
    }
    /// The file's hash and size (`None`: not a readable file).
    fn hash(&mut self, path: &Path) -> Option<(String, u64)> {
        let (size, mtime) = Self::stat(path)?;
        let k = path.to_string_lossy().to_string();
        if let Some(e) = self.cache.get(&k).filter(|e| e.size == size && e.mtime == mtime) {
            return Some((e.hash.clone(), size));
        }
        let hash = sha256_file(path)?;
        self.cache.insert(k, HashEntry { size, mtime, hash: hash.clone() });
        self.dirty = true;
        Some((hash, size))
    }
    fn save(&self) {
        if self.dirty
            && let Ok(b) = serde_json::to_vec(&self.cache)
        {
            let _ = crate::store::write_atomic(&self.file, &b);
        }
    }
}

fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n)?);
    }
    Some(crate::hash::hex(&h.finalize()))
}

fn ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Whether text files should be stored gzipped.
fn gz_for(path: &str) -> bool {
    matches!(ext(path).as_str(), "ecproj" | "json" | "svg" | "txt" | "jsx" | "csv")
}

/// Every file path a project names (`"path"` strings), with the footage item around it.
fn project_paths(v: &Value) -> Vec<(String, Option<&serde_json::Map<String, Value>>)> {
    fn walk<'a>(v: &'a Value, depth: usize, out: &mut Vec<(String, Option<&'a serde_json::Map<String, Value>>)>) {
        if depth > 256 {
            return;
        }
        match v {
            Value::Object(map) => {
                if let Some(Value::String(p)) = map.get("path")
                    && (p.starts_with('/') || p.get(1..3) == Some(":\\"))
                {
                    let footage = (map.get("type").and_then(Value::as_str) == Some("Footage")).then_some(map);
                    out.push((p.clone(), footage));
                }
                map.values().for_each(|x| walk(x, depth + 1, out));
            }
            Value::Array(items) => items.iter().for_each(|x| walk(x, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, 0, &mut out);
    let mut seen = BTreeSet::new();
    out.retain(|(p, _)| seen.insert(p.clone()));
    out
}

/// An image's width and height from its header (PNG, JPEG, GIF), reading at most 512 KB.
pub fn image_size(path: &Path) -> Option<[u32; 2]> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(512 * 1024).read_to_end(&mut head).ok()?;
    image_size_of(&head)
}

fn image_size_of(b: &[u8]) -> Option<[u32; 2]> {
    let be16 = |i: usize| Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let be32 = |i: usize| Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]));
    let size = |w: u32, h: u32| (w > 0 && h > 0).then_some([w, h]);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return size(be32(16)?, be32(20)?);
    }
    if b.starts_with(b"GIF8") {
        let le16 = |i: usize| Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
        return size(le16(6)?, le16(8)?);
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        // Walk the segments to the first start-of-frame (SOF0…SOF15 except DHT, JPG, DAC).
        let mut i = 2usize;
        while i + 4 <= b.len() {
            if *b.get(i)? != 0xFF {
                return None;
            }
            let marker = *b.get(i + 1)?;
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return size(be16(i + 7)?, be16(i + 5)?);
            }
            let len = be16(i + 2)? as usize;
            i = i.checked_add(2)?.checked_add(len)?;
        }
    }
    None
}

/// The library's folders under the published ones, in its order: saved positions first, then
/// (top folders) the library's priority list, then by name; parents before their subfolders.
fn collections(index: &Value, folders: &[String], cards: &[Card], b: &mut Builder) -> Vec<Collection> {
    let meta = index.get("collections");
    // `""` among `folders`: every folder.
    let published = |path: &str| folders.iter().any(|f| f.is_empty() || path == f || path.strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('/')));
    let mut all: BTreeSet<String> = BTreeSet::new();
    let listed = meta.and_then(|m| m.get("folders")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    for path in listed.map(str::to_string).chain(cards.iter().map(|c| c.category.clone())) {
        let mut acc = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if published(&acc) {
                all.insert(acc.clone());
            }
        }
    }
    let order = |path: &str| meta.and_then(|m| m.get("order")).and_then(|o| o.get(path)).and_then(Value::as_i64);
    let priorities: Vec<&str> =
        meta.and_then(|m| m.get("priorities")).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in &all {
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        children.entry(parent.to_string()).or_default().push(path.clone());
    }
    for (parent, list) in children.iter_mut() {
        list.sort_by(|a, b| {
            let key = |p: &str| {
                let pri = if parent.is_empty() { priorities.iter().position(|x| *x == p).unwrap_or(99) } else { 99 };
                (order(p).is_none(), order(p).unwrap_or(0), pri, p.to_lowercase())
            };
            key(a).cmp(&key(b)).then_with(|| a.cmp(b))
        });
    }
    let mut out = Vec::new();
    let mut stack: Vec<String> = children.get("").map(|c| c.iter().rev().cloned().collect()).unwrap_or_default();
    while let Some(path) = stack.pop() {
        let icon = meta.and_then(|m| m.get("icons")).and_then(|i| i.get(&path)).and_then(Value::as_str).filter(|p| !p.is_empty());
        // Icons are small images; a big file stays home (the folder shows the plain glyph).
        let icon = icon.map(|p| b.resolve(p)).filter(|p| Hasher::stat(p).is_some_and(|(size, _)| size <= 4 * 1000 * 1000)).and_then(|p| {
            let name = file_name(&p.to_string_lossy());
            b.blob(&p, &name)
        });
        let group = meta.and_then(|m| m.get("groups")).and_then(|g| g.get(&path)).and_then(Value::as_str).unwrap_or_default().to_string();
        if let Some(kids) = children.get(&path) {
            stack.extend(kids.iter().rev().cloned());
        }
        out.push(Collection { path, icon, group });
    }
    out
}

fn text(e: &Value, key: &str) -> String {
    e.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn strings(e: &Value, key: &str) -> Vec<String> {
    e.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(o) => o.get("description").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A blob to upload: its reference and where its bytes come from.
struct Upload {
    blob: BlobRef,
    source: PathBuf,
}

struct Builder<'a> {
    opts: &'a Options,
    hasher: Hasher,
    uploads: BTreeMap<String, Upload>,
    report: Report,
    /// `variants.json`'s variants by their entry's id.
    variants: BTreeMap<String, Vec<Value>>,
}

impl Builder<'_> {
    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { self.opts.library.join(p) }
    }

    /// A blob for the file at `path` (queued for upload); `None` when it can't be read.
    fn blob(&mut self, path: &Path, name: &str) -> Option<BlobRef> {
        let (hash, size) = self.hasher.hash(path)?;
        let blob = BlobRef { hash, size, gz: gz_for(name), name: name.to_string() };
        self.uploads.entry(blob.remote_key()).or_insert_with(|| Upload { blob: blob.clone(), source: path.to_path_buf() });
        Some(blob)
    }

    /// A card listed with its thumbnail only (no project, preview or files).
    fn mac_card(&mut self, e: &Value) -> Card {
        let mut card = Card {
            id: text(e, "id"),
            name: text(e, "name"),
            category: text(e, "category"),
            kind: "mac".into(),
            kind_label: text(e, "type_label"),
            location: text(e, "location"),
            crop: e.get("crop").and_then(|c| serde_json::from_value::<Crop>(c.clone()).ok()).and_then(Crop::valid),
            description: e.get("raw_description").and_then(Value::as_str).map_or_else(|| text(e, "description"), str::to_string),
            tags: strings(e, "tags"),
            aliases: strings(e, "aliases"),
            visual_caption: text(e, "visual_caption"),
            motion_events: strings(e, "motion_events"),
            summary: text(e, "summary"),
            generated_summary: text(e, "generated_summary"),
            motion_terms: text(e, "motion_terms"),
            priority: e.get("priority").and_then(Value::as_bool).unwrap_or(false),
            duration: e.get("duration").and_then(Value::as_f64).filter(|d| d.is_finite()),
            on_mac: true,
            ..Card::default()
        };
        let p = text(e, "thumbnail");
        if !p.is_empty() {
            let path = self.resolve(&p);
            if Hasher::stat(&path).is_some_and(|(size, _)| size <= self.opts.max_preview_bytes.min(self.opts.max_file_bytes)) {
                card.thumbnail = self.blob(&path, &file_name(&p));
                card.thumb_size = card.thumbnail.as_ref().and_then(|_| image_size(&path));
            }
        }
        card.rev = card.compute_rev();
        card
    }

    fn card(&mut self, e: &Value) -> Result<Card, String> {
        let id = text(e, "id");
        let name = text(e, "name");
        let kind = text(e, "kind");
        let file = text(e, "file");
        if file.is_empty() {
            return Err(format!("{name}: no file"));
        }
        let project_path = self.resolve(&file);
        let mut card = Card {
            id: id.clone(),
            name: name.clone(),
            category: text(e, "category"),
            kind: kind.clone(),
            kind_label: text(e, "type_label"),
            location: text(e, "location"),
            crop: e.get("crop").and_then(|c| serde_json::from_value::<Crop>(c.clone()).ok()).and_then(Crop::valid),
            comp: text(e, "comp"),
            // The library's own description when the index carries it (search parity with
            // retrieval.py); else the index's display text.
            description: e.get("raw_description").and_then(Value::as_str).map_or_else(|| text(e, "description"), str::to_string),
            tags: strings(e, "tags"),
            aliases: strings(e, "aliases"),
            visual_caption: text(e, "visual_caption"),
            motion_events: strings(e, "motion_events"),
            summary: text(e, "summary"),
            generated_summary: text(e, "generated_summary"),
            motion_terms: text(e, "motion_terms"),
            priority: e.get("priority").and_then(Value::as_bool).unwrap_or(false),
            duration: e.get("duration").and_then(Value::as_f64).filter(|d| d.is_finite()),
            ..Card::default()
        };
        let category = text(e, "category");
        let max = self
            .opts
            .folder_max_bytes
            .iter()
            .filter(|(f, _)| category == *f || category.strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('/')))
            .map(|(_, m)| *m)
            .min()
            .unwrap_or(self.opts.max_file_bytes);
        for (key, slot) in [("thumbnail", 0), ("preview", 1)] {
            let p = text(e, key);
            if p.is_empty() {
                continue;
            }
            let path = self.resolve(&p);
            let limit = if slot == 1 { self.opts.max_preview_bytes.min(self.opts.max_file_bytes) } else { self.opts.max_file_bytes };
            if Hasher::stat(&path).is_some_and(|(size, _)| size <= limit) {
                let b = self.blob(&path, &file_name(&p));
                if slot == 0 {
                    card.thumb_size = b.as_ref().and_then(|_| image_size(&path));
                    card.thumbnail = b;
                } else {
                    card.preview = b;
                }
            }
        }
        let project_name = file_name(&file);
        card.project = Some(self.blob(&project_path, &project_name).ok_or_else(|| format!("{name}: cannot read {}", project_path.display()))?);
        if kind == "comp" {
            (card.files, card.placeholders) = self.footage(&project_path, &name, max)?;
        }
        // A broken variant is reported and left out; the card itself still goes up.
        for v in self.variants.get(&id).cloned().unwrap_or_default() {
            match self.variant(&v, &name, max) {
                Ok(v) => card.variants.push(v),
                Err(e) => self.report.errors.push(e),
            }
        }
        card.rev = card.compute_rev();
        Ok(card)
    }

    /// The footage a comp project uses: uploaded files, and placeholders for what stays home.
    fn footage(&mut self, project_path: &Path, name: &str, max: u64) -> Result<(Vec<CardFile>, Vec<Placeholder>), String> {
        let bytes = std::fs::read(project_path).map_err(|e| format!("{name}: {}: {e}", project_path.display()))?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{name}: {}: {e}", project_path.display()))?;
        let (mut files, mut placeholders) = (Vec::new(), Vec::new());
        for (path, footage) in project_paths(&v) {
            let stat = Hasher::stat(Path::new(&path));
            let size = stat.map_or(0, |s| s.0);
            let reason = match stat {
                None => Some("missing"),
                Some(_) if !self.opts.include_video && VIDEO.contains(&ext(&path).as_str()) => Some("video"),
                Some((size, _)) if size > max => Some("too large"),
                Some(_) => None,
            };
            let blob = if reason.is_none() { self.blob(Path::new(&path), &file_name(&path)) } else { None };
            match blob {
                Some(blob) => files.push(CardFile { path, blob }),
                None => {
                    let reason = reason.unwrap_or("unreadable").to_string();
                    let num = |k: &str| footage.and_then(|f| f.get(k)).and_then(Value::as_f64);
                    let rate = footage.and_then(|f| f.get("frame_rate")).and_then(|r| Some(r.get("num")?.as_f64()? / r.get("den")?.as_f64()?.max(1.0)));
                    self.report.skipped.push(Skipped { card: name.to_string(), path: path.clone(), size, reason: reason.clone() });
                    placeholders.push(Placeholder {
                        name: file_name(&path),
                        path,
                        size,
                        reason,
                        width: num("width").map(|w| w as u32).filter(|w| *w > 1),
                        height: num("height").map(|h| h as u32).filter(|h| *h > 1),
                        duration: num("duration").map(|d| d / TICKS_PER_SECOND),
                        fps: rate.filter(|r| r.is_finite() && *r > 0.0),
                    });
                }
            }
        }
        Ok((files, placeholders))
    }

    /// One of `variants.json`'s variants of the card `name`: its project and footage.
    fn variant(&mut self, v: &Value, name: &str, max: u64) -> Result<CardVariant, String> {
        let file = text(v, "file");
        let vname = text(v, "name");
        let path = self.resolve(&file);
        let project = self.blob(&path, &file_name(&file)).ok_or_else(|| format!("{name}: variant “{vname}”: cannot read {}", path.display()))?;
        let (files, _) = self.footage(&path, &format!("{name} · {vname}"), max)?;
        Ok(CardVariant {
            id: text(v, "id"),
            name: vname,
            kind: text(v, "kind"),
            comp: text(v, "comp"),
            layers: strings(v, "layers"),
            card: v.get("card").cloned().unwrap_or(Value::Null),
            project: Some(project),
            files,
        })
    }
}

/// The variants in `variants.json` beside the library's `library.json`, by their entry's id (none
/// when there is no sidecar; a damaged one is an error rather than a catalog without them).
fn read_variants(library: &Path) -> Result<BTreeMap<String, Vec<Value>>, String> {
    let path = library.join(VARIANTS);
    let Ok(bytes) = std::fs::read(&path) else { return Ok(BTreeMap::new()) };
    let side: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut by_parent: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for v in side.get("variants").and_then(Value::as_array).into_iter().flatten() {
        let parent = text(v, "parent");
        if !parent.is_empty() && !text(v, "id").is_empty() && !text(v, "file").is_empty() {
            by_parent.entry(parent).or_default().push(v.clone());
        }
    }
    Ok(by_parent)
}

/// The catalog published at `remote` now (head and manifest), if any.
pub fn current(remote: &dyn Remote) -> Result<Option<(Head, Manifest)>, String> {
    let Some(raw) = get_blocking(remote, HEAD_KEY, TIMEOUT)? else { return Ok(None) };
    let head = Head::parse(&raw)?;
    let stored = get_blocking(remote, &head.manifest.remote_key(), TIMEOUT)?.ok_or("the published head names a manifest that isn't there")?;
    let m = Manifest::parse(&decode_blob(&head.manifest, &stored)?)?;
    Ok(Some((head, m)))
}

/// Publish `opts.folders` of the library at `opts.library` to `remote`. `progress` gets a line
/// now and then (hashing, uploads).
pub fn publish(remote: &dyn Remote, opts: &Options, progress: &mut dyn FnMut(String)) -> Result<Report, String> {
    let index_path = opts.library.join("library.json");
    let index: Value = serde_json::from_slice(&std::fs::read(&index_path).map_err(|e| format!("{}: {e}", index_path.display()))?)
        .map_err(|e| format!("{}: {e}", index_path.display()))?;
    let lib_name = index.get("name").and_then(Value::as_str).unwrap_or("Motion Library").to_string();
    let folders: Vec<String> = opts.folders.iter().map(|f| f.trim().trim_matches('/').to_string()).filter(|f| !f.is_empty()).collect();
    if folders.is_empty() {
        return Err("no folders to publish".into());
    }
    let listed = |key: &str| index.get(key).and_then(Value::as_array).map(|a| a.iter().collect::<Vec<_>>()).unwrap_or_default();
    let previous = current(remote)?;
    // Saved cards: asked for now, or published in full before (outside the folders too).
    let saved: BTreeSet<String> = opts
        .ids
        .iter()
        .cloned()
        .chain(previous.iter().flat_map(|(_, m)| m.cards.iter().filter(|c| !c.on_mac && c.project.is_some()).map(|c| c.id.clone())))
        .collect();
    let in_folders = |e: &Value| {
        let cat = text(e, "category");
        saved.contains(text(e, "id").as_str()) || folders.iter().any(|f| cat == *f || cat.strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('/')))
    };
    let applicable = |e: &Value| matches!(e.get("kind").and_then(Value::as_str), Some("comp" | "footage"));
    let entries: Vec<&Value> = listed("entries").into_iter().filter(|e| applicable(e) && in_folders(e)).collect();
    // Everything else, thumbnails only (applied on the Mac).
    let mac_entries: Vec<&Value> = if opts.catalog_all {
        listed("entries")
            .into_iter()
            .filter(|e| !(applicable(e) && in_folders(e)))
            .chain(listed("catalog_only"))
            .filter(|e| !text(e, "id").is_empty())
            .collect()
    } else {
        vec![]
    };
    progress(format!(
        "{} cards in {}{}",
        entries.len(),
        folders.join(", "),
        if opts.catalog_all { format!(", {} listed only", mac_entries.len()) } else { String::new() }
    ));

    let mut b = Builder {
        opts,
        hasher: Hasher::open(&opts.library),
        uploads: BTreeMap::new(),
        report: Report { dry_run: opts.dry_run, ..Report::default() },
        variants: read_variants(&opts.library)?,
    };
    let mut cards = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if i % 50 == 0 && i > 0 {
            progress(format!("read {i} of {} cards", entries.len()));
        }
        match b.card(e) {
            Ok(c) => cards.push(c),
            Err(err) => b.report.errors.push(err),
        }
    }
    for e in &mac_entries {
        cards.push(b.mac_card(e));
    }
    let mut seen = BTreeSet::new();
    cards.retain(|c| seen.insert(c.id.clone()));
    let shown: Vec<String> = if opts.catalog_all { vec![String::new()] } else { folders.clone() };
    let collections = collections(&index, &shown, &cards, &mut b);
    b.hasher.save();

    let prev_version = previous.as_ref().map_or(0, |(h, _)| h.version);
    let mut report = std::mem::take(&mut b.report);
    report.previous = prev_version;
    report.cards = cards.len();
    report.needs_footage = cards.iter().filter(|c| c.needs_footage()).count();
    report.on_mac = cards.iter().filter(|c| c.on_mac).count();
    let prev_cards: Vec<(String, String)> = previous.as_ref().map(|(_, m)| m.cards.iter().map(|c| (c.id.clone(), c.rev.clone())).collect()).unwrap_or_default();
    let now_cards: Vec<(String, String)> = cards.iter().map(|c| (c.id.clone(), c.rev.clone())).collect();
    let same_folders = previous.as_ref().is_some_and(|(_, m)| m.folders == folders && m.name == lib_name && m.collections == collections);
    report.unchanged = previous.is_some() && prev_cards == now_cards && same_folders && !opts.force;
    let version = if report.unchanged { prev_version } else { prev_version + 1 };
    report.version = version;
    report.diffs =
        opts.ids.iter().filter_map(|id| cards.iter().find(|c| c.id == *id)).map(|c| diff_card(previous.as_ref().and_then(|(_, m)| m.card(&c.id)), c)).collect();
    let mut manifest = Manifest::new(&lib_name, folders, cards, version);
    manifest.collections = collections;
    if let Some((_, m)) = &previous {
        let ch = m.changes_to(&manifest);
        (report.new, report.changed, report.removed) = (ch.new, ch.changed, ch.removed);
    } else {
        report.new = manifest.cards.len();
    }

    // What the remote has: every blob the published catalog references.
    let have: BTreeSet<String> = previous
        .as_ref()
        .map(|(h, m)| {
            m.cards
                .iter()
                .flat_map(|c| {
                    c.thumbnail
                        .iter()
                        .chain(c.preview.iter())
                        .chain(c.project.iter())
                        .chain(c.files.iter().map(|f| &f.blob))
                        .chain(c.variants.iter().flat_map(|v| v.project.iter().chain(v.files.iter().map(|f| &f.blob))))
                })
                .chain(m.collections.iter().filter_map(|c| c.icon.as_ref()))
                .map(BlobRef::remote_key)
                .chain(std::iter::once(h.manifest.remote_key()))
                .collect()
        })
        .unwrap_or_default();
    report.blobs = b.uploads.len();
    let mut todo: Vec<&Upload> = b.uploads.values().filter(|u| opts.force || !have.contains(&u.blob.remote_key())).collect();
    // Blobs an interrupted publish already sent (not in the published catalog, but there).
    if !opts.force && !todo.is_empty() {
        progress(format!("checking which of {} blobs the remote already has", todo.len()));
        let mut keep = Vec::with_capacity(todo.len());
        for u in todo {
            if !remote.exists(&u.blob.remote_key())? {
                keep.push(u);
            }
        }
        todo = keep;
    }
    report.uploads = todo.len();
    for u in b.uploads.values() {
        report.total_bytes += u.blob.size;
    }
    // What goes over the wire: gzipped blobs at their compressed size.
    for u in &todo {
        report.upload_bytes += if u.blob.gz {
            std::fs::read(&u.source).ok().and_then(|d| encode_blob(&d, true).ok()).map_or(u.blob.size, |z| z.len() as u64)
        } else {
            u.blob.size
        };
    }
    if opts.dry_run || report.unchanged {
        return Ok(report);
    }
    let mut sent = 0u64;
    for (i, u) in todo.iter().enumerate() {
        let data = std::fs::read(&u.source).map_err(|e| format!("{}: {e}", u.source.display()))?;
        if sha256_hex(&data) != u.blob.hash {
            return Err(format!("{} changed while publishing; publish again", u.source.display()));
        }
        let stored = encode_blob(&data, u.blob.gz)?;
        remote.put(&u.blob.remote_key(), &stored)?;
        sent += u.blob.size;
        if i % 25 == 0 || i + 1 == todo.len() {
            progress(format!("uploaded {} of {} ({:.1} of {:.1} MB)", i + 1, todo.len(), sent as f64 / 1e6, report.upload_bytes as f64 / 1e6));
        }
    }
    // The manifest, then the head that points at it (clients never see a half-published catalog).
    let raw = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    let mblob = BlobRef { hash: sha256_hex(&raw), size: raw.len() as u64, gz: true, name: String::new() };
    remote.put(&mblob.remote_key(), &encode_blob(&raw, true)?)?;
    let published = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let head = Head::new(version, mblob, published, manifest.cards.len());
    remote.put(HEAD_KEY, &serde_json::to_vec_pretty(&head).map_err(|e| e.to_string())?)?;
    progress(format!("published version {version}: {} cards", manifest.cards.len()));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_sizes_from_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&360u32.to_be_bytes());
        assert_eq!(image_size_of(&png), Some([640, 360]));
        // SOI, an APP0 segment, then SOF0 (precision, height, width).
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0x01, 0x68, 0x02, 0x80, 3];
        assert_eq!(image_size_of(&jpeg), Some([640, 360]));
        assert_eq!(image_size_of(b"GIF89a\x10\x00\x20\x00"), Some([16, 32]));
        assert_eq!(image_size_of(b"nope"), None);
        assert_eq!(image_size_of(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]), None);
        assert_eq!(image_size_of(&png[..20]), None);
    }

    #[test]
    fn collections_in_library_order() {
        let dir = std::env::temp_dir().join(format!("ml-collections-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("icon.png"), b"png").unwrap();
        let opts = Options::new(&dir, vec!["Devin Jatho".into(), "SceneYard".into()]);
        let mut b = Builder { opts: &opts, hasher: Hasher::open(&dir), uploads: BTreeMap::new(), report: Report::default(), variants: BTreeMap::new() };
        let index = json!({"collections": {
            "order": {"Devin Jatho": 2, "Devin Jatho/Screen Setups": 0, "Devin Jatho/Basics": 1, "Tapa": 0},
            "icons": {"Devin Jatho": "icon.png", "SceneYard": "missing.png"},
            "groups": {"SceneYard": "Stock"},
            "folders": ["Tapa", "Devin Jatho/Icons", "Devin Jatho/Basics"],
            "priorities": ["Devin Jatho"],
        }});
        let card = |cat: &str| Card { id: cat.into(), category: cat.into(), ..Default::default() };
        let cards = [card("SceneYard"), card("Devin Jatho/Screen Setups"), card("Devin Jatho/Era elements"), card("Devin Jatho")];
        let folders = ["Devin Jatho".to_string(), "SceneYard".to_string()];
        let got = collections(&index, &folders, &cards, &mut b);
        let paths: Vec<&str> = got.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["Devin Jatho", "Devin Jatho/Screen Setups", "Devin Jatho/Basics", "Devin Jatho/Era elements", "Devin Jatho/Icons", "SceneYard"]);
        assert_eq!(got[0].icon.as_ref().map(|i| i.name.as_str()), Some("icon.png"));
        assert!(got[5].icon.is_none());
        assert_eq!(got[5].group, "Stock");
        assert_eq!(b.uploads.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn variants_go_up_with_their_card_and_text_edits_show_in_the_diff() {
        let root = std::env::temp_dir().join(format!("ml-variants-{}", std::process::id()));
        let (lib, bucket) = (root.join("lib"), root.join("bucket"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(lib.join("variants/a")).unwrap();
        std::fs::write(lib.join("a.png"), b"png").unwrap();
        std::fs::write(lib.join("variants/a/v1.ecproj"), b"{}").unwrap();
        let index =
            |desc: &str| json!({"entries": [{"id": "a", "name": "A", "category": "F", "kind": "footage", "file": "a.png", "description": desc}]}).to_string();
        std::fs::write(lib.join("library.json"), index("one")).unwrap();
        let variant = |id: &str| json!({"id": id, "parent": "a", "name": format!("A {id}"), "file": format!("variants/a/{id}.ecproj"), "kind": "comp", "comp": "C", "card": {"word": "go"}});
        std::fs::write(lib.join("variants.json"), json!({"format": "effectcraft-library-variants", "version": 1, "variants": [variant("v1")]}).to_string())
            .unwrap();
        let remote = crate::DirRemote::new(&bucket);
        let mut opts = Options::new(&lib, vec!["F".into()]);
        opts.ids = vec!["a".into()];
        let first = publish(&remote, &opts, &mut |_| {}).unwrap();
        assert_eq!(first.diffs[0].status, "new");
        assert_eq!(first.diffs[0].variants_added, ["A v1"]);
        let (_, m) = current(&remote).unwrap().unwrap();
        let card = m.card("a").unwrap();
        assert_eq!(card.variants.len(), 1);
        assert!(card.variants[0].project.is_some());
        assert_eq!(card.variants[0].card["word"], "go");

        // A text-only edit: nothing to upload, but the card changed.
        std::fs::write(lib.join("library.json"), index("two")).unwrap();
        opts.dry_run = true;
        let dry = publish(&remote, &opts, &mut |_| {}).unwrap();
        assert_eq!(dry.uploads, 0);
        assert_eq!(dry.diffs[0].status, "changed");
        assert_eq!(dry.diffs[0].fields, ["description"]);

        // A new variant: its project is the upload.
        std::fs::write(lib.join("variants/a/v2.ecproj"), b"{\"v\": 2}").unwrap();
        std::fs::write(
            lib.join("variants.json"),
            json!({"format": "effectcraft-library-variants", "version": 1, "variants": [variant("v1"), variant("v2")]}).to_string(),
        )
        .unwrap();
        let dry = publish(&remote, &opts, &mut |_| {}).unwrap();
        assert_eq!(dry.uploads, 1);
        assert_eq!(dry.diffs[0].variants_added, ["A v2"]);
        assert!(dry.diffs[0].fields.contains(&"variants".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
