//! Cards (batch 1, CX1): a picture becomes a post / reel card, as in Devin's edits and Elias's
//! S2 05 cards (`~/.claude/speech-edit/SYSTEM.md`, "Post/reel cards").
//!
//! A card is the picture layer itself with a rounded-rectangle mask named `Card · <style>`: the
//! card's aspect (2:3 by default, never 3:4), as large as fits inside the picture and centred on
//! it (the picture is cropped, never stretched), with corners rounded by a share of the card's
//! width (8.6 % by default). Because the layer stays the same layer, its keys, effects, parent and
//! timing stay as they are, and `link.paste` / `layer.replaceSource` only swap its pixels.
//!
//! **Devin's hairline** (Elias, 19 Oct 2026, SYSTEM.md "Post/reel cards"): every card gets a
//! Stroke layer style of 1 px on screen, white at 32 % opacity, inside the card's edge
//! ([`put_hairline`]). Only thick solid strokes (the old 3 px white rim) are banned.
//!
//! - `card.make {layers?, style?}`: pictures become cards (cards are restyled).
//! - `card.style {layers?, style}`: restyle cards.
//! - `card.saveStyle {layer?, name?}`: the card's style is saved (Motion Library ▸ Elias ▸ Cards,
//!   kept in the settings) and shows on the card wheel.
//! - `card.styles`: the styles the wheel shows.

use effectcraft_keyframe::{ShapePath, Value as KV};
use effectcraft_project::build::Ids;
use effectcraft_project::styles::{self as layer_styles, GlobalLight};
use effectcraft_project::{ItemId, Layer, LayerId, MaskMode, Node, PropGroup, build};
use serde_json::{Value, json};

use super::{CommandSpec, bad, has_layers, layer_mut, layer_p, layers_p, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd, query};

/// Name prefix of a card's mask: the layer is a card while it has one.
pub const MASK_PREFIX: &str = "Card · ";

/// The settings key holding the card styles Elias saved (`card.saveStyle`).
pub const SAVED_KEY: &str = "cardStyles";

/// Where saved styles show in the Motion Library.
pub const SAVED_FOLDER: &str = "Elias/Cards";

/// A card's shape.
#[derive(Clone, Debug, PartialEq)]
pub struct CardStyle {
    /// Wheel item id (`devin`, `reel`…, or `my-…` for saved ones).
    pub id: String,
    pub label: String,
    /// Width : height (2:3 is `[2, 3]`).
    pub aspect: [f64; 2],
    /// Corner radius in percent of the card's width.
    pub radius: f64,
    /// Saved by the user (`card.saveStyle`), not built in.
    pub saved: bool,
}

impl CardStyle {
    fn json(&self) -> Value {
        json!({"id": self.id, "label": self.label, "aspect": format!("{}:{}", self.aspect[0], self.aspect[1]), "radius": self.radius, "saved": self.saved})
    }

    /// Width / height, `None` when the aspect is degenerate.
    fn ratio(&self) -> Option<f64> {
        let r = self.aspect[0] / self.aspect[1];
        (r.is_finite() && r > 0.0).then_some(r)
    }
}

/// The built-in styles (the mock's CSTYLES): Devin's 2:3 · 8.6 % first, the default.
const BUILTIN: [(&str, &str, [f64; 2], f64); 6] = [
    ("devin", "Devin 2:3 · 8.6 %", [2.0, 3.0], 8.6),
    ("reel", "Reel 9:16", [9.0, 16.0], 8.6),
    ("post", "Post 4:5", [4.0, 5.0], 8.6),
    ("square", "Square 1:1", [1.0, 1.0], 8.6),
    ("round", "Very round", [2.0, 3.0], 22.0),
    ("sharp", "Sharp", [2.0, 3.0], 0.0),
];

/// The hairline's opacity (percent) and width on screen (px): Devin's 1 px white at ~32 %.
pub const HAIRLINE_OPACITY: f64 = 32.0;
pub const HAIRLINE_PX: f64 = 1.0;

/// Most saved styles kept (the wheel shows at most 16 slices).
const MAX_SAVED: usize = 8;

fn saved_styles(s: &Session) -> Vec<CardStyle> {
    let Some(Value::Array(a)) = s.prefs.extra.get(SAVED_KEY) else { return vec![] };
    a.iter()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.to_string();
            let label = v.get("label")?.as_str()?.to_string();
            let asp = v.get("aspect")?.as_array()?;
            let aspect = [asp.first()?.as_f64()?, asp.get(1)?.as_f64()?];
            let radius = v.get("radius")?.as_f64()?;
            let st = CardStyle { id, label, aspect, radius: radius.clamp(0.0, 50.0), saved: true };
            st.ratio().map(|_| st)
        })
        .take(MAX_SAVED)
        .collect()
}

/// Every style, built-in first.
pub fn styles(s: &Session) -> Vec<CardStyle> {
    let mut out: Vec<CardStyle> = BUILTIN
        .iter()
        .map(|(id, label, aspect, radius)| CardStyle { id: id.to_string(), label: label.to_string(), aspect: *aspect, radius: *radius, saved: false })
        .collect();
    out.extend(saved_styles(s));
    out
}

/// A style by id or label (case-insensitive), or an `aspect` like `2:3`.
fn find_style(s: &Session, name: &str, cmd: &str) -> Result<CardStyle> {
    let all = styles(s);
    if let Some(st) = all.iter().find(|st| st.id.eq_ignore_ascii_case(name) || st.label.eq_ignore_ascii_case(name)) {
        return Ok(st.clone());
    }
    let ids: Vec<String> = all.iter().map(|s| s.id.clone()).collect();
    Err(bad(cmd, format!("unknown card style `{name}`; one of {}", ids.join(", "))))
}

/// The card mask of a card layer.
pub fn card_mask(l: &Layer) -> Option<&PropGroup> {
    l.masks()?.groups().find(|g| g.name.starts_with(MASK_PREFIX))
}

/// Whether `l` is a card.
pub fn is_card(l: &Layer) -> bool {
    card_mask(l).is_some()
}

/// The style a card was made with (by its mask's name).
pub fn style_of(s: &Session, l: &Layer) -> Option<CardStyle> {
    let label = card_mask(l)?.name.strip_prefix(MASK_PREFIX)?.to_string();
    styles(s).into_iter().find(|st| st.label == label)
}

/// The card's rounded rectangle for a `size` source (layer space): the style's aspect, as large
/// as fits, centred.
pub fn card_path(size: (f64, f64), st: &CardStyle) -> Option<ShapePath> {
    let (w, h) = size;
    let a = st.ratio()?;
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return None;
    }
    let (cw, ch) = if w / h > a { (h * a, h) } else { (w, w / a) };
    let radius = (st.radius.clamp(0.0, 50.0) / 100.0 * cw).max(0.0);
    let bez = effectcraft_path::rect([cw, ch], [w / 2.0, h / 2.0], radius);
    effectcraft_path::from_kurbo(&bez).into_iter().next()
}

/// The card's width in layer pixels for a `size` source (what `card_path` draws).
pub fn card_width(size: (f64, f64), st: &CardStyle) -> Option<f64> {
    let (w, h) = size;
    let a = st.ratio()?;
    (w > 0.0 && h > 0.0).then(|| if w / h > a { h * a } else { w })
}

/// The source size of layer `lid` (0×0 when it has none).
fn source_size(s: &Session, cid: ItemId, lid: LayerId) -> (f64, f64) {
    let Some(l) = s.project.comp(cid).and_then(|c| c.layer(lid)) else { return (0.0, 0.0) };
    let (w, h) = effectcraft_render::source_size(&s.project, l);
    (w as f64, h as f64)
}

/// Put (or reshape) the card mask on `l` for a `size` source. False when `l` can't have masks.
pub(crate) fn put_card_mask(l: &mut Layer, next: &mut u64, size: (f64, f64), st: &CardStyle, color: [u8; 3]) -> std::result::Result<bool, String> {
    let path = card_path(size, st).ok_or_else(|| format!("“{}” has no picture to make a card of", l.name))?;
    let name = format!("{MASK_PREFIX}{}", st.label);
    let layer_name = l.name.clone();
    let Some(masks) = l.props.sub_mut("masks") else { return Ok(false) };
    let existing = masks.children.iter_mut().find_map(|c| match c {
        Node::Group(g) if g.name.starts_with(MASK_PREFIX) => Some(g),
        _ => None,
    });
    match existing {
        Some(g) => {
            g.name = name;
            let pr = g.get_mut("path").ok_or_else(|| format!("“{layer_name}”: its card mask has no path"))?;
            if pr.is_animated() {
                return Err(format!("“{layer_name}”: its card mask is animated; remove the Mask Path keys first"));
            }
            pr.value = KV::path(path);
        }
        None => {
            let g = build::mask(&mut Ids(&mut *next), &name, path, MaskMode::Add, color);
            // First, so the layer's own masks still cut the card.
            masks.children.insert(0, g.into());
        }
    }
    Ok(true)
}

/// Give the card `l` its hairline: a Stroke layer style, white, [`HAIRLINE_OPACITY`] %, inside,
/// [`HAIRLINE_PX`] on screen (its size divided by the layer's own scale; a parent's scale or the
/// viewer zoom aren't counted). A card that already has a Stroke keeps it as the user set it.
/// False when the layer can't take layer styles.
// The renderer strokes the layer's alpha after its masks and effects (layer_buf: styles run on the
// masked content), so the hairline follows the card's rounded edge, as in After Effects.
pub(crate) fn put_hairline(l: &mut Layer, next: &mut u64, gl: &GlobalLight) -> bool {
    let had = l.layer_styles().and_then(|g| g.sub("stroke")).is_some();
    let scale = l
        .props
        .prop("transform/scale")
        .and_then(|p| p.value.components().first().copied())
        .map(f64::abs)
        .filter(|v| v.is_finite() && *v > 1e-6)
        .unwrap_or(100.0);
    let Some(uid) = layer_styles::add_style(l, &mut Ids(&mut *next), "stroke", gl, true) else { return false };
    if had {
        return true;
    }
    let Some(g) = l.props.find_group_mut(uid) else { return false };
    if let Some(p) = g.get_mut("color") {
        p.value = KV::Color([1.0, 1.0, 1.0, 1.0]);
    }
    if let Some(p) = g.get_mut("size") {
        p.value = KV::Scalar((HAIRLINE_PX * 100.0 / scale).clamp(0.25, 250.0));
    }
    if let Some(p) = g.get_mut("opacity") {
        p.value = KV::Scalar(HAIRLINE_OPACITY);
    }
    // STROKE_POSITION: Outside, Inside, Center.
    if let Some(p) = g.get_mut("position") {
        p.value = KV::Enum(1);
    }
    true
}

/// Multiply a layer's Scale (its value and every key) by `ratio`: the card keeps its size on
/// screen when its picture changes size.
pub(crate) fn scale_layer(l: &mut Layer, ratio: f64) {
    if !ratio.is_finite() || ratio <= 0.0 || (ratio - 1.0).abs() < 1e-9 {
        return;
    }
    let Some(pr) = l.props.prop_mut("transform/scale") else { return };
    let mul = |v: &mut KV| match v {
        KV::Vec2(a) => a.iter_mut().for_each(|x| *x *= ratio),
        KV::Vec3(a) => a.iter_mut().for_each(|x| *x *= ratio),
        KV::Scalar(x) => *x *= ratio,
        _ => {}
    };
    mul(&mut pr.value);
    for k in &mut pr.keys {
        mul(&mut k.value);
    }
}

/// Make or restyle cards on `ids` with `st`, as one undo step called `label`.
fn apply(s: &mut Session, cid: ItemId, ids: &[LayerId], st: &CardStyle, label: &str, cmd: &str) -> Result<Value> {
    let sizes: Vec<(LayerId, (f64, f64))> = ids.iter().map(|lid| (*lid, source_size(s, cid, *lid))).collect();
    let cycle = s.prefs.appearance.cycle_mask_colors;
    let st2 = st.clone();
    let cmd_s = cmd.to_string();
    let done = s.edit(label, None, |proj, _| {
        let gl = proj.comp(cid).ok_or(EngineError::NoComp)?.global_light.clone();
        let mut next = proj.next_id;
        let mut cards = vec![];
        let mut skipped = vec![];
        for (lid, size) in &sizes {
            let l = layer_mut(proj, cid, *lid)?;
            let n = l.masks().map(|m| m.children.len()).unwrap_or(0);
            match put_card_mask(l, &mut next, *size, &st2, crate::prefs::mask_color(cycle, n)) {
                Ok(true) => {
                    let hairline = put_hairline(l, &mut next, &gl);
                    cards.push(json!({"layer": lid.0, "name": l.name, "style": st2.id, "hairline": hairline}));
                }
                Ok(false) => skipped.push(json!({"layer": lid.0, "why": "this layer can't have masks"})),
                Err(why) => skipped.push(json!({"layer": lid.0, "why": why})),
            }
        }
        if cards.is_empty() {
            let why = skipped.first().and_then(|v| v.get("why")).and_then(Value::as_str).unwrap_or("no picture layer").to_string();
            return Err(bad(&cmd_s, why));
        }
        proj.next_id = next;
        Ok(json!({"cards": cards, "skipped": skipped, "style": st2.json()}))
    })?;
    Ok(done)
}

/// Layers to work on: `layers` or the selection, unlocked.
fn targets(s: &Session, p: &Value, cmd: &str) -> Result<(ItemId, Vec<LayerId>)> {
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(cmd, "select a picture layer (or pass `layers`)"));
    }
    Ok((cid, unlocked(s, cid, ids, cmd)?))
}

fn make(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "card.make";
    let st = find_style(s, str_p(p, "style").unwrap_or("devin"), cmd)?;
    let (cid, ids) = targets(s, p, cmd)?;
    apply(s, cid, &ids, &st, "Make Card", cmd)
}

fn restyle(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "card.style";
    let name = str_p(p, "style").ok_or_else(|| bad(cmd, "missing `style` (see card.styles)"))?;
    let st = find_style(s, name, cmd)?;
    let (cid, ids) = targets(s, p, cmd)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let cards: Vec<LayerId> = ids.into_iter().filter(|id| comp.layer(*id).is_some_and(is_card)).collect();
    if cards.is_empty() {
        return Err(bad(cmd, "none of these layers is a card yet (card.make makes one)"));
    }
    apply(s, cid, &cards, &st, "Card Style", cmd)
}

/// The card's aspect measured from its mask (for a card whose mask was reshaped by hand).
fn measured_aspect(l: &Layer) -> Option<[f64; 2]> {
    let pr = card_mask(l)?.get("path")?;
    let KV::Path(sp) = &pr.value else { return None };
    let xs = sp.vertices.iter().map(|v| v[0]);
    let ys = sp.vertices.iter().map(|v| v[1]);
    let (x0, x1) = xs.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(x), b.max(x)));
    let (y0, y1) = ys.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), y| (a.min(y), b.max(y)));
    let (w, h) = (x1 - x0, y1 - y0);
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some([w / h, 1.0])
}

fn save_style(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "card.saveStyle";
    let (cid, lid) = layer_p(s, p, cmd)?;
    let l = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or(EngineError::NoComp)?;
    if !is_card(l) {
        return Err(bad(cmd, format!("“{}” is not a card (card.make makes one)", l.name)));
    }
    let base = style_of(s, l);
    let aspect = base.as_ref().map(|b| b.aspect).or_else(|| measured_aspect(l)).unwrap_or([2.0, 3.0]);
    let radius = base.as_ref().map(|b| b.radius).unwrap_or(8.6);
    let mut saved: Vec<Value> = match s.prefs.extra.get(SAVED_KEY) {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let n = saved.len() + 1;
    let label = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| format!("Elias card {n}"));
    if label.chars().count() > 60 {
        return Err(bad(cmd, "`name`: at most 60 characters"));
    }
    if styles(s).iter().any(|st| st.label.eq_ignore_ascii_case(&label)) {
        return Err(bad(cmd, format!("there is already a card style called “{label}”")));
    }
    // A stable id the wheel and agents use: `my-<n>`, unused.
    let mut k = n;
    let id = loop {
        let id = format!("my-{k}");
        if !styles(s).iter().any(|st| st.id == id) {
            break id;
        }
        k = k.saturating_add(1);
    };
    let entry = json!({"id": id, "label": label, "aspect": aspect, "radius": radius, "folder": SAVED_FOLDER});
    saved.push(entry);
    // The oldest go first past the limit.
    let over = saved.len().saturating_sub(MAX_SAVED);
    saved.drain(..over);
    s.prefs.extra.insert(SAVED_KEY.into(), Value::Array(saved));
    s.save_prefs();
    s.prefs_revision += 1;
    // The Motion Library backend (backend.py save_asset) has no card-style kind
    // yet, so the style lives in the settings (shown as Motion Library ▸ Elias ▸ Cards on the wheel)
    // and not as a library card.
    let st = CardStyle { id, label, aspect, radius, saved: true };
    Ok(json!({"style": st.json(), "folder": SAVED_FOLDER}))
}

fn list(s: &mut Session, _p: &Value) -> Result<Value> {
    Ok(json!({"styles": styles(s).iter().map(CardStyle::json).collect::<Vec<_>>(), "default": "devin"}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "card.make",
            "Make Card",
            [],
            None,
            "{layers?, style?: devin|reel|post|square|round|sharp or a saved style's id or name (default devin: 2:3, corners 8.6 % of the width)} → {cards: [{layer, name, style, hairline}], skipped: [{layer, why}], style}: each picture becomes a card (a rounded mask of the card's aspect, centred, with Devin's hairline: a Stroke layer style, 1 px on screen, white 32 %, inside; never a thick stroke); keys, effects and timing stay; one undo step",
            has_layers,
            make
        ),
        cmd!(
            "card.style",
            "Card Style",
            [],
            None,
            "{layers?, style: devin|reel|post|square|round|sharp or a saved style's id or name} → {cards, skipped, style}: restyle cards (layers made with card.make); one undo step",
            has_layers,
            restyle
        ),
        cmd!(
            "card.saveStyle",
            "Save Card Style",
            [],
            None,
            "{layer?, name? (default \"Elias card N\")} → {style, folder}: the card's style is saved (Motion Library ▸ Elias ▸ Cards) and shows on the card wheel",
            has_layers,
            save_style
        ),
        query!("card.styles", "Card Styles", "{} → {styles: [{id, label, aspect, radius, saved}], default}", list),
    ]
}
