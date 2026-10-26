//! EMBED12: the layer dialogs EffectCraft's menus open, for a host that shows them itself
//! (FigCraft's EffectCraft cards): Solid Settings (Layer ▸ New ▸ Solid…, Layer Settings on a solid
//! or adjustment layer), Camera Settings and Light Settings (New ▸ Camera… / Light…, Layer
//! Settings on a camera or light), Null Settings (Layer Settings on a null), Time Stretch… and the
//! Layer Style dialog (Layer Style Options…, Layer Styles ▸ <style>).
//!
//! A host asks [`open`] when one of EffectCraft's commands runs without its settings, as
//! EffectCraft's own menus route them (`menus::invoke`): it gets the dialog's starting state, read
//! from its session alone. Each frame it draws the dialog with [`draw`] in a window of its own
//! ([`title`], [`size`]), runs the live edits [`Step::actions`] holds (the Layer Style dialog's)
//! through its command dispatch, and on OK runs [`command`] the same way (one undo step); then
//! [`finish`] (the Layer Style dialog's Cancel puts the layer's styles back). The dialog code is
//! EffectCraft's own (`dialogs::draw_solid`, `dialogs_3d::draw_camera` / `draw_light`,
//! `key_dialogs::draw_time_stretch`, `layer_styles_dialog::draw`, `forms::draw_form`), so
//! EffectCraft's app and a host show the same dialog and run the same command.

use effectcraft_engine::Session;
use effectcraft_engine::project::{LayerId, LayerSource};
use serde_json::Value;

use super::dialogs::SolidDraft;
use super::dialogs_3d::{CameraDraft, LightDraft};
use super::forms::{Field, Form};
use super::key_dialogs::StretchDraft;
use super::layer_styles_dialog::LayerStyleState;
use crate::automation::Registry;
use crate::theme::Tokens;

/// One of the hosted layer dialogs and its state.
#[derive(Clone, Debug)]
pub enum Hosted {
    /// Solid Settings: a new solid, or a layer's solid.
    Solid(SolidDraft),
    /// Camera Settings: a new camera, or a camera layer's settings.
    Camera(CameraDraft),
    /// Light Settings: a new light, or a light layer's settings.
    Light(LightDraft),
    /// A parameter form (Null Settings: the null's name).
    Form(Form),
    /// Time Stretch on the selected layers.
    TimeStretch(StretchDraft),
    /// The Layer Style dialog: its edits are live, Cancel takes them back.
    LayerStyle(LayerStyleState),
}

/// What one frame of a dialog decided.
#[derive(Debug, Default)]
pub struct Step {
    pub ok: bool,
    pub cancel: bool,
    /// Edits to run now, in order (the Layer Style dialog's controls), through the host's
    /// command dispatch.
    pub actions: Vec<(String, Value)>,
}

fn bare(p: &Value) -> bool {
    p.as_object().is_none_or(|m| m.is_empty())
}

fn only_layer(p: &Value) -> bool {
    p.as_object().is_none_or(|m| m.keys().all(|k| k == "layer"))
}

/// The dialog EffectCraft's command `id` with `params` opens in EffectCraft's app, for `s`'s open
/// comp: `None` when it opens none there (its settings are given, or the layer has no such
/// dialog: it runs as it is); `Some(Err)` says why it can't open (nothing selected). For the
/// Layer Styles ▸ <style> choices the style is added to `s` first, as in EffectCraft (the
/// dialog's Cancel removes it again). `last_solid_color`: the colour of the last solid made
/// (New Solid starts from it; `[0; 3]`: EffectCraft's blue).
pub fn open(s: &mut Session, id: &str, params: &Value, last_solid_color: [f32; 3]) -> Option<Result<Hosted, String>> {
    let layer = || -> Option<(u64, LayerSource)> {
        let c = s.active_comp()?;
        let l = params.get("layer").and_then(Value::as_u64).or_else(|| s.state.selected_layers.first().map(|l| l.0))?;
        Some((l, c.layer(LayerId(l))?.source.clone()))
    };
    match id {
        "app.solidSettings" | "layer.newSolid" if bare(params) => Some(super::dialogs::new_solid_draft(s, last_solid_color).map(Hosted::Solid)),
        "layer.newCamera" if bare(params) => Some(super::dialogs_3d::camera_draft(s, None).map(Hosted::Camera)),
        "layer.newLight" if bare(params) => Some(super::dialogs_3d::light_draft(s, None).map(Hosted::Light)),
        "layer.settings" | "layer.cameraSettings" | "layer.lightSettings" if only_layer(params) => {
            let (l, source) = layer()?;
            match source {
                LayerSource::Camera => Some(super::dialogs_3d::camera_draft(s, Some(l)).map(Hosted::Camera)),
                LayerSource::Light { .. } => Some(super::dialogs_3d::light_draft(s, Some(l)).map(Hosted::Light)),
                LayerSource::Solid { item } if id == "layer.settings" => Some(super::dialogs::layer_solid_draft(s, l, item).map(Hosted::Solid)),
                LayerSource::Null if id == "layer.settings" => {
                    let name = s.active_comp().and_then(|c| c.layer(LayerId(l))).map(|x| x.name.clone()).unwrap_or_default();
                    Some(Ok(Hosted::Form(Form {
                        title: "Null Settings".into(),
                        command: "layer.settings".into(),
                        base: serde_json::json!({"layer": l}),
                        fields: vec![Field::text("name", "Name", &name)],
                    })))
                }
                _ => None,
            }
        }
        "layer.timeStretch" if bare(params) => Some(super::key_dialogs::stretch_draft(s).map(Hosted::TimeStretch)),
        "layer.style.options" => Some(super::layer_styles_dialog::open_in(s, params).map(Hosted::LayerStyle)),
        _ => super::layer_styles_dialog::route_in(s, id, params).map(|r| r.map(Hosted::LayerStyle)),
    }
}

/// The dialog's title (`None`: what it edits is gone, close it).
pub fn title(s: &Session, h: &Hosted) -> Option<String> {
    Some(match h {
        Hosted::Solid(_) => "Solid Settings".into(),
        Hosted::Camera(_) => "Camera Settings".into(),
        Hosted::Light(_) => "Light Settings".into(),
        Hosted::Form(f) => f.title.clone(),
        Hosted::TimeStretch(_) => "Time Stretch".into(),
        Hosted::LayerStyle(st) => return super::layer_styles_dialog::title(s, st),
    })
}

/// The size EffectCraft gives the dialog's window.
pub fn size(h: &Hosted) -> egui::Vec2 {
    match h {
        Hosted::Solid(d) => super::dialogs::solid_size(d),
        Hosted::Camera(_) => super::dialogs_3d::CAMERA_SIZE,
        Hosted::Light(_) => super::dialogs_3d::LIGHT_SIZE,
        Hosted::Form(_) => egui::vec2(480.0, 200.0),
        Hosted::TimeStretch(_) => super::key_dialogs::STRETCH_SIZE,
        Hosted::LayerStyle(_) => super::layer_styles_dialog::SIZE,
    }
}

/// One frame of the dialog's contents in `ui` (the host's window, under its title), for `s`'s
/// open comp. `height`: the room the body may take (the form and the Layer Style dialog scroll in
/// it). Enter is the caller's (the Layer Style dialog has no default button).
pub fn draw(ui: &mut egui::Ui, h: &mut Hosted, s: &Session, t: &Tokens, height: f32, auto: &mut Registry) -> Step {
    let answer = |a: super::precomp::Answer| Step { ok: a.ok, cancel: a.cancel, actions: Vec::new() };
    match h {
        Hosted::Solid(d) => {
            let comp = s.active_comp().map(|c| (c.width, c.height)).unwrap_or((1920, 1080));
            answer(super::dialogs::draw_solid(ui, d, comp, t, auto))
        }
        Hosted::Camera(d) => {
            let w = s.active_comp().map(|c| f64::from(c.width)).unwrap_or(1920.0);
            answer(super::dialogs_3d::draw_camera(ui, d, w, t, auto))
        }
        Hosted::Light(d) => answer(super::dialogs_3d::draw_light(ui, d, t, auto)),
        Hosted::Form(f) => {
            let (mut regs, mut browse) = (Vec::new(), None);
            let (ok, cancel) = super::forms::draw_form(ui, f, t, height, &mut regs, &mut browse);
            Step { ok, cancel, actions: Vec::new() }
        }
        Hosted::TimeStretch(d) => answer(super::key_dialogs::draw_time_stretch(ui, d, t, auto)),
        Hosted::LayerStyle(st) => {
            let (close, actions) = super::layer_styles_dialog::draw(ui, height, s, st, auto);
            Step { ok: close == Some(true), cancel: close == Some(false), actions }
        }
    }
}

/// Whether Enter means OK for the dialog (every one but the Layer Style dialog, as in EffectCraft).
pub fn enter_is_ok(h: &Hosted) -> bool {
    !matches!(h, Hosted::LayerStyle(_))
}

/// The command OK runs and its parameters (`None`: the dialog's edits are already made, the
/// Layer Style dialog's).
pub fn command(h: &Hosted) -> Option<(String, Value)> {
    match h {
        Hosted::Solid(d) => Some((d.command().into(), d.params())),
        Hosted::Camera(d) => {
            let (id, p) = super::dialogs_3d::camera_command(d);
            Some((id.into(), p))
        }
        Hosted::Light(d) => {
            let (id, p) = super::dialogs_3d::light_command(d);
            Some((id.into(), p))
        }
        Hosted::Form(f) => Some((f.command.clone(), f.params())),
        Hosted::TimeStretch(d) => Some(("layer.timeStretch".into(), super::key_dialogs::stretch_params(d))),
        Hosted::LayerStyle(_) => None,
    }
}

/// The dialog closed (`ok`: with OK, after [`command`] ran). The Layer Style dialog's Cancel puts
/// the layer's styles back on `s` as they were when it opened; OK ends its undo merging.
pub fn finish(s: &mut Session, h: &mut Hosted, ok: bool) -> Result<(), String> {
    match h {
        Hosted::LayerStyle(st) => super::layer_styles_dialog::finish_in(s, st, ok),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "C", "width": 640, "height": 360, "frameRate": 25, "duration": 2})).unwrap();
        s
    }

    #[test]
    fn a_host_gets_each_layer_dialog_from_its_session() {
        let mut s = session();
        // New Solid: the comp's size; OK makes it.
        let Some(Ok(Hosted::Solid(mut d))) = open(&mut s, "layer.newSolid", &json!({}), [0.0; 3]) else { panic!("solid") };
        assert_eq!((d.size, d.layer), ([640, 360], None));
        d.size = [100, 50];
        let (id, p) = command(&Hosted::Solid(d)).unwrap();
        let l = s.execute(&id, p).unwrap()["layer"].as_u64().unwrap();
        // Layer Settings on it: Solid Settings on its solid.
        s.state.selected_layers = vec![LayerId(l)];
        assert!(matches!(open(&mut s, "layer.settings", &json!({}), [0.0; 3]), Some(Ok(Hosted::Solid(d))) if d.layer == Some(l) && d.size == [100, 50]));
        // With settings given: no dialog.
        assert!(open(&mut s, "layer.newSolid", &json!({"color": "#ff0000"}), [0.0; 3]).is_none());
        assert!(matches!(open(&mut s, "layer.newCamera", &json!({}), [0.0; 3]), Some(Ok(Hosted::Camera(d))) if d.layer.is_none()));
        assert!(matches!(open(&mut s, "layer.newLight", &json!({}), [0.0; 3]), Some(Ok(Hosted::Light(d))) if d.layer.is_none()));
        assert!(matches!(open(&mut s, "layer.timeStretch", &json!({}), [0.0; 3]), Some(Ok(Hosted::TimeStretch(_)))));
        assert!(matches!(open(&mut s, "layer.style.options", &json!({}), [0.0; 3]), Some(Ok(Hosted::LayerStyle(_)))));
        // A null: its name.
        let n = s.execute("layer.newNull", json!({})).unwrap()["layer"].as_u64().unwrap();
        s.state.selected_layers = vec![LayerId(n)];
        assert!(matches!(open(&mut s, "layer.settings", &json!({}), [0.0; 3]), Some(Ok(Hosted::Form(f))) if f.title == "Null Settings"));
        // Nothing selected: why not.
        s.state.selected_layers.clear();
        assert!(matches!(open(&mut s, "layer.timeStretch", &json!({}), [0.0; 3]), Some(Err(_))));
        assert!(open(&mut s, "layer.settings", &json!({}), [0.0; 3]).is_none());
    }
}
