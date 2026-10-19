//! Composition viewer overlays and their drags: mask and shape paths (vertices, tangents,
//! segments for Add Vertex), position motion paths with keyframe dots and spatial Bezier
//! handles, the path free-transform box and the Mask Feather tool.
//!
//! Every edit is an engine command (`mask.*`, `path.freeTransform`, `keys.set`,
//! `keys.setSpatialTangents`, `keys.bendSpatialSegment`, `prop.set`).

use effectcraft_engine::geom::{Mat3, vec2 as gv2};
use effectcraft_engine::keyframe::ShapePath;
use effectcraft_engine::project::{GroupKind, Layer, LayerId};
use effectcraft_engine::render::EvalCtx;
use effectcraft_engine::time::Tick;
use effectcraft_engine::{KeyRef, VertexRef};
use egui::{Color32, Pos2, Rect, Stroke, StrokeKind, vec2};
use serde_json::json;

use super::viewer::ViewerMap;
use crate::EffectcraftApp;
use crate::state::FreeTransform;

/// A vertex or tangent handle drawn this frame (masks and shape paths alike; `mask` is the path
/// group's uid).
#[derive(Clone, Copy)]
pub(crate) struct VertexHit {
    pub layer: LayerId,
    pub mask: u64,
    pub index: usize,
    pub pos: Pos2,
    /// None = the vertex itself, Some(out?) = a tangent handle.
    pub tangent: Option<bool>,
    /// Path-space vertex position and path → comp matrix.
    pub vertex: [f64; 2],
    pub l2c: Mat3,
}

/// A vertex marker to draw. Markers are drawn after the layer bounding boxes so a box handle
/// never covers a path point (#1042).
#[derive(Clone, Copy)]
pub(crate) struct VertexMark {
    pub pos: Pos2,
    pub color: Color32,
    pub selected: bool,
}

/// Draw the vertex markers collected by [`draw_paths`], `size` points wide.
pub(crate) fn draw_vertex_marks(painter: &egui::Painter, marks: &[VertexMark], size: f32) {
    for m in marks {
        let r = Rect::from_center_size(m.pos, vec2(size, size));
        if m.selected {
            painter.rect_filled(r, 0.0, m.color);
        } else {
            painter.rect_filled(r, 0.0, Color32::from_black_alpha(160));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, m.color), StrokeKind::Inside);
        }
    }
}

/// A path drawn this frame (for segment hits and the free-transform box).
#[derive(Clone)]
pub(crate) struct PathInfo {
    pub layer: LayerId,
    pub uid: u64,
    /// Path space → comp.
    pub m: Mat3,
    pub sp: ShapePath,
    pub is_mask: bool,
    /// Mask feather points' radius handles on screen (feather point index, handle position).
    pub feather: Vec<(usize, Pos2)>,
}

fn cubic(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2], t: f64) -> [f64; 2] {
    let u = 1.0 - t;
    let f = |k: usize| u * u * u * p0[k] + 3.0 * u * u * t * p1[k] + 3.0 * u * t * t * p2[k] + t * t * t * p3[k];
    [f(0), f(1)]
}

/// Control points of segment `i` (vertex i → next) in path space.
pub(crate) fn segment(sp: &ShapePath, i: usize) -> Option<[[f64; 2]; 4]> {
    let n = sp.vertices.len();
    let j = if i + 1 < n {
        i + 1
    } else if sp.closed && n > 1 {
        0
    } else {
        return None;
    };
    let (a, d) = (sp.vertices[i], sp.vertices[j]);
    let to = sp.out_tangents.get(i).copied().unwrap_or_default();
    let ti = sp.in_tangents.get(j).copied().unwrap_or_default();
    Some([a, [a[0] + to[0], a[1] + to[1]], [d[0] + ti[0], d[1] + ti[1]], d])
}

/// The path segment under a screen point: (layer, path uid, segment, t).
pub(crate) fn segment_at(paths: &[PathInfo], map: &ViewerMap, pos: Pos2, tol: f32) -> Option<(LayerId, u64, usize, f64)> {
    let mut best: Option<(f32, (LayerId, u64, usize, f64))> = None;
    for p in paths {
        for i in 0..p.sp.vertices.len() {
            let Some(c) = segment(&p.sp, i) else { continue };
            for k in 1..48 {
                let t = k as f64 / 48.0;
                let q = cubic(c[0], c[1], c[2], c[3], t);
                let cq = p.m.apply(gv2(q[0], q[1]));
                let d = map.to_screen([cq.x, cq.y]).distance(pos);
                if d < tol && best.as_ref().is_none_or(|b| d < b.0) {
                    best = Some((d, (p.layer, p.uid, i, t)));
                }
            }
        }
    }
    best.map(|b| b.1)
}

/// The open path whose last vertex lies within `tol` points of `pos`: the Pen resumes it there.
pub(crate) fn open_end_at(paths: &[PathInfo], map: &ViewerMap, pos: Pos2, tol: f32) -> Option<(LayerId, u64)> {
    paths.iter().find_map(|p| {
        let v = p.sp.vertices.last().filter(|_| !p.sp.closed)?;
        let q = p.m.apply(gv2(v[0], v[1]));
        (map.to_screen([q.x, q.y]).distance(pos) < tol).then_some((p.layer, p.uid))
    })
}

/// A mask or shape path of a layer by group uid, evaluated at the context time.
pub(crate) fn path_of(ectx: &EvalCtx, layer: LayerId, uid: u64) -> Option<PathInfo> {
    let l = ectx.comp.layer(layer)?;
    let g = l.props.find_group(uid)?;
    let sp = g.get("path").map(|pr| ectx.value(l, pr)).and_then(|v| v.as_path().cloned())?;
    let (m, _) = super::viewer::l2c(ectx, l);
    let is_mask = matches!(g.kind, GroupKind::Mask { .. });
    let m = if is_mask { m } else { m * effectcraft_engine::render::shapes::item_matrix(ectx, l, uid).unwrap_or(Mat3::IDENTITY) };
    Some(PathInfo { layer, uid, m, sp, is_mask, feather: Vec::new() })
}

/// The path on screen: `sp` through `m` and the viewer map, flattened, with the closing point
/// repeated for a closed path.
pub(crate) fn flatten(sp: &ShapePath, m: &Mat3, map: &ViewerMap) -> Vec<Pos2> {
    let k = effectcraft_engine::render::kurbo_path(sp);
    let mut pts: Vec<Pos2> = vec![];
    kurbo::flatten(k.iter(), 0.5, |el| match el {
        kurbo::PathEl::MoveTo(q) | kurbo::PathEl::LineTo(q) => {
            let c = m.apply(gv2(q.x, q.y));
            pts.push(map.to_screen([c.x, c.y]));
        }
        _ => {}
    });
    if sp.closed && !pts.is_empty() {
        pts.push(pts[0]);
    }
    pts
}

/// Draw the masks and (on shape layers) shape paths of a selected layer with their vertices;
/// selected vertices show their tangent handles. Collects vertex hits and paths.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_paths(
    app: &mut EffectcraftApp,
    painter: &egui::Painter,
    map: &ViewerMap,
    ectx: &EvalCtx,
    l: &Layer,
    label_col: Color32,
    sel: &[VertexRef],
    pen: Option<(LayerId, u64)>,
    hover: Option<Pos2>,
    hits: &mut Vec<VertexHit>,
    marks: &mut Vec<VertexMark>,
    paths: &mut Vec<PathInfo>,
) {
    let (m, _) = super::viewer::l2c(ectx, l);
    let mut list: Vec<(u64, String, ShapePath, Mat3, Color32, bool, bool)> = vec![];
    if let Some(masks) = l.masks() {
        for g in masks.groups() {
            let GroupKind::Mask { color, locked, .. } = g.kind else { continue };
            if locked && app.session.state.hide_locked_masks {
                continue;
            }
            let Some(sp) = g.get("path").map(|pr| ectx.value(l, pr)).and_then(|v| v.as_path().cloned()) else { continue };
            list.push((g.uid, g.name.clone(), sp, m, Color32::from_rgb(color[0], color[1], color[2]), true, locked || l.switches.locked));
        }
    }
    for (uid, sp) in effectcraft_engine::viewer::shape_paths(ectx, l) {
        let im = effectcraft_engine::render::shapes::item_matrix(ectx, l, uid).unwrap_or(Mat3::IDENTITY);
        let name = l.props.find_group(uid).map(|g| g.name.clone()).unwrap_or_default();
        list.push((uid, name, sp, m * im, label_col, false, l.switches.locked));
    }
    for (uid, name, sp, pm, mc, is_mask, locked) in list {
        painter.add(egui::Shape::line(flatten(&sp, &pm, map), Stroke::new(1.0, mc)));
        if locked {
            continue;
        }
        let scr = |p: [f64; 2]| {
            let c = pm.apply(gv2(p[0], p[1]));
            map.to_screen([c.x, c.y])
        };
        for (i, v) in sp.vertices.iter().enumerate() {
            let s = scr(*v);
            let selected = sel.iter().any(|x| x.layer == l.id && x.mask == uid && x.index == i);
            if selected {
                for (out, tg) in [(false, sp.in_tangents.get(i)), (true, sp.out_tangents.get(i))] {
                    let Some(tg) = tg.filter(|t| t[0] != 0.0 || t[1] != 0.0) else { continue };
                    let h = scr([v[0] + tg[0], v[1] + tg[1]]);
                    painter.line_segment([s, h], Stroke::new(1.0, mc));
                    painter.circle_filled(h, 3.0, mc);
                    hits.push(VertexHit { layer: l.id, mask: uid, index: i, pos: h, tangent: Some(out), vertex: *v, l2c: pm });
                }
            }
            marks.push(VertexMark { pos: s, color: mc, selected });
            let kind = if is_mask { "mask" } else { "shapePath" };
            app.auto.add(&format!("viewer.{kind}.{uid}.vertex.{i}"), Rect::from_center_size(s, vec2(8.0, 8.0)), &name);
            hits.push(VertexHit { layer: l.id, mask: uid, index: i, pos: s, tangent: None, vertex: *v, l2c: pm });
        }
        // Pen rubber band from the last vertex to the pointer.
        if pen == Some((l.id, uid))
            && let (Some(last), Some(hp)) = (sp.vertices.last(), hover)
        {
            painter.line_segment([scr(*last), hp], Stroke::new(1.0, mc.gamma_multiply(0.6)));
        }
        let feather = if is_mask { draw_feather_points(painter, map, &sp, &pm, mc) } else { Vec::new() };
        for (i, h) in &feather {
            app.auto.add(&format!("viewer.mask.{uid}.feather.{i}"), Rect::from_center_size(*h, vec2(8.0, 8.0)), &name);
        }
        paths.push(PathInfo { layer: l.id, uid, m: pm, sp, is_mask, feather });
    }
}

/// Mask feather points: a hollow dot on the path and a radius handle along the normal (outwards for
/// outer feather, inwards for inner), joined by a line. Returns the handles' screen positions.
fn draw_feather_points(painter: &egui::Painter, map: &ViewerMap, sp: &ShapePath, pm: &Mat3, col: Color32) -> Vec<(usize, Pos2)> {
    let mut out = Vec::new();
    let scr = |p: [f64; 2]| {
        let c = pm.apply(gv2(p[0], p[1]));
        map.to_screen([c.x, c.y])
    };
    let inside = |q: [f64; 2]| {
        // Even-odd test against the vertex polygon (good enough to orient the handle).
        let v = &sp.vertices;
        let mut c = false;
        for i in 0..v.len() {
            let (a, b) = (v[i], v[(i + 1) % v.len()]);
            if (a[1] > q[1]) != (b[1] > q[1]) && q[0] < (b[0] - a[0]) * (q[1] - a[1]) / (b[1] - a[1]) + a[0] {
                c = !c;
            }
        }
        c
    };
    for (i, f) in sp.feather.iter().enumerate() {
        let Some(c) = segment(sp, f.segment) else { continue };
        let at = cubic(c[0], c[1], c[2], c[3], f.t);
        let (t0, t1) = ((f.t - 1e-3).max(0.0), (f.t + 1e-3).min(1.0));
        let (a, b) = (cubic(c[0], c[1], c[2], c[3], t0), cubic(c[0], c[1], c[2], c[3], t1));
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = dx.hypot(dy).max(1e-9);
        let mut n = [-dy / len, dx / len];
        if inside([at[0] + n[0] * 2.0, at[1] + n[1] * 2.0]) {
            n = [-n[0], -n[1]];
        }
        let h = [at[0] + n[0] * f.radius, at[1] + n[1] * f.radius];
        let (ps, hs) = (scr(at), scr(h));
        painter.line_segment([ps, hs], Stroke::new(1.0, col));
        painter.circle_stroke(ps, 3.5, Stroke::new(1.0, col));
        painter.circle_filled(hs, 3.0, col);
        out.push((i, hs));
    }
    out
}

// ---------------------------------------------------------------- motion paths

/// A keyframe dot or spatial tangent handle on a motion path.
#[derive(Clone, Copy)]
pub(crate) struct KeyHit {
    pub layer: LayerId,
    pub prop: u64,
    /// Layer time of the key.
    pub time: Tick,
    pub pos: Pos2,
    /// None = the key, Some(out?) = a tangent handle.
    pub tangent: Option<bool>,
    pub value: [f64; 3],
    /// Parent space → comp.
    pub p2c: Mat3,
    pub curved: bool,
}

/// A displayed motion-path sample interval; times remain in the layer's clock.
pub(crate) struct MotionSegment {
    pub layer: LayerId,
    pub prop: u64,
    pub from: Pos2,
    pub to: Pos2,
    pub start: Tick,
    pub end: Tick,
    /// Parent space → comp.
    pub p2c: Mat3,
}

pub(crate) fn motion_key_at(hits: &[KeyHit], pos: Pos2) -> Option<KeyHit> {
    hits.iter().rev().find(|h| h.tangent.is_none() && h.pos.distance(pos) < 6.0).copied()
}

/// What the Pen slot acts on at a point of a motion path. Decided in one place, so the pointer
/// shown, a click and a drag always agree (they used to apply their own rules and distances: the
/// Add pointer showed where a click grabbed a tangent handle lying on the line, or tried to add a
/// key where there was no free frame).
#[derive(Clone, Copy)]
pub(crate) enum PenTarget {
    /// A keyframe's square: a click deletes it (Alt: converts it), a drag moves it.
    Key(KeyHit),
    /// A spatial tangent handle: a drag bends the path.
    Tangent,
    /// On the path: the free frame nearest the pointer, where a click adds a keyframe (After
    /// Effects: a motion path's dots are its frames and keys sit on frames).
    AddFrame(PathFrame),
    /// On the path between keys on adjacent frames: no frame to add a key on.
    PathFull,
    /// Not on a motion path.
    None,
}

/// Pick radius of a key square, of a tangent handle and of the path line, in points.
const PEN_KEY_R: f32 = 6.0;
const PEN_TANGENT_R: f32 = 5.0;
const PEN_PATH_R: f32 = 6.0;

/// The displayed sample interval nearest `pos` (within the path's pick radius), and how far
/// along it the pointer is.
fn segment_near(segments: &[MotionSegment], pos: Pos2) -> Option<(&MotionSegment, f32)> {
    segments
        .iter()
        .filter_map(|s| {
            let delta = s.to - s.from;
            if delta.length_sq() < 0.001 {
                return None;
            }
            let f = ((pos - s.from).dot(delta) / delta.length_sq()).clamp(0.0, 1.0);
            let d = pos.distance(s.from + delta * f);
            (d < PEN_PATH_R).then_some((s, f, d))
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(s, f, _)| (s, f))
}

/// The [`PenTarget`] at `pos`: a key square first, then a tangent handle, then the path line.
pub(crate) fn pen_target(hits: &[KeyHit], segments: &[MotionSegment], frames: &[PathFrame], pos: Pos2) -> PenTarget {
    let nearest = |tangent: bool, r: f32| {
        hits.iter()
            .filter(|h| h.tangent.is_some() == tangent && h.pos.distance(pos) < r)
            .min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))
            .copied()
    };
    if let Some(k) = nearest(false, PEN_KEY_R) {
        return PenTarget::Key(k);
    }
    if nearest(true, PEN_TANGENT_R).is_some() {
        return PenTarget::Tangent;
    }
    let Some((seg, f)) = segment_near(segments, pos) else {
        return PenTarget::None;
    };
    let is_key = |t: Tick| hits.iter().any(|h| h.tangent.is_none() && h.layer == seg.layer && h.prop == seg.prop && h.time == t);
    let frame = |t: Tick| frames.iter().find(|fr| fr.layer == seg.layer && fr.prop == seg.prop && fr.time == t).copied();
    let order = if f < 0.5 { [seg.start, seg.end] } else { [seg.end, seg.start] };
    order.into_iter().filter(|t| !is_key(*t)).find_map(frame).map_or(PenTarget::PathFull, PenTarget::AddFrame)
}

pub(crate) fn edit_motion_key(app: &mut EffectcraftApp, h: KeyHit, delete: bool) {
    let result = app.session.execute("keys.select", json!({"keys": [{"layer": h.layer.0, "prop": h.prop, "time": h.time.seconds()}]})).and_then(|_| {
        if delete {
            app.session.execute("keys.delete", json!({}))
        } else {
            // Convert Vertex: a curved key becomes Linear, a corner Auto Bezier (as in After Effects).
            app.session.execute("keys.interpolation", json!({"spatial": if h.curved { "linear" } else { "autoBezier" }}))
        }
    });
    if let Err(e) = result {
        app.ui.status = e.to_string();
    }
}

pub(crate) fn insert_motion_key(app: &mut EffectcraftApp, hit: (LayerId, u64, Tick)) {
    let (layer, prop, time) = hit;
    if let Err(e) = app.session.execute("keys.insertSpatial", json!({"layer": layer.0, "prop": prop, "time": time.seconds()})) {
        app.ui.status = e.to_string();
    }
}

/// A frame on a motion path (one of its dots): where the Pen and Add Vertex tools add a
/// keyframe.
#[derive(Clone, Copy)]
pub(crate) struct PathFrame {
    pub layer: LayerId,
    pub prop: u64,
    /// Layer time of the frame.
    pub time: Tick,
    pub pos: Pos2,
}

/// The motion-path frame nearest `pos` (within a few points) where a keyframe can be added: not
/// on a keyframe dot already there.
pub(crate) fn motion_frame_at(frames: &[PathFrame], hits: &[KeyHit], pos: Pos2) -> Option<PathFrame> {
    if motion_key_at(hits, pos).is_some() {
        return None;
    }
    let near = frames.iter().filter(|f| f.pos.distance(pos) < 5.0).min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))?;
    (!hits.iter().any(|h| h.tangent.is_none() && h.layer == near.layer && h.prop == near.prop && h.time == near.time)).then_some(*near)
}

/// Parent space → comp matrix of a 2D layer at the context time.
pub(crate) fn parent_to_comp(ectx: &EvalCtx, l: &Layer) -> Mat3 {
    match l.parent.and_then(|p| ectx.comp.layer(p)) {
        Some(p) => {
            let w = ectx.world_matrix(p).0;
            Mat3([[w[0][0], w[0][1], w[0][3]], [w[1][0], w[1][1], w[1][3]], [0.0, 0.0, 1.0]])
        }
        None => Mat3::IDENTITY,
    }
}

/// Draw the position motion path of a selected 2D layer: frame dots along the path, keyframe
/// squares (filled when selected) and spatial Bezier tangent handles. Works through parents.
#[allow(clippy::too_many_arguments)]
pub(crate) fn motion_path(
    app: &mut EffectcraftApp,
    painter: &egui::Painter,
    map: &ViewerMap,
    ectx: &EvalCtx,
    l: &Layer,
    col: Color32,
    hits: &mut Vec<KeyHit>,
    segments: &mut Vec<MotionSegment>,
    frames: &mut Vec<PathFrame>,
) {
    let Some(pos) = l.transform().and_then(|tr| tr.get("position")) else { return };
    if pos.keys.is_empty() || l.is_3d() {
        return;
    }
    let p2c = parent_to_comp(ectx, l);
    let to_scr = |v: [f64; 3]| {
        let c = p2c.apply(gv2(v[0], v[1]));
        map.to_screen([c.x, c.y])
    };
    // Settings ▸ Composition ▸ Motion Path: all keyframes, none, or a window around the CTI.
    let times: Vec<Tick> = pos.keys.iter().map(|k| k.time).collect();
    let span = app
        .session
        .prefs
        .motion_path_span(&times, l.layer_time(ectx.time))
        .or_else(|| (times.len() == 1 && app.session.prefs.composition.motion_path != "none").then(|| times.first().copied().map(|t| (t, t))).flatten());
    let Some((lt0, lt1)) = span else { return };
    let fd = ectx.comp.frame_duration();
    let mut pts = vec![];
    let mut samples = vec![];
    let mut lt = lt0;
    while lt <= lt1 && pts.len() < 20_000 {
        let q = to_scr(pos.value_at(lt).as_vec3());
        frames.push(PathFrame { layer: l.id, prop: pos.uid, time: lt, pos: q });
        pts.push(q);
        samples.push(lt);
        if fd <= Tick::ZERO || lt.0.checked_add(fd.0).is_none() {
            break;
        }
        lt += fd;
    }
    pts.push(to_scr(pos.value_at(lt1).as_vec3()));
    samples.push(lt1);
    for (points, times) in pts.windows(2).zip(samples.windows(2)) {
        let (Some(&from), Some(&to), Some(&start), Some(&end)) = (points.first(), points.get(1), times.first(), times.get(1)) else { continue };
        // Holds jump rather than travelling through the connecting line.
        let held = pos.keys.iter().rev().find(|k| k.time <= start).is_some_and(|k| k.out_interp == effectcraft_engine::keyframe::Interp::Hold);
        if !held {
            segments.push(MotionSegment { layer: l.id, prop: pos.uid, from, to, start, end, p2c });
        }
    }
    let faint = col.gamma_multiply(0.8);
    painter.add(egui::Shape::line(pts.clone(), Stroke::new(1.0, faint)));
    for q in &pts {
        painter.circle_filled(*q, 1.2, faint);
    }
    let sel = &app.session.state.selected_keys;
    for (i, k) in pos.keys.iter().enumerate() {
        if k.time < lt0 || k.time > lt1 {
            continue;
        }
        let v = k.value.as_vec3();
        let s = to_scr(v);
        let curved = k.spatial_auto || k.spatial_in != [0.0; 3] || k.spatial_out != [0.0; 3];
        // Auto-Bezier keys show handles on a side the path leaves straight too (a two-key path's
        // ends), to drag it into a curve; Linear keys (no tangents) show none.
        let (tin, tout) = if k.spatial_auto {
            effectcraft_engine::keyframe::spatial_handles(&pos.keys, i)
        } else {
            effectcraft_engine::keyframe::spatial_tangents(&pos.keys, i)
        };
        for (out, tg) in [(false, tin), (true, tout)] {
            let has = (out && i + 1 < pos.keys.len()) || (!out && i > 0);
            if !has || (tg[0] == 0.0 && tg[1] == 0.0) {
                continue;
            }
            let h = to_scr([v[0] + tg[0], v[1] + tg[1], v[2] + tg[2]]);
            painter.line_segment([s, h], Stroke::new(1.0, col));
            painter.circle_filled(h, 3.0, col);
            let side = if out { "out" } else { "in" };
            app.auto.add(&format!("viewer.motionPath.{}.{i}.{side}", l.id.0), Rect::from_center_size(h, vec2(8.0, 8.0)), "Spatial tangent");
            hits.push(KeyHit { layer: l.id, prop: pos.uid, time: k.time, pos: h, tangent: Some(out), value: v, p2c, curved });
        }
        let r = Rect::from_center_size(s, vec2(7.0, 7.0));
        if sel.contains(&KeyRef { layer: l.id, prop: pos.uid, time: k.time }) {
            painter.rect_filled(r, 0.0, col);
        } else {
            painter.rect_filled(r, 0.0, Color32::from_black_alpha(140));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, col), StrokeKind::Inside);
        }
        app.auto.add(&format!("viewer.motionPath.{}.{i}", l.id.0), r.expand(2.0), "Position keyframe");
        hits.push(KeyHit { layer: l.id, prop: pos.uid, time: k.time, pos: s, tangent: None, value: v, p2c, curved });
    }
}

// ---------------------------------------------------------------- free transform

/// Enter Free Transform on a path (double-click a path with the Selection tool, Cmd+T with its
/// points selected, Layer ▸ Mask and Shape Path ▸ Free Transform Points): an upright box with the
/// anchor point in its centre.
pub(crate) fn begin_free_transform(app: &mut EffectcraftApp, layer: LayerId, uid: u64) {
    app.ui.viewer.free_transform = Some(FreeTransform::new(layer.0, uid));
}

pub(crate) fn end_free_transform(app: &mut EffectcraftApp) {
    app.ui.viewer.free_transform = None;
}

/// Turn the box (`angle`, degrees) or move its anchor point (`anchor`, box coordinates).
pub(crate) fn set_free_transform(app: &mut EffectcraftApp, angle: Option<f64>, anchor: Option<[f64; 2]>) {
    if let Some(f) = app.ui.viewer.free_transform.as_mut() {
        if let Some(a) = angle.filter(|a| a.is_finite()) {
            f.angle = a.rem_euclid(360.0);
        }
        if let Some(a) = anchor.filter(|a| a[0].is_finite() && a[1].is_finite()) {
            f.anchor = a;
        }
    }
}

pub(crate) fn free_transform_target(app: &EffectcraftApp) -> Option<(LayerId, u64)> {
    app.ui.viewer.free_transform.as_ref().map(|f| (LayerId(f.layer), f.path))
}

/// A 2×3 affine matrix `[a, b, c, d, e, f]`: x' = a·x + c·y + e, y' = b·x + d·y + f (the layout
/// `path.freeTransform` takes as `matrix`).
pub(crate) type Affine = [f64; 6];

pub(crate) const AF_ID: Affine = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

pub(crate) fn af_apply(m: &Affine, p: [f64; 2]) -> [f64; 2] {
    [m[0] * p[0] + m[2] * p[1] + m[4], m[1] * p[0] + m[3] * p[1] + m[5]]
}

/// `p ∘ q`: `q` first, then `p`.
pub(crate) fn af_mul(p: &Affine, q: &Affine) -> Affine {
    [
        p[0] * q[0] + p[2] * q[1],
        p[1] * q[0] + p[3] * q[1],
        p[0] * q[2] + p[2] * q[3],
        p[1] * q[2] + p[3] * q[3],
        p[0] * q[4] + p[2] * q[5] + p[4],
        p[1] * q[4] + p[3] * q[5] + p[5],
    ]
}

pub(crate) fn af_inv(m: &Affine) -> Option<Affine> {
    let det = m[0] * m[3] - m[2] * m[1];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    Some([a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])])
}

/// The box of a free transform: the tightest rectangle around the transformed points, turned by
/// `angle` (degrees, path space), as `uv → path` (`(0, 0)` its first corner, `(1, 1)` the
/// opposite one). A side thinner than `min_side` path units (points in a line) is widened about
/// its centre so the handles stay apart.
pub(crate) fn fit_box(points: &[[f64; 2]], angle: f64, min_side: f64) -> Option<Affine> {
    let (s, c) = angle.to_radians().sin_cos();
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for p in points {
        // Into the box's frame (rotate by -angle).
        let q = [p[0] * c + p[1] * s, -p[0] * s + p[1] * c];
        lo = [lo[0].min(q[0]), lo[1].min(q[1])];
        hi = [hi[0].max(q[0]), hi[1].max(q[1])];
    }
    if !(lo[0] <= hi[0] && lo[1] <= hi[1]) {
        return None;
    }
    let min_side = if min_side.is_finite() { min_side.max(1e-3) } else { 1e-3 };
    for k in 0..2 {
        if hi[k] - lo[k] < min_side {
            let mid = (lo[k] + hi[k]) / 2.0;
            (lo[k], hi[k]) = (mid - min_side / 2.0, mid + min_side / 2.0);
        }
    }
    let (w, h) = (hi[0] - lo[0], hi[1] - lo[1]);
    // uv → box frame, then back to path space (rotate by +angle).
    let rot: Affine = [c, s, -s, c, 0.0, 0.0];
    Some(af_mul(&rot, &[w, 0.0, 0.0, h, lo[0], lo[1]]))
}

/// The handles' box coordinates: corners 0–3 (clockwise from the first corner), edge midpoints
/// 4–7 (top, right, bottom, left).
pub(crate) const FT_HANDLES: [[f64; 2]; 8] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.5, 0.0], [1.0, 0.5], [0.5, 1.0], [0.0, 0.5]];

/// The free-transform box this frame: its `uv → path` matrix, the path → comp matrix, handles,
/// outline and anchor on screen.
pub(crate) struct FtBox {
    pub layer: LayerId,
    pub uid: u64,
    pub angle: f64,
    pub anchor_uv: [f64; 2],
    pub f: Affine,
    pub m: Mat3,
    pub handles: [Pos2; 8],
    pub quad: [Pos2; 4],
    pub anchor: Pos2,
}

/// What the pointer is over on the free-transform box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FtPart {
    Anchor,
    Handle(usize),
    Inside,
    /// Just outside: rotate about the anchor.
    Rotate,
}

impl FtBox {
    pub fn part_at(&self, p: Pos2) -> Option<FtPart> {
        if self.anchor.distance(p) < 8.0 {
            return Some(FtPart::Anchor);
        }
        if let Some(i) =
            (0..8).min_by(|a, b| self.handles[*a].distance(p).total_cmp(&self.handles[*b].distance(p))).filter(|i| self.handles[*i].distance(p) < 7.0)
        {
            return Some(FtPart::Handle(i));
        }
        if point_in_quad(p, &self.quad) {
            return Some(FtPart::Inside);
        }
        let near = (0..4).any(|i| seg_dist(p, self.quad[i], self.quad[(i + 1) % 4]) < 28.0);
        near.then_some(FtPart::Rotate)
    }
}

fn seg_dist(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let l2 = ab.length_sq();
    let t = if l2 > 1e-9 { ((p - a).dot(ab) / l2).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

/// The points the box covers: the path's selected points, or all of them when fewer than two are
/// (as `path.freeTransform` transforms).
fn ft_points(app: &EffectcraftApp, p: &PathInfo) -> Vec<[f64; 2]> {
    let sel: Vec<usize> = app.session.state.selected_vertices.iter().filter(|v| v.layer == p.layer && v.mask == p.uid).map(|v| v.index).collect();
    let idx: Vec<usize> = if sel.len() < 2 { (0..p.sp.vertices.len()).collect() } else { sel };
    idx.iter().filter_map(|i| p.sp.vertices.get(*i).copied()).collect()
}

/// Draw the free-transform box of the target path (if any) and return it for hit tests: the
/// outline, eight handles and the anchor point's crosshair, as After Effects draws them.
pub(crate) fn draw_free_transform(app: &mut EffectcraftApp, painter: &egui::Painter, map: &ViewerMap, paths: &[PathInfo]) -> Option<FtBox> {
    let st = app.ui.viewer.free_transform.clone()?;
    let (layer, uid) = (LayerId(st.layer), st.path);
    let Some(p) = paths.iter().find(|p| p.layer == layer && p.uid == uid) else {
        end_free_transform(app);
        return None;
    };
    // Screen points per path unit, for the thinnest box side (8 points).
    let o = p.m.apply(gv2(0.0, 0.0));
    let (ex, ey) = (p.m.apply(gv2(1.0, 0.0)), p.m.apply(gv2(0.0, 1.0)));
    let ppu = (((ex.x - o.x).hypot(ex.y - o.y) + (ey.x - o.x).hypot(ey.y - o.y)) / 2.0) * map.zoom as f64;
    let min_side = if ppu > 1e-9 { 8.0 / ppu } else { 1.0 };
    let f = fit_box(&ft_points(app, p), st.angle, min_side)?;
    let to_screen = |uv: [f64; 2]| {
        let q = af_apply(&f, uv);
        let c = p.m.apply(gv2(q[0], q[1]));
        map.to_screen([c.x, c.y])
    };
    let mut handles = [Pos2::ZERO; 8];
    for (h, uv) in handles.iter_mut().zip(FT_HANDLES) {
        *h = to_screen(uv);
    }
    let quad = [handles[0], handles[1], handles[2], handles[3]];
    let anchor = to_screen(st.anchor);
    let b = FtBox { layer, uid, angle: st.angle, anchor_uv: st.anchor, f, m: p.m, handles, quad, anchor };
    let col = app.tokens.accent;
    let outline = Stroke::new(1.0, col);
    painter.add(egui::Shape::closed_line(quad.to_vec(), Stroke::new(3.0, Color32::from_black_alpha(90))));
    painter.add(egui::Shape::closed_line(quad.to_vec(), outline));
    for (i, h) in handles.iter().enumerate() {
        let r = Rect::from_center_size(*h, vec2(7.0, 7.0));
        painter.rect_filled(r, 0.0, Color32::WHITE);
        painter.rect_stroke(r, 0.0, outline, StrokeKind::Outside);
        app.auto.add(&format!("viewer.freeTransform.handle.{i}"), r.expand(2.0), "Free transform handle");
    }
    // The anchor point: a circle with a crosshair through it.
    let shadow = Stroke::new(3.0, Color32::from_black_alpha(110));
    let line = Stroke::new(1.0, Color32::WHITE);
    for s in [shadow, line] {
        painter.circle_stroke(anchor, 5.0, s);
        painter.line_segment([anchor - vec2(10.0, 0.0), anchor + vec2(10.0, 0.0)], s);
        painter.line_segment([anchor - vec2(0.0, 10.0), anchor + vec2(0.0, 10.0)], s);
    }
    app.auto.add("viewer.freeTransform.anchor", Rect::from_center_size(anchor, vec2(16.0, 16.0)), "Free transform anchor point");
    let bounds = Rect::from_points(&quad);
    app.auto.add("viewer.freeTransform.box", bounds, "Free transform box");
    Some(b)
}

fn point_in_quad(p: Pos2, q: &[Pos2; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let c = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if c.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = c.signum();
        } else if c.signum() != sign {
            return false;
        }
    }
    true
}

/// A free-transform drag's mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FtMode {
    Move,
    /// Scale about `pivot` (box coordinates) along the box's axes `axes` (x, y).
    Scale {
        pivot: [f64; 2],
        axes: [bool; 2],
    },
    /// Rotate about the anchor.
    Rotate,
    /// Move the anchor point (no edit).
    Anchor,
}

/// The whole transform (path space) of a free-transform drag from `press` to `cur` (path
/// points) on the box `f0` (uv → path) with the anchor at `anchor_uv`, and the box's new angle
/// change (degrees). `shift` keeps proportions (scale), snaps to 45° (rotate) or to an axis (move).
pub(crate) fn ft_total(mode: FtMode, f0: &Affine, anchor_uv: [f64; 2], press: [f64; 2], cur: [f64; 2], shift: bool) -> Option<(Affine, f64)> {
    match mode {
        FtMode::Anchor => None,
        FtMode::Move => {
            let mut d = [cur[0] - press[0], cur[1] - press[1]];
            if shift {
                if d[0].abs() >= d[1].abs() {
                    d[1] = 0.0;
                } else {
                    d[0] = 0.0;
                }
            }
            Some(([1.0, 0.0, 0.0, 1.0, d[0], d[1]], 0.0))
        }
        FtMode::Scale { pivot, axes } => {
            let inv = af_inv(f0)?;
            let (p, c) = (af_apply(&inv, press), af_apply(&inv, cur));
            let k = |i: usize| {
                let a = p[i] - pivot[i];
                if axes[i] && a.abs() > 1e-9 { ((c[i] - pivot[i]) / a).clamp(-100.0, 100.0) } else { 1.0 }
            };
            let (mut sx, mut sy) = (k(0), k(1));
            if shift {
                // Keep the proportions: the axis that changed most leads.
                let s = if !axes[1] || (axes[0] && (sx - 1.0).abs() >= (sy - 1.0).abs()) { sx } else { sy };
                (sx, sy) = (s, s);
            }
            let nz = |s: f64| if s.abs() < 1e-3 { 1e-3f64.copysign(s) } else { s };
            let (sx, sy) = (nz(sx), nz(sy));
            let s_uv: Affine = [sx, 0.0, 0.0, sy, pivot[0] * (1.0 - sx), pivot[1] * (1.0 - sy)];
            Some((af_mul(f0, &af_mul(&s_uv, &inv)), 0.0))
        }
        FtMode::Rotate => {
            let a = af_apply(f0, anchor_uv);
            let a0 = (press[1] - a[1]).atan2(press[0] - a[0]);
            let a1 = (cur[1] - a[1]).atan2(cur[0] - a[0]);
            let mut d = (a1 - a0).to_degrees();
            if shift {
                d = (d / 45.0).round() * 45.0;
            }
            let (s, c) = d.to_radians().sin_cos();
            let m: Affine = [c, s, -s, c, a[0] - c * a[0] + s * a[1], a[1] - s * a[0] - c * a[1]];
            Some((m, d))
        }
    }
}

/// The anchor's box coordinates after a scale that flipped the box (a negative factor mirrors
/// the refitted box, so the anchor's place in it mirrors too).
pub(crate) fn ft_anchor_after(mode: FtMode, total: &Affine, f0: &Affine, anchor_uv: [f64; 2]) -> [f64; 2] {
    let FtMode::Scale { .. } = mode else { return anchor_uv };
    let Some(inv) = af_inv(f0) else { return anchor_uv };
    // The scale in box coordinates: f0⁻¹ ∘ total ∘ f0 = diag(sx, sy) + offset.
    let s = af_mul(&inv, &af_mul(total, f0));
    [if s[0] < 0.0 { 1.0 - anchor_uv[0] } else { anchor_uv[0] }, if s[3] < 0.0 { 1.0 - anchor_uv[1] } else { anchor_uv[1] }]
}

// ---------------------------------------------------------------- drags

/// Overlay drags owned by this module.
#[derive(Clone, Debug)]
pub(crate) enum Drag {
    /// A motion-path keyframe: its value moves with the pointer (parent space).
    Key { layer: LayerId, prop: u64, time: Tick, start: [f64; 3], press: [f64; 2], inv: Mat3 },
    /// A spatial tangent handle of a motion-path key.
    KeyTangent { layer: LayerId, prop: u64, time: Tick, out: bool, key: [f64; 3], inv: Mat3 },
    /// The motion path between two keys, bent through the pointer (`time` inside the segment,
    /// `u` its Bezier parameter under the press, `last` the parent-space pointer).
    Segment { layer: LayerId, prop: u64, time: Tick, u: f64, last: [f64; 2], inv: Mat3 },
    /// Free transform from `press` (path point) on the box `f0` (uv → path) with the box's angle
    /// and anchor at the press; `applied` is the transform sent so far, `inv` comp → path.
    Ft { layer: LayerId, path: u64, mode: FtMode, f0: Affine, angle0: f64, anchor0: [f64; 2], press: [f64; 2], applied: Affine, inv: Mat3 },
    /// Mask Feather tool: drag a feather point's radius handle (comp → layer matrix `inv`).
    Feather { layer: LayerId, mask: u64, index: usize, inv: Mat3 },
    /// A guide dragged in the viewer.
    Guide { index: usize, vertical: bool },
    /// Drawing the region of interest from `start`; `keep` pins the end's x / y (edge handles).
    Roi { start: [f64; 2], keep: [Option<f64>; 2] },
}

/// Start a motion-path drag at `press` (keys before tangents), selecting the key.
pub(crate) fn begin_key_drag(app: &mut EffectcraftApp, hits: &[KeyHit], press: Pos2, cpt: [f64; 2], shift: bool) -> Option<Drag> {
    let h = hits
        .iter()
        .rev()
        .filter(|h| h.tangent.is_some())
        .find(|h| h.pos.distance(press) < 6.0)
        .or_else(|| hits.iter().rev().find(|h| h.tangent.is_none() && h.pos.distance(press) < 6.0))?;
    let inv = h.p2c.inverse().unwrap_or(Mat3::IDENTITY);
    match h.tangent {
        Some(out) => Some(Drag::KeyTangent { layer: h.layer, prop: h.prop, time: h.time, out, key: h.value, inv }),
        None => {
            let k = KeyRef { layer: h.layer, prop: h.prop, time: h.time };
            if !app.session.state.selected_keys.contains(&k) || shift {
                let _ = app.session.execute("keys.select", json!({"keys": [{"layer": h.layer.0, "prop": h.prop, "time": h.time.seconds()}], "add": shift}));
            }
            Some(Drag::Key { layer: h.layer, prop: h.prop, time: h.time, start: h.value, press: cpt, inv })
        }
    }
}

/// Start bending the motion path between two keys at `press` (comp point `cpt`): the point of
/// the path under the press follows the pointer and only the two keys' facing spatial tangents
/// change; no key is added (a Pen click there adds one, as Add Vertex does in After Effects).
pub(crate) fn begin_segment_bend(app: &EffectcraftApp, segments: &[MotionSegment], press: Pos2, cpt: [f64; 2]) -> Option<Drag> {
    let (seg, _) = segment_near(segments, press)?;
    let inv = seg.p2c.inverse()?;
    let keys = &app.session.active_comp()?.layer(seg.layer)?.props.find(seg.prop)?.keys;
    let i = keys.partition_point(|k| k.time <= seg.start).checked_sub(1)?;
    let p = inv.apply(gv2(cpt[0], cpt[1]));
    let u = effectcraft_engine::keyframe::spatial_segment_param(keys, i, [p.x, p.y])?;
    // A time strictly inside the sample interval names the segment exactly.
    let time = Tick(seg.start.0 / 2 + seg.end.0 / 2);
    Some(Drag::Segment { layer: seg.layer, prop: seg.prop, time, u, last: [p.x, p.y], inv })
}

/// Start a free-transform drag on the box (After Effects): the anchor point moves the pivot; a
/// handle scales about the opposite one (corners both axes, edges one), about the anchor with
/// Cmd; inside moves; just outside rotates about the anchor.
pub(crate) fn begin_ft_drag(f: &FtBox, press: Pos2, map: &ViewerMap, cmd: bool) -> Option<Drag> {
    let inv = f.m.inverse()?;
    let c = map.to_comp(press);
    let lp = inv.apply(gv2(c[0], c[1]));
    let mode = match f.part_at(press)? {
        FtPart::Anchor => FtMode::Anchor,
        FtPart::Handle(i) => {
            let opp = [2, 3, 0, 1, 6, 7, 4, 5].get(i).and_then(|o| FT_HANDLES.get(*o)).copied().unwrap_or([0.5, 0.5]);
            let axes = match i {
                4 | 6 => [false, true],
                5 | 7 => [true, false],
                _ => [true, true],
            };
            FtMode::Scale { pivot: if cmd { f.anchor_uv } else { opp }, axes }
        }
        FtPart::Inside => FtMode::Move,
        FtPart::Rotate => FtMode::Rotate,
    };
    Some(Drag::Ft { layer: f.layer, path: f.uid, mode, f0: f.f, angle0: f.angle, anchor0: f.anchor_uv, press: [lp.x, lp.y], applied: AF_ID, inv })
}

/// Mask Feather tool: press on a feather point's handle to drag it, or on a mask path to add a
/// feather point there and drag its radius out.
pub(crate) fn begin_feather(app: &mut EffectcraftApp, ectx: &EvalCtx, paths: &[PathInfo], map: &ViewerMap, press: Pos2) -> Option<Drag> {
    let _ = ectx;
    for p in paths.iter().filter(|p| p.is_mask) {
        if let Some((i, _)) = p.feather.iter().find(|(_, h)| h.distance(press) < 7.0) {
            return Some(Drag::Feather { layer: p.layer, mask: p.uid, index: *i, inv: p.m.inverse()? });
        }
    }
    let masks: Vec<PathInfo> = paths.iter().filter(|p| p.is_mask).cloned().collect();
    let (layer, uid, seg, t) = segment_at(&masks, map, press, 10.0)?;
    let inv = masks.iter().find(|p| p.layer == layer && p.uid == uid)?.m.inverse()?;
    let r = app.session.execute("mask.featherPoint.add", json!({"layer": layer.0, "mask": uid, "segment": seg, "t": t, "radius": 0}));
    match r {
        Ok(v) => Some(Drag::Feather { layer, mask: uid, index: v["index"].as_u64()? as usize, inv }),
        Err(e) => {
            app.ui.status = e.to_string();
            None
        }
    }
}

/// Advance an overlay drag to the pointer (`cpt` comp, `pos` screen). `snap` corrects a comp
/// point for snapping. Returns the replacement drag state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update(
    app: &mut EffectcraftApp,
    d: &Drag,
    cpt: [f64; 2],
    pos: Pos2,
    mods: egui::Modifiers,
    merge: &str,
    zoom: f32,
    snap: &mut dyn FnMut(&EffectcraftApp, [f64; 2], LayerId) -> [f64; 2],
) -> Option<Drag> {
    match d.clone() {
        Drag::Key { layer, prop, time, start, press, inv } => {
            // Snap the key's on-screen position like a layer drag.
            let start_c = inv.inverse().map(|m| m.apply(gv2(start[0], start[1]))).map(|v| [v.x, v.y]).unwrap_or([start[0], start[1]]);
            let mut target = [start_c[0] + cpt[0] - press[0], start_c[1] + cpt[1] - press[1]];
            let s = snap(app, target, layer);
            target = [target[0] + s[0], target[1] + s[1]];
            let p = inv.apply(gv2(target[0], target[1]));
            let v = json!([p.x, p.y, start[2]]);
            let r = app.session.execute("keys.set", json!({"layer": layer.0, "prop": prop, "time": time.seconds(), "value": v, "merge": merge}));
            if let Err(e) = r {
                app.ui.status = e.to_string();
            }
            None
        }
        Drag::KeyTangent { layer, prop, time, out, key, inv } => {
            let p = inv.apply(gv2(cpt[0], cpt[1]));
            let tg = json!([p.x - key[0], p.y - key[1], 0.0]);
            let mut q = json!({"layer": layer.0, "prop": prop, "time": time.seconds(), "break": mods.alt, "merge": merge});
            q[if out { "out" } else { "in" }] = tg;
            let _ = app.session.execute("keys.setSpatialTangents", q);
            None
        }
        Drag::Segment { layer, prop, time, u, last, inv } => {
            let p = inv.apply(gv2(cpt[0], cpt[1]));
            let cur = [p.x, p.y];
            if cur != last {
                let q = json!({"layer": layer.0, "prop": prop, "time": time.seconds(), "u": u, "offset": [cur[0] - last[0], cur[1] - last[1]], "merge": merge});
                if let Err(e) = app.session.execute("keys.bendSpatialSegment", q) {
                    app.ui.status = e.to_string();
                    return None;
                }
            }
            Some(Drag::Segment { layer, prop, time, u, last: cur, inv })
        }
        Drag::Ft { layer, path, mode, f0, angle0, anchor0, press, applied, inv } => {
            let p = inv.apply(gv2(cpt[0], cpt[1]));
            let cur = [p.x, p.y];
            let same = Drag::Ft { layer, path, mode, f0, angle0, anchor0, press, applied, inv };
            if mode == FtMode::Anchor {
                if let Some(fi) = af_inv(&f0) {
                    let (u0, u1) = (af_apply(&fi, press), af_apply(&fi, cur));
                    set_free_transform(app, None, Some([anchor0[0] + u1[0] - u0[0], anchor0[1] + u1[1] - u0[1]]));
                }
                return Some(same);
            }
            let (total, turn) = ft_total(mode, &f0, anchor0, press, cur, mods.shift)?;
            let delta = af_mul(&total, &af_inv(&applied)?);
            if delta.iter().zip(AF_ID).any(|(a, b)| (a - b).abs() > 1e-9) {
                let q = json!({"layer": layer.0, "mask": path, "matrix": delta, "merge": merge});
                if let Err(e) = app.session.execute("path.freeTransform", q) {
                    app.ui.status = e.to_string();
                    return None;
                }
            }
            set_free_transform(app, Some(angle0 + turn), Some(ft_anchor_after(mode, &total, &f0, anchor0)));
            Some(Drag::Ft { layer, path, mode, f0, angle0, anchor0, press, applied: total, inv })
        }
        Drag::Feather { layer, mask, index, inv } => {
            let p = inv.apply(gv2(cpt[0], cpt[1]));
            let _ = (pos, zoom);
            let q = json!({"layer": layer.0, "mask": mask, "index": index, "toward": [p.x, p.y], "merge": merge});
            if let Err(e) = app.session.execute("mask.featherPoint.set", q) {
                app.ui.status = e.to_string();
            }
            None
        }
        Drag::Guide { index, .. } => {
            let _ = index;
            None
        }
        Drag::Roi { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(vertices: Vec<[f64; 2]>, closed: bool) -> PathInfo {
        let n = vertices.len();
        let sp = ShapePath { vertices, in_tangents: vec![[0.0; 2]; n], out_tangents: vec![[0.0; 2]; n], closed, feather: Vec::new() };
        PathInfo { layer: LayerId(3), uid: 7, m: Mat3::IDENTITY, sp, is_mask: true, feather: Vec::new() }
    }

    /// The free-transform box fits the points in its own (turned) frame, widens a line of points
    /// so its handles stay apart, and a scale from a handle maps the grabbed point onto the pointer.
    #[test]
    fn free_transform_box_fits_turns_and_scales() {
        let near = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6;
        let pts = [[10.0, 20.0], [50.0, 20.0], [50.0, 80.0]];
        let f = fit_box(&pts, 0.0, 1.0).unwrap();
        assert!(near(af_apply(&f, [0.0, 0.0]), [10.0, 20.0]) && near(af_apply(&f, [1.0, 1.0]), [50.0, 80.0]));
        // A vertical line of points: the box gets the minimum width about the line.
        let line = fit_box(&[[5.0, 0.0], [5.0, 100.0]], 0.0, 8.0).unwrap();
        assert!(near(af_apply(&line, [0.0, 0.0]), [1.0, 0.0]) && near(af_apply(&line, [1.0, 1.0]), [9.0, 100.0]));
        // Turned 90°: the box's first corner follows the turn.
        let t = fit_box(&pts, 90.0, 1.0).unwrap();
        for p in pts {
            let uv = af_apply(&af_inv(&t).unwrap(), p);
            assert!((-1e-9..=1.0 + 1e-9).contains(&uv[0]) && (-1e-9..=1.0 + 1e-9).contains(&uv[1]), "{uv:?}");
        }
        assert!(fit_box(&[], 0.0, 1.0).is_none());
        // Bottom-right handle dragged from (50, 80) to (90, 140), pivot the first corner.
        let mode = FtMode::Scale { pivot: [0.0, 0.0], axes: [true, true] };
        let (m, turn) = ft_total(mode, &f, [0.5, 0.5], [50.0, 80.0], [90.0, 140.0], false).unwrap();
        assert!(near(af_apply(&m, [50.0, 80.0]), [90.0, 140.0]) && near(af_apply(&m, [10.0, 20.0]), [10.0, 20.0]) && turn == 0.0);
        // Past the pivot: the box flips and the anchor's place in it mirrors.
        let (m, _) = ft_total(mode, &f, [0.25, 0.5], [50.0, 80.0], [-30.0, 80.0], false).unwrap();
        assert_eq!(ft_anchor_after(mode, &m, &f, [0.25, 0.5]), [0.75, 0.5]);
        // Shift on a rotation snaps to 45°.
        let (_, turn) = ft_total(FtMode::Rotate, &f, [0.5, 0.5], [60.0, 50.0], [30.0, 75.0], true).unwrap();
        assert_eq!(turn.rem_euclid(45.0), 0.0);
        assert!(ft_total(FtMode::Anchor, &f, [0.5, 0.5], [0.0; 2], [1.0; 2], false).is_none());
    }

    /// #912: clicking the last vertex of an open path with the Pen resumes that path.
    #[test]
    fn pen_resumes_an_open_path_at_its_last_vertex() {
        let map = ViewerMap { origin: Pos2::ZERO, zoom: 1.0, comp: [100.0, 100.0], area: Rect::EVERYTHING };
        let open = info(vec![[10.0, 10.0], [50.0, 10.0], [50.0, 50.0]], false);
        assert_eq!(open_end_at(std::slice::from_ref(&open), &map, egui::pos2(52.0, 49.0), 8.0), Some((LayerId(3), 7)));
        assert_eq!(open_end_at(std::slice::from_ref(&open), &map, egui::pos2(10.0, 10.0), 8.0), None, "first vertex");
        assert_eq!(open_end_at(std::slice::from_ref(&open), &map, egui::pos2(80.0, 80.0), 8.0), None, "far away");
        let closed = info(vec![[10.0, 10.0], [50.0, 10.0], [50.0, 50.0]], true);
        assert_eq!(open_end_at(&[closed], &map, egui::pos2(50.0, 50.0), 8.0), None, "closed path");
    }

    /// The Pen on a motion path: one decision for the pointer, the click and the drag. A key's
    /// square wins, then a tangent handle (they lie on the line of a straight path), then the
    /// line itself, which adds a key on the nearest frame that has none; between keys on
    /// adjacent frames there is nothing to add.
    #[test]
    fn pen_target_picks_key_then_handle_then_free_frame() {
        let (l, p) = (LayerId(1), 7);
        let fd = Tick::from_seconds_f64(1.0 / 30.0);
        let t = |f: i64| Tick(fd.0 * f);
        let key = |f: i64, x: f32, tangent: Option<bool>| KeyHit {
            layer: l,
            prop: p,
            time: t(f),
            pos: egui::pos2(x, 100.0),
            tangent,
            value: [0.0; 3],
            p2c: Mat3::IDENTITY,
            curved: true,
        };
        // Keys on frames 0, 3 and 4 (20 pt a frame); frame 0's out handle at x = 12.
        let hits = vec![key(0, 0.0, None), key(0, 12.0, Some(true)), key(3, 60.0, None), key(4, 80.0, None)];
        let frames: Vec<PathFrame> = (0..=4).map(|f| PathFrame { layer: l, prop: p, time: t(f), pos: egui::pos2(f as f32 * 20.0, 100.0) }).collect();
        let segments: Vec<MotionSegment> = (0..4)
            .map(|f| MotionSegment {
                layer: l,
                prop: p,
                from: egui::pos2(f as f32 * 20.0, 100.0),
                to: egui::pos2(f as f32 * 20.0 + 20.0, 100.0),
                start: t(f),
                end: t(f + 1),
                p2c: Mat3::IDENTITY,
            })
            .collect();
        let at = |x: f32, y: f32| pen_target(&hits, &segments, &frames, egui::pos2(x, y));
        assert!(matches!(at(1.0, 101.0), PenTarget::Key(k) if k.time == t(0)));
        assert!(matches!(at(13.0, 100.0), PenTarget::Tangent), "the handle on the line");
        assert!(matches!(at(19.0, 100.0), PenTarget::AddFrame(f) if f.time == t(1)), "past the handle's reach: nearest frame");
        assert!(matches!(at(37.0, 100.0), PenTarget::AddFrame(f) if f.time == t(2)), "nearest frame");
        assert!(matches!(at(70.0, 100.0), PenTarget::PathFull), "keys on adjacent frames");
        assert!(matches!(at(40.0, 120.0), PenTarget::None), "off the path");
    }
}

// ── Claude's version and suggestion previews (batch 1, CX4 / CX5) ─────────────────────────────

static ALT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A fresh number for another scratch document (its frames never share a key with the document's).
pub(crate) fn next_alt_epoch() -> u64 {
    ALT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Frame `t` of comp `cid` in another document (`project`: a suggestion's scratch copy, Claude's
/// version), rendered like the viewer's own (CPU pixels), keyed apart by `epoch`.
pub(crate) fn alt_texture(
    app: &EffectcraftApp,
    ctx: &egui::Context,
    project: &std::sync::Arc<effectcraft_engine::project::Project>,
    epoch: u64,
    cid: effectcraft_engine::project::ItemId,
    t: Tick,
    scale: f64,
) -> Option<egui::TextureHandle> {
    use crate::frames::{FrameImage, FrameKey, RenderSource};
    use std::hash::{Hash, Hasher};
    let c = project.comp(cid)?;
    let frame = c.frame_rate.frame_at(t);
    let opts = app.frame_opts(cid, scale);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (crate::frames::comp_content(project, cid), 0xC1A0_DE00_u64, epoch).hash(&mut h);
    let key = FrameKey {
        // The document's revision: the frame queue drops jobs of older revisions.
        revision: app.session.revision,
        content: h.finish(),
        comp: cid.0,
        frame,
        scale: (scale * 1000.0).round() as u32,
        view: app.view_hash(cid),
        opts: crate::frames::opts_hash(&opts),
    };
    let id = egui::Id::new(("agent-compare-tex", epoch));
    let cached: Option<(FrameKey, egui::TextureHandle)> = ctx.data(|d| d.get_temp(id));
    if let Some((k, tex)) = &cached
        && *k == key
    {
        return Some(tex.clone());
    }
    // TODO(batch1-verify): RenderSource's other fields (freeze feedback, layer cache) with another project.
    let src = RenderSource { gpu_display: false, project: project.clone(), ..app.render_source() };
    app.frames.request_urgent(&src, key, cid, c.frame_rate.tick_of(frame), opts);
    match app.frames.get(&key) {
        Some(FrameImage::Cpu(img)) => {
            let tex = crate::frames::load_fitted(ctx, "agent-compare", (*img).clone(), egui::TextureOptions::LINEAR);
            ctx.data_mut(|d| d.insert_temp(id, (key, tex.clone())));
            Some(tex)
        }
        _ => {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
            cached.map(|(_, tex)| tex)
        }
    }
}

/// The scratch copy for suggestion `id` at the document's revision (made once, then kept).
fn suggestion_project(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str) -> Option<(std::sync::Arc<effectcraft_engine::project::Project>, u64)> {
    type Cached = (String, u64, std::sync::Arc<effectcraft_engine::project::Project>, u64);
    let cid = egui::Id::new("suggestion-preview-project");
    let rev = app.session.revision;
    if let Some((i, r, p, e)) = ctx.data(|d| d.get_temp::<Cached>(cid))
        && i == id
        && r == rev
    {
        return Some((p, e));
    }
    match effectcraft_engine::commands::suggestions::preview(&mut app.session, id) {
        Ok(p) => {
            let e = next_alt_epoch();
            ctx.data_mut(|d| d.insert_temp::<Cached>(cid, (id.to_string(), rev, p.clone(), e)));
            Some((p, e))
        }
        Err(e) => {
            app.ui.status = format!("Can't preview this suggestion: {e}");
            None
        }
    }
}

/// Over the viewer's frame: a hovered suggestion (its range looping) or, while `\` or the header
/// button is held, Claude's version; and the corner label saying which one is on screen.
pub(crate) fn agent_compare(
    app: &mut EffectcraftApp,
    ctx: &egui::Context,
    painter: &egui::Painter,
    comp_rect: Rect,
    area: Rect,
    cid: effectcraft_engine::project::ItemId,
    scale: f64,
) {
    let blue = super::suggestions::CLAUDE_BLUE;
    let green = Color32::from_rgb(0x8f, 0xe0, 0xb4);
    let mut label: Option<(String, Color32)> = None;
    // CX4: the hovered suggestion, on a scratch copy, its range looping.
    if let Some((id, since)) = super::suggestions::hovered(ctx) {
        let sg = effectcraft_engine::commands::suggestions::list(&app.session).into_iter().find(|s| s.id == id);
        if let Some(sg) = sg
            && let Some((proj, epoch)) = suggestion_project(app, ctx, &id)
        {
            let now = ctx.input(|i| i.time);
            let t = match sg.range {
                Some([a, b]) if b > a => a + (now - since).max(0.0) % (b - a),
                _ => app.session.time().seconds(),
            };
            if let Some(tex) = alt_texture(app, ctx, &proj, epoch, cid, Tick::from_seconds_f64(t), scale) {
                painter.image(tex.id(), comp_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            ctx.request_repaint();
            label = Some((format!("SUGGESTION {} · ✓ to keep", sg.n), blue));
        }
    }
    // CX5: Claude's version while `\` or the button is held.
    let claude = effectcraft_engine::commands::suggestions::claude_version(&app.session);
    if let Some((proj, by)) = &claude {
        // The button, top right of the viewer: held = Claude's version.
        let b = Rect::from_min_size(egui::pos2(area.max.x - 196.0, area.min.y + 8.0), vec2(188.0, 22.0));
        let resp = ui_button_hold(ctx, b);
        let hot = resp.0;
        painter.rect_filled(b, 4.0, Color32::from_black_alpha(if hot { 170 } else { 120 }));
        painter.text(b.center(), egui::Align2::CENTER_CENTER, "Hold: Claude’s version  \\", crate::theme::Tokens::ui(11.0), blue);
        app.auto.add("viewer.compare.hold", b, "Hold: Claude's version");
        if resp.1 != super::suggestions::flipped(ctx) && (resp.1 || resp.2) {
            super::suggestions::set_flipped(ctx, resp.1);
        }
        if label.is_none() && super::suggestions::flipped(ctx) && !std::sync::Arc::ptr_eq(proj, &app.session.project) {
            let e = claude_epoch(ctx, proj);
            if let Some(tex) = alt_texture(app, ctx, proj, e, cid, app.session.time(), scale) {
                painter.image(tex.id(), comp_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            let who = by.as_deref().unwrap_or("Claude");
            label = Some((format!("CLAUDE’S VERSION · {who}"), blue));
        } else if label.is_none() && super::suggestions::flipped(ctx) {
            label = Some(("CLAUDE’S VERSION (no edits since)".into(), blue));
        }
    }
    if label.is_none() && (claude.is_some() || !effectcraft_engine::commands::suggestions::list(&app.session).is_empty()) {
        label = Some(("YOURS".into(), green));
    }
    if let Some((text, color)) = label {
        let g = painter.layout_no_wrap(text.clone(), crate::theme::Tokens::mono(11.0), color);
        let r = Rect::from_min_size(comp_rect.min + vec2(8.0, 8.0), g.size() + vec2(16.0, 6.0));
        painter.rect_filled(r, 4.0, Color32::from_black_alpha(140));
        painter.galley(r.min + vec2(8.0, 3.0), g, color);
        app.auto.add("viewer.compare.label", r, &text);
    }
}

/// The frame epoch of Claude's version `proj` (one per snapshot).
fn claude_epoch(ctx: &egui::Context, proj: &std::sync::Arc<effectcraft_engine::project::Project>) -> u64 {
    let id = egui::Id::new("claude-version-epoch");
    let ptr = std::sync::Arc::as_ptr(proj) as usize;
    if let Some((p, e)) = ctx.data(|d| d.get_temp::<(usize, u64)>(id))
        && p == ptr
    {
        return e;
    }
    let e = next_alt_epoch();
    ctx.data_mut(|d| d.insert_temp(id, (ptr, e)));
    e
}

/// A press-and-hold area: (hovered, held now, released this frame).
fn ui_button_hold(ctx: &egui::Context, r: Rect) -> (bool, bool, bool) {
    let (pos, down, released) = ctx.input(|i| (i.pointer.interact_pos(), i.pointer.primary_down(), i.pointer.primary_released()));
    let over = pos.is_some_and(|p| r.contains(p));
    let id = egui::Id::new("viewer-compare-held");
    let was: bool = ctx.data(|d| d.get_temp(id)).unwrap_or(false);
    let held = if was { down } else { down && over && ctx.input(|i| i.pointer.primary_pressed()) };
    ctx.data_mut(|d| d.insert_temp(id, held));
    // TODO(batch1-verify): the viewer's own press handling also sees this press (a click-select under the button).
    (over, held, was && (released || !down))
}
