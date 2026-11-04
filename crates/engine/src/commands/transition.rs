//! `transition.apply` (Animation ▸ Transitions ▸ Zoom Cut): one of Elias's 12 approved zoom cuts
//! at the cut between two layers. A port of motion-kit's shelf part `zoom-cut`
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
//! Re-applying on the same pair first removes the previous `ZoomCut …` effects and the overlays
//! it tagged (no stacking). Everything is one undo step.

use std::sync::OnceLock;

use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe};
use effectcraft_project::build::Ids;
use effectcraft_project::{Expression, ItemId, Layer, LayerId, LayerSource, Node, PropGroup, Property, Value as KV};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Map, Value, json};

use super::peak::{Graph, apply_graph};
use super::text_anim::json_value;
use super::{CommandSpec, bad, comp_id, f_p, has_comp, layer_mut, resolve_layer, str_p, unlocked};
use crate::taste::Profile;
use crate::{EngineError, Result, Session, cmd, query};

const C: &str = "transition.apply";

/// The zoom-cut kinds: a copy of motion-kit's `shelf/transitions/zoom-cut.kinds.json`.
pub const ZOOM_CUT_JSON: &str = include_str!("../../presets/zoom-cut.kinds.json");

/// Everything the command adds is named with this prefix (effects and layers).
const PREFIX: &str = "ZoomCut ";
/// Most keys one track may hold, so a bad table can't allocate without bound.
const MAX_KEYS: usize = 64;
/// Farthest a track time may sit from the cut, in seconds.
const MAX_OFFSET: f64 = 600.0;
/// Overlays and effects per kind.
const MAX_ITEMS: usize = 32;

fn table() -> Result<&'static Value> {
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
fn fps_of(s: &str) -> Option<f64> {
    let f = match s.split_once('/') {
        Some((a, b)) => a.trim().parse::<f64>().ok()? / b.trim().parse::<f64>().ok()?,
        None => s.trim().parse::<f64>().ok()?,
    };
    (f.is_finite() && f > 0.0).then_some(f)
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn pt(v: Option<&Value>) -> Option<[f64; 2]> {
    let a = v?.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

/// A `#rrggbb` string as a colour value (straight, alpha 1), else the JSON shaped like `like`.
fn value_like(v: &Value, like: &KV) -> Option<KV> {
    if let Value::String(s) = v {
        let c = effectcraft_color::Rgba::from_hex(s)?;
        return Some(KV::Color([f64::from(c.r), f64::from(c.g), f64::from(c.b), 1.0]));
    }
    json_value(v, like)
}

/// The comp's frames around the cut.
struct Cx<'a> {
    kind: &'a str,
    fr: FrameRate,
    fps: f64,
    /// The reference's frame rate (the kind's `fps`).
    ref_fps: f64,
    /// The cut's comp frame.
    cut_f: i64,
    profile: &'a Profile,
}

impl Cx<'_> {
    /// Comp frame of `t` seconds after the cut.
    fn frame(&self, t: f64) -> Result<i64> {
        if !t.is_finite() || t.abs() > MAX_OFFSET {
            return Err(bad(C, format!("{}: a track time {t} s is out of range", self.kind)));
        }
        Ok(self.cut_f.saturating_add((t * self.fps).round() as i64))
    }

    /// A track's keys as comp frames: an old-shot key in the last reference frame before the cut
    /// goes on the last comp frame before it (a Launch out's peak; in seconds it would land two
    /// frames early at 60 fps and freeze that frame), and no two keys share a frame.
    fn frames(&self, tr: &Value, role: &str) -> Result<Vec<i64>> {
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
                let m = if zoom { sh.mul } else { 1.0 };
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
    Ok(())
}

/// Set an effect's static values (`set`); a parameter the effect doesn't have is an error.
fn set_values(g: &mut PropGroup, set: Option<&Value>, kind: &str) -> Result<()> {
    let Some(Value::Object(m)) = set else { return Ok(()) };
    let gname = g.name.clone();
    for (path, v) in m {
        let pr = g.prop_mut(path).ok_or_else(|| bad(C, format!("{kind}: {gname} has no parameter `{path}`")))?;
        pr.value = value_like(v, &pr.value).ok_or_else(|| bad(C, format!("{kind}: {gname}/{path} doesn't take {v}")))?;
    }
    Ok(())
}

fn new_effect(next: &mut u64, effect: &str, name: &str, size: [f64; 2], kind: &str) -> Result<PropGroup> {
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
fn layer_times(fr: FrameRate, l: &Layer, frames: &[i64]) -> Vec<Tick> {
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
            let mut g = new_effect(&mut next, "ec.stylize.motiontile", &format!("{label} Mirror edges"), fx_size, kind)?;
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
fn run(s: &mut Session, cid: ItemId, id: &str, mut p: Value) -> Result<Value> {
    if let Some(o) = p.as_object_mut() {
        o.insert("comp".into(), json!(cid.0));
    }
    s.execute(id, p)
}

fn layer_of(v: &Value) -> Result<LayerId> {
    v.get("layer").and_then(Value::as_u64).map(LayerId).ok_or_else(|| bad(C, "a sub-command returned no layer"))
}

/// Static values on a layer's properties (`path` → JSON).
fn set_props(s: &mut Session, cid: ItemId, lid: LayerId, vals: &[(&str, Value)]) -> Result<()> {
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
fn put_below(s: &mut Session, cid: ItemId, layer: LayerId, over: LayerId) -> Result<()> {
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
    let foot = s.project.comp(wcomp).and_then(|c| c.layers.iter().last().map(|l| l.id)).ok_or_else(|| bad(C, "the window comp is empty"))?;
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

fn fmt_s(t: Tick) -> String {
    format!("{:.3}", t.seconds())
}

/// The new shot is a window this command made (into-window): it can't go into another one.
fn is_window(s: &Session, l: &Layer) -> bool {
    match l.source {
        LayerSource::Comp { item } => s.project.item(item).is_some_and(|i| i.name.starts_with(PREFIX) && i.name.ends_with(" Window")),
        _ => false,
    }
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let t = table()?;
    let default = t.get("default").and_then(Value::as_str).unwrap_or("kick");
    let kind = str_p(p, "kind").map(str::trim).filter(|k| !k.is_empty()).unwrap_or(default).to_string();
    let k = kind_spec(t, &kind)?;
    let Pair { cid, old, new } = pair(s, p)?;
    if old == new {
        return Err(bad(C, "`old` and `new` are the same layer"));
    }
    if unlocked(s, cid, vec![old, new], C)?.len() != 2 {
        return Err(bad(C, "one of the two layers is locked"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    if !(fps.is_finite() && fps > 0.0) {
        return Err(bad(C, "the comp has no frame rate"));
    }
    let (ol, nl) = (comp.layer(old).ok_or_else(|| bad(C, "no old layer"))?, comp.layer(new).ok_or_else(|| bad(C, "no new layer"))?);
    if is_window(s, nl) {
        return Err(bad(C, format!("“{}” is a zoom-cut window already: undo that transition first, or apply to the shot inside it", nl.name)));
    }
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let cut_f = match f_p(p, "at") {
        Some(a) if a.is_finite() && a.abs() <= 1e7 => frame(Tick::from_seconds_f64(a)),
        Some(_) => return Err(bad(C, "`at` is the cut in comp seconds")),
        None if p.get("at").is_some_and(|v| !v.is_null()) => return Err(bad(C, "`at` is the cut in comp seconds")),
        None => frame(nl.in_point),
    };
    let cut = fr.tick_of(cut_f);
    // The shots meet at the cut: the old one plays up to it, the new one from it.
    let meets = frame(ol.in_point) < cut_f && frame(ol.out_point) >= cut_f && frame(nl.in_point) <= cut_f && frame(nl.out_point) > cut_f;
    if !meets {
        return Err(bad(
            C,
            format!(
                "the layers don't meet at the cut ({} s): “{}” plays {}–{} s and “{}” {}–{} s; the old layer must play up to the cut and the new one from it",
                fmt_s(cut),
                ol.name,
                fmt_s(ol.in_point),
                fmt_s(ol.out_point),
                nl.name,
                fmt_s(nl.in_point),
                fmt_s(nl.out_point)
            ),
        ));
    }
    if comp.index_of(old) < comp.index_of(new) && frame(ol.out_point) > cut_f {
        return Err(bad(
            C,
            format!("“{}” is above “{}” and plays past the cut, so it would hide the new shot: trim it to the cut ({} s)", ol.name, nl.name, fmt_s(cut)),
        ));
    }
    let ref_fps = k.get("fps").and_then(Value::as_str).and_then(fps_of).unwrap_or(30.0);
    let (profile, _) = Profile::load(s.config.as_deref());
    let cx = Cx { kind: &kind, fr, fps, ref_fps, cut_f, profile: &profile };
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
    let title = k.get("title").and_then(Value::as_str).unwrap_or(&kind).to_string();

    let snap = s.project.clone();
    let (history, state) = (s.history.clone(), s.state.clone());
    let mut added = Added::default();
    let result = (|| -> Result<(LayerId, usize, usize)> {
        // Re-applying on the same pair: the previous zoom cut goes first (no stacking).
        let tag = format!("{PREFIX}{}->{}", old.0, new.0);
        let removed = s.edit("Remove Zoom Cut", None, |proj, _| {
            let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
            let mut fx_n = 0usize;
            for l in c.layers.iter_mut_where(|l| l.id == old || l.id == new) {
                if let Some(fx) = l.props.sub_mut("effects") {
                    let before = fx.children.len();
                    fx.children.retain(|n| !matches!(n, Node::Group(g) if g.name.starts_with(PREFIX)));
                    fx_n += before.saturating_sub(fx.children.len());
                }
            }
            let before = c.layers.len();
            c.layers.retain(|l| l.id == old || l.id == new || l.comment != tag);
            Ok((fx_n, before.saturating_sub(c.layers.len())))
        })?;
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
        Ok((new_layer, removed.0, removed.1))
    })();
    match result {
        Ok((new_layer, fx_removed, layers_removed)) => {
            super::batch::fold(s, &snap, &format!("Apply Transition Zoom Cut {title}"));
            if s.active_comp_id() == Some(cid) {
                s.state.selected_layers = vec![old, new_layer];
            }
            let land = fr.tick_of(land_f);
            Ok(json!({
                "kind": kind,
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
                "removed": {"effects": fx_removed, "layers": layers_removed},
            }))
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

/// `transition.kinds`: the zoom-cut kinds in menu order, with what they look like and are for.
fn list(_: &mut Session, _: &Value) -> Result<Value> {
    let t = table()?;
    Ok(Value::Array(
        kinds()
            .iter()
            .filter_map(|id| {
                let k = t.get("kinds")?.get(id)?;
                Some(json!({"kind": id, "title": k.get("title"), "energy": k.get("energy"), "land": k.get("land"), "looks": k.get("looks"), "use": k.get("use")}))
            })
            .collect(),
    ))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "transition.apply",
            "Apply Transition",
            ["Animation", "Transitions", "Zoom Cut"],
            None,
            "{kind?: kick (default) | blur-settle | push-in | into-window | twos-card | twos-push | zoom-through | whip | zoom-out-through | punch | flash | white-flash (transition.kinds), comp?, old?: layer (outgoing shot), new?: layer (incoming shot), layers?: [old, new], at? (comp s, the cut; default the new layer's in point), restScale? (scale % the kind's rest layer settles at; lower = mirrored edges), centre?: [x, y] | {old, new} (zoom centre, px of a 1920×1080 layer)} → {kind, title, comp, old, new (the window layer for into-window), shot, at, cutFrame, land (comp s the move has settled), landFrame, restScale, effects: [{layer, effect, name, path}], layers (added overlay / background layers), removed {effects, layers}}. Without old/new: the two selected layers (the later in point is the new shot), or with nothing selected the layer starting at the playhead and the one under it ending there; pass `comp` with old/new when nothing is selected. Shots' transforms are never touched: every move is a `ZoomCut <kind> …` effect; re-applying on the pair replaces the previous one. One undo step",
            can_apply,
            apply
        ),
        query!("transition.kinds", "Transition Kinds", "{}", list),
    ]
}
