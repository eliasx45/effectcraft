//! Now graphs (Elias, 29 Oct 2026, approved after the pause-graphs mock): while a comp is paused,
//! every keyframed property whose key segment touches the playhead (± a window, 1 s by default)
//! anywhere in the comp or in the precomps it shows, each with its speed graph for that segment,
//! so its ease can be changed right there without digging through layers.
//!
//! - **Which.** The comp's own layers and every precomp shown at the playhead ([`deep_comps`]:
//!   switched-off and guide precomp layers, time-remapped ones and a comp inside itself are not
//!   followed). In each, every rendering layer's properties with at least two keys and values
//!   that interpolate; per property the one segment nearest the playhead that touches the window
//!   and moves (its two values differ, its first key doesn't hold).
//! - **Times** are the searched comp's seconds, mapped up through each precomp layer's start and
//!   stretch ([`precomp_time`]).
//! - **Order** (Elias, 29 Oct: "can't reach the third graph"): segments that contain the playhead
//!   first, then the nearest; objects are numbered in that order (the first card's is 0).
//! - **Objects.** Each animated layer (through its precomp chain) is one object, numbered in the
//!   order listed: the frontend gives each its own colour and outlines its box on the viewer
//!   ([`object_quad`]: the layer's content box through every precomp layer, in the searched comp's
//!   pixels).
//! - **Edits** are commands (`nowGraphs.setInfluence`, `nowGraphs.applyEase`, `nowGraphs.moveKey`): see
//!   `commands/now_graphs.rs`.
//!
//! Nothing here touches the project.

use effectcraft_geom::vec2;
use effectcraft_keyframe::{Interp, Value as KValue};
use effectcraft_project::{ItemId, LayerId, Project, Uid};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::lint::deep_comps;
use crate::precomp_time::{self, Hop};
use crate::taste::{self, Side};

/// The window around the playhead a segment must touch (seconds), by default.
pub const WINDOW: f64 = 1.0;
/// Largest window asked for (seconds).
pub const MAX_WINDOW: f64 = 30.0;
/// Most graphs listed.
pub const MAX_GRAPHS: usize = 64;
/// Speed samples per graph (plus one: both ends).
pub const SAMPLES: usize = 48;

/// One property's segment around the playhead.
#[derive(Clone, Debug, PartialEq)]
pub struct NowGraph {
    /// The comp the layer is in (the searched comp, or a precomp inside it).
    pub comp: ItemId,
    pub comp_name: String,
    /// The precomp layers from the searched comp down to `comp`, outermost first.
    pub via: Vec<Hop>,
    /// Names from the searched comp down: comp, precomp layer…, layer, property.
    pub trail: Vec<String>,
    pub layer: LayerId,
    pub layer_name: String,
    /// The layer in the searched comp it shows through (itself, or the outermost precomp layer).
    pub top_layer: LayerId,
    pub prop: Uid,
    /// Match path (`transform/position`), as commands take it.
    pub path: String,
    pub property: String,
    /// The segment's first key (0-based); the segment is `key → key + 1`.
    pub key: usize,
    /// The segment in the searched comp's seconds.
    pub start: f64,
    pub end: f64,
    /// The two keys' times in their layer's seconds (as `keys.setEase` takes them).
    pub layer_times: (f64, f64),
    /// The two keys' times (`key`, then `key + 1`) in the searched comp's seconds: unlike
    /// `start`/`end` not sorted (a negative stretch puts `key` last).
    pub key_times: (f64, f64),
    /// Seconds from the playhead to the segment (0: it contains the playhead).
    pub gap: f64,
    /// The first key's out and the second key's in interpolation.
    pub interp: (Interp, Interp),
    /// Normalised eases (speed in units of the segment's average speed, influence %), when the
    /// value changes.
    pub ease: Option<(Side, Side)>,
    /// Speed over the segment, [`SAMPLES`] + 1 values, 0..1 of the fastest.
    pub speed: Vec<f64>,
    /// The fastest speed sampled (units per second of layer time).
    pub max_speed: f64,
    /// The object (animated layer) it belongs to: 0, 1, 2… in the order found.
    pub object: usize,
    /// The profile curve whose shape it has (id), if any.
    pub curve: Option<String>,
}

fn interp_id(i: Interp) -> &'static str {
    match i {
        Interp::Linear => "linear",
        Interp::Bezier => "bezier",
        Interp::Hold => "hold",
    }
}

fn r3(v: f64) -> f64 {
    if v.is_finite() { (v * 1000.0).round() / 1000.0 } else { 0.0 }
}

impl NowGraph {
    pub fn to_json(&self) -> Value {
        let side = |s: Side| json!({"speed": r3(s.speed), "influence": r3(s.influence)});
        let mut o = json!({
            "comp": self.comp.0, "compName": self.comp_name,
            "via": self.via.iter().map(|h| json!({"comp": h.comp.0, "layer": h.layer.0})).collect::<Vec<_>>(),
            "trail": self.trail,
            "layer": self.layer.0, "layerName": self.layer_name, "topLayer": self.top_layer.0,
            "prop": self.prop, "path": self.path, "property": self.property,
            "key": self.key,
            "start": r3(self.start), "end": r3(self.end),
            "layerTimes": [r3(self.layer_times.0), r3(self.layer_times.1)],
            "keyTimes": [r3(self.key_times.0), r3(self.key_times.1)],
            "containsPlayhead": self.gap <= 1e-9,
            "interp": {"out": interp_id(self.interp.0), "in": interp_id(self.interp.1)},
            "speed": self.speed.iter().map(|v| r3(*v)).collect::<Vec<_>>(),
            "maxSpeed": r3(self.max_speed),
            "object": self.object,
        });
        if let Some((out, inn)) = self.ease {
            o["ease"] = json!({"out": side(out), "in": side(inn)});
        }
        if let Some(c) = &self.curve {
            o["curve"] = json!(c);
        }
        o
    }
}

/// An object: one animated layer through its precomp chain.
#[derive(Clone, Debug, PartialEq)]
pub struct NowObject {
    pub comp: ItemId,
    pub via: Vec<Hop>,
    pub layer: LayerId,
    pub name: String,
}

/// What [`under_playhead`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NowGraphs {
    pub graphs: Vec<NowGraph>,
    pub objects: Vec<NowObject>,
    /// More graphs than [`MAX_GRAPHS`] were found.
    pub truncated: bool,
    /// Why some precomps were not searched.
    pub skipped: Vec<String>,
}

/// Every keyframed segment touching `t ± window` (`cid`'s seconds) in `cid` and its precomps (see
/// the module doc), objects in the order found, at most [`MAX_GRAPHS`].
pub fn under_playhead(project: &Project, cid: ItemId, t: f64, window: f64, profile: Option<&taste::Profile>) -> NowGraphs {
    let mut out = NowGraphs::default();
    if project.comp(cid).is_none() || !t.is_finite() {
        return out;
    }
    let w = if window.is_finite() { window.clamp(0.0, MAX_WINDOW) } else { WINDOW };
    let (lo, hi) = (t - w, t + w);
    let (comps, skipped) = deep_comps(project, cid, lo, hi);
    out.skipped = skipped;
    let top_name = project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    for dc in comps {
        let Some(comp) = project.comp(dc.comp) else { continue };
        let comp_name = project.item(dc.comp).map(|i| i.name.clone()).unwrap_or_default();
        // Names on the way down: the searched comp, then each precomp layer.
        let mut trail_head = vec![top_name.clone()];
        for h in &dc.via {
            let name = project.comp(h.comp).and_then(|c| c.layer(h.layer)).map(|l| l.name.clone()).unwrap_or_default();
            trail_head.push(name);
        }
        let top_layer_of = |l: LayerId| dc.via.first().map_or(l, |h| h.layer);
        for l in &comp.layers {
            if !crate::lint::moves_pixels(comp, l) {
                continue;
            }
            let mut props: Vec<(String, &effectcraft_project::Property)> = vec![];
            l.props.walk("", &mut |path, p| {
                if p.keys.len() >= 2 && p.value.interpolates() {
                    props.push((path.to_string(), p));
                }
            });
            for (_, p) in props {
                // The segment nearest the playhead that touches the window and moves.
                let mut best: Option<(f64, usize, f64, f64, (f64, f64))> = None;
                for i in 0..p.keys.len().saturating_sub(1) {
                    let (Some(a), Some(b)) = (p.keys.get(i), p.keys.get(i + 1)) else { continue };
                    if a.value == b.value || a.out_interp == Interp::Hold {
                        continue;
                    }
                    let up = |k: Tick| precomp_time::up(&dc.via, l.comp_time(k).seconds());
                    let (x, y) = (up(a.time), up(b.time));
                    let (s0, s1) = (x.min(y), x.max(y));
                    if !s0.is_finite() || !s1.is_finite() || s1 < lo - 1e-9 || s0 > hi + 1e-9 {
                        continue;
                    }
                    let gap = if t < s0 {
                        s0 - t
                    } else if t > s1 {
                        t - s1
                    } else {
                        0.0
                    };
                    if best.is_none_or(|b| gap < b.0) {
                        best = Some((gap, i, s0, s1, (x, y)));
                    }
                }
                let Some((gap, i, s0, s1, key_times)) = best else { continue };
                if out.graphs.len() >= MAX_GRAPHS {
                    out.truncated = true;
                    continue;
                }
                let object = match out.objects.iter().position(|o| o.comp == dc.comp && o.layer == l.id && o.via == dc.via) {
                    Some(k) => k,
                    None => {
                        out.objects.push(NowObject { comp: dc.comp, via: dc.via.clone(), layer: l.id, name: l.name.clone() });
                        out.objects.len() - 1
                    }
                };
                let (Some(a), Some(b)) = (p.keys.get(i), p.keys.get(i + 1)) else { continue };
                let (speed, max_speed) = speed_samples(&p.keys, i, p.spatial);
                let ease = taste::normalised(p, i).map(|(o, n, _)| (o, n));
                let curve = match (profile, ease) {
                    (Some(prof), Some(e)) if a.out_interp == Interp::Bezier && b.in_interp == Interp::Bezier => matching_curve(prof, e, (s1 - s0).abs()),
                    _ => None,
                };
                let path = crate::commands::match_path_of(&l.props, p.uid).unwrap_or_else(|| p.name.clone());
                let mut trail = trail_head.clone();
                trail.push(l.name.clone());
                trail.push(p.name.clone());
                out.graphs.push(NowGraph {
                    comp: dc.comp,
                    comp_name: comp_name.clone(),
                    via: dc.via.clone(),
                    trail,
                    layer: l.id,
                    layer_name: l.name.clone(),
                    top_layer: top_layer_of(l.id),
                    prop: p.uid,
                    path,
                    property: p.name.clone(),
                    key: i,
                    start: s0,
                    end: s1,
                    layer_times: (a.time.seconds(), b.time.seconds()),
                    key_times,
                    gap,
                    interp: (a.out_interp, b.in_interp),
                    ease,
                    speed,
                    max_speed,
                    object,
                    curve,
                });
            }
        }
    }
    order(&mut out);
    out
}

/// Segments containing the playhead first, then the nearest (found order breaks ties); objects
/// renumbered in the order their first graph is listed.
fn order(out: &mut NowGraphs) {
    out.graphs.sort_by(|a, b| a.gap.total_cmp(&b.gap));
    let mut map: Vec<Option<usize>> = vec![None; out.objects.len()];
    let mut objects = Vec::with_capacity(out.objects.len());
    for g in &mut out.graphs {
        let Some(slot) = map.get_mut(g.object) else { continue };
        let k = match *slot {
            Some(k) => k,
            None => {
                let Some(o) = out.objects.get(g.object) else { continue };
                objects.push(o.clone());
                *slot = Some(objects.len() - 1);
                objects.len() - 1
            }
        };
        g.object = k;
    }
    out.objects = objects;
}

/// The profile curve (approved or ⏳) whose shape a segment of `secs` seconds has, if any.
fn matching_curve(profile: &taste::Profile, e: (Side, Side), secs: f64) -> Option<String> {
    profile
        .curves
        .iter()
        .filter(|c| c.status != taste::Status::Rejected)
        .map(|c| (c, taste::distance(e, c.at(secs))))
        .filter(|(_, d)| d.is_finite() && *d <= profile.rules.match_within)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c.id.clone())
}

/// Speed over segment `i → i + 1`: [`SAMPLES`] + 1 values, 0..1 of the fastest, and the fastest.
/// Sampled just inside both keys (the speed at a key is the neighbouring segments' blend).
pub fn speed_samples(keys: &[effectcraft_keyframe::Keyframe], i: usize, spatial: bool) -> (Vec<f64>, f64) {
    let (Some(a), Some(b)) = (keys.get(i), keys.get(i + 1)) else { return (vec![], 0.0) };
    let (t0, t1) = (a.time.seconds(), b.time.seconds());
    let dt = t1 - t0;
    if !dt.is_finite() || dt.abs() < 1e-9 {
        return (vec![0.0; SAMPLES + 1], 0.0);
    }
    let raw: Vec<f64> = (0..=SAMPLES)
        .map(|k| {
            let x = (k as f64 / SAMPLES as f64).clamp(0.004, 0.996);
            let v = effectcraft_keyframe::speed(keys, Tick::from_seconds_f64(t0 + dt * x), spatial);
            if v.is_finite() { v.abs() } else { 0.0 }
        })
        .collect();
    let max = raw.iter().copied().fold(0.0, f64::max);
    if max <= 1e-12 {
        return (vec![0.0; SAMPLES + 1], 0.0);
    }
    (raw.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect(), max)
}

/// Object `o`'s content box at `t` (the searched comp `cid`'s seconds) in `cid`'s pixels: the
/// layer's box in its own comp ([`crate::viewer_handles::layer_box`]) through every precomp layer
/// on the way up. `None`: no bounds, not showing at `t`, a 3D box behind the camera.
pub fn object_quad(project: &Project, cid: ItemId, t: f64, o: &NowObject) -> Option<[[f64; 2]; 4]> {
    let inner = project.comp(o.comp)?;
    let ti = precomp_time::down(&o.via, t);
    if !ti.is_finite() {
        return None;
    }
    let ctx = EvalCtx::new(project, o.comp, inner, Tick::from_seconds_f64(ti));
    let l = inner.layer(o.layer)?;
    if !l.is_active_at(ctx.time) {
        return None;
    }
    let mut q = crate::viewer_handles::layer_box(&ctx, l)?.quad;
    if o.via.first().is_some_and(|h| h.comp != cid) {
        return None;
    }
    for (k, h) in o.via.iter().enumerate().rev() {
        let oc = project.comp(h.comp)?;
        let tc = precomp_time::down(o.via.get(..k)?, t);
        let ctx = EvalCtx::new(project, h.comp, oc, Tick::from_seconds_f64(tc));
        let pl = oc.layer(h.layer)?;
        let (m, _) = ctx.layer_to_comp(pl);
        q = q.map(|p| {
            let v = m.apply(vec2(p[0], p[1]));
            [v.x, v.y]
        });
    }
    q.iter().flatten().all(|v| v.is_finite()).then_some(q)
}

/// Whether quad `q` lies wholly outside a `w` × `h` frame.
pub fn off_frame(q: &[[f64; 2]; 4], w: f64, h: f64) -> bool {
    let (x0, x1) = q.iter().map(|p| p[0]).fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(x), b.max(x)));
    let (y0, y1) = q.iter().map(|p| p[1]).fold((f64::MAX, f64::MIN), |(a, b), y| (a.min(y), b.max(y)));
    x1 < 0.0 || y1 < 0.0 || x0 > w || y0 > h
}

/// The values a segment's keys hold (for a frontend's hover line).
pub fn values(project: &Project, g: &NowGraph) -> Option<(KValue, KValue)> {
    let p = project.comp(g.comp)?.layer(g.layer)?.props.find(g.prop)?;
    Some((p.keys.get(g.key)?.value.clone(), p.keys.get(g.key + 1)?.value.clone()))
}
