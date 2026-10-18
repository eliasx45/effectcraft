//! Undocked panel groups as OS windows, as in After Effects: on the desktop each floating group
//! ([`Floating`]) is a native window of its own (an egui immediate viewport) that can go
//! anywhere, a second monitor included. Where egui embeds viewports instead (the web, the
//! headless test harness and snapshot), the groups float inside the main window as before
//! (`EffectcraftApp::floating_window`).
//!
//! - A group's window opens at its [`Floating::screen`] rect (else at `rect`, relative to the
//!   main window) and its position and size are written back every frame, so they survive
//!   workspace saves and relaunches.
//! - Its close button closes the group's panels (After Effects closes them too); the panel
//!   menu's Dock Panel and Window ▸ <panel> work as for a group floating in the main window.
//! - Shortcuts, the panel menu (≡) and tab drags work in it: a tab dragged out of it lands where
//!   it is dropped, in the main window's dock or another undocked group.
//! - A window saved on a monitor that is gone (unplugged since) opens over the main window
//!   instead ([`rescue`]).
//! - While EffectCraft is the active app the windows float above the main window (After
//!   Effects' floating panels); when another app is, they are ordinary windows ([`OnTop`]).
//! - Agents: automation elements drawn in it carry `window` (the group's [`Floating::id`]) and
//!   their rect is in its coordinates; `ui.click`, `ui.drag`, `ui.scroll` on them (or with
//!   `window`) are delivered to that window, and `ui.key`/`ui.type` follow the last pointer
//!   input. `ui.screenshot` captures the main window only.

use std::collections::{BTreeMap, BTreeSet};

use egui::{Pos2, Rect, Sense, Stroke, StrokeKind, ViewportId, pos2, vec2};

use crate::EffectcraftApp;
use crate::dock::{self, DockAction, Floating, Group, PanelKind};

/// The smallest undocked window (points).
const MIN_SIZE: egui::Vec2 = egui::Vec2::new(160.0, 100.0);

/// The viewport of the floating group `window` ([`Floating::id`]).
pub fn viewport_id(window: u64) -> ViewportId {
    ViewportId::from_hash_of(("effectcraft-floating-panel", window))
}

/// Whether undocked groups get OS windows here (egui opens native windows for viewports).
pub fn enabled(ctx: &egui::Context) -> bool {
    !ctx.embed_viewports()
}

/// Where the main window's content is on screen (points), if the platform says.
pub fn main_origin(ctx: &egui::Context) -> Option<Pos2> {
    ctx.input(|i| i.raw.viewports.get(&ViewportId::ROOT).and_then(|v| v.inner_rect)).map(|r| r.min)
}

/// The main window's content rect in its own coordinates, also from an undocked window's pass
/// (where `ctx.content_rect()` is that window's).
pub fn main_content_rect(ctx: &egui::Context) -> Rect {
    if ctx.viewport_id() == ViewportId::ROOT {
        return ctx.content_rect();
    }
    ctx.input(|i| i.raw.viewports.get(&ViewportId::ROOT).and_then(|v| v.inner_rect))
        .map_or_else(|| ctx.content_rect(), |r| Rect::from_min_size(Pos2::ZERO, r.size()))
}

/// The OS window panel `p` is in (shown in the last frame), if any.
pub fn panel_viewport(app: &EffectcraftApp, p: PanelKind) -> Option<ViewportId> {
    let f = app.ui.floating.iter().find(|f| f.panels.contains(&p))?;
    app.os_windows.shown.contains_key(&f.id).then(|| viewport_id(f.id))
}

/// Runtime state of the undocked groups' OS windows.
#[derive(Default)]
pub struct OsWindows {
    /// The screen rect each open window was created with. Its builder keeps it, so egui doesn't
    /// move or resize the window back every frame.
    created: BTreeMap<u64, [f32; 4]>,
    /// The open windows whose place was checked against the monitors (once, when they open).
    checked: BTreeSet<u64>,
    /// The windows shown in the last frame: group id → content rect on screen (points).
    pub(crate) shown: BTreeMap<u64, Rect>,
    /// The windows shown so far this frame.
    building: BTreeMap<u64, Rect>,
    /// The group whose window comes to the front (Window ▸ <panel>).
    pub(crate) raise: Option<u64>,
    /// The window synthetic key input goes to: the one the last synthetic pointer input went to.
    pub(crate) key_target: Option<u64>,
    /// A tab drag that started in this group's window, and where its pointer is.
    pub(crate) drag_from: Option<u64>,
    pub(crate) drag_feed: Option<DragFeed>,
    /// A tab drag's feedback for the window under the pointer, drawn in its next pass.
    pub(crate) drag_overlay: Option<DragOverlay>,
    /// Whether the windows float above the main window.
    pub(crate) on_top: OnTop,
    /// The windows with a focused text field in their last pass: Edit ▸ Select All (Cmd+A) and
    /// the other text edits from the menu bar go to the field, not the comp.
    pub(crate) typing: BTreeSet<u64>,
}

/// How long no EffectCraft window may have the focus before the undocked windows stop floating
/// above the main window (seconds): focus moving from one of our windows to another passes
/// through a frame or two in which none has it.
const LEVEL_DEBOUNCE: f64 = 0.2;

/// Whether the undocked windows float above the main window, as After Effects' floating panels
/// do: while EffectCraft is the active app (one of its windows has the focus), and not when
/// another app is, so they don't float above its windows. egui has no parent-window API, so
/// this is their window level (`AlwaysOnTop` / `Normal`, set through their viewport builders,
/// which send it only when it changes). Gaining the focus raises them at once; losing it lowers
/// them once it has stayed lost for [`LEVEL_DEBOUNCE`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct OnTop {
    pub on: bool,
    /// Since when (seconds) no window has had the focus while `on`.
    lost_at: Option<f64>,
}

impl OnTop {
    /// A frame at `now` (seconds) in which `focused`: any EffectCraft window has the focus.
    /// Returns the new state when it changes, and while a change waits, how long until it may
    /// happen (repaint then).
    pub(crate) fn update(&mut self, focused: bool, now: f64) -> (Option<bool>, Option<f64>) {
        if focused {
            self.lost_at = None;
            let raised = !self.on;
            self.on = true;
            return (raised.then_some(true), None);
        }
        if !self.on {
            return (None, None);
        }
        let since = *self.lost_at.get_or_insert(now);
        let left = LEVEL_DEBOUNCE - (now - since);
        if left > 0.0 && left <= LEVEL_DEBOUNCE {
            return (None, Some(left));
        }
        // (Also when the clock is odd: never stay above other apps.)
        *self = OnTop::default();
        (Some(false), None)
    }

    /// The undocked windows' window level.
    pub(crate) fn level(&self) -> egui::WindowLevel {
        if self.on { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal }
    }
}

/// What an undocked window draws while a tab is dragged over it (in its own coordinates): the
/// highlight of where the tab will land and the dragged tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DragOverlay {
    /// The group ([`Floating::id`]) whose window draws it.
    pub window: u64,
    pub panel: PanelKind,
    pub pointer: Pos2,
    pub mark: Option<crate::dock_ui::DropMark>,
}

/// The pointer of a tab drag that started in an OS window (which has the pointer until the
/// button is released), for the main window's drop logic.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DragFeed {
    /// On screen, in points.
    pub screen: Pos2,
    pub released: bool,
    pub command: bool,
}

impl OsWindows {
    /// Start a frame's windows: they float above the main window while EffectCraft is the
    /// active app.
    pub(crate) fn begin_frame(&mut self, ctx: &egui::Context) {
        self.building.clear();
        if !enabled(ctx) {
            return;
        }
        let (focused, now) = ctx.input(|i| (i.raw.viewports.values().any(|v| v.focused == Some(true)), i.time));
        if let (_, Some(wait)) = self.on_top.update(focused, now) {
            ctx.request_repaint_after(std::time::Duration::try_from_secs_f64(wait).unwrap_or_default());
        }
    }

    /// After the floating groups are drawn: forget the windows that are gone (they close).
    pub(crate) fn end_frame(&mut self, ctx: &egui::Context) {
        self.shown = std::mem::take(&mut self.building);
        let shown = &self.shown;
        self.created.retain(|id, _| shown.contains_key(id));
        self.checked.retain(|id| shown.contains_key(id));
        if self.key_target.is_some_and(|w| !shown.contains_key(&w)) {
            self.key_target = None;
        }
        // Input for windows that closed is dropped, so the agent waiting on it is answered.
        let live: Vec<ViewportId> = shown.keys().map(|w| viewport_id(*w)).collect();
        ctx.with_plugin::<InputRouter, _>(|r| r.queue.retain(|(v, _)| live.contains(v)));
    }
}

/// Whether a saved screen rect can be a window's: finite, a size, not absurdly far away.
fn plausible(s: &[f32; 4]) -> bool {
    s.iter().all(|v| v.is_finite() && v.abs() < 100_000.0) && s[2] > 0.0 && s[3] > 0.0
}

/// Where a window that is on no monitor goes (on screen, points): inside the main window's
/// content rect `main`, as near its rect `want` and as large as fits (at least [`MIN_SIZE`]).
pub(crate) fn rescue(main: Rect, want: Rect) -> Rect {
    let want = if want.is_finite() { want } else { Rect::from_min_size(main.min, MIN_SIZE) };
    let size = want.size().min(main.size()).max(MIN_SIZE);
    // (`min`/`max` rather than `clamp`, which panics on NaN.)
    let min = want.min.min(main.max - size).max(main.min);
    Rect::from_min_size(min, size)
}

/// Synthetic input (`ui.click` …) for the undocked windows: the main window's goes through
/// `raw_input_hook`, which egui only runs for it.
#[derive(Default)]
pub(crate) struct InputRouter {
    queue: Vec<(ViewportId, egui::Event)>,
}

impl egui::Plugin for InputRouter {
    fn debug_name(&self) -> &'static str {
        "effectcraft-input-router"
    }

    fn input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        let vid = input.viewport_id;
        if vid == ViewportId::ROOT || !self.queue.iter().any(|(v, _)| *v == vid) {
            return;
        }
        let mine: Vec<egui::Event> = self.queue.iter().filter(|(v, _)| *v == vid).map(|(_, e)| e.clone()).collect();
        let n = crate::synthetic_batch(&mine);
        let mut taken = 0;
        self.queue.retain(|(v, _)| {
            let take = *v == vid && taken < n;
            taken += usize::from(take);
            !take
        });
        input.events.extend(mine.into_iter().take(n));
    }
}

/// Whether synthetic input is still queued for an undocked window.
pub(crate) fn input_pending(ctx: &egui::Context) -> bool {
    ctx.with_plugin::<InputRouter, _>(|r| !r.queue.is_empty()).unwrap_or(false)
}

/// Queue synthetic pointer input for the undocked window of group `window`, or the main window
/// (`None`, or a window that isn't open). Later key input follows it.
pub(crate) fn send_pointer(app: &mut EffectcraftApp, ctx: &egui::Context, window: Option<u64>, events: Vec<egui::Event>) {
    let target = window.filter(|w| app.os_windows.shown.contains_key(w));
    app.os_windows.key_target = target;
    send(app, ctx, target, events);
}

/// The undocked window that has the OS focus and a focused text field, if any.
pub(crate) fn typing_window(app: &EffectcraftApp, ctx: &egui::Context) -> Option<u64> {
    let focused = ctx.input(|i| i.raw.viewports.iter().filter(|(_, v)| v.focused == Some(true)).map(|(id, _)| *id).collect::<Vec<_>>());
    app.os_windows.typing.iter().copied().find(|w| app.os_windows.shown.contains_key(w) && focused.contains(&viewport_id(*w)))
}

/// Queue synthetic key input for undocked window `window`.
pub(crate) fn send_keys_to(app: &mut EffectcraftApp, ctx: &egui::Context, window: u64, events: Vec<egui::Event>) {
    send(app, ctx, Some(window), events);
}

/// Queue synthetic key input for the window the last synthetic pointer input went to.
pub(crate) fn send_keys(app: &mut EffectcraftApp, ctx: &egui::Context, events: Vec<egui::Event>) {
    let target = app.os_windows.key_target.filter(|w| app.os_windows.shown.contains_key(w));
    send(app, ctx, target, events);
}

fn send(app: &mut EffectcraftApp, ctx: &egui::Context, target: Option<u64>, events: Vec<egui::Event>) {
    let Some(w) = target else {
        app.synthetic.extend(events);
        return;
    };
    let vid = viewport_id(w);
    ctx.add_plugin(InputRouter::default());
    let mut events = Some(events);
    ctx.with_plugin::<InputRouter, _>(|r| r.queue.extend(events.take().into_iter().flatten().map(|e| (vid, e))));
    if let Some(e) = events {
        // (No router: the main window gets it.)
        app.synthetic.extend(e);
    }
    ctx.request_repaint_of(vid);
    ctx.request_repaint();
}

/// What drawing an undocked window produced.
struct WindowOut {
    group: Group,
    tabs: Vec<(PanelKind, Rect)>,
    actions: Vec<DockAction>,
    close: bool,
    outer: Option<Rect>,
    inner: Option<Rect>,
    /// Whether the OS says the window is on a monitor.
    on_monitor: bool,
}

impl EffectcraftApp {
    /// Floating group `i` as an OS window. Returns its group and tabs in main-window
    /// coordinates (for tab drops); pushes the chrome's actions (its panel menus and tab drags
    /// are handled in the window) and, when its close button was pressed, its first panel to
    /// `closed`.
    pub(crate) fn floating_os_window(
        &mut self,
        ctx: &egui::Context,
        i: usize,
        origin: Pos2,
        info: &dock::TabInfo,
        actions: &mut Vec<DockAction>,
        closed: &mut Vec<PanelKind>,
    ) -> Option<(Group, Vec<(PanelKind, Rect)>)> {
        let f = self.ui.floating.get(i)?.clone();
        let vid = viewport_id(f.id);
        let at = *self
            .os_windows
            .created
            .entry(f.id)
            .or_insert_with(|| f.screen.filter(plausible).unwrap_or([origin.x + f.rect[0], origin.y + f.rect[1], f.rect[2], f.rect[3]]));
        let size = vec2(at[2], at[3]).max(MIN_SIZE);
        let active = f.active.min(f.panels.len().saturating_sub(1));
        let title = f.panels.get(active).map(|p| (info.title)(*p)).unwrap_or_default();
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_position(pos2(at[0], at[1]))
            .with_inner_size(size)
            .with_min_inner_size(MIN_SIZE)
            .with_window_level(self.os_windows.on_top.level())
            .with_taskbar(false);
        if self.os_windows.raise == Some(f.id) {
            self.os_windows.raise = None;
            // Only while EffectCraft is the active app (an agent's Window ▸ <panel> doesn't
            // take the user's focus).
            if ctx.input(|i| i.raw.viewports.values().any(|v| v.focused == Some(true))) {
                ctx.send_viewport_cmd_to(vid, egui::ViewportCommand::Focus);
            }
        }
        // RT10e: its window's surface is configured inside this call.
        let _red = crate::zero_copy::hold_for_new_viewport(self, ctx, vid);
        let out = ctx.show_viewport_immediate(vid, builder, |ui, _| {
            self.auto.window = Some(f.id);
            let out = self.os_window_ui(ui, i, &f, active, info);
            self.auto.window = None;
            if ui.ctx().egui_wants_keyboard_input() {
                self.os_windows.typing.insert(f.id);
            } else {
                self.os_windows.typing.remove(&f.id);
            }
            out
        });
        actions.extend(out.actions);
        if out.close
            && let Some(p) = f.panels.first()
        {
            closed.push(*p);
        }
        let inner = out.inner.unwrap_or_else(|| Rect::from_min_size(pos2(at[0], at[1]), size));
        self.os_windows.building.insert(f.id, inner);
        // Remember where it is (moved or resized by the user, or placed by the OS).
        let outer_min = out.outer.map_or(inner.min, |o| o.min);
        let screen = [outer_min.x, outer_min.y, inner.width(), inner.height()];
        if let Some(fl) = self.ui.floating.get_mut(i).filter(|fl| fl.id == f.id)
            && fl.screen.is_none_or(|s| s.iter().zip(screen).any(|(a, b)| (a - b).abs() > 0.5))
        {
            fl.screen = Some(screen);
            fl.rect = [inner.min.x - origin.x, inner.min.y - origin.y, inner.width(), inner.height()];
        }
        // Once it is open: a window on no monitor (saved on one that is gone) moves over the main
        // window. (macOS says so reliably; Windows always names the nearest monitor.)
        if out.outer.is_some() && self.os_windows.checked.insert(f.id) && !out.on_monitor {
            // (Only when the main window is on a known monitor itself: some docked displays have
            // no monitor id, and then nothing is known.)
            let main = ctx.input(|i| i.raw.viewports.get(&ViewportId::ROOT).filter(|v| v.monitor_size.is_some()).and_then(|v| v.inner_rect));
            if let Some(main) = main {
                let r = rescue(main, Rect::from_min_size(pos2(at[0], at[1]), size));
                // (The builder's new position and size move the window.)
                self.os_windows.created.insert(f.id, [r.min.x, r.min.y, r.width(), r.height()]);
                if let Some(fl) = self.ui.floating.get_mut(i).filter(|fl| fl.id == f.id) {
                    fl.screen = Some([r.min.x, r.min.y, r.width(), r.height()]);
                    fl.rect = [r.min.x - origin.x, r.min.y - origin.y, r.width(), r.height()];
                }
                ctx.request_repaint();
            }
        }
        let off = inner.min - origin;
        let g = Group { rect: out.group.rect.translate(off), content: out.group.content.translate(off), ..out.group };
        Some((g, out.tabs.into_iter().map(|(p, r)| (p, r.translate(off))).collect()))
    }

    /// The inside of an undocked window: the group's tab strip and shown panel.
    fn os_window_ui(&mut self, ui: &mut egui::Ui, i: usize, f: &Floating, active: usize, info: &dock::TabInfo) -> WindowOut {
        let t = self.tokens;
        let ctx = ui.ctx().clone();
        if self.dialog.is_some() {
            ui.disable();
        }
        // Keys pressed in this window (it has the keyboard focus).
        crate::menus::handle_shortcuts(self, &ctx);
        let r = ui.max_rect();
        ui.painter().rect_filled(r, 0.0, t.app_bg);
        // Drag the empty part of the tab strip to move the window (tabs take precedence).
        let strip_h = f.strip_h(&t);
        let strip = Rect::from_min_size(r.min, vec2(r.width(), strip_h));
        let mv = ui.interact(strip, egui::Id::new(("floating-os-panel", f.id, "move")), Sense::drag());
        self.auto.add(&format!("panel.float.{i}.move"), strip, "Move floating panel");
        if mv.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let g = Group {
            path: format!("f{i}"),
            rect: r,
            content: Rect::from_min_max(pos2(r.min.x, r.min.y + strip_h), r.max),
            panels: f.panels.clone(),
            active,
            stacked: None,
        };
        let c = dock::draw_group_chrome(ui, &g, &t, &mut self.auto, info);
        // Menus open, and tab drags start, in this window; the rest is the dock's.
        let mut actions = vec![];
        for a in c.actions {
            match a {
                DockAction::PanelMenu(p, pos) => crate::panels::panel_menu::open(&ctx, p, self.shown_doc(p), pos),
                DockAction::TabMenu(p, doc, pos) => crate::panels::panel_menu::open(&ctx, p, doc, pos),
                DockAction::BeginDrag(p) => {
                    ctx.data_mut(|d| d.insert_temp(crate::dock_ui::drag_id(), p));
                    self.os_windows.drag_from = Some(f.id);
                    self.os_windows.drag_feed = None;
                }
                a => actions.push(a),
            }
        }
        if let Some(p) = g.panels.get(g.active).copied() {
            self.panel_body(ui, p, g.content);
        }
        if g.panels.get(g.active) == Some(&self.ui.focused) {
            ui.painter().rect_stroke(r, 0.0, Stroke::new(1.0, t.focus), StrokeKind::Inside);
        }
        crate::panels::panel_menu::show(self, ui);
        self.feed_tab_drag(ui, f.id);
        self.draw_drag_overlay(&ctx, f.id);
        let (close, outer, inner, on_monitor) =
            ui.input(|i| (i.viewport().close_requested(), i.viewport().outer_rect, i.viewport().inner_rect, i.viewport().monitor_size.is_some()));
        WindowOut { group: g, tabs: c.tabs, actions, close, outer, inner, on_monitor }
    }

    /// A tab drag that started in this window: this window has the pointer, so it tells the
    /// dock where it is (on screen). (The window under the pointer draws the dragged tab.)
    fn feed_tab_drag(&mut self, ui: &egui::Ui, window: u64) {
        if self.os_windows.drag_from != Some(window) {
            return;
        }
        let ctx = ui.ctx().clone();
        if ctx.data(|d| d.get_temp::<PanelKind>(crate::dock_ui::drag_id())).is_none() {
            return;
        }
        let (pos, released, command, inner) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.any_released(), i.modifiers.command, i.viewport().inner_rect));
        let (Some(pos), Some(inner)) = (pos, inner) else { return };
        self.os_windows.drag_feed = Some(DragFeed { screen: inner.min + pos.to_vec2(), released, command });
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        ctx.request_repaint();
    }

    /// A tab dragged over this window (from anywhere): where it will land and the dragged tab,
    /// as the dock's drop logic placed them last frame.
    fn draw_drag_overlay(&self, ctx: &egui::Context, window: u64) {
        let Some(o) = self.os_windows.drag_overlay.filter(|o| o.window == window) else { return };
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("dock-drop")));
        if let Some(m) = o.mark {
            m.paint(&painter, &self.tokens);
        }
        crate::dock_ui::drag_ghost(&painter, o.panel, o.pointer, &self.tokens);
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::{Layout, assign_floating_ids, workspace};

    #[test]
    fn floating_ids_are_unique_and_kept() {
        use PanelKind::*;
        let mut v = vec![Floating::new(vec![Info], 0, [0.0; 4]), Floating::new(vec![Audio], 0, [0.0; 4])];
        v[1].id = 7;
        assert!(assign_floating_ids(&mut v));
        assert_eq!(v.iter().map(|f| f.id).collect::<Vec<_>>(), [8, 7]);
        // Duplicates (a copied layout) get a fresh id; set ids stay.
        v.push(Floating { id: 7, ..Floating::new(vec![Preview], 0, [0.0; 4]) });
        assert!(assign_floating_ids(&mut v));
        assert_eq!(v.iter().map(|f| f.id).collect::<Vec<_>>(), [8, 7, 9]);
        assert!(!assign_floating_ids(&mut v));
        assert_ne!(viewport_id(7), viewport_id(8));
    }

    #[test]
    fn screen_rect_round_trips_and_old_layouts_load() {
        use PanelKind::*;
        let mut l = Layout { root: workspace("Standard"), floating: vec![] };
        assert!(l.float(Info, [200.0, 150.0, 320.0, 240.0]));
        assert_eq!(l.floating[0].screen, None);
        l.floating[0].id = 3;
        // An OS window on a second monitor left of the main one.
        l.floating[0].screen = Some([-1500.0, 80.0, 640.0, 480.0]);
        let s = serde_json::to_string(&l).unwrap();
        let back: Layout = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
        // A layout saved before groups had OS windows: no id, no screen rect.
        let mut old = serde_json::to_value(&l).unwrap();
        let f = old["floating"][0].as_object_mut().unwrap();
        f.remove("id");
        f.remove("screen");
        let old: Layout = serde_json::from_value(old).unwrap();
        let f = &old.floating[0];
        assert_eq!((f.id, f.screen, f.rect, &f.panels), (0, None, [200.0, 150.0, 320.0, 240.0], &vec![Info]));
    }

    #[test]
    fn undocked_windows_float_above_the_main_window_only_while_effectcraft_is_active() {
        let mut t = OnTop::default();
        assert_eq!(t.level(), egui::WindowLevel::Normal);
        // EffectCraft becomes the active app: at once.
        assert_eq!(t.update(true, 1.0), (Some(true), None));
        assert_eq!(t.level(), egui::WindowLevel::AlwaysOnTop);
        assert_eq!(t.update(true, 1.1), (None, None), "no change, nothing sent");
        // The focus moves from one of our windows to another (a frame with none focused).
        let (change, wait) = t.update(false, 1.2);
        assert_eq!(change, None);
        assert!(wait.is_some_and(|w| (w - LEVEL_DEBOUNCE).abs() < 1e-9));
        assert_eq!(t.update(false, 1.25).0, None);
        assert_eq!(t.update(true, 1.3), (None, None));
        assert!(t.on);
        // Another app becomes active: once the focus has stayed away, they are ordinary windows.
        assert_eq!(t.update(false, 2.0).0, None);
        assert_eq!(t.update(false, 2.1).0, None);
        assert_eq!(t.update(false, 2.0 + LEVEL_DEBOUNCE).0, Some(false));
        assert_eq!(t.level(), egui::WindowLevel::Normal);
        assert_eq!(t.update(false, 3.0), (None, None));
        // A clock that jumps back never keeps them above other apps.
        t.update(true, 5.0);
        t.update(false, 5.1);
        assert_eq!(t.update(false, 1.0).0, Some(false));
    }

    #[test]
    fn a_window_on_a_monitor_that_is_gone_opens_over_the_main_window() {
        let main = Rect::from_min_size(pos2(100.0, 50.0), vec2(1600.0, 1000.0));
        // Saved left of the main window, on a second monitor that was unplugged.
        let r = rescue(main, Rect::from_min_size(pos2(-1500.0, 80.0), vec2(640.0, 480.0)));
        assert_eq!(r, Rect::from_min_size(pos2(100.0, 80.0), vec2(640.0, 480.0)));
        // Below and right of it: kept whole inside.
        let r = rescue(main, Rect::from_min_size(pos2(5000.0, 3000.0), vec2(640.0, 480.0)));
        assert_eq!(r, Rect::from_min_size(pos2(1060.0, 570.0), vec2(640.0, 480.0)));
        // Larger than the main window: shrunk to it.
        let r = rescue(main, Rect::from_min_size(pos2(-3000.0, 0.0), vec2(4000.0, 3000.0)));
        assert_eq!(r, main);
        // A tiny main window: the smallest window, at its corner.
        let small = Rect::from_min_size(pos2(10.0, 10.0), vec2(50.0, 40.0));
        assert_eq!(rescue(small, Rect::from_min_size(pos2(900.0, 900.0), vec2(300.0, 200.0))), Rect::from_min_size(small.min, MIN_SIZE));
        // Nonsense: at the main window's corner.
        let nan = Rect::from_min_size(pos2(f32::NAN, 0.0), vec2(300.0, 200.0));
        assert_eq!(rescue(main, nan), Rect::from_min_size(main.min, MIN_SIZE));
        // A main window rect the platform got wrong doesn't panic.
        let odd = Rect::from_min_size(pos2(f32::NAN, 0.0), vec2(f32::NAN, 100.0));
        let _ = rescue(odd, Rect::from_min_size(pos2(10.0, 10.0), vec2(300.0, 200.0)));
        // Saved rects that can't be a window's are ignored before it opens.
        assert!(plausible(&[-1500.0, 80.0, 640.0, 480.0]));
        assert!(!plausible(&[f32::INFINITY, 80.0, 640.0, 480.0]));
        assert!(!plausible(&[1.0e9, 80.0, 640.0, 480.0]));
        assert!(!plausible(&[0.0, 0.0, 0.0, 480.0]));
    }

    #[test]
    fn synthetic_input_for_a_window_is_paced_like_the_main_windows() {
        let ctx = egui::Context::default();
        ctx.add_plugin(InputRouter::default());
        let v = viewport_id(1);
        let pos = pos2(10.0, 10.0);
        let click = [
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() },
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() },
        ];
        ctx.with_plugin::<InputRouter, _>(|r| r.queue.extend(click.iter().cloned().map(|e| (v, e))));
        assert!(input_pending(&ctx));
        let mut seen = 0;
        for _ in 0..5 {
            let mut raw = egui::RawInput { viewport_id: v, ..Default::default() };
            ctx.with_plugin::<InputRouter, _>(|r| egui::Plugin::input_hook(r, &ctx, &mut raw));
            assert!(raw.events.len() <= 1, "one pointer event per frame");
            seen += raw.events.len();
            // The main window's input isn't touched.
            let mut root = egui::RawInput::default();
            ctx.with_plugin::<InputRouter, _>(|r| egui::Plugin::input_hook(r, &ctx, &mut root));
            assert!(root.events.is_empty());
        }
        assert_eq!(seen, 3);
        assert!(!input_pending(&ctx));
    }
}
