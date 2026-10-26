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
/// (or removes it from) those already revealed instead of replacing them.
pub fn reveal(app: &mut EffectcraftApp, kind: &str, now: f64, add: bool) {
    // The double-press shortcuts: the same key again within 0.6 s reveals its second set (LL
    // Waveform, TT Mask Opacity, MM all mask properties, EE Expressions, RR Time Remap, AA
    // Material Options, PP paint / Roto Brush / Puppet, SS selected properties, FF missing
    // effects). A single M shows Mask Path.
    let double = |k: &str| -> Option<&'static str> {
        Some(match k {
            "levels" => "waveform",
            "opacity" => "maskOpacity",
            "masks" => "masks",
            "effects" => "expressions",
            "rotation" => "timeRemap",
            "anchor" => "material",
            "position" => "paint",
            "scale" => "selected",
            "feather" => "missingEffects",
            _ => return None,
        })
    };
    let pressed = kind;
    // (With Shift a second press removes the property again instead.)
    let again = !add && matches!(app.last_reveal.take(), Some((k, t)) if k == pressed && now - t < 0.6);
    app.last_reveal = Some((if again { String::new() } else { pressed.to_string() }, now));
    let kind = match (again, double(pressed)) {
        (true, Some(d)) => {
            // The second press replaces what the first one revealed.
            let first = if pressed == "masks" { "maskPath" } else { pressed };
            app.ui.timeline.reveal.retain(|k| k != first);
            d
        }
        _ if pressed == "masks" => "maskPath",
        _ => pressed,
    };
    let targets = reveal_targets(app);
    let tl = &mut app.ui.timeline;
    if add {
        // Shift+key adds to what each layer shows now, however it got there (another reveal
        // key, U, Alt+Shift+key), as in After Effects; when every layer shows it already, it
        // is taken away.
        let on = !targets.iter().all(|id| tl.layer_reveal.get(id).is_some_and(|k| k.iter().any(|k| k == kind)));
        let mut shown = Vec::with_capacity(targets.len());
        for id in &targets {
            let mut kinds = tl.layer_reveal.get(id).cloned().unwrap_or_default();
            kinds.retain(|k| k != kind && (k != "props" || !tl.reveal_props.is_empty()));
            if on {
                kinds.push(kind.to_string());
            }
            tl.apply_reveal(&[*id], kinds.clone());
            shown.push((*id, kinds));
        }
        for (id, kinds) in shown {
            crate::panels::timeline::open_revealed(app, &[id], &kinds);
        }
        return;
    }
    let present = tl.reveal.iter().any(|k| k == kind);
    let mut kinds = tl.reveal.clone();
    if present && kinds.len() == 1 {
        kinds.clear();
    } else {
        kinds = vec![kind.to_string()];
    }
    tl.apply_reveal(&targets, kinds.clone());
    crate::panels::timeline::open_revealed(app, &targets, &kinds);
}

/// After Alt+Shift+P (A, S, R, T) the Timeline shows the property on each layer it keyed, as in
/// After Effects: added to what a reveal shortcut shows, else with the layer's Transform twirled
/// open, else alone like P.
fn reveal_keyed(app: &mut EffectcraftApp, keyed: &Value, kind: &str) {
    let layers: std::collections::BTreeSet<u64> = keyed.as_array().into_iter().flatten().filter_map(|k| k.get("layer").and_then(Value::as_u64)).collect();
    let Some(comp) = app.session.active_comp() else { return };
    let tl = &mut app.ui.timeline;
    for id in layers {
        let shown = tl.layer_reveal.get(&id).filter(|k| !k.is_empty()).cloned();
        match shown {
            Some(mut kinds) => {
                if !kinds.iter().any(|k| k == kind) {
                    kinds.push(kind.to_string());
                    tl.layer_reveal.insert(id, kinds);
                }
            }
            None if tl.open_layers.contains(&id) => {
                if let Some(tr) = comp.layer(effectcraft_engine::project::LayerId(id)).and_then(|l| l.transform()) {
                    tl.open_groups.insert(tr.uid);
                }
            }
            None => {
                tl.open_layers.insert(id);
                tl.layer_reveal.insert(id, vec![kind.to_string()]);
            }
        }
    }
}

/// The layers a reveal shortcut acts on: the selected ones, else every layer of the active comp.
fn reveal_targets(app: &EffectcraftApp) -> Vec<u64> {
    if app.session.state.selected_layers.is_empty() {
        app.session.active_comp().map(|c| c.layers.iter().map(|l| l.id.0).collect()).unwrap_or_default()
    } else {
        app.session.state.selected_layers.iter().map(|l| l.0).collect()
    }
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
    let sel: Vec<u64> = app.session.state.selected_layers.iter().map(|l| l.0).collect();
    let tl = &mut app.ui.timeline;
    let revealing = sel.iter().any(|l| tl.layer_reveal.contains_key(l));
    tl.reveal.clear();
    for l in &sel {
        tl.layer_reveal.remove(l);
    }
    if !revealing && sel.iter().all(|l| tl.open_layers.contains(l)) {
        for l in &sel {
            tl.open_layers.remove(l);
        }
    } else {
        tl.open_layers.extend(sel);
    }
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
    let props = p.get("props").and_then(Value::as_array).cloned().unwrap_or_default();
    let uids = |k: &str| -> Vec<u64> { props.iter().filter_map(|x| x.get(k).and_then(Value::as_u64)).collect() };
    let (found, with_props) = (uids("prop"), uids("layer"));
    // The layers it looked at (`layers`), else those it found something on. Layers that
    // have nothing to show close; the others' reveals stay.
    let targets: Vec<u64> = match p.get("layers").and_then(Value::as_array) {
        Some(l) => l.iter().filter_map(Value::as_u64).collect(),
        None => with_props.clone(),
    };
    // The targets' earlier picks go (U after UU shows the keyframed ones only).
    let comp = app.session.active_comp_arc();
    let theirs = |u: u64| {
        comp.as_ref().is_some_and(|c| {
            targets
                .iter()
                .filter_map(|l| c.layer(effectcraft_engine::project::LayerId(*l)))
                .any(|l| l.props.find(u).is_some() || l.props.find_group(u).is_some())
        })
    };
    let tl = &mut app.ui.timeline;
    tl.reveal_props.retain(|u| !theirs(*u));
    tl.reveal_props.extend(found.iter().copied());
    let (shown, hidden): (Vec<u64>, Vec<u64>) = targets.into_iter().partition(|l| with_props.contains(l));
    tl.apply_reveal(&hidden, vec![]);
    if !shown.is_empty() {
        tl.apply_reveal(&shown, vec!["props".into()]);
    }
    // Properties inside effects (puppet pins…) show under their effect, twirled open.
    for (layer, prop) in props.iter().filter_map(|x| Some((x.get("layer")?.as_u64()?, x.get("prop")?.as_u64()?))) {
        crate::panels::timeline::open_effect_paths(app, layer, &[prop]);
    }
    Ok(json!({"revealed": found.len()}))
}
