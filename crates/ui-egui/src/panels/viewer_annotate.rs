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

use effectcraft_engine::commands::annotate::ink::{self, InkFilter};
use effectcraft_engine::commands::annotate::{Mark, Tool, bbox};
use effectcraft_engine::project::Comp;
use effectcraft_engine::time::Tick;
use egui::emath::TSTransform;
use egui::{Color32, CornerRadius, Key, Modifiers, Pos2, Rect, Sense, Stroke, pos2, vec2};
use serde_json::json;

use super::viewer::ViewerMap;
use crate::EffectcraftApp;
use crate::theme::Tokens;

/// HyperFrames' mark colour (its teal-green pen).
pub const INK: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x9b);
/// PN2: the drawn curve is cut into pieces at most this long (screen points), so it is round at
/// any zoom.
pub const CURVE_STEP: f64 = 1.5;
/// A finished stroke keeps its points within this of the drawn line (screen points): the curve
/// stays the same, the stored data small.
pub const KEEP_TOLERANCE: f64 = 0.25;
/// The pen's width (screen points).
pub const PEN_WIDTH: f32 = 3.0;
/// The most samples one live stroke keeps (a very long scribble stops growing).
const MAX_LIVE: usize = 20_000;
/// Pop in / out (seconds), ease-out.
const POP_IN: f64 = 0.15;
pub const POP_OUT: f64 = 0.10;
const PEN_BAR_WIDTH: f32 = 360.0;
const COMMENT_WIDTH: f32 = 300.0;

/// The box waiting for its note (EMBED15: public, so a host drawing marks itself, FigCraft's
/// MAIN board, shows the same box with [`mark_box`]).
#[derive(Clone, Debug)]
pub struct Draft {
    pub tool: Tool,
    pub comp: u64,
    pub time: f64,
    pub timecode: String,
    pub points: Vec<[f64; 2]>,
    pub text: String,
    pub opened: f64,
    /// Closing since (pop out), then dropped.
    pub closing: Option<f64>,
    pub focus: bool,
}

impl Draft {
    /// A new box for a mark of `tool` on comp `comp` at comp second `time`, opened at `now`.
    pub fn new(tool: Tool, comp: u64, time: f64, timecode: String, points: Vec<[f64; 2]>, now: f64) -> Draft {
        Draft { tool, comp, time, timecode, points, text: String::new(), opened: now, closing: None, focus: true }
    }
}

#[derive(Clone, Debug, Default)]
struct Live {
    /// The Pen's stroke while the button is down: filtered samples (comp pixels), the filter, and
    /// the newest raw sample, drawn as the stroke's tip so the line never lags the pointer.
    stroke: Vec<[f64; 2]>,
    filter: InkFilter,
    tip: Option<[f64; 2]>,
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
pub fn pop(now: f64, opened: f64, closing: Option<f64>, motion: bool) -> (f32, f32, bool) {
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
pub fn box_origin(tool: Tool, mark: Rect, size: egui::Vec2, area: Rect) -> Pos2 {
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
    let screen: Vec<Pos2> = points.iter().map(|p| map.to_screen(*p)).collect();
    draw_screen_mark(painter, tool, &screen, alpha);
}

/// A mark drawn from its screen points (EMBED15: hosts drawing marks on their own pictures use it):
/// Pen a smooth round-ended curve through them, Comment a pin at the first.
pub fn draw_screen_mark(painter: &egui::Painter, tool: Tool, points: &[Pos2], alpha: f32) {
    let ink = INK.gamma_multiply(alpha);
    match tool {
        Tool::Pen => {
            // PN2: one smooth curve through the samples (the same `ink::smooth` Ghostex burns into
            // its card), round ends, anti-aliased by egui's feathering.
            let screen: Vec<[f64; 2]> = points.iter().map(|s| [f64::from(s.x), f64::from(s.y)]).collect();
            let curve: Vec<Pos2> = ink::smooth(&screen, CURVE_STEP).into_iter().map(|p| pos2(p[0] as f32, p[1] as f32)).collect();
            let r = PEN_WIDTH / 2.0;
            match curve.as_slice() {
                [] => {}
                [only] => {
                    painter.circle_filled(*only, r, ink);
                }
                [first, .., last] => {
                    let (first, last) = (*first, *last);
                    painter.add(egui::Shape::line(curve, Stroke::new(PEN_WIDTH, ink)));
                    painter.circle_filled(first, r, ink);
                    painter.circle_filled(last, r, ink);
                }
            }
        }
        Tool::Comment => {
            if let Some(at) = points.first().copied() {
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
        live.tip = None;
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
                // PN2: every pointer sample of the frame, not one position per frame: from the
                // press (not from where egui decides a drag started) to the release.
                let px = 1.0 / f64::from(map.zoom.max(1e-6));
                let down = resp.is_pointer_button_down_on();
                if down && live.stroke.is_empty() && live.tip.is_none() {
                    live.filter = InkFilter::new(px);
                    if let Some(origin) = ctx.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                        let c = map.to_comp(origin);
                        live.tip = Some(c);
                        live.stroke.extend(live.filter.push(c));
                    }
                }
                if down || live.tip.is_some() {
                    let mut samples = ctx.input(|i| drag_samples(&i.events));
                    if samples.is_empty() && down {
                        samples.extend(resp.interact_pointer_pos());
                    }
                    for s in samples {
                        let c = map.to_comp(s);
                        live.tip = Some(c);
                        if live.stroke.len() < MAX_LIVE {
                            live.stroke.extend(live.filter.push(c));
                        }
                    }
                }
                if !down && live.tip.is_some() {
                    let mut points = std::mem::take(&mut live.stroke);
                    if let Some(tip) = live.tip.take()
                        && points.last().is_some_and(|l| *l != tip)
                    {
                        points.push(tip);
                    }
                    live.filter.reset();
                    let points = ink::finish(&points, KEEP_TOLERANCE * px);
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
    if let Some(tip) = live.tip {
        let mut shown = live.stroke.clone();
        if shown.last().is_none_or(|l| *l != tip) {
            shown.push(tip);
        }
        if shown.len() > 1 {
            draw_mark(&painter, map, Tool::Pen, &shown, 1.0);
        }
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

/// The pointer positions of this frame's events while the button draws, in order: every
/// `PointerMoved` (egui keeps each one the platform sent; `hover_pos` is only the last), from the
/// primary press when it is in this frame, else from the start, up to and including the release.
pub fn drag_samples(events: &[egui::Event]) -> Vec<Pos2> {
    let primary_press = |e: &egui::Event| matches!(e, egui::Event::PointerButton { button: egui::PointerButton::Primary, pressed: true, .. });
    let mut down = !events.iter().any(primary_press);
    let mut out = Vec::new();
    for e in events {
        match e {
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. } => {
                if *pressed {
                    down = true;
                    out.push(*pos);
                } else if down {
                    out.push(*pos);
                    break;
                }
            }
            egui::Event::PointerMoved(p) if down => out.push(*p),
            _ => {}
        }
    }
    out
}

fn new_draft(app: &EffectcraftApp, comp: &Comp, cid: u64, time: Tick, tool: Tool, points: Vec<[f64; 2]>, now: f64) -> Draft {
    let t = comp.frame_rate.snap(time);
    Draft::new(tool, cid, t.seconds(), effectcraft_engine::commands::time::display_time(&app.session, comp, t), points, now)
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
    mark_box(ctx, &t, &mut app.auto, d, mark, map.area, alpha, scale)
}

/// The mark's box (the Pen's bar or the Comment's card) next to `mark` (its screen box), kept
/// inside `area`, popping with `alpha` / `scale` ([`pop`]). `Some(true)`: add, `Some(false)`:
/// cancel. EMBED15: public for hosts that draw marks on their own pictures.
#[allow(clippy::too_many_arguments)]
pub fn mark_box(
    ctx: &egui::Context,
    t: &Tokens,
    auto: &mut crate::automation::Registry,
    d: &mut Draft,
    mark: Rect,
    area: Rect,
    alpha: f32,
    scale: f32,
) -> Option<bool> {
    let width = match d.tool {
        Tool::Pen => PEN_BAR_WIDTH,
        Tool::Comment => COMMENT_WIDTH,
    };
    // The box's size from last frame (its height grows with the note).
    let size_id = egui::Id::new("viewer-annotate-box-size");
    let size = ctx.data(|m| m.get_temp::<egui::Vec2>(size_id)).unwrap_or(vec2(width, if d.tool == Tool::Pen { 44.0 } else { 132.0 }));
    let origin = box_origin(d.tool, mark, size, area);
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
                Tool::Pen => pen_bar(ui, t, d, text_id, &mut result, auto),
                Tool::Comment => comment_card(ui, t, d, text_id, &mut result, auto),
            }
        });
    });
    ctx.data_mut(|m| m.insert_temp(size_id, shown.response.rect.size()));
    auto.add("viewer.annotate.box", shown.response.rect, if d.tool == Tool::Pen { "Drawing" } else { "Comment" });
    result
}

/// The Pen bar's row height (its send button's size): every item is centred on it.
const ROW: f32 = 26.0;
/// A chip's / small button's height.
const CHIP_H: f32 = 20.0;

/// PN2: how far below a galley's top the middle of its ink sits, measured from the glyphs
/// themselves (top of the tallest glyph to the baseline: descenders left out, so "Drawing" centres
/// on its cap height and "esc" on its x height). The galley's half height when it has no ink.
pub(crate) fn ink_middle(galley: &egui::Galley) -> f32 {
    let mut top = f32::INFINITY;
    let mut base = f32::NEG_INFINITY;
    if let Some(row) = galley.rows.first() {
        for g in &row.row.glyphs {
            if g.uv_rect.size.y <= 0.0 {
                continue;
            }
            let baseline = row.pos.y + g.pos.y;
            top = top.min(baseline + g.uv_rect.offset.y);
            base = base.max(baseline);
        }
    }
    if top.is_finite() && base.is_finite() && base > top { (top + base) / 2.0 } else { galley.size().y / 2.0 }
}

/// Paints `galley` with its ink centred on `center` (horizontally by its width).
fn paint_centered(painter: &egui::Painter, center: Pos2, galley: std::sync::Arc<egui::Galley>, color: Color32) {
    let at = pos2(center.x - galley.size().x / 2.0, center.y - ink_middle(&galley));
    painter.galley(at, galley, color);
}

/// A label in a box `height` tall (padded `pad` left and right), optically centred; filled when
/// `fill` isn't transparent. Clickable.
fn text_box(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32, fill: Color32, height: f32, pad: f32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 2.0 * pad, height), Sense::click());
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, CornerRadius::same((height / 2.0).min(8.0) as u8), fill);
    }
    paint_centered(ui.painter(), rect.center(), galley, color);
    resp
}

fn note_field(ui: &mut egui::Ui, d: &mut Draft, text_id: egui::Id, hint: &str, rows: usize, width: f32, auto: &mut crate::automation::Registry) {
    let focused = ui.memory(|m| m.has_focus(text_id));
    let font = Tokens::ui(13.0);
    let sample = if d.text.is_empty() { hint.to_string() } else { d.text.lines().next().unwrap_or_default().to_string() };
    let edit = egui::TextEdit::multiline(&mut d.text)
        .id(text_id)
        .hint_text(hint)
        .desired_rows(rows)
        .desired_width(width)
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::ZERO)
        .font(font.clone())
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
        // The Pen bar's field: its first line's ink centred on the row, measured on the hint (or
        // the typed text) in the field's font.
        let galley = ui.painter().layout_no_wrap(sample, font, Color32::WHITE);
        let top = (ROW / 2.0 - ink_middle(&galley)).max(0.0);
        ui.allocate_ui_with_layout(vec2(width, ROW), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_height(ROW);
            ui.add_space(top);
            ui.add(edit)
        })
        .inner
    };
    if d.focus {
        resp.request_focus();
        d.focus = false;
    }
    auto.add_response("viewer.annotate.note", &resp, hint);
}

fn chip(ui: &mut egui::Ui, text: &str, fill: Color32, color: Color32) -> egui::Response {
    text_box(ui, text, Tokens::medium(11.5), color, fill, CHIP_H, 7.0)
}

/// The round send button: an up arrow drawn as strokes, exactly centred in its circle.
fn send_button(ui: &mut egui::Ui) -> egui::Response {
    let (r, send) = ui.allocate_exact_size(vec2(ROW, ROW), Sense::click());
    let fill = if send.hovered() { INK } else { INK.gamma_multiply(0.9) };
    let c = r.center();
    let p = ui.painter();
    p.circle_filled(c, ROW / 2.0, fill);
    let dark = Stroke::new(1.8, Color32::from_rgb(0x0b, 0x1f, 0x16));
    // Shaft and head span ±5 pt around the centre.
    let (top, bottom, wing) = (c.y - 5.0, c.y + 5.0, 4.0);
    p.line_segment([pos2(c.x, bottom), pos2(c.x, top)], dark);
    p.line_segment([pos2(c.x - wing, top + wing), pos2(c.x, top)], dark);
    p.line_segment([pos2(c.x + wing, top + wing), pos2(c.x, top)], dark);
    send
}

/// HyperFrames' draw bar: [▢ Drawing] Say what should change (optional)   esc (↑), every item
/// centred on one row.
fn pen_bar(ui: &mut egui::Ui, _t: &Tokens, d: &mut Draft, text_id: egui::Id, result: &mut Option<bool>, auto: &mut crate::automation::Registry) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, ROW), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_height(ROW);
        ui.spacing_mut().item_spacing.x = 8.0;
        chip(ui, "▢ Drawing", INK.gamma_multiply(0.18), INK);
        let field = (ui.available_width() - 64.0).max(80.0);
        note_field(ui, d, text_id, "Say what should change (optional)", 1, field, auto);
        text_box(ui, "esc", Tokens::ui(10.5), Color32::from_gray(130), Color32::TRANSPARENT, ROW, 0.0);
        let send = send_button(ui);
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
    ui.horizontal(|ui| chip(ui, &tc, INK.gamma_multiply(0.18), INK));
    let w = ui.available_width() - 18.0;
    note_field(ui, d, text_id, "What should change here?", 2, w, auto);
    let button_h = 26.0;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), button_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_height(button_h);
        text_box(ui, "Enter to add", Tokens::ui(11.0), Color32::from_gray(130), Color32::TRANSPARENT, button_h, 0.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let add = text_box(ui, "Comment", Tokens::medium(12.0), Color32::from_rgb(0x14, 0x15, 0x17), Color32::from_gray(240), button_h, 12.0)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            auto.add_response("viewer.annotate.add", &add, "Comment");
            if add.clicked() {
                *result = Some(true);
            }
            let cancel = text_box(ui, "Cancel", Tokens::ui(12.0), Color32::from_gray(200), Color32::TRANSPARENT, button_h, 8.0)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
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

    fn press(x: f32, y: f32, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos: pos2(x, y), button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    #[test]
    fn a_frame_gives_every_pointer_sample_of_the_drag() {
        // Mid-drag: every move of the frame, in order (not only the last one).
        let mid: Vec<egui::Event> = (0..12).map(|i| egui::Event::PointerMoved(pos2(i as f32 * 9.0, 0.0))).collect();
        assert_eq!(drag_samples(&mid).len(), 12);
        // Press in this frame: moves before it are hover, not ink.
        let ev = vec![egui::Event::PointerMoved(pos2(-5.0, 0.0)), press(0.0, 0.0, true), egui::Event::PointerMoved(pos2(4.0, 0.0))];
        assert_eq!(drag_samples(&ev), vec![pos2(0.0, 0.0), pos2(4.0, 0.0)]);
        // Release: its position is the last sample, later moves are hover.
        let ev = vec![egui::Event::PointerMoved(pos2(4.0, 0.0)), press(6.0, 0.0, false), egui::Event::PointerMoved(pos2(50.0, 0.0))];
        assert_eq!(drag_samples(&ev), vec![pos2(4.0, 0.0), pos2(6.0, 0.0)]);
    }

    #[test]
    fn text_is_centred_on_its_ink_not_its_line_box() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let galley = ctx.fonts_mut(|f| f.layout_no_wrap("Drawing".into(), Tokens::ui(12.0), Color32::WHITE));
        let mid = ink_middle(&galley);
        assert!(mid > 0.0 && mid < galley.size().y, "{mid} in {}", galley.size().y);
        let empty = ctx.fonts_mut(|f| f.layout_no_wrap(String::new(), Tokens::ui(12.0), Color32::WHITE));
        assert!((ink_middle(&empty) - empty.size().y / 2.0).abs() < 1e-4);
    }
}
