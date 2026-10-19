//! View menu and the Composition viewer: zoom, resolution, guides and grids, theme, nudging the
//! selected layers with the arrow keys.

use serde_json::{Value, json};

use super::dispatch::Handler;
use super::{run_engine, toggle};
use crate::EffectcraftApp;
use crate::dock::PanelKind;
use crate::state::Resolution;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[
        Handler::ui_prefix("viewer.nudge.", nudge_cmd),
        Handler::ui("viewer.freeTransform", free_transform),
        Handler::ui("view.theme.toggle", theme_toggle),
        Handler::ui_prefix("view.theme.", theme),
        Handler::ui("view.fit", fit),
        Handler::ui("view.fitUpTo100", fit_up_to_100),
        Handler::ui("view.nextItem", step_item),
        Handler::ui("view.previousItem", step_item),
        Handler::ui("view.actualSize", actual_size),
        Handler::ui("view.safeMargins", safe_margins),
        Handler::ui("view.transparencyGrid", transparency_grid),
        Handler::ui("view.fastPreviews", fast_previews),
        // View ▸ New Viewer.
        Handler::frontend("view.newViewer", new_viewer),
        Handler::frontend("view.zoomIn", zoom),
        Handler::frontend("view.zoomOut", zoom),
        // View ▸ Resolution ▸ Full…Quarter are engine commands (each comp keeps its Resolution);
        // Custom without a factor comes here for its dialog.
        Handler::frontend("view.res.custom", res_custom),
        Handler::frontend("view.rulers", rulers),
        Handler::frontend("view.guides", guides),
        Handler::frontend("view.snapToGuides", snap_to_guides),
        Handler::frontend("view.lockGuides", lock_guides),
        Handler::frontend("view.grid", grid),
        Handler::frontend("view.snapToGrid", snap_to_grid),
        Handler::frontend("view.layerControls", layer_controls),
        Handler::frontend("view.sharpZoom", sharp_zoom),
        Handler::frontend("view.options", options),
        Handler::frontend("view.panelBackground", panel_background),
        Handler::frontend("view.fullScreen", full_screen),
        Handler::frontend("view.lookAt", look_at),
    ];
    H
}

fn nudge_cmd(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    nudge(app, ctx, id.strip_prefix("viewer.nudge.").unwrap_or(id))
}

/// The arrow keys over the Composition panel or the Timeline: move the selected layers 1 pixel
/// at the viewer's magnification (`up`, `down`, `left`, `right`), 10 with Shift (`up10`…), as
/// After Effects does, so zoomed in they move by fractions of a comp pixel. Not while typing,
/// Selected path vertices take precedence over layers.
fn nudge(app: &mut EffectcraftApp, ctx: &egui::Context, dir: &str) -> Result<Value, String> {
    let (side, n) = match dir.strip_suffix("10") {
        Some(side) => (side, 10.0),
        None => (dir, 1.0),
    };
    let (x, y) = match side {
        "up" => (0.0, -n),
        "down" => (0.0, n),
        "left" => (-n, 0.0),
        "right" => (n, 0.0),
        _ => return Err(format!("unknown nudge `{dir}`")),
    };
    let st = &app.session.state;
    if app.ui.focused == PanelKind::Project || st.selected_layers.is_empty() || st.text_edit.is_some() {
        return Ok(Value::Null);
    }
    // Screen pixels per comp pixel.
    let zoom = (app.ui.viewer.zoom.unwrap_or_else(|| crate::panels::viewer::last_fit(ctx)) * ctx.pixels_per_point()) as f64;
    let k = if zoom.is_finite() && zoom > 1e-6 { 1.0 / zoom } else { 1.0 };
    if !st.selected_vertices.is_empty() {
        return crate::panels::viewer::move_path_vertices(app, ctx, [x * k, y * k], None);
    }
    run_engine(app, ctx, "layer.nudge", json!({"x": x * k, "y": y * k}))
}

/// The path Free Transform Points would work on: the one with selected points, else a selected
/// mask or shape path (the Timeline selection).
pub(crate) fn free_transform_target(app: &EffectcraftApp) -> Option<(u64, u64)> {
    let st = &app.session.state;
    if let Some(v) = st.selected_vertices.first() {
        return Some((v.layer.0, v.mask));
    }
    let comp = app.session.active_comp()?;
    st.selected_props
        .iter()
        .find(|(l, uid)| comp.layer(*l).and_then(|l| l.props.find_group(*uid)).and_then(|g| g.get("path")).is_some_and(|p| p.value.as_path().is_some()))
        .map(|(l, uid)| (l.0, *uid))
}

/// Free Transform Points in the viewer (`viewer.freeTransform`): with no params, start it on the
/// path whose points (or mask) are selected, as Cmd+T does in After Effects; `{layer, path}`
/// starts it on that path; `{end: true}` ends it; `angle` (degrees) turns the box and `anchor`
/// ([u, v], box coordinates, `[0.5, 0.5]` the centre) moves its anchor point. Returns the state.
fn free_transform(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    use crate::panels::viewer_overlays as ov;
    if p.get("end").and_then(Value::as_bool) == Some(true) {
        ov::end_free_transform(app);
        return Ok(json!({"freeTransform": null}));
    }
    let given = p.get("layer").and_then(Value::as_u64).zip(p.get("path").and_then(Value::as_u64));
    let start = match given {
        Some(t) => Some(t),
        None if app.ui.viewer.free_transform.is_none() || (p.get("angle").is_none() && p.get("anchor").is_none()) => {
            Some(free_transform_target(app).ok_or("Select points of a mask or shape path first")?)
        }
        None => None,
    };
    if let Some((layer, path)) = start
        && ov::free_transform_target(app) != Some((effectcraft_engine::project::LayerId(layer), path))
    {
        ov::begin_free_transform(app, effectcraft_engine::project::LayerId(layer), path);
    }
    let anchor = p.get("anchor").and_then(Value::as_array).and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]));
    ov::set_free_transform(app, p.get("angle").and_then(Value::as_f64), anchor);
    serde_json::to_value(&app.ui.viewer.free_transform).map(|v| json!({"freeTransform": v})).map_err(|e| e.to_string())
}

/// View ▸ Next Appearance Mode: Dark ↔ Light.
fn theme_toggle(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.cycle_appearance(ctx);
    Ok(json!({"appearanceMode": app.session.prefs.appearance.appearance_mode}))
}

fn theme(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let th = id.strip_prefix("view.theme.").unwrap_or(id);
    let k = crate::theme::ThemeKind::from_name(th).ok_or("unknown theme")?;
    app.set_theme(ctx, k);
    Ok(Value::Null)
}

/// View ▸ Fit: the focused Layer panel has its own magnification; otherwise the Composition
/// viewer.
fn fit(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    if app.ui.focused == PanelKind::Layer {
        app.ui.layer_zoom = None;
        app.ui.layer_pan = [0.0, 0.0];
        return Ok(Value::Null);
    }
    app.ui.viewer.zoom = None;
    app.ui.viewer.fit_up_to_100 = false;
    app.ui.viewer.pan = [0.0, 0.0];
    Ok(Value::Null)
}

fn fit_up_to_100(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.ui.viewer.zoom = None;
    app.ui.viewer.fit_up_to_100 = true;
    app.ui.viewer.pan = [0.0, 0.0];
    Ok(Value::Null)
}

fn step_item(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let open = app.session.state.open_comps.clone();
    let Some(i) = app.session.state.active_comp.and_then(|c| open.iter().position(|o| *o == c)) else { return Ok(Value::Null) };
    let step = if id == "view.nextItem" { 1 } else { open.len().saturating_sub(1) };
    let Some(next) = open.get((i + step) % open.len().max(1)).copied().filter(|n| Some(*n) != app.session.state.active_comp) else { return Ok(Value::Null) };
    run_engine(app, ctx, "comp.open", json!({"comp": next.0}))
}

fn actual_size(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    if app.ui.focused == PanelKind::Layer {
        app.ui.layer_zoom = Some(1.0 / ctx.pixels_per_point());
        app.ui.layer_pan = [0.0, 0.0];
        return Ok(Value::Null);
    }
    app.ui.viewer.zoom = Some(1.0 / ctx.pixels_per_point());
    app.ui.viewer.pan = [0.0, 0.0];
    Ok(Value::Null)
}

fn safe_margins(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.ui.viewer.safe_margins = !app.ui.viewer.safe_margins;
    Ok(Value::Null)
}

fn transparency_grid(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.ui.viewer.transparency_grid = !app.ui.viewer.transparency_grid;
    Ok(Value::Null)
}

fn fast_previews(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    // With a mode: Fast Previews ▸ Off / Adaptive Resolution / Draft / Fast Draft /
    // Wireframe; without: toggle Draft quality (Settings ▸ Previews).
    if params.get("mode").is_some() {
        return run_engine(app, ctx, "view.fastPreviewMode", params);
    }
    let on = !app.ui.viewer.fast_preview;
    app.set_pref("previews.fastPreviews", json!(on))?;
    app.ui.viewer.fast_preview = on;
    Ok(Value::Null)
}

fn new_viewer(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _p: Value) -> Result<Value, String> {
    Ok(json!({"viewer": crate::panels::viewers::new_viewer(app)}))
}

/// Zoom In / Zoom Out act on the focused viewer: the Layer panel when it has focus, else the
/// Composition viewer.
fn zoom(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _p: Value) -> Result<Value, String> {
    if app.ui.focused == PanelKind::Layer {
        let cur = app.ui.layer_zoom.unwrap_or_else(|| crate::panels::layer_panel::last_fit(ctx));
        // The Layer panel zooms in points (pixels-per-point 1).
        let next = super::zoom_step(cur, 1.0, id == "view.zoomIn");
        app.ui.layer_zoom = Some(next);
        return Ok(json!({"zoom": next, "panel": "layer"}));
    }
    let ppp = ctx.pixels_per_point();
    let v = &mut app.ui.viewer;
    let cur = v.zoom.unwrap_or_else(|| crate::panels::viewer::last_fit(ctx));
    let next = super::zoom_step(cur, ppp, id == "view.zoomIn");
    v.zoom = Some(next);
    Ok(json!({"zoom": next}))
}

/// View ▸ Resolution ▸ Custom without a factor (the engine sets the comp's resolution).
fn res_custom(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _p: Value) -> Result<Value, String> {
    let cur = match app.viewer_res() {
        Resolution::Custom(n) => f64::from(n),
        _ => 2.0,
    };
    crate::panels::dialogs::form(
        app,
        "Custom Resolution",
        "view.res.custom",
        json!({}),
        vec![crate::panels::dialogs::Field::num("factor", "Render every n-th pixel", cur)],
    );
    Ok(Value::Null)
}

fn rulers(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.rulers, &p))
}

fn guides(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.guides, &p))
}

fn snap_to_guides(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.snap_guides, &p))
}

fn lock_guides(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.lock_guides, &p))
}

fn grid(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.grid, &p))
}

fn snap_to_grid(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.snap_grid, &p))
}

fn layer_controls(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.show_layer_controls, &p))
}

/// View ▸ Sharp Zoom: render the visible region at the magnification above 100 % (on by
/// default; `value` sets it, else it toggles).
fn sharp_zoom(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    Ok(toggle(&mut app.ui.viewer.sharp_zoom, &p))
}

fn options(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _p: Value) -> Result<Value, String> {
    app.dialog = Some(crate::Dialog::ViewOptions);
    Ok(Value::Null)
}

fn panel_background(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let v = &mut app.ui.viewer;
    if p.get("pick").and_then(Value::as_bool) == Some(true) {
        let [r, g, b] = v.custom_pasteboard;
        crate::panels::dialogs::form(
            app,
            "Panel Background Color",
            "view.panelBackground",
            json!({}),
            vec![crate::panels::dialogs::Field::text("color", "Color (#rrggbb)", &format!("#{r:02x}{g:02x}{b:02x}"))],
        );
        return Ok(Value::Null);
    }
    let c = p.get("color").and_then(Value::as_str).unwrap_or("mediumGray");
    v.pasteboard = match c {
        "black" => Some([0, 0, 0]),
        "darkGray" => Some([0x2a, 0x2a, 0x2a]),
        "mediumGray" => None,
        "lightGray" => Some([0xb4, 0xb4, 0xb4]),
        "white" => Some([0xff, 0xff, 0xff]),
        "custom" => Some(v.custom_pasteboard),
        hex => {
            let c = effectcraft_engine::color::Rgba::from_hex(hex).ok_or_else(|| format!("unknown colour `{hex}`"))?;
            let rgb = [(c.r * 255.0).round() as u8, (c.g * 255.0).round() as u8, (c.b * 255.0).round() as u8];
            v.custom_pasteboard = rgb;
            Some(rgb)
        }
    };
    Ok(Value::Null)
}

fn full_screen(_app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _p: Value) -> Result<Value, String> {
    let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
    Ok(json!(!full))
}

fn look_at(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let r: Vec<f64> = p.get("rect").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    if r.len() == 4 {
        let map: Option<crate::panels::viewer::ViewerMap> = ctx.data(|d| d.get_temp(egui::Id::new("viewer-map")));
        if let Some(m) = map {
            let (rw, rh) = (((r[2] - r[0]) as f32).max(1.0), ((r[3] - r[1]) as f32).max(1.0));
            let zoom = ((m.area.width() - 40.0) / rw).min((m.area.height() - 40.0) / rh).clamp(0.015, 32.0);
            let rc = [(r[0] + r[2]) as f32 / 2.0, (r[1] + r[3]) as f32 / 2.0];
            let v = &mut app.ui.viewer;
            v.zoom = Some(zoom);
            v.pan = [m.comp[0] * zoom / 2.0 - rc[0] * zoom, m.comp[1] * zoom / 2.0 - rc[1] * zoom];
        }
    }
    Ok(Value::Null)
}
