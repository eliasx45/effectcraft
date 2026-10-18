//! The dock area: lays out the docked tree (or the maximized panel), draws each group's chrome
//! and body, floating (undocked) panel groups (OS windows of their own on the desktop, see
//! [`crate::float_windows`]; over the dock where egui can't open windows), and tab
//! drag-and-drop: drag a tab onto a group to tab it there (center) or split it off to a side
//! (After Effects' drop zones, with a highlight of where it will land); hold Cmd/Ctrl when
//! releasing, or drop outside every group, to float it. `` ` `` maximizes the panel under the
//! pointer. A floating group can also be moved explicitly into its own native window
//! ([`EffectcraftApp::set_panel_window`]), which can sit on another monitor.

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

use crate::dock::{self, DockAction, DockNode, Group, Layout, PanelKind, PanelWindow, Zone};
use crate::{EffectcraftApp, panels};
use craft_ui::docking::{Action, Placement};

pub(crate) fn drag_id() -> egui::Id {
    egui::Id::new("dock-tab-drag")
}

impl EffectcraftApp {
    pub(crate) fn dock_command(&mut self, ctx: &egui::Context, action: Action<PanelKind>) -> Result<(), String> {
        let value = serde_json::to_value(action).map_err(|error| error.to_string())?;
        crate::menus::invoke(self, ctx, "window.dockAction", serde_json::json!({"action": value})).map(|_| ())
    }

    /// Tab labels that name what the panel shows, as After Effects does: "Composition Intro",
    /// "Effect Controls Title", "Properties: Title" (the Timeline's tabs are its comps, see
    /// [`Self::tab_docs`]).
    fn tab_titles(&self) -> Vec<(PanelKind, String)> {
        let mut out = Vec::new();
        // ScriptUI panels are named after their script.
        for w in self.session.script_ui.windows.iter().filter(|w| w.kind == effectcraft_engine::scriptui::WindowKind::Panel) {
            if let Some(t) = panels::scriptui_view::panel_title(self, w.id) {
                out.push((PanelKind::ScriptPanel(w.id), t));
            }
        }
        // Each Composition viewer is named after the comp it shows.
        for id in std::iter::once(0).chain(self.ui.viewers.keys().copied().filter(|v| *v != 0)) {
            if let Some(t) = panels::viewers::title(self, id) {
                out.push((panels::viewers::panel(id), t));
            }
        }
        let Some(comp) = self.session.active_comp() else { return out };
        if let Some(l) = self.session.state.selected_layers.first().and_then(|id| comp.layer(*id)) {
            out.push((PanelKind::EffectControls, format!("Effect Controls {}", l.name)));
            out.push((PanelKind::Properties, format!("Properties: {}", l.name)));
        }
        out
    }

    /// A comp's label colour as a tab swatch (faint without a label).
    fn comp_swatch(&self, cid: effectcraft_engine::project::ItemId) -> Color32 {
        match self.session.project.item(cid).map(|i| i.label) {
            Some(l) if l != effectcraft_engine::color::Label::None => self.tokens.label(l),
            _ => self.tokens.text_faint,
        }
    }

    /// The Timeline's tabs: one per open comp, as in After Effects (opening a precomp adds a tab
    /// next to its parent's instead of replacing it).
    fn tab_docs(&self) -> Vec<(PanelKind, Vec<dock::DocTab>)> {
        let st = &self.session.state;
        let locked = self.ui.locked_tabs.contains(&PanelKind::Timeline.id());
        let docs = st
            .open_comps
            .iter()
            .filter_map(|id| {
                let it = self.session.project.item(*id).filter(|i| i.as_comp().is_some())?;
                let deco = dock::TabDeco { swatch: self.comp_swatch(*id), locked, viewer: true };
                Some(dock::DocTab { id: id.0, title: it.name.clone(), deco: Some(deco), active: st.active_comp == Some(*id) })
            })
            .collect();
        vec![(PanelKind::Timeline, docs)]
    }

    /// The Composition and Timeline tabs' close button, label-colour swatch and viewer lock.
    fn tab_decos(&self) -> Vec<(PanelKind, dock::TabDeco)> {
        let Some(cid) = self.session.active_comp_id() else { return vec![] };
        let swatch = self.comp_swatch(cid);
        let mut v: Vec<(PanelKind, dock::TabDeco)> =
            vec![(PanelKind::Timeline, dock::TabDeco { swatch, locked: self.ui.locked_tabs.contains(&PanelKind::Timeline.id()), viewer: true })];
        // Every Composition viewer: its comp's swatch and its own lock.
        for id in std::iter::once(0).chain(self.ui.viewers.keys().copied().filter(|v| *v != 0)) {
            let p = panels::viewers::panel(id);
            let swatch = panels::viewers::comp_of(self, id).map_or(self.tokens.text_faint, |c| self.comp_swatch(c));
            v.push((p, dock::TabDeco { swatch, locked: self.ui.locked_tabs.contains(&p.id()), viewer: true }));
        }
        // Effect Controls carries the selected layer's label colour.
        if let Some(l) = self.session.active_comp().and_then(|c| self.session.state.selected_layers.first().and_then(|id| c.layer(*id)))
            && l.label != effectcraft_engine::color::Label::None
        {
            v.push((PanelKind::EffectControls, dock::TabDeco { swatch: self.tokens.label(l.label), locked: false, viewer: false }));
        }
        v
    }

    /// Settings ▸ Appearance ▸ Use Label Color for Related Tabs: the Composition and Timeline
    /// tabs carry their comp's label colour, Effect Controls and Properties the layer's.
    fn label_tab_marks(&self, ui: &egui::Ui) {
        if !self.session.prefs.appearance.use_label_color_for_tabs {
            return;
        }
        let Some(cid) = self.session.active_comp_id() else { return };
        let comp_label = self.session.project.item(cid).map(|i| i.label);
        let layer_label = self.session.active_comp().and_then(|c| self.session.state.selected_layers.first().and_then(|l| c.layer(*l))).map(|l| l.label);
        // (the Composition, Timeline and Effect Controls tabs carry their swatch already)
        let _ = comp_label;
        for (p, label) in [(PanelKind::Properties, layer_label)] {
            // (Tabs in undocked OS windows are drawn there.)
            let (Some(label), Some(e)) = (label, self.auto.find(&format!("panel.tab.{}", p.id())).filter(|e| e.window.is_none())) else { continue };
            if label == effectcraft_engine::color::Label::None {
                continue;
            }
            let r = Rect::from_min_size(pos2(e.rect[0] + 2.0, e.rect[1] + 9.0), vec2(4.0, (e.rect[3] - 16.0).max(6.0)));
            ui.painter().rect_filled(r, 1.0, self.tokens.label(label));
        }
    }

    /// Edit the full layout (docked tree + floating groups) with a [`Layout`] operation.
    pub fn edit_layout(&mut self, f: impl FnOnce(&mut Layout) -> bool) -> bool {
        let Ok(shared) = crate::dock_shared::encode(&self.ui.dock, &self.ui.floating) else { return false };
        if shared.validate().is_err() {
            return false;
        }
        let mut l = Layout { root: self.ui.dock.clone(), floating: self.ui.floating.clone() };
        if !f(&mut l) {
            return false;
        }
        self.ui.dock = l.root;
        self.ui.floating = l.floating;
        if self.ui.maximized.is_some_and(|m| !self.ui.dock.contains(m)) {
            self.ui.maximized = None;
        }
        true
    }

    /// Close a panel wherever it is (docked or floating).
    pub fn close_panel(&mut self, p: PanelKind) {
        // Commit the layout first: rejected closes must not release viewer or script state.
        if !self.edit_layout(|layout| layout.apply(Action::Close { panel: p }).is_ok()) {
            return;
        }
        // Closing a ScriptUI panel closes its script window (its script's onClose runs).
        if let PanelKind::ScriptPanel(id) = p
            && self.session.script_ui.window(id).is_some()
            && let Err(e) = self.session.execute("scriptui.close", serde_json::json!({"window": id}))
        {
            self.ui.status = e.to_string();
        }
        panels::viewers::on_close(self, p);
        if self.ui.maximized == Some(p) {
            self.ui.maximized = None;
        }
    }

    /// Toggle the maximized panel (`p`, or none).
    pub fn toggle_maximize(&mut self, p: PanelKind) {
        self.ui.maximized = if self.ui.maximized == Some(p) || !self.ui.dock.contains(p) { None } else { Some(p) };
    }

    /// The document a panel shows as a tab of its own: the Timeline's active comp.
    pub fn shown_doc(&self, p: PanelKind) -> Option<u64> {
        let st = &self.session.state;
        st.active_comp.filter(|c| p == PanelKind::Timeline && st.open_comps.contains(c)).map(|c| c.0)
    }

    /// The panel group holding `p` (docked or floating): its panels, the shown one and whether
    /// it floats.
    pub fn panel_group(&self, p: PanelKind) -> Option<(Vec<PanelKind>, usize, bool)> {
        Layout { root: self.ui.dock.clone(), floating: self.ui.floating.clone() }.group_of(p)
    }

    /// The panel menu's Close Panel: a Timeline comp tab (`doc`) closes that comp's Timeline (the
    /// panel stays for the other open comps), any other tab its panel.
    pub fn close_panel_tab(&mut self, ctx: &egui::Context, p: PanelKind, doc: Option<u64>) -> Result<(), String> {
        match doc {
            Some(id) if p == PanelKind::Timeline => crate::menus::invoke(self, ctx, "comp.close", serde_json::json!({"comp": id})).map(|_| ()),
            _ => {
                self.close_panel(p);
                Ok(())
            }
        }
    }

    /// Close Other Panels in Group: every other panel of `p`'s group, and on a Timeline comp tab
    /// (`doc`) the other comps' Timeline tabs too. Returns how many tabs closed.
    pub fn close_other_panels(&mut self, ctx: &egui::Context, p: PanelKind, doc: Option<u64>) -> Result<usize, String> {
        let (group, _, _) = self.panel_group(p).ok_or_else(|| format!("{} is not open", p.title()))?;
        let mut n = 0;
        for q in group.into_iter().filter(|q| *q != p) {
            self.close_panel(q);
            n += 1;
        }
        if let Some(id) = doc.filter(|_| p == PanelKind::Timeline) {
            let others: Vec<u64> = self.session.state.open_comps.iter().map(|c| c.0).filter(|c| *c != id).collect();
            for c in others {
                crate::menus::invoke(self, ctx, "comp.close", serde_json::json!({"comp": c}))?;
                n += 1;
            }
        }
        self.raise_panel(p);
        Ok(n)
    }

    /// Panel Group Settings ▸ Close Panel Group: every panel of `p`'s group.
    pub fn close_panel_group(&mut self, p: PanelKind) -> Result<usize, String> {
        let (group, _, _) = self.panel_group(p).ok_or_else(|| format!("{} is not open", p.title()))?;
        let n = group.len();
        for q in group {
            self.close_panel(q);
        }
        Ok(n)
    }

    /// Dock a floating (or closed) panel back in its usual place: with its neighbours in the
    /// workspace's own layout when one of them is docked, else where Window ▸ <panel> opens it.
    pub fn redock_panel(&mut self, p: PanelKind) {
        if !self.ui.dock.contains(p) {
            self.edit_layout(|l| l.unfloat(p));
            let home = dock::workspace(&self.ui.workspace).group_of(p).map(|(g, _)| g).unwrap_or_default();
            let anchor = home.iter().copied().find(|q| *q != p && self.ui.dock.contains(*q));
            if !anchor.is_some_and(|a| self.ui.dock.insert_tab(p, a, home.iter().skip_while(|q| **q != p).nth(1).copied())) {
                self.raise_panel(p);
            }
            // (Its usual neighbours may all be closed.)
            self.ui.dock.add_anywhere(p);
        }
        self.show_panel(p);
    }

    /// Panel Group Settings ▸ Stacked Panel Group: `stacked`, or toggle when `None`. Returns the
    /// new state.
    pub fn stack_panel_group(&mut self, p: PanelKind, stacked: Option<bool>) -> Result<bool, String> {
        let on = stacked.unwrap_or(!self.ui.dock.is_stacked(p));
        if !self.ui.dock.set_stacked(p, on) {
            return Err(format!("{} is not docked", p.title()));
        }
        Ok(on)
    }

    /// The docked panel whose group is under `pos` (from the last frame), else the focused one.
    pub fn panel_at(&self, pos: Option<egui::Pos2>) -> PanelKind {
        pos.and_then(|p| self.dock_rects.iter().rev().find(|(_, r)| r.contains(p)).map(|(k, _)| *k)).unwrap_or(self.ui.focused)
    }

    pub(crate) fn panel_body(&mut self, ui: &mut egui::Ui, p: PanelKind, rect: Rect) {
        self.auto.add(&format!("panel.{}", p.id()), rect, p.title());
        let mut content = rect.shrink(1.0);
        let viewer = p == panels::viewers::active_panel(self);
        if viewer && !self.ui.start_screen && panels::precomp::has_flow(self) {
            // Composition Navigator: the flow of nested comps above the viewer.
            let nav = Rect::from_min_size(content.min, vec2(content.width(), panels::precomp::NAV_H));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(nav).id_salt("comp-navigator"));
            child.set_clip_rect(nav.intersect(ui.clip_rect()));
            panels::precomp::navigator(self, &mut child, nav);
            content.min.y = nav.max.y;
        }
        let mut child = ui.new_child(
            egui::UiBuilder::new().max_rect(content).id_salt(("panel", p.id())).ui_stack_info(egui::UiStackInfo::default().with_tag(crate::widgets::PANEL_TAG)),
        );
        child.set_clip_rect(content.intersect(ui.clip_rect()));
        panels::show(self, &mut child, p, content);
        if viewer && !self.ui.start_screen {
            panels::anim_tools::sketch_overlay(self, &mut child);
        }
    }

    pub(crate) fn dock_area(&mut self, ui: &mut egui::Ui, body: Rect) {
        let t = self.tokens;
        let ctx = ui.ctx().clone();
        if self.ui.start_screen {
            // Home covers the whole workspace, as in After Effects (the panels wait behind it).
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("home"));
            child.set_clip_rect(body.intersect(ui.clip_rect()));
            panels::home::show(self, &mut child, body);
            // (Undocked OS windows wait behind it too.)
            self.os_windows.drag_overlay = None;
            self.os_windows.begin_frame(&ctx);
            self.os_windows.end_frame(&ctx);
            return;
        }
        // The maximized panel fills the area; the tree is kept as it is.
        let maximized = self.ui.maximized.filter(|m| self.ui.dock.contains(*m));
        let mut dock = match maximized {
            Some(m) => DockNode::Tabs { panels: vec![m], active: 0, small: self.ui.dock.small_tabs(m).unwrap_or(false) },
            None => std::mem::replace(&mut self.ui.dock, DockNode::Tabs { panels: vec![], active: 0, small: false }),
        };
        let mut groups = Vec::new();
        let mut layout_actions = Vec::new();
        dock::layout_with_actions(ui, &mut dock, body, &t, "", &mut groups, &mut self.auto, &mut layout_actions);
        let mut actions = Vec::new();
        let titles = self.tab_titles();
        let decos = self.tab_decos();
        let docs = self.tab_docs();
        let title = |p: PanelKind| titles.iter().find(|(k, _)| *k == p).map(|(_, s)| s.clone()).unwrap_or_else(|| p.title().to_string());
        let info = dock::TabInfo { title: &title, decos: &decos, docs: &docs };
        // Where each group's tabs are (tab drags show where a tab will land between them).
        let mut tab_rects: Vec<(String, Vec<(PanelKind, Rect)>)> = vec![];
        for g in &groups {
            let c = dock::draw_group_chrome(ui, g, &t, &mut self.auto, &info);
            actions.extend(c.actions);
            tab_rects.push((g.path.clone(), c.tabs));
        }
        self.label_tab_marks(ui);
        if maximized.is_none() {
            self.ui.dock = dock;
        }
        self.dock_rects = groups.iter().filter_map(|g| Some((*g.panels.get(g.active)?, g.rect))).collect();
        for g in &groups {
            let Some(p) = g.panels.get(g.active).copied() else { continue };
            if g.content.height() < 2.0 {
                continue; // a collapsed stacked panel: header only
            }
            self.panel_body(ui, p, g.content);
        }
        for g in &groups {
            if g.panels.get(g.active) == Some(&self.ui.focused) {
                ui.painter().rect_stroke(g.rect, 0.0, Stroke::new(1.0, t.focus), StrokeKind::Inside);
            }
        }
        // Floating groups: OS windows of their own on the desktop, else over the dock.
        let mut float_groups: Vec<Group> = vec![];
        // Each one's OS window (none where they float in the main window).
        let mut float_windows: Vec<Option<u64>> = vec![];
        dock::assign_floating_ids(&mut self.ui.floating);
        let origin = crate::float_windows::main_origin(&ctx).filter(|_| crate::float_windows::enabled(&ctx));
        let mut closed = vec![];
        self.os_windows.begin_frame(&ctx);
        // In paint order (groups floating over the dock): the last one is on top for tab drops.
        let layers = ctx.memory(|memory| memory.layer_ids().collect::<Vec<_>>());
        let mut floating_order: Vec<_> = (0..self.ui.floating.len()).collect();
        floating_order.sort_by_key(|index| {
            self.ui
                .floating
                .get(*index)
                .and_then(|group| group.panels.first())
                .and_then(|panel| {
                    let id = egui::Id::new(("floating-panel", panel.id()));
                    layers.iter().position(|layer| layer.id == id)
                })
                .unwrap_or(usize::MAX)
        });
        crate::workspace_state::migrate_panel_windows(&mut self.ui);
        for i in floating_order {
            let shown = match origin {
                Some(o) => self.floating_os_window(&ctx, i, o, &info, &mut actions, &mut closed),
                None => self.floating_window(&ctx, i, &info, &mut actions),
            };
            if let Some((g, tabs)) = shown {
                tab_rects.push((g.path.clone(), tabs));
                float_groups.push(g);
                float_windows.push(origin.and_then(|_| self.ui.floating.get(i)).map(|f| f.id));
            }
        }
        self.os_windows.end_frame(&ctx);
        // An undocked window's close button closes its panels, as in After Effects.
        for p in closed {
            if let Err(e) = self.close_panel_group(p) {
                self.ui.status = e;
            }
        }
        for action in layout_actions {
            let _ = self.dock_command(&ctx, action);
        }
        for a in actions {
            match a {
                DockAction::Activate(p) => {
                    let _ = self.dock_command(&ctx, Action::Activate { panel: p });
                }
                DockAction::ToggleStacked(p) => {
                    let _ = self.dock_command(&ctx, Action::SetStackOpen { panel: p, open: !self.ui.dock.is_visible(p) });
                }
                DockAction::Focus(p) => {
                    self.ui.focused = p;
                    panels::viewer_text::panel_clicked(self, &ctx, p);
                    // A click in (or on the tab of) another Composition viewer makes it active.
                    if let Some(id) = panels::viewers::id_of(p) {
                        panels::viewers::activate(self, &ctx, id);
                    }
                }
                DockAction::Close(p) => {
                    let _ = self.dock_command(&ctx, Action::Close { panel: p });
                }
                DockAction::PanelMenu(p, pos) => panels::panel_menu::open(&ctx, p, self.shown_doc(p), pos),
                DockAction::TabMenu(p, doc, pos) => panels::panel_menu::open(&ctx, p, doc, pos),
                DockAction::BeginDrag(p) => {
                    ctx.data_mut(|d| d.insert_temp(drag_id(), p));
                    self.os_windows.drag_from = None;
                }
                DockAction::ToggleLock(p) => {
                    if !self.ui.locked_tabs.remove(&p.id()) {
                        self.ui.locked_tabs.insert(p.id().to_string());
                    }
                }
                // The Timeline's comp tabs: show that comp / close its Timeline.
                DockAction::ActivateDoc(p, id) => {
                    let _ = self.dock_command(&ctx, Action::Activate { panel: p });
                    if self.session.state.active_comp.map(|c| c.0) != Some(id) {
                        let _ = crate::menus::invoke(self, &ctx, "comp.open", serde_json::json!({"comp": id}));
                    }
                }
                DockAction::CloseDoc(_, id) => {
                    let _ = crate::menus::invoke(self, &ctx, "comp.close", serde_json::json!({"comp": id}));
                }
                DockAction::MoveDoc(_, id, before) => {
                    let _ = crate::menus::invoke(self, &ctx, "comp.moveTab", serde_json::json!({"comp": id, "before": before}));
                }
            }
        }
        self.tab_drag(&ctx, &groups, &float_groups, &float_windows, &tab_rects);
        panels::panel_menu::show(self, ui);
    }

    /// One floating group: a window with a tab strip (drag the empty strip to move it, the
    /// corner to resize it) and the active panel's body.
    fn floating_window(
        &mut self,
        ctx: &egui::Context,
        i: usize,
        info: &dock::TabInfo,
        actions: &mut Vec<DockAction>,
    ) -> Option<(Group, Vec<(PanelKind, Rect)>)> {
        let t = self.tokens;
        let f = self.ui.floating.get(i)?.clone();
        let screen = ctx.content_rect();
        let mut rect = Rect::from_min_size(pos2(f.rect[0], f.rect[1]), vec2(f.rect[2].max(160.0), f.rect[3].max(100.0)));
        // Keep the strip reachable.
        rect = rect.translate(vec2(
            (screen.min.x - rect.min.x).max(0.0) + (screen.max.x - 40.0 - rect.min.x).min(0.0),
            (screen.min.y - rect.min.y).max(0.0) + (screen.max.y - 30.0 - rect.min.y).min(0.0),
        ));
        let id = egui::Id::new(("floating-panel", f.panels.first().map(|p| p.id()).unwrap_or_default()));
        let mut group = None;
        // Not constrained to the window: the group is drawn where its rect says (the clamp above
        // keeps its strip reachable), so it follows the pointer past the window's edges.
        egui::Area::new(id).order(egui::Order::Middle).fixed_pos(rect.min).constrain(false).interactable(true).show(ctx, |ui| {
            let (r, _) = ui.allocate_exact_size(rect.size(), Sense::hover());
            ui.painter().rect_filled(r.expand(1.0), t.radius, Color32::from_black_alpha(90));
            // (Shorter with Small Tabs.)
            let strip_h = f.strip_h(&t);
            let strip = Rect::from_min_size(r.min, vec2(r.width(), strip_h));
            // Move by the strip (tabs, registered later, take precedence over it).
            // (`id.with("move")` is the id of the `Area`'s own background widget.)
            let mv = ui.interact(strip, id.with("strip"), Sense::drag());
            self.auto.add(&format!("panel.float.{i}.move"), strip, "Move floating panel");
            let mut nr = r;
            if mv.dragged() {
                nr = nr.translate(mv.drag_delta());
            }
            let g = Group {
                path: format!("f{i}"),
                rect: r,
                content: Rect::from_min_max(pos2(r.min.x, r.min.y + strip_h), r.max),
                panels: f.panels.clone(),
                active: f.active.min(f.panels.len().saturating_sub(1)),
                stacked: None,
            };
            let c = dock::draw_group_chrome(ui, &g, &t, &mut self.auto, info);
            actions.extend(c.actions);
            if let Some(p) = g.panels.get(g.active).copied() {
                self.panel_body(ui, p, g.content);
            }
            ui.painter().rect_stroke(r, t.radius, Stroke::new(1.0, t.field_border), StrokeKind::Outside);
            if g.panels.get(g.active) == Some(&self.ui.focused) {
                ui.painter().rect_stroke(r, t.radius, Stroke::new(1.0, t.focus), StrokeKind::Inside);
            }
            // Resize from the corner.
            let grip = Rect::from_min_max(r.max - vec2(14.0, 14.0), r.max);
            let rs = ui.interact(grip, id.with("resize"), Sense::click_and_drag());
            rs.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, rs.enabled(), crate::i18n::tr("Resize floating panel")));
            self.auto.add_response(&format!("panel.float.{i}.resize"), &rs, "Resize floating panel");
            for k in 0..3 {
                let o = 4.0 + k as f32 * 4.0;
                ui.painter().line_segment([pos2(r.max.x - o, r.max.y - 2.0), pos2(r.max.x - 2.0, r.max.y - o)], Stroke::new(1.0, t.text_faint));
            }
            if rs.hovered() || rs.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            }
            let delta = floating_resize_delta(ui, &rs);
            if rs.has_focus() {
                ui.painter().rect_stroke(grip, 2.0, Stroke::new(1.5, t.focus), StrokeKind::Inside);
            }
            if delta != egui::Vec2::ZERO {
                nr = Rect::from_min_size(nr.min, (nr.size() + delta).max(vec2(160.0, 100.0)));
            }
            if nr != r
                && let Some(panel) = f.panels.first().copied()
            {
                let _ = self.dock_command(ctx, Action::MoveFloating { panel, rect: [nr.min.x, nr.min.y, nr.width(), nr.height()] });
            }
            group = Some((g, c.tabs));
        });
        group
    }

    /// All undocked groups use the same native window implementation on desktop.
    pub fn in_panel_window(&self, p: PanelKind) -> bool {
        self.ui.floating.iter().any(|group| group.panels.contains(&p))
    }

    /// Move into the shared floating-window path, or dock the whole group back in the main window.
    pub fn set_panel_window(&mut self, ctx: &egui::Context, p: PanelKind, window: Option<bool>) -> Result<bool, String> {
        crate::workspace_state::migrate_panel_windows(&mut self.ui);
        let current = self.in_panel_window(p);
        let window = window.unwrap_or(!current);
        if window == current {
            return Ok(current);
        }
        if !window {
            let panels = self.ui.floating.iter().find(|g| g.panels.contains(&p)).map(|g| g.panels.clone()).unwrap_or_default();
            for panel in panels {
                self.redock_panel(panel);
            }
            return Ok(false);
        }
        if !self.ui.dock.contains(p) {
            return Err(format!("{} is not open", p.title()));
        }
        let rect = default_float_rect(crate::float_windows::main_content_rect(ctx));
        if !self.edit_layout(|layout| layout.float(p, rect)) {
            return Err(format!("can't undock {}", p.title()));
        }
        let origin = crate::float_windows::main_origin(ctx).unwrap_or(Pos2::ZERO);
        if let Some(group) = self.ui.floating.iter_mut().find(|g| g.panels.contains(&p)) {
            group.screen = Some(PanelWindow::sanitized([origin.x + rect[0], origin.y + rect[1], rect[2], rect[3]]));
        }
        if self.ui.maximized == Some(p) {
            self.ui.maximized = None;
        }
        Ok(true)
    }

    /// A tab being dragged. Over a tab strip (or the middle of a group) it shows where the tab
    /// will land: an insertion mark between two tabs (a drop there reorders a group's own tabs
    /// too); near a group's edge, the half it will split off. On release it lands there (or
    /// floats with Cmd/Ctrl or outside every group). Over an undocked OS window that window
    /// draws the highlight and the dragged tab (`floats` are the floating groups, `windows`
    /// their OS windows).
    fn tab_drag(&mut self, ctx: &egui::Context, groups: &[Group], floats: &[Group], windows: &[Option<u64>], tab_rects: &[(String, Vec<(PanelKind, Rect)>)]) {
        self.os_windows.drag_overlay = None;
        let Some(p) = ctx.data(|d| d.get_temp::<PanelKind>(drag_id())) else {
            self.os_windows.drag_from = None;
            self.os_windows.drag_feed = None;
            return;
        };
        let t = self.tokens;
        // Escape, a lost button release (or window focus) or the panel closing meanwhile
        // cancels the drag. (A drag from an undocked OS window is fed by that window.)
        let canceled = ctx.input(|i| i.key_pressed(egui::Key::Escape))
            || (self.os_windows.drag_from.is_none() && ctx.input(|i| !i.focused || (!i.pointer.primary_down() && !i.pointer.any_released())));
        if canceled || !(self.ui.dock.contains(p) || self.ui.floating.iter().any(|group| group.panels.contains(&p))) {
            ctx.data_mut(|d| d.remove::<PanelKind>(drag_id()));
            self.os_windows.drag_from = None;
            self.os_windows.drag_feed = None;
            self.os_windows.drag_overlay = None;
            return;
        }
        // The pointer (main-window points): a drag that started in an undocked OS window is
        // followed by that window, which has the pointer until the button is released.
        let (pos, released, float_drop) = match self.os_windows.drag_from {
            Some(w) => {
                let origin = crate::float_windows::main_origin(ctx);
                if !self.os_windows.shown.contains_key(&w) || origin.is_none() {
                    // (Its window closed.)
                    ctx.data_mut(|d| d.remove::<PanelKind>(drag_id()));
                    self.os_windows.drag_from = None;
                    return;
                }
                let (Some(f), Some(o)) = (self.os_windows.drag_feed, origin) else { return };
                (f.screen - o.to_vec2(), f.released, f.command)
            }
            None => {
                let Some(pos) = ctx.pointer_latest_pos() else { return };
                (pos, ctx.input(|i| i.pointer.any_released()), ctx.input(|i| i.modifiers.command))
            }
        };
        let target = drop_target(p, pos, float_drop, groups, floats, tab_rects, &t);
        // The undocked OS window under the pointer draws the feedback in its next pass, in its
        // own coordinates (this window's painter can't reach it); else it is drawn here.
        let window = target.float.and_then(|i| windows.get(i).copied().flatten());
        let to_window = window.and_then(|w| Some((w, self.os_windows.shown.get(&w)?.min, crate::float_windows::main_origin(ctx)?)));
        match to_window {
            Some((w, inner, origin)) => {
                let d = origin - inner;
                self.os_windows.drag_overlay =
                    Some(crate::float_windows::DragOverlay { window: w, panel: p, pointer: pos + d, mark: target.mark.map(|m| m.translate(d)) });
            }
            None => {
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("dock-drop")));
                if let Some(m) = target.mark {
                    m.paint(&painter, &t);
                }
                drag_ghost(&painter, p, pos, &t);
            }
        }
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        ctx.request_repaint();
        if released {
            ctx.data_mut(|d| d.remove::<PanelKind>(drag_id()));
            self.os_windows.drag_from = None;
            self.os_windows.drag_feed = None;
            self.os_windows.drag_overlay = None;
            let ok = match target.plan {
                // One shared docking transaction each (tab order, edge splits, floating).
                Some(DropPlan::Tab { anchor, before }) => {
                    self.dock_command(ctx, Action::Move { panel: p, anchor, placement: Placement::Tab { before } }).is_ok()
                }
                Some(DropPlan::Side { anchor, zone }) => {
                    self.dock_command(ctx, Action::Move { panel: p, anchor, placement: Placement::Split(zone.shared()) }).is_ok()
                }
                None if float_drop || !target.over_group => {
                    let r = [pos.x - 40.0, pos.y - 12.0, 420.0, 320.0];
                    self.dock_command(ctx, Action::Float { panel: p, rect: r }).is_ok()
                }
                None => false,
            };
            if ok {
                self.ui.focused = p;
                self.show_panel(p);
            }
        }
    }
}

/// What a tab drag would do with the pointer at `pos`: the group under it, where the tab would
/// land and the highlight that shows it.
struct DropTarget {
    /// The floating group under the pointer (an index into the floating groups).
    float: Option<usize>,
    /// Whether any group is under the pointer (a drop elsewhere floats the tab).
    over_group: bool,
    plan: Option<DropPlan>,
    mark: Option<DropMark>,
}

/// Where dragged tab `p` would land with the pointer at `pos` (main-window points): floating
/// groups are on top, then the docked groups; `float_drop` (Cmd/Ctrl held) floats it anyway.
/// `floats` are in main-window points too (an undocked OS window's group is translated by where
/// the window is); `tab_rects` are each group's tabs, by [`Group::path`].
fn drop_target(
    p: PanelKind,
    pos: Pos2,
    float_drop: bool,
    groups: &[Group],
    floats: &[Group],
    tab_rects: &[(String, Vec<(PanelKind, Rect)>)],
    t: &crate::theme::Tokens,
) -> DropTarget {
    // The floating groups are in paint order: the topmost one under the pointer takes it.
    let float = floats.iter().rposition(|g| g.rect.contains(pos));
    let target = match float.and_then(|i| floats.get(i)) {
        Some(g) => Some((g, true)),
        None => groups.iter().find(|g| g.rect.contains(pos)).map(|g| (g, false)),
    };
    let mut out = DropTarget { float, over_group: target.is_some(), plan: None, mark: None };
    let Some((g, is_float)) = target.filter(|_| !float_drop) else { return out };
    let others: Vec<PanelKind> = g.panels.iter().copied().filter(|x| *x != p).collect();
    // The dragged tab alone in its group: nowhere to go here.
    let Some(first) = others.first().copied() else { return out };
    let zone = if is_float { Zone::Center } else { dock::drop_zone(g.rect, g.strip_h(t), pos) };
    if zone != Zone::Center {
        let shown = g.panels.get(g.active).copied().filter(|a| *a != p).unwrap_or(first);
        out.plan = Some(DropPlan::Side { anchor: shown, zone });
        out.mark = Some(DropMark::Side { zone: dock::zone_rect(g.rect, zone).shrink(2.0), group: g.rect });
        return out;
    }
    let rects: Vec<(PanelKind, Rect)> =
        tab_rects.iter().find(|(path, _)| *path == g.path).map(|(_, r)| r.iter().copied().filter(|(k, _)| *k != p).collect()).unwrap_or_default();
    let strip = Rect::from_min_size(g.rect.min, vec2(g.rect.width(), g.strip_h(t)));
    let before = if strip.contains(pos) {
        // Before the first tab whose middle is right of the pointer.
        rects.iter().find(|(_, r)| r.center().x > pos.x).map(|(k, _)| *k)
    } else if g.panels.contains(&p) {
        // The middle of its own group: it stays where it is.
        Some(p)
    } else {
        // The middle of another group: right after its shown tab.
        let shown = g.panels.get(g.active).and_then(|a| others.iter().position(|x| x == a)).unwrap_or(others.len());
        others.get(shown + 1).copied()
    };
    if before == Some(p) {
        return out;
    }
    let x = match before.and_then(|b| rects.iter().find(|(k, _)| *k == b)) {
        Some((_, r)) => r.min.x - 4.0,
        None => rects.last().map_or(strip.min.x + 8.0, |(_, r)| r.max.x + 4.0),
    };
    out.plan = Some(DropPlan::Tab { anchor: first, before });
    out.mark = Some(DropMark::Tab { strip, x, group: (!strip.contains(pos)).then_some(g.rect) });
    out
}

/// The highlight of where a dragged tab will land.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DropMark {
    /// A tab in the tab strip `strip`, at the insertion mark `x`; `group` is outlined when the
    /// pointer is over its body.
    Tab { strip: Rect, x: f32, group: Option<Rect> },
    /// The half `zone` split off `group`.
    Side { zone: Rect, group: Rect },
}

impl DropMark {
    /// Moved by `d` (into another window's coordinates).
    pub(crate) fn translate(self, d: Vec2) -> Self {
        match self {
            DropMark::Tab { strip, x, group } => DropMark::Tab { strip: strip.translate(d), x: x + d.x, group: group.map(|g| g.translate(d)) },
            DropMark::Side { zone, group } => DropMark::Side { zone: zone.translate(d), group: group.translate(d) },
        }
    }

    pub(crate) fn paint(&self, painter: &egui::Painter, t: &crate::theme::Tokens) {
        match *self {
            DropMark::Tab { strip, x, group } => {
                painter.rect_filled(strip.shrink2(vec2(2.0, 1.0)), t.radius, t.focus.gamma_multiply(0.12));
                if let Some(g) = group {
                    painter.rect_stroke(g, t.radius, Stroke::new(1.0, t.focus.gamma_multiply(0.6)), StrokeKind::Inside);
                }
                insertion_mark(painter, x, strip, t.focus);
            }
            DropMark::Side { zone, group } => {
                painter.rect_filled(zone, t.radius, t.focus.gamma_multiply(0.25));
                painter.rect_stroke(zone, t.radius, Stroke::new(2.0, t.focus), StrokeKind::Inside);
                // Outline the whole group for the side zones, like AE's drop-zone frame.
                painter.rect_stroke(group, t.radius, Stroke::new(1.0, t.focus.gamma_multiply(0.6)), StrokeKind::Inside);
            }
        }
    }
}

/// Where a dragged tab lands.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DropPlan {
    /// A tab in the group of `anchor`, before `before` (last when `None`).
    Tab { anchor: PanelKind, before: Option<PanelKind> },
    /// A new group split off one side of `anchor`'s group.
    Side { anchor: PanelKind, zone: Zone },
}

/// The ghost of a dragged tab at the pointer `pos`.
pub(crate) fn drag_ghost(painter: &egui::Painter, p: PanelKind, pos: egui::Pos2, t: &crate::theme::Tokens) {
    let g = painter.layout_no_wrap(p.title().to_string(), crate::theme::Tokens::ui(12.0), Color32::WHITE);
    let gr = Rect::from_min_size(pos + vec2(12.0, 10.0), g.size() + vec2(16.0, 8.0));
    painter.rect_filled(gr, 4.0, t.focus.gamma_multiply(0.85));
    painter.galley(gr.min + vec2(8.0, 4.0), g, Color32::WHITE);
}

/// The insertion mark of a tab drag: a bar between two tabs at `x`, with caps.
fn insertion_mark(painter: &egui::Painter, x: f32, strip: Rect, col: Color32) {
    let (y0, y1) = (strip.min.y + 3.0, strip.max.y - 2.0);
    painter.line_segment([pos2(x, y0), pos2(x, y1)], Stroke::new(2.5, col));
    for (y, d) in [(y0, 1.0), (y1, -1.0)] {
        painter.add(egui::Shape::convex_polygon(vec![pos2(x - 4.5, y), pos2(x + 4.5, y), pos2(x, y + 5.0 * d)], col, Stroke::NONE));
    }
}

/// A new floating group's default rect near the centre of `screen`.
pub fn default_float_rect(screen: Rect) -> [f32; 4] {
    let c = screen.center();
    [c.x - 210.0, c.y - 160.0, 420.0, 320.0]
}

/// A corner is a two-axis resize control; focused arrows belong to it and Tab moves on.
fn floating_resize_delta(ui: &mut egui::Ui, response: &egui::Response) -> egui::Vec2 {
    if response.drag_started() {
        response.request_focus();
    }
    let mut delta = if response.dragged() { response.drag_delta() } else { egui::Vec2::ZERO };
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(response.id, egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() })
        });
        ui.input_mut(|input| {
            for (key, amount) in [
                (egui::Key::ArrowLeft, vec2(-8.0, 0.0)),
                (egui::Key::ArrowRight, vec2(8.0, 0.0)),
                (egui::Key::ArrowUp, vec2(0.0, -8.0)),
                (egui::Key::ArrowDown, vec2(0.0, 8.0)),
            ] {
                if input.consume_key(egui::Modifiers::NONE, key) {
                    delta += amount;
                }
            }
        });
    }
    if response.enabled() { delta } else { egui::Vec2::ZERO }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ThemeKind, Tokens};
    use PanelKind::*;

    fn group(path: &str, rect: Rect, panels: Vec<PanelKind>, t: &Tokens) -> Group {
        Group { path: path.into(), rect, content: Rect::from_min_max(pos2(rect.min.x, rect.min.y + t.tab_h), rect.max), panels, active: 0, stacked: None }
    }

    #[test]
    fn a_tab_dragged_over_an_undocked_window_lands_in_its_group_and_is_drawn_there() {
        let t = Tokens::for_kind(ThemeKind::Dark);
        let docked = group("a", Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0)), vec![Project, Composition], &t);
        // An undocked OS window: content at (1900, 300) on screen, the main window's at (100, 50);
        // its group in main-window points.
        let (origin, inner) = (pos2(100.0, 50.0), pos2(1900.0, 300.0));
        let off = inner - origin;
        let float = group("f0", Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0)).translate(off), vec![Info, Audio], &t);
        let strip = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0, 0.0), pos2(x1, t.tab_h)).translate(off);
        let tabs = vec![("f0".to_string(), vec![(Info, strip(0.0, 60.0)), (Audio, strip(60.0, 130.0))])];
        let (groups, floats) = (vec![docked], vec![float]);
        // Over its tab strip, between its two tabs.
        let pos = pos2(70.0, 10.0) + off;
        let d = drop_target(Project, pos, false, &groups, &floats, &tabs, &t);
        assert_eq!(d.float, Some(0));
        assert_eq!(d.plan, Some(DropPlan::Tab { anchor: Info, before: Some(Audio) }));
        // In the window's own coordinates, the mark is where its tabs are.
        let Some(DropMark::Tab { strip: s, x, group: None }) = d.mark.map(|m| m.translate(origin - inner)) else { panic!("{:?}", d.mark) };
        assert_eq!((s.min, x), (Pos2::ZERO, 56.0));
        // Over its body: after its shown tab, the whole group outlined.
        let d = drop_target(Project, pos2(200.0, 200.0) + off, false, &groups, &floats, &tabs, &t);
        assert_eq!(d.plan, Some(DropPlan::Tab { anchor: Info, before: Some(Audio) }));
        assert!(matches!(d.mark, Some(DropMark::Tab { group: Some(_), .. })));
        // With Cmd/Ctrl the tab floats, but the window is still the one under the pointer.
        let d = drop_target(Project, pos, true, &groups, &floats, &tabs, &t);
        assert_eq!((d.float, d.plan, d.mark, d.over_group), (Some(0), None, None, true));
        // Over the docked group's edge (the main window): a side split, drawn there.
        let d = drop_target(Info, pos2(1190.0, 400.0), false, &groups, &floats, &tabs, &t);
        assert_eq!((d.float, d.plan), (None, Some(DropPlan::Side { anchor: Project, zone: Zone::Right })));
        // Outside every group: it floats.
        let d = drop_target(Info, pos2(1500.0, 900.0), false, &groups, &floats, &tabs, &t);
        assert_eq!((d.float, d.over_group, d.plan), (None, false, None));
    }
}
