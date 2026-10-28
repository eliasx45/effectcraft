//! SFX1: the animation hits a sound effect lands on (`sfx.hits`), for Elias's SFX pass
//! (skill `editing-sfx`): the live map keeps Premiere's SFX clips on these hits and moves them
//! when an animation is retimed (Elias, 28 Oct 2026: "u would know where the peaks of the
//! animations are … syncing the peak of the keyframes of the main thing we need sfx for w the
//! peak of the sfx … like a live map").
//!
//! Read-only. The comp (default `MAIN`) and every precomp shown in it, at any depth, are walked
//! with lint's traversal ([`lint::deep_comps`]: times mapped through each precomp layer's start
//! and stretch, switched-off / guide / time-remapped precomp layers not followed). Each keyframe
//! segment's peak is lint's [`lint::segment_peak`] (the fastest point of the speed graph); a
//! linear segment has none, so its peak is its landing. Properties with an expression are
//! evaluated for real (the renderer's [`EvalCtx::value`] with the session's expression engine, once per frame of their comp) and
//! each stretch where the value changes is a segment. The hit kinds and thresholds are the
//! skill's `hits.py` (the offline reader of the .ecproj) so both agree:
//! - `appear` / `exit`: Opacity or Scale crossing 15 upward / downward; a picture layer cut in
//!   inside a precomp after its start, with no keyed hit (`source: inPoint`).
//! - `move`: Position ≥ 250 px, or Scale changing ≥ 25 %.
//! - `camera`: a null or camera layer (zoom / plane / CTRL rigs) moving ≥ 100 px or scaling ≥ 5 %.
//! - `run`: a Slider (Slider Control) ramping by ≥ 3 (counters, calendars: things expressions
//!   drive). Expression-driven moves inside a run's span fold into it (`driven`).
//! - `scene`: a precomp layer of the comp starting (also one starting inside a precomp).
//!
//! Skipped as in `hits.py`: switched-off layers (nulls kept: they drive others), guides, lights,
//! layers whose name contains `BG PLATE`, `CAM`, `BG CAM`, `no-bg`, `audio lead-in` or
//! `Presenter Zoom` (and everything inside such precomps), layer styles.
//!
//! Hits of one kind starting within 70 ms of each other are one hit (a stagger); its id is its
//! first member's.
//!
//! Ids are stable across retimes: `<via>/<layer>:<property uid path>:k<key index>` (`via` = the
//! precomp layer ids from the comp down, `>`-joined), `run:` + the slider's, `x<n>` for the n-th
//! changing stretch of an expression, `scene:` / `in:` + the layer's. `sfx.hitsRevision` is the
//! project revision, so a watcher re-reads only after a change.

use effectcraft_keyframe::{Interp, Value as KValue};
use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Project, PropGroup, Property};
use effectcraft_render::{EvalCtx, ExprHost};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, bad, comp_id};
use crate::lint::{self, DeepComp};
use crate::precomp_time::{self, Hop};
use crate::render_view::seconds_p;
use crate::{EngineError, Result, Session, query};

const C: &str = "sfx.hits";
/// Staggered elements this close (seconds, by start) are one hit (`hits.py`'s `group`).
pub const GROUP_WINDOW: f64 = 0.07;
/// Layer names `hits.py` skips (substring, case-sensitive): plates, camera plates, the presenter.
pub const SKIP_NAMES: [&str; 6] = ["BG PLATE", "CAM", "BG CAM", "no-bg", "audio lead-in", "Presenter Zoom"];
/// Opacity / Scale crossing this (percent) is an appear or an exit.
const VISIBLE: f64 = 15.0;
/// A Position move this long (px) is a `move`.
const MOVE_PX: f64 = 250.0;
/// A Scale change this big (percent) is a `move`.
const MOVE_PCT: f64 = 25.0;
/// A null / camera moving this far (px) or scaling this much (percent) is a `camera` hit.
const CAMERA_PX: f64 = 100.0;
const CAMERA_PCT: f64 = 5.0;
/// A slider ramping by this much is a `run`.
const RUN_MIN: f64 = 3.0;
/// Most raw hits collected (before grouping).
const MAX_EVENTS: usize = 20_000;
/// Most expression evaluations one call makes (then the rest is listed in `skipped`).
const MAX_EVALS: usize = 200_000;
/// A per-frame change smaller than this is no change (expression sampling).
const STILL: f64 = 1e-6;

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Appear,
    Move,
    Scene,
    Camera,
    Run,
    Exit,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Appear => "appear",
            Kind::Move => "move",
            Kind::Scene => "scene",
            Kind::Camera => "camera",
            Kind::Run => "run",
            Kind::Exit => "exit",
        }
    }
}

/// What a property is for the hit rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Opacity,
    Scale,
    Position,
    Slider,
}

/// One raw hit (before grouping). Times are the layer's comp seconds.
#[derive(Clone, Debug)]
struct Ev {
    id: String,
    kind: Kind,
    comp: ItemId,
    via: Vec<Hop>,
    layer: LayerId,
    layer_name: String,
    property: String,
    path: String,
    source: &'static str,
    start: f64,
    peak: f64,
    end: f64,
    /// `fastest` (speed-graph peak), `landing` (linear: no peak) or `cut` (a hold or an in point).
    peak_is: &'static str,
    px: Option<f64>,
    pct: Option<f64>,
    value: Option<f64>,
    speed: f64,
    /// Main-comp seconds of start / peak / end.
    main: (f64, f64, f64),
}

fn nums(v: &KValue) -> Option<Vec<f64>> {
    match v {
        KValue::Scalar(x) => Some(vec![*x]),
        KValue::Vec2(a) => Some(a.to_vec()),
        KValue::Vec3(a) => Some(a.to_vec()),
        _ => None,
    }
}

fn dist2(a: &[f64], b: &[f64]) -> f64 {
    let d = |i: usize| b.get(i).copied().unwrap_or(0.0) - a.get(i).copied().unwrap_or(0.0);
    (d(0).powi(2) + d(1).powi(2)).sqrt()
}

fn first(v: &[f64]) -> f64 {
    v.first().copied().unwrap_or(0.0)
}

/// The size of a change and the hit kind it makes (`hits.py`'s rules). `rig`: a null or camera.
fn classify(role: Role, rig: bool, v0: &[f64], v1: &[f64]) -> Option<(Kind, Option<f64>, Option<f64>, Option<f64>)> {
    if v0.len() != v1.len() || v0 == v1 {
        return None;
    }
    let (a, b) = (first(v0), first(v1));
    match role {
        Role::Slider => ((b - a).abs() >= RUN_MIN).then_some((Kind::Run, None, None, Some(b - a))),
        Role::Position if rig => {
            let d = dist2(v0, v1);
            (d >= CAMERA_PX).then_some((Kind::Camera, Some(d), None, None))
        }
        Role::Scale if rig => ((b - a).abs() >= CAMERA_PCT).then_some((Kind::Camera, None, Some(b - a), None)),
        _ if rig => None,
        Role::Opacity => {
            let k = if a < VISIBLE && b >= VISIBLE {
                Kind::Appear
            } else if b < VISIBLE && a >= VISIBLE {
                Kind::Exit
            } else {
                return None;
            };
            Some((k, None, None, Some(b - a)))
        }
        Role::Scale => {
            let k = if a < VISIBLE && b >= VISIBLE {
                Kind::Appear
            } else if b < VISIBLE && a >= VISIBLE {
                Kind::Exit
            } else if (b - a).abs() >= MOVE_PCT {
                Kind::Move
            } else {
                return None;
            };
            Some((k, None, Some(b - a), None))
        }
        Role::Position => {
            let d = dist2(v0, v1);
            (d >= MOVE_PX).then_some((Kind::Move, Some(d), None, None))
        }
    }
}

/// The properties the rules read, with their uid paths: Transform's Opacity / Scale / Position
/// (a camera's too), the layer's own Slider Controls, and the same match ids in content groups
/// and text animators (as `hits.py` reads them). Layer styles are left out.
fn props_of(l: &Layer) -> Vec<(String, &Property, Role)> {
    fn walk<'a>(g: &'a PropGroup, prefix: &str, depth: usize, out: &mut Vec<(String, &'a Property, Role)>) {
        if depth > 32 {
            return;
        }
        for c in &g.children {
            let path = format!("{prefix}@{}", c.uid());
            match c {
                effectcraft_project::Node::Prop(p) => {
                    let role = match p.match_id.as_str() {
                        "opacity" => Role::Opacity,
                        "scale" => Role::Scale,
                        "position" => Role::Position,
                        "slider" => Role::Slider,
                        _ => continue,
                    };
                    out.push((path, p, role));
                }
                effectcraft_project::Node::Group(sub) => {
                    if sub.match_id == effectcraft_project::styles::GROUP {
                        continue;
                    }
                    walk(sub, &format!("{path}/"), depth + 1, out);
                }
            }
        }
    }
    let mut out = vec![];
    walk(&l.props, "", 0, &mut out);
    out
}

fn skipped_name(name: &str) -> bool {
    SKIP_NAMES.iter().any(|s| name.contains(s))
}

fn rig(l: &Layer) -> bool {
    matches!(l.source, LayerSource::Null | LayerSource::Camera)
}

/// Whether `l`'s moves can make a hit (`hits.py`'s skips).
fn counts(comp: &Comp, l: &Layer) -> bool {
    if l.switches.guide || skipped_name(&l.name) || matches!(l.source, LayerSource::Light { .. }) {
        return false;
    }
    lint::moves_pixels(comp, l) || matches!(l.source, LayerSource::Null)
}

/// Whether a precomp chain passes through a layer `hits.py` skips (by name).
fn chain_skipped(project: &Project, via: &[Hop]) -> bool {
    via.iter().any(|h| project.comp(h.comp).and_then(|c| c.layer(h.layer)).is_some_and(|l| skipped_name(&l.name)))
}

fn via_id(via: &[Hop]) -> String {
    let mut s = String::new();
    for h in via {
        s.push_str(&h.layer.0.to_string());
        s.push('>');
    }
    s
}

struct Walk<'a> {
    project: &'a Project,
    expr: Option<&'a dyn ExprHost>,
    out: Vec<Ev>,
    evals: usize,
    skipped: Vec<String>,
}

impl Walk<'_> {
    fn push(&mut self, mut e: Ev, via: &[Hop]) {
        if self.out.len() >= MAX_EVENTS {
            return;
        }
        let up = |t: f64| precomp_time::up(via, t);
        let (a, b) = (up(e.start), up(e.end));
        e.main = (a.min(b), up(e.peak), a.max(b));
        self.out.push(e);
    }

    /// The keyframe segments of one property.
    #[allow(clippy::too_many_arguments)]
    fn keyed(&mut self, dc: &DeepComp, l: &Layer, path: &str, p: &Property, role: Role, base: &str) {
        let (lo, hi) = (l.in_point.seconds(), l.out_point.seconds());
        for (i, pair) in p.keys.windows(2).enumerate() {
            let [k0, k1] = pair else { continue };
            let (Some(v0), Some(v1)) = (nums(&k0.value), nums(&k1.value)) else { continue };
            let Some((kind, px, pct, value)) = classify(role, rig(l), &v0, &v1) else { continue };
            let (t0, t1) = (l.comp_time(k0.time).seconds(), l.comp_time(k1.time).seconds());
            let (start, end) = (t0.min(t1), t0.max(t1));
            let (peak, peak_is, speed) = if k0.out_interp == Interp::Hold {
                (end, "cut", 0.0)
            } else {
                match lint::segment_peak(&p.keys, i, p.spatial) {
                    Some((lt, top)) => (l.comp_time(Tick::from_seconds_f64(lt)).seconds(), "fastest", top),
                    None => {
                        let d = if p.spatial || v0.len() == 1 { dist2(&v0, &v1).max((first(&v1) - first(&v0)).abs()) } else { (first(&v1) - first(&v0)).abs() };
                        (end, "landing", d / (end - start).max(1e-9))
                    }
                }
            };
            // Shown at some point of the move, and its peak inside the window.
            if end < lo - 1e-9 || start > hi + 1e-9 || peak < dc.window.0 - 1e-6 || peak > dc.window.1 + 1e-6 {
                continue;
            }
            let id = format!("{}{base}:k{i}", if kind == Kind::Run { "run:" } else { "" });
            let e = Ev {
                id,
                kind,
                comp: dc.comp,
                via: dc.via.clone(),
                layer: l.id,
                layer_name: l.name.clone(),
                property: p.name.clone(),
                path: path.to_string(),
                source: "keys",
                start,
                peak,
                end,
                peak_is,
                px,
                pct,
                value,
                speed,
                main: (0.0, 0.0, 0.0),
            };
            self.push(e, &dc.via);
        }
    }

    /// An expression evaluated once per frame of its comp over the layer's shown part of the
    /// window; each stretch where the value changes is a segment.
    #[allow(clippy::too_many_arguments)]
    fn expression(&mut self, dc: &DeepComp, comp: &Comp, l: &Layer, path: &str, p: &Property, role: Role, base: &str) {
        let fps = comp.frame_rate.as_f64();
        if !fps.is_finite() || fps <= 0.0 {
            return;
        }
        let a = dc.window.0.max(l.in_point.seconds()).max(0.0);
        let b = dc.window.1.min(l.out_point.seconds()).min(comp.duration.seconds());
        if a.is_nan() || b.is_nan() || a > b {
            return;
        }
        let (f0, f1) = ((a * fps).floor() as i64, (b * fps).ceil() as i64);
        let n = usize::try_from(f1.saturating_sub(f0).saturating_add(1)).unwrap_or(0);
        if self.evals.saturating_add(n) > MAX_EVALS {
            let why = format!("expressions not evaluated past {MAX_EVALS} samples (narrow start/end)");
            if !self.skipped.contains(&why) {
                self.skipped.push(why);
            }
            return;
        }
        self.evals += n;
        let mut samples: Vec<(f64, Vec<f64>)> = Vec::with_capacity(n);
        for f in f0..=f1 {
            let t = f as f64 / fps;
            let ctx = EvalCtx { expr: self.expr, ..EvalCtx::new(self.project, dc.comp, comp, Tick::from_seconds_f64(t)) };
            let Some(v) = nums(&ctx.value(l, p)) else { return };
            samples.push((t, v));
        }
        let moved = |x: &[f64], y: &[f64]| x.len() != y.len() || x.iter().zip(y).any(|(p, q)| (p - q).abs() > STILL);
        let mut seg = 0usize;
        let mut i = 1usize;
        while i < samples.len() {
            let (Some(prev), Some(cur)) = (samples.get(i - 1), samples.get(i)) else { break };
            if !moved(&prev.1, &cur.1) {
                i += 1;
                continue;
            }
            // A changing stretch: from frame i-1 (still) to the last frame that still changes.
            let s0 = i - 1;
            let mut j = i;
            let (mut top, mut top_at) = (0.0f64, i);
            while let (Some(x), Some(y)) = (samples.get(j - 1), samples.get(j)) {
                if !moved(&x.1, &y.1) {
                    break;
                }
                let d = if role == Role::Position { dist2(&x.1, &y.1) } else { (first(&y.1) - first(&x.1)).abs() };
                if d > top {
                    (top, top_at) = (d, j);
                }
                j += 1;
            }
            let s1 = j - 1;
            i = j;
            let (Some(v0), Some(v1)) = (samples.get(s0), samples.get(s1)) else { break };
            let Some((kind, px, pct, value)) = classify(role, rig(l), &v0.1, &v1.1) else {
                seg += 1;
                continue;
            };
            // The fastest frame step: its middle.
            let peak = samples.get(top_at).map_or(v1.0, |s| s.0 - 0.5 / fps);
            let id = format!("{}{base}:x{seg}", if kind == Kind::Run { "run:" } else { "" });
            seg += 1;
            let e = Ev {
                id,
                kind,
                comp: dc.comp,
                via: dc.via.clone(),
                layer: l.id,
                layer_name: l.name.clone(),
                property: p.name.clone(),
                path: path.to_string(),
                source: "expression",
                start: v0.0,
                peak,
                end: v1.0,
                peak_is: "fastest",
                px,
                pct,
                value,
                speed: top * fps,
                main: (0.0, 0.0, 0.0),
            };
            self.push(e, &dc.via);
        }
    }

    fn comp(&mut self, dc: &DeepComp) {
        let Some(comp) = self.project.comp(dc.comp) else { return };
        if chain_skipped(self.project, &dc.via) {
            return;
        }
        let nested = !dc.via.is_empty();
        let inside = |t: f64| t >= dc.window.0 - 1e-6 && t <= dc.window.1 + 1e-6;
        for l in &comp.layers {
            if !counts(comp, l) {
                continue;
            }
            let base = format!("{}{}", via_id(&dc.via), l.id.0);
            let before = self.out.len();
            // Separated dimensions: Position reads as X/Y/Z Position, evaluated.
            let separated = l.transform().and_then(|tr| tr.get("positionX")).is_some();
            for (path, p, role) in props_of(l) {
                let pbase = format!("{base}:{path}");
                if p.has_expression() || (separated && role == Role::Position && is_transform_position(l, p)) {
                    self.expression(dc, comp, l, &path, p, role, &pbase);
                } else if p.keys.len() >= 2 {
                    self.keyed(dc, l, &path, p, role, &pbase);
                }
            }
            let keyed = self.out.len() > before;
            let t = l.in_point.seconds();
            let precomp = matches!(l.source, LayerSource::Comp { .. });
            // A precomp starting is a scene change (at the top: always; inside: as a cut-in).
            if precomp && !nested && inside(t) && l.in_point > Tick::ZERO {
                self.cut(dc, l, Kind::Scene, format!("scene:{base}"));
            } else if nested && !keyed && inside(t) && l.in_point > Tick::ZERO && renders_picture(l) {
                self.cut(dc, l, if precomp { Kind::Scene } else { Kind::Appear }, format!("in:{base}"));
            }
        }
    }

    fn cut(&mut self, dc: &DeepComp, l: &Layer, kind: Kind, id: String) {
        let t = l.in_point.seconds();
        let e = Ev {
            id,
            kind,
            comp: dc.comp,
            via: dc.via.clone(),
            layer: l.id,
            layer_name: l.name.clone(),
            property: "In Point".into(),
            path: String::new(),
            source: "inPoint",
            start: t,
            peak: t,
            end: t,
            peak_is: "cut",
            px: None,
            pct: None,
            value: None,
            speed: 0.0,
            main: (0.0, 0.0, 0.0),
        };
        self.push(e, &dc.via);
    }
}

/// Whether `p` is the layer's Transform ▸ Position.
fn is_transform_position(l: &Layer, p: &Property) -> bool {
    l.transform().and_then(|tr| tr.get("position")).is_some_and(|x| x.uid == p.uid)
}

/// Layers with a picture that can cut in (`hits.py`: text, shape, footage, solid, precomp).
fn renders_picture(l: &Layer) -> bool {
    l.switches.video
        && matches!(l.source, LayerSource::Text | LayerSource::Shape | LayerSource::Footage { .. } | LayerSource::Solid { .. } | LayerSource::Comp { .. })
}

/// A grouped hit: its members, first (earliest start) first.
struct Hit {
    members: Vec<Ev>,
    driven: usize,
}

/// Raw hits → hits: expression moves inside a run fold into it; then one kind within
/// [`GROUP_WINDOW`] of the previous member is the same hit (`hits.py`'s `group`).
fn group(mut evs: Vec<Ev>, frame: f64) -> Vec<Hit> {
    evs.sort_by(|a, b| a.main.0.total_cmp(&b.main.0).then_with(|| a.id.cmp(&b.id)));
    let runs: Vec<(f64, f64)> = evs.iter().filter(|e| e.kind == Kind::Run).map(|e| (e.main.0, e.main.2)).collect();
    let mut driven = vec![0usize; runs.len()];
    evs.retain(|e| {
        if e.kind == Kind::Run || e.source != "expression" {
            return true;
        }
        match runs.iter().position(|(a, b)| e.main.0 >= a - frame && e.main.2 <= b + frame) {
            Some(r) => {
                if let Some(d) = driven.get_mut(r) {
                    *d += 1;
                }
                false
            }
            None => true,
        }
    });
    let mut hits: Vec<Hit> = vec![];
    let mut run_i = 0usize;
    for e in evs {
        let is_run = e.kind == Kind::Run;
        let d = if is_run {
            let d = driven.get(run_i).copied().unwrap_or(0);
            run_i += 1;
            d
        } else {
            0
        };
        let open = hits
            .iter_mut()
            .rev()
            .take(6)
            .find(|h| h.members.first().is_some_and(|m| m.kind == e.kind) && h.members.last().is_some_and(|m| e.main.0 - m.main.0 <= GROUP_WINDOW + 1e-9));
        match open {
            Some(h) => {
                h.members.push(e);
                h.driven += d;
            }
            None => hits.push(Hit { members: vec![e], driven: d }),
        }
    }
    hits
}

/// Main comp: `comp`, else the comp named MAIN, else the active comp.
fn main_comp(s: &Session, p: &Value) -> Result<ItemId> {
    if p.get("comp").is_some_and(|v| !v.is_null()) {
        return comp_id(s, p);
    }
    let named: Vec<ItemId> = s.project.items.values().filter(|i| i.name == "MAIN" && i.as_comp().is_some()).map(|i| i.id).collect();
    if let [one] = named.as_slice() {
        return Ok(*one);
    }
    s.active_comp_id().ok_or_else(|| bad(C, "no comp named MAIN and no active comp: pass `comp`"))
}

fn times(comp_t: f64, main_t: f64, comp_fps: f64, main_fps: f64) -> Value {
    json!({"comp": r3(comp_t), "compFrame": r2(comp_t * comp_fps), "main": r3(main_t), "mainFrame": r2(main_t * main_fps)})
}

fn hits(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = main_comp(s, p)?;
    let project = &*s.project;
    let comp = project.comp(cid).ok_or(EngineError::NoComp)?;
    let fps = comp.frame_rate.as_f64();
    let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
    let a = seconds_p(p, "start", C)?.unwrap_or(0.0);
    let b = seconds_p(p, "end", C)?.unwrap_or(comp.duration.seconds().max(0.0));
    if b < a {
        return Err(bad(C, format!("empty range: end ({b} s) is before start ({a} s)")));
    }
    let (comps, skipped) = lint::deep_comps(project, cid, a, b);
    let mut w = Walk { project, expr: s.expr.as_deref(), out: vec![], evals: 0, skipped };
    for dc in &comps {
        w.comp(dc);
    }
    let truncated = w.out.len() >= MAX_EVENTS;
    let Walk { out, skipped, .. } = w;
    let grouped = group(out, 1.0 / fps);
    let map = comp.speech.as_deref().filter(|m| !m.words.is_empty());
    let stressed: Vec<(f64, &str)> = map
        .map(|m| m.stressed().into_iter().map(|(w, syl)| (lint::accent_time(syl, fps, lint::DEFAULT_ACCENT_LEAD), w.word.as_str())).collect())
        .unwrap_or_default();
    let name_of = |id: ItemId| project.item(id).map(|i| i.name.clone()).unwrap_or_default();
    let fps_of = |id: ItemId| project.comp(id).map(|c| c.frame_rate.as_f64()).filter(|f| f.is_finite() && *f > 0.0).unwrap_or(fps);
    let mut kinds = serde_json::Map::new();
    let list: Vec<Value> = grouped
        .iter()
        .filter_map(|h| {
            let lead = h.members.first()?;
            let start = h.members.iter().map(|m| m.main.0).fold(f64::INFINITY, f64::min);
            let end = h.members.iter().map(|m| m.main.2).fold(f64::NEG_INFINITY, f64::max);
            let cf = fps_of(lead.comp);
            let scene =
                lead.via.first().and_then(|hop| project.comp(hop.comp).and_then(|c| c.layer(hop.layer))).map_or(lead.layer_name.clone(), |l| l.name.clone());
            let max_of = |f: fn(&Ev) -> Option<f64>| h.members.iter().filter_map(f).max_by(|x, y| x.abs().total_cmp(&y.abs()));
            let mut size = serde_json::Map::new();
            if let Some(v) = max_of(|m| m.px) {
                size.insert("px".into(), json!(r2(v)));
            }
            if let Some(v) = max_of(|m| m.pct) {
                size.insert("scalePct".into(), json!(r2(v)));
            }
            if let Some(v) = max_of(|m| m.value) {
                size.insert("value".into(), json!(r2(v)));
            }
            let speed = h.members.iter().map(|m| m.speed).fold(0.0, f64::max);
            let mut layers: Vec<&str> = h.members.iter().map(|m| m.layer_name.as_str()).collect();
            layers.sort_unstable();
            layers.dedup();
            let mut o = json!({
                "id": lead.id,
                "kind": lead.kind.name(),
                "n": h.members.len(),
                "layer": lead.layer.0,
                "layerName": lead.layer_name,
                "layers": layers.iter().take(6).collect::<Vec<_>>(),
                "comp": lead.comp.0,
                "compName": name_of(lead.comp),
                "scene": scene,
                "via": lead.via.iter().map(|h| h.layer.0).collect::<Vec<_>>(),
                "property": lead.property,
                "path": lead.path,
                "source": lead.source,
                "start": times(precomp_time::down(&lead.via, start), start, cf, fps),
                "peak": times(lead.peak, lead.main.1, cf, fps),
                "end": times(precomp_time::down(&lead.via, end), end, cf, fps),
                "duration": r3(end - start),
                "peakIs": lead.peak_is,
                "size": Value::Object(size),
                "speed": r2(speed),
            });
            if h.members.len() > 1 {
                o["members"] = json!(h.members.iter().map(|m| json!({"id": m.id, "layerName": m.layer_name, "peak": r3(m.main.1)})).collect::<Vec<_>>());
            }
            if h.driven > 0 {
                o["driven"] = json!(h.driven);
            }
            let pk = lead.main.1;
            if let Some(m) = map {
                let near = m.words.iter().min_by(|x, y| gap(x, pk).total_cmp(&gap(y, pk)));
                if let Some(wd) = near {
                    o["word"] = json!({"word": wd.word, "start": r3(wd.start.seconds()), "end": r3(wd.end.seconds()), "distance": r3(gap(wd, pk))});
                }
                let ns = stressed.iter().min_by(|x, y| (x.0 - pk).abs().total_cmp(&(y.0 - pk).abs()));
                if let Some((t, word)) = ns {
                    o["stress"] = json!({"word": word, "accent": r3(*t), "accentFrame": (t * fps).round(), "offsetFrames": r2((pk - t) * fps)});
                }
            }
            let k = kinds.entry(lead.kind.name().to_string()).or_insert(json!(0));
            *k = json!(k.as_u64().unwrap_or(0) + 1);
            Some(o)
        })
        .collect();
    Ok(json!({
        "comp": cid.0,
        "compName": name_of(cid),
        "fps": fps,
        "revision": s.revision,
        "range": [r3(a), r3(b)],
        "count": list.len(),
        "kinds": Value::Object(kinds),
        "speech": map.is_some(),
        "truncated": truncated,
        "skipped": skipped,
        "hits": list,
    }))
}

/// Seconds from `t` to a word (0 inside it).
fn gap(w: &effectcraft_project::speech::SpeechWord, t: f64) -> f64 {
    let (a, b) = (w.start.seconds(), w.end.seconds());
    if t < a {
        a - t
    } else if t > b {
        t - b
    } else {
        0.0
    }
}

fn revision(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({"revision": s.revision}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "sfx.hits",
            "SFX Hits",
            "{comp? (default: the comp named MAIN, else the active comp), start? (s), end? (s)} → {comp, compName, fps, revision, range, count, kinds, speech, truncated, skipped, hits: [{id (stable across retimes: <via>layer:<property uid path>:k<key index>, run:…, …:x<n> for an expression's n-th change, scene:/in: + layer), kind: appear|move|scene|camera|run|exit, n (staggered members within 70 ms), members?: [{id, layerName, peak}], layer, layerName, layers, comp (the layer's comp), compName, scene (the top-level layer it is in), via (precomp layer ids, outermost first), property, path, source: keys|expression|inPoint, start|peak|end: {comp, compFrame, main, mainFrame} (peak = fastest point of the speed graph, or the landing of a linear move), duration, peakIs: fastest|landing|cut, size: {px?, scalePct?, value?}, speed, driven? (expression moves folded into a run), word? {word, start, end, distance}, stress? {word, accent, accentFrame, offsetFrames}}]}. Read-only: every nested precomp, nulls / camera rigs, slider runs and expressions (evaluated per frame) included; skips switched-off layers (not nulls), guides, BG plates and the presenter like the editing-sfx skill's hits.py",
            hits
        ),
        query!("sfx.hitsRevision", "SFX Hits Revision", "{} → {revision}: the project revision; re-read sfx.hits when it changes", revision),
    ]
}
