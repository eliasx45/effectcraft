//! Timeline panel commands: the reveal shortcuts (P, S, R, T, A, E, M, F, L, U and their
//! double presses), keyframe toggles, time zoom, columns, twirling, work area and layer timing.

use serde_json::{Value, json};

use super::dispatch::Handler;
use super::run_engine;
use crate::EffectcraftApp;
use crate::dock::PanelKind;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[
        Handler::ui_prefix("timeline.reveal.", reveal_cmd),
        Handler::ui_prefix("timeline.keyAt.", key_at),
        Handler::ui_prefix("timeline.revealAdd.", reveal_add),
        Handler::ui("anim.reveal", anim_reveal),
        Handler::ui("timeline.zoomIn", zoom),
        Handler::ui("timeline.zoomOut", zoom),
        Handler::ui("timeline.zoomFit", zoom_fit),
        Handler::ui("timeline.scrollToTime", scroll_to),
        Handler::ui("timeline.scrollToLayer", scroll_to),
        Handler::ui("timeline.parentColumn", parent_column),
        Handler::ui("timeline.zoomFrameToggle", zoom_frame_toggle),
        Handler::ui("timeline.column", column),
        Handler::ui("timeline.columnLayout", column_layout),
        Handler::ui("timeline.sourceName", source_name),
        Handler::ui("timeline.search", search),
        Handler::ui("timeline.graphEditor", graph_editor),
        Handler::ui("timeline.graph.fit", graph_fit),
        Handler::ui("timeline.switchesModes", switches_modes),
        Handler::ui("timeline.collapseAll", collapse_all),
        Handler::ui("timeline.twirlSelected", twirl_selected),
        Handler::ui("timeline.workAreaBegin", work_area_begin),
        Handler::ui("timeline.workAreaEnd", work_area_end),
        Handler::ui("timeline.moveInToTime", timing),
        Handler::ui("timeline.moveOutToTime", timing),
        Handler::ui("timeline.trimInToTime", timing),
        Handler::ui("timeline.trimOutToTime", timing),
        Handler::frontend("timeline.revealProps", reveal_props),
    ];
    H
}

/// The reveal shortcuts (P, S, R, T, A, E, M, F, L…). `add` (Shift+key) adds the property to
/// (or removes it from) those already revealed instead of replacing them. Shared with hosted
/// Timelines (`panels::timeline::hosted::reveal`).
pub fn reveal(app: &mut EffectcraftApp, kind: &str, now: f64, add: bool) {
    let last = app.last_reveal.take();
    app.last_reveal = crate::panels::timeline::hosted::reveal(app, kind, now, add, last);
}

/// After Alt+Shift+P (A, S, R, T) the Timeline shows the property on each layer it keyed.
fn reveal_keyed(app: &mut EffectcraftApp, keyed: &Value, kind: &str) {
    crate::panels::timeline::hosted::reveal_keyed(app, keyed, kind);
}

/// The layers a reveal shortcut acts on: the selected ones, else every layer of the active comp.
fn reveal_targets(app: &EffectcraftApp) -> Vec<u64> {
    crate::panels::timeline::hosted::reveal_targets(app)
}

fn reveal_cmd(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    let k = id.strip_prefix("timeline.reveal.").unwrap_or(id);
    if k == "animated" {
        return run_engine(app, ctx, "anim.reveal", json!({"kind": "keyframes"}));
    }
    reveal(app, k, now, false);
    Ok(Value::Null)
}

fn key_at(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let k = id.strip_prefix("timeline.keyAt.").unwrap_or(id);
    let r = run_engine(app, ctx, "keys.toggleTransform", json!({"prop": k}))?;
    reveal_keyed(app, &r, k);
    Ok(r)
}

fn reveal_add(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    let k = id.strip_prefix("timeline.revealAdd.").unwrap_or(id);
    reveal(app, k, now, true);
    Ok(json!({"revealed": app.ui.timeline.reveal}))
}

/// U reveals keyframed properties; UU (pressed twice quickly) all modified properties; U
/// again later hides them. Anything else goes to the engine.
fn anim_reveal(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    if params.get("kind").and_then(Value::as_str) == Some("keyframes") {
        let last = app.last_reveal.take();
        app.last_reveal = Some(("keyframes".into(), now));
        match last {
            Some((k, t)) if k == "keyframes" && now - t < 0.6 => {
                return run_engine(app, ctx, "anim.reveal", json!({"kind": "modified"}));
            }
            Some((k, _)) if k == "keyframes" && app.ui.timeline.reveal == ["props"] => {
                let targets = reveal_targets(app);
                app.ui.timeline.apply_reveal(&targets, vec![]);
                return Ok(json!({"revealed": 0}));
            }
            _ => {}
        }
    }
    super::engine(app, ctx, id, params)
}

fn zoom(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let k = if id == "timeline.zoomIn" { crate::panels::timeline::ZOOM_STEP } else { 1.0 / crate::panels::timeline::ZOOM_STEP };
    crate::panels::timeline::zoom(app, ctx, k);
    Ok(Value::Null)
}

fn zoom_fit(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    crate::panels::timeline::toggle_fit_zoom(app, ctx);
    Ok(Value::Null)
}

fn scroll_to(app: &mut EffectcraftApp, _ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    app.ui.timeline.scroll_to = Some(if id == "timeline.scrollToTime" { crate::state::ScrollTo::Time } else { crate::state::ScrollTo::Layer });
    Ok(Value::Null)
}

fn parent_column(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    let on = !crate::panels::timeline_columns::column_visible(&app.ui.timeline, "parent");
    crate::panels::timeline_columns::set_column(&mut app.ui.timeline, "parent", on)?;
    Ok(json!({"parent": on}))
}

fn zoom_frame_toggle(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    crate::panels::timeline::toggle_frame_zoom(app, ctx);
    Ok(Value::Null)
}

/// `timeline.column {column, visible?, width?, before?}`: show or hide a column (toggles when
/// nothing else is given), resize it, or move it before another column (`before: null`: to the
/// right end). Returns the layout ([`crate::panels::timeline_columns::describe`]).
fn column(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    use crate::panels::timeline_columns as tc;
    let col = params.get("column").and_then(Value::as_str).ok_or("timeline.column: need `column`")?;
    let tl = &mut app.ui.timeline;
    let width = params.get("width");
    let before = params.get("before");
    if let Some(w) = width {
        tc::set_column_width(tl, col, w.as_f64().ok_or("timeline.column: `width` must be a number")? as f32)?;
    }
    if let Some(b) = before {
        tc::move_column(tl, col, b.as_str())?;
    }
    let visible = params.get("visible").and_then(Value::as_bool);
    if visible.is_some() || (width.is_none() && before.is_none()) {
        let on = visible.unwrap_or(!tc::column_visible(tl, col));
        tc::set_column(tl, col, on)?;
    }
    Ok(tc::describe(tl))
}

/// `timeline.columnLayout {reset?, outlineWidth?}`: read or reset the column layout, or place the
/// outline / time graph splitter (`outlineWidth: null` fits the outline to its columns).
fn column_layout(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::timeline_columns::layout_command(app, &params)
}

fn source_name(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    let tl = &mut app.ui.timeline;
    tl.source_name = params.get("value").and_then(Value::as_bool).unwrap_or(!tl.source_name);
    Ok(json!({"sourceName": tl.source_name}))
}

fn search(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    app.ui.timeline.search = params.get("query").and_then(Value::as_str).unwrap_or_default().to_string();
    app.show_panel(PanelKind::Timeline);
    Ok(Value::Null)
}

/// Shift+F3: the Graph Editor in the Timeline, on or off (`value` sets it). Shared with hosted
/// Timelines (`panels::timeline::hosted`).
fn graph_editor(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
    crate::panels::timeline::hosted::run(app, ctx, id, &params).unwrap_or(Ok(Value::Null))
}

/// F in the Graph Editor (its bottom bar's Fit button): Fit All Graphs to View. The Graph Editor
/// frames the graphs when it is next drawn, where its plot is known.
fn graph_fit(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    crate::panels::timeline::hosted::graph_fit(app, ctx)
}

fn switches_modes(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    crate::panels::timeline_columns::toggle_switches_modes(&mut app.ui.timeline);
    Ok(Value::Null)
}

fn collapse_all(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.ui.timeline.open_layers.clear();
    app.ui.timeline.open_groups.clear();
    app.ui.timeline.reveal.clear();
    app.ui.timeline.layer_reveal.clear();
    Ok(Value::Null)
}

/// Ctrl+`: twirl the selected layers open (all closed ones) or closed (all open). From a
/// reveal shortcut's view it opens their full property trees.
fn twirl_selected(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    crate::panels::timeline::hosted::twirl_selected(app);
    Ok(Value::Null)
}

fn work_area_begin(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.session.execute("comp.workArea", json!({"set": "begin"})).map_err(|e| e.to_string())
}

fn work_area_end(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.session.execute("comp.workArea", json!({"set": "end"})).map_err(|e| e.to_string())
}

/// `timeline.moveInToTime` … `timeline.trimOutToTime`: `layer.timing {op}`.
fn timing(app: &mut EffectcraftApp, _ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let op = id.strip_prefix("timeline.").unwrap_or(id);
    app.session.execute("layer.timing", json!({"op": op})).map_err(|e| e.to_string())
}

fn reveal_props(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(crate::panels::timeline::hosted::reveal_props(app, &p))
}
