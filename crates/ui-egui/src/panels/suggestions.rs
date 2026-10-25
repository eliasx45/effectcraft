//! Claude's suggestions in the Timeline (batch 1, CX4): a "N suggestions" badge in the header, a
//! row of pills (one per suggestion, with ✓ keep and ✕ drop), and the layers each one touches
//! drawn dashed over their bars for its time range. Hovering a pill previews it in the viewer
//! (applied to a scratch copy, its range looping: `viewer_overlays::agent_compare`); leaving it
//! goes back to yours. Y keeps all and N drops all, only while the pointer is over the row (Y and
//! N keep their usual shortcuts everywhere else).
//!
//! Every button runs `suggestion.accept` / `suggestion.reject`, so agents and History see them.

use effectcraft_engine::commands::suggestions::{self, Suggestion};
use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use serde_json::json;

use crate::EffectcraftApp;
use crate::theme::Tokens;

/// Claude's blue (the mock's #8fc1ff).
pub const CLAUDE_BLUE: Color32 = Color32::from_rgb(0x8f, 0xc1, 0xff);

fn hover_id() -> egui::Id {
    egui::Id::new("suggestion-hover")
}
fn row_hot_id() -> egui::Id {
    egui::Id::new("suggestion-row-hot")
}
fn row_open_id() -> egui::Id {
    egui::Id::new("suggestion-row-open")
}

/// The suggestion the pointer is over and when it got there (input time).
pub fn hovered(ctx: &egui::Context) -> Option<(String, f64)> {
    ctx.data(|d| d.get_temp::<(String, f64)>(hover_id()))
}

/// Keep / drop one suggestion (or all), reporting in a toast.
fn decide(app: &mut EffectcraftApp, ctx: &egui::Context, id: Option<&str>, keep: bool) {
    let cmd = if keep { "suggestion.accept" } else { "suggestion.reject" };
    let params = match id {
        Some(id) => json!({"id": id}),
        None => json!({"all": true}),
    };
    ctx.data_mut(|d| d.remove::<(String, f64)>(hover_id()));
    match crate::menus::invoke(app, ctx, cmd, params) {
        Ok(r) => {
            let msg = if keep {
                let n = r["kept"].as_array().map(Vec::len).unwrap_or(0);
                format!("Kept {n}: applied, one undo step each")
            } else {
                let n = r["dropped"].as_array().map(Vec::len).unwrap_or(0);
                format!("Dropped {n}: nothing changed")
            };
            app.toast = Some((msg, ctx.input(|i| i.time)));
        }
        Err(e) => app.ui.status = e,
    }
}

// ── Claude's version vs yours (CX5) ─────────────────────────────────────────────────────────

/// How long `\` is held before Claude's version shows (a tap keeps Switch Between Composition and
/// Timeline Panel).
const FLIP_HOLD: f64 = 0.2;

fn flip_id() -> egui::Id {
    egui::Id::new("agent-flip")
}
fn flip_press_id() -> egui::Id {
    egui::Id::new("agent-flip-press")
}

/// Whether Claude's version is on screen (`\` or the viewer button held).
pub fn flipped(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(flip_id())).unwrap_or(false)
}

/// Show (or stop showing) Claude's version: the viewer button.
pub fn set_flipped(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(flip_id(), on));
    ctx.request_repaint();
}

/// A held `\`: Claude's version after [`FLIP_HOLD`], yours again on release; a tap is replayed to
/// the shortcuts (as lane A's G / A wheels do).
fn hold_backslash(app: &EffectcraftApp, ctx: &egui::Context) {
    let key = egui::Key::Backslash;
    let now = ctx.input(|i| i.time);
    if let Some(since) = ctx.data(|d| d.get_temp::<f64>(flip_press_id())) {
        let down = ctx.input(|i| i.key_down(key));
        if !down {
            ctx.data_mut(|d| d.remove::<f64>(flip_press_id()));
            if flipped(ctx) {
                set_flipped(ctx, false);
            } else {
                // A tap: `\`'s own meaning through the normal shortcut path.
                // The same replay as timeline/wheels.rs hold_keys (egui 0.36 `Event::Key` fields checked).
                ctx.input_mut(|i| i.events.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }));
            }
            return;
        }
        if now - since >= FLIP_HOLD && !flipped(ctx) {
            set_flipped(ctx, true);
        }
        ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
        ctx.request_repaint_after(std::time::Duration::from_millis(30));
        return;
    }
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() || crate::widgets::any_menu_open(ctx) {
        return;
    }
    // Only with a Claude's version to show: otherwise `\` is untouched.
    if effectcraft_engine::commands::suggestions::claude_version(&app.session).is_none() {
        return;
    }
    let pressed = ctx.input(|i| {
        i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, modifiers, .. } if *k == key && !modifiers.any()))
    });
    if pressed {
        ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
        ctx.data_mut(|d| d.insert_temp(flip_press_id(), now));
        ctx.request_repaint_after(std::time::Duration::from_millis(30));
    }
}

/// Before the shortcuts (from `cards::before_shortcuts`): the held `\` (CX5), and Y / N while
/// the pointer is over the suggestion row (CX4).
pub fn before_shortcuts(app: &mut EffectcraftApp, ctx: &egui::Context) {
    hold_backslash(app, ctx);
    if !ctx.data(|d| d.get_temp::<bool>(row_hot_id())).unwrap_or(false) || ctx.egui_wants_keyboard_input() {
        return;
    }
    if suggestions::list(&app.session).is_empty() {
        return;
    }
    for (key, keep) in [(egui::Key::Y, true), (egui::Key::N, false)] {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key)) {
            decide(app, ctx, None, keep);
            return;
        }
    }
}

/// Draw the badge, the pill row and the dashed ghosts over the Timeline `rect` (called at the end
/// of `timeline::show`). The Why? strip ([`super::why_chip`]) goes under the row.
pub fn timeline_overlay(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let ctx = ui.ctx().clone();
    let why_h = super::why_chip::strip(app, ui, rect);
    let list: Vec<Suggestion> = suggestions::list(&app.session);
    if list.is_empty() {
        ctx.data_mut(|d| {
            d.remove::<(String, f64)>(hover_id());
            d.insert_temp(row_hot_id(), false);
        });
        return;
    }
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(rect);
    // The badge, top right of the header.
    let text = format!("{} suggestion{}", list.len(), if list.len() == 1 { "" } else { "s" });
    let g = p.layout_no_wrap(text.clone(), Tokens::medium(11.0), CLAUDE_BLUE);
    let badge = Rect::from_min_size(pos2(rect.max.x - g.size().x - 34.0, rect.min.y + 6.0), g.size() + vec2(16.0, 6.0));
    let bresp = ui.interact(badge, egui::Id::new("suggestion-badge"), Sense::click());
    p.rect_filled(badge, 9.0, Color32::from_rgba_unmultiplied(0x8f, 0xc1, 0xff, if bresp.hovered() { 60 } else { 34 }));
    p.galley(badge.min + vec2(8.0, 3.0), g, CLAUDE_BLUE);
    app.auto.add("timeline.suggestions.badge", badge, &text);
    let open = ctx.data(|d| d.get_temp::<bool>(row_open_id())).unwrap_or(true);
    if bresp.on_hover_text("Claude's suggestions: show or hide them").clicked() {
        ctx.data_mut(|d| d.insert_temp(row_open_id(), !open));
    }
    if !open {
        ctx.data_mut(|d| d.insert_temp(row_hot_id(), false));
        return;
    }
    // The row of pills: just above the Why? strip, at the bottom of the layers.
    // TODO(batch1-verify): the mock puts it right under the header; at the bottom it never covers the first layers.
    let row_h = 28.0;
    let footer_h = 24.0;
    let row = Rect::from_min_max(pos2(rect.min.x, rect.max.y - footer_h - why_h - row_h), pos2(rect.max.x, rect.max.y - footer_h - why_h));
    p.rect_filled(row, 0.0, Color32::from_rgba_unmultiplied(0x16, 0x22, 0x33, 235));
    p.line_segment([row.left_top(), row.right_top()], Stroke::new(1.0, CLAUDE_BLUE.gamma_multiply(0.5)));
    let row_hot = ui.rect_contains_pointer(row);
    ctx.data_mut(|d| d.insert_temp(row_hot_id(), row_hot));
    app.auto.add("timeline.suggestions.row", row, "Claude's suggestions");
    let mut x = row.min.x + 10.0;
    let cy = row.center().y;
    p.text(pos2(x, cy), Align2::LEFT_CENTER, "Claude:", Tokens::semibold(11.5), CLAUDE_BLUE);
    x += 52.0;
    let now = ctx.input(|i| i.time);
    let mut hover_now: Option<String> = None;
    let mut action: Option<(String, bool)> = None;
    for sg in &list {
        let label = format!("{} · {}", sg.n, sg.label);
        let g = p.layout_no_wrap(label.clone(), Tokens::ui(11.0), t.text);
        let w = g.size().x + 16.0 + 44.0;
        if x + w > row.max.x - 150.0 {
            p.text(pos2(x, cy), Align2::LEFT_CENTER, "…", Tokens::ui(11.0), t.text_dim);
            break;
        }
        let pill = Rect::from_min_size(pos2(x, cy - 10.0), vec2(w, 20.0));
        let presp = ui.interact(pill, egui::Id::new(("suggestion-pill", sg.id.as_str())), Sense::hover());
        let hot = presp.hovered();
        p.rect_filled(
            pill,
            10.0,
            if hot { Color32::from_rgba_unmultiplied(0x8f, 0xc1, 0xff, 70) } else { Color32::from_rgba_unmultiplied(0x8f, 0xc1, 0xff, 28) },
        );
        p.galley(pos2(pill.min.x + 8.0, cy - g.size().y / 2.0), g, t.text);
        app.auto.add(&format!("timeline.suggestions.{}", sg.id), pill, &label);
        if hot {
            hover_now = Some(sg.id.clone());
        }
        // ✓ and ✕.
        for (k, (glyph, keep)) in [("✓", true), ("✕", false)].into_iter().enumerate() {
            let b = Rect::from_center_size(pos2(pill.max.x - 34.0 + k as f32 * 20.0, cy), vec2(18.0, 18.0));
            let r = ui.interact(b, egui::Id::new(("suggestion-btn", sg.id.as_str(), keep)), Sense::click());
            let c = if keep { t.cache_green } else { t.danger };
            if r.hovered() {
                p.circle_filled(b.center(), 9.0, c.gamma_multiply(0.3));
            }
            p.text(b.center(), Align2::CENTER_CENTER, glyph, Tokens::semibold(12.0), c);
            app.auto.add(&format!("timeline.suggestions.{}.{}", sg.id, if keep { "keep" } else { "drop" }), b, if keep { "Keep" } else { "Drop" });
            if r.on_hover_text(if keep { "Keep: apply it (one undo step)" } else { "Drop: nothing changes" }).clicked() {
                action = Some((sg.id.clone(), keep));
            }
        }
        x = pill.max.x + 8.0;
    }
    p.text(pos2(row.max.x - 10.0, cy), Align2::RIGHT_CENTER, "hover = preview · Y keep all · N drop all", Tokens::ui(10.5), t.text_dim);
    // The hovered pill previews; the time it started is kept while it stays.
    match (&hover_now, hovered(&ctx)) {
        (Some(id), Some((was, _))) if *id == was => {}
        (Some(id), _) => ctx.data_mut(|d| {
            d.insert_temp(hover_id(), (id.clone(), now));
        }),
        (None, Some(_)) => ctx.data_mut(|d| {
            d.remove::<(String, f64)>(hover_id());
        }),
        (None, None) => {}
    }
    if hover_now.is_some() {
        ctx.request_repaint();
    }
    // The touched layers, dashed over their bars for the suggestion's range (the hovered one
    // brighter).
    let hov = hover_now.as_deref();
    for sg in &list {
        if sg.comp.is_some() && sg.comp != app.session.active_comp_id().map(|c| c.0) {
            continue;
        }
        let a = if hov == Some(sg.id.as_str()) { 1.0 } else { 0.55 };
        for layer in &sg.layers {
            let Some(bar) = app
                .auto
                .find_current(&format!("timeline.layer.{layer}.bar"))
                .map(|e| Rect::from_min_size(pos2(e.rect[0], e.rect[1]), vec2(e.rect[2], e.rect[3])))
            else {
                continue;
            };
            let (x0, x1) = match sg.range {
                Some([s0, s1]) => {
                    let a0 = super::timeline::locate(app, *layer, Some(s0)).map(|(x, _)| x).unwrap_or(bar.min.x);
                    let a1 = super::timeline::locate(app, *layer, Some(s1)).map(|(x, _)| x).unwrap_or(bar.max.x);
                    (a0.max(bar.min.x - 200.0), a1.max(a0 + 4.0))
                }
                None => (bar.min.x, bar.max.x),
            };
            let r = Rect::from_min_max(pos2(x0, bar.min.y + 1.0), pos2(x1, bar.max.y - 1.0));
            dashed_rect(&p, r, Stroke::new(1.2, CLAUDE_BLUE.gamma_multiply(a)));
            if hov == Some(sg.id.as_str()) {
                p.text(pos2(r.min.x + 3.0, r.min.y - 1.0), Align2::LEFT_BOTTOM, format!("{} · {}", sg.n, sg.label), Tokens::mono(9.5), CLAUDE_BLUE);
            }
        }
    }
    if let Some((id, keep)) = action {
        decide(app, &ctx, Some(&id), keep);
    }
}

/// A dashed rectangle outline.
pub(crate) fn dashed_rect(p: &egui::Painter, r: Rect, stroke: Stroke) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    for w in pts.windows(2) {
        if let [a, b] = w {
            p.extend(egui::Shape::dashed_line(&[*a, *b], stroke, 5.0, 3.0));
        }
    }
}

/// The strips at the bottom of the Timeline (Claude's suggestions, Why?) that a toast must not
/// cover: the toast took the clicks meant for a pill's ✓ / ✕ (BUILD2 CX4).
pub(crate) const TOAST_CLEAR_OF: [&str; 2] = ["timeline.suggestions.row", "timeline.why"];

/// `toast` moved up until it is clear of every rect in `rows` it overlaps.
pub(crate) fn toast_clear_of(mut toast: Rect, rows: &[Rect]) -> Rect {
    for row in rows {
        if row.intersects(toast) {
            toast = toast.translate(vec2(0.0, row.min.y - 6.0 - toast.max.y));
        }
    }
    toast
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_moves_above_the_suggestion_row() {
        let row = Rect::from_min_size(pos2(5.0, 963.0), vec2(1366.0, 28.0));
        let toast = Rect::from_min_size(pos2(16.0, 976.0), vec2(600.0, 28.0));
        let moved = toast_clear_of(toast, &[row]);
        assert!(!moved.intersects(row));
        assert_eq!(moved.max.y, 957.0);
        assert_eq!(moved.width(), toast.width());
        let far = Rect::from_min_size(pos2(16.0, 500.0), vec2(600.0, 28.0));
        assert_eq!(toast_clear_of(far, &[row]), far, "nothing to avoid");
    }
}
