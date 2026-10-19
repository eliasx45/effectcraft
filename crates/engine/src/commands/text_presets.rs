//! Text presets (batch 1b, lane D: DX1): Elias's text animations, kept as one layer per variation
//! in the workshop comp "Creating Text Presets" and saved to the Motion Library under names like
//! `T3b Pop · mid` (family number, then a / b / c for near / mid / far).
//!
//! - `text.presetFamilies` lists the families and their variants: the workshop comp's layers
//!   (and any other comp's layer named that way), the library folders' entries and the Motion
//!   Library catalog's cards. (`text.presets` is already taken by the built-in text animator
//!   presets of `text_anim.rs`; see the TODO below.)
//! - `text.apply {preset, layers?|words?, sync?, glow?, distance?, word?}` applies one with
//!   add-ons: to the given text layers, or to a new text layer for `words` (a phrase of the comp's
//!   speech map, timed to it). `sync` puts the animation's speed peak (the one the `off-accent`
//!   lint and `keys.alignPeak` find, [`lint::segment_peak`]) on the stressed syllable of `word`,
//!   else of the first stressed word. `distance` picks the family's near / mid / far variant;
//!   without that variant the travel is scaled (near 0.5×, mid 1×, far 1.6×). `glow` also applies
//!   the Motion Library style "Caption glow + double shadow". One undo step.
//!
//! Applying copies the template layer's animation: its text animators, its keyed transform
//! properties (relative to the target's own values, so the text settles where it is) and its
//! effects, with the keys moved so the template's first key lands at the start time. A Motion
//! Library card is brought in with `library.apply` (its composition is imported; the nested layer
//! it adds is removed again), so later applies find it in the project.

use effectcraft_keyframe::Value as KValue;
use effectcraft_project::{ItemId, Layer, LayerId, LayerSource, Node, PropGroup, Property};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, has_comp, layers_p, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd, lint, query};

const C: &str = "text.apply";
// The listing is `text.presetFamilies`.
// TODO(batch1-verify): the brief names it `text.presets`, but `text_anim.rs` already registers
// `text.presets` (the built-in animator presets; `tests_text.rs` and the unique-id test in
// `tests.rs` rely on it). Lane B's phrase popover reads `text.presets` for its list: point it at
// `text.presetFamilies` (`families[].variants[].short`) in the integration pass.
/// Elias's workshop comp: one layer per variation.
pub const WORKSHOP: &str = "Creating Text Presets";
/// The Motion Library text style `glow` applies.
pub const GLOW_STYLE: &str = "Caption glow + double shadow";
/// Most layers one call animates.
const MAX_LAYERS: usize = 256;
/// Most characters of a new layer's text.
const MAX_TEXT: usize = 500;
/// How long a new layer stays after its words and its animation (seconds).
// TODO(batch1-verify): mock value; Elias tunes it.
const HOLD: f64 = 0.5;

// ---------------------------------------------------------------- names

/// Near / mid / far: the variant letters a / b / c.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distance {
    Near,
    Mid,
    Far,
}

impl Distance {
    pub fn id(self) -> &'static str {
        match self {
            Distance::Near => "near",
            Distance::Mid => "mid",
            Distance::Far => "far",
        }
    }
    /// How far the travel goes against mid.
    // TODO(batch1-verify): mock values (0.5× / 1× / 1.6×); Elias tunes them.
    pub fn factor(self) -> f64 {
        match self {
            Distance::Near => 0.5,
            Distance::Mid => 1.0,
            Distance::Far => 1.6,
        }
    }
    pub fn letter(self) -> char {
        match self {
            Distance::Near => 'a',
            Distance::Mid => 'b',
            Distance::Far => 'c',
        }
    }
    pub fn from_letter(c: char) -> Option<Distance> {
        match c.to_ascii_lowercase() {
            'a' => Some(Distance::Near),
            'b' => Some(Distance::Mid),
            'c' => Some(Distance::Far),
            _ => None,
        }
    }
    pub fn parse(s: &str) -> Option<Distance> {
        match s.trim().to_ascii_lowercase().as_str() {
            "near" | "a" | "close" => Some(Distance::Near),
            "mid" | "b" | "middle" | "medium" => Some(Distance::Mid),
            "far" | "c" => Some(Distance::Far),
            _ => None,
        }
    }
}

/// A preset name read: `T3b Pop · mid` is family 3, variant `b`, title "Pop", mid.
#[derive(Clone, Debug, PartialEq)]
pub struct PresetName {
    pub family: u32,
    pub variant: Option<char>,
    pub title: String,
    pub distance: Option<Distance>,
}

impl PresetName {
    /// `T3b` (or `T3` without a variant).
    pub fn short(&self) -> String {
        match self.variant {
            Some(v) => format!("T{}{v}", self.family),
            None => format!("T{}", self.family),
        }
    }
    pub fn family_id(&self) -> String {
        format!("T{}", self.family)
    }
}

/// Read a preset name: `T3b Pop · mid`, `T3b Pop`, `T3b`, `t3`, `T12c Slide up - far`. Anything
/// else (`Text 1`, `T3x2`, `Title`) is `None`.
pub fn parse_name(name: &str) -> Option<PresetName> {
    let s = name.trim();
    let rest = s.strip_prefix('T').or_else(|| s.strip_prefix('t'))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let family: u32 = digits.parse().ok()?;
    // Digits are ASCII: their byte length is their count.
    let after = rest.get(digits.len()..)?;
    let mut it = after.chars();
    let (variant, tail) = match (it.next(), it.next()) {
        (Some(v), next) if v.is_ascii_alphabetic() && next.is_none_or(|n| !n.is_alphanumeric()) => (Some(v.to_ascii_lowercase()), after.get(1..).unwrap_or("")),
        (Some(v), _) if v.is_alphanumeric() => return None,
        _ => (None, after),
    };
    // "Pop · mid", "Pop - mid", "Pop mid", "Pop".
    let tail = tail.trim();
    let (mut title, mut distance) = (tail.to_string(), None);
    if let Some((a, b)) = tail.rsplit_once('·').or_else(|| tail.rsplit_once(" - ")).or_else(|| tail.rsplit_once('|')) {
        if let Some(d) = Distance::parse(b) {
            (title, distance) = (a.trim().to_string(), Some(d));
        }
    } else if let Some((a, b)) = tail.rsplit_once(' ')
        && let Some(d) = Distance::parse(b).filter(|_| b.len() > 1)
    {
        (title, distance) = (a.trim().to_string(), Some(d));
    }
    let distance = distance.or_else(|| variant.and_then(Distance::from_letter));
    Some(PresetName { family, variant, title: title.chars().take(200).collect(), distance })
}

// ---------------------------------------------------------------- where presets are

#[derive(Clone, Debug, PartialEq)]
enum Source {
    /// A layer of the project (the workshop comp, or a card's comp imported earlier).
    Layer { comp: ItemId, layer: LayerId },
    /// A Motion Library card (a library folder's entry when `local`, else the catalog's).
    Card { id: String, kind: String, local: bool },
}

#[derive(Clone, Debug)]
struct Preset {
    name: String,
    parsed: PresetName,
    source: Source,
}

impl Preset {
    fn source_json(&self, s: &Session) -> Value {
        match &self.source {
            Source::Layer { comp, layer } => {
                json!({"source": "project", "comp": comp.0, "compName": s.project.items.get(comp).map(|i| i.name.clone()), "layer": layer.0})
            }
            Source::Card { id, kind, local } => json!({"source": if *local { "library" } else { "motion" }, "card": id, "kind": kind}),
        }
    }
}

fn is_workshop(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == WORKSHOP.to_lowercase() || n.contains("text presets")
}

/// Layers named like presets that carry animation, the workshop comp's first.
fn project_presets(s: &Session) -> Vec<Preset> {
    let mut comps: Vec<(bool, u64, ItemId)> =
        s.project.items.values().filter(|i| i.as_comp().is_some()).map(|i| (!is_workshop(&i.name), i.id.0, i.id)).collect();
    comps.sort();
    let mut out = vec![];
    for (_, _, cid) in comps {
        let Some(comp) = s.project.comp(cid) else { continue };
        for l in comp.layers.iter() {
            let Some(parsed) = parse_name(&l.name) else { continue };
            if !l.props.any_animated() {
                continue;
            }
            out.push(Preset { name: l.name.clone(), parsed, source: Source::Layer { comp: cid, layer: l.id } });
        }
    }
    out
}

/// Library folders' entries, then the Motion Library catalog's cards, named like presets.
fn card_presets(s: &Session) -> Vec<Preset> {
    let mut out: Vec<Preset> = vec![];
    for e in s.motion_library_local().values() {
        if let Some(parsed) = parse_name(&e.name) {
            out.push(Preset { name: e.name.clone(), parsed, source: Source::Card { id: e.id.clone(), kind: e.kind.to_lowercase(), local: true } });
        }
    }
    // TODO(batch1-verify): lane C's `ml.saveTextPreset` (CX8) decides where text presets live
    // (category "Text animations"?); this reads any card named like one.
    if let Ok(client) = s.motion_library()
        && let Some(cat) = client.catalog()
    {
        for c in &cat.cards {
            if out.iter().any(|p| matches!(&p.source, Source::Card { id, .. } if *id == c.id)) {
                continue;
            }
            if let Some(parsed) = parse_name(&c.name) {
                out.push(Preset { name: c.name.clone(), parsed, source: Source::Card { id: c.id.clone(), kind: c.kind.to_lowercase(), local: false } });
            }
        }
    }
    out
}

/// Every preset, best source first: the project, a library folder, the catalog.
fn all_presets(s: &Session) -> Vec<Preset> {
    let mut out = project_presets(s);
    out.extend(card_presets(s));
    out
}

/// The preset `query` names (`T3b`, `T3`, `T3b Pop · mid`, or a family's title "Pop") for
/// `distance`, and the travel scale to apply to it: the family's variant for the distance when
/// there is one (scale 1), else the named (or mid, or first) variant scaled to the distance.
fn pick<'a>(all: &'a [Preset], query: &str, distance: Option<Distance>) -> Option<(&'a Preset, f64)> {
    let q = query.trim();
    let parsed = parse_name(q);
    let family = match &parsed {
        Some(n) => n.family,
        None => {
            // A full name or a family's title.
            let hit = all.iter().find(|p| p.name.eq_ignore_ascii_case(q)).or_else(|| all.iter().find(|p| p.parsed.title.eq_ignore_ascii_case(q)))?;
            hit.parsed.family
        }
    };
    let family_list: Vec<&Preset> = all.iter().filter(|p| p.parsed.family == family).collect();
    let asked_variant = parsed.as_ref().and_then(|n| n.variant).or_else(|| all.iter().find(|p| p.name.eq_ignore_ascii_case(q)).and_then(|p| p.parsed.variant));
    let want = distance.or_else(|| asked_variant.and_then(Distance::from_letter)).unwrap_or(Distance::Mid);
    let by_letter = |c: char| family_list.iter().copied().find(|p| p.parsed.variant == Some(c));
    if let Some(p) = by_letter(want.letter()) {
        return Some((p, 1.0));
    }
    let base = asked_variant.and_then(by_letter).or_else(|| by_letter('b')).or_else(|| family_list.first().copied())?;
    let own = base.parsed.distance.unwrap_or(Distance::Mid).factor();
    Some((base, want.factor() / own))
}

fn shorts(all: &[Preset]) -> String {
    let mut v: Vec<String> = all.iter().map(|p| p.parsed.short()).collect();
    v.sort();
    v.dedup();
    if v.is_empty() {
        format!("none yet: name layers of the comp “{WORKSHOP}” like “T3b Pop · mid”, or save them to the Motion Library")
    } else {
        v.join(", ")
    }
}

// ---------------------------------------------------------------- text.presetFamilies

fn families(s: &mut Session, _: &Value) -> Result<Value> {
    let all = all_presets(s);
    let mut fams: Vec<(u32, String, Vec<Value>, Vec<String>)> = vec![];
    for p in &all {
        let short = p.parsed.short();
        let i = match fams.iter().position(|f| f.0 == p.parsed.family) {
            Some(i) => i,
            None => {
                fams.push((p.parsed.family, p.parsed.title.clone(), vec![], vec![]));
                fams.len() - 1
            }
        };
        let Some(f) = fams.get_mut(i) else { continue };
        // One entry per short name: the best source (listed first).
        if f.3.contains(&short) {
            continue;
        }
        if f.1.is_empty() {
            f.1 = p.parsed.title.clone();
        }
        let mut v = json!({
            "short": short,
            "name": p.name,
            "variant": p.parsed.variant.map(String::from),
            "distance": p.parsed.distance.map(Distance::id),
        });
        if let (Some(o), Value::Object(src)) = (v.as_object_mut(), p.source_json(s)) {
            o.extend(src);
        }
        f.2.push(v);
        f.3.push(short);
    }
    fams.sort_by_key(|f| f.0);
    let workshop = s.project.items.values().find(|i| i.as_comp().is_some() && is_workshop(&i.name)).map(|i| i.id.0);
    Ok(json!({
        "families": fams.into_iter().map(|(n, title, mut variants, _)| {
            variants.sort_by(|a, b| a["short"].as_str().unwrap_or("").cmp(b["short"].as_str().unwrap_or("")));
            json!({"family": format!("T{n}"), "name": title, "variants": variants})
        }).collect::<Vec<_>>(),
        "workshop": workshop,
        "distances": {"near": Distance::Near.factor(), "mid": Distance::Mid.factor(), "far": Distance::Far.factor()},
        "glowStyle": GLOW_STYLE,
    }))
}

// ---------------------------------------------------------------- copying a template

/// Normalised word for matching (`"WORKS,"` → `works`).
fn clean(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// The earliest key of the properties a template hands on (layer time).
fn first_key(l: &Layer) -> Option<Tick> {
    let mut first: Option<Tick> = None;
    let mut see = |pr: &Property| {
        if let Some(t) = pr.keys.iter().map(|k| k.time).min() {
            first = Some(first.map_or(t, |f| f.min(t)));
        }
    };
    for path in ["transform", "text/animators", "effects"] {
        if let Some(g) = l.props.group(path) {
            g.walk("", &mut |_, pr| see(pr));
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

/// The template's main move: its speed peak (layer seconds) and the property's match id. The
/// position (else scale, rotation, anchor, opacity, anything) whose first eased move peaks.
fn lead_peak(l: &Layer) -> Option<(f64, String)> {
    let mut best: Option<(u8, f64, String)> = None;
    for path in ["transform", "text/animators"] {
        let Some(g) = l.props.group(path) else { continue };
        g.walk("", &mut |_, pr| {
            if pr.keys.len() < 2 {
                return;
            }
            let Some((t, _)) = (0..pr.keys.len() - 1).find_map(|i| lint::segment_peak(&pr.keys, i, pr.spatial)) else { return };
            let rank = lead_rank(&pr.match_id);
            if best.as_ref().is_none_or(|(r, bt, _)| rank < *r || (rank == *r && t < *bt)) {
                best = Some((rank, t, pr.match_id.clone()));
            }
        });
    }
    best.map(|(_, t, m)| (t, m))
}

/// Scale a key's eases and spatial tangents by `gain` (the value change scaled by it).
fn scale_motion(k: &mut effectcraft_keyframe::Keyframe, gain: f64) {
    if !gain.is_finite() || (gain - 1.0).abs() < 1e-12 {
        return;
    }
    for e in k.in_ease.as_mut_slice().iter_mut().chain(k.out_ease.as_mut_slice().iter_mut()) {
        e.speed *= gain;
    }
    for v in k.spatial_in.iter_mut().chain(k.spatial_out.iter_mut()) {
        *v *= gain;
    }
}

/// A text animator property with its travel scaled by `f` (offsets from rest: position, anchor,
/// rotation, skew, tracking from 0; scale from 100).
fn scale_animator_prop(pr: &mut Property, f: Option<f64>) {
    let Some(f) = f.filter(|f| f.is_finite() && (f - 1.0).abs() > 1e-12) else { return };
    let map = |v: &KValue| -> KValue {
        let c = v.components();
        if c.is_empty() {
            return v.clone();
        }
        let out: Vec<f64> = match pr.match_id.as_str() {
            "position" | "anchor" | "rotation" | "rotationX" | "rotationY" | "skew" | "tracking" | "lineSpacing" => c.iter().map(|x| x * f).collect(),
            "scale" => c.iter().map(|x| 100.0 + f * (x - 100.0)).collect(),
            _ => return v.clone(),
        };
        v.with_components(&out)
    };
    let scaled = matches!(pr.match_id.as_str(), "position" | "anchor" | "rotation" | "rotationX" | "rotationY" | "skew" | "tracking" | "lineSpacing" | "scale");
    if !scaled {
        return;
    }
    pr.value = map(&pr.value);
    for k in &mut pr.keys {
        k.value = map(&k.value);
        scale_motion(k, f);
    }
}

/// A transform key value moved onto the target: `rest_t` is where the template settles (its last
/// key), `rest` the target's own value; the move keeps its shape, scaled by `f`. Opacity stays
/// absolute; scale goes by ratio.
fn retarget(match_id: &str, v: &KValue, rest_t: &KValue, rest: &KValue, f: f64) -> KValue {
    let (a, rt, r) = (v.components(), rest_t.components(), rest.components());
    if a.is_empty() || r.is_empty() {
        return v.clone();
    }
    let out: Vec<f64> = (0..r.len())
        .map(|i| {
            let ri = r.get(i).copied().unwrap_or(0.0);
            let rti = rt.get(i).copied().unwrap_or(ri);
            let ai = a.get(i).copied().unwrap_or(rti);
            match match_id {
                "opacity" => ai,
                "scale" if rti.abs() > 1e-9 => ri * (1.0 + f * (ai / rti - 1.0)),
                _ => ri + f * (ai - rti),
            }
        })
        .collect();
    rest.with_components(&out)
}

/// The gain of a retargeted move's speeds (value units per second scale with it).
fn retarget_gain(match_id: &str, rest_t: &KValue, rest: &KValue, f: f64) -> f64 {
    match match_id {
        "opacity" => 1.0,
        "scale" => {
            let (rt, r) = (rest_t.components().first().copied().unwrap_or(0.0), rest.components().first().copied().unwrap_or(0.0));
            if rt.abs() > 1e-9 { f * (r / rt).abs() } else { f }
        }
        _ => f,
    }
}

/// What one target got.
#[derive(Default)]
struct Copied {
    animators: usize,
    properties: Vec<String>,
    effects: usize,
    skipped: Vec<String>,
}

/// Copy `tpl`'s animation onto `dst`: keys move by `shift` (template layer time → target layer
/// time), travel scales by `f`. Uids come from `next`.
fn copy_animation(tpl: &Layer, dst: &mut Layer, shift: Tick, f: f64, next: &mut u64) -> Copied {
    let mut done = Copied::default();
    // Text animators: appended (a second apply adds them again).
    // TODO(batch1-verify): replace animators an earlier text.apply added instead of stacking them?
    if let Some(src) = tpl.props.group("text/animators") {
        match dst.props.group_mut("text/animators") {
            Some(anims) => {
                for c in &src.children {
                    let Node::Group(g) = c else { continue };
                    let mut g = g.clone();
                    g.reassign_uids(next);
                    g.walk_mut(&mut |pr| {
                        for k in &mut pr.keys {
                            k.time += shift;
                        }
                    });
                    if let Some(props) = g.sub_mut("properties") {
                        props.walk_mut(&mut |pr| scale_animator_prop(pr, Some(f)));
                    }
                    anims.children.push(Node::Group(g));
                    done.animators += 1;
                }
            }
            None if src.children.iter().any(|c| matches!(c, Node::Group(_))) => done.skipped.push("text animators (not a text layer)".into()),
            None => {}
        }
    }
    // Keyed transform properties, relative to the target's own values.
    if let Some(tr) = tpl.props.sub("transform") {
        for pr_t in tr.props() {
            if pr_t.keys.is_empty() || pr_t.has_expression() {
                continue;
            }
            let Some(rest_t) = pr_t.keys.last().map(|k| k.value.clone()) else { continue };
            let Some(pr) = dst.props.sub_mut("transform").and_then(|g| g.get_mut(&pr_t.match_id)) else {
                done.skipped.push(format!("transform/{}", pr_t.match_id));
                continue;
            };
            if pr.static_only || std::mem::discriminant(&pr.value) != std::mem::discriminant(&rest_t) {
                done.skipped.push(format!("transform/{}", pr_t.match_id));
                continue;
            }
            // Where the target settles: its own value (its last key when it has keys already).
            let rest = pr.keys.last().map(|k| k.value.clone()).unwrap_or_else(|| pr.value.clone());
            let gain = retarget_gain(&pr_t.match_id, &rest_t, &rest, f);
            let mut keys = pr_t.keys.clone();
            for k in &mut keys {
                k.time += shift;
                k.value = retarget(&pr_t.match_id, &k.value, &rest_t, &rest, f);
                scale_motion(k, gain);
            }
            pr.keys = keys;
            done.properties.push(format!("transform/{}", pr_t.match_id));
        }
    }
    // Effects: the template's replace ones of the same kind and name.
    if let Some(fx_t) = tpl.props.sub("effects") {
        let list: Vec<PropGroup> = fx_t.groups().cloned().collect();
        if let Some(fx) = dst.props.sub_mut("effects") {
            for mut g in list {
                fx.children.retain(|c| !matches!(c, Node::Group(x) if x.match_id == g.match_id && x.name == g.name));
                g.reassign_uids(next);
                g.walk_mut(&mut |pr| {
                    for k in &mut pr.keys {
                        k.time += shift;
                    }
                });
                fx.children.push(Node::Group(g));
                done.effects += 1;
            }
        } else if !list.is_empty() {
            done.skipped.push("effects".into());
        }
    }
    done
}

/// The look of `tpl` (effects and layer styles) onto `dst`, for the glow style.
fn copy_look(tpl: &Layer, dst: &mut Layer, next: &mut u64) -> usize {
    let mut n = 0;
    if let Some(fx_t) = tpl.props.sub("effects") {
        let list: Vec<PropGroup> = fx_t.groups().cloned().collect();
        if let Some(fx) = dst.props.sub_mut("effects") {
            for mut g in list {
                fx.children.retain(|c| !matches!(c, Node::Group(x) if x.match_id == g.match_id && x.name == g.name));
                g.reassign_uids(next);
                fx.children.push(Node::Group(g));
                n += 1;
            }
        }
    }
    // TODO(batch1-verify): a layer without a `layerStyles` group gets the template's appended at
    // the end of its property tree; check the renderer and the Timeline find it there.
    if dst.can_have_styles()
        && let Some(st_t) = tpl.props.sub(effectcraft_project::styles::GROUP)
    {
        let mut g = st_t.clone();
        g.reassign_uids(next);
        match dst.props.children.iter().position(|c| matches!(c, Node::Group(x) if x.match_id == effectcraft_project::styles::GROUP)) {
            Some(i) => {
                if let Some(slot) = dst.props.children.get_mut(i) {
                    *slot = Node::Group(g);
                }
            }
            None => dst.props.children.push(Node::Group(g)),
        }
        n += 1;
    }
    n
}

// ---------------------------------------------------------------- text.apply

/// Where a preset's template layer is in the project, bringing a card in when needed.
fn template(s: &mut Session, p: &Preset, cid: ItemId, notes: &mut Vec<String>) -> Result<(ItemId, LayerId)> {
    match &p.source {
        Source::Layer { comp, layer } => Ok((*comp, *layer)),
        Source::Card { id, kind, .. } if kind == "preset" => Err(bad(
            C,
            format!(
                "“{}” is an animation preset card (.ecpreset): it holds no text animators to copy. Apply it with library.insert {{id: \"{id}\", layers}}, or save the workshop layer as a layers card",
                p.name
            ),
        )),
        Source::Card { id, .. } => {
            // TODO(batch1-verify): library.apply nests the card's comp in `cid`; the nested layer is
            // removed right away and the imported comp stays as the template's home.
            let r = s.execute("library.apply", json!({"id": id, "comp": cid.0}))?;
            if r.get("pending").and_then(Value::as_bool) == Some(true) {
                return Err(bad(C, format!("“{}” is still downloading ({}/{}): run text.apply again in a moment", p.name, r["done"], r["total"])));
            }
            let item = r.get("item").and_then(Value::as_u64).map(ItemId).ok_or_else(|| bad(C, format!("“{}”: the card brought no composition", p.name)))?;
            if let Some(nested) = r.get("layer").and_then(Value::as_u64).map(LayerId) {
                s.edit("Remove Nested Card", None, |proj, st| {
                    if let Some(comp) = proj.comp_mut(cid) {
                        comp.layers.retain(|l| l.id != nested);
                    }
                    st.selected_layers.retain(|l| *l != nested);
                    Ok(())
                })?;
            }
            let comp = s.project.comp(item).ok_or_else(|| bad(C, format!("“{}”: the card's composition is missing", p.name)))?;
            let short = p.parsed.short();
            let layer = comp
                .layers
                .iter()
                .find(|l| parse_name(&l.name).is_some_and(|n| n.short().eq_ignore_ascii_case(&short)))
                .or_else(|| comp.layers.iter().find(|l| matches!(l.source, LayerSource::Text) && l.props.any_animated()))
                .or_else(|| comp.layers.iter().find(|l| l.props.any_animated()))
                .ok_or_else(|| bad(C, format!("“{}”: the card's composition has no animated layer", p.name)))?;
            notes.push(format!("brought “{}” in from the Motion Library", p.name));
            Ok((item, layer.id))
        }
    }
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

/// One layer to animate: an existing one, or one made for words.
struct Job {
    layer: LayerId,
    created: bool,
    /// Comp seconds: where the template's first key lands without sync.
    start: f64,
    /// Comp seconds: the stressed syllable the peak lands on, with its word.
    accent: Option<(f64, String)>,
    /// A made layer's words (comp s).
    span: Option<(f64, f64)>,
}

fn distance_p(p: &Value) -> Result<Option<Distance>> {
    match p.get("distance").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(d)) if d.trim().is_empty() => Ok(None),
        Some(Value::String(d)) => Distance::parse(d).map(Some).ok_or_else(|| bad(C, format!("`distance` is near, mid or far, not {d:?}"))),
        Some(v) => Err(bad(C, format!("`distance` is near, mid or far, not {v}"))),
    }
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let query =
        str_p(p, "preset").map(str::trim).filter(|q| !q.is_empty()).ok_or_else(|| bad(C, "missing `preset` (e.g. \"T3b\"; text.presetFamilies lists them)"))?;
    let distance = distance_p(p)?;
    let sync = b_p(p, "sync").unwrap_or(false);
    let glow = b_p(p, "glow").unwrap_or(false);
    let word = str_p(p, "word").map(str::trim).filter(|w| !w.is_empty()).map(str::to_string);
    let words = words_p(p)?;
    let occurrence = match p.get("occurrence").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_u64().filter(|n| *n >= 1).ok_or_else(|| bad(C, format!("`occurrence` is 1 or more, not {v}")))?),
    };
    let cid = comp_id(s, p)?;
    let all = all_presets(s);
    let (preset, scale) =
        pick(&all, query, distance).map(|(p, f)| (p.clone(), f)).ok_or_else(|| bad(C, format!("no text preset {query:?}; there are: {}", shorts(&all))))?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(bad(C, "the distance scale is not a positive number"));
    }

    let snap = s.project.clone();
    let history = s.history.clone();
    let mut notes: Vec<String> = vec![];
    let result = run(s, p, cid, &preset, scale, sync, glow, word.as_deref(), words.as_ref(), occurrence, &mut notes);
    match result {
        Ok(mut r) => {
            super::batch::fold(s, &snap, &format!("Text Preset {}", preset.parsed.short()));
            r["notes"] = json!(notes);
            Ok(r)
        }
        Err(e) => {
            // Leave the project as it was (a card brought in, a layer made).
            s.project = snap;
            s.history = history;
            s.sanitize_state();
            s.bump();
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    s: &mut Session,
    p: &Value,
    cid: ItemId,
    preset: &Preset,
    scale: f64,
    sync: bool,
    glow: bool,
    word: Option<&str>,
    words: Option<&Words>,
    occurrence: Option<u64>,
    notes: &mut Vec<String>,
) -> Result<Value> {
    let (tc, tl) = template(s, preset, cid, notes)?;
    let tpl = s.project.comp(tc).and_then(|c| c.layer(tl)).cloned().ok_or_else(|| bad(C, format!("the template layer of “{}” is gone", preset.name)))?;
    let first = first_key(&tpl).ok_or_else(|| bad(C, format!("“{}” has no keyframes to copy", preset.name)))?;
    let span_t = last_key(&tpl).map(|l| (l - first).seconds().max(0.0)).unwrap_or(0.0);
    let lead = lead_peak(&tpl);

    // The layers: given ones, else a new one for `words` (or `word`), else the selected text layers.
    let explicit_layers = p.get("layers").or(p.get("layer")).is_some_and(|v| !v.is_null());
    let mut jobs: Vec<Job> = vec![];
    if explicit_layers || (words.is_none() && word.is_none()) {
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
        let phrase = match (words, word) {
            (Some(w), _) if sync => Some(said_of(s, cid, w, occurrence)?),
            (None, Some(w)) if sync => Some(find_said(s, cid, w, occurrence)?),
            _ => None,
        };
        for lid in text_ids {
            let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
            let Some(l) = comp.layer(lid) else { continue };
            let start = l.in_point.seconds();
            let accent = if !sync {
                None
            } else if let Some(ph) = &phrase {
                accent_in(&ph.words, word).or_else(|| ph.accent.map(|t| (t, word.unwrap_or("").to_string())))
            } else {
                // The words said while the layer shows.
                let r = s.execute("layer.words", json!({"comp": cid.0, "layer": lid.0}))?;
                accent_in(r.get("words").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), word)
            };
            if sync && accent.is_none() {
                notes.push(format!("layer {}: no stressed syllable to sync on, so it starts at its in point", lid.0));
            }
            jobs.push(Job { layer: lid, created: false, start, accent, span: None });
        }
    } else {
        let said = match (words, word) {
            (Some(w), _) => said_of(s, cid, w, occurrence)?,
            (None, Some(w)) => find_said(s, cid, w, occurrence)?,
            (None, None) => return Err(bad(C, "give `layers`, `words` or `word`")),
        };
        let text = match words {
            Some(Words::Text(t)) => t.clone(),
            _ => word.unwrap_or("").to_string(),
        };
        let accent = if sync { accent_in(&said.words, word).or_else(|| said.accent.map(|t| (t, text.clone()))) } else { None };
        if sync && accent.is_none() {
            notes.push(format!("{text:?} has no stressed syllable in the speech map: the animation starts with the words"));
        }
        // The text as said in the map (a phrase's words joined), else as given.
        let shown: String = {
            let said_text: Vec<&str> = said.words.iter().filter_map(|w| w.get("word").and_then(Value::as_str)).collect();
            if said_text.is_empty() { text.clone() } else { said_text.join(" ") }
        };
        let r = s.execute("layer.newText", json!({"comp": cid.0, "text": shown, "name": shown.chars().take(60).collect::<String>()}))?;
        let lid = r.get("layer").and_then(Value::as_u64).map(LayerId).ok_or_else(|| bad(C, "layer.newText made no layer"))?;
        jobs.push(Job { layer: lid, created: true, start: said.start, accent, span: Some((said.start, said.end)) });
    }

    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    // Template seconds from its first key to its peak.
    let rel = lead.as_ref().map(|(t, _)| t - first.seconds());
    if sync && lead.is_none() {
        notes.push(format!("“{}” has no eased move with a speed peak: sync starts it on the word instead", preset.name));
    }
    let tpl_doc = tpl.props.prop("text/sourceText").map(|pr| pr.value.clone());
    let targets: Vec<LayerId> = jobs.iter().map(|j| j.layer).collect();

    let reports = s.edit(&format!("Apply Text Preset {}", preset.parsed.short()), None, |proj, st| {
        let mut next = proj.next_id;
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut reports = vec![];
        for job in &jobs {
            let Some(l) = comp.layer_mut(job.layer) else { continue };
            // Comp time of the template's first key.
            let base_s = match (job.accent.as_ref(), rel) {
                (Some((a, _)), Some(rel)) => a - rel,
                (Some((a, _)), None) => *a,
                _ => job.start,
            };
            let base = fr.snap_nearest(Tick::from_seconds_f64(base_s));
            if job.created {
                // The template's font, size and fill with the words' text.
                if let (Some(KValue::Text(doc_t)), Some(pr)) = (tpl_doc.as_ref(), l.props.prop_mut("text/sourceText"))
                    && let KValue::Text(cur) = &pr.value
                {
                    let mut doc = doc_t.clone();
                    doc.set_text(&cur.text);
                    // TODO(batch1-verify): a template with a paragraph box keeps its box here.
                    pr.value = KValue::Text(doc);
                }
                if let Some((a, b)) = job.span {
                    let inn = fr.snap_nearest(Tick::from_seconds_f64(a)).min(base);
                    let end = (base + Tick::from_seconds_f64(span_t)).max(Tick::from_seconds_f64(b)) + Tick::from_seconds_f64(HOLD);
                    l.in_point = inn;
                    l.out_point = fr.snap_nearest(end).max(inn + fr.frame_duration());
                }
            }
            let shift = l.layer_time(base) - first;
            let copied = copy_animation(&tpl, l, shift, scale, &mut next);
            let mut rep = json!({
                "layer": l.id.0,
                "name": l.name,
                "created": job.created,
                "start": (base.seconds() * 1000.0).round() / 1000.0,
                "startFrame": (base.seconds() * fps).round() as i64,
                "animators": copied.animators,
                "properties": copied.properties,
                "effects": copied.effects,
            });
            if !copied.skipped.is_empty() {
                rep["skipped"] = json!(copied.skipped);
            }
            if base < l.in_point {
                rep["warning"] = json!("the animation starts before the layer's in point");
            }
            if let (Some((a, w)), Some(rel)) = (job.accent.as_ref(), rel) {
                let peak = base.seconds() + rel;
                rep["sync"] = json!({
                    "word": w,
                    "accentFrame": ((a * fps) * 100.0).round() / 100.0,
                    "peakFrame": ((peak * fps) * 100.0).round() / 100.0,
                    "offsetFrames": (((peak - a) * fps) * 100.0).round() / 100.0,
                    "property": lead.as_ref().map(|(_, m)| m.clone()),
                });
            }
            reports.push(rep);
        }
        st.selected_layers = targets.clone();
        proj.next_id = next;
        Ok(reports)
    })?;

    let glow_report = if glow { apply_glow(s, cid, &targets, notes)? } else { Value::Null };
    let mut out = json!({
        "preset": {
            "short": preset.parsed.short(),
            "name": preset.name,
            "distance": preset.parsed.distance.map(Distance::id),
            "travelScale": (scale * 1000.0).round() / 1000.0,
        },
        "comp": cid.0,
        "sync": sync,
        "layers": reports,
    });
    if let (Some(o), Value::Object(src)) = (out["preset"].as_object_mut(), preset.source_json(s)) {
        o.extend(src);
    }
    if !glow_report.is_null() {
        out["glow"] = glow_report.clone();
        if let Some(f) = glow_report.get("followUp") {
            out["followUp"] = json!([f]);
        }
    }
    Ok(out)
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
            // TODO(batch1-verify): `motionLibrary.apply` is a UI command (the Motion Library's plug
            // applies text styles): lane A's chips / lane B's popover run `followUp` after
            // text.apply; agents run it with command_run. The target layers are selected.
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
            "Text Preset Families",
            "{} → {families: [{family (\"T3\"), name (\"Pop\"), variants: [{short (\"T3b\"), name (\"T3b Pop · mid\"), variant, distance (near|mid|far), source (project|library|motion), comp?, compName?, layer?, card?, kind?}]}], workshop (the comp “Creating Text Presets”, if open), distances: {near, mid, far} (travel scales), glowStyle}: Elias's text animations, from the workshop comp, library folders and the Motion Library",
            families
        ),
        cmd!(
            "text.apply",
            "Apply Text Preset",
            [],
            None,
            "{preset (\"T3b\", \"T3\" (the distance's variant, default mid) or the full name \"T3b Pop · mid\"), layers? (text layers; default the selected ones unless `words` is given), words? (a phrase of the comp's speech map as text, or [from, to] / {from, to} speech-map word indices: a new text layer shows it, timed to it), word? (the word to sync on; also the text when neither layers nor words are given), occurrence? (1-based, of words or word), sync? (bool: the animation's speed peak lands on the stressed syllable of `word`, else the first stressed word), glow? (bool: also the Motion Library style “Caption glow + double shadow”), distance?: near|mid|far (the family's variant; without it the travel scales 0.5× / 1× / 1.6×), comp?} → {preset: {short, name, distance, travelScale, source}, comp, sync, layers: [{layer, name, created, start, startFrame, animators, properties, effects, skipped?, warning?, sync?: {word, accentFrame, peakFrame, offsetFrames, property}}], glow?, followUp? ([{command, params}] the app runs next), notes}. One undo step",
            has_comp,
            apply
        ),
    ]
}

#[cfg(test)]
pub(crate) fn parse_for_tests(name: &str) -> Option<PresetName> {
    parse_name(name)
}
