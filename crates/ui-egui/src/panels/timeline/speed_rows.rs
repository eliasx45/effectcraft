//! Speed curves in the property rows (Timeline ▸ footer ∿ toggle, on by default): every eased
//! segment between two keys draws its speed curve inside the row, scaled to that segment's own
//! peak, with a dot on the peak (hover: speed, time and influences). Linear and hold segments
//! draw nothing. Editing in place: a yellow handle at each Bezier side, at the height of the
//! speed there, as the Graph Editor's speed handles: dragging it sideways changes the influence,
//! up or down the speed (velocity) at the key. ⌘-drag anywhere on the curve sculpts the side
//! nearer the pointer (left of the peak: the first key's out influence, right of it: the second
//! key's in influence), keeping the speeds. The same side of the selected keys of other
//! properties follows the dragged handle, but not the property's own other keys (no mirrored
//! handle across the segment) (`keys.setEaseSelected`); ⇧ at the press moves the grabbed handle
//! alone. The handle
//! nearest the pointer (within [`HANDLE_PICK`]) is the one that answers, and stays so until
//! another is clearly nearer, so the cursor doesn't flicker between handles and the curve.

use effectcraft_engine::keyframe::{Interp, side_ease, speed};
use effectcraft_engine::project::{Comp, Layer, ParamUi, Property};
use effectcraft_engine::time::{TICKS_PER_SECOND, Tick};
use egui::{Color32, Painter, Pos2, Rect, Sense, Shape, Stroke, pos2, vec2};
use serde_json::json;

use super::TMap;
use crate::panels::timeline::TimelineHost;
use crate::panels::timeline::ids::tl_id;

/// The Graph Editor's speed-curve yellow.
const CURVE: Color32 = Color32::from_rgb(226, 201, 79);
const HANDLE: Color32 = Color32::from_rgb(255, 227, 106);
/// 100% influence reaches this fraction of the segment, as the Graph Editor's speed handles.
const HANDLE_REACH: f64 = 0.5;
/// Influence is kept in this range (%), as `keys.setEase` and the Graph Editor.
const MIN_INFLUENCE: f64 = 0.1;
const MAX_INFLUENCE: f64 = 100.0;
/// A handle answers the pointer within this distance (points) when it is the nearest...
const HANDLE_PICK: f32 = 9.0;
/// ...and the one answering keeps answering until the pointer is this much farther from it
/// than from another (or leaves its reach).
const HANDLE_HYSTERESIS: f32 = 4.0;

/// One eased segment as drawn: its keys' comp-time x, the sampled curve (x, speed) and its peak.
struct Segment {
    i: usize,
    xa: f32,
    xb: f32,
    /// (x, speed) samples, left to right.
    pts: Vec<(f32, f64)>,
    peak: f64,
    peak_x: f32,
    peak_t: Tick,
    /// Out influence of key `i`, in influence of key `i + 1` (0..1); `None` for a linear side.
    out_inf: Option<f64>,
    in_inf: Option<f64>,
}

impl Segment {
    /// The speed shown at the key on one side (the curve's end there).
    fn speed_at(&self, out: bool) -> f64 {
        let end = if out { self.pts.first() } else { self.pts.last() };
        end.map_or(0.0, |(_, v)| *v)
    }

    /// The average of the speed shown (its units per second).
    fn mean(&self) -> f64 {
        if self.pts.is_empty() { 0.0 } else { self.pts.iter().map(|(_, v)| v).sum::<f64>() / self.pts.len() as f64 }
    }
}

/// A handle or sculpt drag in progress (egui memory, per property).
#[derive(Clone, Copy)]
struct Grab {
    /// Key index whose side moves, and which side.
    key: usize,
    out: bool,
    /// Pointer x and influence (%) at the press; pixels per 100% influence.
    x0: f32,
    inf0: f64,
    reach_px: f32,
    /// A handle drag's speed: `None` for a sculpt (influence only).
    speed: Option<SpeedGrab>,
    /// ⇧ at the press: the grabbed handle moves alone.
    alone: bool,
}

/// What a handle's vertical drag starts from.
#[derive(Clone, Copy)]
struct SpeedGrab {
    /// Pointer y at the press; the segment's peak and the row's height then, which keep the
    /// vertical scale for the whole drag.
    y0: f32,
    peak: f64,
    amp: f32,
    /// The side's stored speed (first dimension) at the press.
    ease: f64,
    /// +1 or -1: the way the value moves on that side, so up always means faster.
    dir: f64,
    /// Stored speed units per shown speed unit (1 for one-dimensional values).
    ratio: f64,
}

fn grab_id(uid: u64) -> egui::Id {
    tl_id(("tl-speed-grab", uid))
}

/// The handle (key, out) answering the pointer in a property's row.
fn hot_id(uid: u64) -> egui::Id {
    tl_id(("tl-speed-hot", uid))
}

/// One handle as drawn: its segment (index into the drawn segments), key and side, the key's x,
/// where it is and its influence (0..1).
struct HandleAt {
    seg: usize,
    key: usize,
    out: bool,
    kx: f32,
    pos: Pos2,
    inf: f64,
}

/// The handle that answers a pointer at `ptr`: the nearest within [`HANDLE_PICK`], except that
/// `hot` (the one answering until now) keeps answering while it is within reach and no other
/// is [`HANDLE_HYSTERESIS`] nearer.
fn pick(handles: &[HandleAt], ptr: Pos2, hot: Option<(usize, bool)>) -> Option<(usize, bool)> {
    let dist = |h: &HandleAt| h.pos.distance(ptr);
    let nearest = handles.iter().filter(|h| dist(h) <= HANDLE_PICK).min_by(|a, b| dist(a).total_cmp(&dist(b)));
    let kept = hot.and_then(|(k, o)| handles.iter().find(|h| h.key == k && h.out == o)).filter(|h| dist(h) <= HANDLE_PICK + HANDLE_HYSTERESIS);
    match (kept, nearest) {
        (Some(k), Some(n)) if dist(n) + HANDLE_HYSTERESIS < dist(k) => Some((n.key, n.out)),
        (Some(k), _) => Some((k.key, k.out)),
        (None, n) => n.map(|n| (n.key, n.out)),
    }
}

/// Speed units for the readout (as the Graph Editor's speed graph).
fn units(pr: &Property) -> &'static str {
    match pr.ui {
        ParamUi::Angle => "°/s",
        ParamUi::Percent => "%/s",
        ParamUi::Pixels => "px/s",
        _ if pr.spatial => "px/s",
        _ if pr.match_id == "opacity" => "%/s",
        _ => "/s",
    }
}

/// Whether segment `i` → `i + 1` changes speed: not hold, not linear on both sides, and of a
/// value that interpolates.
fn eased(prop: &Property, i: usize) -> bool {
    let (Some(a), Some(b)) = (prop.keys.get(i), prop.keys.get(i + 1)) else { return false };
    if a.out_interp == Interp::Hold || b.in_interp == Interp::Hold || !a.value.interpolates() {
        return false;
    }
    !(a.out_interp == Interp::Linear && b.in_interp == Interp::Linear)
}

fn influence(prop: &Property, i: usize, out: bool) -> Option<f64> {
    let k = prop.keys.get(i)?;
    if (if out { k.out_interp } else { k.in_interp }) != Interp::Bezier {
        return None;
    }
    side_ease(&prop.keys, i, 0, prop.spatial, out).map(|e| e.influence.clamp(0.0, 1.0))
}

/// The visible eased segments of `prop`, sampled at about one point per 2 px.
fn segments(prop: &Property, to_comp: &dyn Fn(Tick) -> Tick, tm: TMap, x_min: f32, x_max: f32) -> Vec<Segment> {
    let mut out = vec![];
    for i in 0..prop.keys.len().saturating_sub(1) {
        if !eased(prop, i) {
            continue;
        }
        let (Some(a), Some(b)) = (prop.keys.get(i), prop.keys.get(i + 1)) else { continue };
        let xa = tm.x(to_comp(a.time).seconds());
        let xb = tm.x(to_comp(b.time).seconds());
        if xb < x_min || xa > x_max || xb - xa < 6.0 {
            continue;
        }
        let n = (((xb - xa) / 2.0) as usize).clamp(8, 240);
        // The numeric speed differentiates over ±1 ms: stay that far inside the segment.
        let h = Tick(TICKS_PER_SECOND / 500);
        let (t0, t1) = (a.time.0.saturating_add(h.0), b.time.0.saturating_sub(h.0));
        if t1 <= t0 {
            continue;
        }
        let mut pts = Vec::with_capacity(n + 1);
        let (mut peak, mut peak_x, mut peak_t) = (0.0f64, xa, a.time);
        for j in 0..=n {
            let s = j as f64 / n as f64;
            let t = Tick(t0 + ((t1 - t0) as f64 * s) as i64);
            let v = speed(&prop.keys, t, prop.spatial);
            let x = xa + (xb - xa) * s as f32;
            if v.is_finite() && v > peak {
                (peak, peak_x, peak_t) = (v, x, t);
            }
            pts.push((x, if v.is_finite() { v } else { 0.0 }));
        }
        if peak <= 1e-9 {
            continue;
        }
        out.push(Segment { i, xa, xb, pts, peak, peak_x, peak_t, out_inf: influence(prop, i, true), in_inf: influence(prop, i + 1, false) });
    }
    out
}

/// Draws `prop`'s speed curves and handles in `strip` (the row's time-graph span, one row high)
/// and handles their drags, pushing `keys.setEase` (merged into one undo step per drag).
#[allow(clippy::too_many_arguments)]
pub(crate) fn show(
    app: &mut dyn TimelineHost,
    ui: &egui::Ui,
    p: &Painter,
    comp: &Comp,
    tm: TMap,
    layer: &Layer,
    prop: &Property,
    strip: Rect,
    actions: &mut Vec<(String, serde_json::Value)>,
) {
    if !app.ui().timeline.speed_rows || prop.keys.len() < 2 {
        return;
    }
    let segs = segments(prop, &|t| layer.comp_time(t), tm, strip.min.x, strip.max.x);
    let uid = prop.uid;
    let ctx = ui.ctx().clone();
    let (top, base) = (strip.min.y + 2.0, strip.max.y - 1.5);
    let amp = (base - top).max(1.0);
    let mut grab: Option<Grab> = ctx.data(|d| d.get_temp(grab_id(uid)));
    // A released drag (this runs before the handle sees its release, and also catches one
    // released where this row wasn't drawn) leaves no grab and no open undo merge behind: an
    // open merge made the next handle drag start from before this one, undoing it (23 Oct 2026).
    if grab.is_some() && !ui.input(|i| i.pointer.any_down()) {
        ctx.data_mut(|d| d.remove::<Grab>(grab_id(uid)));
        grab = None;
        actions.push(("__endMerge".into(), json!({})));
    }
    let (cmd, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
    let unit = units(prop);
    // Locked layers show their curves but take no edits.
    let editable = !layer.switches.locked;
    let mut handles: Vec<HandleAt> = vec![];
    for (si, s) in segs.iter().enumerate() {
        // A handle being dragged keeps its segment's scale from the press.
        let peak = grab.filter(|g| if g.out { g.key == s.i } else { g.key == s.i + 1 }).and_then(|g| g.speed).map_or(s.peak, |sg| sg.peak);
        let y = |v: f64| base - (v / peak.max(1e-9)).clamp(0.0, 1.0) as f32 * amp;
        let line: Vec<Pos2> = s.pts.iter().map(|(x, v)| pos2(*x, y(*v))).collect();
        // Faint fill under the curve, then the curve.
        let fill = Color32::from_rgba_unmultiplied(CURVE.r(), CURVE.g(), CURVE.b(), 34);
        for w in line.windows(2) {
            let [a, b] = [w[0], w[1]];
            p.add(Shape::convex_polygon(vec![a, b, pos2(b.x, base), pos2(a.x, base)], fill, Stroke::NONE));
        }
        p.add(Shape::line(line, Stroke::new(1.2, CURVE.gamma_multiply(0.8))));
        // Peak dot and its readout.
        let dot = pos2(s.peak_x, top + 0.5);
        p.circle(dot, 3.2, HANDLE, Stroke::new(1.0, Color32::from_gray(20)));
        let infl = |v: Option<f64>| v.map_or("linear".to_string(), |v| format!("{:.0}%", v * 100.0));
        let tip = format!(
            "{:.1} {unit} @ {}  ·  out {} · in {}",
            s.peak,
            crate::panels::timecode(app.session(), comp, layer.comp_time(s.peak_t)),
            infl(s.out_inf),
            infl(s.in_inf)
        );
        let dr = Rect::from_center_size(dot, vec2(10.0, 10.0));
        let dresp = ui.interact(dr, tl_id(("tl-speed-peak", uid, s.i)), Sense::hover());
        app.auto_mut().add(&format!("timeline.speed.{uid}.{}.peak", s.i), dr, &tip);
        if dresp.hovered() {
            egui::Tooltip::always_open(ctx.clone(), ui.layer_id(), tl_id("tl-speed-tip"), egui::PopupAnchor::Pointer).show(|ui| ui.label(&tip));
        }
        // ⌘-drag on the curve: sculpt the side nearer the pointer.
        let sculpt_rect = Rect::from_min_max(pos2(s.xa + 7.0, strip.min.y), pos2(s.xb - 7.0, strip.max.y));
        let sculpt_id = tl_id(("tl-speed-sculpt", uid, s.i));
        if editable && sculpt_rect.width() > 4.0 && (cmd || grab.is_some_and(|g| g.key == s.i || g.key == s.i + 1)) {
            let resp = ui.interact(sculpt_rect, sculpt_id, Sense::drag());
            app.auto_mut().add(&format!("timeline.speed.{uid}.{}.sculpt", s.i), sculpt_rect, "⌘-drag to sculpt the speed curve");
            if resp.hovered() || resp.dragged() {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
            if resp.drag_started()
                && let Some(px) = resp.interact_pointer_pos()
            {
                let out = px.x < s.peak_x;
                let (key, inf) = if out { (s.i, s.out_inf) } else { (s.i + 1, s.in_inf) };
                let reach_px = ((s.xb - s.xa) as f64 * HANDLE_REACH) as f32;
                let g = Grab { key, out, x0: px.x, inf0: inf.unwrap_or(1.0 / 6.0) * 100.0, reach_px, speed: None, alone: shift };
                ctx.data_mut(|d| d.insert_temp(grab_id(uid), g));
                grab = Some(g);
            }
            drag(&ctx, &resp, prop, layer, grab, actions, |g, px| {
                (g.inf0 + ((px.x - g.x0) / g.reach_px.max(1.0)) as f64 * 100.0 * if g.out { 1.0 } else { -1.0 }, None)
            });
        }
        // Handles at the speed of each Bezier side: sideways influence, up and down speed.
        let reach = (s.xb - s.xa) * HANDLE_REACH as f32;
        for (out, key, inf, kx) in [(true, s.i, s.out_inf, s.xa), (false, s.i + 1, s.in_inf, s.xb)] {
            let Some(inf) = inf else { continue };
            let hx = if out { kx + reach * inf as f32 } else { kx - reach * inf as f32 };
            let hy = y(s.speed_at(out));
            p.line_segment([pos2(kx, hy), pos2(hx, hy)], Stroke::new(1.5, HANDLE));
            let hr = Rect::from_center_size(pos2(hx, hy), vec2(5.0, 5.0));
            p.rect(hr, 0.0, HANDLE, Stroke::new(1.0, Color32::from_gray(20)), egui::StrokeKind::Inside);
            handles.push(HandleAt { seg: si, key, out, kx, pos: pos2(hx, hy.min(base - 2.0)), inf });
        }
    }
    if !editable {
        return;
    }
    // One handle answers at a time: the one being dragged, else the one the pointer is nearest.
    let dragged = grab.filter(|g| g.speed.is_some()).map(|g| (g.key, g.out));
    let hot = dragged.or_else(|| {
        let ptr = ui.input(|i| i.pointer.hover_pos()).filter(|p| strip.expand(HANDLE_PICK).contains(*p))?;
        pick(&handles, ptr, ctx.data(|d| d.get_temp(hot_id(uid))))
    });
    ctx.data_mut(|d| match hot {
        Some(h) => {
            d.insert_temp(hot_id(uid), h);
        }
        None => d.remove::<(usize, bool)>(hot_id(uid)),
    });
    for h in &handles {
        app.auto_mut().add(
            &format!("timeline.speed.{uid}.{}.{}", h.key, if h.out { "out" } else { "in" }),
            Rect::from_center_size(h.pos, vec2(12.0, 12.0)),
            &prop.name,
        );
    }
    let Some(h) = hot.and_then(|(k, o)| handles.iter().find(|h| h.key == k && h.out == o)) else { return };
    let Some(s) = segs.get(h.seg) else { return };
    let (key, out, kx, inf) = (h.key, h.out, h.kx, h.inf);
    let reach = (s.xb - s.xa) * HANDLE_REACH as f32;
    // Big enough for the pick distance; only this handle has one, so they never overlap.
    let hit = Rect::from_center_size(h.pos, vec2(2.0 * HANDLE_PICK, 2.0 * HANDLE_PICK));
    let resp = ui.interact(hit, tl_id(("tl-speed-handle", uid, key, out)), Sense::drag());
    let side = if out { "out" } else { "in" };
    let shown = s.speed_at(out);
    let tip = format!("{side} influence {:.0}% · speed {shown:.1} {unit}", inf * 100.0);
    if resp.hovered() || resp.dragged() {
        ctx.set_cursor_icon(egui::CursorIcon::Move);
        if !resp.dragged() {
            let _ = resp.clone().on_hover_text(format!("{tip}\nDrag sideways: influence · up/down: speed · ⇧: only this handle"));
        }
    }
    if resp.drag_started()
        && let Some(press) = resp.interact_pointer_pos()
    {
        // The handle follows the pointer: influence from the key's x, speed from how far
        // the pointer moved up or down at the press's scale.
        let ease = side_ease(&prop.keys, key, 0, prop.spatial, out).map_or(0.0, |e| e.speed);
        let lead = seg_velocity(prop, key, out);
        let dir = if lead < 0.0 { -1.0 } else { 1.0 };
        let mean = s.mean();
        let ratio = if mean > 1e-9 && lead.abs() > 1e-9 { lead.abs() / mean } else { 1.0 };
        let sg = SpeedGrab { y0: press.y, peak: s.peak, amp, ease, dir, ratio };
        let g = Grab { key, out, x0: kx, inf0: 0.0, reach_px: reach, speed: Some(sg), alone: shift };
        ctx.data_mut(|d| d.insert_temp(grab_id(uid), g));
        grab = Some(g);
    }
    if grab.is_some_and(|g| g.key == key && g.out == out) {
        drag(&ctx, &resp, prop, layer, grab, actions, |g, px| {
            let inf = ((px.x - g.x0) / g.reach_px.max(1.0)) as f64 * 100.0 * if g.out { 1.0 } else { -1.0 };
            let speed = g.speed.map(|sg| {
                let more = f64::from(sg.y0 - px.y) / f64::from(sg.amp.max(1.0)) * sg.peak;
                let v = sg.ease + sg.dir * more * sg.ratio;
                // Down to a stop, never backwards.
                if v * sg.dir < 0.0 { 0.0 } else { v }
            });
            (inf, speed)
        });
    }
}

/// Average velocity of the first dimension (path speed for a spatial value) on one side of key
/// `i`, signed the way the value moves.
fn seg_velocity(prop: &Property, i: usize, out: bool) -> f64 {
    let j = if out { i.checked_add(1) } else { i.checked_sub(1) };
    let (Some(k), Some(n)) = (prop.keys.get(i), j.and_then(|j| prop.keys.get(j))) else { return 0.0 };
    let dur = (n.time.seconds() - k.time.seconds()).abs();
    if dur <= 0.0 {
        return 0.0;
    }
    let (a, b) = (k.value.components(), n.value.components());
    if prop.spatial && a.len() > 1 {
        return a.iter().zip(&b).map(|(x, y)| (y - x).powi(2)).sum::<f64>().sqrt() / dur;
    }
    let dv = b.first().copied().unwrap_or(0.0) - a.first().copied().unwrap_or(0.0);
    if out { dv / dur } else { -dv / dur }
}

/// Follows a handle or sculpt drag: `keys.setEaseSelected` (other properties' selected keys follow,
/// none with ⇧) with
/// the influence and speed `f` gives for the pointer; on release ends the undo merge and forgets
/// the grab.
fn drag(
    ctx: &egui::Context,
    resp: &egui::Response,
    prop: &Property,
    layer: &Layer,
    grab: Option<Grab>,
    actions: &mut Vec<(String, serde_json::Value)>,
    f: impl Fn(&Grab, Pos2) -> (f64, Option<f64>),
) {
    let Some(g) = grab else { return };
    if resp.dragged()
        && let Some(px) = resp.interact_pointer_pos()
        && let Some(k) = prop.keys.get(g.key)
    {
        let (inf, speed) = f(&g, px);
        let mut params = json!({"layer": layer.id.0, "prop": prop.uid, "time": k.time.seconds(), "side": if g.out { "out" } else { "in" }, "influence": inf.clamp(MIN_INFLUENCE, MAX_INFLUENCE), "merge": "tl-speed-ease"});
        if let Some(v) = speed.filter(|v| v.is_finite()) {
            params["speed"] = json!(v);
        }
        // The selected keys of other properties follow on the same side; with ⇧ at the press
        // the grabbed handle moves alone.
        params["respective"] = json!(true);
        if g.alone {
            params["keys"] = json!([]);
        }
        actions.push(("keys.setEaseSelected".into(), params));
        ctx.request_repaint();
    }
    if resp.drag_stopped() {
        ctx.data_mut(|d| d.remove::<Grab>(grab_id(prop.uid)));
        actions.push(("__endMerge".into(), json!({})));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::keyframe::{Keyframe, Value};

    fn prop_with(keys: Vec<Keyframe>) -> Property {
        let mut p = Property::new(1, "opacity", "Opacity", Value::Scalar(0.0));
        p.keys = keys;
        p
    }

    #[test]
    fn linear_and_hold_segments_draw_no_curve() {
        let k = |s: f64, v: f64| Keyframe::new(Tick::from_seconds_f64(s), Value::Scalar(v));
        assert!(!eased(&prop_with(vec![k(0.0, 0.0), k(1.0, 100.0)]), 0));
        assert!(!eased(&prop_with(vec![k(0.0, 0.0).hold(), k(1.0, 100.0)]), 0));
        assert!(eased(&prop_with(vec![k(0.0, 0.0).eased(), k(1.0, 100.0).eased()]), 0));
        // One Bezier side is enough: the speed changes along the segment.
        let mut a = k(0.0, 0.0);
        a.out_interp = Interp::Bezier;
        assert!(eased(&prop_with(vec![a, k(1.0, 100.0)]), 0));
        // No segment past the last key.
        assert!(!eased(&prop_with(vec![k(0.0, 0.0).eased()]), 0));
    }

    #[test]
    fn easy_ease_peaks_mid_segment_at_one_and_a_half_times_the_average_speed() {
        let k = |s: f64, v: f64| Keyframe::new(Tick::from_seconds_f64(s), Value::Scalar(v)).eased();
        let prop = prop_with(vec![k(0.0, 0.0), k(1.0, 100.0)]);
        let tm = TMap { x0: 0.0, start: 0.0, pps: 400.0 };
        let segs = segments(&prop, &|t| t, tm, 0.0, 400.0);
        let s = segs.first().expect("one eased segment");
        assert!((s.peak_x - 200.0).abs() < 4.0, "peak at {}", s.peak_x);
        // Easy Ease (33.33% both sides): the bezier peaks at 1.5× the average speed.
        assert!((s.peak - 150.0).abs() < 3.0, "peak {}", s.peak);
        assert_eq!(s.out_inf.map(|v| (v * 100.0).round()), Some(33.0));
        assert_eq!(s.in_inf.map(|v| (v * 100.0).round()), Some(33.0));
    }
}
