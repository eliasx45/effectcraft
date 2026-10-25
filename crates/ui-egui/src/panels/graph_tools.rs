//! Graph Editor and timeline keyframe tools: the transform box around several selected keys
//! (scale and move them in time and value), key-time snapping, and Alt-drag of a key group's
//! first or last key in the timeline to scale the group in time (edits are `keys.transform`), and
//! keyframe selection by press, click and marquee, shown live while the button is down.

use effectcraft_engine::KeyRef;
use effectcraft_engine::project::Comp;
use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::{Value, json};

use super::timeline::TMap;
use crate::panels::timeline::TimelineHost;
use crate::panels::timeline::ids::tl_id;

type Actions = Vec<(String, Value)>;

/// A selected key point in the graph: comp seconds, graph value, the dimension it shows, and
/// whether its value is editable there.
#[derive(Clone, Copy)]
pub(crate) struct SelKey {
    pub t: f64,
    pub v: f64,
    pub dim: usize,
    pub editable: bool,
}

/// Transform box geometry (data space): [t0, v0, t1, v1].
#[derive(Clone, Copy, Debug)]
struct BoxDrag {
    /// 0–3 corners (t0v1, t1v1, t1v0, t0v0 = screen TL, TR, BR, BL), 4–7 edges (top, right,
    /// bottom, left), 8 = move.
    handle: usize,
    /// Fixed opposite point (comp s, value).
    anchor: [f64; 2],
    /// Where the drag started (comp s, value): the pointer for a move, the handle for a scale.
    /// Every step sends the whole transform since then (`keys.transform {fromStart}`).
    last: [f64; 2],
    /// Original lower-left bound for snapping a move without accumulating offsets.
    origin: [f64; 2],
}

fn box_id() -> egui::Id {
    tl_id("graph-transform-box")
}

/// Discard drag origins that belong to a different graph/timeline composition.
pub(crate) fn reset_context(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove::<BoxDrag>(box_id());
        d.remove::<AltScale>(alt_id());
    });
}

/// The selection a keyframe marquee gives (Timeline and Graph Editor): the keys in the box
/// alone, added to the current selection with Shift, or toggled in it with Ctrl/Cmd. Keys show
/// it while the box is dragged, and the release commits the same with [`marquee_params`].
pub(crate) fn marquee_selection(current: &[KeyRef], in_box: &[KeyRef], mods: egui::Modifiers) -> Vec<KeyRef> {
    if mods.command {
        let mut sel: Vec<KeyRef> = current.iter().filter(|k| !in_box.contains(k)).copied().collect();
        sel.extend(in_box.iter().filter(|k| !current.contains(k)));
        sel
    } else if mods.shift {
        let mut sel = current.to_vec();
        sel.extend(in_box.iter().filter(|k| !current.contains(k)));
        sel
    } else {
        in_box.to_vec()
    }
}

/// The `keys.select` parameters committing a marquee over `in_box` (see [`marquee_selection`]).
pub(crate) fn marquee_params(in_box: &[KeyRef], mods: egui::Modifiers, select_properties: bool) -> Value {
    let mut keys: Vec<KeyRef> = vec![];
    for k in in_box {
        if !keys.contains(k) {
            keys.push(*k);
        }
    }
    let keys: Vec<Value> = keys.iter().map(|k| json!({"layer": k.layer.0, "prop": k.prop, "time": k.time.seconds()})).collect();
    let mut p = json!({"keys": keys});
    if mods.command {
        p["toggle"] = json!(true);
    } else if mods.shift {
        p["add"] = json!(true);
    }
    if select_properties {
        p["selectProperties"] = json!(true);
    }
    p
}

/// Pressing and clicking a keyframe (Timeline and Graph Editor), as in After Effects: pressing
/// an unselected key selects it at once (Shift adds it), so it shows selected while the button
/// is still down and a drag moves it; pressing a selected key keeps the selection (a drag moves
/// them all), and releasing without a drag then reduces the selection to that key (Shift takes
/// it out instead). `this_key` is the `keys.select` parameters for the key alone; `selected` and
/// `n_selected` describe the selection as drawn. Returns the `keys.select` parameters to queue.
/// Ctrl/Cmd clicks are the caller's (interpolation); a Ctrl/Cmd drag still grabs the key.
pub(crate) fn key_select_gesture(ui: &egui::Ui, resp: &egui::Response, this_key: &Value, selected: bool, n_selected: usize) -> Option<Value> {
    let mods = ui.input(|i| i.modifiers);
    let select = || {
        let mut p = this_key.clone();
        if mods.shift {
            p["add"] = json!(true);
        }
        p
    };
    if mods.command {
        return (resp.drag_started() && !selected).then(select);
    }
    // Whether the key was selected when the button went down, for the release.
    let memo = resp.id.with("key-press-selected");
    let mut out = None;
    if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
        ui.data_mut(|d| d.insert_temp(memo, selected));
        if !selected {
            out = Some(select());
        }
    }
    if resp.clicked() {
        let pressed = ui.data_mut(|d| d.remove_temp::<bool>(memo));
        match (pressed.unwrap_or(selected), mods.shift) {
            (true, true) => {
                let mut p = this_key.clone();
                p["toggle"] = json!(true);
                out = Some(p);
            }
            (true, false) if n_selected > 1 => out = Some(this_key.clone()),
            // The press went unseen: select on the click.
            (false, _) if pressed.is_none() => out = Some(select()),
            _ => {}
        }
    } else if out.is_none() && resp.drag_started() && !selected {
        out = Some(select());
    }
    out
}

/// Bounds of the selected keys: (t0, v0, t1, v1).
fn bounds(keys: &[SelKey]) -> Option<[f64; 4]> {
    if keys.len() < 2 {
        return None;
    }
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for k in keys {
        b = [b[0].min(k.t), b[1].min(k.v), b[2].max(k.t), b[3].max(k.v)];
    }
    (b[2] > b[0] || b[3] > b[1]).then_some(b)
}

fn handle_points(r: Rect) -> [Pos2; 8] {
    let c = r.center();
    [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), pos2(c.x, r.min.y), pos2(r.max.x, c.y), pos2(c.x, r.max.y), pos2(r.min.x, c.y)]
}

/// The transform box for two or more selected keys. Call before the keys are drawn (its inside
/// sits below them) with `handles = false`, and after them with `handles = true` (the handles sit
/// on top). Dragging a corner scales time and value about the opposite corner, an edge one axis,
/// the inside moves the keys; times land on frames.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transform_box(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    p: &egui::Painter,
    keys: &[SelKey],
    snap_keys: &[SelKey],
    tm: TMap,
    ymap: &dyn Fn(f64) -> f32,
    vmap: &dyn Fn(f32) -> f64,
    speed: bool,
    handles: bool,
    actions: &mut Actions,
) {
    if !app.ui().timeline.graph_transform_box {
        return;
    }
    let Some(b) = bounds(keys) else { return };
    let r = Rect::from_min_max(pos2(tm.x(b[0]), ymap(b[3])), pos2(tm.x(b[2]), ymap(b[1]))).expand(6.0);
    let value_ok = !speed && keys.iter().all(|k| k.editable);
    let col = Color32::from_rgb(0xd8, 0xd8, 0xd8);
    let ctx = ui.ctx().clone();
    let drag_id = tl_id("graph-dragging");
    let hp = handle_points(r);
    let data = |q: Pos2| [tm.t(q.x), vmap(q.y)];
    // Which data point each handle sits on, and its opposite.
    let corner = |i: usize| -> [f64; 2] {
        match i {
            0 => [b[0], b[3]],
            1 => [b[2], b[3]],
            2 => [b[2], b[1]],
            3 => [b[0], b[1]],
            4 => [(b[0] + b[2]) / 2.0, b[3]],
            5 => [b[2], (b[1] + b[3]) / 2.0],
            6 => [(b[0] + b[2]) / 2.0, b[1]],
            _ => [b[0], (b[1] + b[3]) / 2.0],
        }
    };
    let mut start: Option<BoxDrag> = None;
    let mut resp_any: Option<egui::Response> = None;
    if !handles {
        p.rect_stroke(r, 0.0, Stroke::new(1.0, col.gamma_multiply(0.7)), StrokeKind::Middle);
        let resp = ui.interact(r, tl_id("graph-box-inside"), Sense::drag());
        app.auto_mut().add("timeline.graph.transformBox", r, "Transform box");
        if resp.drag_started()
            && let Some(o) = ui.input(|i| i.pointer.press_origin())
        {
            start = Some(BoxDrag { handle: 8, anchor: [0.0; 2], last: data(o), origin: [b[0], b[1]] });
        }
        resp_any = Some(resp);
    } else {
        for (i, h) in hp.iter().enumerate() {
            // Value handles only when values can change.
            if !value_ok && matches!(i, 4 | 6) {
                continue;
            }
            let hr = Rect::from_center_size(*h, vec2(7.0, 7.0));
            p.rect_filled(hr, 0.0, Color32::from_gray(0x20));
            p.rect_stroke(hr, 0.0, Stroke::new(1.0, col), StrokeKind::Middle);
            let resp = ui.interact(hr.expand(2.0), tl_id(("graph-box-handle", i)), Sense::drag());
            app.auto_mut().add(&format!("timeline.graph.transformBox.{i}"), hr, "Transform box handle");
            if resp.drag_started() {
                let opp = [2, 3, 0, 1, 6, 7, 4, 5][i];
                start = Some(BoxDrag { handle: i, anchor: corner(opp), last: corner(i), origin: [b[0], b[1]] });
            }
            if resp.dragged() || resp.drag_stopped() {
                resp_any = Some(resp);
            }
        }
    }
    if let Some(s) = start {
        ctx.data_mut(|d| {
            d.insert_temp(box_id(), s);
            d.insert_temp(drag_id, true);
        });
    }
    let Some(resp) = resp_any else { return };
    let Some(st) = ctx.data(|d| d.get_temp::<BoxDrag>(box_id())) else { return };
    if (st.handle == 8) == handles {
        return;
    }
    if resp.dragged()
        && let Some(pt) = resp.interact_pointer_pos()
    {
        let mut cur = data(pt);
        if app.ui().timeline.graph_snap != ui.input(|i| i.modifiers.command) {
            let mut times: Vec<f64> = snap_keys.iter().map(|k| k.t).collect();
            times.push(app.session().time().seconds());
            if let Some(comp) = app.session().active_comp() {
                times.extend(comp_snap_times(comp));
            }
            let values: Vec<f64> = snap_keys.iter().map(|k| k.v).collect();
            let offset = if st.handle == 8 { [st.origin[0] - st.last[0], st.origin[1] - st.last[1]] } else { [0.0, 0.0] };
            let col = Color32::from_rgb(255, 153, 51);
            if !matches!(st.handle, 4 | 6)
                && let Some(at) = snap_target(cur[0] + offset[0], &times, 6.0 / tm.pps.max(1e-6))
            {
                cur[0] = at - offset[0];
                p.line_segment([pos2(tm.x(at), p.clip_rect().min.y), pos2(tm.x(at), p.clip_rect().max.y)], Stroke::new(1.0, col));
            }
            let tolerance = (vmap(pt.y + 6.0) - vmap(pt.y)).abs();
            if value_ok
                && !matches!(st.handle, 5 | 7)
                && let Some(at) = snap_target(cur[1] + offset[1], &values, tolerance)
            {
                cur[1] = at - offset[1];
                p.line_segment([pos2(p.clip_rect().min.x, ymap(at)), pos2(p.clip_rect().max.x, ymap(at))], Stroke::new(1.0, col));
            }
        }
        let mut params = json!({"merge": "graph-box", "fromStart": true});
        if st.handle == 8 {
            params["timeOffset"] = json!(cur[0] - st.last[0]);
            if value_ok {
                params["valueOffset"] = json!(cur[1] - st.last[1]);
            }
        } else {
            let (time_axis, value_axis) = match st.handle {
                4 | 6 => (false, true),
                5 | 7 => (true, false),
                _ => (true, true),
            };
            // Scale relative to where the handle was when the drag started.
            let now = st.last;
            if time_axis && (now[0] - st.anchor[0]).abs() > 1e-6 {
                let k = (cur[0] - st.anchor[0]) / (now[0] - st.anchor[0]);
                if k > 0.01 {
                    params["timeScale"] = json!(k);
                    params["timeAnchor"] = json!(st.anchor[0]);
                }
            }
            if value_axis && value_ok && (now[1] - st.anchor[1]).abs() > 1e-9 {
                params["valueScale"] = json!((cur[1] - st.anchor[1]) / (now[1] - st.anchor[1]));
                params["valueAnchor"] = json!(st.anchor[1]);
            }
        }
        let mut dims: Vec<usize> = keys.iter().map(|k| k.dim).collect();
        dims.sort_unstable();
        dims.dedup();
        params["dims"] = json!(dims);
        if params.as_object().is_some_and(|m| m.len() > 2) {
            actions.push(("keys.transform".into(), params));
        }
        ctx.data_mut(|d| d.insert_temp(box_id(), st));
    }
    if resp.drag_stopped() {
        ctx.data_mut(|d| {
            d.remove::<BoxDrag>(box_id());
            d.insert_temp(drag_id, false);
        });
        actions.push(("__endMerge".into(), json!({})));
    }
}

/// Snap targets of a comp besides keys: work area, markers, and every layer's in / out points
/// and markers (comp seconds).
pub fn comp_snap_times(c: &Comp) -> Vec<f64> {
    let mut v = vec![0.0, c.duration.seconds(), c.work_area.0.seconds(), c.work_area.1.seconds()];
    v.extend(c.markers.iter().map(|m| m.time.seconds()));
    for l in &c.layers {
        v.push(l.in_point.seconds());
        v.push(l.out_point.seconds());
        v.extend(l.markers.iter().map(|m| l.comp_time(m.time).seconds()));
    }
    v
}

/// Nearest finite candidate in data units. The optional result also drives snap guides.
pub(crate) fn snap_target(value: f64, candidates: &[f64], tolerance: f64) -> Option<f64> {
    if !value.is_finite() || !tolerance.is_finite() || tolerance <= 0.0 {
        return None;
    }
    candidates.iter().copied().filter(|v| v.is_finite() && (v - value).abs() <= tolerance).min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
}

/// The candidate nearest `t` within `tol` screen points, else `t`.
#[cfg(test)]
fn snap_px(tm: TMap, t: f64, cands: &[f64], tol: f32) -> f64 {
    let x = tm.x(t);
    cands.iter().copied().map(|c| (c, (tm.x(c) - x).abs())).filter(|(_, d)| *d < tol).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(c, _)| c).unwrap_or(t)
}

#[cfg(test)]
mod snap_tests {
    use super::*;

    #[test]
    fn snapping_reports_exact_alignment_and_ignores_nonfinite_candidates() {
        assert_eq!(super::snap_target(100.0, &[f64::NAN, 100.0, 102.0], 3.0), Some(100.0));
        assert_eq!(super::snap_target(101.0, &[100.0], 0.5), None);
        assert_eq!(super::snap_target(f64::INFINITY, &[100.0], 3.0), None);
    }

    #[test]
    fn graph_snap_targets_markers_and_layer_ends() {
        use effectcraft_engine::Session;
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "S", "width": 100, "height": 100, "frameRate": 30, "duration": 4})).unwrap();
        s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 10, "height": 10})).unwrap();
        s.execute("comp.addMarker", json!({"time": 1.5})).unwrap();
        let c = s.active_comp().unwrap();
        let cands = comp_snap_times(c);
        assert!(cands.contains(&1.5), "{cands:?}");
        assert!(cands.iter().any(|t| (*t - c.layers[0].out_point.seconds()).abs() < 1e-9));
        let tm = TMap { x0: 0.0, start: 0.0, pps: 100.0 };
        // 1.53 s is 3 points from the marker: snaps; 1.6 s (10 points) doesn't.
        assert_eq!(snap_px(tm, 1.53, &cands, 6.0), 1.5);
        assert_eq!(snap_px(tm, 1.6, &[1.5], 6.0), 1.6);
    }
}

// ---------------------------------------------------------------- timeline Alt-drag scaling

#[derive(Clone, Copy, Debug)]
struct AltScale {
    /// The fixed end of the group (comp s).
    anchor: f64,
    /// The dragged end is the group's last key (else its first).
    end_is_max: bool,
    /// The group's span when the drag started (set on its first step).
    span: Option<f64>,
}

fn alt_id() -> egui::Id {
    tl_id("timeline-alt-scale")
}

/// Selected keys' comp-time range.
fn sel_range(app: &dyn TimelineHost, comp: &Comp) -> Option<(f64, f64)> {
    let ts: Vec<f64> = app.session().state.selected_keys.iter().filter_map(|k| comp.layer(k.layer).map(|l| l.comp_time(k.time).seconds())).collect();
    if ts.len() < 2 {
        return None;
    }
    let lo = ts.iter().copied().fold(f64::MAX, f64::min);
    let hi = ts.iter().copied().fold(f64::MIN, f64::max);
    (hi - lo > 1e-9).then_some((lo, hi))
}

/// Alt-drag on the first or last key of a selected group starts scaling the group in time
/// about the other end. Returns true when it took the drag.
pub(crate) fn alt_scale_begin(app: &dyn TimelineHost, ctx: &egui::Context, comp: &Comp, key_ct: f64) -> bool {
    let Some((lo, hi)) = sel_range(app, comp) else { return false };
    let st = if (key_ct - hi).abs() < 1e-6 {
        AltScale { anchor: lo, end_is_max: true, span: None }
    } else if (key_ct - lo).abs() < 1e-6 {
        AltScale { anchor: hi, end_is_max: false, span: None }
    } else {
        return false;
    };
    ctx.data_mut(|d| d.insert_temp(alt_id(), st));
    true
}

/// While an Alt-drag scale is active: scale the group so its dragged end follows the pointer.
pub(crate) fn alt_scale_update(app: &dyn TimelineHost, ctx: &egui::Context, comp: &Comp, tm: TMap, actions: &mut Actions) {
    let Some(st) = ctx.data(|d| d.get_temp::<AltScale>(alt_id())) else { return };
    if !ctx.input(|i| i.pointer.primary_down()) {
        ctx.data_mut(|d| d.remove::<AltScale>(alt_id()));
        actions.push(("__endMerge".into(), json!({})));
        return;
    }
    let (Some((lo, hi)), Some(px)) = (sel_range(app, comp), ctx.input(|i| i.pointer.latest_pos())) else { return };
    let fr = comp.frame_rate;
    let target = fr.snap_nearest(effectcraft_engine::time::Tick::from_seconds_f64(tm.t(px.x).max(0.0))).seconds();
    // The group's span when the drag started: every step sends the whole scale since then.
    let span = st.span.unwrap_or((if st.end_is_max { hi } else { lo }) - st.anchor);
    if st.span.is_none() {
        ctx.data_mut(|d| d.insert_temp(alt_id(), AltScale { span: Some(span), ..st }));
    }
    let want = target - st.anchor;
    if span.abs() < 1e-9 || want.abs() < 1e-9 || want.signum() != span.signum() {
        return;
    }
    actions.push(("keys.transform".into(), json!({"timeScale": want / span, "timeAnchor": st.anchor, "merge": "key-alt-scale", "fromStart": true})));
}

/// An Alt-drag group scale is in progress (the plain key move stands aside).
pub(crate) fn alt_scale_active(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<AltScale>(alt_id())).is_some()
}

/// Graph Editor ▸ Show Reference Graph: the other graph type, normalized to the plot, faint.
pub(crate) fn reference_graph(p: &egui::Painter, plot: Rect, curves: &[(Color32, Vec<Pos2>)]) {
    for (c, pts) in curves {
        p.add(egui::Shape::line(pts.clone(), Stroke::new(1.0, c.gamma_multiply(0.35))));
    }
    let _ = plot;
}
