//! `clip.move` (Animation ▸ Clip Moves ▸ …): one of Elias's 7 approved moves on ONE footage
//! layer (a talking head, b-roll): punch, zoom-ladder, chain, crop-reframe, retime, speed-ramp,
//! freeze. A port of motion-kit's shelf part `clip-move` (`shelf/camera/clip-move.jsx`, the
//! HyperFrames keyframes skill's "creator editing" rows rebuilt natively in hf-keyframes-demo and
//! approved on 5 Nov 2026); every number comes from the same data table, shipped here as
//! `presets/clip-move.kinds.json` (re-copied when the shelf table changes, see its sidecar).
//!
//! The shot's own transform, in / out points, parent and place in the stack are never touched:
//! - framing (punch, zoom-ladder, chain): a `ClipMove <kind> Frame` Transform effect (anchor
//!   [0, 0], position + uniform scale keyed, a straight path, its own shutter off) showing the
//!   table's point at the frame centre at the table's scale, clamped so no frame edge ever shows;
//! - crop (crop-reframe): a keyed `ClipMove <kind> Crop` rect mask on the shot (clamped inside the
//!   frame) and a `ClipMove <kind> BG` solid right below it (`bg`, default the table's #08080b);
//! - speed (retime, speed-ramp, freeze): the shot's Time Remap, the picture only. A shot with sound
//!   gets an audio-only copy `<name> audio (ClipMove keep)` (video off, no remap, the same source
//!   time, right below it, over its whole span) and its own audio goes off, so the mix is what it
//!   was. A speed move on a layer with time remapping of its own or a time stretch is an error.
//!
//! Times are seconds after `at` (the kind's first frame), keyed on the comp's own frames. Added
//! effects, masks and markers are named `ClipMove <kind> …`, added layers tagged `ClipMove <layer>
//! · …` (comment); calling `clip.move` again on the layer replaces the previous ClipMove (any kind):
//! its effects, masks, markers, BG (and its solid when nothing else uses it), audio copy (the shot's
//! audio back on) and time remapping go first. One undo step; any error rolls everything back.

use std::sync::OnceLock;

use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe, ShapePath};
use effectcraft_project::{ItemId, ItemKind, Layer, LayerId, LayerSource, Marker, Node, Value as KV};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::transition::{Cx, finish_track, layer_of, new_effect, put_below, run, set_values};
use super::transition_safety::{AUDIO_KEEP, MUTED, has_audio};
use super::{CommandSpec, bad, comp_id, f_p, has_comp, layer_mut, resolve_layer, str_p, unlocked};
use crate::taste::Profile;
use crate::{EngineError, Result, Session, cmd, query};

const C: &str = "clip.move";

/// The clip-move kinds: a copy of motion-kit's `shelf/camera/clip-move.kinds.json`.
pub const CLIP_MOVE_JSON: &str = include_str!("../../presets/clip-move.kinds.json");

/// Everything the command adds is named (effects, masks, markers, layers) or tagged with this.
pub(super) const PREFIX: &str = "ClipMove ";
/// Most keys one track may hold, so a bad table can't allocate without bound.
const MAX_KEYS: usize = 64;
/// Farthest a key may sit from `at`, seconds.
const MAX_OFFSET: f64 = 600.0;
/// The marker comment ending of a speed move (its time remapping is the ClipMove's).
const REMAP_MARK: &str = " · time remap";

fn table() -> Result<&'static Value> {
    static T: OnceLock<std::result::Result<Value, String>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(CLIP_MOVE_JSON).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| EngineError::Other(format!("{C}: the clip-move table doesn't parse: {e}")))
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

fn kind_spec(kind: &str) -> Result<&'static Value> {
    table()?.get("kinds").and_then(|k| k.get(kind)).ok_or_else(|| bad(C, format!("unknown kind `{kind}`; kinds: {}", kinds().join(", "))))
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn pt(v: Option<&Value>) -> Option<[f64; 2]> {
    let a = v?.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

/// `max(lo, min(hi, v))` (the shelf's clamp: `lo` wins when the range is empty, never a panic).
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    lo.max(hi.min(v))
}

fn fmt_s(t: Tick) -> String {
    format!("{:.3}", t.seconds())
}

/// The move's comp frames: `at_f` + each time × fps (rounded); two keys never share a frame.
fn frames(at_f: i64, fps: f64, ts: &[Value], kind: &str) -> Result<Vec<i64>> {
    if ts.len() < 2 || ts.len() > MAX_KEYS {
        return Err(bad(C, format!("{kind}: a track needs 2 to {MAX_KEYS} times, has {}", ts.len())));
    }
    let mut out: Vec<i64> = Vec::with_capacity(ts.len());
    for t in ts {
        let t = t.as_f64().filter(|t| t.is_finite() && t.abs() <= MAX_OFFSET).ok_or_else(|| bad(C, format!("{kind}: a track time is out of range")))?;
        let mut f = at_f.saturating_add((t * fps).round() as i64);
        if let Some(prev) = out.last()
            && f <= *prev
        {
            f = prev.saturating_add(1);
        }
        out.push(f);
    }
    Ok(out)
}

/// One comp frame of `at_f` + `t` s.
fn frame_of(at_f: i64, fps: f64, t: f64, kind: &str) -> Result<i64> {
    if !t.is_finite() || t.abs() > MAX_OFFSET {
        return Err(bad(C, format!("{kind}: a time {t} s is out of range")));
    }
    Ok(at_f.saturating_add((t * fps).round() as i64))
}

/// The layer's px size: its source, else its effect bounds, else the comp.
fn layer_size(s: &Session, cid: ItemId, l: &Layer) -> Result<[f64; 2]> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (fx_size, _) = effectcraft_render::effect_bounds(&s.project, comp, l);
    let (w, h) = effectcraft_render::source_size(&s.project, l);
    let (w, h) = if w == 0 || h == 0 { (fx_size[0], fx_size[1]) } else { (f64::from(w), f64::from(h)) };
    Ok(if w > 0.0 && h > 0.0 { [w, h] } else { [f64::from(comp.width), f64::from(comp.height)] })
}

/// The source's duration (footage that isn't a still, a precomp); `None` = no end (a still, a solid).
fn source_duration(s: &Session, l: &Layer) -> Option<Tick> {
    match &l.source {
        LayerSource::Footage { item } | LayerSource::Comp { item } => s.project.item(*item)?.duration(),
        _ => None,
    }
}

/// What the previous ClipMove on the layer left (read only).
#[derive(Default)]
struct Previous {
    /// A speed move's time remapping (its marker says so).
    remap: bool,
    /// Its audio copy turned the shot's audio off.
    unmute: bool,
}

/// The tag on every layer a ClipMove on `lid` adds: `ClipMove <lid> · …`.
fn tag(lid: LayerId) -> String {
    format!("{PREFIX}{}", lid.0)
}

fn tagged_for(comment: &str, lid: LayerId) -> bool {
    comment.strip_prefix(&tag(lid)).is_some_and(|rest| rest.starts_with(" · "))
}

fn previous(s: &Session, cid: ItemId, lid: LayerId) -> Previous {
    let Some(c) = s.project.comp(cid) else { return Previous::default() };
    let remap = c.layer(lid).is_some_and(|l| l.markers.iter().any(|m| m.comment.starts_with(PREFIX) && m.comment.ends_with(REMAP_MARK)));
    let unmute = c.layers.iter().any(|l| tagged_for(&l.comment, lid) && l.comment.ends_with(MUTED));
    Previous { remap, unmute }
}

/// What a re-call removed.
#[derive(Default, Clone, Copy)]
struct Removed {
    effects: usize,
    masks: usize,
    markers: usize,
    layers: usize,
    items: usize,
}

/// The previous ClipMove on the layer goes (any kind): the layers tagged for it and the solids only
/// they used, the shot's audio back on, its time remapping, `ClipMove …` effects, masks and markers.
fn clean(s: &mut Session, cid: ItemId, lid: LayerId, prev: &Previous) -> Result<Removed> {
    let (remap, unmute) = (prev.remap, prev.unmute);
    let out = s.edit("Remove Clip Move", None, |proj, st| {
        let mut out = Removed::default();
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let gone = |l: &Layer| l.id != lid && tagged_for(&l.comment, lid);
        let doomed: Vec<LayerId> = c.layers.iter().filter(|l| gone(l)).map(|l| l.id).collect();
        let mut solids: Vec<ItemId> =
            c.layers.iter().filter(|l| gone(l)).filter_map(|l| if let LayerSource::Solid { item } = l.source { Some(item) } else { None }).collect();
        let before = c.layers.len();
        c.layers.retain(|l| !gone(l));
        out.layers = before.saturating_sub(c.layers.len());
        for l in c.layers.iter_mut_where(|l| l.parent.is_some_and(|p| doomed.contains(&p)) || l.track_matte.is_some_and(|m| doomed.contains(&m.layer))) {
            if l.parent.is_some_and(|p| doomed.contains(&p)) {
                l.parent = None;
            }
            if l.track_matte.is_some_and(|m| doomed.contains(&m.layer)) {
                l.track_matte = None;
            }
        }
        let l = c.layer_mut(lid).ok_or_else(|| bad(C, "the layer is gone"))?;
        if unmute {
            l.switches.audio = true;
        }
        if remap {
            l.props.children.retain(|n| n.match_id() != "timeRemap");
        }
        let ours = |n: &Node| matches!(n, Node::Group(g) if g.name.starts_with(PREFIX));
        if let Some(fx) = l.props.sub_mut("effects") {
            let n = fx.children.len();
            fx.children.retain(|c| !ours(c));
            out.effects = n.saturating_sub(fx.children.len());
        }
        if let Some(ms) = l.props.sub_mut("masks") {
            let n = ms.children.len();
            ms.children.retain(|c| !ours(c));
            out.masks = n.saturating_sub(ms.children.len());
        }
        let n = l.markers.len();
        l.markers.retain(|m| !m.comment.starts_with(PREFIX));
        out.markers = n.saturating_sub(l.markers.len());
        // A solid only the removed layers used goes with them.
        solids.sort_unstable_by_key(|i| i.0);
        solids.dedup();
        for sid in solids {
            if proj.item(sid).is_some_and(|i| matches!(i.kind, ItemKind::Solid(_))) && proj.item_users(sid).is_empty() {
                proj.items.remove(&sid);
                st.project_selection.retain(|i| *i != sid);
                out.items += 1;
            }
        }
        Ok(out)
    })?;
    if out.layers > 0 || out.effects > 0 || out.masks > 0 || remap {
        s.sanitize_state();
    }
    Ok(out)
}

/// One move being applied.
struct Mv<'a> {
    kind: &'a str,
    k: &'a Value,
    cid: ItemId,
    lid: LayerId,
    fr: FrameRate,
    fps: f64,
    at_f: i64,
    /// The subject point (px of 1920×1080) minus the one the kind was built on.
    shift: [f64; 2],
    profile: &'a Profile,
}

impl Mv<'_> {
    fn name(&self, what: &str) -> String {
        format!("{PREFIX}{} {what}", self.kind)
    }
    /// The Graph eases of a keyed track (`hold` = a hold key: a one-frame jump at the next key).
    fn ease(&self, pr: &mut effectcraft_project::Property, keys: Vec<Keyframe>, eases: &[Value]) -> Result<()> {
        let cx = Cx { kind: self.kind, fr: self.fr, fps: self.fps, ref_fps: self.fps, cut_f: self.at_f, profile: self.profile, scale: 1.0 };
        let hold = |e: &Value| e.as_str() == Some("hold");
        let plain: Vec<Value> = eases.iter().map(|e| if hold(e) { json!("linear") } else { e.clone() }).collect();
        finish_track(&cx, pr, keys, &json!({ "ease": plain }), 1.0)?;
        for (i, e) in eases.iter().enumerate() {
            if hold(e)
                && let Some(k) = pr.keys.get_mut(i)
            {
                k.out_interp = Interp::Hold;
            }
        }
        Ok(())
    }
}

/// framing: a Transform effect, anchor [0, 0], so position P and scale m show layer point p at
/// P + m·p: the table's point at the frame centre, clamped so no edge shows.
fn framing(s: &mut Session, mv: &Mv, l: &Layer) -> Result<Value> {
    let kind = mv.kind;
    let fk = mv.k.get("frame").ok_or_else(|| bad(C, format!("{kind}: no frame in the table")))?;
    let [w, h] = layer_size(s, mv.cid, l)?;
    let (kx, ky) = (w / 1920.0, h / 1080.0);
    let d = mv.shift;
    let fr = frames(mv.at_f, mv.fps, arr(fk, "t"), kind)?;
    let vs = arr(fk, "v");
    let eases = arr(fk, "ease");
    if vs.len() != fr.len() || eases.len().saturating_add(1) != fr.len() {
        return Err(bad(C, format!("{kind}: the frame track has {} times, {} values and {} eases", fr.len(), vs.len(), eases.len())));
    }
    let mut pos: Vec<[f64; 2]> = vec![];
    let mut sc: Vec<f64> = vec![];
    for v in vs {
        let s_ = v.get(0).and_then(Value::as_f64).filter(|x| x.is_finite() && *x > 0.0 && *x <= 10_000.0);
        let (Some(s_), Some(p)) = (s_, pt(v.get(1))) else { return Err(bad(C, format!("{kind}: a frame value is [scale %, [x, y]]"))) };
        let m = s_ / 100.0;
        let (fx, fy) = ((p[0] + d[0]) * kx, (p[1] + d[1]) * ky);
        pos.push([clamp(w / 2.0 - m * fx, w * (1.0 - m), 0.0), clamp(h / 2.0 - m * fy, h * (1.0 - m), 0.0)]);
        sc.push(s_);
    }
    let times: Vec<Tick> = fr.iter().map(|f| l.layer_time(mv.fr.tick_of(*f))).collect();
    let comp = s.project.comp(mv.cid).ok_or(EngineError::NoComp)?;
    let (fx_size, _) = effectcraft_render::effect_bounds(&s.project, comp, l);
    let name = mv.name("Frame");
    let (lid, cid) = (mv.lid, mv.cid);
    let r = s.edit(&name, None, |proj, _| {
        let mut next = proj.next_id;
        let layer = layer_mut(proj, cid, lid)?;
        let lname = layer.name.clone();
        let fx = layer.props.sub_mut("effects").ok_or_else(|| bad(C, format!("“{lname}” can't take effects")))?;
        let mut g = new_effect(&mut next, "ec.distort.transform", &name, fx_size, kind)?;
        set_values(&mut g, Some(&json!({"useCompositionShutterAngle": false, "shutterAngle": 0, "anchor": [0.0, 0.0]})), kind)?;
        {
            let pr = g.prop_mut("position").ok_or_else(|| bad(C, "the Transform effect has no position"))?;
            let keys: Vec<Keyframe> = times.iter().zip(&pos).map(|(t, p)| Keyframe::new(*t, KV::Vec2(*p))).collect();
            mv.ease(pr, keys, eases)?;
            // A straight path between every key (no overshoot).
            for k in &mut pr.keys {
                (k.spatial_auto, k.spatial_continuous, k.spatial_in, k.spatial_out) = (false, false, [0.0; 3], [0.0; 3]);
            }
        }
        {
            let pr = g.prop_mut("scaleHeight").ok_or_else(|| bad(C, "the Transform effect has no scaleHeight"))?;
            let keys: Vec<Keyframe> = times.iter().zip(&sc).map(|(t, v)| Keyframe::new(*t, KV::Scalar(*v))).collect();
            mv.ease(pr, keys, eases)?;
        }
        let uid = g.uid;
        fx.children.push(Node::Group(g));
        let n = fx.children.len();
        proj.next_id = next;
        Ok(json!({"effect": uid, "name": name, "path": format!("effects/#{n}")}))
    })?;
    Ok(json!({"effect": r, "frames": fr, "position": pos, "scale": sc}))
}

/// A rect [x0, y0, x1, y1] (px of 1920×1080 × `k`) as a closed straight mask path.
fn rect_path(r: [f64; 4], k: [f64; 2]) -> KV {
    let [x0, y0, x1, y1] = [r[0] * k[0], r[1] * k[1], r[2] * k[0], r[3] * k[1]];
    KV::path(ShapePath {
        vertices: vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
        in_tangents: vec![[0.0; 2]; 4],
        out_tangents: vec![[0.0; 2]; 4],
        closed: true,
        feather: vec![],
    })
}

/// `bg`: `None` = the table's backing, `Some(None)` = none (false), `Some(Some(c))` = that colour.
fn bg_p(p: &Value) -> Result<Option<Option<String>>> {
    let err = || bad(C, "`bg` is #rrggbb, or false (no backing: what is under the shot shows)");
    match p.get("bg") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => Ok(None),
        Some(Value::Bool(false)) => Ok(Some(None)),
        Some(Value::String(c)) if c.len() == 7 && c.starts_with('#') && c.get(1..).is_some_and(|h| h.chars().all(|x| x.is_ascii_hexdigit())) => {
            Ok(Some(Some(c.clone())))
        }
        Some(_) => Err(err()),
    }
}

/// crop: a rect mask on the shot (layer space), keyed and clamped inside the frame; the backing
/// right below the shot while it plays from `at`.
fn crop(s: &mut Session, mv: &Mv, l: &Layer, bg: Option<Option<String>>) -> Result<Value> {
    let kind = mv.kind;
    let mk = mv.k.get("mask").ok_or_else(|| bad(C, format!("{kind}: no mask in the table")))?;
    let [w, h] = layer_size(s, mv.cid, l)?;
    let kk = [w / 1920.0, h / 1080.0];
    let d = mv.shift;
    let fr = frames(mv.at_f, mv.fps, arr(mk, "t"), kind)?;
    let vs = arr(mk, "v");
    let eases = arr(mk, "ease");
    if vs.len() != fr.len() || eases.len().saturating_add(1) != fr.len() {
        return Err(bad(C, format!("{kind}: the mask track has {} times, {} values and {} eases", fr.len(), vs.len(), eases.len())));
    }
    let mut rects: Vec<[f64; 4]> = vec![];
    for v in vs {
        let n: Vec<f64> = v.as_array().map(|a| a.iter().filter_map(Value::as_f64).filter(|x| x.is_finite()).collect()).unwrap_or_default();
        let [x0, y0, x1, y1] = n[..] else { return Err(bad(C, format!("{kind}: a mask value is [x0, y0, x1, y1]"))) };
        let (rw, rh) = (x1 - x0, y1 - y0);
        let (a, b) = (clamp(x0 + d[0], 0.0, 1920.0 - rw), clamp(y0 + d[1], 0.0, 1080.0 - rh));
        rects.push([a, b, a + rw, b + rh]);
    }
    let times: Vec<Tick> = fr.iter().map(|f| l.layer_time(mv.fr.tick_of(*f))).collect();
    let (cid, lid) = (mv.cid, mv.lid);
    let m = run(s, cid, "layer.addMask", json!({"layer": lid.0, "shape": "rect", "rect": [0.0, 0.0, w, h]}))?;
    let mp = m.get("path").and_then(Value::as_str).ok_or_else(|| bad(C, "layer.addMask returned no path"))?.to_string();
    let muid = m.get("mask").and_then(Value::as_u64).ok_or_else(|| bad(C, "layer.addMask returned no mask"))?;
    let name = mv.name("Crop");
    let path = format!("{mp}/path");
    s.edit("Add Keyframes", None, |proj, _| {
        let layer = layer_mut(proj, cid, lid)?;
        if let Some(g) = layer.props.find_group_mut(muid) {
            g.name = name.clone();
        }
        let pr = layer.props.prop_mut(&path).ok_or_else(|| bad(C, format!("{kind}: the mask has no path")))?;
        let keys: Vec<Keyframe> = times.iter().zip(&rects).map(|(t, r)| Keyframe::new(*t, rect_path(*r, kk))).collect();
        mv.ease(pr, keys, eases)
    })?;
    let colour = match bg {
        None => mv.k.get("bg").and_then(Value::as_str).map(String::from),
        Some(c) => c,
    };
    let mut bg_layer = Value::Null;
    if let Some(col) = colour {
        let b = layer_of(&run(s, cid, "layer.newSolid", json!({"color": col, "name": mv.name("BG")}))?)?;
        let tin = l.in_point.max(mv.fr.tick_of(mv.at_f));
        let (tout, t) = (l.out_point, format!("{} · bg", tag(lid)));
        s.edit("Layer Timing", None, |proj, _| {
            let bl = layer_mut(proj, cid, b)?;
            (bl.in_point, bl.out_point) = (tin, tout.max(tin));
            bl.comment = t;
            Ok(())
        })?;
        put_below(s, cid, b, lid)?;
        bg_layer = json!(b.0);
    }
    Ok(json!({"mask": mp, "name": name, "frames": fr, "bg": bg_layer}))
}

/// The source seconds a speed move's keys show (at 1× before `at`), and the source the picture needs.
fn remap_values(mv: &Mv, l: &Layer, fr: &[i64]) -> Result<(Vec<f64>, f64)> {
    let kind = mv.kind;
    let rk = mv.k.get("remap").ok_or_else(|| bad(C, format!("{kind}: no remap in the table")))?;
    let segs = arr(rk, "seg");
    if segs.len().saturating_add(1) != fr.len() {
        return Err(bad(C, format!("{kind}: the remap track has {} keys but {} segments", fr.len(), segs.len())));
    }
    let speed: Vec<f64> = arr(rk, "speed").iter().filter_map(Value::as_f64).collect();
    let mut vals = vec![l.layer_time(mv.fr.tick_of(mv.at_f)).seconds()];
    for (i, g) in segs.iter().enumerate() {
        let (Some(a), Some(b)) = (fr.get(i), fr.get(i + 1)) else { break };
        let dt = (b - a) as f64 / mv.fps;
        let v = vals.last().copied().unwrap_or(0.0);
        let step = if g.get("hold").and_then(Value::as_bool).unwrap_or(false) {
            0.0
        } else if g.get("ramp").and_then(Value::as_bool).unwrap_or(false) {
            let (Some(s0), Some(s1)) = (speed.get(i), speed.get(i + 1)) else { return Err(bad(C, format!("{kind}: a ramp needs a speed on both keys"))) };
            (s0 + s1) / 2.0 * dt
        } else {
            g.get("rate")
                .and_then(Value::as_f64)
                .filter(|r| r.is_finite())
                .ok_or_else(|| bad(C, format!("{kind}: a segment is {{rate}}, {{hold}} or {{ramp}}")))?
                * dt
        };
        if !step.is_finite() {
            return Err(bad(C, format!("{kind}: a segment's speed is out of range")));
        }
        vals.push(v + step);
    }
    let end_t = mv.fr.tick_of(fr.last().copied().unwrap_or(mv.at_f)).seconds();
    let last = vals.last().copied().unwrap_or(0.0);
    let vmax = vals.iter().copied().fold(f64::MIN, f64::max).max(last + (l.out_point.seconds() - end_t));
    Ok((vals, vmax))
}

/// speed: the shot's Time Remap: 1× up to `at`, the move's keys, 1× again to the out point.
fn remap(s: &mut Session, mv: &Mv, l: &Layer) -> Result<Value> {
    let kind = mv.kind;
    let rk = mv.k.get("remap").ok_or_else(|| bad(C, format!("{kind}: no remap in the table")))?;
    let fr = frames(mv.at_f, mv.fps, arr(rk, "t"), kind)?;
    let (vals, _) = remap_values(mv, l, &fr)?;
    let segs = arr(rk, "seg");
    let speed: Vec<f64> = arr(rk, "speed").iter().filter_map(Value::as_f64).collect();
    let inf = rk.get("influence").and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(33.333);
    let end = mv.fr.tick_of(fr.last().copied().unwrap_or(mv.at_f));
    let out_v = vals.last().copied().unwrap_or(0.0) + (l.out_point.seconds() - end.seconds());
    let at = mv.fr.tick_of(mv.at_f);
    let mut keys: Vec<Keyframe> = vec![];
    if l.in_point < at {
        let t = l.layer_time(l.in_point);
        keys.push(Keyframe::new(t, KV::Scalar(t.seconds())));
    }
    for (i, (f, v)) in fr.iter().zip(&vals).enumerate() {
        let mut k = Keyframe::new(l.layer_time(mv.fr.tick_of(*f)), KV::Scalar(*v));
        if segs.get(i).is_some_and(|g| g.get("hold").and_then(Value::as_bool).unwrap_or(false)) {
            k.out_interp = Interp::Hold;
        }
        if let Some(sp) = speed.get(i).filter(|x| x.is_finite()) {
            let e = Ease { speed: *sp, influence: (inf / 100.0).clamp(0.001, 1.0) };
            (k.in_interp, k.out_interp, k.auto_bezier) = (Interp::Bezier, if k.out_interp == Interp::Hold { Interp::Hold } else { Interp::Bezier }, false);
            (k.in_ease, k.out_ease) = (Eases::splat(e, 1), Eases::splat(e, 1));
        }
        keys.push(k);
    }
    if l.out_point.seconds() > end.seconds() + 0.5 / mv.fps {
        keys.push(Keyframe::new(l.layer_time(l.out_point), KV::Scalar(out_v)));
    }
    keys.sort_by_key(|k| k.time);
    keys.dedup_by_key(|k| k.time);
    let (cid, lid) = (mv.cid, mv.lid);
    run(s, cid, "layer.enableTimeRemap", json!({"layers": [lid.0], "value": true}))?;
    let first = vals.first().copied().unwrap_or(0.0);
    let mark = Marker { time: l.layer_time(at), comment: format!("{PREFIX}{kind}{REMAP_MARK}"), ..Default::default() };
    s.edit("Time Remap", None, |proj, _| {
        let layer = layer_mut(proj, cid, lid)?;
        let pr = layer.props.get_mut("timeRemap").ok_or_else(|| bad(C, "time remapping didn't turn on"))?;
        pr.value = KV::Scalar(first);
        pr.keys = keys;
        pr.expr = None;
        super::markers::place_marker(&mut layer.markers, mark);
        Ok(())
    })?;
    Ok(json!({"frames": fr, "values": vals, "out": out_v}))
}

/// Sound first: an audio-only copy of the shot as it is (no remap), right below it, over its whole
/// span; the shot's own audio goes off. Returns the copy.
fn keep_audio(s: &mut Session, cid: ItemId, lid: LayerId) -> Result<(LayerId, String)> {
    let t = format!("{}{AUDIO_KEEP}{}{MUTED}", tag(lid), lid.0);
    s.edit("Keep Audio", None, |proj, _| {
        let mut next = proj.next_id;
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let i = comp.layers.iter().position(|l| l.id == lid).ok_or_else(|| bad(C, "the layer is gone"))?;
        let mut l = comp.layer(lid).ok_or_else(|| bad(C, "the layer is gone"))?.clone();
        super::edit::reid(&mut l, &mut next);
        l.name = format!("{} audio (ClipMove keep)", l.name);
        l.comment = t;
        l.switches.video = false;
        l.switches.audio = true;
        l.track_matte = None;
        l.parent = None;
        let made = (l.id, l.name.clone());
        comp.layers.insert(i.saturating_add(1), l);
        if let Some(shot) = comp.layer_mut(lid) {
            shot.switches.audio = false;
        }
        proj.next_id = next;
        Ok(made)
    })
}

/// The layer: `layer`, else the one selected layer.
fn the_layer(s: &Session, cid: ItemId, p: &Value) -> Result<LayerId> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    if let Some(v) = p.get("layer").filter(|v| !v.is_null()) {
        return resolve_layer(comp, v).ok_or_else(|| bad(C, super::no_layer(v)));
    }
    let sel: Vec<LayerId> =
        if s.active_comp_id() == Some(cid) { s.state.selected_layers.iter().copied().filter(|id| comp.layer(*id).is_some()).collect() } else { vec![] };
    match sel[..] {
        [one] => Ok(one),
        [] => Err(bad(C, "select the layer to move (or pass `layer`)")),
        _ => Err(bad(C, format!("select one layer ({} selected)", sel.len()))),
    }
}

fn can_apply(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    the_layer(s, s.active_comp_id().ok_or("no composition is open")?, &json!({})).map(|_| ()).map_err(|e| e.to_string())
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let t = table()?;
    let default = t.get("default").and_then(Value::as_str).unwrap_or("punch");
    let kind = str_p(p, "kind").map(str::trim).filter(|k| !k.is_empty()).unwrap_or(default).to_string();
    let k = kind_spec(&kind)?;
    let cid = comp_id(s, p)?;
    let lid = the_layer(s, cid, p)?;
    if unlocked(s, cid, vec![lid], C)?.is_empty() {
        return Err(bad(C, "the layer is locked"));
    }
    let centre = match p.get("centre").filter(|v| !v.is_null()) {
        None => [960.0, 540.0],
        Some(v) => {
            pt(Some(v)).filter(|c| c[0].abs() <= 100_000.0 && c[1].abs() <= 100_000.0).ok_or_else(|| bad(C, "`centre` is [x, y] (px of a 1920×1080 shot)"))?
        }
    };
    let bg = bg_p(p)?;
    let id = str_p(p, "id").unwrap_or("clip-move").to_string();
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    if !(fps.is_finite() && fps > 0.0) {
        return Err(bad(C, "the comp has no frame rate"));
    }
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let at_f = match f_p(p, "at") {
        Some(a) if a.is_finite() && a.abs() <= 1e7 => frame(Tick::from_seconds_f64(a)),
        Some(_) => return Err(bad(C, "`at` is the move's first frame in comp seconds")),
        None if p.get("at").is_some_and(|v| !v.is_null()) => return Err(bad(C, "`at` is the move's first frame in comp seconds")),
        None => frame(s.time_of(cid)),
    };
    let l = comp.layer(lid).ok_or_else(|| bad(C, "no layer"))?.clone();
    let track =
        k.get("frame").or_else(|| k.get("mask")).or_else(|| k.get("remap")).ok_or_else(|| bad(C, format!("{kind}: the table has no frame, mask or remap")))?;
    let last_t = arr(track, "t").last().and_then(Value::as_f64).unwrap_or(0.0);
    let last_f = frame_of(at_f, fps, last_t, &kind)?;
    // Everything checked before the first change.
    if at_f < frame(l.in_point) || last_f > frame(l.out_point) {
        return Err(bad(
            C,
            format!(
                "{kind}: the move runs {}–{} s, but “{}” plays {}–{} s: it has to play over the whole move",
                fmt_s(fr.tick_of(at_f)),
                fmt_s(fr.tick_of(last_f)),
                l.name,
                fmt_s(l.in_point),
                fmt_s(l.out_point)
            ),
        ));
    }
    let prev = previous(s, cid, lid);
    let speed = k.get("remap").is_some();
    if speed {
        if !matches!(l.source, LayerSource::Footage { .. } | LayerSource::Comp { .. }) {
            return Err(bad(C, format!("{kind}: a speed move needs a footage or composition layer; “{}” is neither", l.name)));
        }
        if (l.stretch - 100.0).abs() > 1e-6 {
            return Err(bad(C, format!("{kind}: “{}” has a time stretch of {} %: a speed move needs it at 100 %", l.name, l.stretch)));
        }
        if l.props.get("timeRemap").is_some() && !prev.remap {
            return Err(bad(C, format!("{kind}: “{}” already has time remapping of its own: turn it off first (a speed move never edits it)", l.name)));
        }
    } else if l.props.sub(if k.get("frame").is_some() { "effects" } else { "masks" }).is_none() {
        return Err(bad(C, format!("{kind}: “{}” can't take {}", l.name, if k.get("frame").is_some() { "effects" } else { "masks" })));
    }
    let built = pt(k.get("builtCentre")).unwrap_or([960.0, 540.0]);
    let (profile, _) = Profile::load(s.config.as_deref());
    let mv = Mv { kind: &kind, k, cid, lid, fr, fps, at_f, shift: [centre[0] - built[0], centre[1] - built[1]], profile: &profile };
    if speed {
        let rk = k.get("remap").ok_or_else(|| bad(C, "no remap"))?;
        let fr_ = frames(at_f, fps, arr(rk, "t"), &kind)?;
        let (_, vmax) = remap_values(&mv, &l, &fr_)?;
        if let Some(d) = source_duration(s, &l)
            && d > Tick::ZERO
            && vmax > d.seconds() + 0.5 / fps
        {
            return Err(bad(
                C,
                format!(
                    "{kind}: the picture would need {:.3} s of “{}”'s footage, it has {} s: trim the layer's out point {:.3} s earlier",
                    vmax,
                    l.name,
                    fmt_s(d),
                    vmax - d.seconds()
                ),
            ));
        }
    }
    let title = k.get("title").and_then(Value::as_str).unwrap_or(&kind).to_string();
    let snap = s.project.clone();
    let (history, state) = (s.history.clone(), s.state.clone());
    let result = (|| -> Result<Value> {
        let removed = clean(s, cid, lid, &prev)?;
        let l = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or_else(|| bad(C, "the layer is gone"))?.clone();
        let mut layers = json!({"shot": lid.0});
        let mut moved: Vec<Value> = vec![];
        let f = |t: Tick| fr.frame_at(fr.snap_nearest(t));
        if speed {
            // Sound first: an audio-only copy of the shot as it is, then its picture is retimed.
            let mut audio = vec![];
            let copy = if has_audio(s, &l) { Some(keep_audio(s, cid, lid)?) } else { None };
            let rm = remap(s, &mv, &l)?;
            moved.push(json!({
                "layer": lid.0, "name": l.name, "what": "time-remap",
                "from": fr.tick_of(at_f).seconds(), "to": fr.tick_of(last_f).seconds(), "fromFrame": at_f, "toFrame": last_f,
                "picture": "time remapped (picture only)",
            }));
            if let Some((cp, name)) = copy {
                audio.push(cp.0);
                moved.push(json!({
                    "layer": cp.0, "name": name, "what": "audio-keep", "shot": lid.0, "of": lid.0,
                    "from": l.in_point.seconds(), "to": l.out_point.seconds(), "fromFrame": f(l.in_point), "toFrame": f(l.out_point), "shotAudio": "off",
                }));
            }
            layers["remap"] = rm;
            layers["audio"] = json!(audio);
        } else if k.get("frame").is_some() {
            layers["frame"] = framing(s, &mv, &l)?;
        } else {
            layers["crop"] = crop(s, &mv, &l, bg.clone())?;
        }
        // Layer markers on the shot (chain: one per beat).
        let mut marks: Vec<Marker> = vec![];
        for b in arr(k, "beats") {
            let Some(m) = b.get("marker").and_then(Value::as_str) else { continue };
            let mt = b.get("markerAt").or_else(|| b.get("t")).and_then(Value::as_f64).unwrap_or(0.0);
            let mf = frame_of(at_f, fps, mt, &kind)?;
            marks.push(Marker { time: l.layer_time(fr.tick_of(mf)), comment: format!("{PREFIX}{kind} {m}"), ..Default::default() });
        }
        if !marks.is_empty() {
            s.edit("Add Marker", None, |proj, _| {
                let layer = layer_mut(proj, cid, lid)?;
                for m in marks {
                    super::markers::place_marker(&mut layer.markers, m);
                }
                Ok(())
            })?;
        }
        let at_s = fr.tick_of(at_f).seconds();
        let mut hits = vec![];
        for b in arr(k, "beats") {
            let bt = b.get("t").and_then(Value::as_f64).unwrap_or(0.0);
            let bid = b.get("id").and_then(Value::as_str).unwrap_or("beat");
            hits.push(json!({
                "part": "clip-move",
                "id": format!("{id}-{bid}"),
                "t": ((at_s + bt) * 1000.0).round() / 1000.0,
                "frame": frame_of(at_f, fps, bt, &kind)?,
                "what": format!("{kind} · {}", b.get("what").and_then(Value::as_str).unwrap_or("")),
                "kind": b.get("kind").and_then(Value::as_str).unwrap_or("peak"),
            }));
        }
        Ok(json!({
            "kind": kind,
            "title": title,
            "comp": cid.0,
            "layer": lid.0,
            "at": at_s,
            "atFrame": at_f,
            "end": fr.tick_of(last_f).seconds(),
            "endFrame": last_f,
            "centre": centre,
            "layers": layers,
            "moved": moved,
            "hits": hits,
            "removed": {"effects": removed.effects, "masks": removed.masks, "markers": removed.markers, "layers": removed.layers, "items": removed.items},
        }))
    })();
    match result {
        Ok(r) => {
            super::batch::fold(s, &snap, &format!("Clip Move {title}"), &history);
            if s.active_comp_id() == Some(cid) {
                s.state.selected_layers = vec![lid];
            }
            Ok(r)
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

/// `clip.moveKinds`: the kinds in menu order, what they look like and are for.
fn list(_: &mut Session, _: &Value) -> Result<Value> {
    let t = table()?;
    let default = t.get("default").and_then(Value::as_str).unwrap_or("punch");
    let rows: Vec<Value> = kinds()
        .iter()
        .filter_map(|id| {
            let k = t.get("kinds")?.get(id)?;
            let move_ = if k.get("frame").is_some() {
                "frame"
            } else if k.get("mask").is_some() {
                "crop"
            } else {
                "speed"
            };
            Some(json!({"kind": id, "default": id == default, "title": k.get("title"), "energy": k.get("energy"), "move": move_, "looks": k.get("looks"), "use": k.get("use")}))
        })
        .collect();
    Ok(Value::Array(rows))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "clip.move",
            "Clip Move",
            ["Animation", "Clip Moves"],
            None,
            "{kind?: punch (default) | zoom-ladder | chain | crop-reframe | retime | speed-ramp | freeze (clip.moveKinds), comp?, layer? (default the selected layer), at? (comp s, the move's first frame; default the playhead), centre?: [x, y] (the subject, px of a 1920×1080 shot, default [960, 540]: the framings and the crop move with it, then clamp inside the frame; the speed kinds ignore it), bg?: #rrggbb | false (crop-reframe's backing, default #08080b; false: what is under the shot shows), id? (hit ids' prefix, default clip-move)} → {kind, title, comp, layer, at, atFrame, end, endFrame, centre, layers {shot, frame {effect, frames, position, scale} | crop {mask, name, frames, bg} | remap {frames, values, out}, audio}, moved: [{layer, name, what (time-remap | audio-keep), from, to, fromFrame, toFrame, shot / of, shotAudio}], hits: [{part, id, t, frame, what, kind (start | peak | cut | land)}], removed {effects, masks, markers, layers, items}}. The layer must play from `at` to the move's last key. The shot's transform, in / out points, parent and place in the stack are never touched: framing = a `ClipMove <kind> Frame` Transform effect (clamped: no frame edge shows), crop = a `ClipMove <kind> Crop` mask + a `ClipMove <kind> BG` solid right below it, speed = the shot's Time Remap (an error on a layer with its own time remapping or a time stretch, or when its footage is too short) with its sound kept at 1× on an audio-only copy `<name> audio (ClipMove keep)` (the shot's audio off). Layer markers `ClipMove <kind> …` (chain: one per beat; speed kinds: `· time remap`). Calling it again on the layer replaces the previous ClipMove (effects, masks, markers, BG and its solid, audio copy, time remap). One undo step; errors change nothing",
            can_apply,
            apply
        ),
        query!("clip.moveKinds", "Clip Move Kinds", "{}", list),
    ]
}
