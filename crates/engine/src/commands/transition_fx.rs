//! The FX cuts of `transition.apply` (Animation ▸ Transitions ▸ FX Cut): Elias's 15 approved
//! scene changes carried by a light, a distortion, a cover or a burn instead of a zoom (light-leak,
//! overexposure-burn, film-burn, glitch, chromatic-aberration, ripple, vhs-tape, shutter,
//! clock-wipe, grid-dissolve, gravity-drop, morph-circle, blur-through, directional-blur,
//! page-burn). A port of motion-kit's shelf part `fx-cut` (`shelf/transitions/fx-cut.jsx`,
//! verified against the approved renders at 24/25/29.97/59.94); every number, overlay and
//! expression comes from the same data table, shipped here as `presets/fx-cut.kinds.json`
//! (re-copied when the shelf table changes, see its sidecar).
//!
//! `at` is the cut: the old shot's out point, the first frame the new picture owns. The shots are
//! trimmed to the kind (`shots`: clock-wipe, gravity-drop, blur-through, directional-blur and
//! page-burn start the new shot 0.27–1.2 s early, under the old one; page-burn also ends the old
//! shot when its burn starts, a copy burns). Their transforms are never touched: moves on a shot are
//! effects (Transform, blurs, Exposure, Radial Wipe); copies of the cut are duplicates of the shots
//! in a precomp `FxCut <kind> Scenes`; lights, covers and strips are overlays (solids, radial-
//! gradient glows, rect / ellipse shape layers, precomps), with masks, track mattes, blend modes
//! and expressions (`${refFps}`, `${amount}`: a Slider Control keyed with the kind's envelope,
//! `${i}`: a repeat's index). Times are seconds after the cut × `dur` / the kind's window, keyed
//! on the comp's own frames. Everything added is named `FxCut <kind> …` and tagged (layer / item
//! comment `FxCut <old>-><new>`), so re-applying on the pair replaces it.
//!
//! The scene cuts ([`super::transition_scene`], `SceneCut …`) are built here too: the same schema
//! plus effect pivots, hold keys and straight (spatially linear) paths, polygon mask paths, copies
//! of a shot with their own trims or turned in 3-D, and sized solids ([`Family`]).

use std::sync::OnceLock;

use effectcraft_color::Label;
use effectcraft_keyframe::{Interp, Keyframe, ShapePath};
use effectcraft_project::{Expression, FootageKind, ItemId, ItemKind, Layer, LayerId, LayerSource, Node, PropGroup, Property, Value as KV};
use effectcraft_time::Tick;
use serde_json::{Map, Value, json};

use super::transition::{
    Applied, C, Cx, Removed, arr, check_meets, cut_frame, finish_track, fmt_s, fps_of, layer_of, layer_times, new_effect, pt, put_below, run, set_props,
    set_values, tagged, value_like,
};
use super::{bad, layer_mut};
use crate::taste::Profile;
use crate::{EngineError, Result, Session};

/// The FX-cut kinds: a copy of motion-kit's `shelf/transitions/fx-cut.kinds.json`.
pub const FX_CUT_JSON: &str = include_str!("../../presets/fx-cut.kinds.json");

/// Everything an FX cut adds is named with this prefix (effects, layers, precomps).
pub(super) const PREFIX: &str = "FxCut ";

/// Which shelf table a cut comes from: the FX cuts (`FxCut …`) or the scene cuts (`SceneCut …`,
/// [`super::transition_scene`]). Both are built by [`apply_family`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Family {
    Fx,
    Scene,
}

impl Family {
    pub(super) fn prefix(self) -> &'static str {
        match self {
            Family::Fx => PREFIX,
            Family::Scene => super::transition_scene::PREFIX,
        }
    }
    /// The reply's `family`.
    pub(super) fn id(self) -> &'static str {
        match self {
            Family::Fx => "fx-cut",
            Family::Scene => "scene-cut",
        }
    }
    /// The submenu (Animation ▸ Transitions ▸ …) and the undo step's name.
    fn menu(self) -> &'static str {
        match self {
            Family::Fx => "FX Cut",
            Family::Scene => "Scene Cut",
        }
    }
    /// The approved rebuild batch the kinds come from (a layer's reference-ease comment).
    fn batch(self) -> &'static str {
        match self {
            Family::Fx => "transitions-b",
            Family::Scene => "transitions-a",
        }
    }
    fn table(self) -> Result<&'static Value> {
        match self {
            Family::Fx => table(),
            Family::Scene => super::transition_scene::table(),
        }
    }
    /// The tag on everything this family adds for a pair.
    pub(super) fn tag(self, old: LayerId, new: LayerId) -> String {
        format!("{}{}->{}", self.prefix(), old.0, new.0)
    }
}
/// `${refFps}`: the approved comps' rate (per-frame randomness and the ripple's phase stay on the
/// reference frame grid at any comp fps).
const REF_FPS: &str = "29.97002997";
/// Overlays, effects or a nested comp's layers per spec.
const MAX_ITEMS: usize = 32;
/// Most copies a `repeat` makes.
const MAX_REPEAT: usize = 64;
/// How deep overlays nest (a comp overlay's layers).
const MAX_DEPTH: usize = 4;
/// Longest `dur`, seconds.
const MAX_DUR: f64 = 60.0;

pub(super) fn table() -> Result<&'static Value> {
    static T: OnceLock<std::result::Result<Value, String>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(FX_CUT_JSON).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| EngineError::Other(format!("{C}: the fx-cut table doesn't parse: {e}")))
}

/// The FX-cut kinds in menu order.
pub(super) fn kinds() -> Vec<String> {
    table()
        .ok()
        .and_then(|t| t.get("order"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub(super) fn is_kind(kind: &str) -> bool {
    table().ok().and_then(|t| t.get("kinds")).and_then(|k| k.get(kind)).is_some()
}

fn kind_spec(kind: &str) -> Result<&'static Value> {
    table()?.get("kinds").and_then(|k| k.get(kind)).ok_or_else(|| bad(C, format!("unknown FX cut `{kind}`; kinds: {}", kinds().join(", "))))
}

/// `transition.kinds`' FX-cut rows.
pub(super) fn list() -> Vec<Value> {
    let Ok(t) = table() else { return vec![] };
    let default = t.get("default").and_then(Value::as_str).unwrap_or("light-leak");
    kinds()
        .iter()
        .filter_map(|id| {
            let k = t.get("kinds")?.get(id)?;
            Some(json!({
                "kind": id,
                "family": "fx-cut",
                "default": id == default,
                "title": k.get("title"),
                "energy": k.get("energy"),
                "window": k.get("window"),
                "land": k.get("land"),
                "looks": k.get("looks"),
                "use": k.get("use"),
                "dir": null,
                "dirs": [],
            }))
        })
        .collect()
}

/// The kind an `FxCut <kind> …` / `SceneCut <kind> …` name belongs to.
fn kind_of<'a>(prefix: &str, name: &'a str) -> Option<&'a str> {
    name.strip_prefix(prefix)?.split(' ').next().filter(|k| !k.is_empty())
}

/// A previous FX / scene cut on the pair that trimmed the shots: where they go back to (the old
/// shot's out point, the new shot's in point), both on its cut. The cut is the old shot's out point,
/// or, when the kind ended the old shot early (page-burn, diagonal-split, card-flip-3d), the out
/// point of its overlay that ends on the cut.
pub(super) fn previous_trims(fam: Family, s: &Session, cid: ItemId, old: LayerId, new: LayerId) -> Option<(Option<Tick>, Option<Tick>)> {
    let t = fam.table().ok()?;
    let (prefix, tag) = (fam.prefix(), fam.tag(old, new));
    let tag = tag.as_str();
    let comp = s.project.comp(cid)?;
    let mut kind: Option<String> = None;
    for l in comp.layers.iter() {
        if l.id == old || l.id == new {
            if let Some(k) = l.effects().and_then(|fx| fx.groups().find_map(|g| kind_of(prefix, &g.name))) {
                kind = Some(k.to_string());
            }
        } else if tagged(&l.comment, tag)
            && let Some(k) = kind_of(prefix, &l.name)
        {
            kind = Some(k.to_string());
        }
    }
    let kind = kind?;
    let k = t.get("kinds")?.get(&kind)?;
    let sh = k.get("shots")?;
    let moved = |role: &str, end: &str| sh.get(role).and_then(|r| r.get(end)).and_then(Value::as_f64).is_some_and(|x| x != 0.0);
    let (trims_old, trims_new) = (moved("old", "out"), moved("new", "in"));
    let cut = if trims_old {
        arr(k, "overlays").iter().filter(|o| o.get("out").and_then(Value::as_f64) == Some(0.0)).find_map(|o| {
            let name = format!("{prefix}{kind} {}", o.get("name")?.as_str()?);
            comp.layers.iter().find(|l| l.name == name && tagged(&l.comment, tag)).map(|l| l.out_point)
        })?
    } else {
        comp.layer(old)?.out_point
    };
    Some((trims_old.then_some(cut), trims_new.then_some(cut)))
}

/// `colour`: one cover colour, or a palette (grid-dissolve's cells).
enum Colour {
    One(String),
    Palette(Vec<String>),
}

fn colour_p(p: &Value) -> Result<Option<Colour>> {
    let hex = |v: &Value| {
        v.as_str().filter(|s| s.len() == 7 && s.starts_with('#') && s.get(1..).is_some_and(|h| h.chars().all(|c| c.is_ascii_hexdigit()))).map(str::to_string)
    };
    let err = || bad(C, "`colour` is #rrggbb, or a list of them (a palette)");
    match p.get("colour").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v @ Value::String(_)) => hex(v).map(|c| Some(Colour::One(c))).ok_or_else(err),
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= MAX_REPEAT => {
            a.iter().map(|v| hex(v).ok_or_else(err)).collect::<Result<Vec<_>>>().map(|c| Some(Colour::Palette(c)))
        }
        Some(_) => Err(err()),
    }
}

/// One FX cut being applied: the comp, its frames, the pair and the options.
struct Fx<'a> {
    fam: Family,
    cx: Cx<'a>,
    k: &'a Value,
    cid: ItemId,
    old: LayerId,
    new: LayerId,
    cut: Tick,
    /// The comp's size, and its px per 1920×1080 px (`u`: along the diagonal).
    w: u32,
    h: u32,
    sx: f64,
    sy: f64,
    u: f64,
    tag: String,
    colour: Option<Colour>,
    expr_check: Option<fn(&str) -> std::result::Result<(), String>>,
}

impl Fx<'_> {
    fn cw(&self) -> f64 {
        f64::from(self.w)
    }
    fn ch(&self) -> f64 {
        f64::from(self.h)
    }
    fn name(&self, what: &str) -> String {
        format!("{}{} {what}", self.fam.prefix(), self.cx.kind)
    }
}

/// Where the next overlay goes: above everything so far (`top`), under both shots (`lower` is the
/// lower shot) or right above the new shot (`above_new`).
struct Place {
    top: LayerId,
    lower: LayerId,
    above_new: LayerId,
}

/// What was added, for the reply and the tags.
#[derive(Default)]
struct Built {
    effects: Vec<Value>,
    /// Layers added to the comp (tagged at the end).
    made: Vec<LayerId>,
    /// Layers whose keys got one of Elias's Graphs (their tag names the reference ease).
    eased: Vec<LayerId>,
    /// Precomps added (tagged as items).
    items: Vec<ItemId>,
    /// The copies-of-the-cut precomps by their key (shots | pad | inner effects).
    scenes: Vec<(String, ItemId)>,
    /// Comp overlays by name (`of` refers to them).
    comps: Vec<(String, ItemId)>,
}

/// How a track's table values become property values.
#[derive(Clone, Copy)]
enum Vals {
    /// Positions are px of 1920×1080 × `k`, or offsets from `base` with `offset`; a layer's
    /// `transform/scale` number is `[v, v, 100]`; anything else as it is.
    Plain { base: [f64; 2], k: [f64; 2] },
    /// A mask path: a circle about `c` (layer px), radius = the value × `u`.
    Circle { c: [f64; 2], u: f64 },
    /// A mask path: a straight-edged closed polygon, its vertices px of 1920×1080 × `k`.
    Poly { k: [f64; 2] },
}

/// A circle as 4 Bezier vertices (as transitions-b's masks).
fn circle(c: [f64; 2], r: f64) -> KV {
    let q = 0.5523 * r;
    let [x, y] = c;
    KV::path(ShapePath {
        vertices: vec![[x, y - r], [x + r, y], [x, y + r], [x - r, y]],
        in_tangents: vec![[-q, 0.0], [0.0, -q], [q, 0.0], [0.0, q]],
        out_tangents: vec![[q, 0.0], [0.0, q], [-q, 0.0], [0.0, -q]],
        closed: true,
        feather: vec![],
    })
}

/// A straight-edged closed path through `pts` (px of 1920×1080 × `k`).
fn polygon(pts: &[Value], k: [f64; 2]) -> Option<KV> {
    if pts.len() < 2 || pts.len() > super::transition::MAX_KEYS {
        return None;
    }
    let vertices = pts.iter().map(|p| pt(Some(p)).map(|p| [p[0] * k[0], p[1] * k[1]])).collect::<Option<Vec<_>>>()?;
    let n = vertices.len();
    Some(KV::path(ShapePath { vertices, in_tangents: vec![[0.0; 2]; n], out_tangents: vec![[0.0; 2]; n], closed: true, feather: vec![] }))
}

fn fx_value(kind: &str, path: &str, v: &Value, tr: &Value, m: Vals, like: &KV) -> Result<KV> {
    let err = || bad(C, format!("{kind}: {path} doesn't take {v}"));
    let (base, k) = match m {
        Vals::Circle { c, u } => {
            let r = v.as_f64().filter(|r| r.is_finite() && *r >= 0.0).ok_or_else(err)?;
            return Ok(circle(c, r * u));
        }
        Vals::Poly { k } => return v.as_array().and_then(|a| polygon(a, k)).ok_or_else(err),
        Vals::Plain { base, k } => (base, k),
    };
    let last = path.rsplit('/').next().unwrap_or(path);
    let j = if last == "position" {
        let p = pt(Some(v)).ok_or_else(err)?;
        let offset = tr.get("offset").and_then(Value::as_bool).unwrap_or(false);
        let q = if offset { [base[0] + p[0] * k[0], base[1] + p[1] * k[1]] } else { [p[0] * k[0], p[1] * k[1]] };
        if path.starts_with("transform/") { json!([q[0], q[1], 0.0]) } else { json!(q) }
    } else if path == "transform/scale"
        && let Some(x) = v.as_f64()
    {
        json!([x, x, 100.0])
    } else {
        v.clone()
    };
    value_like(&j, like).ok_or_else(err)
}

/// Key one track (layer times `times`, one per comp frame of `frames`) with its eases: a Graph,
/// `linear`, or `hold` (the value jumps at the next key); `spatial: "linear"` makes the path
/// straight between keys (no overshoot). Returns whether a segment got one of Elias's Graphs.
fn key_track(cx: &Cx, pr: &mut Property, path: &str, tr: &Value, frames: &[i64], times: &[Tick], m: Vals) -> Result<bool> {
    let kind = cx.kind;
    let vals = arr(tr, "v");
    if vals.len() != frames.len() || times.len() != frames.len() {
        return Err(bad(C, format!("{kind}: {path} has {} times but {} values", frames.len(), vals.len())));
    }
    if pr.static_only {
        return Err(bad(C, format!("{kind}: {} can't be keyframed", pr.name)));
    }
    let mut keys: Vec<Keyframe> = Vec::with_capacity(vals.len());
    for (v, t) in vals.iter().zip(times) {
        let kv = fx_value(kind, path, v, tr, m, &pr.value)?;
        keys.push(Keyframe::new(*t, kv));
    }
    let eases = arr(tr, "ease");
    let hold = |e: &Value| e.as_str() == Some("hold");
    let held: Vec<usize> = eases.iter().enumerate().filter(|(_, e)| hold(e)).map(|(i, _)| i).collect();
    let straight = tr.get("spatial").and_then(Value::as_str) == Some("linear");
    if held.is_empty() {
        finish_track(cx, pr, keys, tr, 1.0)?;
    } else {
        // The Graphs go on the other segments; a hold segment stays linear until its key holds.
        let mut t2 = tr.clone();
        if let Some(Value::Array(es)) = t2.get_mut("ease") {
            for e in es.iter_mut().filter(|e| hold(e)) {
                *e = json!("linear");
            }
        }
        finish_track(cx, pr, keys, &t2, 1.0)?;
    }
    for i in held {
        if let Some(k) = pr.keys.get_mut(i) {
            k.out_interp = Interp::Hold;
        }
    }
    if straight && pr.spatial {
        for k in &mut pr.keys {
            (k.spatial_auto, k.spatial_continuous, k.spatial_in, k.spatial_out) = (false, false, [0.0; 3], [0.0; 3]);
        }
    }
    Ok(eases.iter().any(|e| e.as_str() != Some("linear") && !hold(e)))
}

/// An expression from the table with its placeholders filled in; one the expression engine can't
/// parse (when the session has its checker) is an error.
fn expr_text(fx: &Fx, text: &Value, idx: usize) -> Result<String> {
    let kind = fx.cx.kind;
    let text = text.as_str().ok_or_else(|| bad(C, format!("{kind}: an expression is a string")))?;
    let amount = format!("effect(\"{}\")(\"Slider\")", fx.name("Amount"));
    let t = text.replace("${refFps}", REF_FPS).replace("${i}", &idx.max(1).to_string()).replace("${amount}", &amount);
    if t.contains("${") {
        return Err(bad(C, format!("{kind}: an expression has a placeholder this version doesn't know: {t}")));
    }
    if let Some(check) = fx.expr_check {
        check(&t).map_err(|e| bad(C, format!("{kind}: the expression `{t}` doesn't parse: {e}")))?;
    }
    Ok(t)
}

/// Whether a spec's expressions (its own or its effects') read `${amount}`.
fn uses_amount(spec: &Value) -> bool {
    let reads = |m: Option<&Value>| m.and_then(Value::as_object).is_some_and(|m| m.values().any(|v| v.as_str().is_some_and(|t| t.contains("${amount}"))));
    reads(spec.get("expr")) || arr(spec, "effects").iter().any(|e| reads(e.get("expr")))
}

/// A layer's px size: its source, else its effect bounds, else the comp.
fn layer_size(s: &Session, cid: ItemId, lid: LayerId) -> Result<[f64; 2]> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let (fx_size, _) = effectcraft_render::effect_bounds(&s.project, comp, l);
    let (w, h) = effectcraft_render::source_size(&s.project, l);
    let (w, h) = if w == 0 || h == 0 { (fx_size[0], fx_size[1]) } else { (f64::from(w), f64::from(h)) };
    Ok(if w > 0.0 && h > 0.0 { [w, h] } else { [f64::from(comp.width), f64::from(comp.height)] })
}

/// Where a footage or precomp layer's source starts in comp time (`None`: a solid, a still, a
/// shape, or time remapped: it has pictures at any time).
fn source_start(s: &Session, l: &Layer) -> Option<Tick> {
    if l.props.get("timeRemap").is_some() {
        return None;
    }
    let dur = match &l.source {
        LayerSource::Footage { item } => match s.project.item(*item).map(|i| &i.kind) {
            Some(ItemKind::Footage(f)) if f.kind != FootageKind::Still => Some(f.looped_duration()),
            _ => None,
        },
        LayerSource::Comp { item } => s.project.comp(*item).map(|c| c.duration),
        _ => None,
    }?;
    let (a, b) = (l.comp_time(Tick::ZERO), l.comp_time(dur));
    Some(a.min(b))
}

/// A spec's effects on a layer (stack order, after its own), its Amount slider when its
/// expressions read `${amount}`, then their keys and expressions (in that order: the expressions
/// read the slider). `base`: the layer px a Transform effect is centred on and offset tracks add
/// to; `k`: layer px per 1920×1080 px; `idx`: a repeat's index (`${i}`).
#[allow(clippy::too_many_arguments)]
fn layer_effects(s: &mut Session, fx: &Fx, comp: ItemId, lid: LayerId, spec: &Value, base: [f64; 2], k: [f64; 2], idx: usize, b: &mut Built) -> Result<()> {
    let kind = fx.cx.kind;
    let effs = arr(spec, "effects");
    if effs.len() > MAX_ITEMS {
        return Err(bad(C, format!("{kind}: too many effects")));
    }
    let amount = fx.k.get("amount").filter(|_| uses_amount(spec));
    if effs.is_empty() && amount.is_none() {
        return Ok(());
    }
    let c = s.project.comp(comp).ok_or(EngineError::NoComp)?;
    let l = c.layer(lid).ok_or_else(|| bad(C, "no layer"))?.clone();
    let (fx_size, _) = effectcraft_render::effect_bounds(&s.project, c, &l);
    // Every track and expression, planned before the edit (errors change nothing).
    let mut plans: Vec<(usize, String, Value, Vec<i64>, Vec<Tick>)> = vec![];
    let mut exprs: Vec<(usize, String, String)> = vec![];
    for (i, e) in effs.iter().enumerate() {
        if let Some(Value::Object(keys)) = e.get("keys") {
            for (p, tr) in keys {
                let frames = fx.cx.frames(tr, "fx")?;
                let times = layer_times(fx.cx.fr, &l, &frames);
                plans.push((i, p.clone(), tr.clone(), frames, times));
            }
        }
        if let Some(Value::Object(m)) = e.get("expr") {
            for (p, t) in m {
                exprs.push((i, p.clone(), expr_text(fx, t, idx)?));
            }
        }
    }
    let amount = match amount {
        Some(tr) => {
            let frames = fx.cx.frames(tr, "fx")?;
            let times = layer_times(fx.cx.fr, &l, &frames);
            Some((tr.clone(), frames, times))
        }
        None => None,
    };
    let label = format!("{}{kind}", fx.fam.prefix());
    let m = Vals::Plain { base, k };
    let (made, eased) = s.edit(&label, None, |proj, _| {
        let mut next = proj.next_id;
        let layer = layer_mut(proj, comp, lid)?;
        let lname = layer.name.clone();
        let list = layer.props.sub_mut("effects").ok_or_else(|| bad(C, format!("“{lname}” can't take effects")))?;
        let mut groups: Vec<PropGroup> = vec![];
        let mut eased = false;
        for (i, e) in effs.iter().enumerate() {
            let id = e.get("effect").and_then(Value::as_str).unwrap_or_default();
            let name = format!("{label} {}", e.get("name").and_then(Value::as_str).unwrap_or("Effect"));
            let mut g = new_effect(&mut next, id, &name, fx_size, kind)?;
            set_values(&mut g, e.get("set"), kind)?;
            // A pivot (px of the 1920×1080 shot) moves the effect's centre: anchor = position = it,
            // and offset tracks add to it.
            let b = pt(e.get("pivot")).map_or(base, |p| [p[0] * k[0], p[1] * k[1]]);
            if id == "ec.distort.transform" {
                set_values(&mut g, Some(&json!({"anchor": b, "position": b})), kind)?;
            }
            for (_, p, tr, frames, times) in plans.iter().filter(|x| x.0 == i) {
                let pr = g.prop_mut(p).ok_or_else(|| bad(C, format!("{kind}: {name} has no parameter `{p}`")))?;
                eased |= key_track(&fx.cx, pr, p, tr, frames, times, Vals::Plain { base: b, k })?;
            }
            for (_, p, t) in exprs.iter().filter(|x| x.0 == i) {
                let pr = g.prop_mut(p).ok_or_else(|| bad(C, format!("{kind}: {name} has no parameter `{p}`")))?;
                pr.expr = Some(Expression { text: t.clone(), enabled: true });
            }
            groups.push(g);
        }
        if let Some((tr, frames, times)) = &amount {
            let name = format!("{label} Amount");
            let mut g = new_effect(&mut next, "ec.control.slider", &name, fx_size, kind)?;
            let pr = g.prop_mut("slider").ok_or_else(|| bad(C, format!("{kind}: {name} has no slider")))?;
            eased |= key_track(&fx.cx, pr, "slider", tr, frames, times, m)?;
            groups.push(g);
        }
        let mut made = vec![];
        for g in groups {
            made.push((g.uid, g.name.clone()));
            list.children.push(Node::Group(g));
        }
        let n0 = list.children.len().saturating_sub(made.len());
        proj.next_id = next;
        let made: Vec<Value> = made
            .into_iter()
            .enumerate()
            .map(|(j, (uid, name))| json!({"comp": comp.0, "layer": lid.0, "effect": uid, "name": name, "path": format!("effects/#{}", n0 + j + 1)}))
            .collect();
        Ok((made, eased))
    })?;
    b.effects.extend(made);
    if eased && !b.eased.contains(&lid) {
        b.eased.push(lid);
    }
    Ok(())
}

/// A spec's layer values (`set`), keys (`keys`: positions px of 1920×1080 about the comp, or
/// offsets from its centre) and expressions (`expr`).
fn layer_props(s: &mut Session, fx: &Fx, comp: ItemId, lid: LayerId, spec: &Value, idx: usize, b: &mut Built) -> Result<()> {
    if let Some(Value::Object(m)) = spec.get("set") {
        let vals: Vec<(&str, Value)> = m.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        set_props(s, comp, lid, &vals)?;
    }
    let l = s.project.comp(comp).and_then(|c| c.layer(lid)).ok_or_else(|| bad(C, "no layer"))?.clone();
    let mut plans: Vec<(String, Value, Vec<i64>, Vec<Tick>)> = vec![];
    if let Some(Value::Object(keys)) = spec.get("keys") {
        for (p, tr) in keys {
            let frames = fx.cx.frames(tr, "fx")?;
            let times = layer_times(fx.cx.fr, &l, &frames);
            plans.push((p.clone(), tr.clone(), frames, times));
        }
    }
    let mut exprs: Vec<(String, String)> = vec![];
    if let Some(Value::Object(m)) = spec.get("expr") {
        for (p, t) in m {
            exprs.push((p.clone(), expr_text(fx, t, idx)?));
        }
    }
    if plans.is_empty() && exprs.is_empty() {
        return Ok(());
    }
    let m = Vals::Plain { base: [fx.cw() / 2.0, fx.ch() / 2.0], k: [fx.sx, fx.sy] };
    let eased = s.edit("Add Keyframes", None, |proj, _| {
        let l = layer_mut(proj, comp, lid)?;
        let lname = l.name.clone();
        let mut eased = false;
        for (p, tr, frames, times) in &plans {
            let pr = l.props.prop_mut(p).ok_or_else(|| bad(C, format!("“{lname}” has no {p}")))?;
            eased |= key_track(&fx.cx, pr, p, tr, frames, times, m)?;
        }
        for (p, t) in &exprs {
            let pr = l.props.prop_mut(p).ok_or_else(|| bad(C, format!("“{lname}” has no {p}")))?;
            pr.expr = Some(Expression { text: t.clone(), enabled: true });
        }
        Ok(eased)
    })?;
    if eased && !b.eased.contains(&lid) {
        b.eased.push(lid);
    }
    Ok(())
}

/// A spec's mask: `rect` [x, y, w, h] (px of 1920×1080, moved by `repeat.maskStep` × the copy's
/// index), `circle` [x, y] (on the padded canvas) with a keyed `radius`, or a polygon (`keys.path`:
/// a track of vertex lists, px of 1920×1080, straight edges); any of them with a `feather`.
#[allow(clippy::too_many_arguments)]
fn mask(s: &mut Session, fx: &Fx, comp: ItemId, lid: LayerId, spec: &Value, idx: usize, pad: f64, b: &mut Built) -> Result<()> {
    let kind = fx.cx.kind;
    let Some(m) = spec.get("mask").filter(|m| !m.is_null()) else { return Ok(()) };
    let feather = |s: &mut Session, mp: &str| -> Result<()> {
        if let Some(f) = m.get("feather").filter(|f| !f.is_null()) {
            let fp = format!("{mp}/feather");
            set_props(s, comp, lid, &[(fp.as_str(), f.clone())])?;
        }
        Ok(())
    };
    if let Some(r) = m.get("rect").and_then(Value::as_array) {
        let n: Vec<f64> = r.iter().filter_map(Value::as_f64).filter(|x| x.is_finite()).collect();
        let [x, y, w, h] = n[..] else { return Err(bad(C, format!("{kind}: a rect mask is [x, y, w, h]"))) };
        let step = pt(spec.get("repeat").and_then(|r| r.get("maskStep"))).unwrap_or([0.0, 0.0]);
        let i0 = idx.saturating_sub(1) as f64;
        let rect = [(x + step[0] * i0) * fx.sx, (y + step[1] * i0) * fx.sy, w * fx.sx, h * fx.sy];
        let r = run(s, comp, "layer.addMask", json!({"layer": lid.0, "shape": "rect", "rect": rect}))?;
        if m.get("feather").is_some_and(|f| !f.is_null()) {
            let mp = r.get("path").and_then(Value::as_str).ok_or_else(|| bad(C, "layer.addMask returned no path"))?.to_string();
            feather(s, &mp)?;
        }
        return Ok(());
    }
    let (mp, mm, tracks): (String, Vals, Vec<&Value>) = if let Some(c) = pt(m.get("circle")) {
        let p0 = spec.get("pad").and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(0.0);
        let cc = [(c[0] - p0) * fx.sx + pad, (c[1] - p0) * fx.sy + pad];
        let r = run(s, comp, "layer.addMask", json!({"layer": lid.0, "shape": "ellipse", "rect": [cc[0] - 1.0, cc[1] - 1.0, 2.0, 2.0]}))?;
        let mp = r.get("path").and_then(Value::as_str).ok_or_else(|| bad(C, "layer.addMask returned no path"))?.to_string();
        let tracks = m.get("keys").and_then(Value::as_object).map(|k| k.values().collect()).unwrap_or_default();
        (mp, Vals::Circle { c: cc, u: fx.u }, tracks)
    } else if let Some(tr) = m.get("keys").and_then(|k| k.get("path")).filter(|t| t.is_object()) {
        // A polygon: a mask over the whole layer, its path keyed.
        let (w, h) = (fx.cw() + 2.0 * pad, fx.ch() + 2.0 * pad);
        let r = run(s, comp, "layer.addMask", json!({"layer": lid.0, "shape": "rect", "rect": [0.0, 0.0, w, h]}))?;
        let mp = r.get("path").and_then(Value::as_str).ok_or_else(|| bad(C, "layer.addMask returned no path"))?.to_string();
        (mp, Vals::Poly { k: [fx.sx, fx.sy] }, vec![tr])
    } else {
        return Err(bad(C, format!("{kind}: a mask is {{rect}}, {{circle}} or a keyed polygon {{keys: {{path}}}}")));
    };
    let l = s.project.comp(comp).and_then(|c| c.layer(lid)).ok_or_else(|| bad(C, "no layer"))?.clone();
    let mut plans: Vec<(Value, Vec<i64>, Vec<Tick>)> = vec![];
    for tr in tracks {
        let frames = fx.cx.frames(tr, "fx")?;
        let times = layer_times(fx.cx.fr, &l, &frames);
        plans.push((tr.clone(), frames, times));
    }
    let path = format!("{mp}/path");
    let eased = s.edit("Add Keyframes", None, |proj, _| {
        let l = layer_mut(proj, comp, lid)?;
        let pr = l.props.prop_mut(&path).ok_or_else(|| bad(C, format!("{kind}: the mask has no path")))?;
        let mut eased = false;
        for (tr, frames, times) in &plans {
            eased |= key_track(&fx.cx, pr, &path, tr, frames, times, mm)?;
        }
        Ok(eased)
    })?;
    if eased && !b.eased.contains(&lid) {
        b.eased.push(lid);
    }
    feather(s, &mp)
}

fn rename(s: &mut Session, comp: ItemId, lid: LayerId, name: &str) -> Result<()> {
    s.edit("Rename Layer", None, |proj, _| {
        layer_mut(proj, comp, lid)?.name = name.to_string();
        Ok(())
    })
}

/// The colour of a spec: its own, or the `colour` option where it is `tint` (a palette by its
/// `palette` index).
fn tint<'a>(fx: &'a Fx, spec: &'a Value) -> Option<&'a str> {
    let own = spec.get("colour").and_then(Value::as_str);
    if !spec.get("tint").and_then(Value::as_bool).unwrap_or(false) {
        return own;
    }
    match &fx.colour {
        None => own,
        Some(Colour::One(c)) => Some(c),
        Some(Colour::Palette(p)) => {
            let i = spec.get("palette").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).unwrap_or(0);
            i.checked_rem(p.len()).and_then(|i| p.get(i)).map(String::as_str).or(own)
        }
    }
}

/// A copy's own trims (`trim`: {old: {out}, new: {in}}, s after the cut; default 0 = a plain cut
/// at `at`).
fn copy_trim(spec: &Value, role: &str, end: &str) -> f64 {
    spec.get("trim").and_then(|t| t.get(role)).and_then(|r| r.get(end)).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Copies of the cut: duplicates of the shots (`shots`, default both) with a plain cut at `at` (or
/// the spec's `trim`), in a precomp `FxCut <kind> Scenes` (tagged, its layer removed: overlays add
/// it), oversize by `pad` px of 1920×1080 on every side (the copies centred), with `inner` effects
/// on the copies. One per (shots, pad, inner, trim).
fn scenes(s: &mut Session, fx: &Fx, spec: &Value, b: &mut Built) -> Result<ItemId> {
    let kind = fx.cx.kind;
    let shots: Vec<&str> = match spec.get("shots").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).collect(),
        None => vec!["old", "new"],
    };
    if shots.is_empty() || shots.len() > 2 {
        return Err(bad(C, format!("{kind}: a copy's `shots` is [old], [new] or [old, new]")));
    }
    let pad = spec.get("pad").and_then(Value::as_f64).filter(|x| x.is_finite() && (0.0..=10_000.0).contains(x)).unwrap_or(0.0);
    let inner = arr(spec, "inner");
    let trim = spec.get("trim").cloned().unwrap_or(Value::Null);
    let key = format!("{}|{pad}|{}|{trim}", shots.join("+"), Value::Array(inner.to_vec()));
    if let Some((_, c)) = b.scenes.iter().find(|(k, _)| *k == key) {
        return Ok(*c);
    }
    let src: Vec<LayerId> = shots
        .iter()
        .map(|r| match *r {
            "old" => Ok(fx.old),
            "new" => Ok(fx.new),
            x => Err(bad(C, format!("{kind}: a copy's shot is old or new, not `{x}`"))),
        })
        .collect::<Result<_>>()?;
    // The duplicates, right above their shots, cut at `at` (or trimmed as the spec says).
    let (cid, prefix) = (fx.cid, format!("{}{kind} copy", fx.fam.prefix()));
    let fr = fx.cx.fr;
    let (old_out, new_in) = (fr.tick_of(fx.cx.frame(copy_trim(spec, "old", "out"))?), fr.tick_of(fx.cx.frame(copy_trim(spec, "new", "in"))?));
    if shots.contains(&"new") && new_in < fx.cut {
        // A copy of the new shot that starts before the cut needs that much footage.
        let nl = s.project.comp(cid).and_then(|c| c.layer(fx.new)).ok_or_else(|| bad(C, "no new layer"))?;
        if new_in < nl.in_point
            && let Some(src) = source_start(s, nl)
            && new_in < src
        {
            return Err(bad(
                C,
                format!(
                    "{kind}'s copy of “{}” starts at {} s, {:.3} s before the cut, but its footage starts at {} s: it needs that much footage before its in point",
                    nl.name,
                    fmt_s(new_in),
                    fx.cut.seconds() - new_in.seconds(),
                    fmt_s(src)
                ),
            ));
        }
    }
    let dups = s.edit("Duplicate", None, |proj, _| {
        let mut next = proj.next_id;
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let fd = comp.frame_duration();
        let mut out = vec![];
        for (id, role) in src.iter().zip(&shots) {
            let i = comp.layers.iter().position(|l| l.id == *id).ok_or_else(|| bad(C, "a shot is gone"))?;
            let mut l = comp.layer(*id).ok_or_else(|| bad(C, "a shot is gone"))?.clone();
            super::edit::reid(&mut l, &mut next);
            l.name = format!("{prefix} {role}");
            if *role == "old" {
                l.out_point = old_out.max(l.in_point + fd);
            } else {
                l.in_point = new_in.min(l.out_point - fd);
            }
            out.push(l.id);
            comp.layers.insert(i, l);
        }
        proj.next_id = next;
        Ok(out)
    })?;
    let name = if b.scenes.is_empty() { fx.name("Scenes") } else { fx.name(&format!("Scenes {}", b.scenes.len() + 1)) };
    let pc = run(s, cid, "layer.precompose", json!({"layers": dups.iter().map(|l| l.0).collect::<Vec<_>>(), "name": name, "mode": "move"}))?;
    let pl = layer_of(&pc)?;
    let pcomp = pc.get("comp").and_then(Value::as_u64).map(ItemId).ok_or_else(|| bad(C, "layer.precompose returned no comp"))?;
    let tag = fx.tag.clone();
    s.edit("Pre-compose", None, |proj, _| {
        if let Some(it) = proj.item_mut(pcomp) {
            it.comment = tag;
        }
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        c.layers.retain(|l| l.id != pl);
        Ok(())
    })?;
    b.items.push(pcomp);
    let padpx = (pad * fx.u).round();
    if padpx > 0.0 {
        let (w, h) = ((fx.cw() + 2.0 * padpx) as u64, (fx.ch() + 2.0 * padpx) as u64);
        run(s, pcomp, "comp.settings", json!({"width": w, "height": h, "anchor": 4}))?;
    }
    if !inner.is_empty() {
        let fxs = json!({"effects": inner});
        for d in &dups {
            let [w, h] = layer_size(s, pcomp, *d)?;
            layer_effects(s, fx, pcomp, *d, &fxs, [w / 2.0, h / 2.0], [fx.sx, fx.sy], 0, b)?;
        }
    }
    b.scenes.push((key, pcomp));
    Ok(pcomp)
}

/// Every copy's precomp, made before the shots are trimmed or get effects.
fn prepare_scenes(s: &mut Session, fx: &Fx, specs: &[Value], b: &mut Built, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(bad(C, format!("{}: overlays nest too deep", fx.cx.kind)));
    }
    for sp in specs.iter().take(MAX_ITEMS) {
        if sp.get("kind").and_then(Value::as_str) == Some("copy") && sp.get("of").is_none() {
            scenes(s, fx, sp, b)?;
        }
        let ch = arr(sp, "layers");
        if !ch.is_empty() {
            prepare_scenes(s, fx, ch, b, depth.saturating_add(1))?;
        }
    }
    Ok(())
}

/// One overlay layer in `comp` (the transition's comp, or a comp overlay's own): a `solid` (the
/// comp's size + `pad`, or `size` px of 1920×1080: a strip), a `glow` (a radial-gradient ellipse:
/// the colour at the centre, opacity 1 / 0.6 at 45 % / 0 at the rim), a `rect` / `ellipse` shape, a
/// `copy` of the cut or a `comp` built from its own layers; `threeD` makes it a 3-D layer (turned
/// through the comp's camera); then its blend mode, mask, effects, values, keys, expressions and in
/// / out.
fn overlay(s: &mut Session, fx: &Fx, comp: ItemId, spec: &Value, idx: usize, depth: usize, b: &mut Built) -> Result<LayerId> {
    let kind = fx.cx.kind;
    if depth > MAX_DEPTH {
        return Err(bad(C, format!("{kind}: overlays nest too deep")));
    }
    let sname = spec.get("name").and_then(Value::as_str).unwrap_or("Overlay");
    let name = if idx > 0 { fx.name(&format!("{sname} {idx}")) } else { fx.name(sname) };
    let col = tint(fx, spec);
    let pad_t = spec.get("pad").and_then(Value::as_f64).filter(|x| x.is_finite() && (0.0..=10_000.0).contains(x)).unwrap_or(0.0);
    let pad = (pad_t * fx.u).round();
    let (w, h) = (fx.cw() + 2.0 * pad, fx.ch() + 2.0 * pad);
    let okind = spec.get("kind").and_then(Value::as_str).unwrap_or("solid");
    let missing = |what: &str| bad(C, format!("{kind}: overlay “{sname}” has no `{what}`"));
    let l = match okind {
        "solid" => {
            let mut q = json!({"color": col.unwrap_or("#000000"), "name": name});
            if pad > 0.0 {
                q["width"] = json!(w as u64);
                q["height"] = json!(h as u64);
            }
            if let Some(Value::Array(_)) = spec.get("size") {
                let sz = pt(spec.get("size")).filter(|p| p[0] > 0.0 && p[1] > 0.0 && p[0] <= 100_000.0 && p[1] <= 100_000.0).ok_or_else(|| missing("size"))?;
                q["width"] = json!((sz[0] * fx.sx).round().max(1.0) as u64);
                q["height"] = json!((sz[1] * fx.sy).round().max(1.0) as u64);
            }
            layer_of(&run(s, comp, "layer.newSolid", q)?)?
        }
        "glow" | "ellipse" | "rect" => {
            let pos = pt(spec.get("position")).ok_or_else(|| missing("position"))?;
            let size = match okind {
                "rect" => pt(spec.get("size")).map(|p| [p[0] * fx.sx, p[1] * fx.sy]),
                "glow" => spec.get("size").and_then(Value::as_f64).filter(|d| d.is_finite()).map(|d| [d * fx.u, d * fx.u]),
                _ => pt(spec.get("size")).map(|p| [p[0] * fx.u, p[1] * fx.u]),
            }
            .ok_or_else(|| missing("size"))?;
            let fill = col.unwrap_or("#ffffff");
            let shape = if okind == "rect" { "rect" } else { "ellipse" };
            let mut q = json!({
                "kind": shape,
                "size": size,
                "position": [pos[0] * fx.sx, pos[1] * fx.sy],
                "name": name,
                "fill": fill,
                "strokeWidth": 0,
            });
            if okind == "glow" {
                q["fillType"] = json!("radial");
            }
            let l = layer_of(&run(s, comp, "shape.newShape", q)?)?;
            if okind == "glow" {
                let stops = json!({"colors": [[0, fill], [1, fill]], "opacities": [[0, 1], [0.45, 0.6], [1, 0]]});
                run(s, comp, "prop.set", json!({"layer": l.0, "path": "contents/group/contents/gfill/colors", "value": stops}))?;
                run(s, comp, "prop.set", json!({"layer": l.0, "path": "contents/group/contents/gfill/end", "value": [size[0] / 2.0, 0.0]}))?;
            }
            l
        }
        "copy" => {
            let item = match spec.get("of").and_then(Value::as_str) {
                Some(of) => b
                    .comps
                    .iter()
                    .find(|(n, _)| n == of)
                    .map(|(_, c)| *c)
                    .ok_or_else(|| bad(C, format!("{kind}: “{sname}” copies `{of}`, which isn't made yet")))?,
                None => scenes(s, fx, spec, b)?,
            };
            let l = layer_of(&run(s, comp, "layer.addItem", json!({"item": item.0, "time": 0.0}))?)?;
            rename(s, comp, l, &name)?;
            l
        }
        "comp" => {
            let (main, tag) = (fx.cid, fx.tag.clone());
            let dur = s.project.comp(comp).ok_or(EngineError::NoComp)?.duration;
            let cname = name.clone();
            let c = s.edit("New Composition", None, |proj, _| {
                let m = proj.comp(main).ok_or(EngineError::NoComp)?;
                let inner = m.nested_like(m.width, m.height, dur);
                let id = proj.add_item(&cname, Label::Sandstone, None, ItemKind::Comp(inner.into()));
                if let Some(it) = proj.item_mut(id) {
                    it.comment = tag;
                }
                Ok(id)
            })?;
            b.items.push(c);
            for ch in arr(spec, "layers").iter().take(MAX_ITEMS) {
                let rep = ch.get("repeat").filter(|r| !r.is_null());
                let n = rep.and_then(|r| r.get("count")).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(1).clamp(1, MAX_REPEAT);
                for r in 0..n {
                    let cl = overlay(s, fx, c, ch, if rep.is_some() { r.saturating_add(1) } else { 0 }, depth.saturating_add(1), b)?;
                    run(s, c, "layer.arrange", json!({"layers": [cl.0], "to": "front"}))?;
                }
            }
            b.comps.push((sname.to_string(), c));
            let l = layer_of(&run(s, comp, "layer.addItem", json!({"item": c.0, "time": 0.0}))?)?;
            rename(s, comp, l, &name)?;
            l
        }
        x => return Err(bad(C, format!("{kind}: overlay kind `{x}`? (solid, glow, rect, ellipse, copy or comp)"))),
    };
    if spec.get("threeD").and_then(Value::as_bool).unwrap_or(false) {
        run(s, comp, "layer.setSwitch", json!({"layers": [l.0], "switch": "threeD", "value": true}))?;
    }
    if let Some(mode) = spec.get("blend").and_then(Value::as_str) {
        run(s, comp, "layer.setBlendMode", json!({"layers": [l.0], "mode": mode}))?;
    }
    mask(s, fx, comp, l, spec, idx, pad, b)?;
    layer_effects(s, fx, comp, l, spec, [w / 2.0, h / 2.0], [fx.sx, fx.sy], idx, b)?;
    layer_props(s, fx, comp, l, spec, idx, b)?;
    let at = |k: &str| spec.get(k).and_then(Value::as_f64).map(|t| fx.cx.frame(t)).transpose();
    let (fin, fout) = (at("in")?, at("out")?);
    if fin.is_some() || fout.is_some() {
        let fr = fx.cx.fr;
        s.edit("Layer Timing", None, |proj, _| {
            let l = layer_mut(proj, comp, l)?;
            if let Some(f) = fin {
                l.in_point = fr.tick_of(f);
            }
            if let Some(f) = fout {
                let first = fr.frame_at(fr.snap_nearest(l.in_point));
                l.out_point = fr.tick_of(f.max(first.saturating_add(1)));
            }
            Ok(())
        })?;
    }
    if comp == fx.cid {
        b.made.push(l);
    }
    Ok(l)
}

/// `layer` where the spec says (`where`: top, below, above-new).
fn place(s: &mut Session, fx: &Fx, l: LayerId, where_: &str, at: &mut Place) -> Result<()> {
    match where_ {
        "below" => put_below(s, fx.cid, l, at.lower)?,
        "above-new" => {
            run(s, fx.cid, "layer.arrange", json!({"layers": [l.0], "to": "front", "above": at.above_new.0}))?;
            if at.top == at.above_new {
                at.top = l;
            }
            at.above_new = l;
        }
        "top" => {
            run(s, fx.cid, "layer.arrange", json!({"layers": [l.0], "to": "front", "above": at.top.0}))?;
            at.top = l;
        }
        x => return Err(bad(C, format!("{}: `where` is top, below or above-new, not `{x}`", fx.cx.kind))),
    }
    Ok(())
}

/// A top-level overlay: made, placed, and its `matte` (a solid spec, same in / out) made right
/// above it and set as its track matte.
fn top_overlay(s: &mut Session, fx: &Fx, spec: &Value, at: &mut Place, b: &mut Built) -> Result<LayerId> {
    let l = overlay(s, fx, fx.cid, spec, 0, 0, b)?;
    place(s, fx, l, spec.get("where").and_then(Value::as_str).unwrap_or("top"), at)?;
    if let Some(m) = spec.get("matte").filter(|m| m.is_object()) {
        let mut ms = Map::new();
        for key in ["name", "kind", "colour", "pad", "mask", "effects"] {
            if let Some(v) = m.get(key) {
                ms.insert(key.into(), v.clone());
            }
        }
        for key in ["in", "out"] {
            if let Some(v) = spec.get(key) {
                ms.insert(key.into(), v.clone());
            }
        }
        let ml = overlay(s, fx, fx.cid, &Value::Object(ms), 0, 0, b)?;
        run(s, fx.cid, "layer.arrange", json!({"layers": [ml.0], "to": "front", "above": l.0}))?;
        if at.top == l {
            at.top = ml;
        }
        let mode = m.get("mode").and_then(Value::as_str).unwrap_or("alpha");
        run(s, fx.cid, "layer.setTrackMatte", json!({"layer": l.0, "matte": ml.0, "kind": mode}))?;
    }
    Ok(l)
}

/// Every copy's `trim` time in a list of overlay specs (nested comps' layers too), for the
/// comp-length check.
fn trim_times(specs: &[Value], depth: usize, out: &mut Vec<f64>) {
    if depth > MAX_DEPTH {
        return;
    }
    for sp in specs.iter().take(MAX_ITEMS) {
        if sp.get("kind").and_then(Value::as_str) == Some("copy") {
            for (role, end) in [("old", "out"), ("new", "in")] {
                if let Some(t) = sp.get("trim").and_then(|t| t.get(role)).and_then(|r| r.get(end)).and_then(Value::as_f64) {
                    out.push(t);
                }
            }
        }
        trim_times(arr(sp, "layers"), depth.saturating_add(1), out);
    }
}

/// An FX cut on the pair (the previous transition is gone already).
pub(super) fn apply(s: &mut Session, p: &Value, kind: &str, cid: ItemId, old: LayerId, new: LayerId, removed: Removed) -> Result<Applied> {
    let k = kind_spec(kind)?;
    apply_family(s, Family::Fx, k, p, kind, cid, old, new, removed)
}

/// An FX or scene cut on the pair (the previous transition is gone already): `k` is the kind's
/// table entry (a scene cut's already mapped to its `dir`).
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_family(
    s: &mut Session,
    fam: Family,
    k: &Value,
    p: &Value,
    kind: &str,
    cid: ItemId,
    old: LayerId,
    new: LayerId,
    removed: Removed,
) -> Result<Applied> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    if !(fps.is_finite() && fps > 0.0) {
        return Err(bad(C, "the comp has no frame rate"));
    }
    let (ol, nl) = (comp.layer(old).ok_or_else(|| bad(C, "no old layer"))?, comp.layer(new).ok_or_else(|| bad(C, "no new layer"))?);
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let cut_f = cut_frame(p, fr, nl)?;
    let cut = fr.tick_of(cut_f);
    check_meets(fr, cut_f, ol, nl)?;
    let win = arr(k, "window").iter().filter_map(Value::as_f64).collect::<Vec<_>>();
    let [w0, w1] = win[..] else { return Err(bad(C, format!("{kind}: no window in the table"))) };
    if !(w0.is_finite() && w1.is_finite() && w1 > w0) {
        return Err(bad(C, format!("{kind}: a bad window in the table")));
    }
    let mul = match p.get("dur").filter(|v| !v.is_null()) {
        None => 1.0,
        Some(v) => v
            .as_f64()
            .filter(|d| d.is_finite() && *d > 0.0 && *d <= MAX_DUR)
            .map(|d| d / (w1 - w0))
            .ok_or_else(|| bad(C, format!("`dur` is the transition's length in seconds (up to {MAX_DUR})")))?,
    };
    let colour = colour_p(p)?;
    let ref_fps = k.get("fps").and_then(Value::as_str).and_then(fps_of).unwrap_or(30000.0 / 1001.0);
    let (profile, _) = Profile::load(s.config.as_deref());
    let cx = Cx { kind, fr, fps, ref_fps, cut_f, profile: &profile, scale: mul };
    // The shots, trimmed to the kind; the window, every overlay and copy inside the comp.
    let sh = k.get("shots");
    let end = |role: &str, e: &str| sh.and_then(|s| s.get(role)).and_then(|r| r.get(e)).and_then(Value::as_f64).unwrap_or(0.0);
    let (old_out_f, new_in_f) = (cx.frame(end("old", "out"))?, cx.frame(end("new", "in"))?);
    let (start_f, land_f) = (cx.frame(w0)?, cx.frame(k.get("land").and_then(Value::as_f64).unwrap_or(w1))?);
    let mut frames = vec![start_f, cx.frame(w1)?, land_f, old_out_f, new_in_f, cut_f];
    for o in arr(k, "overlays") {
        for e in ["in", "out"] {
            if let Some(t) = o.get(e).and_then(Value::as_f64) {
                frames.push(cx.frame(t)?);
            }
        }
    }
    let mut trims = vec![];
    trim_times(arr(k, "overlays"), 0, &mut trims);
    for t in trims {
        frames.push(cx.frame(t)?);
    }
    let (lo, hi) = (frames.iter().copied().min().unwrap_or(cut_f), frames.iter().copied().max().unwrap_or(cut_f));
    if fr.tick_of(lo) < Tick::ZERO {
        return Err(bad(
            C,
            format!("the cut at {} s is too close to the comp's start: {kind} starts {} frame(s) before it", fmt_s(cut), cut_f.saturating_sub(lo)),
        ));
    }
    if fr.tick_of(hi) > comp.duration {
        return Err(bad(
            C,
            format!(
                "comp too short: {kind} runs to {} s ({} frame(s) after the cut), the comp ends at {} s",
                fmt_s(fr.tick_of(hi)),
                hi.saturating_sub(cut_f),
                fmt_s(comp.duration)
            ),
        ));
    }
    if frame(ol.in_point) >= old_out_f {
        return Err(bad(
            C,
            format!("“{}” starts at {} s, but {kind} ends it at {} s: it needs to start earlier", ol.name, fmt_s(ol.in_point), fmt_s(fr.tick_of(old_out_f))),
        ));
    }
    if frame(nl.out_point) <= new_in_f {
        return Err(bad(
            C,
            format!("{kind} starts “{}” at {} s, but it ends at {} s: it needs to play longer", nl.name, fmt_s(fr.tick_of(new_in_f)), fmt_s(nl.out_point)),
        ));
    }
    if new_in_f < frame(nl.in_point)
        && let Some(src) = source_start(s, nl)
        && fr.tick_of(new_in_f) < src
    {
        return Err(bad(
            C,
            format!(
                "{kind} starts “{}” at {} s, {:.3} s before the cut, but its footage starts at {} s: it needs that much footage before its in point",
                nl.name,
                fmt_s(fr.tick_of(new_in_f)),
                cut.seconds() - fr.tick_of(new_in_f).seconds(),
                fmt_s(src)
            ),
        ));
    }
    // FX cuts: while both shots show, the old one is on top (the new one starts under it). Scene
    // cuts that slide or fade the new shot in over the old one (`stack: new-over-old`): the new one
    // goes right above the old one.
    let new_below = comp.index_of(new) > comp.index_of(old);
    let restack = fam == Family::Fx && new_in_f < old_out_f && !new_below;
    let stack = fam == Family::Scene && k.get("stack").and_then(Value::as_str) == Some("new-over-old") && new_below;
    let (w, h) = (comp.width, comp.height);
    let title = k.get("title").and_then(Value::as_str).unwrap_or(kind).to_string();
    let reference = {
        let t = k.get("reference").and_then(|r| r.get("t")).and_then(Value::as_str).unwrap_or("");
        let from = k.get("from").and_then(Value::as_str).unwrap_or("");
        format!("{} {t} {title} (HyperFrames {})", fam.batch(), from.rsplit('/').next().unwrap_or(from))
    };
    let (cw, ch) = (f64::from(w), f64::from(h));
    let fx = Fx {
        fam,
        cx,
        k,
        cid,
        old,
        new,
        cut,
        w,
        h,
        sx: cw / 1920.0,
        sy: ch / 1080.0,
        u: ((cw * cw + ch * ch) / (1920.0 * 1920.0 + 1080.0 * 1080.0)).sqrt(),
        tag: fam.tag(old, new),
        colour,
        expr_check: s.expr_check,
    };
    let mut b = Built::default();
    let mut arranged = vec![];
    if restack {
        run(s, cid, "layer.arrange", json!({"layers": [old.0], "to": "front", "above": new.0}))?;
        arranged.push(old);
    }
    // Copies of the cut first: the shots as they are, before trims and effects.
    prepare_scenes(s, &fx, arr(k, "overlays"), &mut b, 0)?;
    if stack {
        run(s, cid, "layer.arrange", json!({"layers": [new.0], "to": "front", "above": old.0}))?;
        arranged.push(new);
    }
    s.edit("Layer Timing", None, |proj, _| {
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        if let Some(l) = c.layer_mut(old) {
            l.out_point = fr.tick_of(old_out_f);
        }
        if let Some(l) = c.layer_mut(new) {
            l.in_point = fr.tick_of(new_in_f);
        }
        Ok(())
    })?;
    // Moves on the shots: effects about the layer's centre or the effect's pivot (its own px per
    // 1920×1080 px).
    if let Some(Value::Object(roles)) = k.get("layers") {
        for role in ["old", "new"] {
            let Some(spec) = roles.get(role) else { continue };
            let lid = if role == "old" { old } else { new };
            let [lw, lh] = layer_size(s, cid, lid)?;
            let kk = [lw / 1920.0, lh / 1080.0];
            layer_effects(s, &fx, cid, lid, spec, [960.0 * kk[0], 540.0 * kk[1]], kk, 0, &mut b)?;
        }
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let old_on_top = comp.index_of(old) < comp.index_of(new);
    let mut at = if old_on_top { Place { top: old, lower: new, above_new: new } } else { Place { top: new, lower: old, above_new: new } };
    let ovs = arr(k, "overlays");
    if ovs.len() > MAX_ITEMS {
        return Err(bad(C, format!("{kind}: too many overlays")));
    }
    for o in ovs {
        top_overlay(s, &fx, o, &mut at, &mut b)?;
    }
    // Tags: a re-apply finds and removes what was added here.
    let (made, eased, ftag) = (b.made.clone(), b.eased.clone(), fx.tag.clone());
    s.edit("Layer Comment", None, |proj, _| {
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        for l in c.layers.iter_mut_where(|l| made.contains(&l.id)) {
            l.comment = if eased.contains(&l.id) { format!("{ftag} · reference ease: {reference}") } else { ftag.clone() };
        }
        Ok(())
    })?;
    let t = |f: i64| fr.tick_of(f).seconds();
    let mut reply = json!({
        "kind": kind,
        "family": fam.id(),
        "title": title,
        "comp": cid.0,
        "old": old.0,
        "new": new.0,
        "shot": new.0,
        "at": cut.seconds(),
        "cutFrame": cut_f,
        "start": t(start_f),
        "startFrame": start_f,
        "land": t(land_f),
        "landFrame": land_f,
        "dur": (w1 - w0) * mul,
        "effects": b.effects,
        "layers": b.made.iter().map(|l| l.0).collect::<Vec<_>>(),
        "comps": b.items.iter().map(|i| i.0).collect::<Vec<_>>(),
        "removed": {"effects": removed.effects, "layers": removed.layers, "items": removed.items},
    });
    if let Some(o) = reply.as_object_mut() {
        match fam {
            Family::Fx => o.insert("restacked".into(), json!(restack)),
            Family::Scene => o.insert("stacked".into(), json!(stack)),
        };
    }
    Ok(Applied { reply, label: format!("Apply Transition {} {title}", fam.menu()), select: vec![old, new], arranged })
}
