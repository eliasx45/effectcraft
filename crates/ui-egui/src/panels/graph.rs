//! The Graph Editor (Timeline ▸ Shift+F3): value graph and speed graph of the selected (or all
//! animated) properties, keyframes you can drag in time and value, Bezier/influence handles on
//! selected keys, auto-zoom / fit, and the bottom button bar (graph type, show selected,
//! auto-zoom, fit, separate dimensions, hold/linear/auto Bezier, Easy Ease family).
//!
//! Every edit is an engine command (`keys.set`, `keys.setEase`, `keys.select`, `keys.*`).

use effectcraft_engine::KeyRef;
use effectcraft_engine::keyframe::{Interp, Value, side_ease};
use effectcraft_engine::project::{Comp, Layer, LayerId, Property};
use effectcraft_engine::render::EvalCtx;
use effectcraft_engine::time::Tick;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::json;

use super::timeline::TMap;
#[cfg(test)]
use crate::EffectcraftApp;
use crate::panels::timeline::TimelineHost;
use crate::panels::timeline::ids::tl_id;
use crate::theme::Tokens;
use crate::widgets;

type Actions = Vec<(String, serde_json::Value)>;

const DIM_COLORS: [Color32; 4] =
    [Color32::from_rgb(0xe0, 0x55, 0x55), Color32::from_rgb(0x5c, 0xc8, 0x5c), Color32::from_rgb(0x50, 0x8c, 0xf0), Color32::from_rgb(0xe8, 0xc8, 0x40)];
const PROP_COLORS: [Color32; 6] = [
    Color32::from_rgb(0xe8, 0xc8, 0x40),
    Color32::from_rgb(0xd0, 0x70, 0xe0),
    Color32::from_rgb(0x40, 0xc8, 0xc8),
    Color32::from_rgb(0xf0, 0x90, 0x40),
    Color32::from_rgb(0x9c, 0xd8, 0x50),
    Color32::from_rgb(0xf0, 0x70, 0x90),
];

struct Handle {
    out: bool,
    /// Comp seconds / graph value of the handle end.
    t: f64,
    v: f64,
    /// The value falls through this side: the speed graph shows the speed's magnitude, and a
    /// drag there keeps this sign.
    negative: bool,
    /// Segment duration (layer seconds) the influence is relative to.
    dur: f64,
    /// Graph value the handle starts from: the key's value, or in the speed graph this side's
    /// speed (a key whose speeds differ shows two points joined by a line, as in After Effects).
    kv: f64,
}

/// Share of its segment a handle at 100% influence reaches: After Effects draws it halfway, so
/// two handles at 100% meet in the middle and never cross.
const HANDLE_REACH: f64 = 0.5;

struct KeyPt {
    idx: usize,
    time: Tick,
    /// Comp seconds.
    t: f64,
    v: f64,
    sel: bool,
    handles: Vec<Handle>,
}

struct Curve<'a> {
    layer: &'a Layer,
    prop: &'a Property,
    dim: usize,
    color: Color32,
    pts: Vec<(f64, f64)>,
    /// Unexpressed samples, only populated when expression comparison is enabled.
    base_pts: Vec<(f64, f64)>,
    /// Extremes of each keyframe segment over the whole curve (see [`segment_peaks`]).
    peaks: Vec<(f64, f64, f64, f64)>,
    keys: Vec<KeyPt>,
    /// Values editable in the value graph (not spatial / combined dimensions).
    editable: bool,
    /// One speed curve for all dimensions (see [`linked_speed`]).
    linked: bool,
}

/// Properties shown: the selected ones, or every animated property of the selected layers.
/// Whether the speed graph shows: Edit Speed Graph, or Auto-Select Graph Type with only spatial
/// properties shown (Position, Anchor Point… move along a path: their speed is what to edit).
fn speed_graph(app: &dyn TimelineHost, comp: &Comp) -> bool {
    match app.ui().timeline.graph_mode.as_str() {
        "speed" => true,
        "value" => false,
        _ => {
            let props = shown_props(app, comp);
            !props.is_empty()
                && props.iter().all(|(lid, uid)| {
                    comp.layer(*lid).and_then(|l| l.props.find(*uid)).is_some_and(|pr| pr.spatial && matches!(pr.value, Value::Vec2(_) | Value::Vec3(_)))
                })
        }
    }
}

pub(crate) fn shown_props(app: &dyn TimelineHost, comp: &Comp) -> Vec<(LayerId, u64)> {
    let st = &app.session().state;
    let numeric = |pr: &Property| !pr.value.components().is_empty() && pr.value.interpolates();
    // A selected keyframe counts as selecting its property and layer (as clicking a key does in
    // After Effects), so selecting a key in the Timeline shows its graph.
    let mut picked: Vec<(LayerId, u64)> = st.selected_props.clone();
    for k in &st.selected_keys {
        if !picked.contains(&(k.layer, k.prop)) {
            picked.push((k.layer, k.prop));
        }
    }
    let mut v = if app.ui().timeline.graph_show_selected && !picked.is_empty() {
        picked.iter().copied().filter(|(l, u)| comp.layer(*l).and_then(|l| l.props.find(*u)).is_some_and(numeric)).collect()
    } else {
        let mut animated = vec![];
        for l in comp.layers.iter().filter(|l| st.selected_layers.contains(&l.id) || picked.iter().any(|(pl, _)| *pl == l.id)) {
            l.props.walk("", &mut |_, pr| {
                if (pr.is_animated() || pr.has_expression()) && numeric(pr) {
                    animated.push((l.id, pr.uid));
                }
            });
        }
        animated
    };
    if app.ui().timeline.graph_set_only {
        v.clear();
    }
    if app.ui().timeline.graph_show_set || app.ui().timeline.graph_set_only {
        for l in &comp.layers {
            l.props.walk("", &mut |_, pr| {
                if pr.graph_editor && numeric(pr) && !v.contains(&(l.id, pr.uid)) {
                    v.push((l.id, pr.uid));
                }
            });
        }
    }
    v
}

const POSITION_AXES: [&str; 3] = ["positionX", "positionY", "positionZ"];

/// EvalCtx reads separated Position from its followers, ignoring the original property's keys.
fn separated_position<'a>(l: &'a Layer, pr: &Property) -> Option<&'a effectcraft_engine::project::PropGroup> {
    if pr.match_id != "position" {
        return None;
    }
    let tr = l.transform()?;
    (tr.get("positionX").is_some() && tr.get("position").is_some_and(|position| position.uid == pr.uid)).then_some(tr)
}

fn curve_has_expression(l: &Layer, pr: &Property) -> bool {
    if let Some(tr) = separated_position(l, pr) {
        POSITION_AXES.iter().filter_map(|axis| tr.get(axis)).any(Property::has_expression)
    } else {
        pr.has_expression()
    }
}

fn shown_dims(l: &Layer, pr: &Property) -> usize {
    let n = pr.value.components().len();
    if pr.shown_dims > 0 && !l.is_3d() {
        (pr.shown_dims as usize).min(n)
    } else if !l.is_3d() && n == 3 && matches!(pr.match_id.as_str(), "anchor" | "position" | "scale") {
        // A 2D layer's transform has no Z to show (After Effects), also when the property was
        // made without its shown dimensions (imported or older projects).
        2
    } else {
        n.min(4)
    }
}

/// The speed graph shows a non-spatial property with several dimensions (Scale) as one curve,
/// as After Effects does: the dimension that moves most leads, and a handle drag eases every
/// dimension together.
fn linked_speed(l: &Layer, pr: &Property, speed: bool) -> bool {
    speed && !pr.spatial && matches!(pr.value, Value::Vec2(_) | Value::Vec3(_)) && shown_dims(l, pr) > 1
}

/// The dimension of `pr` that moves most across its keys (the one a linked speed curve shows).
fn lead_dim(l: &Layer, pr: &Property) -> usize {
    let travel = |d: usize| {
        pr.keys
            .windows(2)
            .map(|w| (w[1].value.components().get(d).copied().unwrap_or(0.0) - w[0].value.components().get(d).copied().unwrap_or(0.0)).abs())
            .sum::<f64>()
    };
    (0..shown_dims(l, pr)).max_by(|a, b| travel(*a).total_cmp(&travel(*b))).unwrap_or(0)
}

#[derive(Clone, Default)]
struct CurveSamples {
    result: Vec<(f64, f64)>,
    base: Vec<(f64, f64)>,
    /// Lowest and highest value of each keyframe segment (comp seconds `t0`, `t1`, `lo`, `hi`),
    /// over the whole curve rather than the view.
    peaks: Vec<(f64, f64, f64, f64)>,
}

/// Samples per keyframe segment when finding its extremes (fewer for long key lists).
const PEAK_SAMPLES: usize = 48;

/// The lowest and highest value of every keyframe segment of `pr` (dimension `d`), whatever
/// part of it is in view. A sharp speed spike can fall between the per-pixel samples, and Auto-zoom
/// keeps the whole curve's height (After Effects), so each segment is sampled and its extremes
/// refined.
fn segment_peaks(ectx: &EvalCtx, l: &Layer, pr: &Property, d: usize, speed: bool) -> Vec<(f64, f64, f64, f64)> {
    let per = if pr.keys.len() > 200 { PEAK_SAMPLES / 4 } else { PEAK_SAMPLES };
    let f = |t: f64| sample(ectx, l, pr, d, speed, t);
    let mut out = Vec::with_capacity(pr.keys.len().saturating_sub(1));
    for w in pr.keys.windows(2) {
        let [a, b] = w else { continue };
        let (ta, tb) = (l.comp_time(a.time).seconds(), l.comp_time(b.time).seconds());
        let (t0, t1) = (ta.min(tb), ta.max(tb));
        if !t0.is_finite() || !t1.is_finite() || t1 <= t0 {
            continue;
        }
        let step = (t1 - t0) / per as f64;
        let vals: Vec<f64> = (0..=per).map(|i| f(t0 + step * i as f64)).collect();
        let pick = |better: fn(f64, f64) -> bool| {
            vals.iter().enumerate().filter(|(_, v)| v.is_finite()).fold(None, |best: Option<(usize, f64)>, (i, v)| match best {
                Some((_, bv)) if !better(*v, bv) => best,
                _ => Some((i, *v)),
            })
        };
        // Refine around the best sample (ternary search over its two neighbouring steps).
        let refine = |i: usize, v: f64, sign: f64| {
            let (mut lo, mut hi) = (t0 + step * i.saturating_sub(1) as f64, (t0 + step * (i + 1) as f64).min(t1));
            let score = |v: f64| if v.is_finite() { sign * v } else { f64::NEG_INFINITY };
            let mut best = v;
            for _ in 0..24 {
                let (m1, m2) = (lo + (hi - lo) / 3.0, hi - (hi - lo) / 3.0);
                let (v1, v2) = (f(m1), f(m2));
                for x in [v1, v2] {
                    if score(x) > score(best) {
                        best = x;
                    }
                }
                if score(v1) < score(v2) { lo = m1 } else { hi = m2 }
            }
            best
        };
        let (Some((imax, vmax)), Some((imin, vmin))) = (pick(|a, b| a > b), pick(|a, b| a < b)) else { continue };
        out.push((t0, t1, refine(imin, vmin, -1.0), refine(imax, vmax, 1.0)));
    }
    out
}

struct SampleCache {
    key: u64,
    // Retain the actual owners: copy-on-write edits must allocate new layers, and these
    // allocations cannot be recycled into a false cache hit while this cache is alive.
    owners: Vec<std::sync::Arc<Layer>>,
    curves: Vec<CurveSamples>,
}

type Samples = std::sync::Arc<SampleCache>;

/// Build the curves for the visible time span. Their samples evaluate every shown property
/// (expressions too) across the plot, so they are remembered until the project, the shown
/// properties or the view change.
/// The graph's value of dimension `d` of `pr` at comp time `ct` (s): its value, or in the speed
/// graph its speed (spatial properties) or velocity.
fn sample(ectx: &EvalCtx, l: &Layer, pr: &Property, d: usize, speed: bool, ct: f64) -> f64 {
    sample_with_speed_evaluation(ectx, l, pr, d, speed, ct, false)
}

fn sample_base(ectx: &EvalCtx, l: &Layer, pr: &Property, d: usize, speed: bool, ct: f64) -> f64 {
    let base = EvalCtx { expr: None, ..*ectx };
    // Both phases must use the same derivative stencil, including at Hold jumps and errors.
    sample_with_speed_evaluation(&base, l, pr, d, speed, ct, true)
}

fn sample_with_speed_evaluation(ectx: &EvalCtx, l: &Layer, pr: &Property, d: usize, speed: bool, ct: f64, evaluate_speed: bool) -> f64 {
    let tt = Tick::from_seconds_f64(ct);
    if !ct.is_finite() || !l.stretch.is_finite() || l.stretch.abs() < 1e-9 || tt.0.checked_sub(l.start_time.0).is_none() {
        return 0.0;
    }
    if !speed {
        return ectx.at(tt).value(l, pr).components().get(d).copied().unwrap_or(0.0);
    }
    let lt = l.layer_time(tt);
    if evaluate_speed || separated_position(l, pr).is_some() || (curve_has_expression(l, pr) && ectx.expr.is_some()) {
        // Ease handles/velocity dialogs use units per layer second. Evaluate at the matching
        // comp times so moving, stretching or reversing the layer does not change that unit.
        const STEP: f64 = 0.0001;
        let a = lt.seconds() - STEP;
        let b = lt.seconds() + STEP;
        let ta = Tick::from_seconds_f64(l.start_time.seconds() + a * l.stretch / 100.0);
        let tb = Tick::from_seconds_f64(l.start_time.seconds() + b * l.stretch / 100.0);
        if ta == tb || ta.0.checked_sub(l.start_time.0).is_none() || tb.0.checked_sub(l.start_time.0).is_none() {
            return 0.0;
        }
        // EvalCtx applies the renderer's keyframe fallback for expression errors.
        let va = ectx.at(ta).value(l, pr).components();
        let vb = ectx.at(tb).value(l, pr).components();
        let velocity = |i: usize| (vb.get(i).copied().unwrap_or(0.0) - va.get(i).copied().unwrap_or(0.0)) / (b - a);
        let v = if pr.spatial && matches!(pr.value, Value::Vec2(_) | Value::Vec3(_)) {
            (0..shown_dims(l, pr)).fold(0.0_f64, |magnitude, i| magnitude.hypot(velocity(i)))
        } else {
            velocity(d).abs()
        };
        return if v.is_finite() { v } else { 0.0 };
    }
    if pr.spatial && matches!(pr.value, Value::Vec2(_) | Value::Vec3(_)) {
        effectcraft_engine::keyframe::speed(&pr.keys, lt, true)
    } else {
        effectcraft_engine::keyframe::velocity(&pr.keys, lt, false).get(d).copied().unwrap_or(0.0).abs()
    }
}

fn curves<'a>(app: &dyn TimelineHost, ctx: &egui::Context, comp: &'a Comp, ectx: &EvalCtx, tm: TMap, plot: Rect, speed: bool) -> Vec<Curve<'a>> {
    let sel = &app.session().state.selected_keys;
    let n = ((plot.width() / 2.0) as usize).clamp(2, 2048);
    let props = shown_props(app, comp);
    let properties: Vec<_> = props
        .iter()
        .filter_map(|(lid, uid)| {
            let owner = comp.layers.arcs().iter().find(|layer| layer.id == *lid)?;
            Some((owner, owner.props.find(*uid)?))
        })
        .collect();
    // Plain local properties depend only on their owning layer. Expressions may read any
    // other layer or composition, so retain whole-document invalidation for those graphs.
    let expression_revision = properties.iter().any(|(layer, prop)| curve_has_expression(layer, prop)).then_some(app.session().revision);
    let compare_expressions = app.ui().timeline.graph_expression_overlay && ectx.expr.is_some() && speed == speed_graph(app, comp);
    let samples_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (expression_revision, ectx.comp_id.0, n, &props, compare_expressions, ectx.expr.is_some()).hash(&mut h);
        for v in [plot.min.x, plot.max.x, tm.x0] {
            v.to_bits().hash(&mut h);
        }
        (tm.start.to_bits(), tm.pps.to_bits()).hash(&mut h);
        h.finish()
    };
    let samples_id = tl_id(("graph-samples", speed));
    let cached = ctx.data(|d| d.get_temp::<Samples>(samples_id)).filter(|cache| {
        cache.key == samples_key
            && cache.owners.len() == properties.len()
            && properties.iter().zip(&cache.owners).all(|((owner, _), cached_owner)| std::sync::Arc::ptr_eq(owner, cached_owner))
    });
    let owners = if cached.is_none() { properties.iter().map(|(owner, _)| std::sync::Arc::clone(owner)).collect() } else { Vec::new() };
    let mut sampled: Vec<CurveSamples> = vec![];
    let mut out = vec![];
    for (pi, (owner, pr)) in properties.into_iter().enumerate() {
        let l = owner.as_ref();
        let (lid, uid) = (l.id, pr.uid);
        let spatial = pr.spatial && matches!(pr.value, Value::Vec2(_) | Value::Vec3(_));
        let linked = linked_speed(l, pr, speed);
        let dims = if (speed && spatial) || linked { 1 } else { shown_dims(l, pr) };
        for d in 0..dims {
            let d = if linked { lead_dim(l, pr) } else { d };
            let color = if dims > 1 { DIM_COLORS[d % 4] } else { PROP_COLORS[pi % PROP_COLORS.len()] };
            let ci = out.len();
            let mut points = cached.as_ref().and_then(|cache| cache.curves.get(ci)).cloned().unwrap_or_default();
            if points.result.is_empty() {
                let include_base = compare_expressions && curve_has_expression(l, pr);
                points.result.reserve(n);
                if include_base {
                    points.base.reserve(n);
                }
                for i in 0..n {
                    let x = plot.min.x + plot.width() * i as f32 / (n - 1) as f32;
                    let ct = tm.t(x);
                    points.result.push((ct, sample(ectx, l, pr, d, speed, ct)));
                    if include_base {
                        points.base.push((ct, sample_base(ectx, l, pr, d, speed, ct)));
                    }
                }
                points.peaks = segment_peaks(ectx, l, pr, d, speed);
            }
            if cached.is_none() {
                sampled.push(points.clone());
            }
            let CurveSamples { result: pts, base: base_pts, peaks } = points;
            let mut keys = vec![];
            // A pinned aggregate remains a useful readout after separation; only its followers
            // own editable keys now. Never offer edits to the ignored pre-separation keys.
            let base_keys = if separated_position(l, pr).is_some() { &[][..] } else { pr.keys.as_slice() };
            for (i, k) in base_keys.iter().enumerate() {
                let ks = sel.contains(&KeyRef { layer: lid, prop: uid, time: k.time });
                let kt = l.comp_time(k.time).seconds();
                let ed = if spatial { 0 } else { d };
                let mut handles = vec![];
                let mut side_speeds = vec![];
                for out_side in [false, true] {
                    let Some(e) = side_ease(&pr.keys, i, ed, pr.spatial, out_side) else { continue };
                    let j = if out_side { i + 1 } else { i - 1 };
                    let dur = (pr.keys[j].time.seconds() - k.time.seconds()).abs();
                    side_speeds.push(e.speed.abs());
                    let dt = e.influence * dur * HANDLE_REACH * if out_side { 1.0 } else { -1.0 };
                    let kv = if speed { e.speed.abs() } else { k.value.components().get(d).copied().unwrap_or(0.0) };
                    let (hv, show) = if speed { (kv, true) } else { (kv + e.speed * dt, !spatial) };
                    let bezier = if out_side { k.out_interp == Interp::Bezier } else { k.in_interp == Interp::Bezier };
                    if ks && show && (bezier || speed) {
                        // A side at speed 0 has no sign of its own: it takes the direction its
                        // segment moves, so pulling it up speeds the value the way it is going
                        // rather than reversing it (a falling value would stop and turn back).
                        let negative = if e.speed != 0.0 {
                            e.speed < 0.0
                        } else {
                            let at = |kk: &effectcraft_engine::keyframe::Keyframe| kk.value.components().get(ed).copied().unwrap_or(0.0);
                            let other = pr.keys.get(j).map_or(at(k), at);
                            let (a, b) = if out_side { (at(k), other) } else { (other, at(k)) };
                            !pr.spatial && b < a
                        };
                        handles.push(Handle { out: out_side, t: kt + dt, v: hv, negative, dur, kv });
                    }
                }
                let v = if speed {
                    if side_speeds.is_empty() { 0.0 } else { side_speeds.iter().sum::<f64>() / side_speeds.len() as f64 }
                } else {
                    k.value.components().get(d).copied().unwrap_or(0.0)
                };
                keys.push(KeyPt { idx: i, time: k.time, t: kt, v, sel: ks, handles });
            }
            out.push(Curve { layer: l, prop: pr, dim: d, color, pts, base_pts, peaks, keys, editable: !spatial, linked });
        }
    }
    if cached.is_none() {
        ctx.data_mut(|d| d.insert_temp::<Samples>(samples_id, std::sync::Arc::new(SampleCache { key: samples_key, owners, curves: sampled })));
    }
    out
}

/// The Graph Editor's button bar, laid out left to right. Buttons that would run past the right
/// edge (a Timeline narrower than the whole row, #457) go to the "»" menu at its end instead of
/// being clipped, so every one stays reachable.
struct ButtonBar {
    x: f32,
    /// Where the next button must end to stay on the bar (the "»" button's room is kept free).
    limit: f32,
    /// Buttons moved to the "»" menu: (automation id, label, on, tooltip).
    hidden: Vec<(String, String, bool, String)>,
    /// The id picked from the "»" menu last frame, run by its button this frame.
    chosen: Option<String>,
}

/// The "»" menu's popup and the id it picked (read by the next frame's buttons).
const MORE_POPUP: &str = "graph-more";

impl ButtonBar {
    const MORE_W: f32 = 24.0;

    fn new(ctx: &egui::Context, bar: Rect) -> Self {
        let chosen = ctx.data_mut(|d| d.remove_temp::<String>(tl_id(MORE_POPUP).with("chosen")));
        ButtonBar { x: bar.min.x + 8.0, limit: bar.max.x - 8.0 - Self::MORE_W - 4.0, hidden: Vec::new(), chosen }
    }

    /// The "»" button and its menu, when buttons did not fit.
    fn more(&mut self, app: &mut dyn TimelineHost, ui: &mut egui::Ui, p: &egui::Painter, bar: Rect) {
        if self.hidden.is_empty() {
            return;
        }
        let t = app.tokens();
        let r = Rect::from_min_size(pos2(bar.max.x - 8.0 - Self::MORE_W, bar.center().y - 9.0), vec2(Self::MORE_W, 18.0));
        let pid = tl_id(MORE_POPUP);
        let resp = ui.interact(r, pid.with("button"), Sense::click()).on_hover_text(crate::i18n::tr("More"));
        p.rect_filled(r, 3.0, if resp.hovered() { t.hover } else { Color32::from_rgb(0x2c, 0x2c, 0x2c) });
        p.text(r.center(), Align2::CENTER_CENTER, "»", Tokens::ui(12.0), t.text);
        app.auto_mut().add("timeline.graph.more", r, "More");
        if resp.clicked() {
            let open = widgets::popup_is_open(ui, pid);
            ui.data_mut(|d| d.insert_temp(pid.with("open"), !open));
        }
        if !widgets::popup_is_open(ui, pid) {
            return;
        }
        let rows = self.hidden.len();
        let pos = r.left_top() - vec2(0.0, rows as f32 * 22.0 + 18.0);
        let chosen = widgets::popup_list(ui, pid, pos, r, rows, 0, |ui| {
            let mut chosen = None;
            for (id, label, on, tip) in &self.hidden {
                let item = ui.selectable_label(*on, label.as_str()).on_hover_text(tip.as_str());
                app.auto_mut().add(&format!("timeline.graph.more.{id}"), item.rect, tip);
                if item.clicked() {
                    chosen = Some(id.clone());
                }
            }
            chosen
        });
        if let Some(id) = chosen {
            ui.data_mut(|d| d.insert_temp(pid.with("chosen"), id));
            ui.ctx().request_repaint();
        }
    }
}

/// Compact menus keep graph/view options out of the editing button strip.
fn options_menu(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    p: &egui::Painter,
    bar: &mut ButtonBar,
    y: f32,
    id: &str,
    label: &str,
    options: &[(&str, &str, bool)],
) -> Option<usize> {
    let x = bar.x.min(bar.limit - 80.0);
    let anchor = Rect::from_min_size(pos2(x, y - 9.0), vec2(80.0, 18.0));
    let pid = tl_id(("graph-options", id));
    if text_button(app, ui, p, bar, y, label, false, id, label) {
        let open = widgets::popup_is_open(ui, pid);
        ui.data_mut(|d| d.insert_temp(pid.with("open"), !open));
    }
    if !widgets::popup_is_open(ui, pid) {
        return None;
    }
    widgets::popup_list(ui, pid, anchor.left_top() - vec2(0.0, options.len() as f32 * 22.0 + 18.0), anchor, options.len(), 0, |ui| {
        let mut chosen = None;
        for (i, (key, label, on)) in options.iter().enumerate() {
            let resp = ui.selectable_label(*on, crate::i18n::tr(label));
            app.auto_mut().add(&format!("timeline.graph.{key}"), resp.rect, label);
            if resp.clicked() {
                chosen = Some(i);
            }
        }
        chosen
    })
}

#[allow(clippy::too_many_arguments)]
fn text_button(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    p: &egui::Painter,
    bar: &mut ButtonBar,
    y: f32,
    label: &str,
    on: bool,
    id: &str,
    tip: &str,
) -> bool {
    let t = app.tokens();
    let name = match label {
        "Fit" => "graph-fit",
        "Fit Selection" => "graph-fit-selection",
        "Separate Dimensions" => "graph-separate",
        "Hold" => "graph-hold",
        "Linear" => "graph-linear",
        "Auto Bezier" => "graph-auto-bezier",
        "Easy Ease" => "graph-ease",
        "Ease In" => "graph-ease-in",
        "Ease Out" => "graph-ease-out",
        _ => "",
    };
    let icon = if name.is_empty() { None } else { crate::local_assets::icon(ui.ctx(), name) };
    let compact = !name.is_empty() || matches!(label, "Auto-zoom" | "Snap" | "Transform Box");
    let w = if compact { 24.0 } else { p.layout_no_wrap(label.to_string(), Tokens::ui(11.0), t.text).size().x + 14.0 };
    // Once one button overflows, the rest follow it into the menu (they keep their order).
    if !bar.hidden.is_empty() || bar.x + w > bar.limit {
        bar.hidden.push((id.to_string(), label.to_string(), on, tip.to_string()));
        return bar.chosen.as_deref() == Some(id);
    }
    let r = Rect::from_min_size(pos2(bar.x, y - 9.0), vec2(w, 18.0));
    let resp = ui.interact(r, tl_id(("graph-btn", id)), Sense::click()).on_hover_text(tip);
    p.rect_filled(
        r,
        3.0,
        if on {
            t.accent.gamma_multiply(0.55)
        } else if resp.hovered() {
            t.hover
        } else {
            Color32::from_rgb(0x2c, 0x2c, 0x2c)
        },
    );
    let color = if on { Color32::WHITE } else { t.text };
    if let Some(icon) = icon {
        p.image(icon.id(), Rect::from_center_size(r.center(), vec2(16.0, 16.0)), Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), color);
    } else {
        use crate::icons::{self, Icon};
        let original = match label {
            "Fit" => Some(Icon::Fullscreen),
            "Fit Selection" => Some(Icon::Region),
            "Separate Dimensions" => Some(Icon::Channel),
            "Auto-zoom" => Some(Icon::MountainLarge),
            "Snap" => Some(Icon::Link),
            "Transform Box" => Some(Icon::Rectangle),
            _ => None,
        };
        if let Some(icon) = original {
            icons::paint(p, r.shrink(3.0), icon, color);
        } else {
            let short = match label {
                "Hold" => "H",
                "Linear" => "L",
                "Auto Bezier" => "B",
                "Easy Ease" => "E",
                "Ease In" => "EI",
                "Ease Out" => "EO",
                _ => label,
            };
            p.text(r.center(), Align2::CENTER_CENTER, short, Tokens::ui(11.0), color);
        }
    }
    app.auto_mut().add(&format!("timeline.graph.{id}"), r, tip);
    bar.x += w + 4.0;
    resp.clicked()
}

/// Draw the Graph Editor into `area` (the right side of the Timeline) and queue edits.
pub(crate) fn show(app: &mut dyn TimelineHost, ui: &mut egui::Ui, p: &egui::Painter, comp: &Comp, ectx: &EvalCtx, tm: TMap, area: Rect, actions: &mut Actions) {
    let t = app.tokens();
    let ctx = ui.ctx().clone();
    let footer_h = 28.0;
    let plot = Rect::from_min_max(area.min + vec2(0.0, 6.0), pos2(area.max.x, area.max.y - footer_h));
    let bar = Rect::from_min_max(pos2(area.min.x, area.max.y - footer_h), area.max);
    // Background first so keys and handles (added later) sit on top for hit-testing.
    let navigating = super::space_hand(&ctx)
        || matches!(app.ui().tool, crate::state::Tool::Hand | crate::state::Tool::Zoom)
        || ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle))
        || ctx.data(|d| d.get_temp::<bool>(tl_id("graph-pan"))).unwrap_or(false);
    let edit_sense = if navigating { Sense::hover() } else { Sense::click_and_drag() };
    let bg = ui.interact(plot, tl_id("graph-bg"), edit_sense);
    p.rect_filled(area, 0.0, Color32::from_rgb(0x1d, 0x1d, 0x1d));
    let mode = app.ui().timeline.graph_mode.clone();
    app.auto_mut().add("timeline.graph", plot, &mode);
    let speed = speed_graph(app, comp);
    let cs = curves(app, &ctx, comp, ectx, tm, plot, speed);
    // Just opened, or the graph type changed (value and speed have different units): frame the
    // graphs (Fit Selection to View, else Fit All Graphs to View).
    let was_speed_id = tl_id("graph-was-speed");
    let type_changed = ctx.data(|d| d.get_temp::<bool>(was_speed_id)).is_some_and(|was| was != speed);
    ctx.data_mut(|d| d.insert_temp(was_speed_id, speed));
    if ctx.data_mut(|d| d.remove_temp::<bool>(fit_pending_id())).unwrap_or(false) || type_changed {
        if cs.iter().any(|c| c.keys.iter().any(|k| k.sel)) {
            fit_selection(app, &cs, ectx, speed, plot);
        } else {
            fit_all(app, &cs, ectx, speed, plot);
        }
    }
    // F (timeline.graph.fit): Fit All Graphs to View, as the Fit button.
    if take_fit_all_request(&ctx) {
        fit_all(app, &cs, ectx, speed, plot);
    }

    // Vertical range: auto-zoom to the curves + handles, or the stored manual range.
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in &cs {
        for (_, v) in c.pts.iter().chain(&c.base_pts) {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        // The whole curve's height, not just the part in view: zooming in on time never pushes
        // a peak out of the top (After Effects).
        for &(_, _, plo, phi) in &c.peaks {
            lo = lo.min(plo);
            hi = hi.max(phi);
        }
        for k in &c.keys {
            for v in std::iter::once(k.v).chain(k.handles.iter().map(|h| h.v)) {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
    }
    if !lo.is_finite() {
        (lo, hi) = (0.0, 100.0);
    }
    if speed {
        lo = lo.min(0.0);
    }
    if (hi - lo).abs() < 1e-9 {
        hi = lo + 1.0;
    }
    let pad = (hi - lo) * 0.08;
    let auto = (lo - pad, hi + pad);
    // Keep a stable scale while keys are dragged. A handle drag refits live (After Effects): the
    // spike it pulls up stays in view. Handle deltas use the gesture-start scale, so the refit
    // cannot feed back into the edit.
    let drag_id = tl_id("graph-dragging");
    let dragging = ctx.data(|d| d.get_temp::<bool>(drag_id)).unwrap_or(false) && ctx.input(|i| i.pointer.any_down());
    let stored = app.ui().timeline.graph_range.filter(|(a, b)| a.is_finite() && b.is_finite() && b > a);
    let (lo, hi) = if app.ui().timeline.graph_auto_zoom {
        if dragging && ctx.data(|d| d.get_temp::<HandleDrag>(handle_drag_id())).is_none() { stored.unwrap_or(auto) } else { auto }
    } else {
        stored.unwrap_or(auto)
    };
    if app.ui().timeline.graph_auto_zoom {
        app.ui_mut().timeline.graph_range = Some((lo, hi));
    }
    let ymap = |v: f64| plot.max.y - ((v - lo) / (hi - lo)) as f32 * plot.height();
    let vmap = |y: f32| lo + (plot.max.y - y) as f64 / f64::from(plot.height().max(1.0)) * (hi - lo);

    // Grid + value labels.
    let pc = p.with_clip_rect(plot);
    let step = nice_step((hi - lo) / 6.0);
    let mut gv = (lo / step).ceil() * step;
    while gv <= hi {
        let y = ymap(gv);
        pc.line_segment([pos2(plot.min.x, y), pos2(plot.max.x, y)], Stroke::new(1.0, Color32::from_white_alpha(12)));
        pc.text(pos2(plot.min.x + 4.0, y - 1.0), Align2::LEFT_BOTTOM, fmt(gv, step), Tokens::ui(10.0), t.text_faint);
        gv += step;
    }
    if let Some(first) = cs.first() {
        let unit = graph_units(first.prop, speed);
        let label = if cs.iter().all(|c| graph_units(c.prop, speed) == unit) { unit } else { "mixed units" };
        pc.text(plot.left_top() + vec2(4.0, 2.0), Align2::LEFT_TOP, label, Tokens::ui(10.0), t.text_dim);
    }
    if cs.is_empty() {
        p.text(
            plot.center(),
            Align2::CENTER_CENTER,
            crate::i18n::tr("Select animated properties to show them in the Graph Editor"),
            Tokens::ui(12.0),
            t.text_faint,
        );
    }

    // Show Reference Graph: the other graph type, faint, scaled to the plot.
    if app.ui().timeline.graph_reference && !cs.is_empty() {
        let other = curves(app, &ctx, comp, ectx, tm, plot, !speed);
        let (mut rlo, mut rhi) = (f64::INFINITY, f64::NEG_INFINITY);
        for c in &other {
            for (_, v) in &c.pts {
                rlo = rlo.min(*v);
                rhi = rhi.max(*v);
            }
        }
        if rlo.is_finite() {
            let span = (rhi - rlo).max(1e-9);
            let lines: Vec<(Color32, Vec<Pos2>)> = other
                .iter()
                .map(|c| {
                    (
                        c.color,
                        c.pts
                            .iter()
                            .map(|(ct, v)| pos2(tm.x(*ct), plot.max.y - ((v - rlo) / span) as f32 * plot.height() * 0.9 - plot.height() * 0.05))
                            .collect(),
                    )
                })
                .collect();
            super::graph_tools::reference_graph(&pc, plot, &lines);
            for i in 0..=4 {
                let f = f64::from(i) / 4.0;
                let value = rlo + f * span;
                let y = plot.max.y - (f as f32 * 0.9 + 0.05) * plot.height();
                pc.text(pos2(plot.max.x - 4.0, y), Align2::RIGHT_CENTER, format!("{value:.2}"), Tokens::ui(10.0), t.text_dim);
            }
            let unit = other.first().map(|c| graph_units(c.prop, !speed)).unwrap_or("");
            pc.text(plot.right_top() + vec2(-4.0, 2.0), Align2::RIGHT_TOP, format!("Reference · {unit}"), Tokens::ui(10.0), t.text_dim);
        }
    }
    // Expression comparison shares the main axis; Reference Graph above keeps its own axis.
    for c in &cs {
        if !c.base_pts.is_empty() {
            let pts: Vec<Pos2> = c.base_pts.iter().map(|(ct, v)| pos2(tm.x(*ct), ymap(*v))).collect();
            pc.add(egui::Shape::line(pts, Stroke::new(1.5, c.color.gamma_multiply(0.4))));
        }
    }
    // Curves.
    for c in &cs {
        let pts: Vec<Pos2> = c.pts.iter().map(|(ct, v)| pos2(tm.x(*ct), ymap(*v))).collect();
        pc.add(egui::Shape::line(pts, Stroke::new(1.5, c.color)));
    }
    if cs.iter().any(|c| !c.base_pts.is_empty()) {
        expression_legend(app, &pc, plot);
    }
    if !navigating
        && !ui.input(|i| i.pointer.any_down())
        && let Some(q) = crate::layer_pointer::Lp::of(ui).pos(ui.input(|i| i.pointer.hover_pos())).filter(|q| plot.contains(*q))
    {
        let hit = cs
            .iter()
            .flat_map(|c| {
                [false, true].into_iter().filter_map(move |before| {
                    if before && c.base_pts.is_empty() {
                        return None;
                    }
                    let value = if before {
                        sample_base(ectx, c.layer, c.prop, c.dim, speed, tm.t(q.x))
                    } else {
                        sample(ectx, c.layer, c.prop, c.dim, speed, tm.t(q.x))
                    };
                    let distance = (ymap(value) - q.y).abs();
                    (distance < 7.0).then_some((c, value, distance, before))
                })
            })
            .min_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((c, value, _, before)) = hit {
            egui::Tooltip::always_open(ctx.clone(), ui.layer_id(), tl_id("graph-curve-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                ui.label(format!(
                    "{} · {} · {}\n{:.3} s  ·  {:.3} {}",
                    c.layer.name,
                    c.prop.name,
                    dimension_name(c.dim),
                    tm.t(q.x),
                    value,
                    graph_units(c.prop, speed)
                ));
                if speed {
                    ui.label(crate::i18n::tr("Speed per layer second"));
                }
                if before {
                    ui.label(crate::i18n::tr("Before expression"));
                } else if curve_has_expression(c.layer, c.prop) {
                    ui.label(crate::i18n::tr("After expression"));
                    ui.label(crate::i18n::tr("Expression result; keys and handles edit the underlying animation"));
                }
            });
        }
    }
    // Transform box around several selected keys (inside below the keys, handles above).
    let sel_keys: Vec<super::graph_tools::SelKey> = cs
        .iter()
        .flat_map(|c| c.keys.iter().filter(|k| k.sel).map(move |k| super::graph_tools::SelKey { t: k.t, v: k.v, dim: c.dim, editable: c.editable }))
        .collect();
    let snap_keys: Vec<super::graph_tools::SelKey> = cs
        .iter()
        .flat_map(|c| c.keys.iter().filter(|k| !k.sel).map(move |k| super::graph_tools::SelKey { t: k.t, v: k.v, dim: c.dim, editable: c.editable }))
        .collect();
    if !navigating {
        super::graph_tools::transform_box(app, ui, &pc, &sel_keys, &snap_keys, tm, &ymap, &vmap, speed, false, actions);
    }
    // Key marquee (dragging the background): the box now (the last one on release), the keys
    // inside it, and the selection the keys show while it is dragged, as in After Effects.
    let key_box = match (bg.dragged(), crate::layer_pointer::Lp::of(ui).pos(ctx.input(|i| i.pointer.press_origin())), bg.interact_pointer_pos()) {
        (true, Some(o), Some(cur)) => Some(Rect::from_two_pos(o, cur)),
        _ if bg.drag_stopped() => ctx.data(|d| d.get_temp::<Rect>(tl_id("graph-box"))),
        _ => None,
    };
    let box_keys: Option<Vec<KeyRef>> = key_box.map(|br| {
        cs.iter()
            .filter(|c| !c.layer.switches.locked)
            .flat_map(|c| {
                c.keys
                    .iter()
                    .filter(move |k| {
                        let kp = pos2(tm.x(k.t), ymap(k.v));
                        plot.expand(6.0).contains(kp) && br.contains(kp)
                    })
                    .map(move |k| KeyRef { layer: c.layer.id, prop: c.prop.uid, time: k.time })
            })
            .collect()
    });
    let current = app.session().state.selected_keys.clone();
    let key_preview = box_keys.as_ref().map(|in_box| super::graph_tools::marquee_selection(&current, in_box, ui.input(|i| i.modifiers)));
    let n_selected = current.len();
    // Keys.
    for c in &cs {
        let (lid, uid, d) = (c.layer.id, c.prop.uid, c.dim);
        for k in &c.keys {
            let kp = pos2(tm.x(k.t), ymap(k.v));
            // In the speed graph an end key rises with the handle being dragged; skipping it once it
            // leaves the plot would drop the handle's widget and end the drag at the top edge.
            let handle_dragged = || ["in", "out"].iter().any(|side| ctx.is_being_dragged(tl_id(("ghandle", uid, d, k.idx, *side))));
            if !plot.expand(6.0).contains(kp) && !handle_dragged() {
                continue;
            }
            let kjson = json!({"layer": lid.0, "prop": uid, "time": k.time.seconds()});
            // Handle lines under the key (their ends and grips go on top, after every key).
            for h in &k.handles {
                let ap = pos2(kp.x, ymap(h.kv));
                let side = if h.out { "out" } else { "in" };
                let pinned = !ctx.is_being_dragged(tl_id(("ghandle", uid, d, k.idx, side)));
                let hp = handle_pos(ap, pos2(tm.x(h.t), ymap(h.v)), h.out, plot, pinned);
                pc.line_segment([ap, hp], Stroke::new(1.0, Color32::from_rgb(0xd8, 0xd8, 0x60)));
            }
            // Speed graph: a key entered and left at different speeds shows both, joined by a
            // vertical line (After Effects).
            if let [a, b] = k.handles.as_slice()
                && speed
                && (ymap(a.kv) - ymap(b.kv)).abs() > 1.0
            {
                pc.line_segment([pos2(kp.x, ymap(a.kv)), pos2(kp.x, ymap(b.kv))], Stroke::new(1.0, Color32::from_rgb(0xd8, 0xd8, 0x60)));
                for h in [a, b] {
                    pc.circle_filled(pos2(kp.x, ymap(h.kv)), 2.5, Color32::from_rgb(0xd8, 0xd8, 0x60));
                }
            }
            // The key itself.
            // Drawn selected: what a marquee being dragged would select, else the selection.
            let shown = key_preview.as_ref().map_or(k.sel, |p| p.contains(&KeyRef { layer: lid, prop: uid, time: k.time }));
            let col = if shown { t.keyframe_selected } else { Color32::from_gray(0xe0) };
            pc.rect_filled(Rect::from_center_size(kp, vec2(7.0, 7.0)), 0.0, col);
            pc.rect_stroke(Rect::from_center_size(kp, vec2(7.0, 7.0)), 0.0, Stroke::new(1.0, c.color), StrokeKind::Outside);
            let kr = Rect::from_center_size(kp, vec2(11.0, 11.0));
            let kresp = ui.interact(kr, tl_id(("gkey", uid, d, k.idx)), edit_sense);
            let label = if shown { format!("{} (selected)", c.prop.name) } else { c.prop.name.clone() };
            app.auto_mut().add(&format!("timeline.graph.key.{uid}.{d}.{}", k.idx), kr, &label);
            let mods = ui.input(|i| i.modifiers);
            let Some(key) = c.prop.keys.get(k.idx) else { continue };
            let kresp = kresp.on_hover_text(format!(
                "{} · {} · {}\n{:.3} s · {:.3} {}",
                c.layer.name,
                c.prop.name,
                dimension_name(d),
                k.t,
                k.v,
                graph_units(c.prop, speed)
            ));
            if kresp.clicked() && mods.command {
                // Ctrl+click: Linear ↔ Auto Bezier; Ctrl+Alt+click: Hold on / off (as in the Timeline).
                actions.push(("keys.select".into(), json!({"keys": [kjson.clone()]})));
                if mods.alt {
                    actions.push(("keys.toggleHold".into(), json!({})));
                } else {
                    let linear = key.in_interp == Interp::Linear && key.out_interp == Interp::Linear;
                    actions.push(("keys.interpolation".into(), json!({"interpolation": if linear { "autoBezier" } else { "linear" }})));
                }
            } else if let Some(p) = super::graph_tools::key_select_gesture(ui, &kresp, &json!({"keys": [kjson.clone()]}), k.sel, n_selected) {
                // Selected on the press (Shift adds); a click on a selected key reduces the
                // selection to it (Shift: takes it out).
                actions.push(("keys.select".into(), p));
            }
            if kresp.secondary_clicked() && !k.sel {
                actions.push(("keys.select".into(), json!({"keys": [kjson.clone()]})));
            }
            if kresp.double_clicked() {
                if speed {
                    actions.push(("keys.velocity".into(), json!({})));
                } else if matches!(key.value, Value::Scalar(_) | Value::Vec2(_) | Value::Vec3(_)) {
                    actions.push(("keys.set".into(), kjson.clone()));
                } else {
                    actions.push(("keys.interpolation".into(), json!({})));
                }
            }
            widgets::context_menu(&kresp, |ui| {
                for (id, label, command, params) in [
                    ("editValue", "Edit Value…", "keys.set", kjson.clone()),
                    ("interpolation", "Keyframe Interpolation…", "keys.interpolation", json!({})),
                    ("velocity", "Keyframe Velocity…", "keys.velocity", json!({})),
                    ("hold", "Toggle Hold Keyframe", "keys.toggleHold", json!({})),
                    ("ease", "Easy Ease", "keys.easyEase", json!({})),
                    ("easeIn", "Easy Ease In", "keys.easyEaseIn", json!({})),
                    ("easeOut", "Easy Ease Out", "keys.easyEaseOut", json!({})),
                    ("delete", "Delete Keyframes", "keys.delete", json!({})),
                ] {
                    if id == "editValue" && !matches!(key.value, Value::Scalar(_) | Value::Vec2(_) | Value::Vec3(_)) {
                        continue;
                    }
                    let response = ui.button(crate::i18n::tr(label));
                    app.auto_mut().add(&format!("timeline.graph.keyMenu.{id}"), response.rect, label);
                    if response.clicked() {
                        // A context action on a selected key operates on the whole selection.
                        if !k.sel {
                            actions.push(("keys.select".into(), json!({"keys": [kjson.clone()]})));
                        }
                        actions.push((command.into(), params));
                        ui.close();
                    }
                }
            });
            // A drag moves every selected key, by how far the pointer went from where the button
            // went down (the key doesn't jump under the pointer); Shift keeps it to the time or
            // the value axis, whichever moved more.
            if kresp.drag_started()
                && let Some(o) = crate::layer_pointer::Lp::of(ui).pos(ui.input(|i| i.pointer.press_origin()))
            {
                ctx.data_mut(|dd| {
                    dd.insert_temp(key_grab_id(), KeyGrab { press: o, t: k.t, v: k.v, value_per_pixel: (hi - lo) / f64::from(plot.height().max(1.0)) })
                });
            }
            if kresp.dragged()
                && let Some(pt) = kresp.interact_pointer_pos()
                && let Some(g) = ctx.data(|dd| dd.get_temp::<KeyGrab>(key_grab_id()))
            {
                ctx.data_mut(|dd| dd.insert_temp(drag_id, true));
                let (dx, dy) = (pt.x - g.press.x, pt.y - g.press.y);
                let (time_ok, value_ok) = shift_axes(mods.shift, dx, dy);
                // Graph Editor ▸ Snap: to the current time and other keys.
                let others: Vec<f64> = cs.iter().flat_map(|c| c.keys.iter().filter(|k| !k.sel).map(|k| k.t)).collect();
                let raw_t = g.t + (tm.t(pt.x) - tm.t(g.press.x));
                let snapping = app.ui().timeline.graph_snap != mods.command;
                let mut candidates = super::graph_tools::comp_snap_times(comp);
                candidates.extend(others);
                candidates.push(app.session().time().seconds());
                let snapped_t = snapping.then(|| super::graph_tools::snap_target(raw_t, &candidates, 6.0 / tm.pps.max(1e-6))).flatten();
                let t = if time_ok { snapped_t.unwrap_or(raw_t) } else { g.t };
                if time_ok && let Some(at) = snapped_t {
                    pc.line_segment([pos2(tm.x(at), plot.min.y), pos2(tm.x(at), plot.max.y)], Stroke::new(1.0, Color32::from_rgb(255, 153, 51)));
                }
                let raw_v = g.v - f64::from(dy) * g.value_per_pixel;
                let values: Vec<f64> = cs.iter().flat_map(|c| c.keys.iter().filter(|k| !k.sel).map(|k| k.v)).collect();
                let snapped_v = (snapping && value_ok)
                    .then(|| super::graph_tools::snap_target(raw_v, &values, 6.0 * (hi - lo) / f64::from(plot.height().max(1.0))))
                    .flatten();
                let value_offset = snapped_v.unwrap_or(raw_v) - g.v;
                if let Some(at) = snapped_v {
                    pc.line_segment([pos2(plot.min.x, ymap(at)), pos2(plot.max.x, ymap(at))], Stroke::new(1.0, Color32::from_rgb(255, 153, 51)));
                }
                let mut params = json!({"timeOffset": t - g.t, "merge": "graph-key", "fromStart": true});
                if value_ok && c.editable {
                    // Value graph: the value. Speed graph (After Effects): up speeds up the motion
                    // entering and leaving the key, down slows it, along with any move in time,
                    // in one edit from where the drag began.
                    if speed {
                        // Every dimension and every selected property, each by the same share of
                        // its own speed as this key (mixed units move together).
                        params["speedOffset"] = json!(value_offset);
                        params["speedRef"] = kjson.clone();
                    } else {
                        params["valueOffset"] = json!(value_offset);
                        params["dim"] = json!(d);
                    }
                } else if value_ok && speed {
                    params["speedOffset"] = json!(value_offset);
                    params["speedRef"] = kjson.clone();
                }
                actions.push(("keys.transform".into(), params));
            }
            if kresp.drag_stopped() {
                ctx.data_mut(|dd| dd.insert_temp(drag_id, false));
                actions.push(("__endMerge".into(), json!({})));
            }
        }
    }

    if !navigating {
        super::graph_tools::transform_box(app, ui, &pc, &sel_keys, &snap_keys, tm, &ymap, &vmap, speed, true, actions);
    }
    // Handles of the selected keys, registered last so a press on one grabs it rather than the
    // key or the transform box under it (After Effects gives handles priority).
    for c in &cs {
        let (uid, d) = (c.prop.uid, c.dim);
        for k in &c.keys {
            let kp = pos2(tm.x(k.t), ymap(k.v));
            // A key above or below the visible range (a speed spike) keeps its handles: they are
            // pinned inside the plot so they can still be grabbed. Only keys off its time span
            // (or unplottable) have none.
            if !(plot.min.x - 6.0..=plot.max.x + 6.0).contains(&kp.x) || !kp.y.is_finite() {
                continue;
            }
            for h in &k.handles {
                let side = if h.out { "out" } else { "in" };
                let handle_id = tl_id(("ghandle", uid, d, k.idx, side));
                // The handle being dragged follows the pointer instead of staying pinned in the plot.
                let hp = handle_pos(pos2(kp.x, ymap(h.kv)), pos2(tm.x(h.t), ymap(h.v)), h.out, plot, !ctx.is_being_dragged(handle_id));
                pc.circle_filled(hp, 3.5, Color32::from_rgb(0xd8, 0xd8, 0x60));
                let hr = Rect::from_center_size(hp, vec2(HANDLE_GRIP, HANDLE_GRIP));
                let hresp = ui.interact(hr, handle_id, if navigating { Sense::hover() } else { Sense::drag() });
                app.auto_mut().add(&format!("timeline.graph.handle.{uid}.{d}.{}.{side}", k.idx), hr, "Bezier handle");
                let ease = side_ease(&c.prop.keys, k.idx, d, c.prop.spatial, h.out);
                let hresp = hresp.on_hover_text(ease.map_or_else(
                    || "Bezier handle".to_owned(),
                    |e| {
                        format!(
                            "{} · {}\nSpeed: {:.3} {}\nInfluence: {:.2}%\nOption-drag to split / join",
                            c.prop.name,
                            side,
                            e.speed,
                            graph_units(c.prop, true),
                            e.influence * 100.0
                        )
                    },
                ));
                // Every selected key's handle follows the grabbed one (After Effects:
                // `keys.setEaseSelected`, the same influence change and share of the speed; an
                // unselected key's handle moves alone). Option at the press splits a continuous
                // key's handles or joins split ones, for the whole drag.
                if hresp.drag_started() {
                    let continuous = c.prop.keys.get(k.idx).is_some_and(|k| k.continuous);
                    let grab = HandleGrab {
                        layer: c.layer.id,
                        prop: c.prop.uid,
                        dim: c.editable.then_some(c.dim),
                        time: k.time,
                        out: h.out,
                        key: (k.t, h.kv),
                        end: (h.t, h.v),
                        negative: h.negative,
                        dur: h.dur,
                        toggle: ui.input(|i| i.modifiers.alt).then_some(!continuous),
                        linked: if c.linked { (0..shown_dims(c.layer, c.prop)).collect() } else { vec![] },
                    };
                    let press = crate::layer_pointer::Lp::of(ui).pos(ui.input(|i| i.pointer.press_origin())).unwrap_or(hp);
                    ctx.data_mut(|dd| {
                        dd.insert_temp::<HandleDrag>(handle_drag_id(), (press, vec![grab], (hi - lo) / f64::from(plot.height().max(1.0)), tm.pps))
                    });
                }
                if hresp.dragged()
                    && let Some(pt) = hresp.interact_pointer_pos()
                    && let Some((press, grabs, value_per_pixel, pixels_per_second)) = ctx.data(|dd| dd.get_temp::<HandleDrag>(handle_drag_id()))
                {
                    ctx.data_mut(|dd| dd.insert_temp(drag_id, true));
                    // Shift keeps the handle on the time or the value axis, whichever moved more (as for keys).
                    let (time_ok, value_ok) = shift_axes(ui.input(|i| i.modifiers.shift), pt.x - press.x, pt.y - press.y);
                    let dt = if time_ok { f64::from(pt.x - press.x) / pixels_per_second.max(1e-6) } else { 0.0 };
                    let dv = if value_ok { -f64::from(pt.y - press.y) * value_per_pixel } else { 0.0 };
                    // Graph Editor ▸ Snap: the handle's end to the current time and other keys.
                    let snapping = app.ui().timeline.graph_snap != ui.input(|i| i.modifiers.command);
                    let mut candidates = super::graph_tools::comp_snap_times(comp);
                    candidates.extend(cs.iter().flat_map(|c2| c2.keys.iter().map(|k2| k2.t)).filter(|t| (t - k.t).abs() > 1e-9));
                    candidates.push(app.session().time().seconds());
                    let merge = format!("ghandle-{uid}-{}", k.idx);
                    for g in &grabs {
                        let raw_t = g.end.0 + dt;
                        let snapped = (snapping && time_ok).then(|| g.snap_end(raw_t, &candidates, 6.0 / pixels_per_second.max(1e-6))).flatten();
                        if let Some(at) = snapped {
                            pc.line_segment([pos2(tm.x(at), plot.min.y), pos2(tm.x(at), plot.max.y)], Stroke::new(1.0, Color32::from_rgb(255, 153, 51)));
                        }
                        let mut params = g.ease(snapped.unwrap_or(raw_t), g.end.1 + dv, speed, &merge);
                        if let Some(join) = g.toggle {
                            params["continuous"] = json!(join);
                        }
                        if !g.linked.is_empty() {
                            // A linked curve: every dimension, the same influence, its share of the speed.
                            params["dims"] = json!(g.linked);
                        }
                        actions.push(("keys.setEaseSelected".into(), params));
                    }
                }
                if hresp.drag_stopped() {
                    ctx.data_mut(|dd| {
                        dd.insert_temp(drag_id, false);
                        dd.remove::<HandleDrag>(handle_drag_id());
                    });
                    actions.push(("__endMerge".into(), json!({})));
                }
            }
        }
    }

    // Background: click deselects keys, drag box-selects.
    if bg.clicked() {
        actions.push(("keys.select".into(), json!({"keys": []})));
    }
    if let Some(br) = key_box.filter(|_| bg.dragged()) {
        crate::widgets::marquee(&pc, br, &t);
        ctx.data_mut(|d| d.insert_temp(tl_id("graph-box"), br));
    }
    // Releasing the marquee commits the selection its keys showed.
    if bg.drag_stopped()
        && let Some(in_box) = &box_keys
    {
        ctx.data_mut(|d| d.remove::<Rect>(tl_id("graph-box")));
        actions.push(("keys.select".into(), super::graph_tools::marquee_params(in_box, ui.input(|i| i.modifiers), false)));
    }
    navigation(app, ui, comp, tm, plot, (lo, hi), navigating);

    // Button bar.
    p.rect_filled(bar, 0.0, t.panel_bg);
    p.line_segment([bar.left_top(), bar.right_top()], Stroke::new(1.0, t.separator));
    let y = bar.center().y;
    let mut buttons = ButtonBar::new(&ctx, bar);
    let mode = app.ui().timeline.graph_mode.clone();
    let label = if mode == "auto" {
        if speed { "Auto: Speed ▾" } else { "Auto: Value ▾" }
    } else if speed {
        "Speed ▾"
    } else {
        "Value ▾"
    };
    let opts = [
        ("autoSelectGraphType", "Auto-Select Graph Type", mode == "auto"),
        ("valueGraph", "Edit Value Graph", mode == "value"),
        ("speedGraph", "Edit Speed Graph", mode == "speed"),
        ("reference", "Show Reference Graph", app.ui().timeline.graph_reference),
        ("expressionOverlay", "Compare Before/After Expression", app.ui().timeline.graph_expression_overlay),
    ];
    match options_menu(app, ui, p, &mut buttons, y, "typeOptions", label, &opts) {
        Some(0) => set_graph_type(app, "auto"),
        Some(1) => set_graph_type(app, "value"),
        Some(2) => set_graph_type(app, "speed"),
        Some(3) => app.ui_mut().timeline.graph_reference = !app.ui().timeline.graph_reference,
        Some(4) => app.ui_mut().timeline.graph_expression_overlay = !app.ui().timeline.graph_expression_overlay,
        _ => {}
    }
    let opts = [
        ("showSelected", "Show Selected Properties", app.ui().timeline.graph_show_selected && !app.ui().timeline.graph_set_only),
        ("showAnimated", "Show Animated Properties", !app.ui().timeline.graph_show_selected && !app.ui().timeline.graph_set_only),
        ("showSet", "Show Graph Editor Set", app.ui().timeline.graph_show_set),
        ("setOnly", "Graph Editor Set Only", app.ui().timeline.graph_set_only),
    ];
    match options_menu(app, ui, p, &mut buttons, y, "propertyOptions", "Properties ▾", &opts) {
        Some(0) => {
            app.ui_mut().timeline.graph_show_selected = true;
            app.ui_mut().timeline.graph_set_only = false;
        }
        Some(1) => {
            app.ui_mut().timeline.graph_show_selected = false;
            app.ui_mut().timeline.graph_set_only = false;
        }
        Some(2) => {
            app.ui_mut().timeline.graph_show_set = !app.ui().timeline.graph_show_set;
            app.ui_mut().timeline.graph_set_only = false;
        }
        Some(3) => app.ui_mut().timeline.graph_set_only = !app.ui().timeline.graph_set_only,
        _ => {}
    }
    buttons.x += 6.0;
    let auto_on = app.ui().timeline.graph_auto_zoom;
    if text_button(app, ui, p, &mut buttons, y, "Auto-zoom", auto_on, "autoZoom", "Auto-zoom graph height") {
        app.ui_mut().timeline.graph_auto_zoom = !auto_on;
    }
    let fit_tip = match app.session().shortcuts().shortcut_of("timeline.graph.fit", &serde_json::Value::Null) {
        Some(sc) => format!("Fit all graphs to view ({})", crate::menus::shortcut_text(sc)),
        None => "Fit all graphs to view".to_string(),
    };
    if text_button(app, ui, p, &mut buttons, y, "Fit", false, "fit", &fit_tip) {
        fit_all(app, &cs, ectx, speed, plot);
    }
    if text_button(app, ui, p, &mut buttons, y, "Fit Selection", false, "fitSelection", "Fit selection to view") {
        fit_selection(app, &cs, ectx, speed, plot);
    }
    buttons.x += 6.0;
    let (snap, tbox) = (app.ui().timeline.graph_snap, app.ui().timeline.graph_transform_box);
    if text_button(app, ui, p, &mut buttons, y, "Snap", snap, "snap", "Snap to times and values (hold Command/Ctrl while dragging to toggle)") {
        app.ui_mut().timeline.graph_snap = !snap;
    }
    if text_button(app, ui, p, &mut buttons, y, "Transform Box", tbox, "transformBox", "Show Transform Box when multiple keys are selected") {
        app.ui_mut().timeline.graph_transform_box = !tbox;
    }
    buttons.x += 6.0;
    let pos_sel = app.session().state.selected_props.iter().find_map(|(l, u)| super::timeline::is_position(comp.layer(*l)?, *u).then_some(l.0));
    if text_button(app, ui, p, &mut buttons, y, "Separate Dimensions", false, "separateDimensions", "Separate Dimensions (Position)")
        && let Some(l) = pos_sel.or(app.session().state.selected_layers.first().map(|l| l.0))
    {
        actions.push(("prop.separateDimensions".into(), json!({"layer": l})));
    }
    buttons.x += 6.0;
    for (label, cmd, params, tip) in [
        ("Hold", "keys.interpolation", json!({"interpolation": "hold"}), "Convert selected keyframes to Hold"),
        ("Linear", "keys.interpolation", json!({"interpolation": "linear"}), "Convert selected keyframes to Linear"),
        ("Auto Bezier", "keys.interpolation", json!({"interpolation": "autoBezier"}), "Convert selected keyframes to Auto Bezier"),
        ("Easy Ease", "keys.easyEase", json!({}), "Easy Ease (F9)"),
        ("Ease In", "keys.easyEaseIn", json!({}), "Easy Ease In (Shift+F9)"),
        ("Ease Out", "keys.easyEaseOut", json!({}), "Easy Ease Out (Cmd+Shift+F9)"),
    ] {
        let id = cmd.trim_start_matches("keys.").to_string() + label.split_whitespace().next().unwrap_or("");
        if text_button(app, ui, p, &mut buttons, y, label, false, &id, tip) {
            actions.push((cmd.into(), params));
        }
    }
    buttons.more(app, ui, p, bar);
}

fn expression_legend(app: &mut dyn TimelineHost, painter: &egui::Painter, plot: Rect) {
    let before = crate::i18n::tr("Before expression");
    let after = crate::i18n::tr("After expression");
    let font = Tokens::ui(10.0);
    let left = painter.layout_no_wrap(before.into(), font.clone(), Color32::from_gray(130));
    let right = painter.layout_no_wrap(after.into(), font, Color32::from_gray(230));
    let width = left.size().x.max(right.size().x) + 28.0;
    // Leave the units and reference axis readable even in a narrow dock.
    let origin = plot.left_top() + vec2(8.0, 20.0);
    let rect = Rect::from_min_size(origin, vec2(width, 34.0)).intersect(plot);
    painter.rect_filled(rect, 2.0, Color32::from_black_alpha(180));
    for (row, (galley, color)) in [(left, Color32::from_gray(130)), (right, Color32::from_gray(230))].into_iter().enumerate() {
        let at = origin + vec2(4.0, 2.0 + row as f32 * 16.0);
        painter.line_segment([at + vec2(0.0, 6.0), at + vec2(14.0, 6.0)], Stroke::new(1.5, color));
        painter.galley(at + vec2(20.0, 0.0), galley, color);
    }
    app.auto_mut().add("timeline.graph.expressionLegend", rect, "Before expression / After expression");
}

fn graph_units(pr: &Property, speed: bool) -> &'static str {
    use effectcraft_engine::project::ParamUi;
    let value = match pr.ui {
        ParamUi::Angle => "degrees",
        ParamUi::Percent => "%",
        ParamUi::Pixels => "px",
        _ if pr.spatial => "px",
        _ if pr.match_id == "opacity" => "%",
        _ => "units",
    };
    if !speed {
        return value;
    }
    match value {
        "degrees" => "degrees/s",
        "%" => "%/s",
        "px" => "px/s",
        _ => "units/s",
    }
}

fn dimension_name(dim: usize) -> &'static str {
    match dim {
        0 => "X / value",
        1 => "Y",
        2 => "Z",
        _ => "W",
    }
}

#[allow(clippy::too_many_arguments)]
fn navigation(app: &mut dyn TimelineHost, ui: &mut egui::Ui, comp: &Comp, tm: TMap, plot: Rect, range: (f64, f64), navigating: bool) {
    let ctx = ui.ctx().clone();
    let over = ui.rect_contains_pointer(plot);
    let (lo, hi) = range;
    let value_per_pixel = (hi - lo) / f64::from(plot.height().max(1.0));
    let fit = super::timeline::fit_pps(plot.width(), comp);
    let requested_hand =
        navigating && app.ui().tool != crate::state::Tool::Zoom || super::space_hand(&ctx) || ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
    let pan_id = tl_id("graph-pan");
    // A middle press can arrive before this frame's navigation widget participates in
    // hit-testing. Claim it from its press origin, then retain ownership until release.
    let middle_id = pan_id.with("middle");
    if over && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Middle)) {
        ctx.data_mut(|d| {
            d.insert_temp(middle_id, true);
            d.insert_temp(pan_id, true);
            d.insert_temp(pan_id.with("hand"), true);
        });
    }
    let middle_drag = ctx.data(|d| d.get_temp::<bool>(middle_id)).unwrap_or(false) && ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
    let hand = ctx.data(|d| d.get_temp::<bool>(pan_id.with("hand"))).unwrap_or(requested_hand);
    if navigating || ctx.data(|d| d.get_temp::<bool>(pan_id)).unwrap_or(false) {
        let resp = ui.interact(plot, pan_id, Sense::click_and_drag());
        app.auto_mut().add("timeline.graph.navigate", plot, if hand { "Pan graph" } else { "Zoom graph" });
        if over || resp.dragged() {
            ctx.set_cursor_icon(if hand { if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab } } else { egui::CursorIcon::ZoomIn });
        }
        if resp.drag_started() {
            ctx.data_mut(|d| {
                d.insert_temp(pan_id, true);
                d.insert_temp(pan_id.with("hand"), hand);
            });
        }
        if resp.dragged() || middle_drag {
            let delta = crate::layer_pointer::Lp::of(ui).vec(ui.input(|i| i.pointer.delta()));
            if hand {
                app.ui_mut().timeline.start = (app.ui().timeline.start - f64::from(delta.x) / tm.pps.max(1e-6)).clamp(0.0, comp.duration.seconds());
                if !app.ui().timeline.graph_auto_zoom {
                    app.ui_mut().timeline.graph_range = Some((lo + f64::from(delta.y) * value_per_pixel, hi + f64::from(delta.y) * value_per_pixel));
                }
            } else if let Some(at) = resp.interact_pointer_pos() {
                if ui.input(|i| i.modifiers.alt) && delta.y.abs() > delta.x.abs() {
                    if !app.ui().timeline.graph_auto_zoom {
                        let k = (f64::from(delta.y) / 200.0).clamp(-10.0, 10.0).exp();
                        app.ui_mut().timeline.graph_range = bounded_zoom(range, k);
                    }
                } else {
                    let k = (f64::from(delta.x) / 200.0).clamp(-10.0, 10.0).exp();
                    super::timeline::set_zoom(&mut app.ui_mut().timeline, comp, tm.pps * k, fit, tm.t(at.x), f64::from(at.x - tm.x0));
                }
            }
        }
        if resp.clicked()
            && !hand
            && let Some(at) = resp.interact_pointer_pos()
        {
            let k = if ui.input(|i| i.modifiers.alt) { 0.5 } else { 2.0 };
            super::timeline::set_zoom(&mut app.ui_mut().timeline, comp, tm.pps * k, fit, tm.t(at.x), f64::from(at.x - tm.x0));
        }
        if !ui.input(|i| i.pointer.any_down()) {
            ctx.data_mut(|d| {
                d.remove::<bool>(pan_id);
                d.remove::<bool>(pan_id.with("hand"));
                d.remove::<bool>(middle_id);
            });
        }
    }
    if !over {
        return;
    }
    let lp = crate::layer_pointer::Lp::of(ui);
    let (scroll, mods, pinch, at) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers, i.zoom_delta(), lp.pos(i.pointer.hover_pos())));
    let Some(at) = at else {
        return;
    };
    if scroll == egui::Vec2::ZERO && (pinch - 1.0).abs() < 1e-4 {
        return;
    }
    if mods.command || mods.ctrl {
        if !app.ui().timeline.graph_auto_zoom {
            let k = if (pinch - 1.0).abs() > 1e-4 { 1.0 / f64::from(pinch.max(0.001)) } else { (-f64::from(scroll.y) / 200.0).clamp(-10.0, 10.0).exp() };
            app.ui_mut().timeline.graph_range = bounded_zoom(range, k);
        }
    } else if mods.alt || (pinch - 1.0).abs() > 1e-4 {
        let k = if (pinch - 1.0).abs() > 1e-4 { f64::from(pinch) } else { (f64::from(scroll.y) / 200.0).clamp(-10.0, 10.0).exp() };
        super::timeline::set_zoom(&mut app.ui_mut().timeline, comp, tm.pps * k, fit, tm.t(at.x), f64::from(at.x - tm.x0));
    } else {
        let horizontal = if mods.shift { scroll.x + scroll.y } else { scroll.x };
        app.ui_mut().timeline.start = (app.ui().timeline.start - f64::from(horizontal) / tm.pps.max(1e-6)).clamp(0.0, comp.duration.seconds());
        if !mods.shift && !app.ui().timeline.graph_auto_zoom {
            // The curve follows the fingers, as when dragging with the Hand (a wheel delta is
            // the content's move, positive = down).
            let offset = f64::from(scroll.y) * value_per_pixel;
            app.ui_mut().timeline.graph_range = Some((lo + offset, hi + offset));
        }
    }
}

fn bounded_zoom((lo, hi): (f64, f64), factor: f64) -> Option<(f64, f64)> {
    let mid = lo * 0.5 + hi * 0.5;
    let half = ((hi - lo) * 0.5 * factor).clamp(1e-9, 1e15);
    let range = (mid - half, mid + half);
    (range.0.is_finite() && range.1.is_finite() && range.1 > range.0).then_some(range)
}

/// Shortest on-screen length of a handle (points): a tiny influence would leave it under its key.
const MIN_HANDLE_LEN: f32 = 12.0;

/// Side of a handle's grab area (points), larger than its dot so it is easy to catch.
const HANDLE_GRIP: f32 = 14.0;

/// Where a key's handle is drawn and grabbed: its end `raw` (screen), at least
/// [`MIN_HANDLE_LEN`] from the key `kp` (along its direction, or sideways when it has none) and
/// pinned inside the plot when its value lies off the visible range (a speed spike), so it can
/// always be grabbed (not while `pinned` is false: the handle being dragged follows the
/// pointer). Drags move the handle by the pointer's travel, so the shown place only changes where
/// the grab is, not the edit.
fn handle_pos(kp: Pos2, raw: Pos2, out: bool, plot: Rect, pinned: bool) -> Pos2 {
    let inner = plot.shrink(4.0);
    let pin = |q: Pos2| if pinned && inner.is_positive() { q.clamp(inner.min, inner.max) } else { q };
    let side = if out { 1.0 } else { -1.0 };
    let mut q = pin(raw);
    let dir = q - kp;
    let len = dir.length();
    if !len.is_finite() || len < MIN_HANDLE_LEN {
        let unit = if len.is_finite() && len > 0.5 { dir / len } else { vec2(side, 0.0) };
        q = pin(kp + unit * MIN_HANDLE_LEN);
    }
    q
}

/// Show graph type `mode` and remember it (Preferences `graphType`): the Graph Editor opens on
/// it until another type is chosen, across launches.
fn set_graph_type(app: &mut dyn TimelineHost, mode: &str) {
    app.ui_mut().timeline.graph_mode = mode.into();
    if app.session().prefs.graph_type != mode {
        app.session_mut().prefs.graph_type = mode.into();
        app.session().save_prefs();
    }
}

/// Fit shown animation, rather than shrinking a short move into the entire composition.
fn fit_all(app: &mut dyn TimelineHost, cs: &[Curve], ectx: &EvalCtx, speed: bool, plot: Rect) {
    fit_keys(app, cs, ectx, speed, plot, false);
    app.ui_mut().timeline.graph_auto_zoom = true;
}

fn fit_pending_id() -> egui::Id {
    tl_id("graph-fit-pending")
}

fn fit_all_pending_id() -> egui::Id {
    tl_id("graph-fit-all-pending")
}

/// Fit All Graphs to View (F, `timeline.graph.fit`) the next time the Graph Editor is drawn.
pub(crate) fn request_fit_all(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(fit_all_pending_id(), true));
}

/// Takes a pending [`request_fit_all`].
pub(crate) fn take_fit_all_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(fit_all_pending_id())).unwrap_or(false)
}

/// A graph now targets another composition: no gesture may continue editing the previous one.
pub(crate) fn reset_context(ctx: &egui::Context) {
    ctx.stop_dragging();
    ctx.data_mut(|d| {
        d.insert_temp(fit_pending_id(), true);
        d.remove::<KeyGrab>(key_grab_id());
        d.remove::<HandleDrag>(handle_drag_id());
        d.remove::<Rect>(tl_id("graph-box"));
        d.remove::<bool>(tl_id("graph-dragging"));
        let pan = tl_id("graph-pan");
        for id in [pan, pan.with("hand"), pan.with("middle")] {
            d.remove::<bool>(id);
        }
    });
    super::graph_tools::reset_context(ctx);
}

/// Called every Timeline frame: switching the Graph Editor on (header button, menu, Shift+F3 or
/// the control channel) frames the graphs on its first frame, when the plot rect is known, as
/// After Effects does; switching it off puts back the time zoom from before it opened, so the
/// fit doesn't leave the layer bars zoomed in.
pub(crate) fn track_open(ctx: &egui::Context, tl: &mut crate::state::TimelineState) {
    let on = tl.graph_editor;
    let was_id = tl_id("graph-editor-was-on");
    let zoom_id = tl_id("graph-editor-zoom-before");
    let was = ctx.data(|d| d.get_temp::<bool>(was_id)).unwrap_or(false);
    if on && !was {
        reset_context(ctx);
        ctx.data_mut(|d| {
            d.insert_temp(zoom_id, (tl.start, tl.pps));
        });
    } else if was
        && !on
        && let Some((start, pps)) = ctx.data_mut(|d| d.remove_temp::<(f64, Option<f64>)>(zoom_id))
    {
        tl.start = start;
        tl.pps = pps;
    }
    ctx.data_mut(|d| d.insert_temp(was_id, on));
}

/// Zoom time and value to the selected keys (and their handles).
fn fit_selection(app: &mut dyn TimelineHost, cs: &[Curve], ectx: &EvalCtx, speed: bool, plot: Rect) {
    fit_keys(app, cs, ectx, speed, plot, true);
}

fn fit_keys(app: &mut dyn TimelineHost, cs: &[Curve], ectx: &EvalCtx, speed: bool, plot: Rect, selected_only: bool) {
    let mut t0 = f64::INFINITY;
    let mut t1 = f64::NEG_INFINITY;
    let mut v0 = f64::INFINITY;
    let mut v1 = f64::NEG_INFINITY;
    // The keys' own time span, without their handles: one selected key is a lone key however far
    // its handles reach.
    let (mut k0, mut k1) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in cs {
        if !selected_only && let Some(tr) = separated_position(c.layer, c.prop) {
            for key in POSITION_AXES.iter().filter_map(|axis| tr.get(axis)).flat_map(|prop| &prop.keys) {
                let time = c.layer.start_time.seconds() + key.time.seconds() * c.layer.stretch / 100.0;
                if time.is_finite() {
                    t0 = t0.min(time);
                    t1 = t1.max(time);
                }
            }
        }
        for k in c.keys.iter().filter(|k| !selected_only || k.sel) {
            k0 = k0.min(k.t);
            k1 = k1.max(k.t);
            for (t, v) in std::iter::once((k.t, k.v)).chain(k.handles.iter().map(|h| (h.t, h.v))) {
                t0 = t0.min(t);
                t1 = t1.max(t);
                v0 = v0.min(v);
                v1 = v1.max(v);
            }
        }
    }
    if !selected_only {
        // Expressions can animate beyond their base keys; frame their layer's active interval.
        for c in cs.iter().filter(|c| curve_has_expression(c.layer, c.prop)) {
            t0 = t0.min(c.layer.in_point.seconds());
            t1 = t1.max(c.layer.out_point.seconds());
        }
    }
    if !t0.is_finite() || !t1.is_finite() {
        if !selected_only {
            app.ui_mut().timeline.start = 0.0;
            app.ui_mut().timeline.pps = None;
        }
        return;
    }
    // A lone selected key (one point) also frames its neighbouring keys, so its curve shows
    // instead of a flat sliver a fraction of a frame wide.
    if (k1 - k0).is_nan() || k1 - k0 < 1e-6 {
        for c in cs {
            for (i, k) in c.keys.iter().enumerate().filter(|(_, k)| !selected_only || k.sel) {
                for n in [i.checked_sub(1).and_then(|j| c.keys.get(j)), c.keys.get(i + 1), Some(k)].into_iter().flatten() {
                    t0 = t0.min(n.t);
                    t1 = t1.max(n.t);
                    v0 = v0.min(n.v);
                    v1 = v1.max(n.v);
                }
            }
        }
    }
    // And the curves between them: an eased speed graph is 0 at both keys and peaks in between
    // (sampled here: the drawn samples only cover the view before the fit).
    for c in cs.iter().filter(|c| !selected_only || c.keys.iter().any(|k| k.sel)) {
        for i in 0..=FIT_SAMPLES {
            let time = t0 + (t1 - t0) * i as f64 / FIT_SAMPLES as f64;
            let result = sample(ectx, c.layer, c.prop, c.dim, speed, time);
            let base = if c.base_pts.is_empty() { f64::NAN } else { sample_base(ectx, c.layer, c.prop, c.dim, speed, time) };
            for v in [result, base] {
                if v.is_finite() {
                    v0 = v0.min(v);
                    v1 = v1.max(v);
                }
            }
        }
        // A spike narrower than the samples above still fits: each framed segment's extremes.
        for &(_, _, lo, hi) in c.peaks.iter().filter(|(a, b, ..)| *a >= t0 - 1e-9 && *b <= t1 + 1e-9) {
            v0 = v0.min(lo);
            v1 = v1.max(hi);
        }
    }
    let span = (t1 - t0).max(0.2);
    let pps = plot.width() as f64 * 0.8 / span;
    // Through the timeline's zoom, so the fit never zooms out past the whole comp.
    if let Some(comp) = app.session().active_comp_arc() {
        let fit = super::timeline::fit_pps(plot.width(), &comp);
        super::timeline::set_zoom(&mut app.ui_mut().timeline, &comp, pps, fit, (t0 - span * 0.1).max(0.0), 0.0);
    }
    let pad = ((v1 - v0) * 0.15).max(1.0);
    app.ui_mut().timeline.graph_auto_zoom = false;
    app.ui_mut().timeline.graph_range = Some((v0 - pad, v1 + pad));
}

/// Curve samples Fit Selection to View takes between the selected keys.
const FIT_SAMPLES: usize = 200;

fn nice_step(raw: f64) -> f64 {
    let raw = raw.abs().max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    let n = raw / mag;
    mag * if n < 1.5 {
        1.0
    } else if n < 3.5 {
        2.0
    } else if n < 7.5 {
        5.0
    } else {
        10.0
    }
}

fn fmt(v: f64, step: f64) -> String {
    let dec = if step >= 1.0 { 0 } else { (-step.log10()).ceil() as usize };
    let s = format!("{v:.dec$}");
    if s.trim_start_matches('-').chars().all(|c| c == '0' || c == '.') { s.trim_start_matches('-').to_string() } else { s }
}

/// A Graph Editor key drag: where the button went down and the grabbed key's comp time then.
#[derive(Clone, Copy, Debug)]
struct KeyGrab {
    press: Pos2,
    t: f64,
    v: f64,
    value_per_pixel: f64,
}

fn key_grab_id() -> egui::Id {
    tl_id("graph-key-grab")
}

/// A Bezier handle moving with a handle drag: its key, side and dimension, the key's place
/// (comp seconds, graph value) and the handle's end when the drag began.
#[derive(Clone, Debug)]
struct HandleGrab {
    layer: LayerId,
    prop: u64,
    /// `None` for a property drawn as one curve (spatial).
    dim: Option<usize>,
    time: Tick,
    out: bool,
    key: (f64, f64),
    end: (f64, f64),
    /// The speed was negative (the speed graph shows its magnitude).
    negative: bool,
    /// Segment duration (layer seconds) the influence is relative to.
    dur: f64,
    /// Option was held at the press: `Some(true)` joins the key's handles (continuous),
    /// `Some(false)` splits them, for the whole drag.
    toggle: Option<bool>,
    /// A linked speed curve's dimensions, which all move (empty: only `dim`).
    linked: Vec<usize>,
}

impl HandleGrab {
    /// `keys.setEase` putting the handle's end at (`t` comp seconds, `v` in the graph).
    fn ease(&self, t: f64, v: f64, speed: bool, merge: &str) -> serde_json::Value {
        let dir = if self.out { 1.0 } else { -1.0 };
        // 100% influence reaches HANDLE_REACH of the segment (halfway, After Effects).
        let reach = self.dur * HANDLE_REACH;
        let dt = ((t - self.key.0) * dir).max(reach * 0.001);
        let influence = (dt / reach.max(1e-9) * 100.0).clamp(0.1, 100.0);
        let sp = if speed {
            let mag = v.max(0.0);
            if self.negative { -mag } else { mag }
        } else {
            (v - self.key.1) / dt * dir
        };
        let side = if self.out { "out" } else { "in" };
        let mut params =
            json!({"layer": self.layer.0, "prop": self.prop, "time": self.time.seconds(), "side": side, "speed": sp, "influence": influence, "merge": merge});
        if let Some(d) = self.dim {
            params["dim"] = json!(d);
        }
        params
    }
}

impl HandleGrab {
    /// Where Snap puts the handle's end dragged to `raw_t` (comp seconds), if anywhere: the
    /// nearest candidate within `tol` seconds that lies ahead of the key on the handle's side by
    /// more than `tol`. A candidate at the key itself (comp start, the layer's in point, the work
    /// area, the current time, often all on the first key) would pull the end onto the key and
    /// collapse the influence to its 0.1% minimum on the first pixel of a drag; After Effects
    /// changes the influence in proportion to the handle's travel instead.
    fn snap_end(&self, raw_t: f64, candidates: &[f64], tol: f64) -> Option<f64> {
        let dir = if self.out { 1.0 } else { -1.0 };
        let ahead: Vec<f64> = candidates.iter().copied().filter(|c| (c - self.key.0) * dir > tol).collect();
        super::graph_tools::snap_target(raw_t, &ahead, tol)
    }
}

/// A handle drag: where the button went down, and the handles that move.
type HandleDrag = (Pos2, Vec<HandleGrab>, f64, f64);

fn handle_drag_id() -> egui::Id {
    tl_id("graph-handle-drag")
}

/// Which axes a drag may move on (time, value): both, or with Shift only the one the pointer moved more along.
fn shift_axes(shift: bool, dx: f32, dy: f32) -> (bool, bool) {
    if shift { (dx.abs() >= dy.abs(), dy.abs() > dx.abs()) } else { (true, true) }
}

#[cfg(test)]
mod tests {
    use super::shift_axes;

    #[derive(Default)]
    struct FakeExpressions(std::sync::atomic::AtomicUsize);

    impl effectcraft_render::ExprHost for FakeExpressions {
        fn eval(&self, ctx: &super::EvalCtx, _: &super::Layer, prop: &super::Property, value: &super::Value) -> Result<super::Value, String> {
            use std::sync::atomic::Ordering;
            self.0.fetch_add(1, Ordering::Relaxed);
            match prop.expr.as_ref().map(|expression| expression.text.as_str()) {
                Some("error") => Err("synthetic expression error".into()),
                Some("identity") => Ok(value.clone()),
                Some("comp-duration") => Ok(value.with_components(&value.components().iter().map(|v| v + ctx.comp.duration.seconds()).collect::<Vec<_>>())),
                Some("step") => Ok(super::Value::Scalar(if ctx.time.seconds() < 1.0 { 0.0 } else { 100.0 })),
                Some("zero") => Ok(value.with_components(&value.components().iter().map(|_| 0.0).collect::<Vec<_>>())),
                Some("twice") => Ok(value.with_components(&value.components().iter().map(|v| v * 2.0).collect::<Vec<_>>())),
                _ => Ok(value.with_components(&value.components().iter().map(|v| v + 200.0).collect::<Vec<_>>())),
            }
        }
    }

    fn comparison_app(expression: &str) -> (crate::EffectcraftApp, super::LayerId, u64, std::sync::Arc<FakeExpressions>) {
        use super::*;
        let host = std::sync::Arc::new(FakeExpressions::default());
        let mut session = effectcraft_engine::Session { expr: Some(host.clone()), ..Default::default() };
        session.execute("comp.new", json!({"name": "Expression comparison", "width": 32, "height": 32, "frameRate": 30, "duration": 8})).unwrap();
        session.execute("layer.newSolid", json!({"name": "Box", "width": 10, "height": 10})).unwrap();
        let layer = session.active_comp().unwrap().layers[0].id;
        for (time, value) in [(0.0, 0.0), (2.0, 20.0)] {
            session.execute("prop.addKey", json!({"layer": layer.0, "path": "transform/rotation", "time": time, "value": value})).unwrap();
        }
        let uid = session.active_comp().unwrap().layer(layer).unwrap().props.prop("transform/rotation").unwrap().uid;
        session.execute("prop.setExpression", json!({"layer": layer.0, "prop": uid, "expression": expression})).unwrap();
        session.execute("prop.select", json!({"layer": layer.0, "prop": uid})).unwrap();
        let mut app = crate::EffectcraftApp::new(session);
        app.ui.timeline.graph_mode = "value".into();
        (app, layer, uid, host)
    }

    fn comparison_samples(app: &crate::EffectcraftApp, ctx: &egui::Context, speed: bool) -> Vec<super::CurveSamples> {
        use super::*;
        let comp = app.session.active_comp().unwrap();
        let eval = EvalCtx { expr: app.session.expr.as_deref(), ..EvalCtx::new(&app.session.project, app.session.active_comp_id().unwrap(), comp, Tick::ZERO) };
        curves(app, ctx, comp, &eval, TMap { x0: 0.0, start: 0.0, pps: 100.0 }, Rect::from_min_size(Pos2::ZERO, vec2(200.0, 200.0)), speed)
            .into_iter()
            .map(|c| CurveSamples { result: c.pts, base: c.base_pts, peaks: c.peaks })
            .collect()
    }

    fn stored_samples(ctx: &egui::Context, speed: bool) -> super::Samples {
        ctx.data(|data| data.get_temp::<super::Samples>(egui::Id::new(("graph-samples", speed)))).unwrap()
    }

    fn plain_cache_app() -> (crate::EffectcraftApp, super::LayerId, u64, super::LayerId) {
        use super::*;
        let (mut app, layer, uid, _) = comparison_app("identity");
        app.session.execute("prop.setExpression", json!({"layer": layer.0, "prop": uid, "expression": "identity", "enabled": false})).unwrap();
        app.session.execute("layer.newSolid", json!({"name": "Unrelated", "width": 10, "height": 10})).unwrap();
        let other = app.session.active_comp().unwrap().layers.iter().find(|l| l.name == "Unrelated").unwrap().id;
        app.session.execute("prop.select", json!({"layer": layer.0, "prop": uid})).unwrap();
        (app, layer, uid, other)
    }

    #[test]
    fn plain_graph_cache_reuses_samples_after_an_unrelated_layer_edit_and_retains_real_owners() {
        use super::*;
        let (mut app, layer, _, other) = plain_cache_app();
        let ctx = egui::Context::default();
        let first_points = comparison_samples(&app, &ctx, false);
        let before = stored_samples(&ctx, false);
        let owner = app.session.active_comp().unwrap().layers.arcs().iter().find(|l| l.id == layer).unwrap();
        assert!(std::sync::Arc::ptr_eq(&before.owners[0], owner));
        let revision = app.session.revision;
        app.session.execute("prop.set", json!({"layer": other.0, "path": "transform/opacity", "value": 31.0})).unwrap();
        assert!(app.session.revision > revision);
        let after_points = comparison_samples(&app, &ctx, false);
        assert!(std::sync::Arc::ptr_eq(&before, &stored_samples(&ctx, false)), "unrelated layers do not resample plain graphs");
        assert_eq!(after_points[0].result, first_points[0].result);
    }

    #[test]
    fn plain_graph_cache_invalidates_for_owner_key_value_stretch_and_dimension_edits() {
        use super::*;
        for edit in ["key", "value", "stretch", "3d"] {
            let (mut app, layer, uid, _) = plain_cache_app();
            if edit == "3d" {
                let position = app.session.active_comp().unwrap().layer(layer).unwrap().props.prop("transform/position").unwrap().uid;
                app.session.execute("prop.select", json!({"layer": layer.0, "prop": position})).unwrap();
            }
            let ctx = egui::Context::default();
            let first_points = comparison_samples(&app, &ctx, false);
            let before = stored_samples(&ctx, false);
            match edit {
                "key" => {
                    app.session.execute("prop.addKey", json!({"layer": layer.0, "prop": uid, "time": 1.0, "value": 50.0})).unwrap();
                }
                "value" => {
                    app.session.execute("prop.set", json!({"layer": layer.0, "prop": uid, "value": 77.0})).unwrap();
                }
                "stretch" => {
                    app.session.execute("layer.timeStretch", json!({"layers": [layer.0], "percent": 200.0})).unwrap();
                }
                _ => {
                    app.session.execute("layer.setSwitch", json!({"layers": [layer.0], "switch": "threeD", "value": true})).unwrap();
                }
            }
            let after_points = comparison_samples(&app, &ctx, false);
            assert!(!std::sync::Arc::ptr_eq(&before, &stored_samples(&ctx, false)), "{edit} changes the shown owner");
            if edit == "3d" {
                assert_eq!(first_points.len(), 2);
                assert_eq!(after_points.len(), 3);
                let warm = comparison_samples(&app, &ctx, false);
                assert!(warm.iter().zip(&after_points).all(|(a, b)| a.result == b.result), "warm multi-dimensional curves must keep separate point lists");
            } else {
                assert_ne!(first_points[0].result, after_points[0].result, "{edit} must change the visible curve");
            }
        }
    }

    #[test]
    fn separated_follower_edits_invalidate_without_relying_on_session_revision() {
        use super::*;
        let (mut app, layer, _, _) = plain_cache_app();
        app.session.execute("prop.separateDimensions", json!({"layer": layer.0, "value": true})).unwrap();
        let uid = app.session.active_comp().unwrap().layer(layer).unwrap().props.prop("transform/position").unwrap().uid;
        app.session.execute("prop.select", json!({"layer": layer.0, "prop": uid})).unwrap();
        let ctx = egui::Context::default();
        let initial = comparison_samples(&app, &ctx, false);
        let before = stored_samples(&ctx, false);
        let revision = app.session.revision;
        let cid = app.session.active_comp_id().unwrap();
        // A source snapshot can change through COW independently of the outer Session cursor.
        let follower =
            std::sync::Arc::make_mut(&mut app.session.project).comp_mut(cid).unwrap().layer_mut(layer).unwrap().props.prop_mut("transform/positionX").unwrap();
        follower.value = Value::Scalar(123.0);
        let after = comparison_samples(&app, &ctx, false);
        assert_eq!(app.session.revision, revision);
        assert!(!std::sync::Arc::ptr_eq(&before, &stored_samples(&ctx, false)));
        assert_ne!(initial[0].result, after[0].result);
        assert!(after[0].result.iter().all(|(_, value)| *value == 123.0));
    }

    #[test]
    fn enabling_an_expression_restores_global_invalidation_including_separated_followers() {
        use super::*;
        for separate in [false, true] {
            let (mut app, layer, mut uid, _) = plain_cache_app();
            let path = if separate {
                app.session.execute("prop.separateDimensions", json!({"layer": layer.0, "value": true})).unwrap();
                uid = app.session.active_comp().unwrap().layer(layer).unwrap().props.prop("transform/position").unwrap().uid;
                app.session.execute("prop.select", json!({"layer": layer.0, "prop": uid})).unwrap();
                "transform/positionX"
            } else {
                "transform/rotation"
            };
            let ctx = egui::Context::default();
            comparison_samples(&app, &ctx, false);
            let plain = stored_samples(&ctx, false);
            app.session.execute("prop.setExpression", json!({"layer": layer.0, "path": path, "expression": "comp-duration", "enabled": true})).unwrap();
            let first = comparison_samples(&app, &ctx, false);
            let expression = stored_samples(&ctx, false);
            assert!(!std::sync::Arc::ptr_eq(&plain, &expression));
            app.session.execute("comp.settings", json!({"duration": 16.0})).unwrap();
            let changed = comparison_samples(&app, &ctx, false);
            let global_edit = stored_samples(&ctx, false);
            assert!(expression.owners.iter().zip(&global_edit.owners).all(|(a, b)| std::sync::Arc::ptr_eq(a, b)), "only composition data changed");
            assert!(!std::sync::Arc::ptr_eq(&expression, &global_edit), "expression can read non-owner state, separate={separate}, prop={uid}");
            assert!(first[0].result.iter().zip(&changed[0].result).all(|(a, b)| (b.1 - a.1 - 8.0).abs() < 1e-8));
        }
    }

    #[test]
    fn graph_cache_keeps_view_graph_type_and_composition_invalidation() {
        use super::*;
        let (mut app, _, _, _) = plain_cache_app();
        let ctx = egui::Context::default();
        comparison_samples(&app, &ctx, false);
        let initial = stored_samples(&ctx, false);
        let project = app.session.project.clone();
        let cid = app.session.active_comp_id().unwrap();
        let comp = project.comp(cid).unwrap();
        let eval = EvalCtx { expr: app.session.expr.as_deref(), ..EvalCtx::new(&project, cid, comp, Tick::ZERO) };
        let tm = TMap { x0: 0.0, start: 0.5, pps: 150.0 };
        let plot = Rect::from_min_size(Pos2::ZERO, vec2(240.0, 200.0));
        curves(&app, &ctx, comp, &eval, tm, plot, false);
        let view_changed = stored_samples(&ctx, false);
        assert!(!std::sync::Arc::ptr_eq(&initial, &view_changed));
        // Same shared owners/UIDs can appear in snapshots; the composition namespace still matters.
        let other_comp = EvalCtx { comp_id: effectcraft_engine::project::ItemId(cid.0 + 1), ..eval };
        curves(&app, &ctx, comp, &other_comp, tm, plot, false);
        let comp_changed = stored_samples(&ctx, false);
        assert!(!std::sync::Arc::ptr_eq(&view_changed, &comp_changed));
        app.ui.timeline.graph_mode = "speed".into();
        comparison_samples(&app, &ctx, true);
        assert!(!std::sync::Arc::ptr_eq(&comp_changed, &stored_samples(&ctx, true)));
    }

    #[test]
    fn expression_overlay_pairs_values_and_caches_both_without_extra_expression_calls() {
        use std::sync::atomic::Ordering;
        let (mut app, _, _, host) = comparison_app("offset");
        let ctx = egui::Context::default();
        let revision = app.session.revision;
        let without = comparison_samples(&app, &ctx, false);
        assert!(without[0].base.is_empty());
        app.ui.timeline.graph_expression_overlay = true;
        let paired = comparison_samples(&app, &ctx, false);
        let curve = &paired[0];
        assert_eq!(curve.result, without[0].result);
        assert_eq!(curve.base.first().unwrap().1, 0.0);
        assert_eq!(curve.base.last().unwrap().1, 20.0);
        assert!(curve.result.iter().zip(&curve.base).all(|(after, before)| (after.1 - before.1 - 200.0).abs() < 1e-8));
        let calls = host.0.load(Ordering::Relaxed);
        let warm = comparison_samples(&app, &ctx, false);
        assert_eq!(warm[0].base, curve.base);
        assert_eq!(host.0.load(Ordering::Relaxed), calls, "base samples never invoke expressions; warm curves reuse both lists");
        // Source context can lose its expression host without changing the document revision.
        app.session.expr = None;
        let no_host = comparison_samples(&app, &ctx, false);
        assert!(no_host[0].base.is_empty());
        assert_eq!(no_host[0].result, curve.base);
        assert_eq!(app.session.revision, revision);
    }

    #[test]
    fn expression_overlay_disabled_and_error_states_use_the_renderers_base_fallback() {
        use super::*;
        let (mut app, layer, uid, _) = comparison_app("error");
        let ctx = egui::Context::default();
        app.ui.timeline.graph_expression_overlay = true;
        let error = comparison_samples(&app, &ctx, false);
        assert_eq!(error[0].result, error[0].base);
        app.session.execute("prop.setExpression", json!({"layer": layer.0, "prop": uid, "expression": "offset", "enabled": false})).unwrap();
        let disabled = comparison_samples(&app, &ctx, false);
        assert!(disabled[0].base.is_empty());
        assert_eq!(disabled[0].result, error[0].base);
    }

    #[test]
    fn expression_overlay_identity_and_error_speed_match_base_at_smooth_segments_and_hold_jumps() {
        use super::*;
        for expression in ["identity", "error"] {
            for hold in [false, true] {
                let (mut app, layer, uid, _) = comparison_app(expression);
                if hold {
                    app.session.execute("keys.select", json!({"keys": [{"layer": layer.0, "prop": uid, "time": 0.0}]})).unwrap();
                    app.session.execute("keys.interpolation", json!({"interpolation": "hold"})).unwrap();
                }
                app.ui.timeline.graph_mode = "speed".into();
                app.ui.timeline.graph_expression_overlay = true;
                let curves = comparison_samples(&app, &egui::Context::default(), true);
                assert!(curves[0].result.iter().zip(&curves[0].base).all(|(after, before)| (after.1 - before.1).abs() < 1e-6), "{expression}, hold={hold}");
                if hold {
                    assert!(curves[0].result.last().unwrap().1 > 10_000.0, "both phases show the same jump approximation");
                }
            }
        }
    }

    #[test]
    fn expression_overlay_compares_separated_position_speed_in_layer_seconds() {
        use super::*;
        let (mut app, layer, _, _) = comparison_app("twice");
        for (time, x) in [(0.0, 0.0), (2.0, 20.0)] {
            app.session.execute("prop.addKey", json!({"layer": layer.0, "path": "transform/position", "time": time, "value": [x, 0.0, 0.0]})).unwrap();
        }
        let uid = app.session.active_comp().unwrap().layer(layer).unwrap().props.prop("transform/position").unwrap().uid;
        app.session.execute("prop.separateDimensions", json!({"layer": layer.0, "value": true})).unwrap();
        app.session.execute("prop.setExpression", json!({"layer": layer.0, "path": "transform/positionX", "expression": "twice"})).unwrap();
        app.session.execute("prop.select", json!({"layer": layer.0, "prop": uid})).unwrap();
        app.session.execute("layer.timeStretch", json!({"layers": [layer.0], "percent": 200.0})).unwrap();
        app.ui.timeline.graph_mode = "speed".into();
        app.ui.timeline.graph_expression_overlay = true;
        let ctx = egui::Context::default();
        let curves = comparison_samples(&app, &ctx, true);
        assert_eq!(curves.len(), 1);
        assert!((curves[0].base[50].1 - 10.0).abs() < 1e-4);
        assert!((curves[0].result[50].1 - 20.0).abs() < 1e-4);
        // Legacy Reference Graph displays the other graph type, without another comparison pair.
        let reference = comparison_samples(&app, &ctx, false);
        assert!(reference.iter().all(|c| c.base.is_empty()));
    }

    #[test]
    fn expression_overlay_keeps_discontinuities_separate_from_the_smooth_base() {
        use super::*;
        let (mut app, layer, uid, _) = comparison_app("step");
        app.ui.timeline.graph_expression_overlay = true;
        let curves = comparison_samples(&app, &egui::Context::default(), false);
        assert!(curves[0].result.iter().all(|(_, value)| *value == 0.0 || *value == 100.0));
        assert!(curves[0].base.iter().all(|(_, value)| (0.0..=20.0).contains(value)));
        let comp = app.session.active_comp().unwrap();
        let l = comp.layer(layer).unwrap();
        let prop = l.props.find(uid).unwrap();
        let eval = EvalCtx { expr: app.session.expr.as_deref(), ..EvalCtx::new(&app.session.project, app.session.active_comp_id().unwrap(), comp, Tick::ZERO) };
        let speed_at_jump = sample(&eval, l, prop, 0, true, 1.0);
        assert!(speed_at_jump.is_finite() && speed_at_jump > 100_000.0, "a discontinuity remains a finite numerical derivative spike");
    }

    #[test]
    fn expression_overlay_fit_includes_base_curve_extrema_even_when_result_is_flat() {
        use super::*;
        let (mut app, layer, uid, _) = comparison_app("zero");
        for (time, side, speed) in [(0.0, "out", 500.0), (2.0, "in", -500.0)] {
            app.session.execute("keys.setEase", json!({"layer": layer.0, "prop": uid, "time": time, "side": side, "speed": speed, "influence": 50.0})).unwrap();
        }
        // New keys are selected and a selected key shows its (far out) handles in the fit; this
        // test is about the curve extrema, so leave the keys unselected.
        app.session.state.selected_keys.clear();
        let project = app.session.project.clone();
        let expr = app.session.expr.clone();
        let comp = project.comp(app.session.active_comp_id().unwrap()).unwrap();
        let eval = EvalCtx { expr: expr.as_deref(), ..EvalCtx::new(&project, app.session.active_comp_id().unwrap(), comp, Tick::ZERO) };
        let ctx = egui::Context::default();
        let plot = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 200.0));
        let tm = TMap { x0: 0.0, start: 0.0, pps: 100.0 };
        for compare in [false, true] {
            app.ui.timeline.graph_expression_overlay = compare;
            let cs = curves(&app, &ctx, comp, &eval, tm, plot, false);
            fit_all(&mut app, &cs, &eval, false, plot);
            let (_, hi) = app.ui.timeline.graph_range.unwrap();
            assert!(hi.is_finite());
            if compare {
                assert!(hi > 100.0, "base curve extrema must fit: {hi}");
            } else {
                assert!(hi < 30.0, "result-only view retains existing fit behavior: {hi}");
            }
        }
    }

    #[test]
    fn changing_graph_context_cancels_old_gestures_and_requests_fit() {
        use super::*;
        let ctx = egui::Context::default();
        ctx.set_dragged_id(egui::Id::new("graph-bg"));
        ctx.data_mut(|d| {
            d.insert_temp(key_grab_id(), KeyGrab { press: Pos2::ZERO, t: 1.0, v: 2.0, value_per_pixel: 1.0 });
            d.insert_temp(handle_drag_id(), (Pos2::ZERO, Vec::<HandleGrab>::new(), 1.0, 100.0));
            d.insert_temp(egui::Id::new("graph-box"), Rect::EVERYTHING);
            d.insert_temp(egui::Id::new("graph-dragging"), true);
            d.insert_temp(egui::Id::new("graph-pan"), true);
        });
        reset_context(&ctx);
        assert!(ctx.dragged_id().is_none());
        ctx.data(|d| {
            assert_eq!(d.get_temp::<bool>(fit_pending_id()), Some(true));
            assert!(d.get_temp::<KeyGrab>(key_grab_id()).is_none());
            assert!(d.get_temp::<HandleDrag>(handle_drag_id()).is_none());
            assert!(d.get_temp::<Rect>(egui::Id::new("graph-box")).is_none());
            assert!(d.get_temp::<bool>(egui::Id::new("graph-dragging")).is_none());
            assert!(d.get_temp::<bool>(egui::Id::new("graph-pan")).is_none());
        });
    }

    #[test]
    fn pinned_separated_position_uses_follower_expressions_keys_and_fit_bounds() {
        use super::*;
        let mut s = effectcraft_host::session();
        s.execute("comp.new", json!({"name": "Separated speed", "frameRate": 25, "duration": 20})).unwrap();
        s.execute("layer.newSolid", json!({"name": "Box", "width": 10, "height": 10})).unwrap();
        let cid = s.state.active_comp.unwrap();
        let lid = s.active_comp().unwrap().layers[0].id;
        let uid = s.active_comp().unwrap().layer(lid).unwrap().props.prop("transform/position").unwrap().uid;
        s.execute("prop.addKey", json!({"layer": lid.0, "path": "transform/position", "time": 10.0, "value": [300.0, 200.0, 0.0]})).unwrap();
        s.execute("prop.graphPin", json!({"layer": lid.0, "prop": uid, "value": true})).unwrap();
        s.execute("prop.separateDimensions", json!({"layer": lid.0, "value": true})).unwrap();
        s.execute("prop.setExpression", json!({"layer": lid.0, "path": "transform/positionX", "expression": "time * 30"})).unwrap();
        let l = std::sync::Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(lid).unwrap();
        l.in_point = Tick::from_seconds_f64(3.0);
        l.out_point = Tick::from_seconds_f64(6.0);
        // Removing follower keys leaves the obsolete combined keys in the document.
        for axis in POSITION_AXES {
            l.props.sub_mut("transform").unwrap().get_mut(axis).unwrap().keys.clear();
        }
        let mut app = crate::EffectcraftApp::new(s);
        app.ui.timeline.graph_set_only = true;
        app.ui.timeline.graph_mode = "speed".into();
        let plot = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 400.0));
        for expressions in [true, false] {
            if !expressions {
                app.session
                    .execute("prop.setExpression", json!({"layer": lid.0, "path": "transform/positionX", "expression": "time * 30", "enabled": false}))
                    .unwrap();
                for (time, value) in [(4.0, 0.0), (5.0, 100.0)] {
                    app.session.execute("prop.addKey", json!({"layer": lid.0, "path": "transform/positionX", "time": time, "value": value})).unwrap();
                }
                // No expression engine is needed to sample composed follower keyframes.
                app.session.expr = None;
            }
            let project = app.session.project.clone();
            let expr = app.session.expr.clone();
            let comp = project.comp(cid).unwrap();
            let layer = comp.layer(lid).unwrap();
            let prop = layer.props.find(uid).unwrap();
            let ctx = EvalCtx { expr: expr.as_deref(), ..EvalCtx::new(&project, cid, comp, Tick::ZERO) };
            assert_eq!(shown_props(&app, comp), vec![(lid, uid)]);
            assert_eq!(curve_has_expression(layer, prop), expressions);
            assert!((sample(&ctx, layer, prop, 0, true, 4.5) - if expressions { 30.0 } else { 100.0 }).abs() < 1e-4);
            let curves = curves(&app, &egui::Context::default(), comp, &ctx, TMap { x0: 0.0, start: 0.0, pps: 40.0 }, plot, true);
            assert!(curves.iter().all(|c| c.keys.is_empty()));
            fit_all(&mut app, &curves, &ctx, true, plot);
            let span = f64::from(plot.width()) / app.ui.timeline.pps.unwrap();
            assert!((span - if expressions { 3.75 } else { 1.25 }).abs() < 1e-6);
            assert!((app.ui.timeline.start - if expressions { 2.7 } else { 3.9 }).abs() < 1e-6);
        }
    }

    #[test]
    fn expression_speed_samples_evaluated_values_and_preserves_layer_time_units() {
        use super::*;
        let mut s = effectcraft_host::session();
        s.execute("comp.new", json!({"name": "Expression speed", "frameRate": 25, "duration": 20})).unwrap();
        s.execute("layer.newSolid", json!({"name": "Box", "width": 10, "height": 10})).unwrap();
        let cid = s.state.active_comp.unwrap();
        let lid = s.active_comp().unwrap().layers[0].id;
        for (time, value) in [(0.0, 0.0), (10.0, 100.0)] {
            s.execute("prop.addKey", json!({"layer": lid.0, "path": "transform/rotation", "time": time, "value": value})).unwrap();
        }
        for stretch in [100.0_f64, 200.0, -100.0] {
            for (expression, expected) in [("time * 30", 30.0 * stretch.abs() / 100.0), ("value", 10.0), ("throw new Error('test')", 10.0)] {
                s.execute("prop.setExpression", json!({"layer": lid.0, "path": "transform/rotation", "expression": expression})).unwrap();
                let comp = s.active_comp().unwrap();
                let mut layer = comp.layer(lid).unwrap().clone();
                layer.stretch = stretch;
                layer.start_time = Tick::from_seconds_f64(8.0);
                let prop = layer.props.prop("transform/rotation").unwrap();
                let ctx = EvalCtx { expr: s.expr.as_deref(), ..EvalCtx::new(&s.project, cid, comp, Tick::ZERO) };
                let ct = layer.comp_time(Tick::from_seconds_f64(2.0)).seconds();
                let speed = sample(&ctx, &layer, prop, 0, true, ct);
                assert!((speed - expected).abs() < 1e-4, "{expression}, stretch {stretch}: {speed} != {expected}");
            }
        }
        s.execute("prop.setExpression", json!({"layer": lid.0, "path": "transform/position", "expression": "[time * 3, time * 4, 0]"})).unwrap();
        let comp = s.active_comp().unwrap();
        let layer = comp.layer(lid).unwrap();
        let ctx = EvalCtx { expr: s.expr.as_deref(), ..EvalCtx::new(&s.project, cid, comp, Tick::ZERO) };
        let prop = layer.props.prop("transform/position").unwrap();
        assert!((sample(&ctx, layer, prop, 0, true, 2.0) - 5.0).abs() < 1e-4);
    }

    #[test]
    fn expression_speed_rejects_nonfinite_results_and_disabled_expressions_use_keys() {
        use super::*;
        struct Nonfinite;
        impl effectcraft_render::ExprHost for Nonfinite {
            fn eval(&self, _: &EvalCtx, _: &Layer, _: &Property, _: &Value) -> Result<Value, String> {
                Ok(Value::Scalar(f64::INFINITY))
            }
        }
        let mut s = effectcraft_engine::Session::default();
        s.execute("comp.new", json!({"name": "Invalid expression", "frameRate": 25, "duration": 10})).unwrap();
        s.execute("layer.newSolid", json!({"name": "Box", "width": 10, "height": 10})).unwrap();
        let cid = s.state.active_comp.unwrap();
        let lid = s.active_comp().unwrap().layers[0].id;
        for (time, value) in [(0.0, 0.0), (10.0, 100.0)] {
            s.execute("prop.addKey", json!({"layer": lid.0, "path": "transform/rotation", "time": time, "value": value})).unwrap();
        }
        s.execute("prop.setExpression", json!({"layer": lid.0, "path": "transform/rotation", "expression": "Infinity"})).unwrap();
        let comp = s.active_comp().unwrap();
        let layer = comp.layer(lid).unwrap();
        let mut prop = layer.props.prop("transform/rotation").unwrap().clone();
        let ctx = EvalCtx { expr: Some(&Nonfinite), ..EvalCtx::new(&s.project, cid, comp, Tick::ZERO) };
        assert_eq!(sample(&ctx, layer, &prop, 0, true, 2.0), 0.0);
        assert_eq!(sample(&ctx, layer, &prop, 0, true, f64::NAN), 0.0);
        let mut malformed = layer.clone();
        malformed.start_time = Tick(i64::MIN);
        assert_eq!(sample(&ctx, &malformed, &prop, 0, true, 2.0), 0.0);
        prop.expr.as_mut().unwrap().enabled = false;
        assert!((sample(&ctx, layer, &prop, 0, true, 2.0) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_never_collapses_the_value_axis() {
        let (lo, hi) = super::bounded_zoom((0.0, 100.0), 0.0).unwrap();
        assert!(hi > lo);
        assert!(super::bounded_zoom((f64::NAN, 100.0), 1.0).is_none());
    }

    #[test]
    fn shift_locks_drag_to_dominant_axis() {
        assert_eq!(shift_axes(false, 10.0, 3.0), (true, true));
        assert_eq!(shift_axes(true, 10.0, -3.0), (true, false));
        assert_eq!(shift_axes(true, 2.0, -9.0), (false, true));
        assert_eq!(shift_axes(true, 5.0, 5.0), (true, false));
    }

    /// GE-HANDLE1: a 2-point drag of the first key's out handle in the speed graph (key at comp 0,
    /// where comp start, the in point, the work area and the playhead all snap) changed the
    /// influence from 30% to the 0.1% minimum: Snap pulled the handle's end onto its own key.
    #[test]
    fn small_handle_drag_changes_influence_a_little() {
        let grab = super::HandleGrab {
            layer: super::LayerId(1),
            prop: 7,
            dim: None,
            time: super::Tick::ZERO,
            out: true,
            key: (0.0, 0.0),
            // 30% influence of a 1 s segment (100% reaches halfway).
            end: (0.15, 0.0),
            negative: false,
            dur: 1.0,
            toggle: None,
            linked: vec![],
        };
        let pixels_per_second = 100.0;
        let tol = 6.0 / pixels_per_second;
        // The pointer moves 2 points left: 0.02 s at 100 points per second.
        let raw_t = grab.end.0 + -2.0 / pixels_per_second;
        let candidates = [0.0, 0.0, 0.0, 5.0];
        let snapped = grab.snap_end(raw_t, &candidates, tol);
        assert_eq!(snapped, None, "the key's own time is no snap target for its handle");
        let ease = grab.ease(snapped.unwrap_or(raw_t), 0.0, true, "m");
        let influence = ease["influence"].as_f64().unwrap_or(0.0);
        assert!((influence - 26.0).abs() < 1e-6, "2 points of travel move the influence 4 points, got {influence}");
        // A candidate ahead of the key still snaps, and so does one behind on the in side.
        assert_eq!(grab.snap_end(0.31, &[0.0, 0.3], tol), Some(0.3));
        let in_grab = super::HandleGrab { out: false, key: (1.0, 0.0), end: (0.85, 0.0), ..grab };
        assert_eq!(in_grab.snap_end(0.99, &[1.0], tol), None);
        assert_eq!(in_grab.snap_end(0.81, &[0.8, 1.0], tol), Some(0.8));
    }
}

#[cfg(test)]
mod interaction_profile {
    use super::*;
    use std::{hint::black_box, time::Instant};

    #[test]
    #[ignore = "explicit CPU profiling probe; run without concurrent builds"]
    fn graph_sampling_cost() {
        let repeats = std::env::var("EC_PROFILE_REPEATS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(25).clamp(5, 200);
        for (count, expression) in [(1, false), (20, false), (100, false), (1, true), (20, true)] {
            let mut session = effectcraft_host::session();
            session.execute("comp.new", json!({"name":"Synthetic graph probe","duration":10})).unwrap();
            let mut selected = Vec::new();
            for i in 0..count {
                let lid = session.execute("layer.newSolid", json!({"name":format!("Probe {i}"),"width":10,"height":10})).unwrap()["layer"].as_u64().unwrap();
                for (time, value) in [(0.0, 0.0), (1.0, 90.0)] {
                    session.execute("prop.addKey", json!({"layer":lid,"path":"transform/rotation","time":time,"value":value})).unwrap();
                }
                if expression {
                    session.execute("prop.setExpression", json!({"layer":lid,"path":"transform/rotation","expression":"Math.sin(time * 3) * 90"})).unwrap();
                }
                let uid = session.active_comp().unwrap().layer(LayerId(lid)).unwrap().props.prop("transform/rotation").unwrap().uid;
                selected.push((LayerId(lid), uid));
            }
            let other = session.execute("layer.newSolid", json!({"name":"Unrelated layer","width":10,"height":10})).unwrap()["layer"].as_u64().unwrap();
            session.state.selected_props = selected;
            let mut app = EffectcraftApp::new(session);
            app.ui.timeline.graph_show_selected = true;
            let ctx = egui::Context::default();
            let plot = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 480.0));
            let tm = TMap { x0: 0.0, start: 0.0, pps: 144.0 };
            for invalidate in [false, true] {
                let mut samples = Vec::new();
                let mut point_count = 0;
                for i in 0..repeats + 3 {
                    if invalidate {
                        app.session.execute("prop.set", json!({"layer":other,"path":"transform/opacity","value":if i%2==0 {50.0}else{100.0}})).unwrap();
                    }
                    let cid = app.session.active_comp_id().unwrap();
                    let comp = app.session.active_comp().unwrap();
                    let ectx = EvalCtx { expr: app.session.expr.as_deref(), ..EvalCtx::new(&app.session.project, cid, comp, app.session.time()) };
                    let t = Instant::now();
                    let values = curves(&app, &ctx, comp, &ectx, tm, plot, true);
                    point_count = values.iter().map(|c| c.pts.len()).sum::<usize>();
                    black_box(&values);
                    drop(values);
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    if i >= 3 {
                        samples.push(ms);
                    }
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "EC_PROFILE {}",
                    json!({"case":"graph","curves":count,"expressions":expression,"unrelated_edit":invalidate,
                    "points":point_count,"samples":samples.len(),"p50_ms":samples[samples.len()/2],"p95_ms":samples[(samples.len()*95/100).min(samples.len()-1)],
                    "raw_ms":samples})
                );
            }
        }
    }
}
