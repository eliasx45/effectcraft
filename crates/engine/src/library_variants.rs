//! Library variants: refined copies of a library entry, each remembered with the situation it was
//! made for (a [`SituationCard`]), so agents pick the closest one later instead of learning global
//! timing numbers ("numbers depend on the layer size").
//!
//! A library folder keeps its variants in a sidecar beside `library.json`, which the converter
//! rewrites and the bucket publish reads; neither touches the sidecar:
//!
//! ```json
//! {"format": "effectcraft-library-variants", "version": 1,
//!  "variants": [{"id": "v18f3…", "parent": "e10fa…", "name": "DJ1 Trial Rings · phone",
//!                "file": "variants/e10fa…/v18f3….ecproj", "comp": "Main", "kind": "layers",
//!                "layers": ["Ring 1"], "card": {…}}]}
//! ```
//!
//! Matching is a weighted feature distance (shot, size in frame, aspect, emphasised word's
//! category, duration, energy): [`similarity`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Session;

/// The sidecar beside a library folder's `library.json`.
pub const SIDECAR: &str = "variants.json";
/// Variant projects, under the library folder: `variants/<parent>/<id>.ecproj`.
pub const FOLDER: &str = "variants";
const FORMAT: &str = "effectcraft-library-variants";
const VERSION: u32 = 1;

/// Limits on a card's text (it's a note, not a document).
const MAX_TEXT: usize = 4000;
const MAX_WORD: usize = 120;
const MAX_TAGS: usize = 32;
const MAX_TAG: usize = 80;
const MAX_NAME: usize = 200;

/// The kind of shot the variant sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Shot {
    TalkingHead,
    Phone,
    FullScreen,
    Overlay,
}

impl Shot {
    pub const NAMES: &'static str = "talkingHead, phone, fullScreen, overlay";
    /// "talking head", "talking-head", "talkingHead", "FULL_SCREEN"… → the shot.
    pub fn parse(s: &str) -> Option<Shot> {
        let k: String = s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        Some(match k.as_str() {
            "talkinghead" | "talking" | "head" | "presenter" | "th" => Shot::TalkingHead,
            "phone" | "mobile" | "screen" => Shot::Phone,
            "fullscreen" | "full" | "fs" => Shot::FullScreen,
            "overlay" => Shot::Overlay,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Shot::TalkingHead => "talking head",
            Shot::Phone => "phone",
            Shot::FullScreen => "full-screen",
            Shot::Overlay => "overlay",
        }
    }
}

/// The situation a variant was made for. Every field is optional: a card says what is known.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SituationCard {
    /// What was said over it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The emphasised word.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub word: String,
    /// The word's category ([`word_category`]); worked out from `word` when not given.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub word_category: String,
    /// Frames from the variant's start to the accent (the speed graph's peak).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_frame: Option<f64>,
    /// The layer's box in comp pixels: [x, y, width, height].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<[f64; 4]>,
    /// The comp's size in pixels: [width, height].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_size: Option<[f64; 2]>,
    /// Width / height of the frame (from `compSize` when not given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shot: Option<Shot>,
    /// Seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// The section's energy, 0 (calm) to 1 (hype).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<f64>,
    /// The ease graph used (a Devin Jatho ▸ Graphs card's name or id).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ease: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub style: String,
    /// Elias's "why" answers ("too fast", "wrong word", "too big", "not my style"…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Unix milliseconds.
    pub created: u64,
    pub updated: u64,
}

/// The fields a card takes (input checking names them).
pub const CARD_FIELDS: &[&str] = &[
    "text",
    "word",
    "wordCategory",
    "accentFrame",
    "layer",
    "compSize",
    "aspect",
    "shot",
    "duration",
    "energy",
    "ease",
    "client",
    "style",
    "why",
    "note",
    "created",
    "updated",
];

fn num(v: &Value, what: &str) -> Result<f64, String> {
    v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| format!("`{what}` is a number, not {v}"))
}

/// `[x, y, w, h]` or `{x|left, y|top, width|w, height|h}` → an array.
fn rect_value(v: &Value) -> Result<Value, String> {
    match v {
        Value::Array(a) if a.len() == 4 => Ok(json!([num(&a[0], "layer")?, num(&a[1], "layer")?, num(&a[2], "layer")?, num(&a[3], "layer")?])),
        Value::Object(o) => {
            let get = |keys: &[&str]| keys.iter().find_map(|k| o.get(*k)).ok_or_else(|| format!("`layer` needs {}", keys.join("|")));
            Ok(json!([
                num(get(&["x", "left"])?, "layer.x")?,
                num(get(&["y", "top"])?, "layer.y")?,
                num(get(&["width", "w"])?, "layer.width")?,
                num(get(&["height", "h"])?, "layer.height")?
            ]))
        }
        _ => Err(format!("`layer` is [x, y, width, height] in comp pixels, not {v}")),
    }
}

fn size_value(v: &Value) -> Result<Value, String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok(json!([num(&a[0], "compSize")?, num(&a[1], "compSize")?])),
        Value::Object(o) => {
            let w = o.get("width").or(o.get("w")).ok_or("`compSize` needs width")?;
            let h = o.get("height").or(o.get("h")).ok_or("`compSize` needs height")?;
            Ok(json!([num(w, "compSize.width")?, num(h, "compSize.height")?]))
        }
        _ => Err(format!("`compSize` is [width, height] in pixels, not {v}")),
    }
}

impl SituationCard {
    /// A card from agents' or the panel's JSON: unknown fields, wrong types and out-of-range
    /// values are refused with a message (never stored). `null` fields are left unset.
    pub fn from_input(v: &Value) -> Result<SituationCard, String> {
        let obj = match v {
            Value::Null => return Ok(SituationCard::default()),
            Value::Object(o) => o,
            other => return Err(format!("the situation card is an object ({{text, word, shot, layer, …}}), not {other}")),
        };
        let mut clean = serde_json::Map::new();
        for (k, v) in obj {
            if !CARD_FIELDS.contains(&k.as_str()) {
                return Err(format!("unknown card field `{k}` (fields: {})", CARD_FIELDS.join(", ")));
            }
            if v.is_null() {
                continue;
            }
            let v = match k.as_str() {
                "layer" => rect_value(v)?,
                "compSize" => size_value(v)?,
                "shot" => {
                    let s = v.as_str().ok_or_else(|| format!("`shot` is one of {}, not {v}", Shot::NAMES))?;
                    let shot = Shot::parse(s).ok_or_else(|| format!("unknown shot `{s}` ({})", Shot::NAMES))?;
                    serde_json::to_value(shot).map_err(|e| e.to_string())?
                }
                "energy" => match v {
                    Value::String(s) => json!(match s.to_lowercase().as_str() {
                        "low" | "calm" => 0.2,
                        "mid" | "medium" => 0.5,
                        "high" | "hype" => 0.85,
                        _ => return Err(format!("`energy` is 0–1 or low/medium/high, not \"{s}\"")),
                    }),
                    _ => json!(num(v, "energy")?),
                },
                "why" => match v {
                    Value::String(s) => json!(s.split(',').map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>()),
                    Value::Array(a) => {
                        json!(a.iter().map(|t| t.as_str().map(str::trim).ok_or(format!("`why` holds text tags, not {t}"))).collect::<Result<Vec<_>, _>>()?)
                    }
                    _ => return Err(format!("`why` is a list of tags, not {v}")),
                },
                "accentFrame" | "duration" | "aspect" => json!(num(v, k)?),
                "created" | "updated" => json!(v.as_u64().ok_or_else(|| format!("`{k}` is a time in Unix milliseconds"))?),
                _ => {
                    let s = v.as_str().ok_or_else(|| format!("`{k}` is text, not {v}"))?;
                    json!(s.trim())
                }
            };
            clean.insert(k.clone(), v);
        }
        let mut card: SituationCard = serde_json::from_value(Value::Object(clean)).map_err(|e| format!("situation card: {e}"))?;
        card.check()?;
        card.fill_derived();
        Ok(card)
    }

    /// Range checks (input and stored cards alike).
    pub fn check(&self) -> Result<(), String> {
        let long = |what: &str, s: &str, max: usize| if s.chars().count() > max { Err(format!("`{what}` is longer than {max} characters")) } else { Ok(()) };
        long("text", &self.text, MAX_TEXT)?;
        long("note", &self.note, MAX_TEXT)?;
        for (what, s) in [("word", &self.word), ("wordCategory", &self.word_category), ("ease", &self.ease), ("client", &self.client), ("style", &self.style)] {
            long(what, s, MAX_WORD)?;
        }
        if self.why.len() > MAX_TAGS {
            return Err(format!("`why` has more than {MAX_TAGS} tags"));
        }
        for t in &self.why {
            long("why", t, MAX_TAG)?;
        }
        if let Some([x, y, w, h]) = self.layer
            && (![x, y, w, h].iter().all(|v| v.is_finite() && v.abs() < 1e6) || w <= 0.0 || h <= 0.0)
        {
            return Err("`layer` needs a positive width and height in comp pixels".into());
        }
        if let Some([w, h]) = self.comp_size
            && !(w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 && w <= 1e5 && h <= 1e5)
        {
            return Err("`compSize` is [width, height], 1–100000 pixels".into());
        }
        if let Some(a) = self.aspect
            && !(a.is_finite() && (0.01..=100.0).contains(&a))
        {
            return Err("`aspect` is width / height, 0.01–100".into());
        }
        if let Some(d) = self.duration
            && !(d.is_finite() && d > 0.0 && d <= 86_400.0)
        {
            return Err("`duration` is seconds, more than 0".into());
        }
        if let Some(e) = self.energy
            && !(e.is_finite() && (0.0..=1.0).contains(&e))
        {
            return Err("`energy` is 0 (calm) to 1 (hype)".into());
        }
        if let Some(f) = self.accent_frame
            && !(f.is_finite() && f.abs() <= 1e6)
        {
            return Err("`accentFrame` is a frame offset from the variant's start".into());
        }
        Ok(())
    }

    /// The aspect from the comp size and the word's category from the word, when not given.
    pub fn fill_derived(&mut self) {
        if self.aspect.is_none()
            && let Some([w, h]) = self.comp_size
            && h > 0.0
        {
            self.aspect = Some(w / h);
        }
        if self.word_category.is_empty() && !self.word.is_empty() {
            self.word_category = word_category(&self.word).to_string();
        }
    }

    /// Fill what `other` knows and this card doesn't (agents' fields win over what the
    /// document says).
    pub fn fill_from(&mut self, other: &SituationCard) {
        if self.layer.is_none() {
            self.layer = other.layer;
        }
        if self.comp_size.is_none() {
            self.comp_size = other.comp_size;
        }
        if self.duration.is_none() {
            self.duration = other.duration;
        }
        self.fill_derived();
    }

    /// The layer's share of the frame (area / comp area).
    pub fn size_ratio(&self) -> Option<f64> {
        let [_, _, w, h] = self.layer?;
        let [cw, ch] = self.comp_size?;
        let r = (w * h) / (cw * ch);
        (r.is_finite() && r > 0.0).then_some(r)
    }

    /// One line for lists and the panel: "phone · 32% of frame · 9:16 · 1.2 s · energy 0.7 · “content”".
    pub fn summary(&self) -> String {
        let mut parts = vec![];
        if let Some(s) = self.shot {
            parts.push(s.label().to_string());
        }
        if let Some(r) = self.size_ratio() {
            parts.push(format!("{:.0}% of frame", r * 100.0));
        }
        if let Some(a) = self.aspect {
            parts.push(aspect_label(a));
        }
        if let Some(d) = self.duration {
            parts.push(format!("{d:.1} s"));
        }
        if let Some(e) = self.energy {
            parts.push(format!("energy {e:.1}"));
        }
        if !self.word.is_empty() {
            parts.push(format!("“{}”", self.word));
        }
        if !self.ease.is_empty() {
            parts.push(format!("ease {}", self.ease));
        }
        if !self.why.is_empty() {
            parts.push(format!("why: {}", self.why.join(", ")));
        }
        parts.join(" · ")
    }
}

fn aspect_label(a: f64) -> String {
    for (w, h) in [(16, 9), (9, 16), (1, 1), (4, 5), (4, 3), (3, 4), (21, 9)] {
        if (a - w as f64 / h as f64).abs() < 0.02 {
            return format!("{w}:{h}");
        }
    }
    format!("{a:.2}:1")
}

/// The emphasised word's category, for matching: number, money, negation, time, intensifier,
/// person or word.
pub fn word_category(word: &str) -> &'static str {
    let w = word.trim().to_lowercase();
    let bare: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect();
    if w.is_empty() {
        return "";
    }
    if w.contains(['$', '€', '£'])
        || ["money", "dollars", "dollar", "cash", "revenue", "profit", "paid", "price", "cost", "sales", "income", "rich", "pay"].contains(&bare.as_str())
    {
        return "money";
    }
    const NUMBERS: &[&str] = &[
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "twenty", "thirty", "fifty", "hundred", "thousand",
        "million", "billion", "half", "double", "twice", "triple", "first", "second", "third", "zero",
    ];
    if bare.chars().any(|c| c.is_ascii_digit()) || w.contains('%') || NUMBERS.contains(&bare.as_str()) {
        return "number";
    }
    if ["no", "not", "never", "nothing", "none", "nobody", "don't", "can't", "won't", "isn't", "stop", "without"].contains(&bare.as_str()) {
        return "negation";
    }
    const TIME: &[&str] = &[
        "today",
        "now",
        "tomorrow",
        "yesterday",
        "day",
        "days",
        "week",
        "weeks",
        "month",
        "months",
        "year",
        "years",
        "hours",
        "minutes",
        "daily",
        "overnight",
        "instantly",
    ];
    if TIME.contains(&bare.as_str()) {
        return "time";
    }
    const INTENSE: &[&str] = &[
        "very",
        "most",
        "best",
        "biggest",
        "every",
        "everything",
        "all",
        "only",
        "always",
        "huge",
        "massive",
        "insane",
        "crazy",
        "secret",
        "exactly",
        "literally",
        "worst",
        "fastest",
        "easiest",
        "free",
    ];
    if INTENSE.contains(&bare.as_str()) {
        return "intensifier";
    }
    if ["you", "your", "you're", "i", "me", "my", "we", "us", "our", "they"].contains(&bare.as_str()) {
        return "person";
    }
    "word"
}

/// A saved variant of a library entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Variant {
    pub id: String,
    /// The library entry it was refined from.
    pub parent: String,
    pub name: String,
    /// Its project, relative to the library folder.
    pub file: String,
    /// The comp in that project holding the layers (`layers`) or the whole variant (`comp`).
    pub comp: String,
    /// `layers` (layers to add to a comp) or `comp` (a precomp to nest).
    pub kind: String,
    /// The saved layers' names.
    pub layers: Vec<String>,
    pub card: SituationCard,
    /// The library folder keeping it (not stored).
    #[serde(skip)]
    pub folder: String,
}

impl Variant {
    /// The project's path on this machine.
    pub fn path(&self) -> String {
        resolve(&self.folder, &self.file)
    }
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "parent": self.parent,
            "name": self.name,
            "kind": self.kind,
            "file": self.path(),
            "comp": self.comp,
            "layers": self.layers,
            "card": self.card,
            "summary": self.card.summary(),
            "library": self.folder,
        })
    }
}

pub(crate) fn resolve(folder: &str, path: &str) -> String {
    if path.is_empty() || std::path::Path::new(path).is_absolute() {
        path.to_string()
    } else {
        std::path::Path::new(folder).join(path).to_string_lossy().to_string()
    }
}

pub(crate) fn sidecar_path(folder: &str) -> String {
    std::path::Path::new(folder).join(SIDECAR).to_string_lossy().to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Sidecar {
    format: String,
    version: u32,
    variants: Vec<Variant>,
}

/// A library folder's variants (none when it has no sidecar yet). A damaged sidecar is an error:
/// saving over it would lose Elias's variants.
pub fn read(s: &Session, folder: &str) -> Result<Vec<Variant>, String> {
    let path = sidecar_path(folder);
    if !s.services.exists(&path) {
        return Ok(vec![]);
    }
    let bytes = s.services.read_file(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let side: Sidecar = serde_json::from_slice(&bytes).map_err(|e| format!("{path} is damaged ({e}); fix or move it before saving variants"))?;
    if side.format != FORMAT {
        return Err(format!("{path} is not an EffectCraft variants file"));
    }
    if side.version > VERSION {
        return Err(format!("{path} was written by a newer EffectCraft (version {})", side.version));
    }
    Ok(side
        .variants
        .into_iter()
        .filter(|v| !v.id.is_empty() && !v.parent.is_empty() && v.card.check().is_ok())
        .map(|mut v| {
            v.folder = folder.to_string();
            v
        })
        .collect())
}

/// Write a library folder's variants (atomically on the desktop).
pub fn write(s: &Session, folder: &str, variants: &[Variant]) -> Result<(), String> {
    let path = sidecar_path(folder);
    let side = Sidecar { format: FORMAT.into(), version: VERSION, variants: variants.to_vec() };
    let text = serde_json::to_vec_pretty(&side).map_err(|e| e.to_string())?;
    s.services.write_file(&path, &text).map_err(|e| format!("cannot write {path}: {e}"))?;
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Bumped on every write, so the index below reads again even within one mtime tick.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Every configured library folder's variants, by parent entry id (oldest first).
#[derive(Debug, Default)]
pub struct Index {
    pub by_parent: BTreeMap<String, Vec<Variant>>,
    /// Folders whose sidecar doesn't read, with the reason.
    pub errors: Vec<(String, String)>,
}

impl Index {
    pub fn count(&self, parent: &str) -> usize {
        self.by_parent.get(parent).map_or(0, Vec::len)
    }
    pub fn all(&self) -> impl Iterator<Item = &Variant> {
        self.by_parent.values().flatten()
    }
    pub fn find(&self, id: &str) -> Option<&Variant> {
        self.all().find(|v| v.id == id)
    }
}

type IndexKey = (u64, Vec<(String, Option<(u64, u128)>)>);

/// The variants index, read again only when a sidecar changes (the panel asks every frame).
pub fn index(s: &Session) -> Arc<Index> {
    static CACHE: Mutex<Option<(IndexKey, Arc<Index>)>> = Mutex::new(None);
    let key: IndexKey = (GENERATION.load(Ordering::Relaxed), s.prefs.libraries.iter().map(|f| (f.clone(), s.services.stamp(&sidecar_path(f)))).collect());
    if let Ok(c) = CACHE.lock()
        && let Some((k, idx)) = c.as_ref()
        && *k == key
    {
        return idx.clone();
    }
    let mut idx = Index::default();
    for folder in &s.prefs.libraries {
        match read(s, folder) {
            Ok(list) => {
                for v in list {
                    // The first folder listing an id wins.
                    if idx.find(&v.id).is_none() {
                        idx.by_parent.entry(v.parent.clone()).or_default().push(v);
                    }
                }
            }
            Err(e) => idx.errors.push((folder.clone(), e)),
        }
    }
    for list in idx.by_parent.values_mut() {
        list.sort_by(|a, b| a.card.created.cmp(&b.card.created).then_with(|| a.id.cmp(&b.id)));
    }
    let idx = Arc::new(idx);
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((key, idx.clone()));
    }
    idx
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0)
}

// ---------------------------------------------------------------- matching

/// The features and their weights: what the shot is and how big the layer sits in the frame
/// matter most.
pub const WEIGHTS: &[(&str, f64)] = &[("shot", 3.0), ("size", 2.0), ("aspect", 2.0), ("wordCategory", 1.5), ("duration", 1.0), ("energy", 1.0)];

/// A feature counts as matched at this similarity or above.
pub const MATCHED: f64 = 0.75;

/// How alike two logarithmic quantities are: 1 equal, 0 at `span`× apart or more.
fn log_sim(a: f64, b: f64, span: f64) -> Option<f64> {
    (a > 0.0 && b > 0.0).then(|| 1.0 - ((a / b).ln().abs() / span.ln()).min(1.0))
}

/// The query's features: each one's similarity to the variant's (`None`: the variant doesn't say,
/// counted as 0.5). Features the query doesn't have are left out.
fn features(q: &SituationCard, v: &SituationCard) -> Vec<(&'static str, Option<f64>)> {
    let mut out = vec![];
    if let Some(qs) = q.shot {
        out.push(("shot", v.shot.map(|vs| if vs == qs { 1.0 } else { 0.0 })));
    }
    if let Some(qr) = q.size_ratio() {
        out.push(("size", v.size_ratio().and_then(|vr| log_sim(qr, vr, 4.0))));
    }
    if let Some(qa) = q.aspect {
        out.push(("aspect", v.aspect.and_then(|va| log_sim(qa, va, 2.5))));
    }
    if !q.word_category.is_empty() {
        out.push(("wordCategory", (!v.word_category.is_empty()).then(|| if v.word_category == q.word_category { 1.0 } else { 0.0 })));
    }
    if let Some(qd) = q.duration {
        out.push(("duration", v.duration.and_then(|vd| log_sim(qd, vd, 3.0))));
    }
    if let Some(qe) = q.energy {
        out.push(("energy", v.energy.map(|ve| 1.0 - (qe - ve).abs().min(1.0))));
    }
    out
}

/// How alike a situation and a variant's card are: a score 0–1 (weighted mean of the features'
/// similarities), the features that matched, and each feature's similarity.
pub fn similarity(query: &SituationCard, card: &SituationCard) -> (f64, Vec<&'static str>, BTreeMap<&'static str, Value>) {
    let mut total = 0.0;
    let mut weight = 0.0;
    let mut matched = vec![];
    let mut each = BTreeMap::new();
    for (name, sim) in features(query, card) {
        let w = WEIGHTS.iter().find(|(n, _)| *n == name).map_or(1.0, |(_, w)| *w);
        let s = sim.unwrap_or(0.5);
        total += w * s;
        weight += w;
        if sim.is_some_and(|s| s >= MATCHED) {
            matched.push(name);
        }
        each.insert(name, sim.map_or(Value::Null, |s| json!((s * 1000.0).round() / 1000.0)));
    }
    let score = if weight > 0.0 { total / weight } else { 0.0 };
    (score, matched, each)
}

/// Whether a situation has anything to match on.
pub fn has_features(q: &SituationCard) -> bool {
    !features(q, q).is_empty()
}

/// Rank `variants` for `query`, best first (ties: the newest first).
pub fn rank<'a>(
    query: &SituationCard,
    variants: impl Iterator<Item = &'a Variant>,
) -> Vec<(f64, Vec<&'static str>, BTreeMap<&'static str, Value>, &'a Variant)> {
    let mut out: Vec<_> = variants
        .map(|v| {
            let (score, matched, each) = similarity(query, &v.card);
            (score, matched, each, v)
        })
        .collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.3.card.updated.cmp(&a.3.card.updated)).then_with(|| a.3.id.cmp(&b.3.id)));
    out
}

/// A file-name-safe version of `s` (ids are used as folder names).
pub fn safe_name(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_alphanumeric() || "-_".contains(c) { c } else { '_' }).take(MAX_NAME).collect();
    if out.is_empty() { "entry".into() } else { out }
}

pub const NAME_LIMIT: usize = MAX_NAME;
