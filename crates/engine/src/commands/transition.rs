//! `transition.apply` (Animation ▸ Transitions ▸ Zoom Cut / FX Cut / Scene Cut): one of Elias's 13
//! approved zoom cuts, one of his 15 approved FX cuts ([`super::transition_fx`]) or one of his 18
//! approved scene cuts ([`super::transition_scene`]), at the cut between two layers. The zoom cuts are a port of motion-kit's shelf part `zoom-cut`
//! (`shelf/transitions/zoom-cut.jsx`, verified frame-exact against the approved renders at
//! 23.976/24/25/29.97/59.94); every number comes from the same data table, shipped here as
//! `presets/zoom-cut.kinds.json` (re-copied when the shelf table changes, see its sidecar).
//!
//! A hard cut, never a crossfade: the shots' own transforms are never touched. Every move is an
//! effect on the shot (Transform, plus Gaussian / Radial / Directional Blur, Exposure, Glow and a
//! mirrored Motion Tile wherever a zoom goes below 100 %), all named `ZoomCut <kind> …`. Track
//! times are seconds after the cut, keyed on the comp's own frames, so any frame rate works; an
//! old-shot key in the last reference frame before the cut goes on the comp's last frame before
//! it. "Twos" tracks hold every 2 comp frames (an expression). `into-window` precomposes the new
//! shot into a window over a background; `white-flash` adds light overlays. Eases are Elias's
//! Graphs by name, resolved through his ease profile ([`crate::taste`]: the Graphs cards and his
//! saved Motion Library curves), custom `{out, in}` influences, or measured velocities.
//!
//! Re-applying on the same pair (any kind of any family) first removes the previous transition: its
//! `ZoomCut …` / `FxCut …` / `SceneCut …` effects, the overlays and precomps it tagged, the solids
//! only those overlays used, and an FX or scene cut's trims of the shots (no stacking). Everything is
//! one undo step; any error rolls everything back. Whatever the command changed on the user's own
//! layers beyond adding effects (in / out points, layer order, a shot moved into a precomp, the audio
//! keep copies) is listed in the reply's `moved`. Elias's safety rules (never cut the sound, never
//! touch a third layer) are in [`super::transition_safety`].

use std::sync::OnceLock;

use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe};
use effectcraft_project::build::Ids;
use effectcraft_project::{Expression, ItemId, Layer, LayerId, LayerSource, Node, PropGroup, Property, Value as KV};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Map, Value, json};

use super::peak::{Graph, apply_graph};
use super::text_anim::json_value;
use super::transition_fx::{self, Family};
use super::transition_safety::{self as safety, Side};
use super::transition_scene;
use super::{CommandSpec, bad, comp_id, f_p, has_comp, layer_mut, resolve_layer, str_p, unlocked};
use crate::taste::Profile;
use crate::{EngineError, Result, Session, cmd, query};

pub(super) const C: &str = "transition.apply";

/// The zoom-cut kinds: a copy of motion-kit's `shelf/transitions/zoom-cut.kinds.json`.
pub const ZOOM_CUT_JSON: &str = include_str!("../../presets/zoom-cut.kinds.json");

/// Everything the command adds is named with this prefix (effects and layers).
pub(super) const PREFIX: &str = "ZoomCut ";
/// The mirrored Motion Tile's name after `ZoomCut <kind> `: the table's own, and the one a shot
/// gets when its zoom goes below 100 % and its kind lists none.
pub(super) const MIRROR: &str = "Mirror edges";
/// A layer comment or effect name the FX cuts add starts with this.
const FX_PREFIX: &str = transition_fx::PREFIX;
/// A layer comment or effect name the scene cuts add starts with this.
const SCENE_PREFIX: &str = transition_scene::PREFIX;
/// Most keys one track may hold, so a bad table can't allocate without bound.
pub(super) const MAX_KEYS: usize = 64;
/// Farthest a track time may sit from the cut, in seconds.
const MAX_OFFSET: f64 = 600.0;
/// Overlays and effects per kind.
const MAX_ITEMS: usize = 32;

pub(super) fn table() -> Result<&'static Value> {
    static T: OnceLock<std::result::Result<Value, String>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(ZOOM_CUT_JSON).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| EngineError::Other(format!("{C}: the zoom-cut table doesn't parse: {e}")))
}

/// The kinds in menu order.
pub fn kinds() -> Vec<String> {
    table()
        .ok()
        .and_then(|t| t.get("order"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn kind_spec<'a>(t: &'a Value, kind: &str) -> Result<&'a Value> {
    t.get("kinds").and_then(|k| k.get(kind)).ok_or_else(|| bad(C, format!("unknown kind `{kind}`; kinds: {}", kinds().join(", "))))
}

/// "30", "24000/1001" → frames per second.
pub(super) fn fps_of(s: &str) -> Option<f64> {
    let f = match s.split_once('/') {
        Some((a, b)) => a.trim().parse::<f64>().ok()? / b.trim().parse::<f64>().ok()?,
        None => s.trim().parse::<f64>().ok()?,
    };
    (f.is_finite() && f > 0.0).then_some(f)
}

pub(super) fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub(super) fn pt(v: Option<&Value>) -> Option<[f64; 2]> {
    let a = v?.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

/// A `#rrggbb` string as a colour value (straight, alpha 1), else the JSON shaped like `like`.
pub(super) fn value_like(v: &Value, like: &KV) -> Option<KV> {
    if let Value::String(s) = v {
        let c = effectcraft_color::Rgba::from_hex(s)?;
        return Some(KV::Color([f64::from(c.r), f64::from(c.g), f64::from(c.b), 1.0]));
    }
    json_value(v, like)
}

/// The comp's frames around the cut.
pub(super) struct Cx<'a> {
    pub kind: &'a str,
    pub fr: FrameRate,
    pub fps: f64,
    /// The reference's frame rate (the kind's `fps`).
    pub ref_fps: f64,
    /// The cut's comp frame.
    pub cut_f: i64,
    pub profile: &'a Profile,
    /// Every table time is multiplied by this (an FX cut's `dur`; 1 for the zoom cuts).
    pub scale: f64,
}

impl Cx<'_> {
    /// Comp frame of `t` seconds after the cut.
    pub(super) fn frame(&self, t: f64) -> Result<i64> {
        if !t.is_finite() || t.abs() > MAX_OFFSET {
            return Err(bad(C, format!("{}: a track time {t} s is out of range", self.kind)));
        }
        let f = t * self.scale * self.fps;
        if !f.is_finite() || f.abs() > MAX_OFFSET * 1000.0 {
            return Err(bad(C, format!("{}: a track time {t} s is out of range", self.kind)));
        }
        Ok(self.cut_f.saturating_add(f.round() as i64))
    }

    /// A track's keys as comp frames: an old-shot key in the last reference frame before the cut
    /// goes on the last comp frame before it (a Launch out's peak; in seconds it would land two
    /// frames early at 60 fps and freeze that frame), and no two keys share a frame.
    pub(super) fn frames(&self, tr: &Value, role: &str) -> Result<Vec<i64>> {
        let ts = arr(tr, "t");
        if ts.len() < 2 || ts.len() > MAX_KEYS {
            return Err(bad(C, format!("{}: a track needs 2 to {MAX_KEYS} times, has {}", self.kind, ts.len())));
        }
        let mut out: Vec<i64> = Vec::with_capacity(ts.len());
        for t in ts {
            let t = t.as_f64().ok_or_else(|| bad(C, format!("{}: a track time isn't a number", self.kind)))?;
            let mut f = self.frame(t)?;
            if role == "old" && t < 0.0 && t >= -1.0 / self.ref_fps - 1e-6 {
                f = self.cut_f.saturating_sub(1);
            }
            if let Some(prev) = out.last()
                && f <= *prev
            {
                f = prev.saturating_add(1);
            }
            out.push(f);
        }
        Ok(out)
    }
}

/// How a track's values are shaped: `mul` the zoom factor (restScale / rest) on `zoom` tracks,
/// `base` the zoom centre (layer px) `offset` tracks add to, scaled by `sx`/`sy` (1920×1080 px →
/// the layer's), `scale3` a layer scale `[v, v, 100]`.
#[derive(Clone, Copy)]
struct Shape {
    mul: f64,
    base: [f64; 2],
    sx: f64,
    sy: f64,
    scale3: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Shape { mul: 1.0, base: [0.0, 0.0], sx: 1.0, sy: 1.0, scale3: false }
    }
}

enum SegEase {
    Linear,
    Graph(Graph),
    Velocity { out_speed: f64, out_inf: f64, in_speed: f64, in_inf: f64 },
}

fn influence(pct: f64) -> f64 {
    if pct.is_finite() { (pct / 100.0).clamp(0.001, 1.0) } else { 1.0 / 3.0 }
}

/// One segment's ease: a Graph name (Elias's profile; Expo In-Out follows the move's length),
/// `linear`, `{out, in}` influences (%) or `{outSpeed, outInfluence, inSpeed, inInfluence}`.
fn seg_ease(profile: &Profile, e: &Value, secs: f64, kind: &str) -> Result<SegEase> {
    let num = |k: &str| e.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
    match e {
        Value::String(s) if s.eq_ignore_ascii_case("linear") => Ok(SegEase::Linear),
        Value::String(name) => {
            let c = profile.find(name).ok_or_else(|| bad(C, format!("{kind}: no Graph named “{name}” in the ease profile (taste.profile)")))?;
            let (o, i) = c.at(secs);
            Ok(SegEase::Graph(Graph {
                id: "zoom-cut",
                name: "Zoom cut",
                out: Ease { speed: o.speed, influence: influence(o.influence) },
                inn: Ease { speed: i.speed, influence: influence(i.influence) },
            }))
        }
        Value::Object(_) if e.get("outSpeed").is_some() => Ok(SegEase::Velocity {
            out_speed: num("outSpeed").unwrap_or(0.0),
            out_inf: influence(num("outInfluence").unwrap_or(33.3)),
            in_speed: num("inSpeed").unwrap_or(0.0),
            in_inf: influence(num("inInfluence").unwrap_or(33.3)),
        }),
        Value::Object(_) => match (num("out"), num("in")) {
            (Some(o), Some(i)) => Ok(SegEase::Graph(Graph {
                id: "zoom-cut",
                name: "Zoom cut",
                out: Ease { speed: 0.0, influence: influence(o) },
                inn: Ease { speed: 0.0, influence: influence(i) },
            })),
            _ => Err(bad(C, format!("{kind}: a custom ease needs `out` and `in` (%)"))),
        },
        _ => Err(bad(C, format!("{kind}: an ease is a Graph name, linear or an object"))),
    }
}

fn ease_dims(v: &KV, spatial: bool) -> usize {
    if spatial && matches!(v, KV::Vec2(_) | KV::Vec3(_)) { 1 } else { v.components().len().max(1) }
}

/// Key one track on `pr`: `times` are the keys' layer times (one per comp frame of `frames`).
fn write_track(cx: &Cx, pr: &mut Property, tr: &Value, frames: &[i64], times: &[Tick], sh: Shape) -> Result<()> {
    let kind = cx.kind;
    let vals = arr(tr, "v");
    if vals.len() != frames.len() || times.len() != frames.len() {
        return Err(bad(C, format!("{kind}: {} has {} times but {} values", pr.name, frames.len(), vals.len())));
    }
    if pr.static_only {
        return Err(bad(C, format!("{kind}: {} can't be keyframed", pr.name)));
    }
    let zoom = tr.get("zoom").and_then(Value::as_bool).unwrap_or(false);
    let offset = tr.get("offset").and_then(Value::as_bool).unwrap_or(false);
    let mut keys: Vec<Keyframe> = Vec::with_capacity(vals.len());
    for (v, t) in vals.iter().zip(times) {
        let j = match v {
            Value::Number(_) => {
                let m = if zoom { sh.mul } else { 1.0 };
                let x = v.as_f64().unwrap_or(0.0) * m;
                if sh.scale3 { json!([x, x, 100.0]) } else { json!(x) }
            }
            Value::Array(_) => {
                let p = pt(Some(v)).ok_or_else(|| bad(C, format!("{kind}: a point value is [x, y]")))?;
                if offset { json!([sh.base[0] + p[0] * sh.sx, sh.base[1] + p[1] * sh.sy]) } else { json!(p) }
            }
            _ => return Err(bad(C, format!("{kind}: a track value is a number or [x, y]"))),
        };
        let kv = value_like(&j, &pr.value).ok_or_else(|| bad(C, format!("{kind}: {} doesn't take {j}", pr.name)))?;
        keys.push(Keyframe::new(*t, kv));
    }
    finish_track(cx, pr, keys, tr, if zoom { sh.mul } else { 1.0 })
}

/// A track's keys on `pr` with the table's eases per segment (`m` scales measured velocities:
/// the zoom factor on zoom tracks), and on twos when the track says so.
pub(super) fn finish_track(cx: &Cx, pr: &mut Property, mut keys: Vec<Keyframe>, tr: &Value, m: f64) -> Result<()> {
    let kind = cx.kind;
    let eases = arr(tr, "ease");
    for i in 0..keys.len().saturating_sub(1) {
        let Some(e) = eases.get(i) else { continue };
        let (Some(a), Some(b)) = (keys.get(i), keys.get(i + 1)) else { continue };
        let secs = (b.time.seconds() - a.time.seconds()).max(1e-9);
        match seg_ease(cx.profile, e, secs, kind)? {
            SegEase::Linear => {}
            SegEase::Graph(g) => apply_graph(&mut keys, i, pr.spatial, &g)?,
            SegEase::Velocity { out_speed, out_inf, in_speed, in_inf } => {
                // Measured speeds are units/s of the table's values: zoom tracks scale with them.
                let n = ease_dims(&a.value, pr.spatial);
                if let Some(a) = keys.get_mut(i) {
                    (a.out_interp, a.auto_bezier, a.continuous) = (Interp::Bezier, false, false);
                    a.out_ease = Eases::splat(Ease { speed: out_speed * m, influence: out_inf }, n);
                }
                if let Some(b) = keys.get_mut(i + 1) {
                    (b.in_interp, b.auto_bezier, b.continuous) = (Interp::Bezier, false, false);
                    b.in_ease = Eases::splat(Ease { speed: in_speed * m, influence: in_inf }, n);
                }
            }
        }
    }
    if let Some(first) = keys.first() {
        pr.value = first.value.clone();
    }
    pr.keys = keys;
    pr.expr = None;
    // On twos: a new picture every 2 comp frames; the cut shows the frame before it.
    if tr.get("steps").and_then(Value::as_str) == Some("twos") {
        let a = cx.cut_f.saturating_sub(1);
        pr.expr = Some(Expression {
            text: format!("var fd = thisComp.frameDuration, a = {a}, f = Math.round(time / fd); valueAtTime((a + 2 * Math.floor((f - a) / 2)) * fd)"),
            enabled: true,
        });
    }
    // `lead`: the track is read that many comp frames later. A Transform effect's own shutter is
    // centred on the frame, so lead 0.5 opens it on the frame (whip-pan: the new shot already moves
    // on its first frame, the old one's last frame ends on the cut).
    if let Some(lead) = tr.get("lead").filter(|v| !v.is_null()) {
        let lead =
            lead.as_f64().filter(|x| x.is_finite() && x.abs() <= 100.0).ok_or_else(|| bad(C, format!("{kind}: a track's `lead` is comp frames (±100)")))?;
        pr.expr = Some(Expression { text: format!("valueAtTime(time + {lead} * thisComp.frameDuration)"), enabled: true });
    }
    Ok(())
}

/// Set an effect's static values (`set`); a parameter the effect doesn't have is an error.
pub(super) fn set_values(g: &mut PropGroup, set: Option<&Value>, kind: &str) -> Result<()> {
    let Some(Value::Object(m)) = set else { return Ok(()) };
    let gname = g.name.clone();
    for (path, v) in m {
        let pr = g.prop_mut(path).ok_or_else(|| bad(C, format!("{kind}: {gname} has no parameter `{path}`")))?;
        pr.value = value_like(v, &pr.value).ok_or_else(|| bad(C, format!("{kind}: {gname}/{path} doesn't take {v}")))?;
    }
    Ok(())
}

pub(super) fn new_effect(next: &mut u64, effect: &str, name: &str, size: [f64; 2], kind: &str) -> Result<PropGroup> {
    let spec = effectcraft_effects::lookup(effect).ok_or_else(|| bad(C, format!("{kind}: unknown effect `{effect}`")))?;
    Ok(effectcraft_effects::instantiate(spec, &mut Ids(next), name, size))
}

/// The zoom track keys of a list of effects reach at least this scale % (100 when none goes lower).
fn min_zoom(effs: &[Value], mul: f64) -> f64 {
    let mut m: f64 = 100.0;
    for e in effs {
        let Some(Value::Object(keys)) = e.get("keys") else { continue };
        for (p, tr) in keys {
            if p == "position" || !tr.get("zoom").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            for v in arr(tr, "v").iter().filter_map(Value::as_f64) {
                m = m.min(v * mul);
            }
        }
    }
    m
}

/// Layer times of comp frames on `l`.
pub(super) fn layer_times(fr: FrameRate, l: &Layer, frames: &[i64]) -> Vec<Tick> {
    frames.iter().map(|f| l.layer_time(fr.tick_of(*f))).collect()
}

/// What was added, for the reply.
#[derive(Default)]
struct Added {
    effects: Vec<Value>,
    layers: Vec<u64>,
}

/// One shot's effects, in stack order after its own (a mirrored Motion Tile first when a zoom
/// goes below 100 % and the kind has none: no frame edge may ever show), each zoom about its
/// centre (1920×1080 px → the layer's own px).
#[allow(clippy::too_many_arguments)]
fn shot(
    s: &mut Session,
    cx: &Cx,
    cid: ItemId,
    lid: LayerId,
    role: &str,
    spec: &Value,
    mul: f64,
    centre: Option<[f64; 2]>,
    grow: bool,
    out: &mut Added,
) -> Result<()> {
    let kind = cx.kind;
    let effs = arr(spec, "effects");
    if effs.len() > MAX_ITEMS {
        return Err(bad(C, format!("{kind}: too many effects")));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?.clone();
    let (fx_size, _) = effectcraft_render::effect_bounds(&s.project, comp, &l);
    let (w, h) = effectcraft_render::source_size(&s.project, &l);
    let (w, h) = if w == 0 || h == 0 { (fx_size[0], fx_size[1]) } else { (f64::from(w), f64::from(h)) };
    let (w, h) = if w > 0.0 && h > 0.0 { (w, h) } else { (f64::from(comp.width), f64::from(comp.height)) };
    let (kx, ky) = (w / 1920.0, h / 1080.0);
    let min_z = min_zoom(effs, mul);
    let has_tile = effs.iter().any(|e| e.get("effect").and_then(Value::as_str) == Some("ec.stylize.motiontile"));
    let c0 = centre.or_else(|| effs.iter().find_map(|e| pt(e.get("centre")))).unwrap_or([960.0, 540.0]);
    // Mirrored area (%) the smallest zoom about centre `c` needs.
    let need = |c: [f64; 2]| -> f64 {
        let off = ((c[0] - 960.0).abs() / 1920.0).max((c[1] - 540.0).abs() / 1080.0);
        (100.0 * 100.0 / min_z.max(1.0) * (1.0 + 2.0 * off)).ceil() + 10.0
    };
    // Every track's keys, planned on the comp's frames before the edit.
    let mut plans: Vec<(usize, String, Value, Vec<i64>, Vec<Tick>)> = vec![];
    for (i, e) in effs.iter().enumerate() {
        let Some(Value::Object(keys)) = e.get("keys") else { continue };
        for (p, tr) in keys {
            let frames = cx.frames(tr, role)?;
            let times = layer_times(cx.fr, &l, &frames);
            plans.push((i, p.clone(), tr.clone(), frames, times));
        }
    }
    let label = format!("{PREFIX}{kind}");
    let added = s.edit(&label, None, |proj, _| {
        let mut next = proj.next_id;
        let layer = layer_mut(proj, cid, lid)?;
        let lname = layer.name.clone();
        let fx = layer.props.sub_mut("effects").ok_or_else(|| bad(C, format!("“{lname}” can't take effects")))?;
        let mut groups: Vec<PropGroup> = vec![];
        if min_z < 100.0 && !has_tile {
            let mut g = new_effect(&mut next, "ec.stylize.motiontile", &format!("{label} {MIRROR}"), fx_size, kind)?;
            let n = need(c0);
            set_values(&mut g, Some(&json!({"outputWidth": n, "outputHeight": n, "mirrorEdges": true})), kind)?;
            groups.push(g);
        }
        for (i, e) in effs.iter().enumerate() {
            let id = e.get("effect").and_then(Value::as_str).unwrap_or_default();
            let name = format!("{label} {}", e.get("name").and_then(Value::as_str).unwrap_or("Effect"));
            let mut g = new_effect(&mut next, id, &name, fx_size, kind)?;
            set_values(&mut g, e.get("set"), kind)?;
            if id == "ec.stylize.motiontile" && grow && min_z < 100.0 {
                // A restScale override: grow the mirrored area to what the lower zoom needs.
                let n = need(c0);
                for k in ["outputWidth", "outputHeight"] {
                    if let Some(pr) = g.prop_mut(k) {
                        let now = pr.value.components().first().copied().unwrap_or(0.0);
                        pr.value = KV::Scalar(now.max(n));
                    }
                }
            }
            let mut sh = Shape { mul, sx: kx, sy: ky, ..Shape::default() };
            if id == "ec.distort.transform" {
                let c = centre.or_else(|| pt(e.get("centre"))).unwrap_or([960.0, 540.0]);
                sh.base = [c[0] * kx, c[1] * ky];
                set_values(&mut g, Some(&json!({"anchor": sh.base, "position": sh.base})), kind)?;
            }
            for (_, p, tr, frames, times) in plans.iter().filter(|x| x.0 == i) {
                let pr = g.prop_mut(p).ok_or_else(|| bad(C, format!("{kind}: {name} has no parameter `{p}`")))?;
                write_track(cx, pr, tr, frames, times, sh)?;
            }
            groups.push(g);
        }
        let mut made = vec![];
        for g in groups {
            made.push((g.uid, g.name.clone()));
            fx.children.push(Node::Group(g));
        }
        let n0 = fx.children.len().saturating_sub(made.len());
        proj.next_id = next;
        Ok(made
            .into_iter()
            .enumerate()
            .map(|(k, (uid, name))| json!({"layer": lid.0, "effect": uid, "name": name, "path": format!("effects/#{}", n0 + k + 1)}))
            .collect::<Vec<_>>())
    })?;
    out.effects.extend(added);
    Ok(())
}

/// Run a sub-command in `cid`.
pub(super) fn run(s: &mut Session, cid: ItemId, id: &str, mut p: Value) -> Result<Value> {
    if let Some(o) = p.as_object_mut() {
        o.insert("comp".into(), json!(cid.0));
    }
    s.execute(id, p)
}

pub(super) fn layer_of(v: &Value) -> Result<LayerId> {
    v.get("layer").and_then(Value::as_u64).map(LayerId).ok_or_else(|| bad(C, "a sub-command returned no layer"))
}

/// Static values on a layer's properties (`path` → JSON).
pub(super) fn set_props(s: &mut Session, cid: ItemId, lid: LayerId, vals: &[(&str, Value)]) -> Result<()> {
    s.edit("Set Property", None, |proj, _| {
        let l = layer_mut(proj, cid, lid)?;
        for (path, v) in vals {
            let pr = l.props.prop_mut(path).ok_or_else(|| bad(C, format!("“{}” has no {path}", l.name)))?;
            pr.value = value_like(v, &pr.value).ok_or_else(|| bad(C, format!("{path} doesn't take {v}")))?;
        }
        Ok(())
    })
}

/// Add effect `effect` named `ZoomCut <kind> <name>` to a layer with `set` values (point values
/// in comp px already scaled by the caller).
#[allow(clippy::too_many_arguments)]
fn add_fx(s: &mut Session, cx: &Cx, cid: ItemId, lid: LayerId, effect: &str, name: &str, set: &Value, out: &mut Added) -> Result<()> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?;
    let (size, _) = effectcraft_render::effect_bounds(&s.project, comp, l);
    let full = format!("{PREFIX}{} {name}", cx.kind);
    let kind = cx.kind;
    let r = s.edit(&full, None, |proj, _| {
        let mut next = proj.next_id;
        let l = layer_mut(proj, cid, lid)?;
        let fx = l.props.sub_mut("effects").ok_or_else(|| bad(C, "this layer can't take effects"))?;
        let mut g = new_effect(&mut next, effect, &full, size, kind)?;
        set_values(&mut g, Some(set), kind)?;
        let uid = g.uid;
        fx.children.push(Node::Group(g));
        let n = fx.children.len();
        proj.next_id = next;
        Ok(json!({"layer": lid.0, "effect": uid, "name": full, "path": format!("effects/#{n}")}))
    })?;
    out.effects.push(r);
    Ok(())
}

/// Key a track on a layer property (`path`) in `cid`.
#[allow(clippy::too_many_arguments)]
fn key_layer(s: &mut Session, cx: &Cx, cid: ItemId, lid: LayerId, path: &str, tr: &Value, role: &str, sh: Shape) -> Result<()> {
    let frames = cx.frames(tr, role)?;
    let l = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or_else(|| bad(C, "no layer"))?;
    let times = layer_times(cx.fr, l, &frames);
    s.edit("Add Keyframes", None, |proj, _| {
        let l = layer_mut(proj, cid, lid)?;
        let pr = l.props.prop_mut(path).ok_or_else(|| bad(C, format!("“{}” has no {path}", l.name)))?;
        write_track(cx, pr, tr, &frames, &times, sh)
    })
}

fn tag_layer(s: &mut Session, cid: ItemId, lid: LayerId, tag: &str) -> Result<()> {
    s.edit("Layer Comment", None, |proj, _| {
        layer_mut(proj, cid, lid)?.comment = tag.to_string();
        Ok(())
    })
}

/// `layer` directly under `over`.
pub(super) fn put_below(s: &mut Session, cid: ItemId, layer: LayerId, over: LayerId) -> Result<()> {
    run(s, cid, "layer.arrange", json!({"layers": [layer.0], "to": "front", "above": over.0}))?;
    run(s, cid, "layer.arrange", json!({"layers": [over.0], "to": "front", "above": layer.0}))?;
    Ok(())
}

/// A property's value at a layer time (its keys when it has any).
fn value_at(l: &Layer, path: &str, t: Tick) -> Vec<f64> {
    let Some(pr) = l.props.prop(path) else { return vec![] };
    effectcraft_keyframe::evaluate(&pr.keys, t, pr.spatial).unwrap_or_else(|| pr.value.clone()).components()
}

/// into-window: the new shot moves into a window precomp (title bar, three dots) over the
/// kind's background, and the window settles. Returns the window layer.
#[allow(clippy::too_many_arguments)]
fn window(s: &mut Session, cx: &Cx, cid: ItemId, nl: LayerId, spec: &Value, w: &Value, mul: f64, out: &mut Added) -> Result<LayerId> {
    let kind = cx.kind;
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite()).ok_or_else(|| bad(C, format!("{kind}: window `{k}` missing")));
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (cw, ch) = (f64::from(comp.width), f64::from(comp.height));
    let (k, ky) = (cw / 1920.0, ch / 1080.0);
    let cut = cx.fr.tick_of(cx.cut_f);
    let src = comp.layer(nl).ok_or_else(|| bad(C, "no layer"))?;
    let lt = src.layer_time(cut);
    let get = |p: &str, d: [f64; 3]| -> [f64; 3] {
        let v = value_at(src, p, lt);
        [v.first().copied().unwrap_or(d[0]), v.get(1).copied().unwrap_or(d[1]), v.get(2).copied().unwrap_or(d[2])]
    };
    let (sc0, po0, an0) = (get("transform/scale", [100.0; 3]), get("transform/position", [cw / 2.0, ch / 2.0, 0.0]), get("transform/anchor", [0.0; 3]));
    let size = pt(w.get("size")).ok_or_else(|| bad(C, format!("{kind}: window `size` missing")))?;
    let top_left = pt(w.get("topLeft")).ok_or_else(|| bad(C, format!("{kind}: window `topLeft` missing")))?;
    let pivot = pt(w.get("pivot")).ok_or_else(|| bad(C, format!("{kind}: window `pivot` missing")))?;
    let bar_ref = num(w, "bar")?;
    let name = format!("{PREFIX}{kind}");
    let pc = run(s, cid, "layer.precompose", json!({"layers": [nl.0], "name": format!("{name} Window"), "mode": "move"}))?;
    let win = layer_of(&pc)?;
    let wcomp = pc.get("comp").and_then(Value::as_u64).map(ItemId).ok_or_else(|| bad(C, "layer.precompose returned no comp"))?;
    s.edit("Layer Timing", None, |proj, _| {
        let l = layer_mut(proj, cid, win)?;
        if cut < l.out_point {
            l.in_point = cut.max(l.in_point);
        }
        Ok(())
    })?;
    // Inside: the footage scaled into the window, clipped to the footage area under the bar.
    let foot = s.project.comp(wcomp).and_then(|c| c.layers.iter().next_back().map(|l| l.id)).ok_or_else(|| bad(C, "the window comp is empty"))?;
    let f = size[0] / 1920.0;
    let (x0, y0, ww, wh, bar) = (top_left[0] * k, top_left[1] * ky, size[0] * k, size[1] * ky, bar_ref * ky);
    let cxw = x0 + ww / 2.0;
    let cyf = y0 + (bar_ref + (size[1] - bar_ref) / 2.0) * ky;
    let sc = [sc0[0] * f, sc0[1] * f, sc0[2]];
    let po = [cxw + (po0[0] - cw / 2.0) * f, cyf + (po0[1] - ch / 2.0) * f];
    set_props(s, wcomp, foot, &[("transform/scale", json!(sc)), ("transform/position", json!([po[0], po[1], 0.0]))])?;
    let sx = if sc[0].abs() > 1e-9 { sc[0] / 100.0 } else { 1.0 };
    let sy = if sc[1].abs() > 1e-9 { sc[1] / 100.0 } else { 1.0 };
    let lx = |x: f64| (x - po[0]) / sx + an0[0];
    let ly = |y: f64| (y - po[1]) / sy + an0[1];
    let (m0, m1) = ([lx(x0), ly(y0 + bar)], [lx(x0 + ww), ly(y0 + wh)]);
    run(s, wcomp, "layer.addMask", json!({"layer": foot.0, "shape": "rect", "rect": [m0[0], m0[1], m1[0] - m0[0], m1[1] - m0[1]]}))?;
    let bar_colour = w.get("barColour").and_then(Value::as_str).unwrap_or("#111111");
    let b = layer_of(&run(
        s,
        wcomp,
        "layer.newSolid",
        json!({"color": bar_colour, "name": format!("{name} Title bar"), "width": ww.round().max(1.0) as u64, "height": bar.round().max(1.0) as u64}),
    )?)?;
    set_props(s, wcomp, b, &[("transform/position", json!([cxw, y0 + bar / 2.0, 0.0]))])?;
    let dot = num(w, "dotSize")?;
    let dot_y = num(w, "dotY")?;
    for d in arr(w, "dots").iter().take(MAX_ITEMS) {
        let dn = d.get("name").and_then(Value::as_str).unwrap_or("Dot");
        let colour = d.get("colour").and_then(Value::as_str).unwrap_or("#ffffff");
        let x = num(d, "x")?;
        run(
            s,
            wcomp,
            "layer.newShape",
            json!({"kind": "ellipse", "size": [dot * k, dot * k], "position": [x0 + x * k, y0 + dot_y * ky], "name": format!("{name} Dot {dn}"), "fill": colour, "strokeWidth": 0}),
        )?;
    }
    // Back in the comp: the background right under the window, from the cut.
    let bg_spec = w.get("bg").ok_or_else(|| bad(C, format!("{kind}: window `bg` missing")))?;
    let bg = layer_of(&run(
        s,
        cid,
        "layer.newSolid",
        json!({"color": bg_spec.get("colour").and_then(Value::as_str).unwrap_or("#000000"), "name": format!("{name} BG")}),
    )?)?;
    let corners = arr(bg_spec, "corners");
    let pts = [[0.0, 0.0], [cw, 0.0], [0.0, ch], [cw, ch]];
    let mut set = Map::new();
    for (i, p) in pts.iter().enumerate() {
        set.insert(format!("positionsColors/point{}", i + 1), json!(p));
        if let Some(c) = corners.get(i) {
            set.insert(format!("positionsColors/color{}", i + 1), c.clone());
        }
    }
    add_fx(s, cx, cid, bg, "ec.generate.fourcolor", "BG", &Value::Object(set), out)?;
    run(s, cid, "layer.timing", json!({"layers": [bg.0], "start": cut.seconds()}))?;
    put_below(s, cid, bg, win)?;
    out.layers.push(bg.0);
    let pv = [pivot[0] * k, pivot[1] * ky];
    set_props(s, cid, win, &[("transform/anchor", json!([pv[0], pv[1], 0.0])), ("transform/position", json!([pv[0], pv[1], 0.0]))])?;
    let tr = spec.get("keys").and_then(|k| k.get("transform/scale")).ok_or_else(|| bad(C, format!("{kind}: window has no transform/scale track")))?;
    key_layer(s, cx, cid, win, "transform/scale", tr, "window", Shape { mul, sx: k, sy: ky, scale3: true, ..Shape::default() })?;
    Ok(win)
}

/// white-flash's lights and colour cast (1920×1080 px → the comp's), above the new shot or
/// above both shots (`top`).
#[allow(clippy::too_many_arguments)]
fn overlays(s: &mut Session, cx: &Cx, cid: ItemId, ovs: &[Value], ol: LayerId, nl: LayerId, tag: &str, out: &mut Added) -> Result<()> {
    let kind = cx.kind;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (sx, sy) = (f64::from(comp.width) / 1920.0, f64::from(comp.height) / 1080.0);
    let mut top = if comp.index_of(ol) < comp.index_of(nl) { ol } else { nl };
    for o in ovs.iter().take(MAX_ITEMS) {
        let oname = o.get("name").and_then(Value::as_str).unwrap_or("Overlay");
        let name = format!("{PREFIX}{kind} {oname}");
        let okind = o.get("kind").and_then(Value::as_str).unwrap_or("solid");
        let l = if okind == "adjustment" {
            layer_of(&run(s, cid, "layer.newAdjustment", json!({"name": name}))?)?
        } else {
            let colour = if okind == "light" { "#000000" } else { o.get("colour").and_then(Value::as_str).unwrap_or("#000000") };
            layer_of(&run(s, cid, "layer.newSolid", json!({"color": colour, "name": name}))?)?
        };
        out.layers.push(l.0);
        if let Some(r) = o.get("ramp") {
            let p = |k: &str| pt(r.get(k)).map(|p| json!([p[0] * sx, p[1] * sy])).ok_or_else(|| bad(C, format!("{kind}: ramp `{k}` missing")));
            let mut set = Map::new();
            set.insert("start".into(), p("start")?);
            set.insert("end".into(), p("end")?);
            for k in ["startColor", "endColor", "shape"] {
                if let Some(v) = r.get(k) {
                    set.insert(k.into(), v.clone());
                }
            }
            add_fx(s, cx, cid, l, "ec.generate.gradientramp", "Light", &Value::Object(set), out)?;
        }
        for e in arr(o, "effects").iter().take(MAX_ITEMS) {
            let id = e.get("effect").and_then(Value::as_str).unwrap_or_default();
            add_fx(s, cx, cid, l, id, e.get("name").and_then(Value::as_str).unwrap_or("Effect"), e.get("set").unwrap_or(&json!({})), out)?;
        }
        if let Some(mode) = o.get("blend").and_then(Value::as_str) {
            run(s, cid, "layer.setBlendMode", json!({"layers": [l.0], "mode": mode}))?;
        }
        let fin = cx.frame(o.get("in").and_then(Value::as_f64).unwrap_or(0.0))?;
        let fout = cx.frame(o.get("out").and_then(Value::as_f64).unwrap_or(0.0))?.max(fin.saturating_add(1));
        let (tin, tout) = (cx.fr.tick_of(fin), cx.fr.tick_of(fout));
        s.edit("Layer Timing", None, |proj, _| {
            let l = layer_mut(proj, cid, l)?;
            (l.in_point, l.out_point) = (tin, tout);
            l.comment = tag.to_string();
            Ok(())
        })?;
        if o.get("above").and_then(Value::as_str) == Some("new") {
            run(s, cid, "layer.arrange", json!({"layers": [l.0], "to": "front", "above": nl.0}))?;
        } else {
            run(s, cid, "layer.arrange", json!({"layers": [l.0], "to": "front", "above": top.0}))?;
            top = l;
        }
        if let Some(Value::Object(keys)) = o.get("keys") {
            for (p, tr) in keys {
                key_layer(s, cx, cid, l, p, tr, "overlay", Shape::default())?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- which layers, where

struct Pair {
    cid: ItemId,
    old: LayerId,
    new: LayerId,
}

fn layer_ref(comp: &effectcraft_project::Comp, v: &Value, what: &str) -> Result<LayerId> {
    resolve_layer(comp, v).ok_or_else(|| bad(C, format!("`{what}`: {}", super::no_layer(v))))
}

/// The two layers: `old` + `new` (or `layers: [old, new]`); else the two selected layers (the
/// one starting later is the new one); else, with nothing selected, the layer starting at the
/// playhead and the one under it that ends there.
fn pair(s: &Session, p: &Value) -> Result<Pair> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    match (p.get("old").filter(|v| !v.is_null()), p.get("new").filter(|v| !v.is_null())) {
        (Some(o), Some(n)) => return Ok(Pair { cid, old: layer_ref(comp, o, "old")?, new: layer_ref(comp, n, "new")? }),
        (Some(_), None) | (None, Some(_)) => return Err(bad(C, "give both `old` and `new` (or neither: the selection or the playhead picks them)")),
        (None, None) => {}
    }
    if let Some(v) = p.get("layers").filter(|v| !v.is_null()) {
        let a = v.as_array().filter(|a| a.len() == 2).ok_or_else(|| bad(C, "`layers` is [old, new]"))?;
        let (Some(o), Some(n)) = (a.first(), a.get(1)) else { return Err(bad(C, "`layers` is [old, new]")) };
        return Ok(Pair { cid, old: layer_ref(comp, o, "layers")?, new: layer_ref(comp, n, "layers")? });
    }
    let sel: Vec<&Layer> = if s.active_comp_id() == Some(cid) { s.state.selected_layers.iter().filter_map(|id| comp.layer(*id)).collect() } else { vec![] };
    match sel.as_slice() {
        [a, b] => {
            // The later in point is the new shot; at the same in point, the upper one.
            let ia = comp.index_of(a.id).unwrap_or(usize::MAX);
            let ib = comp.index_of(b.id).unwrap_or(usize::MAX);
            let a_new = (frame(a.in_point), std::cmp::Reverse(ia)) > (frame(b.in_point), std::cmp::Reverse(ib));
            let (old, new) = if a_new { (b.id, a.id) } else { (a.id, b.id) };
            Ok(Pair { cid, old, new })
        }
        [] => {
            let t = frame(s.time_of(cid));
            let shows = |l: &Layer| l.switches.video && !l.is_camera() && !l.is_light();
            for (i, n) in comp.layers.iter().enumerate() {
                if !shows(n) || frame(n.in_point) != t {
                    continue;
                }
                let below = comp.layers.iter().skip(i + 1).find(|&o| shows(o) && frame(o.out_point) == t);
                let any = || comp.layers.iter().find(|&o| o.id != n.id && shows(o) && frame(o.out_point) == t);
                if let Some(o) = below.or_else(any) {
                    return Ok(Pair { cid, old: o.id, new: n.id });
                }
            }
            Err(bad(C, "no cut at the playhead: select the two layers, or put the playhead where one layer ends and the next starts"))
        }
        _ => Err(bad(C, format!("select the two layers of the cut ({} selected)", sel.len()))),
    }
}

fn can_apply(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    pair(s, &json!({})).map(|_| ()).map_err(|e| e.to_string())
}

/// `centre`: [x, y] for both shots or {old, new} (px of a 1920×1080 layer).
fn centre_p(p: &Value) -> Result<(Option<[f64; 2]>, Option<[f64; 2]>)> {
    match p.get("centre").filter(|v| !v.is_null()) {
        None => Ok((None, None)),
        Some(v @ Value::Array(_)) => {
            let c = pt(Some(v)).ok_or_else(|| bad(C, "`centre` is [x, y] (px of a 1920×1080 layer) or {old, new}"))?;
            Ok((Some(c), Some(c)))
        }
        Some(Value::Object(o)) => {
            let one = |k: &str| match o.get(k).filter(|v| !v.is_null()) {
                None => Ok(None),
                Some(v) => pt(Some(v)).map(Some).ok_or_else(|| bad(C, format!("`centre.{k}` is [x, y]"))),
            };
            Ok((one("old")?, one("new")?))
        }
        Some(_) => Err(bad(C, "`centre` is [x, y] (px of a 1920×1080 layer) or {old, new}")),
    }
}

/// Every key frame of a kind (and its land), for the comp-length check.
fn span(cx: &Cx, k: &Value) -> Result<(i64, i64)> {
    let mut frames = vec![cx.cut_f, cx.frame(k.get("land").and_then(Value::as_f64).unwrap_or(0.0))?];
    if let Some(Value::Object(layers)) = k.get("layers") {
        for (role, spec) in layers {
            let tracks = arr(spec, "effects").iter().filter_map(|e| e.get("keys")).chain(spec.get("keys"));
            for keys in tracks {
                if let Value::Object(m) = keys {
                    for tr in m.values() {
                        frames.extend(cx.frames(tr, role)?);
                    }
                }
            }
        }
    }
    for o in arr(k, "overlays") {
        for t in ["in", "out"] {
            if let Some(x) = o.get(t).and_then(Value::as_f64) {
                frames.push(cx.frame(x)?);
            }
        }
    }
    Ok((frames.iter().copied().min().unwrap_or(cx.cut_f), frames.iter().copied().max().unwrap_or(cx.cut_f)))
}

pub(super) fn fmt_s(t: Tick) -> String {
    format!("{:.3}", t.seconds())
}

/// The new shot is a window this command made (into-window): it can't go into another one.
fn is_window(s: &Session, l: &Layer) -> bool {
    match l.source {
        LayerSource::Comp { item } => s.project.item(item).is_some_and(|i| i.name.starts_with(PREFIX) && i.name.ends_with(" Window")),
        _ => false,
    }
}

/// The cut's comp frame: `at` (comp s), else the new layer's in point.
pub(super) fn cut_frame(p: &Value, fr: FrameRate, nl: &Layer) -> Result<i64> {
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    match f_p(p, "at") {
        Some(a) if a.is_finite() && a.abs() <= 1e7 => Ok(frame(Tick::from_seconds_f64(a))),
        Some(_) => Err(bad(C, "`at` is the cut in comp seconds")),
        None if p.get("at").is_some_and(|v| !v.is_null()) => Err(bad(C, "`at` is the cut in comp seconds")),
        None => Ok(frame(nl.in_point)),
    }
}

/// The shots meet at the cut: the old one plays up to it, the new one from it.
pub(super) fn check_meets(fr: FrameRate, cut_f: i64, ol: &Layer, nl: &Layer) -> Result<()> {
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let meets = frame(ol.in_point) < cut_f && frame(ol.out_point) >= cut_f && frame(nl.in_point) <= cut_f && frame(nl.out_point) > cut_f;
    if meets {
        return Ok(());
    }
    Err(bad(
        C,
        format!(
            "the layers don't meet at the cut ({} s): “{}” plays {}–{} s and “{}” {}–{} s; the old layer must play up to the cut and the new one from it",
            fmt_s(fr.tick_of(cut_f)),
            ol.name,
            fmt_s(ol.in_point),
            fmt_s(ol.out_point),
            nl.name,
            fmt_s(nl.in_point),
            fmt_s(nl.out_point)
        ),
    ))
}

/// What the previous transition on a pair left that re-applying removed.
#[derive(Default, Clone, Copy)]
pub(super) struct Removed {
    pub effects: usize,
    pub layers: usize,
    pub items: usize,
}

/// A comment carries the pair's tag: exactly, or followed by more words (an FX cut's overlays
/// add " · reference ease: …"). "FxCut 3->4" never matches "FxCut 3->45 …".
pub(super) fn tagged(comment: &str, tag: &str) -> bool {
    comment == tag || comment.strip_prefix(tag).is_some_and(|rest| rest.starts_with(' '))
}

/// Whether a layer / item comment carries one of the pair's tags (any family; an audio keep copy
/// adds words after it).
fn pair_tagged(comment: &str, ztag: &str, ftag: &str, stag: &str) -> bool {
    tagged(comment, ztag) || tagged(comment, ftag) || tagged(comment, stag)
}

/// Re-applying on a pair: the previous transition goes first, whatever its family. The shots'
/// `ZoomCut …` / `FxCut …` / `SceneCut …` effects, the layers tagged for the pair (`ZoomCut
/// <old>-><new>`, `FxCut <old>-><new>`, `SceneCut <old>-><new>`), the precomps an FX or scene cut
/// tagged (unless something else uses them), the solids only those layers used (no orphans left in
/// the Solids folder), and an FX or scene cut's trims of the shots go back to its cut.
fn remove_previous(s: &mut Session, cid: ItemId, old: LayerId, new: LayerId) -> Result<Removed> {
    let ztag = format!("{PREFIX}{}->{}", old.0, new.0);
    let ftag = Family::Fx.tag(old, new);
    let stag = Family::Scene.tag(old, new);
    // Only this pair's effects: a shot shared with another transition keeps that one's (the old
    // shot's at its out point, the new shot's at its in point are this pair's).
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut mine: Vec<effectcraft_project::Uid> = vec![];
    for (id, side) in [(old, Side::Out), (new, Side::In)] {
        if let Some(l) = comp.layer(id) {
            mine.extend(safety::owned(l, side));
        }
    }
    let restore =
        transition_fx::previous_trims(Family::Fx, s, cid, old, new, &mine).or_else(|| transition_fx::previous_trims(Family::Scene, s, cid, old, new, &mine));
    let out = s.edit("Remove Transition", None, |proj, st| {
        let mut out = Removed::default();
        // Precomps tagged for the pair, kept while anything outside them uses them.
        let mut items: Vec<ItemId> =
            proj.items.values().filter(|i| i.as_comp().is_some() && (tagged(&i.comment, &ftag) || tagged(&i.comment, &stag))).map(|i| i.id).collect();
        for _ in 0..items.len() {
            let keep: Vec<ItemId> =
                items.iter().copied().filter(|id| proj.item_users(*id).iter().any(|(user, _)| *user != cid && !items.contains(user))).collect();
            if keep.is_empty() {
                break;
            }
            items.retain(|id| !keep.contains(id));
        }
        let mut solids: Vec<ItemId> = vec![];
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        if let Some((old_out, new_in)) = restore {
            if let (Some(t), Some(l)) = (old_out, c.layer_mut(old)) {
                l.out_point = t.max(l.in_point);
            }
            if let (Some(t), Some(l)) = (new_in, c.layer_mut(new)) {
                l.in_point = t.min(l.out_point);
            }
        }
        // The sound the transition kept: the shots get that span back.
        safety::restore_kept(c, &|cm| pair_tagged(cm, &ztag, &ftag, &stag));
        for l in c.layers.iter_mut_where(|l| l.id == old || l.id == new) {
            if let Some(fx) = l.props.sub_mut("effects") {
                let before = fx.children.len();
                fx.children.retain(|n| !matches!(n, Node::Group(g) if is_ours(&g.name) && mine.contains(&g.uid)));
                out.effects += before.saturating_sub(fx.children.len());
            }
        }
        let gone = |l: &Layer| l.id != old && l.id != new && pair_tagged(&l.comment, &ztag, &ftag, &stag);
        let doomed: Vec<LayerId> = c.layers.iter().filter(|l| gone(l)).map(|l| l.id).collect();
        for l in c.layers.iter().filter(|l| gone(l)) {
            if let LayerSource::Solid { item } = l.source {
                solids.push(item);
            }
        }
        let before = c.layers.len();
        c.layers.retain(|l| !gone(l));
        out.layers = before.saturating_sub(c.layers.len());
        // Nothing left points at a removed layer (a matte, a parent).
        for l in c.layers.iter_mut_where(|l| l.parent.is_some_and(|p| doomed.contains(&p)) || l.track_matte.is_some_and(|m| doomed.contains(&m.layer))) {
            if l.parent.is_some_and(|p| doomed.contains(&p)) {
                l.parent = None;
            }
            if l.track_matte.is_some_and(|m| doomed.contains(&m.layer)) {
                l.track_matte = None;
            }
        }
        for id in &items {
            if let Some(ic) = proj.comp(*id) {
                for l in ic.layers.iter() {
                    if let LayerSource::Solid { item } = l.source {
                        solids.push(item);
                    }
                }
            }
        }
        for id in &items {
            proj.items.remove(id);
        }
        out.items = items.len();
        st.project_selection.retain(|i| !items.contains(i));
        // A solid only the removed layers used goes with them.
        solids.sort_unstable_by_key(|i| i.0);
        solids.dedup();
        for sid in solids {
            if proj.item(sid).is_some_and(|i| matches!(i.kind, effectcraft_project::ItemKind::Solid(_))) && proj.item_users(sid).is_empty() {
                proj.items.remove(&sid);
                st.project_selection.retain(|i| *i != sid);
            }
        }
        Ok(out)
    })?;
    if out.items > 0 {
        s.sanitize_state();
    }
    Ok(out)
}

/// A transition effect's name (any family).
fn is_ours(name: &str) -> bool {
    name.starts_with(PREFIX) || name.starts_with(FX_PREFIX) || name.starts_with(SCENE_PREFIX)
}

/// What a family's apply hands back: the reply, the undo step's name, the layers to select, the
/// user's layers it moved in the layer order on purpose (an FX cut's restack, a scene cut's stack),
/// the comp frames it acts in, the shots (id before → the layer showing it now: into-window's
/// window), and the pair tag and family prefix its additions carry.
pub(super) struct Applied {
    pub reply: Value,
    pub label: String,
    pub select: Vec<LayerId>,
    pub arranged: Vec<LayerId>,
    pub span: (i64, i64),
    pub shots: Vec<(LayerId, LayerId)>,
    pub tag: String,
    pub prefix: &'static str,
}

/// Every kind of the three families: zoom cuts, FX cuts, scene cuts.
fn all_kinds() -> Vec<String> {
    let mut k = kinds();
    k.extend(transition_fx::kinds());
    k.extend(transition_scene::kinds());
    k
}

/// The comp's own layers before a transition (not the pair's tagged overlays): id, name, in, out,
/// top to bottom.
type Before = Vec<(LayerId, String, Tick, Tick)>;

fn before_layers(s: &Session, cid: ItemId, old: LayerId, new: LayerId) -> Before {
    let (ztag, ftag, stag) = (format!("{PREFIX}{}->{}", old.0, new.0), Family::Fx.tag(old, new), Family::Scene.tag(old, new));
    let Some(c) = s.project.comp(cid) else { return vec![] };
    c.layers
        .iter()
        .filter(|l| l.id == old || l.id == new || !pair_tagged(&l.comment, &ztag, &ftag, &stag))
        .map(|l| (l.id, l.name.clone(), l.in_point, l.out_point))
        .collect()
}

/// What the transition changed on the user's own layers: `{layer, name, what, from, to}` for an in
/// / out point (comp s, with `fromFrame` / `toFrame`), the layer order (`order`: 1 = the top of the
/// user's layers), or `precomposed` (moved into a precomp: into-window's window). The audio keep
/// copies are added by the caller (`audio-keep`).
fn moved(s: &Session, cid: ItemId, before: &Before, arranged: &[LayerId]) -> Vec<Value> {
    let Some(c) = s.project.comp(cid) else { return vec![] };
    let fr = c.frame_rate;
    let f = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let mut out = vec![];
    for (id, name, tin, tout) in before {
        match c.layer(*id) {
            None => out.push(json!({"layer": id.0, "name": name, "what": "precomposed"})),
            Some(l) => {
                for (what, a, b) in [("in", *tin, l.in_point), ("out", *tout, l.out_point)] {
                    if a != b {
                        out.push(
                            json!({"layer": id.0, "name": name, "what": what, "from": a.seconds(), "to": b.seconds(), "fromFrame": f(a), "toFrame": f(b)}),
                        );
                    }
                }
            }
        }
    }
    // The order among the user's layers that are still here (1 = the top).
    let ids: Vec<LayerId> = before.iter().map(|b| b.0).filter(|id| c.layer(*id).is_some()).collect();
    let after: Vec<LayerId> = c.layers.iter().map(|l| l.id).filter(|id| ids.contains(id)).collect();
    let pos = |list: &[LayerId], id: LayerId| list.iter().position(|x| *x == id).map(|i| i.saturating_add(1));
    let mut order = |id: LayerId| {
        if let (Some(a), Some(b)) = (pos(&ids, id), pos(&after, id))
            && a != b
        {
            let name = before.iter().find(|x| x.0 == id).map(|x| x.1.clone()).unwrap_or_default();
            out.push(json!({"layer": id.0, "name": name, "what": "order", "from": a, "to": b}));
        }
    };
    for id in arranged {
        order(*id);
    }
    // Anything else out of order (never expected): every layer whose place changed.
    let rest = |l: &[LayerId]| l.iter().copied().filter(|id| !arranged.contains(id)).collect::<Vec<_>>();
    if rest(&ids) != rest(&after) {
        for id in rest(&ids) {
            order(id);
        }
    }
    out
}

/// The three families of `transition.apply`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fam {
    Zoom,
    Fx,
    Scene,
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let t = table()?;
    let default = t.get("default").and_then(Value::as_str).unwrap_or("kick");
    let kind = str_p(p, "kind").map(str::trim).filter(|k| !k.is_empty()).unwrap_or(default).to_string();
    let fam = if t.get("kinds").and_then(|k| k.get(&kind)).is_some() {
        Fam::Zoom
    } else if transition_fx::is_kind(&kind) {
        Fam::Fx
    } else if transition_scene::is_kind(&kind) {
        Fam::Scene
    } else {
        return Err(bad(C, format!("unknown kind `{kind}`; kinds: {}", all_kinds().join(", "))));
    };
    // Each family's own options.
    let given = |k: &str| p.get(k).is_some_and(|v| !v.is_null());
    let family = match fam {
        Fam::Zoom => "a zoom cut",
        Fam::Fx => "an FX cut",
        Fam::Scene => "a scene cut",
    };
    let centre_ok = fam == Fam::Zoom || (fam == Fam::Scene && transition_scene::takes_centre(&kind));
    let options: [(&str, &str, bool); 5] = [
        ("restScale", "zoom cuts", fam == Fam::Zoom),
        ("centre", "zoom cuts and point-iris", centre_ok),
        ("dur", "FX cuts and scene cuts", fam != Fam::Zoom),
        ("colour", "FX cuts and scene cuts", fam != Fam::Zoom),
        ("dir", "scene cuts", fam == Fam::Scene),
    ];
    if let Some((k, whose, _)) = options.iter().find(|(k, _, ok)| !ok && given(k)) {
        return Err(bad(C, format!("`{k}` is for {whose}; {kind} is {family}")));
    }
    if fam == Fam::Scene {
        transition_scene::dir_matrix(&kind, p)?;
        transition_scene::centre_p(p)?;
    }
    let Pair { cid, old, new } = pair(s, p)?;
    if old == new {
        return Err(bad(C, "`old` and `new` are the same layer"));
    }
    if unlocked(s, cid, vec![old, new], C)?.len() != 2 {
        return Err(bad(C, "one of the two layers is locked"));
    }
    {
        let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
        let nl = comp.layer(new).ok_or_else(|| bad(C, "no new layer"))?;
        if is_window(s, nl) {
            return Err(bad(C, format!("“{}” is a zoom-cut window already: undo that transition first, or apply to the shot inside it", nl.name)));
        }
    }
    let snap = s.project.clone();
    let (history, state) = (s.history.clone(), s.state.clone());
    let before = before_layers(s, cid, old, new);
    let result = (|| -> Result<(Applied, Vec<Value>)> {
        // Re-applying on the same pair: the previous transition goes first (no stacking).
        let removed = remove_previous(s, cid, old, new)?;
        // The shots' spans with the previous transition gone, and the other transitions on them.
        let spans: Vec<(LayerId, Tick, Tick)> = s
            .project
            .comp(cid)
            .map(|c| c.layers.iter().filter(|l| l.id == old || l.id == new).map(|l| (l.id, l.in_point, l.out_point)).collect())
            .unwrap_or_default();
        let others = safety::others(s, cid, old, new);
        let a = match fam {
            Fam::Zoom => apply_zoom(s, p, &kind, cid, old, new, removed),
            Fam::Fx => transition_fx::apply(s, p, &kind, cid, old, new, removed),
            Fam::Scene => transition_scene::apply(s, p, &kind, cid, old, new, removed),
        }?;
        safety::check_windows(s, cid, &kind, a.span, &others)?;
        // Never cut the sound: what a trim took plays on from an audio-only copy.
        let kept = safety::keep_audio(s, cid, &a.shots, &spans, &a.tag, a.prefix)?;
        Ok((a, kept))
    })();
    match result {
        Ok((mut a, kept)) => {
            let mut mv = moved(s, cid, &before, &a.arranged);
            mv.extend(kept);
            if let Some(o) = a.reply.as_object_mut() {
                o.insert("moved".into(), Value::Array(mv));
            }
            super::batch::fold(s, &snap, &a.label, &history);
            if s.active_comp_id() == Some(cid) {
                s.state.selected_layers = a.select;
            }
            Ok(a.reply)
        }
        Err(e) => {
            s.project = snap;
            s.history = history;
            s.state = state;
            s.bump();
            Err(e)
        }
    }
}

/// A zoom cut on the pair (the previous transition is gone already).
fn apply_zoom(s: &mut Session, p: &Value, kind: &str, cid: ItemId, old: LayerId, new: LayerId, removed: Removed) -> Result<Applied> {
    let t = table()?;
    let k = kind_spec(t, kind)?;
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
    if comp.index_of(old) < comp.index_of(new) && frame(ol.out_point) > cut_f {
        return Err(bad(
            C,
            format!("“{}” is above “{}” and plays past the cut, so it would hide the new shot: trim it to the cut ({} s)", ol.name, nl.name, fmt_s(cut)),
        ));
    }
    let ref_fps = k.get("fps").and_then(Value::as_str).and_then(fps_of).unwrap_or(30.0);
    let (profile, _) = Profile::load(s.config.as_deref());
    let cx = Cx { kind, fr, fps, ref_fps, cut_f, profile: &profile, scale: 1.0 };
    let (lo, hi) = span(&cx, k)?;
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
    // restScale: the scale % the kind's rest layer settles at.
    let layers = k.get("layers").ok_or_else(|| bad(C, format!("{kind}: no layers in the table")))?;
    let rest_role = k.get("restLayer").and_then(Value::as_str).unwrap_or(if layers.get("window").is_some() { "window" } else { "new" });
    let rest = layers.get(rest_role).and_then(|l| l.get("rest")).and_then(Value::as_f64).filter(|r| r.is_finite() && *r > 0.0).unwrap_or(100.0);
    let mul = match p.get("restScale").filter(|v| !v.is_null()) {
        None => 1.0,
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && (1.0..=1000.0).contains(x))
            .map(|x| x / rest)
            .ok_or_else(|| bad(C, "`restScale` is a scale % (1 to 1000)"))?,
    };
    let (c_old, c_new) = centre_p(p)?;
    let land_f = cx.frame(k.get("land").and_then(Value::as_f64).unwrap_or(0.0))?;
    let title = k.get("title").and_then(Value::as_str).unwrap_or(kind).to_string();

    let mut added = Added::default();
    // Transform shutter samples: a smooth smear, no ghost copies (raised, never lowered).
    if let Some(cs) = k.get("comp") {
        let samples = cs.get("motionBlurSamples").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
        let limit = cs.get("adaptiveSampleLimit").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
        s.edit("Composition Settings", None, |proj, _| {
            let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
            if let Some(n) = samples {
                c.motion_blur_samples = c.motion_blur_samples.max(n.min(256));
            }
            if let Some(n) = limit {
                c.motion_blur_adaptive_limit = c.motion_blur_adaptive_limit.max(n.min(256));
            }
            Ok(())
        })?;
    }
    let mut new_layer = new;
    for role in ["old", "new", "window"] {
        let Some(spec) = layers.get(role) else { continue };
        let (m, grow) = if role == rest_role { (mul, (mul - 1.0).abs() > 1e-12) } else { (1.0, false) };
        match role {
            "old" => shot(s, &cx, cid, old, "old", spec, m, c_old, grow, &mut added)?,
            "new" => shot(s, &cx, cid, new, "new", spec, m, c_new, grow, &mut added)?,
            _ => {
                let w = k.get("window").ok_or_else(|| bad(C, format!("{kind}: no window in the table")))?;
                new_layer = window(s, &cx, cid, new, spec, w, m, &mut added)?;
                // The background goes with the final pair (old → window).
                let ftag = format!("{PREFIX}{}->{}", old.0, new_layer.0);
                for bg in added.layers.clone() {
                    tag_layer(s, cid, LayerId(bg), &ftag)?;
                }
            }
        }
    }
    let ovs = arr(k, "overlays");
    if !ovs.is_empty() {
        let ftag = format!("{PREFIX}{}->{}", old.0, new_layer.0);
        overlays(s, &cx, cid, ovs, old, new_layer, &ftag, &mut added)?;
    }
    let land = fr.tick_of(land_f);
    Ok(Applied {
        reply: json!({
            "kind": kind,
            "family": "zoom-cut",
            "title": title,
            "comp": cid.0,
            "old": old.0,
            "new": new_layer.0,
            "shot": new.0,
            "at": cut.seconds(),
            "cutFrame": cut_f,
            "land": land.seconds(),
            "landFrame": land_f,
            "restScale": rest * mul,
            "effects": added.effects,
            "layers": added.layers,
            "removed": {"effects": removed.effects, "layers": removed.layers, "items": removed.items},
        }),
        label: format!("Apply Transition Zoom Cut {title}"),
        select: vec![old, new_layer],
        arranged: vec![],
        span: (lo, hi),
        shots: vec![(old, old), (new, new_layer)],
        tag: format!("{PREFIX}{}->{}", old.0, new_layer.0),
        prefix: PREFIX,
    })
}

/// `transition.kinds`: the three families' kinds in menu order (zoom cuts, FX cuts, scene cuts),
/// with what they look like and are for, and the `dir` values each takes (scene cuts).
fn list(_: &mut Session, _: &Value) -> Result<Value> {
    let t = table()?;
    let default = t.get("default").and_then(Value::as_str).unwrap_or("kick");
    let mut out: Vec<Value> = kinds()
        .iter()
        .filter_map(|id| {
            let k = t.get("kinds")?.get(id)?;
            Some(json!({"kind": id, "family": "zoom-cut", "default": id == default, "title": k.get("title"), "energy": k.get("energy"), "land": k.get("land"), "looks": k.get("looks"), "use": k.get("use"), "dir": null, "dirs": []}))
        })
        .collect();
    out.extend(transition_fx::list());
    out.extend(transition_scene::list());
    Ok(Value::Array(out))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "transition.apply",
            "Apply Transition",
            ["Animation", "Transitions", "Zoom Cut"],
            None,
            "{kind?: zoom cuts kick (default) | blur-settle | push-in | into-window | twos-card | twos-push | zoom-through | whip | whip-pan | zoom-out-through | punch | flash | white-flash, FX cuts light-leak | overexposure-burn | film-burn | glitch | chromatic-aberration | ripple | vhs-tape | shutter | clock-wipe | grid-dissolve | gravity-drop | morph-circle | blur-through | directional-blur | page-burn, scene cuts push-slide | vertical-push | elastic-push | squeeze | circle-iris | diamond-iris | diagonal-split | card-flip-3d | crossfade | blur-crossfade | focus-pull | color-dip | staggered-blocks | horizontal-blinds | vertical-blinds | linear-wipe | point-iris | split-handoff (transition.kinds), comp?, old?: layer (outgoing shot), new?: layer (incoming shot), layers?: [old, new], at? (comp s, the cut; default the new layer's in point), restScale? (zoom cuts: scale % the kind's rest layer settles at; lower = mirrored edges), centre?: [x, y] | {old, new} (zoom cuts: zoom centre, px of a 1920×1080 layer; point-iris: [x, y] where the circle opens, default [960, 440], its last radius grows to still cover the frame), dur? (FX and scene cuts: the transition's length in s; every time scales by dur / the kind's window), colour?: #rrggbb | [#rrggbb…] (FX and scene cuts: the cover / backing / dip / blocks / strips colour; a list = grid-dissolve's or staggered-blocks' palette), dir? (scene cuts that have one, transition.kinds `dirs`: push-slide, elastic-push, squeeze, linear-wipe left | right | up | down; vertical-push up | down | left | right; staggered-blocks right | left | up | down; horizontal-blinds right | left; vertical-blinds down | up; diagonal-split up-right | up-left | down-right | down-left)} → {kind, family (zoom-cut | fx-cut | scene-cut), title, comp, old, new (the window layer for into-window), shot, at, cutFrame, land (comp s the move has settled), landFrame, restScale (zoom), start / startFrame / dur / comps (FX, scene), restacked (FX), stacked / dir (scene), effects: [{layer, effect, name, path}], layers (added overlay / background layers), removed {effects, layers, items}, moved: [{layer, name, what (in | out | order | precomposed | audio-keep), from, to, fromFrame / toFrame, of (audio-keep: the shot)}] (what changed on the user's own layers: in / out points in comp s and frames, order 1 = the top of them, the audio keep copies added)}. Safety: a trim cuts only the picture: a shot with sound gets an audio-only copy (video off, same source time, `<shot> audio (<Family> keep)`, right below it) over exactly the span the trim took, removed again with the transition (the shot gets that span back); copies of a shot inside a transition's precomps are muted. Errors that change nothing: a restack (FX old-over-new, scene new-over-old) past another layer, a kind that shows both shots together with another layer between them while it runs, a window that overlaps another transition's window on the same shot. Re-applying removes only the pair's own effects from a shot it shares with another transition. Without old/new: the two selected layers (the later in point is the new shot), or with nothing selected the layer starting at the playhead and the one under it ending there; pass `comp` with old/new when nothing is selected. Shots' transforms are never touched: every move is a `ZoomCut <kind> …` / `FxCut <kind> …` / `SceneCut <kind> …` effect, overlay or precomp; FX and scene cuts trim the shots to the kind (many start the new shot under or over the old one up to 1.2 s early: it needs that much footage; color-dip starts it 2 frames after the cut), and the scene cuts that slide or fade the new shot in over the old one (elastic-push, crossfade, blur-crossfade, focus-pull) move it right above the old one; linear-wipe and point-iris (a copy of the new shot uncovered over the old one, the new shot starting under it) move the old shot right above the new one; split-handoff trims nothing (masked sliding copies of both shots carry the split: the old shot must play from 1.9 s before the cut and the new one needs that much footage). Re-applying on the pair (any kind, any family) replaces the previous one. One undo step",
            can_apply,
            apply
        ),
        query!("transition.kinds", "Transition Kinds", "{}", list),
    ]
}
