//! Layer ▸ Layer Styles ▸ Layer Style Options… (and choosing a style from that menu): a Layer
//! Style dialog with the style list on the left (a check box per style, Blending Options first)
//! and the selected style's controls on the right. Every control edits the ordinary
//! `layerStyles/<style>/<property>` property, so the viewer previews live; Cancel rolls the
//! layer's styles back to how they were when the dialog opened (edits other drivers made
//! meanwhile are kept), OK keeps them (one undo step per control drag).

use crate::i18n::tr;
use effectcraft_engine::Session;
use effectcraft_engine::keyframe::Value as KV;
use effectcraft_engine::project::styles::{BLENDING, GROUP, STYLES};
use effectcraft_engine::project::{ItemId, Layer, LayerId, Node, ParamUi, Project, Property};
use egui::vec2;
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{Dialog, EffectcraftApp};

/// Dialog state: layer, selected list entry and the undo depth when it opened.
#[derive(Clone, Debug, Default)]
pub struct LayerStyleState {
    pub layer: u64,
    pub selected: String,
    pub undo_mark: usize,
    checkpoint: Option<Checkpoint>,
}

#[derive(Clone)]
struct Checkpoint {
    project: std::sync::Arc<effectcraft_engine::project::Project>,
    history: effectcraft_engine::history::History,
    /// The layer whose styles the dialog edits, in its composition.
    comp: Option<ItemId>,
    layer: Option<LayerId>,
}

impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoint").field("undo_steps", &self.history.undo.len()).finish_non_exhaustive()
    }
}

fn checkpoint(s: &mut Session, layer: Option<LayerId>) -> Checkpoint {
    s.history.merge_key = None;
    Checkpoint { project: s.project.clone(), history: s.history.clone(), comp: s.active_comp_id(), layer }
}

/// The Layer Styles group of `layer` and its place among the layer's top-level groups.
fn styles_of(p: &Project, comp: ItemId, layer: LayerId) -> Option<(usize, Node)> {
    let l = p.comp(comp)?.layer(layer)?;
    let i = l.props.children.iter().position(|c| c.match_id() == GROUP)?;
    Some((i, l.props.children.get(i)?.clone()))
}

/// Give `layer` the Layer Styles group `saved` (none: remove it).
fn put_styles(p: &mut Project, comp: ItemId, layer: LayerId, saved: Option<&(usize, Node)>) {
    let Some(l) = p.comp_mut(comp).and_then(|c| c.layer_mut(layer)) else { return };
    let cur = l.props.children.iter().position(|c| c.match_id() == GROUP);
    match (cur, saved) {
        (Some(i), Some((_, g))) => {
            if let Some(slot) = l.props.children.get_mut(i) {
                *slot = g.clone();
            }
        }
        (Some(i), None) => {
            l.props.children.remove(i);
        }
        (None, Some((at, g))) => {
            let at = (*at).min(l.props.children.len());
            l.props.children.insert(at, g.clone());
        }
        (None, None) => {}
    }
}

/// Open the dialog on the first selected (or given) layer, showing `style` (a style id;
/// Blending Options when none).
pub fn open(app: &mut EffectcraftApp, params: &Value) -> Result<(), String> {
    app.dialog_state.layer_style = open_in(&mut app.session, params)?;
    app.dialog = Some(Dialog::LayerStyles);
    Ok(())
}

/// The dialog's state for the first selected (or given) layer of `s`'s open comp, showing
/// `style`, with the checkpoint Cancel goes back to (a host's dialog too, EMBED12).
pub fn open_in(s: &mut Session, params: &Value) -> Result<LayerStyleState, String> {
    let comp = s.active_comp().ok_or("no composition is open")?;
    let layer = params
        .get("layer")
        .and_then(Value::as_u64)
        .or_else(|| s.state.selected_layers.first().map(|l| l.0))
        .filter(|l| comp.layer(LayerId(*l)).is_some())
        .ok_or("select a layer first")?;
    let style = params.get("style").and_then(Value::as_str).unwrap_or(BLENDING).to_string();
    if style != BLENDING && !STYLES.iter().any(|s| s.0 == style) {
        return Err(format!("unknown layer style `{style}`"));
    }
    let checkpoint = checkpoint(s, Some(LayerId(layer)));
    Ok(LayerStyleState { layer, selected: style, undo_mark: s.history.undo.len(), checkpoint: Some(checkpoint) })
}

/// Menu routing: choosing one of the Layer ▸ Layer Styles ▸ <style> items (no parameters) adds
/// the style as usual and opens the dialog on it.
pub fn route(app: &mut EffectcraftApp, id: &str, params: &Value) -> Result<bool, String> {
    let Some(r) = route_in(&mut app.session, id, params) else { return Ok(false) };
    app.dialog_state.layer_style = r?;
    app.dialog = Some(Dialog::LayerStyles);
    Ok(true)
}

/// [`route`] for `s` (a host's dialog too, EMBED12): `None` when `id` / `params` isn't a Layer
/// Styles ▸ <style> menu choice; else the style is added and the dialog's state returned, whose
/// Cancel also removes it.
pub fn route_in(s: &mut Session, id: &str, params: &Value) -> Option<Result<LayerStyleState, String>> {
    let style = id.strip_prefix("layer.style.").filter(|s| STYLES.iter().any(|x| x.0 == *s))?;
    if !params.as_object().is_none_or(|m| m.is_empty()) {
        return None;
    }
    let mark = s.history.undo.len();
    let target = s.state.selected_layers.first().copied();
    let checkpoint = checkpoint(s, target);
    if let Err(e) = s.execute(id, json!({})) {
        return Some(Err(e.to_string()));
    }
    Some(open_in(s, &json!({"style": style})).map(|mut st| {
        // Cancel also removes the style just added.
        st.undo_mark = mark;
        st.checkpoint = Some(checkpoint);
        st
    }))
}

fn layer(s: &Session, st: &LayerStyleState) -> Option<std::sync::Arc<Layer>> {
    super::layer_arc(s.active_comp()?, LayerId(st.layer))
}

/// The dialog's title ("Layer Style — <layer>"); `None` when its layer is gone (the dialog
/// closes).
pub fn title(s: &Session, st: &LayerStyleState) -> Option<String> {
    layer(s, st).map(|l| format!("Layer Style — {}", l.name))
}

/// The dialog's window size.
pub const SIZE: egui::Vec2 = vec2(720.0, 520.0);

/// One control row for a property. Returns the new value when it changed.
fn control(ui: &mut egui::Ui, p: &Property, v: &KV) -> Option<Value> {
    match (v, &p.ui) {
        (KV::Bool(b), _) => {
            let mut on = *b;
            ui.checkbox(&mut on, "").changed().then(|| json!(on))
        }
        (KV::Enum(i), ParamUi::Popup { options }) => {
            let mut sel = *i as usize;
            egui::ComboBox::from_id_salt(("ls-popup", p.uid)).selected_text(options.get(sel).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                for (k, o) in options.iter().enumerate() {
                    ui.selectable_value(&mut sel, k, o);
                }
            });
            (sel != *i as usize).then(|| json!(sel))
        }
        (KV::Scalar(x), ui_kind) => {
            let mut y = *x;
            let r = match ui_kind {
                ParamUi::Slider { slider_min, slider_max, decimals, .. } => {
                    ui.add(egui::Slider::new(&mut y, *slider_min..=*slider_max).clamping(egui::SliderClamping::Never).max_decimals(*decimals as usize))
                }
                ParamUi::Percent => ui.add(crate::widgets::drag_value(egui::DragValue::new(&mut y).speed(0.5).suffix("%"))),
                ParamUi::Angle => ui.add(crate::widgets::drag_value(egui::DragValue::new(&mut y).speed(0.5).suffix("°"))),
                ParamUi::Pixels => ui.add(crate::widgets::drag_value(egui::DragValue::new(&mut y).speed(0.25).suffix(" px"))),
                _ => ui.add(crate::widgets::drag_value(egui::DragValue::new(&mut y).speed(0.1))),
            };
            (r.changed() && y != *x).then(|| json!(y))
        }
        (KV::Color(c), _) => {
            let mut rgb = [c[0] as f32, c[1] as f32, c[2] as f32];
            let changed = crate::widgets::srgb_color_button(ui, &mut rgb).changed();
            changed.then(|| json!(rgb))
        }
        (KV::Gradient(_), _) => {
            ui.label(egui::RichText::new(tr("Edit in the Timeline")).weak());
            None
        }
        _ => {
            ui.label(egui::RichText::new(format!("{v:?}").chars().take(24).collect::<String>()).weak());
            None
        }
    }
}

pub fn show(app: &mut EffectcraftApp, ctx: &egui::Context, t: &Tokens) {
    let Some(title) = title(&app.session, &app.dialog_state.layer_style) else {
        app.dialog = None;
        return;
    };
    let mut actions: Vec<(String, Value)> = Vec::new();
    let mut close: Option<bool> = None;
    // Taken for the frame, not cloned: its checkpoint holds the project and history.
    let mut st = std::mem::take(&mut app.dialog_state.layer_style);
    let (session, auto) = (&app.session, &mut app.auto);
    super::dialogs::modal_fit(ctx, &title, SIZE, t, |ui, height| {
        (close, actions) = draw(ui, height, session, &mut st, auto);
    });
    app.dialog_state.layer_style = st;
    for (id, p) in actions {
        if let Err(e) = app.session.execute(&id, p) {
            app.ui.status = e.to_string();
        }
    }
    match close {
        Some(true) => finish(app, true),
        Some(false) => finish(app, false),
        None => {}
    }
}

/// The dialog's contents in a body of `height` (as `dialogs::modal_fit` gives): the style list,
/// the selected style's controls and OK / Cancel at the bottom, for `st`'s layer in `s`'s open
/// comp, in any `ui` (EffectCraft's modal or a host's window, EMBED12). Returns OK (`Some(true)`)
/// or Cancel (`Some(false)`) and the edits to run, in order (`layer.style.add` / `toggle`,
/// `prop.set` with a merge key per control, so a drag is one undo step), which the caller runs on
/// its session; then [`finish_in`] on OK or Cancel.
pub fn draw(
    ui: &mut egui::Ui,
    height: f32,
    s: &Session,
    st: &mut LayerStyleState,
    auto: &mut crate::automation::Registry,
) -> (Option<bool>, Vec<(String, Value)>) {
    let mut actions: Vec<(String, Value)> = Vec::new();
    let mut close: Option<bool> = None;
    let Some(l) = layer(s, st) else { return (Some(false), actions) };
    let lid = l.id.0;
    let styles = l.layer_styles().cloned();
    let sel = st.selected.clone();
    let lt = l.layer_time(s.time());
    {
        // OK and Cancel stay at the bottom, in view; the list and the style's controls scroll
        // above them (#1062).
        let (mut body, body_h) = super::dialogs::above_footer(ui, height, |foot| {
            foot.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = ui.button(tr("OK"));
                    auto.add("dialog.layerStyle.ok", ok.rect, "OK");
                    if ok.clicked() {
                        close = Some(true);
                    }
                    let cancel = ui.button(tr("Cancel"));
                    auto.add("dialog.layerStyle.cancel", cancel.rect, "Cancel");
                    if cancel.clicked() {
                        close = Some(false);
                    }
                });
            });
        });
        {
            let ui = &mut body;
            // Style list.
            ui.vertical(|ui| {
                ui.set_width(200.0);
                egui::ScrollArea::vertical().id_salt("ls-list").max_height(body_h).min_scrolled_height(0.0).show(ui, |ui| {
                    let mut entries = vec![(BLENDING, "Blending Options")];
                    entries.extend(STYLES.iter().copied());
                    for (id, name) in entries {
                        ui.horizontal(|ui| {
                            if id != BLENDING {
                                let g = styles.as_ref().and_then(|s| s.sub(id));
                                let mut on = g.is_some_and(|g| g.enabled);
                                if ui.checkbox(&mut on, "").changed() {
                                    if g.is_none() {
                                        actions.push(("layer.style.add".into(), json!({"layers": [lid], "style": id})));
                                    } else {
                                        actions.push(("layer.style.toggle".into(), json!({"layer": lid, "style": id, "value": on})));
                                    }
                                }
                            } else {
                                ui.add_space(24.0);
                            }
                            let r = ui.selectable_label(sel == id, name);
                            if r.clicked() {
                                st.selected = id.to_string();
                            }
                            let rect = r.rect;
                            auto.add(&format!("dialog.layerStyle.list.{id}"), rect, name);
                        });
                    }
                });
            });
            ui.separator();
            // Controls of the selected style.
            ui.vertical(|ui| {
                let name = if sel == BLENDING { "Blending Options" } else { STYLES.iter().find(|s| s.0 == sel).map(|s| s.1).unwrap_or("") };
                let head = ui.label(egui::RichText::new(name).font(Tokens::semibold(13.0)));
                auto.add("dialog.layerStyle.page", head.rect, &sel);
                ui.add_space(6.0);
                let Some(g) = styles.as_ref().and_then(|s| s.sub(&sel)) else {
                    ui.label(egui::RichText::new(tr("Not applied: tick it in the list to add it.")).weak());
                    return;
                };
                // Both ways, in the room left under the heading: in a narrow window (or at a high UI
                // Scale) the controls scroll sideways rather than being cut off.
                egui::ScrollArea::both()
                    .id_salt("ls-controls")
                    .max_width(ui.available_width().max(0.0))
                    .max_height(ui.available_height().max(0.0))
                    .min_scrolled_width(0.0)
                    .min_scrolled_height(0.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("ls-grid").num_columns(2).spacing(vec2(12.0, 6.0)).show(ui, |ui| {
                            g.walk("", &mut |_, p| {
                                if matches!(p.ui, ParamUi::Hidden) {
                                    return;
                                }
                                let lbl = ui.label(&p.name);
                                let v = p.value_at(lt);
                                let edit = ui.scope(|ui| control(ui, p, &v));
                                if let Some(nv) = edit.inner {
                                    actions.push((
                                        "prop.set".into(),
                                        json!({"layer": lid, "prop": p.uid, "value": nv, "merge": format!("layer-style-dialog-{}", p.uid)}),
                                    ));
                                }
                                let path = g.match_path_of(p.uid).unwrap_or_default();
                                auto.add(&format!("dialog.layerStyle.prop.{sel}/{path}"), lbl.rect, &p.name);
                                auto.add(&format!("dialog.layerStyle.value.{sel}/{path}"), edit.response.rect, &p.name);
                                ui.end_row();
                            });
                        });
                    });
            });
        }
    }
    (close, actions)
}

/// Show another entry of the list (`blendingOptions` or a style id).
pub fn select(app: &mut EffectcraftApp, style: &str) {
    app.dialog_state.layer_style.selected = style.to_string();
}

/// Close the dialog: OK keeps the edits, Cancel puts the layer's styles back as they were when
/// it opened. When nothing else changed meanwhile, the dialog's steps leave the history too;
/// when another driver (an agent, a script) edited the project meanwhile, only the layer's
/// styles are restored, as one undoable step, and the other edits stay.
pub fn finish(app: &mut EffectcraftApp, ok: bool) {
    if let Err(e) = finish_in(&mut app.session, &mut app.dialog_state.layer_style, ok) {
        app.ui.status = e;
    }
    app.dialog = None;
}

/// [`finish`] on `s` for a host's dialog (EMBED12): OK keeps the edits, Cancel puts the layer's
/// styles back as they were when `st` opened.
pub fn finish_in(s: &mut Session, st: &mut LayerStyleState, ok: bool) -> Result<(), String> {
    let mut r = Ok(());
    if let Some(checkpoint) = st.checkpoint.take()
        && !ok
    {
        r = cancel(s, checkpoint);
    }
    s.history.merge_key = None;
    r
}

fn cancel(s: &mut Session, checkpoint: Checkpoint) -> Result<(), String> {
    if std::sync::Arc::ptr_eq(&s.project, &checkpoint.project) {
        s.history = checkpoint.history;
        return Ok(());
    }
    let (Some(comp), Some(layer)) = (checkpoint.comp, checkpoint.layer) else { return Ok(()) };
    let saved = styles_of(&checkpoint.project, comp, layer);
    // Only the dialog's layer styles changed: back to the checkpoint, history included.
    let mut only_styles = (*s.project).clone();
    put_styles(&mut only_styles, comp, layer, saved.as_ref());
    only_styles.next_id = checkpoint.project.next_id;
    if only_styles.same_content(&checkpoint.project) {
        s.project = checkpoint.project;
        s.history = checkpoint.history;
        s.sanitize_state();
        s.bump();
        return Ok(());
    }
    s.edit("Cancel Layer Style", None, |p, _| {
        put_styles(p, comp, layer, saved.as_ref());
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}
