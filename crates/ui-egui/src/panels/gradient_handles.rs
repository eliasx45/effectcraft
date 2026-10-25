//! A shape layer's Gradient Fill / Gradient Stroke start and end points in the Composition viewer
//! (#612). With the gradient, or a property in it, selected, its Start Point and End Point show as
//! draggable ⊕ controls with a guide between them, as in After Effects. A drag sets the point with
//! `prop.set`: one undo step per drag, and on an animated point a keyframe at the current time.
//!
//! The points are in the space of the shape group holding the gradient, so they follow that
//! group's transform (and the groups around it) as well as the layer's.
//!
//! Automation ids: `viewer.gradientPoint.<uid>` (the Start Point / End Point property's uid).
//!
//! Text layers' Gradient Fill animators (an EffectCraft extension) get the same kind of control
//! ([`text_hook`]): with the layer selected, the Gradient Start and End of each Gradient Fill
//! animator (only the selected one while an animator, or something in it, is selected) show as
//! two dots joined by a line on the first group of characters the animator's selector picks,
//! where the animation puts it now. A drag sets Gradient Start / End in percent of the group's
//! box (one undo step per drag, a keyframe at the current time when animated) and snaps to the
//! box's edges and centre within 6 points; hold Alt (Option) to place freely. Automation ids:
//! `viewer.textGradient.<animator uid>.start` / `.end`.

use effectcraft_engine::geom::Mat3;
use effectcraft_engine::project::{Layer, LayerId, Node, PropGroup, Property};
use effectcraft_engine::render::EvalCtx;
use egui::{Color32, Rect, Sense, Stroke, vec2};
use serde_json::json;

use super::fx_widgets as fw;
use crate::EffectcraftApp;

/// Gradient Fill / Gradient Stroke groups in shape contents `g`, through nested groups.
fn gradient_paints<'a>(g: &'a PropGroup, depth: usize, out: &mut Vec<&'a PropGroup>) {
    if depth >= 16 {
        return;
    }
    for child in g.groups().filter(|c| c.enabled) {
        match child.match_id.as_str() {
            "gfill" | "gstroke" => out.push(child),
            "group" | "contents" => gradient_paints(child, depth + 1, out),
            _ => {}
        }
    }
}

/// `g` or something inside it is selected on `layer`.
fn selected_in(g: &PropGroup, layer: LayerId, selected: &[(LayerId, u64)]) -> bool {
    fn contains(g: &PropGroup, uid: u64, depth: usize) -> bool {
        g.uid == uid || (depth < 16 && g.children.iter().any(|n| n.uid() == uid || matches!(n, Node::Group(c) if contains(c, uid, depth + 1))))
    }
    selected.iter().any(|(l, uid)| *l == layer && contains(g, *uid, 0))
}

/// Draw and drag the selected gradients' start and end points. `l2c` is the viewer's layer→comp
/// matrix (it knows the 3D view). Called after the viewer's own gestures, so a handle takes the
/// press before the layer would move.
pub fn viewer_hook(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    map: &super::viewer::ViewerMap,
    ectx: &EvalCtx,
    l2c: &dyn Fn(&EvalCtx, &Layer) -> Mat3,
) {
    if !app.ui.viewer.show_layer_controls {
        return;
    }
    let t = app.tokens;
    let ctx = ui.ctx().clone();
    let selected_props = app.session.state.selected_props.clone();
    let selected_layers = app.session.state.selected_layers.clone();
    let mut actions: Vec<(String, serde_json::Value)> = vec![];
    for layer in ectx.comp.layers.iter().filter(|l| selected_layers.contains(&l.id) && l.is_active_at(ectx.time)) {
        let Some(contents) = layer.props.sub("contents") else { continue };
        let mut paints = vec![];
        gradient_paints(contents, 0, &mut paints);
        if paints.is_empty() {
            continue;
        }
        let m = l2c(ectx, layer);
        for g in paints.into_iter().filter(|g| selected_in(g, layer.id, &selected_props)) {
            // Gradient (group) space → comp: the enclosing groups' transforms, then the layer's.
            let pm = m * effectcraft_engine::render::shapes::item_matrix(ectx, layer, g.uid).unwrap_or(Mat3::IDENTITY);
            let pts: Vec<&Property> = ["start", "end"].iter().filter_map(|k| g.get(k)).collect();
            let screen: Vec<egui::Pos2> = pts.iter().map(|pr| map.to_screen(fw::layer_to_comp(&pm, ectx.value(layer, pr).as_vec2()))).collect();
            if let [a, b] = screen[..] {
                painter.line_segment([a, b], Stroke::new(1.0, Color32::from_white_alpha(110)));
            }
            for (pr, s) in pts.iter().zip(screen) {
                let hr = Rect::from_center_size(s, vec2(16.0, 16.0));
                let id = egui::Id::new(("viewer-gradient-pt", pr.uid));
                let resp = ui.interact(hr, id, Sense::drag());
                let col = if resp.hovered() || resp.dragged() { t.accent } else { Color32::WHITE };
                fw::draw_crosshair(painter, s + vec2(1.0, 1.0), 5.0, Color32::from_black_alpha(160));
                fw::draw_crosshair(painter, s, 5.0, col);
                app.auto.add(&format!("viewer.gradientPoint.{}", pr.uid), hr, &pr.name);
                if resp.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::Move);
                    resp.clone().on_hover_text(format!("{}: {}", g.name, pr.name));
                }
                if resp.dragged()
                    && let Some(pos) = resp.interact_pointer_pos()
                    && let Some(lp) = fw::comp_to_layer(&pm, map.to_comp(pos))
                {
                    let key = super::effect_controls::gesture_key(ui, id, resp.drag_started());
                    actions.push(("prop.set".into(), json!({"layer": layer.id.0, "prop": pr.uid, "value": [lp[0], lp[1]], "merge": key})));
                }
            }
        }
    }
    text_hook(app, ui, painter, map, ectx, l2c, &mut actions);
    for (id, params) in actions {
        if let Err(e) = crate::menus::invoke(app, &ctx, &id, params) {
            app.ui.status = e;
        }
    }
}

/// Snap distance (points) of a text gradient handle to its group's edges and centre.
const SNAP_PT: f32 = 6.0;

/// The Gradient Fill animators of a text layer to show handles for: the selected one when an
/// animator (or something in it) is selected, else all of them. (animator uid, Start, End).
fn text_gradients<'a>(layer: &'a Layer, selected: &[(LayerId, u64)]) -> Vec<(u64, &'a Property, &'a Property)> {
    let Some(anims) = layer.props.group("text/animators") else { return vec![] };
    let all: Vec<(u64, &Property, &Property)> = anims
        .groups()
        .filter(|a| a.enabled)
        .filter_map(|a| {
            let props = a.sub("properties")?;
            props.get("gradientFillColors")?;
            Some((a.uid, props.get("gradientStart")?, props.get("gradientEnd")?))
        })
        .collect();
    let picked = anims.groups().find(|a| selected_in(a, layer.id, selected)).map(|a| a.uid);
    match picked {
        Some(uid) => all.into_iter().filter(|g| g.0 == uid).collect(),
        None => all,
    }
}

/// Draw and drag the Gradient Start / End of the selected text layers' Gradient Fill animators
/// (see the module docs).
fn text_hook(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    map: &super::viewer::ViewerMap,
    ectx: &EvalCtx,
    l2c: &dyn Fn(&EvalCtx, &Layer) -> Mat3,
    actions: &mut Vec<(String, serde_json::Value)>,
) {
    use effectcraft_engine::render::text::{GradGroup, text_geom};
    let t = app.tokens;
    let ctx = ui.ctx().clone();
    let selected_props = app.session.state.selected_props.clone();
    let selected_layers = app.session.state.selected_layers.clone();
    for layer in ectx.comp.layers.iter().filter(|l| selected_layers.contains(&l.id) && l.is_active_at(ectx.time)) {
        let grads = text_gradients(layer, &selected_props);
        if grads.is_empty() {
            continue;
        }
        let Some(geom) = text_geom(ectx, layer) else { continue };
        let lm = l2c(ectx, layer);
        for (anim, start_p, end_p) in grads {
            let Some(group) = geom.grad_groups.iter().find(|g| g.animator == anim) else { continue };
            let Some(glyph) = geom.glyphs.iter().find(|g| g.char_index == group.char_index) else { continue };
            // The unanimated layout → where the animation draws the group → comp → screen.
            let m = lm * glyph.m2() * Mat3::translate(effectcraft_engine::geom::vec2(-glyph.rest[0], -glyph.rest[1]));
            let to_screen = |p: [f64; 2]| map.to_screen(fw::layer_to_comp(&m, p));
            let (a, b) = (to_screen(group.start), to_screen(group.end));
            painter.line_segment([a, b], Stroke::new(3.0, Color32::from_black_alpha(90)));
            painter.line_segment([a, b], Stroke::new(1.5, Color32::WHITE));
            let colors = glyph.fills.as_ref().and_then(|f| {
                f.layers.iter().find_map(|l| match l {
                    effectcraft_engine::render::text::GlyphFill::Grad { spec, .. } => Some((spec.gradient.sample(0.0), spec.gradient.sample(1.0))),
                    _ => None,
                })
            });
            for (which, pr, s, col) in [("start", start_p, a, colors.map(|c| c.0)), ("end", end_p, b, colors.map(|c| c.1))] {
                let hr = Rect::from_center_size(s, vec2(18.0, 18.0));
                let id = egui::Id::new(("viewer-text-gradient", anim, which));
                let resp = ui.interact(hr, id, Sense::drag());
                let ring = if resp.hovered() || resp.dragged() { t.accent } else { Color32::WHITE };
                painter.circle_filled(s + vec2(0.0, 1.0), 7.5, Color32::from_black_alpha(110));
                painter.circle_filled(s, 7.0, ring);
                let fill = col.map_or(Color32::GRAY, |c| {
                    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    Color32::from_rgb(u(c[0]), u(c[1]), u(c[2]))
                });
                painter.circle_filled(s, 4.5, fill);
                app.auto.add(&format!("viewer.textGradient.{anim}.{which}"), hr, &pr.name);
                if resp.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::Move);
                    resp.clone().on_hover_text(pr.name.clone());
                }
                if resp.dragged()
                    && let Some(pos) = resp.interact_pointer_pos()
                    && let Some(inv) = m.inverse()
                {
                    let c = map.to_comp(pos);
                    let q = inv.apply(effectcraft_engine::geom::vec2(c[0], c[1]));
                    let mut pct = GradGroup::to_percent(group.bbox, [q.x, q.y]);
                    if !ui.input(|i| i.modifiers.alt) {
                        // Snap each axis to the box's edges and centre within SNAP_PT points.
                        for d in 0..2 {
                            for target in [0.0, 50.0, 100.0] {
                                let mut sp = pct;
                                sp[d] = target;
                                if (to_screen(GradGroup::from_percent(group.bbox, sp)) - pos).length() <= SNAP_PT {
                                    pct = sp;
                                    break;
                                }
                            }
                        }
                    }
                    if pct.iter().all(|v| v.is_finite()) {
                        let key = super::effect_controls::gesture_key(ui, id, resp.drag_started());
                        actions.push(("prop.set".into(), json!({"layer": layer.id.0, "prop": pr.uid, "value": [pct[0], pct[1]], "merge": key})));
                    }
                }
            }
        }
    }
}
