//! The Timeline's layer outline columns, as in After Effects: which are shown (column header
//! right-click ▸ Columns, Hide This), their left-to-right order (drag a header), their widths
//! (drag the divider right of a header; the icon columns keep a fixed width), the splitter
//! between the outline and the time graph, the footer's pane buttons (Layer Switches, Transfer
//! Controls, In/Out/Duration/Stretch, Render Time) and Toggle Switches / Modes. The layout is kept in the
//! settings store ([`LAYOUT_FILE`]) and comes back on the next launch.

use std::collections::{BTreeMap, BTreeSet};

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use serde_json::{Value, json};

use crate::icons::{self, Icon};
use crate::panels::timeline::TimelineHost;
use crate::panels::timeline::ids::tl_id;
use crate::state::TimelineState;
use crate::theme::Tokens;
use crate::{EffectcraftApp, widgets};

pub(super) const AV_W: f32 = 76.0;
pub(super) const LABEL_W: f32 = 20.0;
pub(super) const NUM_W: f32 = 26.0;
pub(super) const SW: f32 = 19.0;
pub(super) const KEYS_W: f32 = 56.0;
pub(super) const SWITCHES_W: f32 = SW * 8.0 + 6.0;
pub(super) const MODE_W: f32 = 92.0;
pub(super) const TRKMAT_W: f32 = 112.0;
const MODES_W: f32 = MODE_W + TRKMAT_W;
/// The widest a resizable column gets.
pub const MAX_COLUMN_W: f32 = 1200.0;
/// The narrowest the layer outline and the time graph get when the splitter is dragged.
pub const MIN_OUTLINE_W: f32 = 240.0;
pub const MIN_GRAPH_W: f32 = 120.0;

/// The Timeline's columns in After Effects' default left-to-right order: (id, header label).
/// Right-click a column header to show or hide them; the name column is always shown.
pub const COLUMNS: [(&str, &str); 14] = [
    ("av", "A/V Features"),
    ("keys", "Keys"),
    ("label", "Label"),
    ("num", "#"),
    ("name", "Layer Name"),
    ("comment", "Comment"),
    ("switches", "Switches"),
    ("modes", "Modes"),
    ("parent", "Parent & Link"),
    ("in", "In"),
    ("out", "Out"),
    ("duration", "Duration"),
    ("stretch", "Stretch"),
    ("renderTime", "Render Time"),
];

/// The column header context menu's Columns submenu, in After Effects' order.
pub const COLUMNS_MENU: [&str; 14] =
    ["av", "label", "num", "name", "comment", "switches", "modes", "parent", "keys", "in", "out", "duration", "stretch", "renderTime"];

/// The columns the In/Out/Duration/Stretch pane button shows and hides together.
const IN_OUT_PANE: [&str; 4] = ["in", "out", "duration", "stretch"];

/// A column's header label (the name column follows Source Name / Layer Name).
pub fn column_label(tl: &TimelineState, id: &str) -> &'static str {
    match id {
        "name" if tl.source_name => "Source Name",
        _ => COLUMNS.iter().find(|c| c.0 == id).map_or("", |c| c.1),
    }
}

/// Is a column shown?
pub fn column_visible(tl: &TimelineState, id: &str) -> bool {
    match id {
        "name" => true,
        "modes" => tl.show_modes,
        _ => tl.columns.contains(id),
    }
}

/// Show or hide a column (`modes` is the Transfer Controls column, [`TimelineState::show_modes`]).
pub fn set_column(tl: &mut TimelineState, id: &str, on: bool) -> Result<(), String> {
    match id {
        "name" if on => Ok(()),
        "name" => Err("the name column can't be hidden".into()),
        "modes" => {
            tl.show_modes = on;
            Ok(())
        }
        _ if COLUMNS.iter().any(|c| c.0 == id) => {
            if on {
                tl.columns.insert(id.to_string());
            } else {
                tl.columns.remove(id);
            }
            Ok(())
        }
        _ => Err(unknown(id)),
    }
}

fn unknown(id: &str) -> String {
    format!("unknown column `{id}` (one of: {})", COLUMNS.map(|c| c.0).join(", "))
}

/// (default width, minimum width) of the columns that can be resized. The icon columns (A/V
/// Features, Label, #, Keys, Switches) and Modes have a fixed width, as in After Effects.
fn resizable(id: &str) -> Option<(f32, f32)> {
    Some(match id {
        "name" => (190.0, 80.0),
        "comment" => (120.0, 40.0),
        "parent" => (116.0, 70.0),
        "in" | "out" | "duration" => (72.0, 50.0),
        "stretch" => (60.0, 44.0),
        "renderTime" => (120.0, 60.0),
        _ => return None,
    })
}

/// Can the column be resized (by dragging the divider right of its header)?
pub fn column_resizable(id: &str) -> bool {
    resizable(id).is_some()
}

/// A column's width in points (also when it is hidden).
pub fn column_width(tl: &TimelineState, id: &str) -> f32 {
    match resizable(id) {
        Some((default, min)) => tl.column_widths.get(id).copied().filter(|w| w.is_finite()).unwrap_or(default).clamp(min, MAX_COLUMN_W),
        None => match id {
            "av" => AV_W,
            "keys" => KEYS_W,
            "label" => LABEL_W,
            "num" => NUM_W,
            "switches" => SWITCHES_W,
            "modes" => MODES_W,
            _ => 0.0,
        },
    }
}

/// Resize a column (kept within its minimum and [`MAX_COLUMN_W`]); returns the width it got.
pub fn set_column_width(tl: &mut TimelineState, id: &str, w: f32) -> Result<f32, String> {
    let Some((_, min)) = resizable(id) else {
        return Err(if COLUMNS.iter().any(|c| c.0 == id) { format!("the `{id}` column has a fixed width") } else { unknown(id) });
    };
    if !w.is_finite() {
        return Err("the width must be a number".into());
    }
    let w = w.clamp(min, MAX_COLUMN_W);
    tl.column_widths.insert(id.to_string(), w);
    Ok(w)
}

/// Every column id, left to right: the saved order, then any column it lacks in the default
/// order.
pub fn column_order(tl: &TimelineState) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::with_capacity(COLUMNS.len());
    for id in &tl.column_order {
        if let Some(c) = COLUMNS.iter().find(|c| c.0 == id.as_str())
            && !v.contains(&c.0)
        {
            v.push(c.0);
        }
    }
    for (id, _) in COLUMNS {
        if !v.contains(&id) {
            v.push(id);
        }
    }
    v
}

/// Move a column just before another one (`None`: to the right end).
pub fn move_column(tl: &mut TimelineState, id: &str, before: Option<&str>) -> Result<(), String> {
    if before == Some(id) {
        return Ok(());
    }
    let mut order = column_order(tl);
    let from = order.iter().position(|c| *c == id).ok_or_else(|| unknown(id))?;
    let moving = order.remove(from);
    let at = match before {
        Some(b) => order.iter().position(|c| *c == b).ok_or_else(|| unknown(b))?,
        None => order.len(),
    };
    order.insert(at, moving);
    tl.column_order = order.into_iter().map(String::from).collect();
    Ok(())
}

/// Toggle Switches / Modes (F4): the Switches and Modes columns trade places. With both (or
/// neither) shown, it leaves only the switches.
pub fn toggle_switches_modes(tl: &mut TimelineState) {
    let switches = column_visible(tl, "switches");
    let (sw, modes) = if switches != tl.show_modes { (!switches, switches) } else { (true, false) };
    let _ = set_column(tl, "switches", sw);
    tl.show_modes = modes;
}

/// The In/Out/Duration/Stretch pane button: hides those columns when any is shown, else shows
/// all four.
pub fn toggle_in_out_pane(tl: &mut TimelineState) {
    let on = !IN_OUT_PANE.iter().any(|c| column_visible(tl, c));
    for c in IN_OUT_PANE {
        let _ = set_column(tl, c, on);
    }
}

/// The splitter position kept between the outline's minimum and the graph's.
pub fn clamp_outline(w: f32, panel_w: f32) -> f32 {
    if !w.is_finite() {
        return MIN_OUTLINE_W;
    }
    w.min(panel_w - MIN_GRAPH_W).max(MIN_OUTLINE_W)
}

/// Which columns are shown.
#[derive(Clone, Copy, Default)]
pub(super) struct Vis {
    pub av: bool,
    pub keys: bool,
    pub label: bool,
    pub num: bool,
    pub comment: bool,
    pub switches: bool,
    pub modes: bool,
    pub parent: bool,
    pub in_: bool,
    pub out: bool,
    pub duration: bool,
    pub stretch: bool,
    pub render_time: bool,
}

impl Vis {
    pub fn of(tl: &TimelineState) -> Vis {
        let v = |id| column_visible(tl, id);
        Vis {
            av: v("av"),
            keys: v("keys"),
            label: v("label"),
            num: v("num"),
            comment: v("comment"),
            switches: v("switches"),
            modes: v("modes"),
            parent: v("parent"),
            in_: v("in"),
            out: v("out"),
            duration: v("duration"),
            stretch: v("stretch"),
            render_time: v("renderTime"),
        }
    }
}

/// Where each column starts (a hidden column: where it would start) and how wide the resizable
/// ones are.
pub(super) struct Cols {
    pub vis: Vis,
    pub av: f32,
    pub keys: f32,
    pub label: f32,
    pub num: f32,
    pub name: f32,
    /// End of the name column.
    pub name_end: f32,
    pub comment: f32,
    pub comment_w: f32,
    pub switches: f32,
    pub mode: f32,
    pub trkmat: f32,
    pub parent: f32,
    pub parent_w: f32,
    pub in_: f32,
    pub in_w: f32,
    pub out: f32,
    pub out_w: f32,
    pub duration: f32,
    pub duration_w: f32,
    pub stretch: f32,
    pub stretch_w: f32,
    pub render_time: f32,
    pub render_time_w: f32,
    /// End of the last visible column.
    pub end: f32,
    /// Where property rows put their values and controls: past the name column (and a Comment
    /// column right after it).
    pub vx: f32,
    /// The visible columns, left to right: (id, start, end).
    pub spans: Vec<(&'static str, f32, f32)>,
}

impl Cols {
    /// The visible column `id`'s (start, end).
    pub fn span(&self, id: &str) -> Option<(f32, f32)> {
        self.spans.iter().find(|s| s.0 == id).map(|s| (s.1, s.2))
    }
}

/// Lay the columns out from `x0`.
pub(super) fn cols(x0: f32, tl: &TimelineState) -> Cols {
    let vis = Vis::of(tl);
    let mut at = [x0; COLUMNS.len()];
    let mut spans = Vec::with_capacity(COLUMNS.len());
    let mut x = x0;
    for id in column_order(tl) {
        if let Some(i) = COLUMNS.iter().position(|c| c.0 == id)
            && let Some(slot) = at.get_mut(i)
        {
            *slot = x;
        }
        if column_visible(tl, id) {
            let w = column_width(tl, id);
            spans.push((id, x, x + w));
            x += w;
        }
    }
    let x_of = |id: &str| COLUMNS.iter().position(|c| c.0 == id).and_then(|i| at.get(i).copied()).unwrap_or(x0);
    let w = |id: &str| column_width(tl, id);
    let name = x_of("name");
    let name_end = name + w("name");
    let comment = x_of("comment");
    let vx = if vis.comment && (comment - name_end).abs() < 0.01 { name_end + w("comment") } else { name_end };
    let mode = x_of("modes");
    Cols {
        vis,
        av: x_of("av"),
        keys: x_of("keys"),
        label: x_of("label"),
        num: x_of("num"),
        name,
        name_end,
        comment,
        comment_w: w("comment"),
        switches: x_of("switches"),
        mode,
        trkmat: mode + MODE_W,
        parent: x_of("parent"),
        parent_w: w("parent"),
        in_: x_of("in"),
        in_w: w("in"),
        out: x_of("out"),
        out_w: w("out"),
        duration: x_of("duration"),
        duration_w: w("duration"),
        stretch: x_of("stretch"),
        stretch_w: w("stretch"),
        render_time: x_of("renderTime"),
        render_time_w: w("renderTime"),
        end: x,
        vx,
        spans,
    }
}

/// The width every visible column needs.
pub(super) fn natural_width(tl: &TimelineState) -> f32 {
    column_order(tl).into_iter().filter(|id| column_visible(tl, id)).map(|id| column_width(tl, id)).sum()
}

/// Which column a header x position falls in.
pub(super) fn column_at(cw: &Cols, x: f32) -> Option<&'static str> {
    cw.spans.iter().find(|(_, a, b)| x >= *a && x < *b).map(|s| s.0)
}

/// Where a column dragged to `x` goes: before the visible column whose middle is right of `x`
/// (`None`: the right end), and the x of that insertion point.
fn drop_target(cw: &Cols, dragged: &str, x: f32) -> (Option<&'static str>, f32) {
    let others = cw.spans.iter().filter(|s| s.0 != dragged);
    for (id, a, b) in others {
        if x < (a + b) / 2.0 {
            return (Some(*id), *a);
        }
    }
    (None, cw.spans.iter().filter(|s| s.0 != dragged).map(|s| s.2).fold(cw.end, f32::max))
}

fn drag_id() -> egui::Id {
    tl_id("tl-col-drag")
}

/// The column headers: labels, dividers to resize, drag to reorder, click the name header for
/// Source Name / Layer Name, right-click for Hide This and Columns.
pub(super) fn header(app: &mut dyn TimelineHost, ui: &mut egui::Ui, p: &egui::Painter, cw: &Cols, hdr: Rect, actions: &mut Vec<(String, Value)>) {
    let t = app.tokens();
    let ctx = ui.ctx().clone();
    let vis = cw.vis;
    let hy = hdr.center().y;
    // Column headers scroll with the outline and never spill into the time ruler.
    let hp = p.with_clip_rect(hdr.intersect(p.clip_rect()));
    let hic = |icon: Icon, x: f32| icons::paint(&hp, Rect::from_center_size(pos2(x, hy), vec2(12.0, 12.0)), icon, t.text_dim);
    let htext = |x: f32, room: f32, s: &str| {
        widgets::text_fit(&hp, pos2(x, hy), Align2::LEFT_CENTER, crate::i18n::tr(s), Tokens::ui(11.0), room, t.text_dim);
    };
    let room = |id: &str, inset: f32| cw.span(id).map_or(0.0, |(a, b)| b - a - inset - 4.0);
    if vis.av {
        hic(Icon::Eye, cw.av + 10.0);
        hic(Icon::Speaker, cw.av + 28.0);
        hic(Icon::Solo, cw.av + 46.0);
        hic(Icon::Lock, cw.av + 64.0);
    }
    if vis.keys {
        htext(cw.keys + 6.0, room("keys", 6.0), "Keys");
    }
    if vis.label {
        icons::paint(&hp, Rect::from_center_size(pos2(cw.label + 10.0, hy), vec2(10.0, 10.0)), Icon::Keyframe, t.text_dim);
    }
    if vis.num {
        htext(cw.num + 6.0, room("num", 6.0), "#");
    }
    htext(cw.name + 22.0, room("name", 22.0), column_label(&app.ui().timeline, "name"));
    if vis.comment {
        htext(cw.comment + 6.0, room("comment", 6.0), "Comment");
    }
    if vis.switches {
        for (i, icon) in
            [Icon::Shy, Icon::Collapse, Icon::Quality, Icon::Fx, Icon::FrameBlend, Icon::MotionBlur, Icon::Adjustment, Icon::Cube].into_iter().enumerate()
        {
            hic(icon, cw.switches + SW * i as f32 + SW / 2.0);
        }
    }
    if vis.modes {
        htext(cw.mode + 4.0, 66.0, "Mode");
        htext(cw.mode + 74.0, 14.0, "T");
        htext(cw.trkmat + 4.0, TRKMAT_W - 8.0, "Track Matte");
    }
    if vis.parent {
        icons::paint(&hp, Rect::from_center_size(pos2(cw.parent + 10.0, hy), vec2(12.0, 12.0)), Icon::PickWhip, t.text_dim);
        htext(cw.parent + 20.0, room("parent", 20.0), "Parent & Link");
    }
    for (on, id, x, s) in [
        (vis.in_, "in", cw.in_, "In"),
        (vis.out, "out", cw.out, "Out"),
        (vis.duration, "duration", cw.duration, "Duration"),
        (vis.stretch, "stretch", cw.stretch, "Stretch"),
        (vis.render_time, "renderTime", cw.render_time, "Render Time"),
    ] {
        if on {
            htext(x + 6.0, room(id, 6.0), s);
        }
    }
    // Faint dividers between the headers, as in After Effects.
    for (_, _, b) in &cw.spans {
        hp.line_segment([pos2(b - 0.5, hdr.min.y + 5.0), pos2(b - 0.5, hdr.max.y - 5.0)], Stroke::new(1.0, t.separator));
    }

    let hresp = ui.interact(hdr, tl_id("tl-colhdr"), Sense::click_and_drag());
    app.auto_mut().add("timeline.header", hdr, "Columns (right-click)");
    for (id, a, b) in &cw.spans {
        let label = column_label(&app.ui().timeline, id);
        app.auto_mut().add(&format!("timeline.header.{id}"), Rect::from_min_max(pos2(*a, hdr.min.y), pos2(*b, hdr.max.y)), label);
    }

    // Reorder: drag a header left or right.
    if hresp.drag_started()
        && let Some(origin) = crate::layer_pointer::Lp::of(ui).pos(ctx.input(|i| i.pointer.press_origin()))
        && let Some(id) = column_at(cw, origin.x)
    {
        ctx.data_mut(|d| d.insert_temp(drag_id(), (id, origin)));
    }
    let dragging: Option<(&'static str, egui::Pos2)> = ctx.data(|d| d.get_temp(drag_id()));
    if let Some((id, origin)) = dragging
        && let Some((a, b)) = cw.span(id)
        && let Some(ptr) = crate::layer_pointer::Lp::of(ui).pos(ctx.input(|i| i.pointer.latest_pos()))
    {
        let fg = crate::layer_pointer::Lp::of(ui).overlay(&ctx, egui::Order::Tooltip, tl_id("tl-col-drag-ghost"));
        let ghost = Rect::from_min_max(pos2(a, hdr.min.y), pos2(b, hdr.max.y)).translate(vec2(ptr.x - origin.x, 0.0));
        fg.rect_filled(ghost, 2.0, t.accent.gamma_multiply(0.25));
        fg.text(ghost.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, crate::i18n::tr(column_label(&app.ui().timeline, id)), Tokens::ui(11.0), t.text);
        let (_, mx) = drop_target(cw, id, ptr.x);
        fg.line_segment([pos2(mx, hdr.min.y), pos2(mx, hdr.max.y)], Stroke::new(2.0, t.accent));
        if hresp.drag_stopped() || !ctx.input(|i| i.pointer.any_down()) {
            let (before, _) = drop_target(cw, id, ptr.x);
            if (ptr.x - origin.x).abs() > 2.0 {
                let mut params = json!({"column": id, "before": before});
                if before.is_none() {
                    params["before"] = Value::Null;
                }
                actions.push(("timeline.column".into(), params));
            }
            ctx.data_mut(|d| d.remove::<(&'static str, egui::Pos2)>(drag_id()));
        }
    }

    // Resize: drag the divider right of a resizable column's header.
    for (id, _, b) in cw.spans.clone() {
        if !column_resizable(id) || b < hdr.min.x || b > hdr.max.x + 3.0 {
            continue;
        }
        let r = Rect::from_min_max(pos2(b - 3.0, hdr.min.y), pos2(b + 3.0, hdr.max.y));
        let resp = ui.interact(r, tl_id(("tl-colw", id)), Sense::drag());
        app.auto_mut().add(&format!("timeline.header.{id}.resize"), r, "Drag to resize the column");
        if resp.hovered() || resp.dragged() {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeColumn);
        }
        let start_id = tl_id(("tl-colw-start", id));
        if resp.drag_started() {
            ctx.data_mut(|d| d.insert_temp(start_id, column_width(&app.ui().timeline, id)));
        }
        if resp.dragged()
            && let (Some(origin), Some(ptr)) = crate::layer_pointer::Lp::of(ui).pair(ctx.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos())))
        {
            let start = ctx.data(|d| d.get_temp::<f32>(start_id)).unwrap_or_else(|| column_width(&app.ui().timeline, id));
            let _ = set_column_width(&mut app.ui_mut().timeline, id, start + ptr.x - origin.x);
        }
        if resp.drag_stopped() {
            ctx.data_mut(|d| d.remove::<f32>(start_id));
        }
    }

    if hresp.clicked()
        && let Some(pt) = hresp.interact_pointer_pos()
        && column_at(cw, pt.x) == Some("name")
    {
        app.ui_mut().timeline.source_name = !app.ui().timeline.source_name;
    }
    if hresp.secondary_clicked() {
        let under = hresp.interact_pointer_pos().and_then(|pt| column_at(cw, pt.x));
        ctx.data_mut(|d| d.insert_temp(tl_id("tl-colhdr-under"), under.unwrap_or("")));
    }
    crate::widgets::context_menu(&hresp, |ui| {
        let under: &str = ctx.data(|d| d.get_temp(tl_id("tl-colhdr-under"))).unwrap_or("");
        if !under.is_empty() && under != "name" && ui.button(crate::i18n::tr("Hide This")).clicked() {
            actions.push(("timeline.column".into(), json!({"column": under, "visible": false})));
            ui.close();
        }
        ui.menu_button(crate::i18n::tr("Columns"), |ui| {
            for id in COLUMNS_MENU {
                let on = column_visible(&app.ui().timeline, id);
                let resp = ui.add_enabled(id != "name", egui::Button::new(crate::i18n::tr(column_label(&app.ui().timeline, id))).selected(on));
                let label = column_label(&app.ui().timeline, id);
                app.auto_mut().add(&format!("timeline.columnsMenu.{id}"), resp.rect, label);
                if resp.clicked() {
                    actions.push(("timeline.column".into(), json!({"column": id, "visible": !on})));
                    ui.close();
                }
            }
        });
        let other = if app.ui().timeline.source_name { "Show Layer Name" } else { "Show Source Name" };
        if ui.button(crate::i18n::tr(other)).clicked() {
            actions.push(("timeline.sourceName".into(), json!({})));
            ui.close();
        }
    });
}

/// The divider between the layer outline and the time graph: drag it to resize the outline,
/// double-click it to fit the outline to its columns again.
pub(super) fn splitter(app: &mut dyn TimelineHost, ui: &mut egui::Ui, x: f32, y0: f32, y1: f32, left_w: f32, panel_w: f32) {
    let ctx = ui.ctx().clone();
    let r = Rect::from_min_max(pos2(x - 3.0, y0), pos2(x + 3.0, y1));
    let resp = ui.interact(r, tl_id("tl-splitter"), Sense::click_and_drag());
    app.auto_mut().add("timeline.splitter", r, "Drag to resize the layer outline");
    if resp.hovered() || resp.dragged() {
        ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let start_id = tl_id("tl-splitter-start");
    if resp.drag_started() {
        ctx.data_mut(|d| d.insert_temp(start_id, left_w));
    }
    if resp.dragged()
        && let (Some(origin), Some(ptr)) = crate::layer_pointer::Lp::of(ui).pair(ctx.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos())))
    {
        let start = ctx.data(|d| d.get_temp::<f32>(start_id)).unwrap_or(left_w);
        app.ui_mut().timeline.outline_w = Some(clamp_outline(start + ptr.x - origin.x, panel_w));
    }
    if resp.drag_stopped() {
        ctx.data_mut(|d| d.remove::<f32>(start_id));
    }
    if resp.double_clicked() {
        app.ui_mut().timeline.outline_w = None;
    }
}

/// The footer's pane buttons (After Effects' lower-left corner): Layer Switches, Transfer
/// Controls (Modes), In/Out/Duration/Stretch and Render Time, then the speed curves in the rows
/// (EffectCraft). Returns where the next footer item goes.
pub(super) fn pane_buttons(app: &mut dyn TimelineHost, ui: &mut egui::Ui, p: &egui::Painter, footer: Rect) -> f32 {
    let t = app.tokens();
    let mut x = footer.min.x + 6.0;
    let panes: [(&str, &str, bool); 5] = [
        ("switches", "Expand or Collapse the Layer Switches pane", column_visible(&app.ui().timeline, "switches")),
        ("modes", "Expand or Collapse the Transfer Controls pane", app.ui().timeline.show_modes),
        ("inOut", "Expand or Collapse the In/Out/Duration/Stretch panes", IN_OUT_PANE.iter().any(|c| column_visible(&app.ui().timeline, c))),
        ("renderTime", "Expand or Collapse the Render Time pane", column_visible(&app.ui().timeline, "renderTime")),
        ("speedRows", "Show or Hide Speed Curves in the Property Rows (⌘-drag a curve to sculpt it)", app.ui().timeline.speed_rows),
    ];
    for (id, tip, on) in panes {
        let r = Rect::from_min_size(pos2(x, footer.center().y - 9.0), vec2(18.0, 18.0));
        let resp = ui.interact(r, tl_id(("tl-pane", id)), Sense::click()).on_hover_text(crate::i18n::tr(tip));
        if on {
            p.rect_filled(r, 2.0, t.hover);
        }
        let col = if resp.hovered() || on { t.text } else { t.text_dim };
        pane_glyph(p, r.shrink(3.0), id, col);
        app.auto_mut().add(&format!("timeline.pane.{id}"), r, tip);
        if resp.clicked() {
            let tl = &mut app.ui_mut().timeline;
            match id {
                "switches" => {
                    let _ = set_column(tl, "switches", !on);
                }
                "modes" => tl.show_modes = !on,
                "speedRows" => tl.speed_rows = !on,
                "renderTime" => {
                    let _ = set_column(tl, "renderTime", !on);
                }
                _ => toggle_in_out_pane(tl),
            }
        }
        x += 22.0;
    }
    x + 6.0
}

/// Small code-drawn pane icons: a row of switch wells, two overlapping blend discs, the in/out
/// brackets and a stopwatch.
fn pane_glyph(p: &egui::Painter, r: Rect, id: &str, col: Color32) {
    let s = Stroke::new(1.0, col);
    match id {
        "switches" => {
            for i in 0..3 {
                let x = r.min.x + 1.0 + i as f32 * 4.5;
                p.rect_stroke(Rect::from_min_size(pos2(x, r.center().y - 2.0), vec2(3.5, 4.0)), 0.5, s, egui::StrokeKind::Inside);
            }
        }
        "modes" => {
            p.circle_stroke(pos2(r.center().x - 2.0, r.center().y), 3.5, s);
            p.circle_stroke(pos2(r.center().x + 2.0, r.center().y), 3.5, s);
        }
        "speedRows" => {
            // A small eased speed curve over its baseline.
            let pts: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let s = i as f32 / 12.0;
                    let bump = (std::f32::consts::PI * s).sin().powi(3);
                    pos2(r.min.x + s * r.width(), r.max.y - 1.0 - bump * (r.height() - 2.0))
                })
                .collect();
            p.add(egui::Shape::line(pts, s));
            p.line_segment([pos2(r.min.x, r.max.y), pos2(r.max.x, r.max.y)], s);
        }
        "renderTime" => {
            let c = pos2(r.center().x, r.center().y + 1.0);
            p.circle_stroke(c, 4.5, s);
            p.line_segment([c, pos2(c.x + 2.0, c.y - 2.5)], s);
            p.line_segment([pos2(c.x - 1.5, r.min.y), pos2(c.x + 1.5, r.min.y)], s);
        }
        _ => {
            let (y0, y1) = (r.min.y + 1.0, r.max.y - 1.0);
            p.line_segment([pos2(r.min.x + 3.0, y0), pos2(r.min.x + 1.0, y0)], s);
            p.line_segment([pos2(r.min.x + 1.0, y0), pos2(r.min.x + 1.0, y1)], s);
            p.line_segment([pos2(r.min.x + 1.0, y1), pos2(r.min.x + 3.0, y1)], s);
            p.line_segment([pos2(r.max.x - 3.0, y0), pos2(r.max.x - 1.0, y0)], s);
            p.line_segment([pos2(r.max.x - 1.0, y0), pos2(r.max.x - 1.0, y1)], s);
            p.line_segment([pos2(r.max.x - 1.0, y1), pos2(r.max.x - 3.0, y1)], s);
            p.line_segment([pos2(r.min.x + 3.5, r.center().y), pos2(r.max.x - 3.5, r.center().y)], s);
        }
    }
}

/// The settings store file the column layout is kept in.
pub const LAYOUT_FILE: &str = "timeline_columns.json";

/// The part of the Timeline's state kept between sessions: the columns shown, their order and
/// widths, the name column's mode and the splitter.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnLayout {
    #[serde(default = "crate::state::default_tl_columns")]
    pub columns: BTreeSet<String>,
    #[serde(default)]
    pub show_modes: bool,
    #[serde(default)]
    pub source_name: bool,
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub widths: BTreeMap<String, f32>,
    #[serde(default)]
    pub outline_width: Option<f32>,
}

impl ColumnLayout {
    pub fn of(tl: &TimelineState) -> ColumnLayout {
        ColumnLayout {
            columns: tl.columns.clone(),
            show_modes: tl.show_modes,
            source_name: tl.source_name,
            order: tl.column_order.clone(),
            widths: tl.column_widths.clone(),
            outline_width: tl.outline_w,
        }
    }

    /// Apply a saved layout; unknown columns and unusable widths are dropped.
    pub fn apply(&self, tl: &mut TimelineState) {
        tl.columns =
            self.columns.iter().filter(|c| c.as_str() != "name" && c.as_str() != "modes" && COLUMNS.iter().any(|k| k.0 == c.as_str())).cloned().collect();
        tl.show_modes = self.show_modes;
        tl.source_name = self.source_name;
        tl.column_order = self.order.iter().filter(|c| COLUMNS.iter().any(|k| k.0 == c.as_str())).cloned().collect();
        tl.column_widths.clear();
        for (id, w) in &self.widths {
            let _ = set_column_width(tl, id, *w);
        }
        tl.outline_w = self.outline_width.filter(|w| w.is_finite() && *w > 0.0);
    }
}

/// The layout saved in an earlier session, from the settings store (a file that doesn't read is
/// ignored). Run once when the app starts.
pub(crate) fn load_layout(app: &mut EffectcraftApp) {
    let text = app.session().config.as_ref().and_then(|c| c.read(LAYOUT_FILE));
    match text.map(|t| serde_json::from_str::<ColumnLayout>(&t)) {
        Some(Ok(l)) => l.apply(&mut app.ui_mut().timeline),
        Some(Err(e)) => log::warn!("{LAYOUT_FILE}: {e}"),
        None => {}
    }
    app.timeline_layout_saved = Some(ColumnLayout::of(&app.ui().timeline));
}

/// Keep the layout for the next session once it changed (not in the middle of a drag). Run
/// every frame.
pub(crate) fn store_layout(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let now = ColumnLayout::of(&app.ui().timeline);
    if app.timeline_layout_saved.as_ref() == Some(&now) {
        return;
    }
    if let Some(c) = &app.session().config {
        let written = serde_json::to_string(&now).map_err(std::io::Error::other).and_then(|t| c.write(LAYOUT_FILE, &t));
        if let Err(e) = written {
            log::warn!("{LAYOUT_FILE}: {e}");
        }
    }
    app.timeline_layout_saved = Some(now);
}

/// `timeline.columnLayout {reset?, outlineWidth?}`: the column layout (and the splitter), or
/// back to After Effects' defaults with `reset`.
pub(crate) fn layout_command(app: &mut dyn TimelineHost, params: &Value) -> Result<Value, String> {
    let tl = &mut app.ui_mut().timeline;
    if params.get("reset").and_then(Value::as_bool) == Some(true) {
        let d = TimelineState::default();
        tl.columns = d.columns;
        tl.show_modes = d.show_modes;
        tl.column_order.clear();
        tl.column_widths.clear();
        tl.outline_w = None;
    }
    match params.get("outlineWidth") {
        Some(Value::Null) => tl.outline_w = None,
        Some(v) => tl.outline_w = Some(v.as_f64().ok_or("outlineWidth: need a number or null")?.max(0.0) as f32),
        None => {}
    }
    Ok(describe(tl))
}

/// The layout as agents see it: the visible columns left to right with their widths.
pub(crate) fn describe(tl: &TimelineState) -> Value {
    let order: Vec<&str> = column_order(tl);
    let shown: Vec<&str> = order.iter().copied().filter(|c| column_visible(tl, c)).collect();
    let widths: serde_json::Map<String, Value> = order.iter().map(|c| (c.to_string(), json!(column_width(tl, c)))).collect();
    json!({"columns": shown, "order": order, "widths": widths, "outlineWidth": tl.outline_w, "sourceName": tl.source_name})
}
