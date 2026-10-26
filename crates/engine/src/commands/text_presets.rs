//! Text presets (batch 1b DX1; reworked as CX8c on 19 Oct 2026): Elias's text animations are his
//! own Motion Library preset cards, so changing them (new controls such as "Blur In (px)") never
//! needs code. Nothing about a preset is built in here.
//!
//! - **Where:** the `preset` cards (an `.ecpreset` from `anim.savePreset`, now with whole text
//!   animators) in the Motion Library category "Elias/Text animations" (`category` overrides it),
//!   found through the library code: this Mac's library folders (`library.json`, the
//!   `LocalEntry` index: kind `preset`, `file` = `assets/<id>/preset.ecpreset`), then the
//!   catalog's cards (downloaded when applied). `ml.saveTextPreset` saves one from the workshop
//!   comp. A `.ecpreset` path works too.
//! - `text.presetFamilies {category?}` lists them with the Expression Controls each one carries
//!   (name, type, value, dropdown items: read from the file) and its text animators.
//! - `text.apply {preset, layers?|words?, controls?, distance?, sync?, glow?, word?, at?}` applies
//!   one through `anim.applyPreset` (same-named effects and animators replaced, keys from the
//!   layer's in point), then sets the controls named in `controls` by their exact names
//!   (`{"Travel Distance (px)": 40, "Animate By": "Words"}`; an unknown name is an error listing
//!   the preset's controls). `distance` (the old near / mid / far chips) sets every control whose
//!   name has "Travel" to 40 / 90 / 150 px, and does nothing when the preset has none. `words`
//!   makes a new text layer for a phrase of the speech map. `sync` shifts the preset's keys so the
//!   stressed syllable of `word` (else the first stressed word) lands on the reveal: when the
//!   preset has a "Fade Up" animator with keyed Range Selector Offset, the time that Offset
//!   half-reveals the word's unit (on the clock of the layer's "In" marker when the Offset's
//!   expression plays the keys over it, and that marker moves with the keys; "Out" stays); else
//!   the speed peak of its main move. `glow` also applies the
//!   Motion Library style "Caption glow + double shadow". One undo step.

use effectcraft_keyframe::Value as KValue;
use effectcraft_project::{GroupKind, ItemId, Layer, LayerId, LayerSource, Node, ParamUi, PropGroup};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, has_comp, layers_p, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd, lint, query};

const C: &str = "text.apply";
/// Elias's workshop comp.
pub const WORKSHOP: &str = "Creating Text Presets";
/// The Motion Library category of the text animations (`styles::TEXT_PRESET_FOLDER`).
pub const TEXT_CATEGORY: &str = "Elias/Text animations";
/// The card the Bounce button looks for first (else a card with a "Bounce Overshoot (%)" control).
pub const BOUNCE_CARD: &str = "Bounce In";
/// The control that marks a bounce preset when no card is named [`BOUNCE_CARD`].
pub const BOUNCE_OVERSHOOT: &str = "Bounce Overshoot (%)";
/// The Motion Library text style `glow` applies.
pub const GLOW_STYLE: &str = "Caption glow + double shadow";
/// The old near / mid / far chips, as travel in pixels (Elias, 19 Oct).
pub const TRAVEL: [(&str, f64); 3] = [("near", 40.0), ("mid", 90.0), ("far", 150.0)];
/// The animator whose keyed Range Selector Offset `sync` aims (and its copies, "Fade Up · …").
pub const REVEAL_ANIMATOR: &str = "Fade Up";
/// The layer marker (by comment) that times Elias's reveal: its expression reads
/// `thisLayer.marker.key("In")`; `sync` moves it with the keys (DX1b). The exit's "Out" marker
/// stays where `anim.applyPreset` put it (before the out point).
pub const IN_MARKER: &str = "In";
/// Where in a unit's reveal (0 = it starts to appear, 1 = fully in) the sync puts the accent.
// TODO(batch1-verify): half-revealed is a guess for "the peak" of a Ramp Up reveal with Ease
// High −100; check the frame with speech.accents / the off-accent lint and tune.
const UNIT_PEAK: f64 = 0.5;
/// Most layers one call animates.
const MAX_LAYERS: usize = 256;
/// Most characters of a new layer's text.
const MAX_TEXT: usize = 500;
/// Most preset cards listed.
const MAX_PRESETS: usize = 500;
/// Largest preset file read.
const MAX_FILE: usize = 8 << 20;
/// Most controls one call sets.
const MAX_CONTROLS: usize = 64;
/// How long a new layer stays after its words and its animation (seconds).
// TODO(batch1-verify): mock value; Elias tunes it.
const HOLD: f64 = 0.5;

// ---------------------------------------------------------------- the cards

/// One text preset: a Motion Library card (or a file) and its `.ecpreset` document.
#[derive(Clone, Debug)]
pub struct TextPreset {
    pub name: String,
    /// The card id (`None`: a file given by path).
    pub id: Option<String>,
    /// The `.ecpreset` file, when it is on this machine.
    pub path: Option<String>,
    /// `library` (a library folder of this Mac), `motion` (the catalog) or `file`.
    pub source: &'static str,
    /// The `.ecpreset` document (`anim.savePreset`'s JSON).
    pub doc: Value,
}

/// A text animation card: (id, name, file on this machine, when it was made).
#[derive(Clone, Debug)]
struct CardRef {
    id: String,
    name: String,
    file: Option<String>,
    /// Unix seconds: the index's `created_at`, else the file's modification time, else 0 (a
    /// catalog card).
    created: f64,
}

fn category_p(p: &Value) -> String {
    str_p(p, "category").map(|c| c.trim().trim_matches('/')).filter(|c| !c.is_empty()).unwrap_or(TEXT_CATEGORY).to_string()
}

/// `cat` is `want` or a folder inside it.
fn in_category(cat: &str, want: &str) -> bool {
    let (c, w) = (cat.trim().trim_matches('/').to_lowercase(), want.to_lowercase());
    c == w || c.starts_with(&format!("{w}/"))
}

/// The preset cards of `category`, one per name (see [`one_per_name`]).
fn cards(s: &Session, category: &str) -> Vec<CardRef> {
    let mut out = one_per_name(all_cards(s, category));
    out.truncate(MAX_PRESETS);
    out
}

/// Every preset card of `category`, by name: this Mac's library folders first, then the catalog's.
fn all_cards(s: &Session, category: &str) -> Vec<CardRef> {
    let mut out: Vec<CardRef> = vec![];
    for e in s.motion_library_local().values() {
        if e.kind.eq_ignore_ascii_case("preset") && in_category(&e.category, category) {
            let file = Some(e.file.clone()).filter(|f| !f.is_empty());
            let created = e.created.or_else(|| file.as_deref().and_then(modified)).unwrap_or(0.0);
            out.push(CardRef { id: e.id.clone(), name: e.name.clone(), file, created });
        }
    }
    if let Ok(client) = s.motion_library()
        && let Some(cat) = client.catalog()
    {
        for c in &cat.cards {
            if c.kind.eq_ignore_ascii_case("preset") && in_category(&c.category, category) && !out.iter().any(|x| x.id == c.id) {
                out.push(CardRef { id: c.id.clone(), name: c.name.clone(), file: None, created: 0.0 });
            }
        }
    }
    out.sort_by_key(|c| c.name.to_lowercase());
    out.truncate(MAX_PRESETS.saturating_mul(4));
    out
}

/// One card per name (case and outer spaces ignored): when a name repeats (an old card and its
/// remake, e.g. two "Slide In"), the newest one wins; the others stay reachable by id.
fn one_per_name(list: Vec<CardRef>) -> Vec<CardRef> {
    let mut out: Vec<CardRef> = vec![];
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for c in list {
        let key = c.name.trim().to_lowercase();
        match at.get(&key).and_then(|i| out.get_mut(*i)) {
            Some(x) if c.created > x.created => *x = c,
            Some(_) => {}
            None => {
                at.insert(key, out.len());
                out.push(c);
            }
        }
    }
    out
}

/// A file's modification time in Unix seconds (none on the web or when it can't be read).
fn modified(path: &str) -> Option<f64> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let t = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

fn parse_preset(bytes: &[u8], what: &str, cmd: &str) -> Result<Value> {
    if bytes.len() > MAX_FILE {
        return Err(bad(cmd, format!("{what} is larger than 8 MB: not a text preset")));
    }
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| bad(cmd, format!("{what} is not a preset file: {e}")))?;
    if doc.get(super::animation::PRESET_TAG).is_none() {
        return Err(bad(cmd, format!("{what} is not an EffectCraft animation preset (.ecpreset)")));
    }
    Ok(doc)
}

fn read_preset(s: &Session, path: &str, cmd: &str) -> Result<Value> {
    let bytes = s.services.read_file(path).map_err(|e| bad(cmd, format!("cannot read the text preset {path}: {e}")))?;
    parse_preset(&bytes, path, cmd)
}

/// A card's preset document: its file on this machine, else the catalog's copy (downloaded).
fn load_card(s: &Session, c: &CardRef, cmd: &str) -> Result<TextPreset> {
    if let Some(f) = &c.file {
        let doc = read_preset(s, f, cmd)?;
        return Ok(TextPreset { name: c.name.clone(), id: Some(c.id.clone()), path: Some(f.clone()), source: "library", doc });
    }
    let client = s.motion_library().map_err(|e| bad(cmd, e))?;
    // A preset card's published blob (`card.project`) is its library entry's `file`, the .ecpreset
    // (motionlib publish.rs).
    #[cfg(not(target_arch = "wasm32"))]
    let prepared = client.prepare_blocking(&c.id, std::time::Duration::from_secs(60));
    #[cfg(target_arch = "wasm32")]
    let prepared = client.prepare(&c.id);
    match prepared {
        effectcraft_motionlib::Prepared::Ready { project, .. } => {
            let doc = parse_preset(&project, &format!("“{}”", c.name), cmd)?;
            Ok(TextPreset { name: c.name.clone(), id: Some(c.id.clone()), path: None, source: "motion", doc })
        }
        effectcraft_motionlib::Prepared::Loading { done, total } => {
            Err(bad(cmd, format!("“{}” is still downloading ({done}/{total}): try again in a moment", c.name)))
        }
        effectcraft_motionlib::Prepared::Failed(e) => Err(bad(cmd, e)),
    }
}

fn names_of(list: &[CardRef], category: &str) -> String {
    if list.is_empty() {
        format!("none yet in Motion Library ▸ {category} (save one with ml.saveTextPreset, or Motion Library ▸ Families ▸ Save selected as preset)")
    } else {
        list.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// The preset `query` names: a `.ecpreset` path (with a `/` or `\`), else a card of the text
/// animations category by id or name (case-insensitive, then ignoring spaces and punctuation).
pub fn find_preset(s: &Session, p: &Value, query: &str, cmd: &str) -> Result<TextPreset> {
    let q = query.trim();
    if q.contains('/') || q.contains('\\') {
        let doc = read_preset(s, q, cmd)?;
        let name = std::path::Path::new(q).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        return Ok(TextPreset { name, id: None, path: Some(q.to_string()), source: "file", doc });
    }
    let category = category_p(p);
    let all = all_cards(s, &category);
    let list = one_per_name(all.clone());
    // By id any card (an older one with a repeated name too); by name the newest.
    // Loosely, "slide-in" / "slidein" / "Slide in" all name “Slide In”.
    let loose = |n: &str| n.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let hit = all
        .iter()
        .find(|c| c.id == q)
        .or_else(|| list.iter().find(|c| c.name.trim().eq_ignore_ascii_case(q)))
        .or_else(|| list.iter().find(|c| !loose(q).is_empty() && loose(&c.name) == loose(q)));
    match hit {
        Some(c) => load_card(s, c, cmd),
        None => Err(bad(cmd, format!("no text preset “{q}” in Motion Library ▸ {category}; there are: {}", names_of(&list, &category)))),
    }
}

/// The Bounce button's preset: the card named [`BOUNCE_CARD`], else the first card of the
/// category with a [`BOUNCE_OVERSHOOT`] control.
pub fn bounce_preset(s: &Session, p: &Value, cmd: &str) -> Result<TextPreset> {
    if let Some(q) = str_p(p, "preset").map(str::trim).filter(|q| !q.is_empty()) {
        return find_preset(s, p, q, cmd);
    }
    let category = category_p(p);
    let list = cards(s, &category);
    if let Some(c) = list.iter().find(|c| c.name.trim().eq_ignore_ascii_case(BOUNCE_CARD)) {
        return load_card(s, c, cmd);
    }
    for c in list.iter().filter(|c| c.file.is_some()) {
        if let Ok(t) = load_card(s, c, cmd)
            && controls(&t.doc).iter().any(|k| k.name == BOUNCE_OVERSHOOT)
        {
            return Ok(t);
        }
    }
    Err(bad(
        cmd,
        format!(
            "Save a Bounce text preset first: Motion Library ▸ {category} has no “{BOUNCE_CARD}” card and no card with a “{BOUNCE_OVERSHOOT}” control (save the workshop layer with ml.saveTextPreset {{layer, name: \"{BOUNCE_CARD}\"}})"
        ),
    ))
}

/// An Expression Control a preset carries (an `ec.control.*` effect): its name, kind
/// (`slider`, `angle`, `dropdown`, `checkbox`, `color`, `point`…), value and dropdown items.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub name: String,
    pub kind: String,
    pub value: Value,
    pub options: Vec<String>,
}

/// The Expression Controls of a preset document, in its order.
pub fn controls(doc: &Value) -> Vec<Control> {
    let mut out = vec![];
    for g in doc.get("effects").and_then(Value::as_array).into_iter().flatten() {
        let Ok(g) = serde_json::from_value::<PropGroup>(g.clone()) else { continue };
        let GroupKind::Effect { effect } = &g.kind else { continue };
        let Some(kind) = effect.strip_prefix("ec.control.") else { continue };
        let Some(pr) = g.props().next() else { continue };
        let options = match &pr.ui {
            ParamUi::Popup { options } => options.clone(),
            _ => vec![],
        };
        let value = match &pr.value {
            KValue::Enum(i) if !options.is_empty() => json!(options.get(*i as usize).cloned().unwrap_or_default()),
            KValue::Bool(b) => json!(b),
            v => {
                let c = v.components();
                match c.len() {
                    0 => Value::Null,
                    1 => json!(c.first().copied().unwrap_or(0.0)),
                    _ => json!(c),
                }
            }
        };
        out.push(Control { name: g.name.clone(), kind: kind.to_string(), value, options });
    }
    out
}

/// The text animator names a preset document carries.
pub fn animator_names(doc: &Value) -> Vec<String> {
    doc.get("animators").and_then(Value::as_array).into_iter().flatten().filter_map(|g| g.get("name").and_then(Value::as_str).map(str::to_string)).collect()
}

fn control_json(c: &Control) -> Value {
    let mut v = json!({"name": c.name, "type": c.kind, "value": c.value});
    if !c.options.is_empty() {
        v["options"] = json!(c.options);
    }
    v
}

// ---------------------------------------------------------------- text.presetFamilies

fn families(s: &mut Session, p: &Value) -> Result<Value> {
    let category = category_p(p);
    let list = cards(s, &category);
    let mut presets = vec![];
    let mut unreadable = vec![];
    for c in &list {
        let mut v = json!({"name": c.name, "id": c.id, "file": c.file});
        match c.file.as_deref().map(|f| read_preset(s, f, "text.presetFamilies")) {
            Some(Ok(doc)) => {
                v["controls"] = json!(controls(&doc).iter().map(control_json).collect::<Vec<_>>());
                v["animators"] = json!(animator_names(&doc));
                v["effects"] = json!(doc.get("effects").and_then(Value::as_array).map_or(0, Vec::len));
                v["source"] = json!("library");
            }
            Some(Err(e)) => {
                unreadable.push(json!({"id": c.id, "name": c.name, "error": e.to_string()}));
                continue;
            }
            // A catalog card: its controls show once it is downloaded (text.apply does that).
            None => v["source"] = json!("motion"),
        }
        presets.push(v);
    }
    let workshop = s.project.items.values().find(|i| i.as_comp().is_some() && is_workshop(&i.name)).map(|i| i.id.0);
    Ok(json!({
        "category": category,
        "presets": presets,
        "unreadable": unreadable,
        "travel": TRAVEL.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "workshop": workshop,
        "glowStyle": GLOW_STYLE,
    }))
}

fn is_workshop(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == WORKSHOP.to_lowercase() || n.contains("text presets")
}

// ---------------------------------------------------------------- speech words

/// Normalised word for matching (`"WORKS,"` → `works`).
fn clean(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// Where a word or phrase is said: (accent or start comp seconds, phrase start, phrase end, the
/// phrase's words as speech.find reports them).
struct Said {
    accent: Option<f64>,
    start: f64,
    end: f64,
    words: Vec<Value>,
}

/// `words`: a phrase's text, or a range of speech-map word indices (`[from, to]` or
/// `{from, to}`, inclusive; lane B's phrase popover).
#[derive(Clone, Debug, PartialEq)]
enum Words {
    Text(String),
    Range(usize, usize),
}

fn words_p(p: &Value) -> Result<Option<Words>> {
    let idx = |v: Option<&Value>| v.and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
    match p.get("words").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(t)) => {
            let t: String = t.trim().chars().take(MAX_TEXT).collect();
            Ok((!t.is_empty()).then_some(Words::Text(t)))
        }
        Some(Value::Array(a)) => match (idx(a.first()), idx(a.get(1)).or(idx(a.first()))) {
            (Some(x), Some(y)) => Ok(Some(Words::Range(x.min(y), x.max(y)))),
            _ => Err(bad(C, "`words` as a range is [from, to] (speech-map word indices)")),
        },
        Some(v @ Value::Object(_)) => match (idx(v.get("from")), idx(v.get("to")).or(idx(v.get("from")))) {
            (Some(x), Some(y)) => Ok(Some(Words::Range(x.min(y), x.max(y)))),
            _ => Err(bad(C, "`words` as a range is {from, to} (speech-map word indices)")),
        },
        Some(v) => Err(bad(C, format!("`words` is a phrase or [from, to] word indices, not {v}"))),
    }
}

/// The speech-map words `from..=to` of `cid`, as [`find_said`] reports a phrase.
fn range_said(s: &Session, cid: ItemId, from: usize, to: usize) -> Result<Said> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let map = comp.speech.as_ref().ok_or_else(|| bad(C, "the comp has no speech map (speech.import)"))?;
    let list = map.words.get(from..=to).ok_or_else(|| bad(C, format!("the speech map has {} words; no range {from}..{to}", map.words.len())))?;
    if list.len() > 64 {
        return Err(bad(C, "`words` covers more than 64 words"));
    }
    let fps = comp.frame_rate.as_f64();
    let lead = lint::DEFAULT_ACCENT_LEAD;
    let words: Vec<Value> = list
        .iter()
        .map(|w| {
            let mut o = json!({"word": w.word, "start": w.start.seconds(), "end": w.end.seconds()});
            if let Some((_, syl)) = w.stressed() {
                o["stressTime"] = json!(lint::accent_time(syl, fps, lead));
            }
            o
        })
        .collect();
    Ok(Said {
        accent: words.iter().find_map(|w| w.get("stressTime").and_then(Value::as_f64)),
        start: list.first().map(|w| w.start.seconds()).unwrap_or(0.0),
        end: list.last().map(|w| w.end.seconds()).unwrap_or(0.0),
        words,
    })
}

/// A phrase given as text (found with speech.find) or as a range.
fn said_of(s: &mut Session, cid: ItemId, w: &Words, occurrence: Option<u64>) -> Result<Said> {
    match w {
        Words::Text(t) => find_said(s, cid, t, occurrence),
        Words::Range(a, b) => range_said(s, cid, *a, *b),
    }
}

fn find_said(s: &mut Session, cid: ItemId, text: &str, occurrence: Option<u64>) -> Result<Said> {
    let mut q = json!({"comp": cid.0, "word": text, "go": false});
    if let Some(n) = occurrence {
        q["occurrence"] = json!(n);
    }
    let r = s.execute("speech.find", q)?;
    let stressed = r.get("stressed").and_then(Value::as_bool).unwrap_or(false);
    Ok(Said {
        accent: if stressed { r.get("time").and_then(Value::as_f64) } else { None },
        start: r.get("start").and_then(Value::as_f64).unwrap_or(0.0),
        end: r.get("end").and_then(Value::as_f64).unwrap_or(0.0),
        words: r.get("words").and_then(Value::as_array).cloned().unwrap_or_default(),
    })
}

/// The accent (comp s) of `word` among `words` (layer.words / speech.find entries), else the
/// first stressed one; with the word that was picked.
fn accent_in(words: &[Value], word: Option<&str>) -> Option<(f64, String)> {
    let pick = |w: &Value| w.get("stressTime").and_then(Value::as_f64).map(|t| (t, w.get("word").and_then(Value::as_str).unwrap_or("").to_string()));
    match word.map(clean).filter(|w| !w.is_empty()) {
        Some(q) => words.iter().filter(|w| w.get("word").and_then(Value::as_str).is_some_and(|x| clean(x) == q)).find_map(pick),
        None => words.iter().find_map(pick),
    }
}

// ---------------------------------------------------------------- the peak

/// The earliest key under the layer's transform, text animators and effects (layer time).
fn first_key(l: &Layer) -> Option<Tick> {
    let mut first: Option<Tick> = None;
    for path in ["transform", "text/animators", "effects"] {
        if let Some(g) = l.props.group(path) {
            g.walk("", &mut |_, pr| {
                if let Some(t) = pr.keys.iter().map(|k| k.time).min() {
                    first = Some(first.map_or(t, |f| f.min(t)));
                }
            });
        }
    }
    first
}

/// The last key of those properties (layer time).
fn last_key(l: &Layer) -> Option<Tick> {
    let mut last: Option<Tick> = None;
    for path in ["transform", "text/animators", "effects"] {
        if let Some(g) = l.props.group(path) {
            g.walk("", &mut |_, pr| {
                if let Some(t) = pr.keys.iter().map(|k| k.time).max() {
                    last = Some(last.map_or(t, |x| x.max(t)));
                }
            });
        }
    }
    last
}

/// How much a property's speed peak counts as "the" move (lower first).
fn lead_rank(match_id: &str) -> u8 {
    match match_id {
        "position" => 0,
        "scale" => 1,
        "rotation" | "rotationZ" => 2,
        "anchor" | "anchorPoint" => 3,
        "opacity" => 4,
        _ => 5,
    }
}

/// The layer's main move: its speed peak (layer seconds). The position (else scale, rotation,
/// anchor, opacity, anything) whose first eased move peaks.
fn lead_peak(l: &Layer) -> Option<f64> {
    let mut best: Option<(u8, f64)> = None;
    for path in ["transform", "text/animators"] {
        let Some(g) = l.props.group(path) else { continue };
        g.walk("", &mut |_, pr| {
            if pr.keys.len() < 2 {
                return;
            }
            let Some((t, _)) = (0..pr.keys.len() - 1).find_map(|i| lint::segment_peak(&pr.keys, i, pr.spatial)) else { return };
            let rank = lead_rank(&pr.match_id);
            if best.is_none_or(|(r, bt)| rank < r || (rank == r && t < bt)) {
                best = Some((rank, t));
            }
        });
    }
    best.map(|(_, t)| t)
}

/// The unit (0-based) of `word` in `text` and how many units there are, by a selector's Based On
/// (0 Characters, 1 Characters Excluding Spaces, 2 Words, 3 Lines). A word that isn't in the
/// text is the first unit.
fn unit_of(text: &str, word: &str, based_on: u32) -> (usize, usize) {
    // Words: (first char, chars, line); a line break is `\r`, `\n` or `\r\n`.
    let mut words: Vec<(usize, usize, usize)> = vec![];
    let (mut line, mut start, mut prev_cr, mut total) = (0usize, None::<usize>, false, 0usize);
    for (i, ch) in text.chars().enumerate() {
        total = i + 1;
        if ch.is_whitespace() {
            if let Some(st) = start.take() {
                words.push((st, i - st, line));
            }
            if ch == '\r' || (ch == '\n' && !prev_cr) {
                line += 1;
            }
        } else if start.is_none() {
            start = Some(i);
        }
        prev_cr = ch == '\r';
    }
    if let Some(st) = start {
        words.push((st, total.saturating_sub(st), line));
    }
    let want = clean(word);
    let hit = words.iter().enumerate().find(|(_, (st, len, _))| !want.is_empty() && clean(&text.chars().skip(*st).take(*len).collect::<String>()) == want);
    let (wi, (st, len, ln)) = match hit {
        Some((wi, w)) => (wi, *w),
        None => (0, words.first().copied().unwrap_or((0, 0, 0))),
    };
    match based_on {
        2 => (wi, words.len().max(1)),
        3 => (ln, line.saturating_add(1)),
        1 => {
            let before = text.chars().take(st).filter(|c| !c.is_whitespace()).count();
            (before + len / 2, text.chars().filter(|c| !c.is_whitespace()).count().max(1))
        }
        _ => (st + len / 2, total.max(1)),
    }
}

/// When (comp s) the "Fade Up" reveal brings `word`'s unit in: its Range Selector Offset (keyed,
/// layer time) crosses `c − 100 + 100·UNIT_PEAK` for the unit's centre `c` (a Ramp Up unit
/// appears from Offset `c − 100` to `c`). The units are those of the "Fade Up" copy that is on
/// (its Amount, expressions run, is not 0; else the keyed one).
fn reveal_peak(s: &Session, cid: ItemId, l: &Layer, word: &str) -> Option<f64> {
    let comp = s.project.comp(cid)?;
    let anims = l.props.group("text/animators")?;
    let fade: Vec<&PropGroup> = anims.groups().filter(|g| g.name.starts_with(REVEAL_ANIMATOR)).collect();
    let offset_of = |g: &PropGroup| g.prop("selectors/rangeSelector/offset").filter(|o| o.keys.len() >= 2).cloned();
    let (master, off) = fade.iter().copied().find_map(|g| offset_of(g).map(|o| (g, o)))?;
    let (k0, k1) = (off.keys.first()?.time, off.keys.last()?.time);
    // EvalCtx::value runs the Amount expression (it reads `effect("Animate By")("Menu")`) when
    // the session has an expression host (the app); without one the keyed copy is used.
    let ctx = effectcraft_render::EvalCtx { project: &s.project, comp_id: cid, comp, time: l.comp_time(k0), expr: s.expr.as_deref(), footage: None };
    let active = fade
        .iter()
        .copied()
        .find(|g| g.prop("selectors/rangeSelector/advanced/amount").is_some_and(|a| ctx.value(l, a).as_f64().abs() > 1e-6))
        .unwrap_or(master);
    let based_on = match active.prop("selectors/rangeSelector/advanced/basedOn").map(|p| &p.value) {
        Some(KValue::Enum(i)) => *i,
        _ => 0,
    };
    let text = match l.props.prop("text/sourceText").map(|p| &p.value) {
        Some(KValue::Text(d)) => d.text.clone(),
        _ => return None,
    };
    let (i, n) = unit_of(&text, word, based_on);
    let c = (i as f64 + 0.5) / n as f64 * 100.0;
    let target = c - 100.0 + 100.0 * UNIT_PEAK;
    const STEPS: u32 = 64;
    let (a0, span) = (k0.seconds(), (k1 - k0).seconds());
    // Key time (layer s) → comp s: through the "In" marker when the Offset's expression plays
    // the keys over it (DX1b), else the layer's clock.
    let marker = in_marker_clock(l, &off, comp.frame_rate.frame_duration());
    let to_comp = |tk: f64| match marker {
        Some((m, d)) if span > 0.0 => m + (tk - a0) / span * d,
        _ => l.comp_time(Tick::from_seconds_f64(tk)).seconds(),
    };
    let at = |t: f64| off.value_at(Tick::from_seconds_f64(t)).as_f64();
    let mut prev = (a0, at(a0));
    if (prev.1 - target).abs() < 1e-9 {
        return Some(to_comp(a0));
    }
    for j in 1..=STEPS {
        let t = a0 + span * f64::from(j) / f64::from(STEPS);
        let v = at(t);
        if (prev.1 - target) * (v - target) <= 0.0 && (v - prev.1).abs() > 1e-12 {
            let tl = prev.0 + (target - prev.1) / (v - prev.1) * (t - prev.0);
            return Some(to_comp(tl));
        }
        prev = (t, v);
    }
    None
}

/// The layer marker whose comment is [`IN_MARKER`] (the first, as `marker.key("In")` finds it).
fn in_marker(l: &Layer) -> Option<&effectcraft_project::Marker> {
    l.markers.iter().find(|m| m.comment == IN_MARKER)
}

/// When the reveal is timed by the layer's "In" marker (Elias's presets since PRESET-MARKERS:
/// the Offset's expression plays its keys from the marker's time over its duration,
/// `valueAtTime(k1 + (time − m.time)/d·(k2 − k1))`, `d = max(frameDuration, m.duration)`): the
/// marker's comp time and `d` (s). `None` when the layer has no "In" marker or the Offset has no
/// enabled expression that reads markers (then its keys play on the layer's clock).
fn in_marker_clock(l: &Layer, off: &effectcraft_project::Property, frame: Tick) -> Option<(f64, f64)> {
    let reads_marker = off.expr.as_ref().is_some_and(|e| e.enabled && e.text.contains("marker"));
    let m = in_marker(l).filter(|_| reads_marker)?;
    let d = m.duration.max(frame).seconds();
    (d.is_finite() && d > 0.0).then(|| (l.comp_time(m.time).seconds(), d))
}

/// The time (comp s) `sync` lands on the accent, and how it was found.
fn peak_of(s: &Session, cid: ItemId, lid: LayerId, word: &str) -> Option<(f64, &'static str)> {
    let l = s.project.comp(cid)?.layer(lid)?;
    if let Some(t) = reveal_peak(s, cid, l, word) {
        return Some((t, "reveal"));
    }
    lead_peak(l).map(|t| (l.comp_time(Tick::from_seconds_f64(t)).seconds(), "speedPeak"))
}

// ---------------------------------------------------------------- text.apply

/// One layer to animate: an existing one, or one made for words.
struct Job {
    layer: LayerId,
    created: bool,
    /// Comp seconds: where the preset's keys start without sync.
    start: f64,
    /// Comp seconds: the stressed syllable the peak lands on, with its word.
    accent: Option<(f64, String)>,
    /// A made layer's words (comp s).
    span: Option<(f64, f64)>,
}

/// `distance`: near | mid | far (the old chips) or pixels.
fn distance_p(p: &Value) -> Result<Option<f64>> {
    match p.get("distance").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(d)) if d.trim().is_empty() => Ok(None),
        Some(Value::String(d)) => {
            let d = d.trim().to_ascii_lowercase();
            TRAVEL.iter().find(|(k, _)| *k == d).map(|(_, v)| Some(*v)).ok_or_else(|| bad(C, format!("`distance` is near, mid, far or pixels, not {d:?}")))
        }
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()).map(Some).ok_or_else(|| bad(C, "`distance`: a number of pixels")),
        Some(v) => Err(bad(C, format!("`distance` is near, mid, far or pixels, not {v}"))),
    }
}

/// `controls`: {name: value}.
fn controls_p(p: &Value) -> Result<Vec<(String, Value)>> {
    match p.get("controls") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Object(o)) => {
            if o.len() > MAX_CONTROLS {
                return Err(bad(C, format!("`controls`: at most {MAX_CONTROLS}")));
            }
            Ok(o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        }
        Some(v) => Err(bad(C, format!("`controls` is {{\"Control name\": value}}, not {v}"))),
    }
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let query = str_p(p, "preset")
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| bad(C, "missing `preset` (a text preset card's name, e.g. \"Slide In\"; text.presetFamilies lists them)"))?;
    let distance = distance_p(p)?;
    let given = controls_p(p)?;
    let sync = b_p(p, "sync").unwrap_or(false);
    let glow = b_p(p, "glow").unwrap_or(false);
    let word = str_p(p, "word").map(str::trim).filter(|w| !w.is_empty()).map(str::to_string);
    let words = words_p(p)?;
    let occurrence = match p.get("occurrence").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_u64().filter(|n| *n >= 1).ok_or_else(|| bad(C, format!("`occurrence` is 1 or more, not {v}")))?),
    };
    let at = match p.get("at").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_f64().filter(|x| x.is_finite() && x.abs() <= 86_400.0).ok_or_else(|| bad(C, format!("`at` is comp seconds, not {v}")))?),
    };
    let cid = comp_id(s, p)?;
    let preset = find_preset(s, p, query, C)?;
    let have = controls(&preset.doc);
    // Exact names only: a typo is an error that lists what the preset has.
    let names = || {
        let v: Vec<&str> = have.iter().map(|c| c.name.as_str()).collect();
        if v.is_empty() { "none".to_string() } else { v.join(", ") }
    };
    for (n, _) in &given {
        if !have.iter().any(|c| c.name == *n) {
            return Err(bad(C, format!("“{}” has no control “{n}”; its controls: {}", preset.name, names())));
        }
    }
    let mut sets = given;
    if let Some(px) = distance {
        for c in &have {
            if c.name.to_lowercase().contains("travel") && !sets.iter().any(|(n, _)| *n == c.name) {
                sets.push((c.name.clone(), json!(px)));
            }
        }
    }

    let snap = s.project.clone();
    let history = s.history.clone();
    let mut notes: Vec<String> = vec![];
    let opts = Opts { sync, glow, word: word.as_deref(), words: words.as_ref(), occurrence, at };
    match run(s, p, cid, &preset, &sets, &opts, &mut notes) {
        Ok(mut r) => {
            super::batch::fold(s, &snap, &format!("Text Preset {}", preset.name));
            r["notes"] = json!(notes);
            Ok(r)
        }
        Err(e) => {
            // Leave the project as it was (a layer made, the preset half applied).
            s.project = snap;
            s.history = history;
            s.sanitize_state();
            s.bump();
            Err(e)
        }
    }
}

struct Opts<'a> {
    sync: bool,
    glow: bool,
    word: Option<&'a str>,
    words: Option<&'a Words>,
    occurrence: Option<u64>,
    at: Option<f64>,
}

/// The layers to animate: the given ones, else a new one for `words` (or `word`), else the
/// selected text layers.
fn jobs(s: &mut Session, p: &Value, cid: ItemId, o: &Opts, notes: &mut Vec<String>) -> Result<Vec<Job>> {
    let explicit_layers = p.get("layers").or(p.get("layer")).is_some_and(|v| !v.is_null());
    let mut jobs: Vec<Job> = vec![];
    if explicit_layers || (o.words.is_none() && o.word.is_none()) {
        let (lcid, ids) = layers_p(s, p)?;
        if lcid != cid {
            return Err(bad(C, "the layers are in another comp than `comp`"));
        }
        let ids = unlocked(s, cid, ids, C)?;
        let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
        let text_ids: Vec<LayerId> =
            ids.iter().copied().filter(|id| comp.layer(*id).is_some_and(|l| matches!(l.source, LayerSource::Text))).take(MAX_LAYERS).collect();
        if text_ids.is_empty() {
            return Err(bad(C, "select text layers (or pass `layers`, or `words` to make one from the speech map)"));
        }
        if text_ids.len() < ids.len() {
            notes.push(format!("{} layer(s) that are not text were left alone", ids.len() - text_ids.len()));
        }
        // The phrase to sync on, when given with the layers.
        let phrase = match (o.words, o.word) {
            (Some(w), _) if o.sync => Some(said_of(s, cid, w, o.occurrence)?),
            (None, Some(w)) if o.sync => Some(find_said(s, cid, w, o.occurrence)?),
            _ => None,
        };
        for lid in text_ids {
            let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
            let Some(l) = comp.layer(lid) else { continue };
            let start = o.at.unwrap_or_else(|| l.in_point.seconds());
            let accent = if !o.sync {
                None
            } else if let Some(ph) = &phrase {
                accent_in(&ph.words, o.word).or_else(|| ph.accent.map(|t| (t, o.word.unwrap_or("").to_string())))
            } else {
                // The words said while the layer shows.
                let r = s.execute("layer.words", json!({"comp": cid.0, "layer": lid.0}))?;
                accent_in(r.get("words").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), o.word)
            };
            if o.sync && accent.is_none() {
                notes.push(format!("layer {}: no stressed syllable to sync on, so it starts at its in point", lid.0));
            }
            jobs.push(Job { layer: lid, created: false, start, accent, span: None });
        }
        return Ok(jobs);
    }
    let said = match (o.words, o.word) {
        (Some(w), _) => said_of(s, cid, w, o.occurrence)?,
        (None, Some(w)) => find_said(s, cid, w, o.occurrence)?,
        (None, None) => return Err(bad(C, "give `layers`, `words` or `word`")),
    };
    let text = match o.words {
        Some(Words::Text(t)) => t.clone(),
        _ => o.word.unwrap_or("").to_string(),
    };
    let accent = if o.sync { accent_in(&said.words, o.word).or_else(|| said.accent.map(|t| (t, text.clone()))) } else { None };
    if o.sync && accent.is_none() {
        notes.push(format!("{text:?} has no stressed syllable in the speech map: the animation starts with the words"));
    }
    // The text as said in the map (a phrase's words joined), else as given.
    let shown: String = {
        let said_text: Vec<&str> = said.words.iter().filter_map(|w| w.get("word").and_then(Value::as_str)).collect();
        if said_text.is_empty() { text.clone() } else { said_text.join(" ") }
    };
    let r = s.execute("layer.newText", json!({"comp": cid.0, "text": shown, "name": shown.chars().take(60).collect::<String>()}))?;
    let lid = r.get("layer").and_then(Value::as_u64).map(LayerId).ok_or_else(|| bad(C, "layer.newText made no layer"))?;
    jobs.push(Job { layer: lid, created: true, start: o.at.unwrap_or(said.start), accent, span: Some((said.start, said.end)) });
    Ok(jobs)
}

/// Set the controls `sets` (exact effect names) on the layer: the effect's first parameter, as
/// prop.set reads a value (a dropdown takes its item name or 0-based index). Keyed controls are
/// left alone (reported).
fn set_controls(s: &mut Session, cid: ItemId, lid: LayerId, sets: &[(String, Value)]) -> Result<Vec<Value>> {
    if sets.is_empty() {
        return Ok(vec![]);
    }
    s.edit("Text Preset Controls", None, |proj, _| {
        let l = super::layer_mut(proj, cid, lid)?;
        let fx = l.props.sub_mut("effects").ok_or_else(|| bad(C, "this layer has no effects"))?;
        let mut out = vec![];
        for (name, v) in sets {
            let Some(g) = fx.children.iter_mut().rev().find_map(|c| match c {
                Node::Group(g) if g.name == *name => Some(g),
                _ => None,
            }) else {
                out.push(json!({"control": name, "skipped": "not on the layer"}));
                continue;
            };
            let Some(pr) = g.children.iter_mut().find_map(|c| match c {
                Node::Prop(pr) => Some(pr),
                _ => None,
            }) else {
                out.push(json!({"control": name, "skipped": "no parameter"}));
                continue;
            };
            if pr.is_animated() {
                out.push(json!({"control": name, "skipped": "keyed in the preset"}));
                continue;
            }
            let cur = pr.value.clone();
            let (kv, warnings) = super::prop::coerce_for(pr, &cur, v, C)?;
            pr.value = kv;
            let mut r = json!({"control": name, "value": v});
            if !warnings.is_empty() {
                r["warnings"] = json!(warnings);
            }
            out.push(r);
        }
        Ok(out)
    })
}

/// Move every key the preset put on the layer (its animators, effects and properties, by name
/// and path) and its "In" marker by `shift`.
fn shift_preset_keys(s: &mut Session, cid: ItemId, lid: LayerId, doc: &Value, shift: Tick) -> Result<()> {
    let anims = animator_names(doc);
    let fx_names: Vec<String> =
        doc.get("effects").and_then(Value::as_array).into_iter().flatten().filter_map(|g| g.get("name").and_then(Value::as_str).map(str::to_string)).collect();
    let paths: Vec<String> = doc
        .get("properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect();
    s.edit("Sync Text Preset", None, |proj, _| {
        let l = super::layer_mut(proj, cid, lid)?;
        let mut move_keys = |pr: &mut effectcraft_project::Property| {
            for k in &mut pr.keys {
                k.time += shift;
            }
        };
        for (group, names) in [("text/animators", &anims), ("effects", &fx_names)] {
            if let Some(g) = l.props.group_mut(group) {
                for c in &mut g.children {
                    if let Node::Group(x) = c
                        && names.contains(&x.name)
                    {
                        x.walk_mut(&mut move_keys);
                    }
                }
            }
        }
        for path in &paths {
            if let Some(pr) = l.props.prop_mut(path) {
                move_keys(pr);
            }
        }
        // The "In" marker moves with the keys, so the reveal it times moves too (DX1b); "Out"
        // stays. Locked markers stay (anim.applyPreset already skipped them with a note).
        if !l.markers_locked
            && let Some(m) = l.markers.iter_mut().find(|m| m.comment == IN_MARKER)
        {
            m.time += shift;
            l.markers.sort_by_key(|m| m.time);
        }
        Ok(())
    })
}

/// The preset's markers anchored to the out point (`anim.savePreset` writes `anchor: "out"` and
/// `time` = how long before the out point): (comment, that distance).
fn out_anchored(doc: &Value) -> Vec<(String, Tick)> {
    doc.get("markers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("anchor").and_then(Value::as_str) == Some("out"))
        .filter_map(|m| {
            let mk: effectcraft_project::Marker = serde_json::from_value(m.clone()).ok()?;
            Some((mk.comment, mk.time.clamp(Tick::MIN, Tick::MAX)))
        })
        .collect()
}

/// Moves the out-anchored markers `anim.applyPreset` placed (at `old_out` − their distance, layer
/// time) to the layer's out point now − their distance, the rule it places them by. Locked markers
/// stay (applyPreset skipped them).
fn follow_out_point(l: &mut Layer, outs: &[(String, Tick)], old_out: Tick) {
    let new_out = l.layer_time(l.out_point);
    if outs.is_empty() || l.markers_locked || new_out == old_out {
        return;
    }
    let mut moved = false;
    for (comment, dist) in outs {
        let placed = old_out - *dist;
        if let Some(m) = l.markers.iter_mut().find(|m| m.comment == *comment && m.time == placed) {
            m.time = new_out - *dist;
            moved = true;
        }
    }
    if moved {
        l.markers.sort_by_key(|m| m.time);
    }
}

fn run(s: &mut Session, p: &Value, cid: ItemId, preset: &TextPreset, sets: &[(String, Value)], o: &Opts, notes: &mut Vec<String>) -> Result<Value> {
    let jobs = jobs(s, p, cid, o, notes)?;
    let fr = s.project.comp(cid).ok_or(EngineError::NoComp)?.frame_rate;
    let fps = fr.as_f64();
    let mut reports = vec![];
    let out_markers = out_anchored(&preset.doc);
    for job in &jobs {
        // 1. The preset, from the start (same-named effects and animators replaced).
        let start = fr.snap_nearest(Tick::from_seconds_f64(job.start));
        let r = s.execute("anim.applyPreset", json!({"preset": preset.doc, "layers": [job.layer.0], "at": start.seconds(), "replace": true, "comp": cid.0}))?;
        // 2. Its controls.
        let set = set_controls(s, cid, job.layer, sets)?;
        let mut rep = json!({
            "layer": job.layer.0,
            "created": job.created,
            "applied": r.get("applied"),
            "controls": set,
        });
        if let Some(sk) = r.get("skipped").and_then(Value::as_array).filter(|a| !a.is_empty()) {
            rep["skipped"] = json!(sk);
        }
        // 3. Sync: the reveal (or the main move's speed peak) on the accent, to the frame.
        let mut base = start;
        if let Some((accent, w)) = &job.accent {
            match peak_of(s, cid, job.layer, w) {
                Some((peak, how)) => {
                    let new_base = fr.snap_nearest(Tick::from_seconds_f64(start.seconds() + accent - peak));
                    let shift = new_base - start;
                    shift_preset_keys(s, cid, job.layer, &preset.doc, shift)?;
                    base = new_base;
                    let after = peak + shift.seconds();
                    rep["sync"] = json!({
                        "word": w,
                        "by": how,
                        "accentFrame": ((accent * fps) * 100.0).round() / 100.0,
                        "peakFrame": ((after * fps) * 100.0).round() / 100.0,
                        "offsetFrames": (((after - accent) * fps) * 100.0).round() / 100.0,
                    });
                }
                None => {
                    notes.push(format!("“{}” has no keyed “{REVEAL_ANIMATOR}” Offset or eased move to sync on: it starts at the layer's start", preset.name))
                }
            }
        }
        rep["start"] = json!((base.seconds() * 1000.0).round() / 1000.0);
        rep["startFrame"] = json!((base.seconds() * fps).round() as i64);
        // 4. A made layer shows from its words (or the first key) to the end of both, plus a hold.
        let fit = s.edit("Text Preset Layer", None, |proj, _| {
            let l = super::layer_mut(proj, cid, job.layer)?;
            let first = first_key(l).map(|t| l.comp_time(t));
            let last = last_key(l).map(|t| l.comp_time(t));
            if let (true, Some((a, b))) = (job.created, job.span) {
                let inn = first.map_or(fr.snap_nearest(Tick::from_seconds_f64(a)), |f| f.min(fr.snap_nearest(Tick::from_seconds_f64(a))));
                let end = last.unwrap_or(inn).max(Tick::from_seconds_f64(b)) + Tick::from_seconds_f64(HOLD);
                let old_out = l.layer_time(l.out_point);
                l.in_point = inn;
                l.out_point = fr.snap_nearest(end).max(inn + fr.frame_duration());
                // anim.applyPreset put the out-anchored markers ("Out") before the made layer's
                // first out point; they keep their distance from the new one (DX1c).
                follow_out_point(l, &out_markers, old_out);
            }
            Ok((l.name.clone(), first.is_some_and(|f| f < l.in_point)))
        })?;
        rep["name"] = json!(fit.0);
        if fit.1 {
            rep["warning"] = json!("the animation starts before the layer's in point");
        }
        reports.push(rep);
    }
    let targets: Vec<LayerId> = jobs.iter().map(|j| j.layer).collect();
    s.state.selected_layers = targets.clone();
    let glow_report = if o.glow { apply_glow(s, cid, &targets, notes)? } else { Value::Null };
    let mut out = json!({
        "preset": {"name": preset.name, "id": preset.id, "path": preset.path, "source": preset.source, "controls": controls(&preset.doc).iter().map(control_json).collect::<Vec<_>>()},
        "comp": cid.0,
        "sync": o.sync,
        "layers": reports,
    });
    if !glow_report.is_null() {
        out["glow"] = glow_report.clone();
        if let Some(f) = glow_report.get("followUp") {
            out["followUp"] = json!([f]);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- the glow style

/// The look of `tpl` (effects and layer styles) onto `dst`, for the glow style.
fn copy_look(tpl: &Layer, dst: &mut Layer, next: &mut u64) -> usize {
    let mut n = 0;
    if let Some(fx_t) = tpl.props.sub("effects") {
        let list: Vec<PropGroup> = fx_t.groups().cloned().collect();
        if let Some(fx) = dst.props.sub_mut("effects") {
            for mut g in list {
                fx.children.retain(|c| !matches!(c, Node::Group(x) if x.match_id == g.match_id && x.name == g.name));
                g.reassign_uids(next);
                *next += 1;
                fx.children.push(Node::Group(g));
                n += 1;
            }
        }
    }
    // A layer without a `layerStyles` group gets the template's right after Transform, where
    // styles::add_style puts a new one (the renderer finds it anywhere; the Timeline lists it
    // in tree order).
    if dst.can_have_styles()
        && let Some(st_t) = tpl.props.sub(effectcraft_project::styles::GROUP)
    {
        let mut g = st_t.clone();
        g.reassign_uids(next);
        *next += 1;
        match dst.props.children.iter().position(|c| matches!(c, Node::Group(x) if x.match_id == effectcraft_project::styles::GROUP)) {
            Some(i) => {
                if let Some(slot) = dst.props.children.get_mut(i) {
                    *slot = Node::Group(g);
                }
            }
            None => {
                let at = dst.props.children.iter().position(|c| c.match_id() == "transform").map_or(dst.props.children.len(), |i| i + 1);
                dst.props.children.insert(at, Node::Group(g));
            }
        }
        n += 1;
    }
    n
}

/// The glow style: a project layer named like it lends its effects and layer styles; else the
/// Motion Library card, which the app applies (`followUp`: the engine can't run its tools).
fn apply_glow(s: &mut Session, cid: ItemId, targets: &[LayerId], notes: &mut Vec<String>) -> Result<Value> {
    let want = GLOW_STYLE.to_lowercase();
    let mut comps: Vec<(bool, u64, ItemId)> =
        s.project.items.values().filter(|i| i.as_comp().is_some()).map(|i| (!is_workshop(&i.name), i.id.0, i.id)).collect();
    comps.sort();
    let found = comps.iter().find_map(|(_, _, c)| {
        let comp = s.project.comp(*c)?;
        comp.layers.iter().find(|l| l.name.trim().to_lowercase() == want && !targets.contains(&l.id)).cloned()
    });
    if let Some(tpl) = found {
        let n = s.edit("Caption Glow", None, |proj, _| {
            let mut next = proj.next_id;
            let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
            let mut n = 0;
            for lid in targets {
                if let Some(l) = comp.layer_mut(*lid) {
                    n += copy_look(&tpl, l, &mut next);
                }
            }
            proj.next_id = next;
            Ok(n)
        })?;
        return Ok(json!({"applied": true, "source": "project", "from": tpl.name, "parts": n}));
    }
    let r = s.execute("library.search", json!({"query": GLOW_STYLE, "source": "motion", "limit": 5}))?;
    let entries = r.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    let hit = entries.iter().find(|e| e.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(GLOW_STYLE))).or(entries.first());
    match hit.and_then(|e| e.get("id")).and_then(Value::as_str) {
        Some(id) => {
            // `motionLibrary.apply` is a UI command (the Motion Library's plug applies text styles):
            // the app runs `followUp` after text.apply (`panels/text_apply.rs`); agents run it with
            // command_run. The target layers are selected.
            notes.push(format!("{GLOW_STYLE}: run followUp (motionLibrary.apply) to add it"));
            Ok(json!({"applied": false, "source": "motion", "card": id, "followUp": {"command": "motionLibrary.apply", "params": {"id": id}}}))
        }
        None => {
            notes.push(format!("{GLOW_STYLE}: not in the project or the Motion Library"));
            Ok(json!({"applied": false, "why": format!("no layer or Motion Library card “{GLOW_STYLE}”")}))
        }
    }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "text.presetFamilies",
            "Text Presets",
            "{category? (default \"Elias/Text animations\")} → {category, presets: [{name (\"Slide In\"), id, file, source (library|motion), controls?: [{name (\"Travel Distance (px)\"), type (slider|angle|dropdown|checkbox|color|point…), value, options? (dropdown items)}], animators? (names), effects? (count)}], unreadable: [{id, name, error}], travel: {near, mid, far} (px), workshop, glowStyle}: Elias's text animations, the Motion Library preset cards of that category, with the controls read from each card's .ecpreset (a catalog card shows them once text.apply downloaded it)",
            families
        ),
        cmd!(
            "text.apply",
            "Apply Text Preset",
            [],
            None,
            "{preset (a text preset card of Motion Library ▸ Elias ▸ Text animations by name or id, e.g. \"Slide In\" (text.presetFamilies lists them), or a .ecpreset path), layers? (text layers; default the selected ones unless `words` is given), words? (a phrase of the comp's speech map as text, or [from, to] / {from, to} speech-map word indices: a new text layer shows it, timed to it), word? (the word to sync on; also the text when neither layers nor words are given), occurrence? (1-based, of words or word), controls? ({\"Travel Distance (px)\": 40, \"Animate By\": \"Words\"}: the preset's Expression Controls by exact name; a dropdown takes an item name or 0-based index; an unknown name is an error), distance? (near|mid|far or px: every control named with “Travel” to 40/90/150 px, when the preset has one), sync? (bool: the stressed syllable of `word`, else the first stressed word, lands on the “Fade Up” reveal of its unit (else the main move's speed peak); the preset's keys and its \"In\" marker shift, \"Out\" stays), glow? (bool: also the Motion Library style “Caption glow + double shadow”), at? (comp s: where the preset's keys start; default the layer's in point), category? (default \"Elias/Text animations\"), comp?} → {preset: {name, id, path, source, controls}, comp, sync, layers: [{layer, name, created, start, startFrame, applied, controls, skipped?, warning?, sync?: {word, by (reveal|speedPeak), accentFrame, peakFrame, offsetFrames}}], glow?, followUp? ([{command, params}] the app runs next), notes}. One undo step",
            has_comp,
            apply
        ),
    ]
}

#[cfg(test)]
pub(crate) fn unit_for_tests(text: &str, word: &str, based_on: u32) -> (usize, usize) {
    unit_of(text, word, based_on)
}
