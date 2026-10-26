//! The panel menu, as in After Effects: the ≡ next to a panel's tab name, and the same menu on
//! a right-click on the tab (or the empty part of the group's tab strip). Generic items first
//! (Close Panel, Undock Panel, Close Other Panels in Group, Panel Group Settings ▸), then the
//! panel's own. Every item is a command (`window.closePanel {panel}` …) dispatched through
//! [`crate::menus::invoke`] and registers the automation id `panelMenu.<key>`.

use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::dock::PanelKind;
use crate::i18n::tr;

/// The open menu: its panel, the document tab it was opened on (a Timeline comp) and where:
/// `pos` in the window `viewport` (the main window, or an undocked panel group's OS window,
/// which opens it when it next draws while `pending`).
#[derive(Clone, Copy, Debug)]
struct Target {
    panel: PanelKind,
    doc: Option<u64>,
    pos: egui::Pos2,
    viewport: egui::ViewportId,
    pending: bool,
}

fn popup_id() -> egui::Id {
    egui::Id::new("panel-menu-popup")
}

fn target_id() -> egui::Id {
    egui::Id::new("panel-menu-target")
}

/// Open the panel menu of `panel` (its document tab `doc`) at `pos`.
pub fn open(ctx: &egui::Context, panel: PanelKind, doc: Option<u64>, pos: egui::Pos2) {
    open_in(ctx, ctx.viewport_id(), panel, doc, pos);
}

/// [`open`] in the window `viewport` (`pos` in its coordinates): the panel's undocked OS window.
pub fn open_in(ctx: &egui::Context, viewport: egui::ViewportId, panel: PanelKind, doc: Option<u64>, pos: egui::Pos2) {
    let here = viewport == ctx.viewport_id();
    ctx.data_mut(|d| d.insert_temp(target_id(), Target { panel, doc, pos, viewport, pending: !here }));
    if here {
        egui::Popup::open_id(ctx, popup_id());
    }
}

/// The panel whose menu is open, if any.
pub fn open_panel(ctx: &egui::Context) -> Option<PanelKind> {
    if !egui::Popup::is_id_open(ctx, popup_id()) {
        return None;
    }
    ctx.data(|d| d.get_temp::<Target>(target_id())).map(|t| t.panel)
}

/// One menu row.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// `key` is the automation id's last part; an empty `command` is not available yet.
    Item {
        key: String,
        label: &'static str,
        command: &'static str,
        params: Value,
        enabled: bool,
        checked: bool,
    },
    Sub {
        key: String,
        label: &'static str,
        children: Vec<Entry>,
    },
    Separator,
}

fn item(key: &str, label: &'static str, command: &'static str, params: Value, enabled: bool) -> Entry {
    Entry::Item { key: key.to_string(), label, command, params, enabled, checked: false }
}

fn check(key: &str, label: &'static str, command: &'static str, params: Value, enabled: bool, checked: bool) -> Entry {
    Entry::Item { key: key.to_string(), label, command, params, enabled, checked }
}

/// The menu of `panel` (opened on its document tab `doc`): the generic panel and panel group
/// items, then the panel's own. Empty when the panel isn't open.
pub fn entries(app: &EffectcraftApp, panel: PanelKind, doc: Option<u64>) -> Vec<Entry> {
    let Some((group, _, floating)) = app.panel_group(panel) else { return vec![] };
    let p = json!({"panel": panel.id()});
    let with_doc = match doc.filter(|_| panel == PanelKind::Timeline) {
        Some(id) => json!({"panel": panel.id(), "comp": id}),
        None => p.clone(),
    };
    // The Timeline's comp tabs count as panels of the group (After Effects).
    let comp_tabs = if with_doc.get("comp").is_some() { app.session.state.open_comps.len() } else { 1 };
    let docked = app.ui.dock.panel_count();
    let mut v = vec![
        item("closePanel", tr("Close Panel"), "window.closePanel", with_doc.clone(), true),
        item("floatPanel", tr("Undock Panel"), "window.floatPanel", p.clone(), if floating { group.len() > 1 } else { docked > 1 }),
    ];
    if floating {
        v.push(item("dockPanel", tr("Dock Panel"), "window.dockPanel", p.clone(), true));
    }
    // Multiple monitors (#486): a native window of its own, which can go on another screen.
    if cfg!(not(target_arch = "wasm32")) {
        let windowed = app.in_panel_window(panel);
        let label = if windowed { tr("Move Panel to Main Window") } else { tr("Move Panel to New Window") };
        v.push(item("panelWindow", label, "window.panelWindow", json!({"panel": panel.id(), "window": !windowed}), true));
    }
    v.push(item("closeOtherPanels", tr("Close Other Panels in Group"), "window.closeOtherPanels", with_doc, group.len() > 1 || comp_tabs > 1));
    v.push(Entry::Sub {
        key: "groupSettings".into(),
        label: tr("Panel Group Settings"),
        children: vec![
            item("closePanelGroup", tr("Close Panel Group"), "window.closePanelGroup", p.clone(), true),
            item("floatPanelGroup", tr("Undock Panel Group"), "window.floatPanelGroup", p.clone(), !floating && group.len() < docked),
            check("maximizePanel", tr("Maximize Panel Group"), "window.maximizePanel", p.clone(), !floating, app.group_maximized(panel)),
            Entry::Separator,
            check("stackPanelGroup", tr("Stacked Panel Group"), "window.stackPanelGroup", p.clone(), !floating, app.ui.dock.is_stacked(panel)),
            check(
                "soloPanels",
                tr("Solo Panels in Stack"),
                "window.soloPanels",
                p.clone(),
                !floating && app.ui.dock.is_stacked(panel),
                app.ui.dock.solo(panel) == Some(true),
            ),
            check(
                "smallTabs",
                tr("Small Tabs"),
                "window.smallTabs",
                p.clone(),
                true,
                crate::dock::small_tabs(&app.ui.dock, &app.ui.floating, panel) == Some(true),
            ),
        ],
    });
    let own = panel_items(app, panel);
    if !own.is_empty() {
        v.push(Entry::Separator);
        v.extend(own);
    }
    v
}

/// The panel's own items after the generic ones.
fn panel_items(app: &EffectcraftApp, panel: PanelKind) -> Vec<Entry> {
    let has_comp = app.session.active_comp().is_some();
    match panel {
        PanelKind::Timeline => {
            let tl = &app.ui.timeline;
            let columns = crate::panels::timeline_columns::COLUMNS_MENU
                .iter()
                .filter(|id| **id != "name")
                .map(|id| {
                    let on = crate::panels::timeline::column_visible(tl, id);
                    let label = crate::panels::timeline_columns::column_label(tl, id);
                    check(&format!("column.{id}"), tr(label), "timeline.column", json!({"column": id, "visible": !on}), true, on)
                })
                .collect();
            let mut v = vec![Entry::Sub { key: "columns".into(), label: tr("Columns"), children: columns }, Entry::Separator];
            for (label, id) in
                [("Hide Shy Layers", "hideShy"), ("Enable Frame Blending", "frameBlending"), ("Enable Motion Blur", "motionBlur"), ("Draft 3D", "draft3d")]
            {
                v.push(item(&format!("switch.{id}"), tr(label), "comp.setSwitch", json!({"switch": id}), has_comp));
            }
            v.push(Entry::Separator);
            v.push(item("compSettings", tr("Composition Settings…"), "app.compSettings", json!({}), has_comp));
            v
        }
        PanelKind::Composition | PanelKind::Viewer(_) => vec![
            item("newViewer", tr("New Viewer"), "view.newViewer", json!({}), has_comp),
            item("compSettings", tr("Composition Settings…"), "app.compSettings", json!({}), has_comp),
            item("viewOptions", tr("View Options…"), "view.layerControls", json!({}), has_comp),
            check("grid", tr("Show Grid"), "view.grid", json!({}), has_comp, app.ui.viewer.grid),
            check("safeMargins", tr("Title/Action Safe"), "view.safeMargins", json!({}), has_comp, app.ui.viewer.safe_margins),
        ],
        PanelKind::Project => {
            let columns = crate::panels::project::COLUMNS
                .iter()
                .map(|(id, label, _)| {
                    let on = app.ui.project_columns.iter().any(|c| c == id);
                    check(&format!("column.{id}"), tr(label), "project.column", json!({"column": id, "visible": !on}), true, on)
                })
                .collect();
            vec![
                Entry::Sub { key: "columns".into(), label: tr("Columns"), children: columns },
                item("projectSettings", tr("Project Settings…"), "file.projectSettings", json!({}), true),
            ]
        }
        _ => vec![],
    }
}

/// Draw `entries`; a chosen item's command goes to `chosen`.
fn rows(auto: &mut crate::automation::Registry, ui: &mut egui::Ui, entries: &[Entry], chosen: &mut Option<(String, Value)>) {
    for e in entries {
        match e {
            Entry::Separator => {
                ui.separator();
            }
            Entry::Item { key, label, command, params, enabled, checked } => {
                let r = ui.add_enabled(*enabled && !command.is_empty(), egui::Button::new((crate::menus::gutter(*checked), *label)));
                auto.add(&format!("panelMenu.{key}"), r.rect, label);
                if r.clicked() {
                    *chosen = Some((command.to_string(), params.clone()));
                    ui.close();
                }
            }
            Entry::Sub { key, label, children } => {
                let r = ui.menu_button((crate::menus::gutter(false), *label), |ui| {
                    ui.set_min_width(200.0);
                    rows(auto, ui, children, chosen);
                });
                auto.add(&format!("panelMenu.{key}"), r.response.rect, label);
            }
        }
    }
}

/// Show the open panel menu (drawn by the dock area every frame) and run what is chosen.
pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let Some(mut t) = ctx.data(|d| d.get_temp::<Target>(target_id())) else { return };
    // (Menus open in one window: another window's menu is drawn there.)
    if t.viewport != ctx.viewport_id() {
        return;
    }
    if t.pending {
        t.pending = false;
        ctx.data_mut(|d| d.insert_temp(target_id(), t));
        egui::Popup::open_id(&ctx, popup_id());
    }
    if !egui::Popup::is_id_open(&ctx, popup_id()) {
        ctx.data_mut(|d| d.remove::<Target>(target_id()));
        return;
    }
    let entries = entries(app, t.panel, t.doc);
    if entries.is_empty() {
        // (The panel closed meanwhile.)
        egui::Popup::close_id(&ctx, popup_id());
        return;
    }
    let mut chosen = None;
    let layer = egui::LayerId::new(egui::Order::Foreground, popup_id());
    egui::Popup::new(popup_id(), ctx.clone(), egui::PopupAnchor::Position(t.pos), layer)
        .kind(egui::PopupKind::Menu)
        // (Laid out and styled like every egui menu, as the menu bar's.)
        .layout(egui::Layout::top_down_justified(egui::Align::Min))
        .style(egui::containers::menu::menu_style)
        .open_memory(None)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .show(|ui| {
            ui.set_min_width(200.0);
            rows(&mut app.auto, ui, &entries, &mut chosen);
        });
    if let Some((command, params)) = chosen {
        egui::Popup::close_id(&ctx, popup_id());
        ctx.data_mut(|d| d.remove::<Target>(target_id()));
        if let Err(e) = crate::menus::invoke(app, &ctx, &command, params) {
            app.ui.status = e;
        }
    }
}
