//! EMBED11: the Composition viewer's context menu, for EffectCraft's viewer and for a host that
//! shows the comp somewhere else (FigCraft's EffectCraft cards, where `viewer_hosted` runs the
//! viewer's tools).
//!
//! A right-click on the comp picks the layer under the pointer as a left-click would (a layer
//! already in the selection keeps the group) and opens the menu: Select ▸ (every layer under the
//! pointer, front to back), the layer actions the Timeline's layer menu has
//! ([`super::timeline::layer_menu`]: New ▸, Layer Settings…, Masks ▸, Transform ▸, Time ▸,
//! Markers ▸, Blending Mode ▸, Layer Styles ▸, Effects ▸, Arrange ▸, Pre-compose…, Duplicate,
//! Split, Delete) for the unlocked selected layers, then Copy Frame to Clipboard and Composition
//! Settings… for the comp. With nothing selected the layer part says so.
//!
//! The menu only pushes `(command, params)` pairs; the caller runs them (EffectCraft's
//! `menus::invoke`, a host's `TimelineHost::invoke`), so every edit is the same command and the
//! same undo step wherever the menu is. A host passes a `gate` naming the entries it can't run
//! yet (dialogs only EffectCraft's app has): they show disabled, the reason on hover.
//!
//! - [`press`]: the right-click (pick, the layers under the pointer).
//! - [`contents`]: the menu's entries, inside the host's popup.

use effectcraft_engine::project::{Comp, ItemId, LayerId, Project};
use effectcraft_engine::render::EvalCtx;
use serde_json::{Value, json};

use super::timeline::{LayerMenuCaps, TimelineHost, layer_menu_gated};
use crate::automation::Registry;

/// What a right-click opened the menu on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    /// The comp (project item id).
    pub comp: u64,
    /// The selectable layers under the pointer, front to back: (id, name), for Select ▸.
    pub hits: Vec<(u64, String)>,
}

/// The menu's labels a host may translate.
#[derive(Clone, Debug)]
pub struct Labels {
    pub copy_frame: String,
}

impl Default for Labels {
    fn default() -> Self {
        Labels { copy_frame: crate::i18n::tr("Copy Frame to Clipboard").to_string() }
    }
}

/// The selectable layers under comp point `cpt`, front to back (Select ▸).
pub fn hits(ectx: &EvalCtx, cpt: [f64; 2]) -> Vec<(u64, String)> {
    if !cpt[0].is_finite() || !cpt[1].is_finite() {
        return Vec::new();
    }
    super::viewer::layers_at(ectx, cpt).map(|l| (l.id.0, l.name.clone())).collect()
}

/// A right-click at comp point `cpt` of the host's active comp: the layer there is picked as the
/// viewer picks it (a selected one keeps the selection; an unselected one becomes it) and the
/// menu's target comes back. `None` without a comp.
pub fn press(host: &mut dyn TimelineHost, cpt: [f64; 2]) -> Option<Target> {
    let cid = host.session().active_comp_id()?;
    let comp = host.session().project.comp_arc(cid)?;
    let project = host.session().project.clone();
    let expr = host.session().expr.clone();
    let time = host.session().time();
    let ectx = EvalCtx { project: &project, comp_id: cid, comp: &comp, time, expr: expr.as_deref(), footage: None };
    if cpt[0].is_finite() && cpt[1].is_finite() {
        super::viewer::pick(host, &ectx, cpt, false);
    }
    Some(Target { comp: cid.0, hits: hits(&ectx, cpt) })
}

/// The unlocked layers of `comp` among `selected`: what the layer entries act on.
pub fn context_layers(comp: &Comp, selected: &[LayerId]) -> Vec<u64> {
    selected.iter().filter(|id| comp.layer(**id).is_some_and(|l| !l.switches.locked)).map(|id| id.0).collect()
}

/// The menu's entries (inside the caller's popup). `layers`: [`context_layers`]. Chosen entries
/// go to `actions` as `(command, params)`; `gate(command)` = `Some(why)` shows one disabled.
#[allow(clippy::too_many_arguments)]
pub fn contents(
    ui: &mut egui::Ui,
    project: &Project,
    comp: &Comp,
    target: &Target,
    layers: &[u64],
    labels: &Labels,
    auto: &mut Registry,
    gate: &dyn Fn(&str) -> Option<String>,
    actions: &mut Vec<(String, Value)>,
) {
    if !target.hits.is_empty() {
        ui.menu_button(crate::i18n::tr("Select"), |ui| {
            crate::widgets::menu_scroll(ui, |ui| {
                for (id, name) in &target.hits {
                    if ui.button(name).clicked() {
                        actions.push(("layer.select".to_owned(), json!({"layers":[id]})));
                        ui.close();
                    }
                }
            })
        });
        ui.separator();
    }
    if layers.is_empty() {
        ui.label(crate::i18n::tr("No unlocked layer selected"));
    } else {
        let caps = layers.iter().filter_map(|id| comp.layer(LayerId(*id))).fold(LayerMenuCaps::default(), |mut caps, layer| {
            let c = LayerMenuCaps::of(project, layer);
            caps.vector |= c.vector;
            caps.text |= c.text;
            caps
        });
        layer_menu_gated(ui, layers, caps, actions, gate);
    }
    ui.separator();
    let mut entry = |ui: &mut egui::Ui, label: &str, cmd: &str, auto_id: Option<(&str, &str)>| {
        if let Some(why) = gate(cmd) {
            ui.add_enabled(false, egui::Button::new(label)).on_disabled_hover_text(why);
            return;
        }
        let r = ui.button(label);
        if let Some((id, name)) = auto_id {
            auto.add(id, r.rect, name);
        }
        if r.clicked() {
            actions.push((cmd.to_owned(), json!({"comp": target.comp})));
            ui.close();
        }
    };
    entry(ui, &labels.copy_frame, "comp.copyFrame", None);
    // The comp this viewer shows, which is not always the active one.
    entry(ui, crate::i18n::tr("Composition Settings…"), "comp.settings", Some(("viewer.context.settings", "Composition Settings…")));
}

/// The comp `target` names, if it is still in `project`.
pub fn comp_of<'a>(project: &'a Project, target: &Target) -> Option<&'a Comp> {
    project.comp(ItemId(target.comp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's menu over a layer: Select ▸ names it, its layer entries and the comp's show, a
    /// gated entry can't be chosen; junk points never panic.
    #[test]
    fn the_menu_draws_for_a_host_and_gates_entries() {
        let mut app = crate::EffectcraftApp::new(effectcraft_engine::Session::default());
        app.session.execute("comp.new", json!({"name": "C", "width": 64, "height": 36, "duration": 2, "frameRate": 25})).unwrap();
        let l =
            app.session.execute("layer.newSolid", json!({"name": "Box", "color": "#00ff00", "width": 64, "height": 36})).unwrap()["layer"].as_u64().unwrap();
        app.session.execute("edit.deselectAll", json!({})).unwrap();
        let t = press(&mut app, [32.0, 18.0]).unwrap();
        assert_eq!(t.hits, vec![(l, "Box".to_string())]);
        assert_eq!(app.session.state.selected_layers, vec![LayerId(l)], "the right-clicked layer is picked");
        for junk in [[f64::NAN, 1.0], [f64::INFINITY, f64::NEG_INFINITY], [-1e300, 1e300]] {
            let j = press(&mut app, junk).unwrap();
            assert!(j.hits.is_empty());
        }
        let comp = app.session.active_comp().unwrap();
        let layers = context_layers(comp, &app.session.state.selected_layers);
        assert_eq!(layers, vec![l]);
        let ctx = egui::Context::default();
        let mut actions = Vec::new();
        let mut auto = Registry::default();
        let project = app.session.project.clone();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            contents(ui, &project, comp, &t, &layers, &Labels::default(), &mut auto, &|c| (c == "comp.settings").then(|| "not here".to_string()), &mut actions);
        });
        // The frame's font atlas upload must be handled (epaint panics on a dropped delta).
        out.textures_delta.clear();
        assert!(actions.is_empty(), "nothing chosen without a click");
    }
}
