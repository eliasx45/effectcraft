//! Animation dialogs: Keyframe Velocity, Keyframe Interpolation and Time Stretch. Each one is
//! pre-filled from the engine (`keys.info`, the layer) and commits through one engine command.

use crate::i18n::tr;
use egui::{Color32, vec2};
use serde_json::{Value, json};

use crate::automation::Registry;
use crate::theme::Tokens;
use crate::{Dialog, EffectcraftApp};

#[derive(Clone, Debug, Default)]
pub struct VelocityDraft {
    pub dims: usize,
    pub units: String,
    pub name: String,
    pub in_speed: Vec<f64>,
    pub in_inf: Vec<f64>,
    pub out_speed: Vec<f64>,
    pub out_inf: Vec<f64>,
    /// Which sides exist (first key has no incoming side, last none outgoing).
    pub has_in: bool,
    pub has_out: bool,
    pub continuous: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InterpDraft {
    /// Index into TEMPORAL / SPATIAL / ROVING (0 = Current Settings).
    pub temporal: usize,
    pub spatial: usize,
    pub roving: usize,
    pub has_spatial: bool,
    pub count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct StretchDraft {
    pub percent: f64,
    pub duration: f64,
    /// 0 = Layer In-point, 1 = Current Frame, 2 = Layer Out-point.
    pub hold: usize,
    pub orig_duration: f64,
    pub orig_percent: f64,
}

pub const TEMPORAL: [(&str, &str); 6] = [
    ("Current Settings", ""),
    ("Linear", "linear"),
    ("Bezier", "bezier"),
    ("Continuous Bezier", "continuousBezier"),
    ("Auto Bezier", "autoBezier"),
    ("Hold", "hold"),
];
pub const SPATIAL: [(&str, &str); 5] =
    [("Current Settings", ""), ("Linear", "linear"), ("Bezier", "bezier"), ("Continuous Bezier", "continuousBezier"), ("Auto Bezier", "autoBezier")];
pub const ROVING: [&str; 3] = ["Current Settings", "Lock To Time", "Rove Across Time"];

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Animation ▸ Keyframe Velocity… (⇧⌘K) with keys selected.
pub fn open_velocity(app: &mut EffectcraftApp) -> Result<(), String> {
    app.dialog_state.velocity = velocity_draft(&mut app.session)?;
    app.dialog = Some(Dialog::KeyVelocity);
    Ok(())
}

/// Keyframe Velocity's draft from the selected keyframes of `s` (the first one's ease; a host's
/// dialog too, EMBED12).
pub fn velocity_draft(s: &mut effectcraft_engine::Session) -> Result<VelocityDraft, String> {
    let info = s.execute("keys.info", json!({})).map_err(|e| e.to_string())?;
    let first = info.as_array().and_then(|a| a.first()).ok_or("select keyframes first")?;
    let dims = first["dims"].as_u64().unwrap_or(1).max(1) as usize;
    let side = |k: &str| -> (Vec<f64>, Vec<f64>, bool) {
        let a = first[k].as_array().cloned().unwrap_or_default();
        let has = a.iter().any(|e| !e.is_null());
        let sp = (0..dims).map(|d| a.get(d).map(|e| num(&e["speed"])).unwrap_or(0.0)).collect();
        let inf = (0..dims).map(|d| a.get(d).map(|e| num(&e["influence"])).filter(|v| *v > 0.0).unwrap_or(33.33)).collect();
        (sp, inf, has)
    };
    let (in_speed, in_inf, has_in) = side("inEase");
    let (out_speed, out_inf, has_out) = side("outEase");
    Ok(VelocityDraft {
        dims,
        units: first["units"].as_str().unwrap_or("units/sec").into(),
        name: first["name"].as_str().unwrap_or("").into(),
        in_speed,
        in_inf,
        out_speed,
        out_inf,
        has_in,
        has_out,
        continuous: first["continuous"].as_bool().unwrap_or(false),
    })
}

/// Animation ▸ Keyframe Interpolation… (⌥⌘K).
pub fn open_interpolation(app: &mut EffectcraftApp) -> Result<(), String> {
    app.dialog_state.interp = interp_draft(&mut app.session)?;
    app.dialog = Some(Dialog::KeyInterpolation);
    Ok(())
}

/// Keyframe Interpolation's draft for the selected keyframes of `s` (a host's dialog too, EMBED12).
pub fn interp_draft(s: &mut effectcraft_engine::Session) -> Result<InterpDraft, String> {
    let info = s.execute("keys.info", json!({})).map_err(|e| e.to_string())?;
    let a = info.as_array().cloned().unwrap_or_default();
    if a.is_empty() {
        return Err("select keyframes first".into());
    }
    Ok(InterpDraft { has_spatial: a.iter().any(|k| !k["spatial"].is_null()), count: a.len(), ..Default::default() })
}

/// Layer ▸ Time ▸ Time Stretch…
pub fn open_time_stretch(app: &mut EffectcraftApp) -> Result<(), String> {
    app.dialog_state.stretch = stretch_draft(&app.session)?;
    app.dialog = Some(Dialog::TimeStretch);
    Ok(())
}

/// Time Stretch's draft for the first selected layer of `s`'s open comp (a host's dialog too,
/// EMBED12).
pub fn stretch_draft(s: &effectcraft_engine::Session) -> Result<StretchDraft, String> {
    let lid = *s.state.selected_layers.first().ok_or("select a layer first")?;
    let l = s.active_comp().and_then(|c| c.layer(lid)).ok_or("no layer")?;
    let dur = (l.out_point - l.in_point).seconds();
    Ok(StretchDraft { percent: l.stretch, duration: dur, hold: 0, orig_duration: dur, orig_percent: l.stretch })
}

fn buttons(ui: &mut egui::Ui, auto: &mut Registry, t: &Tokens, prefix: &str) -> (bool, bool) {
    let (mut ok, mut cancel) = (false, false);
    ui.add_space(14.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let r = ui.add(egui::Button::new(egui::RichText::new(tr("   OK   ")).color(Color32::WHITE)).fill(t.accent));
        auto.add(&format!("{prefix}.ok"), r.rect, "OK");
        ok = r.clicked();
        let r = ui.button(tr("Cancel"));
        auto.add(&format!("{prefix}.cancel"), r.rect, "Cancel");
        cancel = r.clicked();
    });
    (ok, cancel)
}

fn drag(ui: &mut egui::Ui, auto: &mut Registry, id: &str, v: &mut f64, speed: f64, range: std::ops::RangeInclusive<f64>, suffix: &str, enabled: bool) {
    let r = ui.add_enabled(enabled, crate::widgets::drag_value(egui::DragValue::new(v).speed(speed).range(range).max_decimals(2).suffix(suffix)));
    auto.add(id, r.rect, id);
}

pub fn velocity(app: &mut EffectcraftApp, ctx: &egui::Context, t: &Tokens) {
    let mut d = app.dialog_state.velocity.clone();
    let mut a = super::precomp::Answer::default();
    let auto = &mut app.auto;
    super::dialogs::modal(ctx, &velocity_title(&d), velocity_size(&d), t, |ui| a = draw_velocity(ui, &mut d, t, auto));
    let mut cancel = a.cancel;
    app.dialog_state.velocity = d.clone();
    if a.ok || super::dialogs::ui_enter(ctx) {
        if let Err(e) = crate::menus::run_engine(app, ctx, "keys.velocity", velocity_params(&d)) {
            app.ui.status = e.to_string();
        }
        cancel = true;
    }
    if cancel {
        app.dialog = None;
    }
}

/// Keyframe Velocity's title (with the property's name).
pub fn velocity_title(d: &VelocityDraft) -> String {
    if d.name.is_empty() { "Keyframe Velocity".to_string() } else { format!("Keyframe Velocity: {}", d.name) }
}

/// Keyframe Velocity's window size (a row per dimension).
pub fn velocity_size(d: &VelocityDraft) -> egui::Vec2 {
    vec2(440.0, 250.0 + 2.0 * 26.0 * d.dims.min(4) as f32)
}

/// Keyframe Velocity's contents (incoming / outgoing speed and influence per dimension,
/// Continuous, OK / Cancel) in any `ui`: EffectCraft's modal or a host's window (EMBED12).
pub fn draw_velocity(ui: &mut egui::Ui, d: &mut VelocityDraft, t: &Tokens, auto: &mut Registry) -> super::precomp::Answer {
    // A draft from elsewhere may be short of dimensions: never index past its lists.
    let dims = d.dims.min(d.in_speed.len()).min(d.out_speed.len()).min(d.in_inf.len()).min(d.out_inf.len());
    for (label, out) in [("Incoming Velocity", false), ("Outgoing Velocity", true)] {
        ui.label(egui::RichText::new(label).font(Tokens::medium(12.5)));
        let enabled = if out { d.has_out } else { d.has_in };
        egui::Grid::new(("vel", out)).num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for dim in 0..dims {
                let tag = if d.dims > 1 { format!("Dimension {}", ["X", "Y", "Z", "W"][dim.min(3)]) } else { String::new() };
                ui.label(egui::RichText::new(tag).color(t.text_dim));
                let side = if out { "out" } else { "in" };
                ui.horizontal(|ui| {
                    ui.label(tr("Speed:"));
                    let v = if out { &mut d.out_speed[dim] } else { &mut d.in_speed[dim] };
                    drag(ui, auto, &format!("dialog.velocity.{side}Speed.{dim}"), v, 1.0, -1e7..=1e7, "", enabled);
                    ui.label(egui::RichText::new(&d.units).color(t.text_dim));
                });
                ui.horizontal(|ui| {
                    ui.label(tr("Influence:"));
                    let v = if out { &mut d.out_inf[dim] } else { &mut d.in_inf[dim] };
                    drag(ui, auto, &format!("dialog.velocity.{side}Influence.{dim}"), v, 0.2, 0.1..=100.0, " %", enabled);
                });
                ui.end_row();
            }
        });
        ui.add_space(8.0);
    }
    let r = ui.checkbox(&mut d.continuous, tr("Continuous (Lock Outgoing to Incoming)"));
    auto.add("dialog.velocity.continuous", r.rect, "Continuous");
    if d.continuous {
        d.out_speed = d.in_speed.clone();
    }
    let (ok, cancel) = buttons(ui, auto, t, "dialog.velocity");
    super::precomp::Answer { ok, cancel }
}

/// The parameters of OK's `keys.velocity` (on the selected keyframes).
pub fn velocity_params(d: &VelocityDraft) -> Value {
    let mut params = json!({"continuous": d.continuous});
    if d.has_in {
        params["inSpeed"] = json!(d.in_speed);
        params["inInfluence"] = json!(d.in_inf);
    }
    if d.has_out {
        params["outSpeed"] = json!(d.out_speed);
        params["outInfluence"] = json!(d.out_inf);
    }
    params
}

fn combo(ui: &mut egui::Ui, auto: &mut Registry, id: &str, sel: &mut usize, opts: &[&str], enabled: bool) {
    ui.add_enabled_ui(enabled, |ui| {
        // The options are the English source strings; the automation id and label stay English.
        let r = egui::ComboBox::from_id_salt(id).width(200.0).selected_text(tr(opts[*sel])).show_ui(ui, |ui| {
            for (i, o) in opts.iter().enumerate() {
                ui.selectable_value(sel, i, tr(o));
            }
        });
        auto.add(id, r.response.rect, id);
    });
}

pub fn interpolation(app: &mut EffectcraftApp, ctx: &egui::Context, t: &Tokens) {
    let mut d = app.dialog_state.interp.clone();
    let mut a = super::precomp::Answer::default();
    let auto = &mut app.auto;
    super::dialogs::modal(ctx, "Keyframe Interpolation", INTERP_SIZE, t, |ui| a = draw_interpolation(ui, &mut d, t, auto));
    let mut cancel = a.cancel;
    app.dialog_state.interp = d.clone();
    if a.ok || super::dialogs::ui_enter(ctx) {
        if let Some(params) = interp_params(&d)
            && let Err(e) = crate::menus::run_engine(app, ctx, "keys.interpolation", params)
        {
            app.ui.status = e.to_string();
        }
        cancel = true;
    }
    if cancel {
        app.dialog = None;
    }
}

/// Keyframe Interpolation's window size.
pub const INTERP_SIZE: egui::Vec2 = vec2(440.0, 250.0);

/// Keyframe Interpolation's contents (temporal, spatial, roving, OK / Cancel) in any `ui`:
/// EffectCraft's modal or a host's window (EMBED12).
pub fn draw_interpolation(ui: &mut egui::Ui, d: &mut InterpDraft, t: &Tokens, auto: &mut Registry) -> super::precomp::Answer {
    d.temporal = d.temporal.min(TEMPORAL.len() - 1);
    d.spatial = d.spatial.min(SPATIAL.len() - 1);
    d.roving = d.roving.min(ROVING.len() - 1);
    egui::Grid::new("interp-grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
        ui.label(tr("Temporal Interpolation:"));
        combo(ui, auto, "dialog.interpolation.temporal", &mut d.temporal, &TEMPORAL.map(|x| x.0), true);
        ui.end_row();
        ui.label(tr("Spatial Interpolation:"));
        combo(ui, auto, "dialog.interpolation.spatial", &mut d.spatial, &SPATIAL.map(|x| x.0), d.has_spatial);
        ui.end_row();
        ui.label(tr("Roving:"));
        combo(ui, auto, "dialog.interpolation.roving", &mut d.roving, &ROVING, d.has_spatial);
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.label(egui::RichText::new(format!("{} keyframe{} selected", d.count, if d.count == 1 { "" } else { "s" })).color(t.text_dim));
    let (ok, cancel) = buttons(ui, auto, t, "dialog.interpolation");
    super::precomp::Answer { ok, cancel }
}

/// The parameters of OK's `keys.interpolation` (`None`: everything left at Current Settings,
/// nothing to change).
pub fn interp_params(d: &InterpDraft) -> Option<Value> {
    let mut params = json!({});
    if let Some((_, v)) = TEMPORAL.get(d.temporal).filter(|_| d.temporal > 0) {
        params["interpolation"] = json!(v);
    }
    if let Some((_, v)) = SPATIAL.get(d.spatial).filter(|_| d.spatial > 0 && d.has_spatial) {
        params["spatial"] = json!(v);
    }
    if d.roving > 0 && d.has_spatial {
        params["roving"] = json!(d.roving == 2);
    }
    params.as_object().is_some_and(|m| !m.is_empty()).then_some(params)
}

pub fn time_stretch(app: &mut EffectcraftApp, ctx: &egui::Context, t: &Tokens) {
    let mut d = app.dialog_state.stretch.clone();
    let mut a = super::precomp::Answer::default();
    let auto = &mut app.auto;
    super::dialogs::modal(ctx, "Time Stretch", STRETCH_SIZE, t, |ui| a = draw_time_stretch(ui, &mut d, t, auto));
    let mut cancel = a.cancel;
    app.dialog_state.stretch = d.clone();
    if a.ok || super::dialogs::ui_enter(ctx) {
        if let Err(e) = app.session.execute("layer.timeStretch", stretch_params(&d)) {
            app.ui.status = e.to_string();
        }
        cancel = true;
    }
    if cancel {
        app.dialog = None;
    }
}

/// Time Stretch's window size.
pub const STRETCH_SIZE: egui::Vec2 = vec2(420.0, 300.0);

/// Time Stretch's contents (Stretch Factor, New Duration, Hold In Place, OK / Cancel) in any
/// `ui`: EffectCraft's modal or a host's window (EMBED12). Enter is the caller's.
pub fn draw_time_stretch(ui: &mut egui::Ui, d: &mut StretchDraft, t: &Tokens, auto: &mut Registry) -> super::precomp::Answer {
    ui.label(egui::RichText::new(tr("Stretch")).font(Tokens::medium(12.5)));
    egui::Grid::new("stretch-grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
        ui.label(tr("Original Duration:"));
        ui.label(format!("{:.2} s", d.orig_duration));
        ui.end_row();
        ui.label(tr("Stretch Factor:"));
        let before = d.percent;
        drag(ui, auto, "dialog.timeStretch.percent", &mut d.percent, 0.5, -10000.0..=10000.0, " %", true);
        if (before - d.percent).abs() > 1e-9 && d.orig_percent.abs() > 1e-9 {
            d.duration = d.orig_duration * d.percent.abs() / d.orig_percent.abs();
        }
        ui.end_row();
        ui.label(tr("New Duration:"));
        let before = d.duration;
        drag(ui, auto, "dialog.timeStretch.duration", &mut d.duration, 0.05, 0.01..=100000.0, " s", true);
        if (before - d.duration).abs() > 1e-9 && d.orig_duration > 1e-9 {
            d.percent = d.orig_percent.signum() * d.orig_percent.abs() * d.duration / d.orig_duration;
        }
        ui.end_row();
    });
    ui.add_space(10.0);
    ui.label(egui::RichText::new(tr("Hold In Place")).font(Tokens::medium(12.5)));
    for (i, l) in ["Layer In-point", "Current Frame", "Layer Out-point"].into_iter().enumerate() {
        let r = ui.radio_value(&mut d.hold, i, l);
        auto.add(&format!("dialog.timeStretch.hold.{i}"), r.rect, l);
    }
    let (ok, cancel) = buttons(ui, auto, t, "dialog.timeStretch");
    super::precomp::Answer { ok, cancel }
}

/// The parameters of OK's `layer.timeStretch` (on the selected layers).
pub fn stretch_params(d: &StretchDraft) -> Value {
    let hold = ["in", "current", "out"][d.hold.min(2)];
    json!({"percent": d.percent, "hold": hold})
}
