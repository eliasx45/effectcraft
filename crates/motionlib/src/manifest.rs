//! The catalog documents: [`Head`] (version and manifest location) and [`Manifest`] (the cards).

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::{is_hash, sha256_hex};

/// `format` of both documents.
pub const FORMAT: &str = "effectcraft-motion-library";
/// The key of the [`Head`] document in a remote and in the local store.
pub const HEAD_KEY: &str = "head.json";
/// Newest document version this build reads.
const SCHEMA: u32 = 1;
/// Largest manifest or project a client inflates (a bad blob can't exhaust memory).
const MAX_INFLATED: u64 = 512 * 1024 * 1024;

/// A file in the bucket, by content.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct BlobRef {
    /// SHA-256 of the file's bytes (before compression).
    pub hash: String,
    /// Size of the file's bytes (before compression).
    pub size: u64,
    /// Stored gzipped (text: projects, manifests).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gz: bool,
    /// The file's name (its extension tells the importer what it is).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl BlobRef {
    /// Where the blob is in the remote: `blobs/<hash>` (`.gz` when gzipped).
    pub fn remote_key(&self) -> String {
        format!("blobs/{}{}", self.hash, if self.gz { ".gz" } else { "" })
    }
    /// Where the blob is in a local store: `blobs/<hash>/<name>` (uncompressed).
    pub fn local_key(&self) -> String {
        format!("blobs/{}/{}", self.hash, safe_name(&self.name))
    }
    pub fn valid(&self) -> bool {
        is_hash(&self.hash)
    }
}

/// A file name that is safe as one path segment (keeps the extension readable).
pub(crate) fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' }).collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    let mut out: String = trimmed.chars().take(120).collect();
    if out.is_empty() {
        out = "blob".into();
    }
    out
}

/// A file a card's project uses that is uploaded with it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardFile {
    /// The path the project names (on the publishing Mac); replaced by the cached copy on apply.
    pub path: String,
    pub blob: BlobRef,
}

/// A variant of a card (Elias's refined copy of the entry, saved with `library.saveVariant` into
/// the library folder's `variants.json`), uploaded with its card.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardVariant {
    pub id: String,
    pub name: String,
    /// `layers` (layers to add to a comp) or `comp` (a precomp to nest).
    pub kind: String,
    /// The comp in its project holding the layers or the whole variant.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub comp: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// Its situation card, as `variants.json` keeps it.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub card: Value,
    /// The variant's `.ecproj`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<BlobRef>,
    /// Footage its project uses that is uploaded with it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<CardFile>,
}

/// Footage a card's project uses that was not uploaded (video, or over the size limit): the
/// project keeps it as a missing item with its name, size, duration and rate, to relink.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Placeholder {
    /// The path the project names.
    pub path: String,
    pub name: String,
    /// File size in bytes (0 = unknown or missing on the publishing Mac).
    pub size: u64,
    /// `video`, `too large` or `missing` (not found when publishing).
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
}

/// A rectangle of a thumbnail (pixels) and the size it was measured on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl Crop {
    /// The crop when it is usable (inside its source, not empty).
    pub fn valid(self) -> Option<Crop> {
        (self.width > 0
            && self.height > 0
            && self.source_width > 0
            && self.source_height > 0
            && self.x.saturating_add(self.width) <= self.source_width
            && self.y.saturating_add(self.height) <= self.source_height)
            .then_some(self)
    }

    /// The part of an image `w` × `h` this crop names: scaled when the image has the source's
    /// shape at another size, none when its shape differs.
    pub fn on(self, w: u32, h: u32) -> Option<[u32; 4]> {
        let c = self.valid()?;
        if w == 0 || h == 0 {
            return None;
        }
        let (sx, sy) = (w as f64 / c.source_width as f64, h as f64 / c.source_height as f64);
        if (sx / sy - 1.0).abs() > 0.02 {
            return None;
        }
        let x = ((c.x as f64 * sx).floor() as u32).min(w - 1);
        let y = ((c.y as f64 * sy).floor() as u32).min(h - 1);
        let cw = ((c.width as f64 * sx).ceil() as u32).clamp(1, w - x);
        let ch = ((c.height as f64 * sy).ceil() as u32).clamp(1, h - y);
        Some([x, y, cw, ch])
    }
}

/// One card of the catalog.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Card {
    pub id: String,
    pub name: String,
    /// Folder path, e.g. `Devin Jatho/UI`.
    pub category: String,
    /// `comp` (a project whose composition is nested) or `footage` (a file added as a layer).
    pub kind: String,
    /// What the card is, as the library labels it (`Comp`, `Overlay`, `Sound`…).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind_label: String,
    /// The folder the card's source is in (shown under its name); empty = its category.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// The composition to nest (`comp` cards); empty = the project's primary composition.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub comp: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub visual_caption: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub motion_events: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub generated_summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub motion_terms: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub priority: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<BlobRef>,
    /// The thumbnail's width and height in pixels (cards keep its shape before it downloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_size: Option<[u32; 2]>,
    /// The visible artwork inside a transparent thumbnail (and its hover frames): cards show
    /// this part, not the empty frame around it (the library's web panel's `thumbnail_crop`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<BlobRef>,
    /// The `.ecproj` (`comp`) or the file (`footage`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<BlobRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<CardFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub placeholders: Vec<Placeholder>,
    /// The entry's variants (`variants.json` beside `library.json`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<CardVariant>,
    /// Listed with its thumbnail only: applied in After Effects on the Mac that keeps the library.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub on_mac: bool,
    /// SHA-256 of everything above: a card whose revision changed is "changed".
    pub rev: String,
}

impl Card {
    /// Whether the card can be applied here (it has a file; not an on-the-Mac card).
    pub fn applicable(&self) -> bool {
        !self.on_mac && self.project.is_some()
    }
    /// Footage left out of the upload: applying the card imports it as missing footage.
    pub fn needs_footage(&self) -> bool {
        !self.placeholders.is_empty()
    }
    /// The revision of this card's contents ([`Card::rev`] excluded).
    pub fn compute_rev(&self) -> String {
        let mut c = self.clone();
        c.rev.clear();
        sha256_hex(&serde_json::to_vec(&c).unwrap_or_default())
    }
    /// Every blob applying needs (project and files), in order.
    pub fn apply_blobs(&self) -> Vec<&BlobRef> {
        self.project.iter().chain(self.files.iter().map(|f| &f.blob)).collect()
    }
    /// The top folder (`Devin Jatho` of `Devin Jatho/UI`).
    pub fn top_folder(&self) -> &str {
        self.category.split('/').next().unwrap_or_default()
    }
    /// The label under the card's name: `Comp · Devin Jatho/UI`.
    pub fn subtitle(&self) -> String {
        let kind = if !self.kind_label.is_empty() {
            self.kind_label.as_str()
        } else if self.kind == "comp" {
            "Comp"
        } else {
            "Footage"
        };
        format!("{kind} · {}", if self.location.is_empty() { &self.category } else { &self.location })
    }
    /// Whether the card is in `folder` or one of its subfolders.
    pub fn in_folder(&self, folder: &str) -> bool {
        folder.is_empty() || self.category == folder || self.category.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))
    }
}

/// A folder of the catalog as the library lists it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Collection {
    /// Folder path, e.g. `Devin Jatho/UI`.
    pub path: String,
    /// The folder's icon (a small image).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<BlobRef>,
    /// The group the folder is filed under in the library (empty = none).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
}

/// One row of the folder tree ([`Manifest::tree`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderRow {
    pub path: String,
    /// The last part of the path.
    pub name: String,
    /// 0 for a top folder.
    pub depth: usize,
    /// Cards in the folder and its subfolders.
    pub cards: usize,
    pub has_children: bool,
    pub icon: Option<BlobRef>,
}

/// The catalog.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub format: String,
    pub schema: u32,
    pub version: u64,
    pub name: String,
    /// The top folders published.
    pub folders: Vec<String>,
    /// Every folder in the library's order (parents before their subfolders), with its icon.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collections: Vec<Collection>,
    pub cards: Vec<Card>,
}

/// The catalog's version and manifest location.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Head {
    pub format: String,
    pub schema: u32,
    pub version: u64,
    pub manifest: BlobRef,
    /// Seconds since 1970 (UTC) of the publish.
    pub published: u64,
    pub cards: usize,
}

impl Head {
    pub fn parse(bytes: &[u8]) -> Result<Head, String> {
        let head: Head = serde_json::from_slice(bytes).map_err(|e| format!("library head: {e}"))?;
        if head.format != FORMAT {
            return Err("library head: not an EffectCraft Motion Library".into());
        }
        if head.schema > SCHEMA {
            return Err(format!("library head: written for a newer EffectCraft (schema {})", head.schema));
        }
        if !head.manifest.valid() {
            return Err("library head: bad manifest reference".into());
        }
        Ok(head)
    }
    pub fn new(version: u64, manifest: BlobRef, published: u64, cards: usize) -> Head {
        Head { format: FORMAT.into(), schema: SCHEMA, version, manifest, published, cards }
    }
}

/// What a newer catalog changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub new: usize,
    pub changed: usize,
    pub removed: usize,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.new + self.changed + self.removed == 0
    }
    /// "3 new · 2 changed" (only the non-zero parts).
    pub fn label(&self) -> String {
        let parts: Vec<String> =
            [(self.new, "new"), (self.changed, "changed"), (self.removed, "removed")].iter().filter(|(n, _)| *n > 0).map(|(n, w)| format!("{n} {w}")).collect();
        if parts.is_empty() { "no card changes".into() } else { parts.join(" · ") }
    }
}

impl Manifest {
    pub fn new(name: &str, folders: Vec<String>, cards: Vec<Card>, version: u64) -> Manifest {
        Manifest { format: FORMAT.into(), schema: SCHEMA, version, name: name.into(), folders, collections: vec![], cards }
    }

    /// Parse a manifest's (uncompressed) bytes, dropping cards without an id.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        let mut m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("library manifest: {e}"))?;
        if m.format != FORMAT {
            return Err("library manifest: not an EffectCraft Motion Library".into());
        }
        if m.schema > SCHEMA {
            return Err(format!("library manifest: written for a newer EffectCraft (schema {})", m.schema));
        }
        m.cards.retain(|c| !c.id.is_empty());
        Ok(m)
    }

    pub fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// What `newer` adds, changes and removes relative to this catalog.
    pub fn changes_to(&self, newer: &Manifest) -> Changes {
        let old: BTreeMap<&str, &str> = self.cards.iter().map(|c| (c.id.as_str(), c.rev.as_str())).collect();
        let new: BTreeMap<&str, &str> = newer.cards.iter().map(|c| (c.id.as_str(), c.rev.as_str())).collect();
        Changes {
            new: new.keys().filter(|id| !old.contains_key(*id)).count(),
            changed: new.iter().filter(|(id, rev)| old.get(*id).is_some_and(|o| o != *rev)).count(),
            removed: old.keys().filter(|id| !new.contains_key(*id)).count(),
        }
    }

    /// Every folder path with its card count (subfolders included in their parents), sorted.
    pub fn folder_counts(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for c in &self.cards {
            let mut path = String::new();
            for part in c.category.split('/').filter(|p| !p.is_empty()) {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(part);
                *counts.entry(path.clone()).or_default() += 1;
            }
        }
        counts.into_iter().collect()
    }

    /// The folder tree in the library's order: [`Manifest::collections`] first (as listed),
    /// folders it doesn't name after their siblings by name. Every folder with cards is in it, and
    /// listed folders without cards too (count 0).
    pub fn tree(&self) -> Vec<FolderRow> {
        let mut counts: BTreeMap<String, usize> = self.folder_counts().into_iter().collect();
        for c in &self.collections {
            let mut path = String::new();
            for part in c.path.split('/').filter(|p| !p.is_empty()) {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(part);
                counts.entry(path.clone()).or_default();
            }
        }
        let rank: BTreeMap<&str, usize> = self.collections.iter().enumerate().map(|(i, c)| (c.path.as_str(), i)).collect();
        let icons: BTreeMap<&str, &BlobRef> = self.collections.iter().filter_map(|c| Some((c.path.as_str(), c.icon.as_ref()?))).collect();
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for path in counts.keys() {
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            children.entry(parent).or_default().push(path.as_str());
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                let (ra, rb) = (rank.get(a).copied().unwrap_or(usize::MAX), rank.get(b).copied().unwrap_or(usize::MAX));
                ra.cmp(&rb).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())).then_with(|| a.cmp(b))
            });
        }
        let mut out = Vec::with_capacity(counts.len());
        // Depth-first, iteratively (folder depth comes from the catalog).
        let mut stack: Vec<&str> = children.get("").map(|c| c.iter().rev().copied().collect()).unwrap_or_default();
        while let Some(path) = stack.pop() {
            let kids = children.get(path);
            out.push(FolderRow {
                path: path.to_string(),
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                depth: path.matches('/').count(),
                cards: counts.get(path).copied().unwrap_or(0),
                has_children: kids.is_some_and(|k| !k.is_empty()),
                icon: icons.get(path).map(|b| (*b).clone()),
            });
            if let Some(kids) = kids {
                stack.extend(kids.iter().rev().copied());
            }
        }
        out
    }

    /// The rank of each folder in [`Manifest::tree`] (cards are grouped into sections in this order).
    pub fn folder_ranks(&self) -> BTreeMap<String, usize> {
        self.tree().into_iter().enumerate().map(|(i, r)| (r.path, i)).collect()
    }
}

/// Bytes as stored: gzipped when `gz`.
pub fn encode_blob(data: &[u8], gz: bool) -> Result<Vec<u8>, String> {
    if !gz {
        return Ok(data.to_vec());
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}

/// A stored blob back to its bytes, checked against its reference (hash and size).
pub fn decode_blob(blob: &BlobRef, stored: &[u8]) -> Result<Vec<u8>, String> {
    let data = if blob.gz {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(stored)
            .take(MAX_INFLATED.min(blob.size.saturating_add(1)))
            .read_to_end(&mut out)
            .map_err(|e| format!("{}: {e}", blob.hash))?;
        out
    } else {
        stored.to_vec()
    };
    if data.len() as u64 != blob.size || sha256_hex(&data) != blob.hash {
        return Err(format!("download of {} is damaged (hash or size differs)", if blob.name.is_empty() { &blob.hash } else { &blob.name }));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, rev: &str, cat: &str) -> Card {
        Card { id: id.into(), name: id.into(), category: cat.into(), rev: rev.into(), ..Default::default() }
    }

    #[test]
    fn changes_and_folders() {
        let a = Manifest::new("L", vec![], vec![card("a", "1", "Devin Jatho/UI"), card("b", "1", "SceneYard"), card("c", "1", "SceneYard")], 1);
        let b = Manifest::new("L", vec![], vec![card("a", "2", "Devin Jatho/UI"), card("b", "1", "SceneYard"), card("d", "1", "SceneYard")], 2);
        let ch = a.changes_to(&b);
        assert_eq!(ch, Changes { new: 1, changed: 1, removed: 1 });
        assert_eq!(ch.label(), "1 new · 1 changed · 1 removed");
        assert_eq!(a.folder_counts(), vec![("Devin Jatho".into(), 1), ("Devin Jatho/UI".into(), 1), ("SceneYard".into(), 2)]);
        assert!(a.cards[0].in_folder("Devin Jatho") && !a.cards[0].in_folder("Devin") && a.cards[0].in_folder(""));
    }

    #[test]
    fn tree_follows_the_library_order() {
        let mut m = Manifest::new(
            "L",
            vec![],
            vec![
                card("a", "1", "Devin Jatho/UI"),
                card("b", "1", "SceneYard"),
                card("c", "1", "Devin Jatho"),
                card("d", "1", "Devin Jatho/Basics"),
                card("e", "1", "Apps"),
            ],
            1,
        );
        let icon = BlobRef { hash: "0".repeat(64), size: 3, gz: false, name: "i.png".into() };
        m.collections = vec![
            Collection { path: "Tapa".into(), ..Default::default() },
            Collection { path: "Devin Jatho".into(), icon: Some(icon.clone()), ..Default::default() },
            Collection { path: "Devin Jatho/Basics".into(), ..Default::default() },
        ];
        let rows: Vec<(String, usize, usize, bool)> = m.tree().into_iter().map(|r| (r.path, r.depth, r.cards, r.has_children)).collect();
        assert_eq!(
            rows,
            vec![
                ("Tapa".into(), 0, 0, false),
                ("Devin Jatho".into(), 0, 3, true),
                ("Devin Jatho/Basics".into(), 1, 1, false),
                ("Devin Jatho/UI".into(), 1, 1, false),
                ("Apps".into(), 0, 1, false),
                ("SceneYard".into(), 0, 1, false),
            ]
        );
        assert_eq!(m.tree()[1].icon, Some(icon));
        assert_eq!(m.folder_ranks()["SceneYard"], 5);
        let c = Card { kind: "comp".into(), category: "Devin Jatho/UI".into(), ..Default::default() };
        assert_eq!(c.subtitle(), "Comp · Devin Jatho/UI");
        let c = Card { kind_label: "Overlay".into(), location: "Devin Jatho/Era elements".into(), ..c };
        assert_eq!(c.subtitle(), "Overlay · Devin Jatho/Era elements");
    }

    #[test]
    fn blobs_round_trip_and_reject_damage() {
        let data = b"{\"hello\": \"world\"}".repeat(50);
        let blob = BlobRef { hash: sha256_hex(&data), size: data.len() as u64, gz: true, name: "x.ecproj".into() };
        let stored = encode_blob(&data, true).unwrap();
        assert!(stored.len() < data.len());
        assert_eq!(decode_blob(&blob, &stored).unwrap(), data);
        assert!(decode_blob(&blob, b"not gzip").is_err());
        let raw = BlobRef { gz: false, ..blob.clone() };
        assert!(decode_blob(&raw, &data[1..]).is_err());
        assert_eq!(blob.remote_key(), format!("blobs/{}.gz", blob.hash));
        assert_eq!(blob.local_key(), format!("blobs/{}/x.ecproj", blob.hash));
    }

    #[test]
    fn bad_documents_are_errors() {
        assert!(Manifest::parse(b"nope").is_err());
        assert!(Manifest::parse(br#"{"format":"other"}"#).is_err());
        assert!(Manifest::parse(br#"{"format":"effectcraft-motion-library","schema":99}"#).is_err());
        assert!(Head::parse(br#"{"format":"effectcraft-motion-library","manifest":{"hash":"../x"}}"#).is_err());
        let m = Manifest::parse(br#"{"format":"effectcraft-motion-library","cards":[{"id":""},{"id":"a","bogus":1}]}"#).unwrap();
        assert_eq!(m.cards.len(), 1);
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("/Users/x/My clip.mov"), "My clip.mov");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(".."), "blob");
        assert_eq!(safe_name("a:b?.png"), "a_b_.png");
    }
}

#[cfg(test)]
mod crop_tests {
    use super::Crop;

    #[test]
    fn crops_scale_to_the_image_and_refuse_other_shapes() {
        let c = Crop { x: 100, y: 400, width: 880, height: 300, source_width: 1080, source_height: 1920 };
        assert_eq!(c.on(1080, 1920), Some([100, 400, 880, 300]));
        assert_eq!(c.on(540, 960), Some([50, 200, 440, 150]));
        assert_eq!(c.on(1920, 1080), None, "another shape");
        assert_eq!(Crop { x: 900, ..c }.valid(), None, "outside its source");
        assert_eq!(Crop::default().on(10, 10), None);
    }
}
