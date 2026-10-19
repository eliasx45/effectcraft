//! An inline gradient stops bar (EffectCraft), like Photoshop's gradient editor but in the panel:
//! the gradient's preview with its colour stops below it. Click under the bar to add a stop (it
//! takes the colour there), drag a stop along the bar to move it (between its neighbours), drag
//! it down off the bar to remove it, click a stop to select it, then its swatch to recolour it;
//! Delete (−) removes the selected stop and "Edit…" opens the full Gradient Editor (opacity
//! stops, midpoints). Every change is one `prop.set` (a drag, a recolour: one undo step each; a
//! keyframe at the current time when the property is animated). A gradient keeps at least two
//! colour stops.
//!
//! Used by the Properties panel for text Gradient Fill animators (Gradient Fill Colors).
//!
//! Automation ids (`<prefix>` from the caller): `<prefix>.bar`, `<prefix>.stop.<i>` (colour
//! stops in order along the bar), `<prefix>.color`, `<prefix>.location`, `<prefix>.delete`,
//! `<prefix>.edit`.

use effectcraft_engine::keyframe::Gradient;
use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::json;

use crate::theme::Tokens;
use crate::{EffectcraftApp, widgets};

type Actions = Vec<(String, serde_json::Value)>;

/// Height the bar takes: preview, stop track and the selected stop's row.
pub const HEIGHT: f32 = 62.0;

/// How far below the stop track a dragged stop is removed (points).
const TEAR_OFF: f32 = 28.0;

fn c32(c: [f32; 4]) -> Color32 {
    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(u(c[0]), u(c[1]), u(c[2]))
}

/// `g` with colour stop `i` moved to `pos` (midpoints kept: the stops keep their order).
pub fn moved(g: &Gradient, i: usize, pos: f64) -> Gradient {
    let mut out = g.clone();
    let lo = i.checked_sub(1).and_then(|j| g.colors.get(j)).map_or(0.0, |s| s.0);
    let hi = g.colors.get(i + 1).map_or(1.0, |s| s.0);
    if let Some(s) = out.colors.get_mut(i) {
        s.0 = if pos.is_finite() { pos.clamp(lo.min(hi), hi.max(lo)) } else { s.0 };
    }
    out
}

/// `g` with a colour stop added at `pos` (the colour there); returns it and the stop's index.
pub fn added(g: &Gradient, pos: f64) -> (Gradient, usize) {
    let pos = if pos.is_finite() { pos.clamp(0.0, 1.0) } else { 0.5 };
    let mut out = g.clone();
    let i = g.colors.iter().position(|s| s.0 > pos).unwrap_or(g.colors.len());
    out.colors.insert(i, (pos, g.color_at(pos)));
    out.color_midpoints.clear();
    (out, i)
}

/// `g` without colour stop `i` (never below two stops: `None`).
pub fn removed(g: &Gradient, i: usize) -> Option<Gradient> {
    if g.colors.len() <= 2 || i >= g.colors.len() {
        return None;
    }
    let mut out = g.clone();
    out.colors.remove(i);
    out.color_midpoints.clear();
    Some(out)
}

/// `g` with colour stop `i` recoloured to `rgb` (its alpha kept).
pub fn recolored(g: &Gradient, i: usize, rgb: [f32; 3]) -> Gradient {
    let mut out = g.clone();
    if let Some(s) = out.colors.get_mut(i) {
        s.1 = [rgb[0], rgb[1], rgb[2], s.1[3]];
    }
    out
}

fn set(actions: &mut Actions, layer: u64, prop: u64, g: &Gradient, merge: Option<String>) {
    let mut p = json!({"layer": layer, "prop": prop, "value": g});
    if let Some(m) = merge {
        p["merge"] = json!(m);
    }
    actions.push(("prop.set".into(), p));
}

/// Draw the bar for gradient property `prop` (value `g`) of layer `layer` in `rect` (its width;
/// [`HEIGHT`] tall) and queue its edits in `actions`.
#[allow(clippy::too_many_arguments)]
pub fn show(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    p: &egui::Painter,
    layer: u64,
    prop: u64,
    g: &Gradient,
    rect: Rect,
    prefix: &str,
    actions: &mut Actions,
) {
    let t = app.tokens;
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), 16.0));
    let track = Rect::from_min_size(pos2(bar.min.x, bar.max.y), vec2(bar.width(), 16.0));
    let sel_id = egui::Id::new(("gradient-stops-selected", prop));
    let mut selected: usize = ui.data(|d| d.get_temp(sel_id)).unwrap_or(0);
    if selected >= g.colors.len() {
        selected = g.colors.len().saturating_sub(1);
    }
    // Preview: the colour stops (opacity shown over a checkerboard).
    let n = (bar.width() / 3.0).clamp(8.0, 160.0) as usize;
    for (k, x0) in (0..n).map(|k| (k, bar.min.x + bar.width() * k as f32 / n as f32)) {
        let x1 = bar.min.x + bar.width() * (k + 1) as f32 / n as f32;
        let c = g.sample((k as f64 + 0.5) / n as f64);
        let cell = Rect::from_min_max(pos2(x0, bar.min.y), pos2(x1, bar.max.y));
        let checker = if (k / 2) % 2 == 0 { Color32::from_gray(200) } else { Color32::from_gray(150) };
        p.rect_filled(cell, 0.0, checker);
        p.rect_filled(cell, 0.0, c32(c).gamma_multiply(c[3].clamp(0.0, 1.0)));
    }
    p.rect_stroke(bar, 2.0, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
    app.auto.add(&format!("{prefix}.bar"), bar, "Gradient");
    let x_of = |pos: f64| bar.min.x + bar.width() * pos.clamp(0.0, 1.0) as f32;
    let pos_of = |x: f32| ((x - bar.min.x) / bar.width().max(1.0)) as f64;
    let merge = |what: &str| format!("gradient-stops-{prop}-{what}");

    // Clicking the track where there is no stop adds one.
    let track_resp = ui.interact(track, egui::Id::new(("gradient-stops-track", prop)), Sense::click());
    let mut on_stop = false;
    let mut removing: Option<(usize, String)> = None;
    for (i, &(pos, col)) in g.colors.iter().enumerate() {
        let x = x_of(pos);
        let hr = Rect::from_center_size(pos2(x, track.center().y), vec2(12.0, track.height()));
        let id = egui::Id::new(("gradient-stop", prop, i));
        let resp = ui.interact(hr, id, Sense::click_and_drag());
        app.auto.add(&format!("{prefix}.stop.{i}"), hr, &format!("Color stop {}", i + 1));
        on_stop |= resp.hovered() || resp.dragged();
        if resp.clicked() || resp.drag_started() {
            selected = i;
        }
        let tearing = resp.dragged() && resp.interact_pointer_pos().is_some_and(|q| q.y > track.max.y + TEAR_OFF) && g.colors.len() > 2;
        if resp.dragged()
            && !tearing
            && let Some(q) = resp.interact_pointer_pos()
        {
            let key = super::effect_controls::gesture_key(ui, id, resp.drag_started());
            let next = moved(g, i, pos_of(q.x));
            if next != *g {
                set(actions, layer, prop, &next, Some(key));
            }
        }
        // Torn off while dragging: removed on release, in the drag's undo step.
        let tear_id = id.with("tear");
        if resp.dragged() {
            ui.data_mut(|d| d.insert_temp(tear_id, tearing));
        }
        if resp.drag_stopped() && ui.data_mut(|d| d.remove_temp::<bool>(tear_id)).unwrap_or(false) {
            removing = Some((i, super::effect_controls::gesture_key(ui, id, false)));
        }
        // The stop: a small pointer up to the bar and a swatch of its colour.
        let fade = if tearing { 0.35 } else { 1.0 };
        let edge = if i == selected {
            t.accent
        } else if resp.hovered() {
            t.text
        } else {
            t.text_dim
        };
        let tip = pos2(x, track.min.y + 1.0);
        p.add(egui::Shape::convex_polygon(
            vec![tip, pos2(x + 4.5, track.min.y + 6.0), pos2(x - 4.5, track.min.y + 6.0)],
            edge.gamma_multiply(fade),
            Stroke::NONE,
        ));
        let sw = Rect::from_center_size(pos2(x, track.min.y + 10.5), vec2(9.0, 9.0));
        p.rect_filled(sw, 1.5, c32(col).gamma_multiply(fade));
        p.rect_stroke(sw, 1.5, Stroke::new(if i == selected { 1.5 } else { 1.0 }, edge.gamma_multiply(fade)), StrokeKind::Outside);
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }
    if let Some((i, key)) = removing
        && let Some(next) = removed(g, i)
    {
        selected = i.saturating_sub(1);
        set(actions, layer, prop, &next, Some(key));
    } else if !on_stop
        && track_resp.clicked()
        && let Some(q) = track_resp.interact_pointer_pos()
    {
        let (next, i) = added(g, pos_of(q.x));
        selected = i;
        set(actions, layer, prop, &next, None);
    }

    // The selected stop: its colour, location and Delete; Edit… opens the Gradient Editor.
    let row_y = track.max.y + 8.0;
    if let Some(&(pos, col)) = g.colors.get(selected) {
        let sw = Rect::from_min_size(pos2(bar.min.x, row_y), vec2(18.0, 18.0));
        let pop = egui::Id::new(("gradient-stop-color", prop));
        if widgets::swatch(ui, sw, col, egui::Id::new(("gradient-stop-swatch", prop)), &t).clicked() {
            widgets::open_popup(ui, pop);
        }
        app.auto.add(&format!("{prefix}.color"), sw, "Stop Color");
        let mut rgb = [col[0], col[1], col[2]];
        if widgets::color_popup(ui, pop, sw.left_bottom(), &mut rgb) {
            set(actions, layer, prop, &recolored(g, selected, rgb), Some(merge("color")));
        }
        let lr = p.text(pos2(sw.max.x + 8.0, sw.center().y), Align2::LEFT_CENTER, format!("{:.0}%", pos * 100.0), Tokens::ui(12.0), t.text_dim);
        app.auto.add(&format!("{prefix}.location"), lr, "Location");
        let dr = Rect::from_min_size(pos2(lr.max.x + 10.0, row_y), vec2(22.0, 18.0));
        let can = g.colors.len() > 2;
        let dresp = ui.interact(dr, egui::Id::new(("gradient-stop-delete", prop)), if can { Sense::click() } else { Sense::hover() });
        p.rect_stroke(dr, 3.0, Stroke::new(1.0, if dresp.hovered() && can { t.text_dim } else { t.field_border }), StrokeKind::Inside);
        p.text(dr.center(), Align2::CENTER_CENTER, "−", Tokens::ui(13.0), if can { t.text } else { t.text_faint });
        app.auto.add(&format!("{prefix}.delete"), dr, "Delete Stop");
        let dresp = dresp.on_hover_text(crate::i18n::tr("Delete the selected colour stop (or drag it down off the bar)"));
        if dresp.clicked()
            && let Some(next) = removed(g, selected)
        {
            selected = selected.saturating_sub(1);
            set(actions, layer, prop, &next, None);
        }
    }
    let er = Rect::from_min_size(pos2(bar.max.x - 56.0, row_y), vec2(56.0, 18.0));
    if widgets::text_button(ui, er, &crate::i18n::tr("Edit…"), false, &t, egui::Id::new(("gradient-stops-edit", prop))).clicked() {
        actions.push(("prop.editGradient".into(), json!({"layer": layer, "prop": prop})));
    }
    app.auto.add(&format!("{prefix}.edit"), er, "Edit Gradient…");
    ui.data_mut(|d| d.insert_temp(sel_id, selected));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g3() -> Gradient {
        Gradient { colors: vec![(0.0, [1.0, 0.0, 0.0, 1.0]), (0.5, [0.0, 1.0, 0.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])], ..Gradient::default() }
    }

    #[test]
    fn stop_edits_keep_order_and_two_stops() {
        let g = g3();
        // A move stays between the neighbours.
        assert_eq!(moved(&g, 1, 0.9).colors[1].0, 0.9);
        assert_eq!(moved(&g, 1, 1.5).colors[1].0, 1.0);
        assert_eq!(moved(&g, 0, f64::NAN).colors[0].0, 0.0);
        // An added stop takes the colour where it lands, in order.
        let (a, i) = added(&g, 0.25);
        assert_eq!(i, 1);
        assert_eq!(a.colors.len(), 4);
        assert_eq!(a.colors[1].1, g.color_at(0.25));
        // Never fewer than two colour stops.
        let r = removed(&g, 1).unwrap();
        assert_eq!(r.colors.len(), 2);
        assert!(removed(&r, 0).is_none());
        assert!(removed(&g, 9).is_none());
        assert_eq!(recolored(&g, 2, [1.0, 1.0, 0.0]).colors[2].1, [1.0, 1.0, 0.0, 1.0]);
    }
}
