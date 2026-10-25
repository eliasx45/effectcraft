//! Motion summary: how layers move over a time range, in numbers and in words (`comp.motion` /
//! `layer.motion`, MCP `get_motion`). Agents can't watch a video; this samples each layer's
//! content rectangle in comp space ([`layout::layer_geometry`]: transform, parents, 3D camera),
//! its opacity, scale and rotation over time (no pixels are rendered) and turns the samples into
//! segments, off-screen intervals, overlap events and one plain-English sentence per layer.
//!
//! Sampling: a grid from `start` to `end` every `step`, plus, per layer, its in and out points
//! and the comp times of every Transform keyframe of the layer and its parents inside the range,
//! so segment boundaries land on keys. A layer counts as on from its in point to its out point
//! (both included, so a fade ending on the out point ends there). The grid is cut short (and
//! `truncated` set) when layers × samples would pass [`MAX_TOTAL_SAMPLES`].
//!
//! Segments (`kind`, `start`, `end`, `from`, `to`):
//! - `move`: the content centre travels at least [`MOVE_MIN`] of the comp diagonal; `from` / `to`
//!   are centres in comp pixels and `direction` the 8-way direction of the travel (`left`,
//!   `up-right`…; y grows downwards). A path that turns by more than [`TURN_DEG`] starts a new
//!   segment.
//! - `enter` / `exit`: a move during which the on-screen share of the layer rises (falls) by at
//!   least [`EDGE_GAIN`]; `edge` is the frame edge it comes in from (leaves by). A move that
//!   does both (flies across) is split into enter, move and exit where it is most on screen. A
//!   layer that just appears on screen at its in point is not an `enter`.
//! - `scale` (average of X and Y scale, %), `rotate` (Z rotation, degrees), `fade-in` /
//!   `fade-out` (opacity 0–100): the layer's own Transform values changing by at least
//!   [`SCALE_MIN`] / [`ROTATE_MIN`] / [`FADE_MIN`]. Scale and rotation inherited from a parent
//!   show up as moves of the content box.
//! - `hold`: no kept segment of any kind for at least [`MIN_HOLD`] s (and three samples).
//!   Changes below the thresholds count as holding.
//!
//! Kinds may overlap in time (a layer that moves while it fades has both segments).
//! `offscreen` lists the intervals where none of the content box is inside the frame while the
//! layer is on. `overlaps` lists pairs of drawn layers whose on-screen boxes overlap by more
//! than [`OVERLAP_MIN`] of the smaller one, sampled on the grid; layers covering the whole frame
//! (backgrounds) are left out. Cameras, lights, nulls, hidden (Video off) and content-less layers
//! are listed with no segments and a summary saying why, unless other layers are parented to
//! them: such a rig (a null carrying the keys) is described by what it moves, its segments those
//! of the child it moves the most (`describedBy`), its children in `moves`.
//!
//! A layer whose moves (move / enter / exit) come at least partly from animated parents lists
//! them in `movedBy` (nearest first), and its summary says which layer to key.

use effectcraft_project::{Layer, LayerId};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::layout;

/// Most samples taken over all layers (layers × grid samples); the range is cut short beyond.
pub const MAX_TOTAL_SAMPLES: usize = 20_000;
/// Most layers described in one call.
pub const MAX_LAYERS: usize = 200;
/// Most keyframe times added to one layer's samples.
pub const MAX_KEY_TIMES: usize = 512;
/// Most segments reported per layer.
pub const MAX_SEGMENTS: usize = 200;
/// Most layers whose pairwise overlaps are tracked.
pub const MAX_OVERLAP_LAYERS: usize = 64;
/// Most overlap events reported.
pub const MAX_OVERLAPS: usize = 200;
/// Default grid: at most this many samples per layer (else one frame).
pub const DEFAULT_SAMPLES: i64 = 600;
/// A move travels at least this share of the comp diagonal.
pub const MOVE_MIN: f64 = 0.005;
/// A scale change of at least this many percent.
pub const SCALE_MIN: f64 = 1.0;
/// A rotation of at least this many degrees.
pub const ROTATE_MIN: f64 = 1.0;
/// An opacity change of at least this many percent.
pub const FADE_MIN: f64 = 1.0;
/// Shortest hold, seconds.
pub const MIN_HOLD: f64 = 0.25;
/// A move whose direction turns by more than this many degrees starts a new segment.
pub const TURN_DEG: f64 = 50.0;
/// On-screen share a move must gain (lose) to be an enter (exit).
pub const EDGE_GAIN: f64 = 0.25;
/// Overlap share of the smaller layer that counts as an overlap.
pub const OVERLAP_MIN: f64 = 0.05;
/// Share of the frame a layer must cover to count as a full-frame background.
pub const FULL_FRAME: f64 = 0.99;

/// The default sampling step for a span: one frame, or coarser so a layer gets at most
/// [`DEFAULT_SAMPLES`] samples.
pub fn default_step(span: Tick, frame: Tick) -> Tick {
    frame.max(Tick(span.0 / DEFAULT_SAMPLES)).max(Tick(1))
}

#[derive(Clone, Debug)]
struct Geo {
    center: [f64; 2],
    corners: [[f64; 2]; 4],
    bounds: [f64; 4],
    /// The part of the content box inside the frame, its area and the box's on-screen share.
    clip: Vec<[f64; 2]>,
    clip_area: f64,
    on_screen: f64,
}

#[derive(Clone, Debug)]
struct Sample {
    t: Tick,
    /// Between the in and out points (both included).
    on: bool,
    /// Drawn by the compositor ([`layout::visibility`]).
    drawn: bool,
    geo: Option<Geo>,
    opacity: f64,
    scale: f64,
    rotation: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Enter,
    Exit,
    Move,
    Hold,
    Scale,
    Rotate,
    FadeIn,
    FadeOut,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Enter => "enter",
            Kind::Exit => "exit",
            Kind::Move => "move",
            Kind::Hold => "hold",
            Kind::Scale => "scale",
            Kind::Rotate => "rotate",
            Kind::FadeIn => "fade-in",
            Kind::FadeOut => "fade-out",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Val {
    Pt([f64; 2]),
    Num(f64),
}

/// A segment between samples `a` and `b` (indices into the layer's samples).
#[derive(Clone, Debug)]
struct Seg {
    kind: Kind,
    a: usize,
    b: usize,
    from: Val,
    to: Val,
    direction: Option<&'static str>,
    edge: Option<&'static str>,
}

fn finite(v: f64, d: f64) -> f64 {
    if v.is_finite() { v } else { d }
}
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
fn r4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}
fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

/// The 8-way direction of a comp-space displacement (y down).
fn direction(d: [f64; 2]) -> &'static str {
    let a = (-d[1]).atan2(d[0]).to_degrees(); // counter-clockwise from +x, y up
    const DIRS: [&str; 8] = ["right", "up-right", "up", "up-left", "left", "down-left", "down", "down-right"];
    let i = (((a + 360.0 + 22.5) % 360.0) / 45.0).floor();
    let i = if i.is_finite() && i >= 0.0 { i as usize % 8 } else { 0 };
    DIRS.get(i).copied().unwrap_or("right")
}

/// The frame edge a layer with comp bounds `b` came in from (`entering`) or leaves by, given its
/// travel `d`: the edge it overshoots most in the direction it comes from / goes to.
fn edge(b: [f64; 4], d: [f64; 2], w: f64, h: f64, entering: bool) -> &'static str {
    let v = if entering { [-d[0], -d[1]] } else { d };
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    let (w, h) = (w.max(1.0), h.max(1.0));
    let over = [(-b[0]).max(0.0) / w, (b[0] + b[2] - w).max(0.0) / w, (-b[1]).max(0.0) / h, (b[1] + b[3] - h).max(0.0) / h];
    let pull = if len > 1e-12 { [(-v[0]).max(0.0) / len, v[0].max(0.0) / len, (-v[1]).max(0.0) / len, v[1].max(0.0) / len] } else { [0.0; 4] };
    const EDGES: [&str; 4] = ["left", "right", "top", "bottom"];
    let pick = |score: [f64; 4]| {
        let (i, best) = score.iter().enumerate().fold((0, 0.0), |acc, (i, &s)| if s > acc.1 { (i, s) } else { acc });
        (best > 1e-12).then(|| EDGES.get(i).copied().unwrap_or("left"))
    };
    pick([over[0] * pull[0], over[1] * pull[1], over[2] * pull[2], over[3] * pull[3]]).or_else(|| pick(pull)).or_else(|| pick(over)).unwrap_or("left")
}

/// The comp times worth sampling exactly for `l`: its in and out points and the keyframes of
/// its Transform group and its parents' (depth-limited), inside `(start, end)`.
fn key_times(ctx: &EvalCtx, l: &Layer, start: Tick, end: Tick) -> Vec<Tick> {
    let mut out = vec![l.in_point, l.out_point];
    let mut cur = Some(l);
    let mut depth = 0;
    while let Some(layer) = cur {
        if let Some(tr) = layer.transform() {
            tr.walk("", &mut |_, p| {
                for k in &p.keys {
                    if out.len() < MAX_KEY_TIMES {
                        out.push(layer.comp_time(k.time));
                    }
                }
            });
        }
        depth += 1;
        cur = if depth < 64 { layer.parent.and_then(|p| ctx.comp.layer(p)) } else { None };
    }
    out.retain(|t| *t > start && *t < end);
    out
}

fn sample(ctx: &EvalCtx, l: &Layer, t: Tick, w: f64, h: f64) -> Sample {
    let c = ctx.at(t);
    let on = t >= l.in_point && t <= l.out_point;
    let drawn = on && layout::visibility(&c, l).0;
    let frame = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
    let geo = if on {
        layout::layer_geometry(&c, l).filter(|g| g.projected && g.center.iter().chain(g.bounds.iter()).all(|v| v.is_finite())).map(|g| {
            let area = layout::polygon_area(&g.corners);
            let clip = layout::clip_convex(&g.corners, &frame);
            let clip_area = layout::polygon_area(&clip);
            let on_screen = if area > 1e-9 { (clip_area / area).clamp(0.0, 1.0) } else { 0.0 };
            Geo { center: g.center, corners: g.corners, bounds: g.bounds, clip, clip_area, on_screen }
        })
    } else {
        None
    };
    let tr = l.transform();
    let scale = tr.map(|tr| c.v3(l, tr, "scale", [100.0; 3])).map_or(100.0, |s| (s[0] + s[1]) / 2.0);
    let rotation = tr.map_or(0.0, |tr| c.f(l, tr, "rotation", 0.0));
    Sample { t, on, drawn, geo, opacity: finite(c.opacity(l) * 100.0, 100.0), scale: finite(scale, 100.0), rotation: finite(rotation, 0.0) }
}

/// Whether the interval from sample `i` to `i + 1` lies inside the layer's on time.
fn on_pair(s: &[Sample], i: usize) -> bool {
    s.get(i).is_some_and(|x| x.on) && s.get(i + 1).is_some_and(|x| x.on)
}

/// Runs of one scalar channel changing in one direction, kept when they change by `min`:
/// (first sample, last sample, rising).
fn scalar_runs(s: &[Sample], val: fn(&Sample) -> f64, min: f64) -> Vec<(usize, usize, bool)> {
    let mut runs = vec![];
    let mut cur: Option<(usize, bool)> = None;
    for i in 0..s.len().saturating_sub(1) {
        let label = match (on_pair(s, i), s.get(i), s.get(i + 1)) {
            (true, Some(x), Some(y)) => {
                let d = val(y) - val(x);
                (d.abs() > 1e-6).then_some(d > 0.0)
            }
            _ => None,
        };
        if let Some((a, up)) = cur
            && label != Some(up)
        {
            runs.push((a, i, up));
            cur = None;
        }
        if cur.is_none()
            && let Some(up) = label
        {
            cur = Some((i, up));
        }
    }
    if let Some((a, up)) = cur {
        runs.push((a, s.len().saturating_sub(1), up));
    }
    let change = |a: usize, b: usize| match (s.get(a), s.get(b)) {
        (Some(x), Some(y)) => (val(y) - val(x)).abs(),
        _ => 0.0,
    };
    runs.retain(|&(a, b, _)| change(a, b) >= min);
    runs
}

fn center(s: &[Sample], i: usize) -> Option<[f64; 2]> {
    s.get(i).and_then(|x| x.geo.as_ref()).map(|g| g.center)
}

/// Runs of the content centre travelling (split where the path turns by more than
/// [`TURN_DEG`]), kept when they travel `min` pixels: (first sample, last sample).
fn move_runs(s: &[Sample], min: f64) -> Vec<(usize, usize)> {
    let cos_turn = TURN_DEG.to_radians().cos();
    let mut runs = vec![];
    let mut cur: Option<usize> = None;
    for i in 0..s.len().saturating_sub(1) {
        let d = match (on_pair(s, i), center(s, i), center(s, i + 1)) {
            (true, Some(p), Some(q)) => Some([q[0] - p[0], q[1] - p[1]]).filter(|d| d[0].hypot(d[1]) > 1e-6),
            _ => None,
        };
        match (cur, d) {
            (Some(a), Some(d)) => {
                // Turned too far from the way the run has been going: start a new run here.
                if let (Some(p0), Some(p)) = (center(s, a), center(s, i)) {
                    let net = [p[0] - p0[0], p[1] - p0[1]];
                    let (ln, ld) = (net[0].hypot(net[1]), d[0].hypot(d[1]));
                    if ln > 1e-6 && (net[0] * d[0] + net[1] * d[1]) / (ln * ld) < cos_turn {
                        runs.push((a, i));
                        cur = Some(i);
                    }
                }
            }
            (Some(a), None) => {
                runs.push((a, i));
                cur = None;
            }
            (None, Some(_)) => cur = Some(i),
            (None, None) => {}
        }
    }
    if let Some(a) = cur {
        runs.push((a, s.len().saturating_sub(1)));
    }
    runs.retain(|&(a, b)| matches!((center(s, a), center(s, b)), (Some(p), Some(q)) if dist(p, q) >= min));
    // Touching runs that end up going the same way are one move.
    let mut merged: Vec<(usize, usize)> = vec![];
    for (a, b) in runs {
        let dir = |a: usize, b: usize| match (center(s, a), center(s, b)) {
            (Some(p), Some(q)) => Some(direction([q[0] - p[0], q[1] - p[1]])),
            _ => None,
        };
        if let Some(last) = merged.last_mut()
            && last.1 == a
            && dir(last.0, last.1) == dir(a, b)
        {
            last.1 = b;
        } else {
            merged.push((a, b));
        }
    }
    merged
}

/// A move run split into its enter, move and exit parts.
fn move_segments(s: &[Sample], a: usize, b: usize, min: f64, w: f64, h: f64) -> Vec<Seg> {
    let frac = |i: usize| s.get(i).and_then(|x| x.geo.as_ref()).map_or(0.0, |g| g.on_screen);
    let max = (a..=b).map(frac).fold(0.0, f64::max);
    let first_max = (a..=b).find(|&i| frac(i) >= max - 1e-3).unwrap_or(a);
    let last_max = (a..=b).rev().find(|&i| frac(i) >= max - 1e-3).unwrap_or(b);
    let mut out = vec![];
    let mut seg = |kind: Kind, x: usize, y: usize| {
        let (Some(p), Some(q)) = (center(s, x), center(s, y)) else { return };
        if y <= x || (kind == Kind::Move && dist(p, q) < min) {
            return;
        }
        let d = [q[0] - p[0], q[1] - p[1]];
        let bounds = |i: usize| s.get(i).and_then(|x| x.geo.as_ref()).map_or([0.0; 4], |g| g.bounds);
        let edge = match kind {
            Kind::Enter => Some(edge(bounds(x), d, w, h, true)),
            Kind::Exit => Some(edge(bounds(y), d, w, h, false)),
            _ => None,
        };
        out.push(Seg { kind, a: x, b: y, from: Val::Pt(p), to: Val::Pt(q), direction: Some(direction(d)), edge });
    };
    // A move that brings the layer on screen is an enter as a whole, one that takes it off an
    // exit; one that does both (flies across) is split where it is most on screen.
    let enters = max - frac(a) >= EDGE_GAIN;
    let exits = max - frac(b) >= EDGE_GAIN;
    match (enters, exits) {
        (true, false) => seg(Kind::Enter, a, b),
        (false, true) => seg(Kind::Exit, a, b),
        (true, true) => {
            seg(Kind::Enter, a, first_max);
            seg(Kind::Move, first_max, last_max);
            seg(Kind::Exit, last_max.max(first_max), b);
        }
        (false, false) => seg(Kind::Move, a, b),
    }
    out
}

/// Every segment of a layer's samples, sorted by start time.
fn segments(s: &[Sample], step: Tick, diag: f64, w: f64, h: f64) -> Vec<Seg> {
    let min_move = MOVE_MIN * diag;
    let mut segs = vec![];
    for (a, b) in move_runs(s, min_move) {
        segs.extend(move_segments(s, a, b, min_move, w, h));
    }
    let scalar: [(fn(&Sample) -> f64, f64, Kind, Kind); 3] = [
        (|x| x.scale, SCALE_MIN, Kind::Scale, Kind::Scale),
        (|x| x.rotation, ROTATE_MIN, Kind::Rotate, Kind::Rotate),
        (|x| x.opacity, FADE_MIN, Kind::FadeIn, Kind::FadeOut),
    ];
    for (val, min, up, down) in scalar {
        for (a, b, rising) in scalar_runs(s, val, min) {
            let (Some(x), Some(y)) = (s.get(a), s.get(b)) else { continue };
            segs.push(Seg { kind: if rising { up } else { down }, a, b, from: Val::Num(val(x)), to: Val::Num(val(y)), direction: None, edge: None });
        }
    }
    // Holds: on-time stretches no other segment covers.
    let covered = |i: usize| segs.iter().any(|g| g.a <= i && i < g.b);
    let min_hold = MIN_HOLD.max(2.0 * step.seconds());
    let mut holds = vec![];
    let mut cur: Option<usize> = None;
    for i in 0..s.len() {
        let open = i + 1 < s.len() && on_pair(s, i) && !covered(i);
        match (cur, open) {
            (None, true) => cur = Some(i),
            (Some(a), false) => {
                holds.push((a, i));
                cur = None;
            }
            _ => {}
        }
    }
    for (a, b) in holds {
        let (Some(x), Some(y)) = (s.get(a), s.get(b)) else { continue };
        if (y.t - x.t).seconds() + 1e-9 < min_hold {
            continue;
        }
        let at = x.geo.as_ref().map_or(Val::Num(x.opacity), |g| Val::Pt(g.center));
        let to = y.geo.as_ref().map_or(Val::Num(y.opacity), |g| Val::Pt(g.center));
        segs.push(Seg { kind: Kind::Hold, a, b, from: at, to, direction: None, edge: None });
    }
    segs.sort_by(|p, q| p.a.cmp(&q.a).then(p.kind.cmp(&q.kind)));
    segs
}

/// Intervals (sample index ranges) where the layer is on but entirely outside the frame, as
/// comp-time (start, end) pairs.
fn offscreen(s: &[Sample], l: &Layer) -> Vec<(Tick, Tick)> {
    let off = |x: &Sample| x.on && x.geo.as_ref().is_some_and(|g| g.on_screen <= 1e-6);
    let mut out = vec![];
    let mut i = 0;
    while i < s.len() {
        if !s.get(i).is_some_and(off) {
            i += 1;
            continue;
        }
        let a = i;
        while i < s.len() && s.get(i).is_some_and(off) {
            i += 1;
        }
        let start = s.get(a).map_or(Tick::ZERO, |x| x.t);
        let end = match (s.get(i), s.get(i.saturating_sub(1))) {
            (Some(next), _) if next.on => next.t,
            (Some(next), _) => next.t.min(l.out_point),
            (None, Some(last)) => last.t,
            _ => start,
        };
        if end > start {
            out.push((start, end));
        }
    }
    out
}

/// "2.5" for 2.5 s: at most two decimals, no trailing zeros.
pub fn fmt_secs(t: f64) -> String {
    let s = format!("{:.2}", r2(t));
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}
fn fmt_range(a: f64, b: f64) -> String {
    format!("{}–{}s", fmt_secs(a), fmt_secs(b))
}
fn fmt_num(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if (r - r.round()).abs() < 1e-9 { format!("{}", r.round()) } else { format!("{r:.1}") }
}

/// The plain-English sentence for one layer.
fn sentence(name: &str, l: &Layer, s: &[Sample], segs: &[Seg], start: Tick, end: Tick) -> String {
    let name = if name.trim().is_empty() { "The layer" } else { name };
    let t = |i: usize| s.get(i).map_or(0.0, |x| x.t.seconds());
    let span_on = (l.in_point.max(start), l.out_point.min(end));
    if span_on.0 >= span_on.1 || !s.iter().any(|x| x.on) {
        return format!("{name} is not on during {}.", fmt_range(start.seconds(), end.seconds()));
    }
    let mut clauses = vec![];
    if l.in_point > start && l.in_point < end {
        clauses.push(format!("appears at {}s", fmt_secs(l.in_point.seconds())));
    }
    for g in segs {
        let range = fmt_range(t(g.a), t(g.b));
        let (from, to) = match (g.from, g.to) {
            (Val::Num(a), Val::Num(b)) => (a, b),
            _ => (0.0, 0.0),
        };
        let what = match g.kind {
            Kind::Enter => format!("flies in from the {}", g.edge.unwrap_or("edge")),
            Kind::Exit => format!("flies out to the {}", g.edge.unwrap_or("edge")),
            Kind::Move => format!("moves {}", g.direction.unwrap_or("")),
            Kind::Scale => format!("scales {} {}→{}%", if to > from { "up" } else { "down" }, fmt_num(from), fmt_num(to)),
            Kind::Rotate => format!("rotates {}° {}", fmt_num((to - from).abs()), if to > from { "clockwise" } else { "counter-clockwise" }),
            Kind::FadeIn if from <= 0.5 && to >= 99.5 => "fades in".into(),
            Kind::FadeIn if from <= 0.5 => format!("fades in to {}%", fmt_num(to)),
            Kind::FadeIn => format!("fades up to {}%", fmt_num(to)),
            Kind::FadeOut if to <= 0.5 => "fades out".into(),
            Kind::FadeOut => format!("fades down to {}%", fmt_num(to)),
            Kind::Hold => {
                let off = (g.a..=g.b).all(|i| s.get(i).and_then(|x| x.geo.as_ref()).is_some_and(|g| g.on_screen <= 1e-6));
                let verb = if off { "stays off screen" } else { "holds" };
                let whole = s.get(g.a).is_some_and(|x| x.t <= start) && s.get(g.b).is_some_and(|x| x.t >= end);
                clauses.push(if whole { format!("{verb} the whole time") } else { format!("{verb} {range}") });
                continue;
            }
        };
        clauses.push(format!("{what} {range}"));
    }
    if l.out_point > start && l.out_point < end {
        clauses.push(format!("ends at {}s", fmt_secs(l.out_point.seconds())));
    }
    const MAX_CLAUSES: usize = 8;
    if clauses.len() > MAX_CLAUSES {
        let more = clauses.len() - MAX_CLAUSES;
        clauses.truncate(MAX_CLAUSES);
        clauses.push(format!("and {more} more change{}", if more == 1 { "" } else { "s" }));
    }
    if clauses.is_empty() {
        return format!("{name} is on {} with no notable change.", fmt_range(span_on.0.seconds(), span_on.1.seconds()));
    }
    format!("{name} {}.", clauses.join(", "))
}

fn val_json(v: Val) -> Value {
    match v {
        Val::Pt(p) => json!([r2(p[0]), r2(p[1])]),
        Val::Num(n) => json!(r2(n)),
    }
}

/// Most parent links followed, and most descendants a rig is described by.
const MAX_DEPTH: usize = 64;
const MAX_RIG_CHILDREN: usize = 64;

/// Whether `l`'s own Transform moves it within `start..=end` (position, anchor, scale or rotation
/// keys that change, or an expression on one; opacity doesn't move anything).
fn own_motion(l: &Layer, start: Tick, end: Tick) -> bool {
    let Some(tr) = l.transform() else { return false };
    let mut found = false;
    tr.walk("", &mut |_, p| {
        if found || p.match_id == "opacity" {
            return;
        }
        if p.has_expression() {
            found = true;
            return;
        }
        for pair in p.keys.windows(2) {
            let [a, b] = pair else { continue };
            let (ta, tb) = (l.comp_time(a.time), l.comp_time(b.time));
            if a.value != b.value && ta.max(tb) >= start && ta.min(tb) <= end {
                found = true;
                return;
            }
        }
    });
    found
}

/// The animated layers up `l`'s parent chain, nearest first.
fn moving_ancestors<'a>(ctx: &EvalCtx<'a>, l: &Layer, start: Tick, end: Tick) -> Vec<&'a Layer> {
    let mut out = vec![];
    let mut cur = l.parent.and_then(|p| ctx.comp.layer(p));
    let mut depth = 0;
    while let Some(p) = cur {
        if own_motion(p, start, end) {
            out.push(p);
        }
        depth += 1;
        cur = if depth < MAX_DEPTH { p.parent.and_then(|q| ctx.comp.layer(q)) } else { None };
    }
    out
}

/// Every layer parented to `id`, directly or further down, with a picture, in stacking order.
fn drawn_descendants<'a>(ctx: &EvalCtx<'a>, id: LayerId) -> (usize, Vec<&'a Layer>) {
    let mut all: Vec<&Layer> = vec![];
    let mut frontier = vec![id];
    let mut depth = 0;
    while !frontier.is_empty() && depth < MAX_DEPTH && all.len() < MAX_RIG_CHILDREN {
        let kids: Vec<&Layer> = ctx.comp.layers.iter().filter(|c| c.parent.is_some_and(|p| frontier.contains(&p))).collect();
        frontier = kids.iter().map(|c| c.id).collect();
        all.extend(kids);
        depth += 1;
    }
    all.truncate(MAX_RIG_CHILDREN);
    let n = all.len();
    (n, all.into_iter().filter(|c| no_picture(c).is_none()).collect())
}

fn name_of(l: &Layer) -> &str {
    if l.name.trim().is_empty() { "The layer" } else { l.name.as_str() }
}

fn segs_json(s: &[Sample], segs: &[Seg]) -> Value {
    let tsec = |i: usize| r4(s.get(i).map_or(0.0, |x| x.t.seconds()));
    segs.iter()
        .map(|g| {
            let mut v = json!({"kind": g.kind.name(), "start": tsec(g.a), "end": tsec(g.b), "from": val_json(g.from), "to": val_json(g.to)});
            if let Some(d) = g.direction {
                v["direction"] = json!(d);
            }
            if let Some(e) = g.edge {
                v["edge"] = json!(e);
            }
            v
        })
        .collect()
}

/// Why a layer has no motion to describe, if it has no picture.
fn no_picture(l: &Layer) -> Option<String> {
    if !l.source.is_av() {
        return Some(format!("is a {} layer: it has no picture, so it has no on-screen motion", l.source.type_name().to_lowercase()));
    }
    if !l.switches.video {
        return Some("is hidden (Video switch off), so it has no on-screen motion".into());
    }
    None
}

/// The motion summary of `layers` (ids of `ctx.comp`) from `start` to `end`, sampled every
/// `step` (see the module docs for the result shape and semantics).
pub fn describe(ctx: &EvalCtx, layers: &[LayerId], start: Tick, end: Tick, step: Tick) -> Value {
    let comp = ctx.comp;
    let (w, h) = (comp.width as f64, comp.height as f64);
    let diag = w.hypot(h);
    let step = step.max(Tick(1));
    let mut truncated = false;
    let mut notes: Vec<String> = vec![];
    let ids: Vec<LayerId> = layers.iter().copied().filter(|id| comp.layer(*id).is_some()).take(MAX_LAYERS).collect();
    if layers.len() > MAX_LAYERS {
        truncated = true;
        notes.push(format!("only the first {MAX_LAYERS} layers are described"));
    }
    // The sample grid, cut short when it would pass the sample budget.
    let mut end = end.max(start);
    let n = usize::try_from((end - start).0 / step.0).unwrap_or(usize::MAX).saturating_add(1);
    let budget = (MAX_TOTAL_SAMPLES / ids.len().max(1)).max(2);
    if n > budget {
        end = start + Tick(step.0.saturating_mul(i64::try_from(budget - 1).unwrap_or(i64::MAX)));
        truncated = true;
        notes.push(format!(
            "{} layers × {n} samples is over the {MAX_TOTAL_SAMPLES}-sample budget: sampled {} only (use a coarser step or a shorter range)",
            ids.len(),
            fmt_range(start.seconds(), end.seconds())
        ));
    }
    let mut grid: Vec<Tick> = (0..n.min(budget)).map(|k| start + Tick(step.0.saturating_mul(k as i64))).filter(|t| *t <= end).collect();
    if grid.last().is_some_and(|t| *t < end) {
        grid.push(end);
    }

    let mut out = vec![];
    // (layer, its samples) of the drawn layers, for the overlap pass.
    let mut tracked: Vec<(&Layer, Vec<Sample>)> = vec![];
    for id in &ids {
        let Some(l) = comp.layer(*id) else { continue };
        let mut o = json!({
            "layer": id.0, "name": l.name,
            "inPoint": r4(l.in_point.seconds()), "outPoint": r4(l.out_point.seconds()),
            "segments": [], "offscreen": [],
        });
        let measure = |c: &Layer| {
            let mut times = grid.clone();
            times.extend(key_times(ctx, c, start, end));
            times.sort();
            times.dedup();
            let s: Vec<Sample> = times.iter().map(|&t| sample(ctx, c, t, w, h)).collect();
            let segs = segments(&s, step, diag, w, h);
            (s, segs)
        };
        if let Some(why) = no_picture(l) {
            let (n, kids) = drawn_descendants(ctx, *id);
            let moving = |g: &&Seg| !matches!(g.kind, Kind::Hold | Kind::FadeIn | Kind::FadeOut);
            // A rig: described by the child it moves the most.
            let best = kids.iter().take(MAX_RIG_CHILDREN).map(|c| (*c, measure(c))).max_by_key(|(_, (_, segs))| segs.iter().filter(moving).count());
            let Some((rep, (s, mut segs))) = best.filter(|_| n > 0 && own_motion(l, start, end)) else {
                let rig =
                    if n > 0 { format!(" It parents {n} layer{} but doesn't animate in this range", if n == 1 { "" } else { "s" }) } else { String::new() };
                o["summary"] = json!(format!("{} {why}.{rig}", name_of(l)));
                out.push(o);
                continue;
            };
            segs.truncate(MAX_SEGMENTS);
            let names: Vec<&str> = kids.iter().take(4).map(|c| c.name.as_str()).collect();
            let more = if kids.len() > names.len() { format!(", +{}", kids.len() - names.len()) } else { String::new() };
            let lead = format!(
                "{} is a {} that moves its {} layer{} ({}{more}): it",
                name_of(l),
                l.source.type_name().to_lowercase(),
                kids.len(),
                if kids.len() == 1 { "" } else { "s" },
                names.join(", ")
            );
            o["segments"] = segs_json(&s, &segs);
            o["describedBy"] = json!({"layer": rep.id.0, "name": rep.name});
            o["moves"] = kids.iter().map(|c| json!({"layer": c.id.0, "name": c.name})).collect();
            o["summary"] = json!(format!("{} (its keys are on this layer).", sentence(&lead, rep, &s, &segs, start, end).trim_end_matches('.')));
            out.push(o);
            continue;
        }
        let (s, segs_all) = measure(l);
        if s.iter().any(|x| x.on) && !s.iter().any(|x| x.geo.is_some()) {
            o["summary"] = json!(format!(
                "{} has no visible content (an audio-only, empty or behind-the-camera layer), so it has no on-screen motion.",
                if l.name.is_empty() { "The layer" } else { l.name.as_str() }
            ));
            out.push(o);
            continue;
        }
        let mut segs = segs_all;
        if segs.len() > MAX_SEGMENTS {
            segs.truncate(MAX_SEGMENTS);
            truncated = true;
            notes.push(format!("“{}”: only the first {MAX_SEGMENTS} segments are listed", l.name));
        }
        o["segments"] = segs_json(&s, &segs);
        o["offscreen"] = offscreen(&s, l).iter().map(|(a, b)| json!({"start": r4(a.seconds()), "end": r4(b.seconds())})).collect();
        let mut summary = sentence(&l.name, l, &s, &segs, start, end);
        // Moves that come from animated parents: say which layer carries the keys.
        let ancestors =
            if segs.iter().any(|g| matches!(g.kind, Kind::Move | Kind::Enter | Kind::Exit)) { moving_ancestors(ctx, l, start, end) } else { vec![] };
        if let Some(first) = ancestors.first() {
            o["movedBy"] = ancestors.iter().map(|a| json!({"layer": a.id.0, "name": a.name})).collect();
            let who = ancestors.iter().map(|a| format!("“{}”", a.name)).collect::<Vec<_>>().join(", ");
            let note = if own_motion(l, start, end) {
                format!(" Its moves combine its own keys with its parent{} {who}.", if ancestors.len() == 1 { "" } else { "s" })
            } else {
                format!(
                    " The moves come from its parent{} {who}: key “{}” (layer {}), not this layer.",
                    if ancestors.len() == 1 { "" } else { "s" },
                    first.name,
                    first.id.0
                )
            };
            summary.push_str(&note);
        }
        o["summary"] = json!(summary);
        out.push(o);
        tracked.push((l, s));
    }

    let overlaps = overlaps(&tracked, &grid, w, h, &mut truncated, &mut notes);
    let mut v = json!({
        "comp": ctx.comp_id.0,
        "compName": ctx.project.item(ctx.comp_id).map_or("", |i| i.name.as_str()),
        "start": r4(start.seconds()), "end": r4(end.seconds()), "step": r4(step.seconds()),
        "truncated": truncated,
        "layers": out,
        "overlaps": overlaps,
    });
    if !notes.is_empty() {
        v["notes"] = json!(notes);
    }
    v
}

/// Overlap events between the tracked layers, sampled on the grid.
fn overlaps(tracked: &[(&Layer, Vec<Sample>)], grid: &[Tick], w: f64, h: f64, truncated: &mut bool, notes: &mut Vec<String>) -> Vec<Value> {
    if tracked.len() > MAX_OVERLAP_LAYERS {
        *truncated = true;
        notes.push(format!("overlaps are tracked between the first {MAX_OVERLAP_LAYERS} layers only"));
    }
    let tracked = tracked.get(..tracked.len().min(MAX_OVERLAP_LAYERS)).unwrap_or(&[]);
    let frame_area = (w * h).max(1e-9);
    // Per layer and grid time: its on-screen geometry when drawn and not a full-frame background.
    let at: Vec<Vec<Option<&Geo>>> = tracked
        .iter()
        .map(|(_, s)| {
            grid.iter()
                .map(|t| {
                    let x = s.binary_search_by(|x| x.t.cmp(t)).ok().and_then(|i| s.get(i))?;
                    let g = x.geo.as_ref().filter(|_| x.drawn)?;
                    (g.clip_area > 1e-6 && g.clip_area / frame_area < FULL_FRAME).then_some(g)
                })
                .collect()
        })
        .collect();
    let mut events: Vec<(Tick, Tick, usize, usize, f64)> = vec![];
    for i in 0..tracked.len() {
        for j in i + 1..tracked.len() {
            let (Some(ga), Some(gb)) = (at.get(i), at.get(j)) else { continue };
            let mut cur: Option<(Tick, f64)> = None;
            for (k, t) in grid.iter().enumerate() {
                let f = match (ga.get(k).copied().flatten(), gb.get(k).copied().flatten()) {
                    (Some(a), Some(b)) => overlap_fraction(a, b),
                    _ => 0.0,
                };
                if f > OVERLAP_MIN {
                    cur = Some(cur.map_or((*t, f), |(s, m)| (s, m.max(f))));
                } else if let Some((s, m)) = cur.take() {
                    events.push((s, *t, i, j, m));
                }
            }
            if let (Some((s, m)), Some(last)) = (cur, grid.last())
                && *last > s
            {
                events.push((s, *last, i, j, m));
            }
        }
    }
    events.sort_by(|p, q| p.0.cmp(&q.0).then(p.2.cmp(&q.2)).then(p.3.cmp(&q.3)));
    if events.len() > MAX_OVERLAPS {
        events.truncate(MAX_OVERLAPS);
        *truncated = true;
        notes.push(format!("only the first {MAX_OVERLAPS} overlaps are listed"));
    }
    events
        .iter()
        .filter_map(|&(s, e, i, j, m)| {
            let (a, b) = (tracked.get(i)?.0, tracked.get(j)?.0);
            Some(json!({"a": a.id.0, "b": b.id.0, "aName": a.name, "bName": b.name, "start": r4(s.seconds()), "end": r4(e.seconds()), "maxFraction": r4(m.clamp(0.0, 1.0))}))
        })
        .collect()
}

/// The overlap of two on-screen boxes as a share of the smaller one.
fn overlap_fraction(a: &Geo, b: &Geo) -> f64 {
    let bb = |g: &Geo| layout::aabb(&g.clip);
    let (x, y) = (bb(a), bb(b));
    if x[0] > y[0] + y[2] || y[0] > x[0] + x[2] || x[1] > y[1] + y[3] || y[1] > x[1] + x[3] {
        return 0.0;
    }
    let small = a.clip_area.min(b.clip_area);
    if small <= 1e-6 {
        return 0.0;
    }
    let inter = layout::polygon_area(&layout::clip_convex(&a.clip, &b.corners));
    finite(inter / small, 0.0)
}
