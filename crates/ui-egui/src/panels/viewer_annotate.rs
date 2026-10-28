//! PN1: Pen and Comment on the live Composition viewer (`annotate.*`, see
//! `effectcraft_engine::commands::annotate`).
//!
//! While a tool is armed (`annotate.start`, from Ghostex's ✏️ Pen · Comment strip, an agent or the
//! control channel) the viewer's pointer is the tool's: Pen drags a freehand stroke, Comment clicks
//! a point, both kept in comp pixels so they stay on the picture under zoom and pan. Then a box pops
//! from the mark, as in HyperFrames: the Pen's compact bar ("Drawing" chip, "Say what should change
//! (optional)", esc, a round send arrow) or the Comment's card (timecode pill, a multi-line field,
//! "Enter to add", Cancel / Comment). Enter adds (`annotate.add`; Shift+Enter is a new line), Esc
//! cancels, Esc again disarms. Hosted (Ghostex in process) the mark becomes a card queued above
//! the chat's composer, never sent; standalone, Enter copies a "Copy for Claude" reference.
//! Marks waiting for their card are drawn faintly on their frame.
//!
//! CDXC:Editing 2026-10-28 DECISION: Elias: "why does the pen appear here and not under the comp like it does in hyperframes?", "its just that the popup appears directly in the comp im viewing", "instead of in the chat, but then when i press enter it gets queued in the chat", "call it the comment tool plz and make sure it works like it does in hyperframes in terms of ui/ux smoothly pop.. same for the draw boxes". The buttons stay in Ghostex's strip above the Claude panel.

use effectcraft_engine::commands::annotate::{Mark, Tool, bbox};
use effectcraft_engine::project::Comp;
use effectcraft_engine::time::Tick;
use egui::emath::TSTransform;
use egui::{Align2, Color32, CornerRadius, Key, Modifiers, Pos2, Rect, Sense, Stroke, pos2, vec2};
use serde_json::json;

use super::viewer::ViewerMap;
use crate::EffectcraftApp;
use crate::theme::Tokens;

/// HyperFrames' mark colour (its teal-green pen).
const INK: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x9b);
/// A stroke keeps a point only once the pointer moved this far (screen points).
const STEP: f32 = 2.0;
/// Pop in / out (seconds), ease-out.
const POP_IN: f64 = 0.15;
const POP_OUT: f64 = 0.10;
const PEN_BAR_WIDTH: f32 = 360.0;
const COMMENT_WIDTH: f32 = 300.0;

/// The box waiting for its note.
#[derive(Clone, Debug)]
struct Draft {
    tool: Tool,
    comp: u64,
    time: f64,
    timecode: String,
    points: Vec<[f64; 2]>,
    text: String,
    opened: f64,
    /// Closing since (pop out), then dropped.
    closing: Option<f64>,
    focus: bool,
}

#[derive(Clone, Debug, Default)]
struct Live {
    /// The Pen's stroke while the button is down (comp pixels) and its last screen point.
    stroke: Vec<[f64; 2]>,
    last: Option<Pos2>,
    draft: Option<Draft>,
    /// The arm the viewer last paused playback for.
    serial: u64,
}

fn live_id() -> egui::Id {
    egui::Id::new("viewer-annotate")
}

fn ease_out(p: f64) -> f32 {
    let p = p.clamp(0.0, 1.0);
    (1.0 - (1.0 - p).powi(3)) as f32
}

/// Pop progress of a box opened at `opened` (closing at `closing`): (opacity, scale), and whether
/// it is still moving. Motion off (egui's animation time 0): no pop.
pub(crate) fn pop(now: f64, opened: f64, closing: Option<f64>, motion: bool) -> (f32, f32, bool) {
    if !motion {
        return (if closing.is_some() { 0.0 } else { 1.0 }, 1.0, false);
    }
    match closing {
        Some(c) => {
            let e = ease_out((now - c) / POP_OUT);
            (1.0 - e, 1.0 - 0.02 * e, e < 1.0)
        }
        None => {
            let e = ease_out((now - opened) / POP_IN);
            (e, 0.96 + 0.04 * e, e < 1.0)
        }
    }
}

/// Where a box of `size` goes for a mark (screen): a comment's card above-right of its point, the
/// Pen's bar under the stroke's box (above it when there is no room); kept inside `area`.
pub(crate) fn box_origin(tool: Tool, mark: Rect, size: egui::Vec2, area: Rect) -> Pos2 {
    let want = match tool {
        Tool::Comment => pos2(mark.center().x + 14.0, mark.center().y - size.y - 14.0),
        Tool::Pen => {
            let below = mark.max.y + 10.0;
            let y = if below + size.y <= area.max.y { below } else { mark.min.y - size.y - 10.0 };
            pos2(mark.min.x, y)
        }
    };
    let x = want.x.min(area.max.x - size.x - 8.0).max(area.min.x + 8.0);
    let y = want.y.min(area.max.y - size.y - 8.0).max(area.min.y + 8.0);
    pos2(x, y)
}

fn screen_rect(map: &ViewerMap, points: &[[f64; 2]]) -> Rect {
    let [x, y, w, h] = bbox(points);
    Rect::from_two_pos(map.to_screen([x, y]), map.to_screen([x + w, y + h])).expand(4.0)
}

fn draw_mark(painter: &egui::Painter, map: &ViewerMap, tool: Tool, points: &[[f64; 2]], alpha: f32) {
    let ink = INK.gamma_multiply(alpha);
    match tool {
        Tool::Pen => {
            let pts: Vec<Pos2> = points.iter().map(|p| map.to_screen(*p)).collect();
            if let [only] = pts.as_slice() {
                painter.circle_filled(*only, 2.0, ink);
            } else {
                painter.add(egui::Shape::line(pts, Stroke::new(3.0, ink)));
            }
        }
        Tool::Comment => {
            if let Some(p) = points.first() {
                let at = map.to_screen(*p);
                painter.circle_filled(at, 7.0, ink);
                painter.circle_stroke(at, 7.0, Stroke::new(2.0, Color32::WHITE.gamma_multiply(alpha)));
            }
        }
    }
}

/// The marks waiting for their card, on the frame they were drawn on (faint).
fn draw_waiting(app: &mut EffectcraftApp, painter: &egui::Painter, map: &ViewerMap, cid: u64, frame: i64) {
    let marks: Vec<Mark> = app.session.annotations.marks.iter().filter(|m| m.comp == cid && m.frame == frame).cloned().collect();
    for m in &marks {
        draw_mark(painter, map, m.tool, &m.points, 0.45);
        app.auto.add(&format!("viewer.annotate.mark.{}", m.id), screen_rect(map, &m.points), if m.note.is_empty() { "Review mark" } else { &m.note });
    }
}

/// Over the viewer, after its own interaction (its overlay layer sits above the viewer, so the
/// viewer's tools don't see the presses).
pub(crate) fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, map: &ViewerMap, cid: u64, comp: &Comp, time: Tick) {
    let ctx = ui.ctx().clone();
    let mut live: Live = ctx.data(|d| d.get_temp(live_id())).unwrap_or_default();
    let armed = app.session.annotations.armed;
    // Arming pauses playback (once per arm: Space still plays afterwards).
    if armed.is_some() && live.serial != app.session.annotations.arm_serial {
        live.serial = app.session.annotations.arm_serial;
        if app.playback.playing {
            app.stop();
        }
    }
    let painter = ui.painter().with_clip_rect(map.area);
    let frame = comp.frame_rate.frame_at(time);
    if app.session.annotations.hosted || armed.is_some() {
        draw_waiting(app, &painter, map, cid, frame);
    }
    // A draft of another comp (the viewer switched comps): dropped.
    if live.draft.as_ref().is_some_and(|d| d.comp != cid) {
        live.draft = None;
    }
    if armed.is_none() && live.draft.is_none() {
        live.stroke.clear();
        live.last = None;
        ctx.data_mut(|d| d.insert_temp(live_id(), live));
        return;
    }
    let now = ctx.input(|i| i.time);
    let motion = ui.style().animation_time > 0.0;

    // Esc: the box first, then the tool.
    let esc = ctx.input(|i| i.key_pressed(Key::Escape));
    if esc {
        match live.draft.as_mut() {
            Some(d) if d.closing.is_none() => d.closing = Some(now),
            Some(_) => {}
            None => {
                let _ = app.session.execute("annotate.stop", json!({}));
                live.stroke.clear();
            }
        }
    }

    // The pointer layer over the viewer area.
    let layer = egui::Area::new(egui::Id::new("viewer-annotate-layer"))
        .order(egui::Order::Foreground)
        .fixed_pos(map.area.min)
        .fade_in(false)
        .constrain(false)
        .show(&ctx, |ui| ui.allocate_response(map.area.size(), Sense::click_and_drag()));
    let resp = layer.inner;
    app.auto.add_response(
        "viewer.annotate.layer",
        &resp,
        match armed {
            Some(Tool::Pen) => "Pen: draw on the comp",
            Some(Tool::Comment) => "Comment: click the comp",
            None => "Review mark",
        },
    );
    if armed.is_some() && resp.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    let open = live.draft.as_ref().is_some_and(|d| d.closing.is_none());
    if let (Some(tool), false) = (armed, open) {
        match tool {
            Tool::Pen => {
                if resp.drag_started()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    live.stroke = vec![map.to_comp(pos)];
                    live.last = Some(pos);
                }
                if resp.dragged()
                    && let Some(pos) = resp.interact_pointer_pos()
                    && live.last.is_none_or(|l| l.distance(pos) >= STEP)
                {
                    live.stroke.push(map.to_comp(pos));
                    live.last = Some(pos);
                }
                if resp.drag_stopped() {
                    let points = std::mem::take(&mut live.stroke);
                    live.last = None;
                    if points.len() >= 2 {
                        live.draft = Some(new_draft(app, comp, cid, time, Tool::Pen, points, now));
                    }
                }
            }
            Tool::Comment => {
                if resp.clicked()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    live.draft = Some(new_draft(app, comp, cid, time, Tool::Comment, vec![map.to_comp(pos)], now));
                }
            }
        }
    }
    if live.stroke.len() > 1 {
        draw_mark(&painter, map, Tool::Pen, &live.stroke, 1.0);
        ctx.request_repaint();
    }

    // The box.
    let mut done: Option<bool> = None;
    if let Some(draft) = live.draft.as_mut() {
        draw_mark(&painter, map, draft.tool, &draft.points, 1.0);
        let (alpha, scale, moving) = pop(now, draft.opened, draft.closing, motion);
        if moving {
            ctx.request_repaint();
        }
        if draft.closing.is_some() && !moving {
            done = Some(false);
        } else {
            done = draft_box(app, &ctx, map, draft, alpha, scale);
        }
    }
    match done {
        Some(true) => {
            if let Some(d) = live.draft.as_mut() {
                add(app, &ctx, d);
                d.closing = Some(now);
            }
        }
        Some(false) if live.draft.as_ref().is_some_and(|d| d.closing.is_some_and(|c| !motion || now - c >= POP_OUT)) => live.draft = None,
        Some(false) => {
            if let Some(d) = live.draft.as_mut().filter(|d| d.closing.is_none()) {
                d.closing = Some(now);
                ctx.request_repaint();
            }
        }
        None => {}
    }
    ctx.data_mut(|d| d.insert_temp(live_id(), live));
}

fn new_draft(app: &EffectcraftApp, comp: &Comp, cid: u64, time: Tick, tool: Tool, points: Vec<[f64; 2]>, now: f64) -> Draft {
    let t = comp.frame_rate.snap(time);
    Draft {
        tool,
        comp: cid,
        time: t.seconds(),
        timecode: effectcraft_engine::commands::time::display_time(&app.session, comp, t),
        points,
        text: String::new(),
        opened: now,
        closing: None,
        focus: true,
    }
}

/// `annotate.add`; standalone, the reference goes on the clipboard.
fn add(app: &mut EffectcraftApp, ctx: &egui::Context, d: &Draft) {
    let params = json!({"comp": d.comp, "time": d.time, "tool": d.tool.id(), "points": d.points, "note": d.text.trim(), "source": "viewer"});
    let now = ctx.input(|i| i.time);
    match app.session.execute("annotate.add", params) {
        Ok(r) if app.session.annotations.hosted => {
            let _ = r;
            app.toast = Some(("Queued in the chat".into(), now));
        }
        Ok(r) => {
            if let Some(text) = r["copyRef"].as_str() {
                ctx.copy_text(text.to_string());
                app.toast = Some(("Copied for Claude: paste it in the chat".into(), now));
            }
        }
        Err(e) => app.ui.status = e.to_string(),
    }
}

/// Draws the box; `Some(true)`: add, `Some(false)`: cancel.
fn draft_box(app: &mut EffectcraftApp, ctx: &egui::Context, map: &ViewerMap, d: &mut Draft, alpha: f32, scale: f32) -> Option<bool> {
    let t = app.tokens;
    let mark = screen_rect(map, &d.points);
    let width = match d.tool {
        Tool::Pen => PEN_BAR_WIDTH,
        Tool::Comment => COMMENT_WIDTH,
    };
    // The box's size from last frame (its height grows with the note).
    let size_id = egui::Id::new("viewer-annotate-box-size");
    let size = ctx.data(|m| m.get_temp::<egui::Vec2>(size_id)).unwrap_or(vec2(width, if d.tool == Tool::Pen { 44.0 } else { 132.0 }));
    let origin = box_origin(d.tool, mark, size, map.area);
    // Pops from the mark: scaled around the corner nearest it.
    let pivot = match d.tool {
        Tool::Comment => pos2(origin.x, origin.y + size.y),
        Tool::Pen => origin,
    };
    let area_id = egui::Id::new("viewer-annotate-box");
    let layer = egui::LayerId::new(egui::Order::Foreground, area_id);
    ctx.set_transform_layer(layer, TSTransform::new(pivot.to_vec2() * (1.0 - scale), scale));
    let mut result = None;
    let text_id = egui::Id::new("viewer-annotate-note");
    // Enter adds (Shift+Enter is the field's new line), taken before the field sees it.
    if ctx.memory(|m| m.has_focus(text_id)) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
        result = Some(true);
    }
    let card_bg = Color32::from_rgb(0x1f, 0x20, 0x23);
    let frame = egui::Frame::NONE
        .fill(card_bg)
        .stroke(Stroke::new(1.0, Color32::from_white_alpha(18)))
        .corner_radius(CornerRadius::same(if d.tool == Tool::Pen { 14 } else { 12 }))
        .inner_margin(if d.tool == Tool::Pen { egui::Margin::symmetric(8, 6) } else { egui::Margin::same(10) })
        .shadow(egui::Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(110) });
    let shown = egui::Area::new(area_id).order(egui::Order::Foreground).fixed_pos(origin).fade_in(false).constrain(false).show(ctx, |ui| {
        ui.set_opacity(alpha);
        frame.show(ui, |ui| {
            ui.set_width(width - 20.0);
            match d.tool {
                Tool::Pen => pen_bar(ui, &t, d, text_id, &mut result, &mut app.auto),
                Tool::Comment => comment_card(ui, &t, d, text_id, &mut result, &mut app.auto),
            }
        });
    });
    ctx.data_mut(|m| m.insert_temp(size_id, shown.response.rect.size()));
    app.auto.add("viewer.annotate.box", shown.response.rect, if d.tool == Tool::Pen { "Drawing" } else { "Comment" });
    result
}

fn note_field(ui: &mut egui::Ui, d: &mut Draft, text_id: egui::Id, hint: &str, rows: usize, width: f32, auto: &mut crate::automation::Registry) {
    let focused = ui.memory(|m| m.has_focus(text_id));
    let edit = egui::TextEdit::multiline(&mut d.text)
        .id(text_id)
        .hint_text(hint)
        .desired_rows(rows)
        .desired_width(width)
        .frame(egui::Frame::NONE)
        .font(Tokens::ui(13.0))
        .text_color(Color32::from_gray(235));
    let resp = if rows > 1 {
        // The Comment's field: framed, with a focus ring.
        let ring = if focused { Stroke::new(1.5, INK) } else { Stroke::new(1.0, Color32::from_white_alpha(30)) };
        egui::Frame::NONE
            .fill(Color32::from_rgb(0x16, 0x17, 0x19))
            .stroke(ring)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| ui.add(edit))
            .inner
    } else {
        ui.add(edit)
    };
    if d.focus {
        resp.request_focus();
        d.focus = false;
    }
    auto.add_response("viewer.annotate.note", &resp, hint);
}

fn chip(ui: &mut egui::Ui, text: &str, fill: Color32, color: Color32) -> egui::Response {
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| ui.label(egui::RichText::new(text).font(Tokens::medium(11.5)).color(color)))
        .response
}

/// HyperFrames' draw bar: [▢ Drawing] Say what should change (optional)   esc (↑)
fn pen_bar(ui: &mut egui::Ui, _t: &Tokens, d: &mut Draft, text_id: egui::Id, result: &mut Option<bool>, auto: &mut crate::automation::Registry) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        chip(ui, "▢ Drawing", INK.gamma_multiply(0.18), INK);
        let field = (ui.available_width() - 64.0).max(80.0);
        note_field(ui, d, text_id, "Say what should change (optional)", 1, field, auto);
        ui.label(egui::RichText::new("esc").font(Tokens::ui(10.5)).color(Color32::from_gray(130)));
        let (r, send) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
        let fill = if send.hovered() { INK } else { INK.gamma_multiply(0.9) };
        ui.painter().circle_filled(r.center(), 13.0, fill);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, "↑", Tokens::medium(14.0), Color32::from_rgb(0x0b, 0x1f, 0x16));
        auto.add_response("viewer.annotate.add", &send, "Add");
        if send.on_hover_text("Add (Enter)").clicked() {
            *result = Some(true);
        }
    });
}

/// HyperFrames' comment card: [00:04] / field / Enter to add ……… Cancel [Comment]
fn comment_card(ui: &mut egui::Ui, _t: &Tokens, d: &mut Draft, text_id: egui::Id, result: &mut Option<bool>, auto: &mut crate::automation::Registry) {
    ui.spacing_mut().item_spacing.y = 8.0;
    let tc = d.timecode.clone();
    chip(ui, &tc, INK.gamma_multiply(0.18), INK);
    let w = ui.available_width() - 18.0;
    note_field(ui, d, text_id, "What should change here?", 2, w, auto);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Enter to add").font(Tokens::ui(11.0)).color(Color32::from_gray(130)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let comment = egui::Button::new(egui::RichText::new("Comment").font(Tokens::medium(12.0)).color(Color32::from_rgb(0x14, 0x15, 0x17)))
                .fill(Color32::from_gray(240))
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(8));
            let add = ui.add(comment);
            auto.add_response("viewer.annotate.add", &add, "Comment");
            if add.clicked() {
                *result = Some(true);
            }
            let cancel = ui.add(egui::Button::new(egui::RichText::new("Cancel").font(Tokens::ui(12.0)).color(Color32::from_gray(200))).frame(false));
            auto.add_response("viewer.annotate.cancel", &cancel, "Cancel");
            if cancel.clicked() {
                *result = Some(false);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_pop_in_and_out() {
        let (a0, s0, moving) = pop(0.0, 0.0, None, true);
        assert!(a0 < 0.01 && (s0 - 0.96).abs() < 1e-4 && moving);
        let (a1, s1, moving) = pop(1.0, 0.0, None, true);
        assert!((a1 - 1.0).abs() < 1e-6 && (s1 - 1.0).abs() < 1e-6 && !moving);
        let (a2, _, moving) = pop(5.0 + POP_OUT, 0.0, Some(5.0), true);
        assert!(a2 < 1e-6 && !moving);
        assert_eq!(pop(0.0, 0.0, None, false), (1.0, 1.0, false), "motion off: no pop");
    }

    #[test]
    fn boxes_stay_inside_the_viewer_next_to_their_mark() {
        let area = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 450.0));
        let mark = Rect::from_min_size(pos2(100.0, 100.0), vec2(50.0, 40.0));
        let at = box_origin(Tool::Pen, mark, vec2(360.0, 44.0), area);
        assert_eq!(at, pos2(100.0, 150.0), "the Pen's bar sits under the stroke");
        let low = Rect::from_min_size(pos2(700.0, 420.0), vec2(20.0, 20.0));
        let at = box_origin(Tool::Pen, low, vec2(360.0, 44.0), area);
        assert!(at.x + 360.0 <= 800.0 && at.y + 44.0 <= 450.0 && at.y < 420.0, "{at:?}");
        let at = box_origin(Tool::Comment, Rect::from_center_size(pos2(10.0, 10.0), vec2(8.0, 8.0)), vec2(300.0, 130.0), area);
        assert!(area.contains(at) && area.contains(at + vec2(300.0, 130.0)), "{at:?}");
    }
}
