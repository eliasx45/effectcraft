//! EMBED1: the Timeline panel's host interface.
//!
//! The Timeline used to take EffectCraft's whole [`EffectcraftApp`]. It now draws against a
//! [`TimelineHost`]: the engine session, the frontend's [`UiState`], the automation registry, the
//! theme tokens, command dispatch and a handful of app services (toasts, playback, scrubbing, the
//! cache bar). EffectCraft's app implements it by forwarding to what it did before, so behaviour
//! is unchanged; another egui app (FigCraft) can host the Timeline by implementing it over its own
//! session. The few app-only parts left (suggestions overlay, RAM chip, frozen-layer hint…) go
//! through [`TimelineHost::app`] and are skipped by a host without an EffectCraft app;
//! `docs/embedding-panels.md` lists each one. The Graph Editor draws in any host, and its UI-only
//! commands run through [`super::hosted`].

use effectcraft_engine::Session;
use effectcraft_engine::project::ItemId;
use effectcraft_engine::time::Tick;
use serde_json::Value;

use crate::EffectcraftApp;
use crate::automation::Registry;
use crate::state::UiState;
use crate::theme::Tokens;

/// Disjoint borrows of a host's state, for code that holds one while it uses another (the
/// Timeline's state while it reads the project, say).
pub struct TimelineParts<'a> {
    pub session: &'a mut Session,
    pub ui: &'a mut UiState,
    pub auto: &'a mut Registry,
    pub tokens: Tokens,
}

/// The frames the cache bar under the ruler shows (frame numbers, ascending).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheBar {
    /// Frames of what the viewer shows that are in memory (green).
    pub cached: Vec<i64>,
    /// Those that are drafts from a cloud playback stream (lighter green).
    pub drafts: Vec<i64>,
    /// Frames in the disk cache (blue); `None` when there is no disk cache.
    pub disk: Option<Vec<i64>>,
}

/// What the Timeline panel needs from the app it is drawn in.
///
/// The accessors are required; every service has a default that suits a host without
/// EffectCraft's playback, frame cache or dialogs (commands run on the session directly).
pub trait TimelineHost {
    fn session(&self) -> &Session;
    fn session_mut(&mut self) -> &mut Session;
    /// The frontend state: the Timeline's own (`ui().timeline`), the focused panel and the status
    /// line are what the panel uses.
    fn ui(&self) -> &UiState;
    fn ui_mut(&mut self) -> &mut UiState;
    /// The automation registry every widget registers its id and rectangle with.
    fn auto(&self) -> &Registry;
    fn auto_mut(&mut self) -> &mut Registry;
    fn tokens(&self) -> Tokens;
    fn parts(&mut self) -> TimelineParts<'_>;
    /// Salts every egui id the panel makes ([`super::ids`]), so its state and widgets never clash
    /// with the host's or another Timeline's. Give each instance its own; `None` (EffectCraft's one
    /// Timeline) keeps the plain ids.
    fn id_scope(&self) -> Option<egui::Id> {
        Some(egui::Id::new("effectcraft-hosted-timeline"))
    }

    /// Runs a command by id the way a menu item would (UI commands included). The default runs
    /// it on the session; a host that wants EffectCraft's Timeline commands (Graph Editor, zoom,
    /// reveal keys, edits in expanded precomps) runs [`super::hosted::inline`] and
    /// [`super::hosted::run`] first (see that module).
    fn invoke(&mut self, _ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
        self.session_mut().execute(id, params).map_err(|e| e.to_string())
    }
    /// Runs an engine command (errors go to the status line).
    fn run_engine(&mut self, _ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
        let r = self.session_mut().execute(id, params).map_err(|e| e.to_string());
        if let Err(e) = &r {
            self.ui_mut().status = e.clone();
        }
        r
    }
    /// A short message near the pointer ("Snapped: …"), shown from `now` (egui time).
    fn toast(&mut self, _msg: String, _now: f64) {}
    /// The modal dialog open, if any.
    fn dialog(&self) -> Option<crate::Dialog> {
        None
    }
    fn set_dialog(&mut self, _dialog: Option<crate::Dialog>) {}
    fn playing(&self) -> bool {
        false
    }
    /// Stops playback (the current time was set by hand).
    fn stop(&mut self) {}
    fn show_panel(&mut self, _panel: crate::dock::PanelKind) {}
    /// `Some(frame the audio is at)` while an audio-only preview plays.
    fn video_preview_off(&self) -> Option<Option<i64>> {
        None
    }
    /// The current time indicator is being dragged (`now`: egui time); `dragged` is false on a click.
    fn record_scrub(&mut self, _now: f64, _dragged: bool) {}
    /// Ctrl/Cmd-drag of the current time: scrub the audio at `t`.
    fn scrub_audio(&mut self, _comp: ItemId, _t: Tick, _now: f64) {}
    /// The cache bar's frames of `comp` (`frames` long).
    fn cache_bar(&self, _comp: ItemId, _frames: i64) -> CacheBar {
        CacheBar::default()
    }
    /// The per-layer render times of the frame the viewer shows (Render Time column).
    fn shown_frame_times(&self) -> Option<std::sync::Arc<crate::frames::times::FrameTimes>> {
        None
    }
    /// How long the last viewer frame took (ms), for the footer.
    fn last_frame_ms(&self) -> f64 {
        0.0
    }
    /// EffectCraft's app, for the parts of the Timeline that still need all of it (see
    /// `docs/embedding-panels.md`). `None` in another host: those parts are left out.
    fn app(&mut self) -> Option<&mut EffectcraftApp> {
        None
    }
    fn app_ref(&self) -> Option<&EffectcraftApp> {
        None
    }

    // ---- EMBED3: Effect Controls (additive; all have defaults).

    /// The frame the Composition viewer shows (Levels/Curves histograms); `None` without a viewer.
    fn viewer_pixels(&mut self) -> Option<std::sync::Arc<egui::ColorImage>> {
        None
    }
    /// An open-files dialog for `filters` (extensions); `None` when the host has none (the panel
    /// then says to set the path with `prop.set`).
    fn pick_files(&self, _filters: &[&str]) -> Option<Vec<String>> {
        None
    }
    /// Brings a panel to the front (Effect Controls after an effect is applied).
    fn raise_panel(&mut self, _panel: crate::dock::PanelKind) {}
    /// The entries of a main-menu menu (`"Effect"`) as a context menu; the chosen command.
    fn menu_contents(&mut self, _ui: &mut egui::Ui, _menu: &str) -> Option<(String, Value)> {
        None
    }
    /// One main-menu command as a context-menu entry (its label, shortcut and enabled state);
    /// sets `clicked` when chosen.
    fn menu_entry(&self, _ui: &mut egui::Ui, _command: &str, _clicked: &mut Option<(String, Value)>) {}

    // ---- EMBED4 (additive; all have defaults).

    /// A dialog the panel asks for (Marker Settings on a double-click, Composition Settings,
    /// the Gradient editor). EffectCraft opens its own; the default says it isn't available, which
    /// the panel shows on the status line.
    fn request_dialog(&mut self, _request: DialogRequest) -> Result<Value, String> {
        Err(crate::i18n::tr("This dialog is not available here").into())
    }
    /// The audio peak summary of footage `item` for the Timeline's waveform rows (`None` while it
    /// is computed). The default computes and keeps it per egui context, so any host has waveforms.
    fn waveform(&self, ctx: &egui::Context, item: ItemId) -> Option<std::sync::Arc<crate::panels::waveform::Summary>> {
        let cache = ctx.data_mut(|d| d.get_temp_mut_or_default::<crate::panels::waveform::Cache>(super::ids::tl_id("hosted-waveforms")).clone());
        crate::panels::waveform::summary_in(&cache, self.session(), ctx, item)
    }
    /// The colour under the pointer in the host's own view of the comp, for an armed eyedropper
    /// ([`crate::panels::effect_controls::hosted_pick`]); `None` when the pointer isn't over it.
    fn pointer_color(&mut self, _ctx: &egui::Context) -> Option<PointerColor> {
        None
    }
}

/// A dialog a hosted panel asks its host for ([`TimelineHost::request_dialog`]). New kinds may be
/// added; a host answers the ones it knows and errors on the rest.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DialogRequest {
    /// Composition/Layer Marker settings for a marker.
    Marker(crate::panels::markers_ui::MarkerRef),
    /// Composition Settings of the active comp, on `tab` (`None`: the one it was on).
    CompSettings { tab: Option<crate::panels::comp_settings::Tab> },
    /// The Gradient editor for `prop.editGradient` params (`{layer, prop, comp?…}`).
    Gradient(Value),
}

/// What a host's comp view shows under the pointer ([`TimelineHost::pointer_color`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerColor {
    /// The displayed colour, 0–1 per channel, as EffectCraft's viewer samples its frame.
    pub rgb: [f32; 3],
    /// The primary button was clicked there this frame (the sample is taken).
    pub clicked: bool,
}

impl TimelineHost for EffectcraftApp {
    fn session(&self) -> &Session {
        &self.session
    }
    fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }
    fn ui(&self) -> &UiState {
        &self.ui
    }
    fn ui_mut(&mut self) -> &mut UiState {
        &mut self.ui
    }
    fn auto(&self) -> &Registry {
        &self.auto
    }
    fn auto_mut(&mut self) -> &mut Registry {
        &mut self.auto
    }
    fn tokens(&self) -> Tokens {
        self.tokens
    }
    fn parts(&mut self) -> TimelineParts<'_> {
        TimelineParts { session: &mut self.session, ui: &mut self.ui, auto: &mut self.auto, tokens: self.tokens }
    }
    fn id_scope(&self) -> Option<egui::Id> {
        None
    }
    fn invoke(&mut self, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
        crate::menus::invoke(self, ctx, id, params)
    }
    fn run_engine(&mut self, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
        crate::menus::run_engine(self, ctx, id, params)
    }
    fn toast(&mut self, msg: String, now: f64) {
        self.toast = Some((msg, now));
    }
    fn dialog(&self) -> Option<crate::Dialog> {
        self.dialog
    }
    fn set_dialog(&mut self, dialog: Option<crate::Dialog>) {
        self.dialog = dialog;
    }
    fn playing(&self) -> bool {
        self.playback.playing
    }
    fn stop(&mut self) {
        EffectcraftApp::stop(self);
    }
    fn show_panel(&mut self, panel: crate::dock::PanelKind) {
        EffectcraftApp::show_panel(self, panel);
    }
    fn video_preview_off(&self) -> Option<Option<i64>> {
        EffectcraftApp::video_preview_off(self)
    }
    fn record_scrub(&mut self, now: f64, dragged: bool) {
        if !dragged {
            self.scrub_preview = Default::default();
        }
        EffectcraftApp::record_scrub(self, now);
    }
    fn scrub_audio(&mut self, comp: ItemId, t: Tick, now: f64) {
        EffectcraftApp::scrub_audio(self, comp, t, now);
    }
    fn cache_bar(&self, comp: ItemId, frames: i64) -> CacheBar {
        let series = self.shown_series(comp);
        let cached = self.frames.cached_frames(&series);
        let drafts = self.frames.draft_frames(&series);
        let disk = (self.session.disk_cache.is_some() || self.frames.remote_disk()).then(|| {
            let opts = self.frame_opts(comp, series.scale as f64 / 1000.0);
            self.frames.disk_frames(&self.render_source(), &series, frames, &opts)
        });
        CacheBar { cached, drafts, disk }
    }
    fn shown_frame_times(&self) -> Option<std::sync::Arc<crate::frames::times::FrameTimes>> {
        let key = self.viewer_shown.as_ref()?.1;
        self.frames.times.get(&key)
    }
    fn last_frame_ms(&self) -> f64 {
        self.frames.last_ms.lock().map(|v| *v).unwrap_or(0.0)
    }
    fn app(&mut self) -> Option<&mut EffectcraftApp> {
        Some(self)
    }
    fn app_ref(&self) -> Option<&EffectcraftApp> {
        Some(self)
    }
    fn viewer_pixels(&mut self) -> Option<std::sync::Arc<egui::ColorImage>> {
        EffectcraftApp::viewer_pixels(self)
    }
    fn pick_files(&self, filters: &[&str]) -> Option<Vec<String>> {
        self.hooks.pick_files.as_ref().map(|pick| pick(filters))
    }
    fn raise_panel(&mut self, panel: crate::dock::PanelKind) {
        EffectcraftApp::raise_panel(self, panel);
    }
    fn menu_contents(&mut self, ui: &mut egui::Ui, menu: &str) -> Option<(String, Value)> {
        crate::menus::menu_contents(self, ui, menu)
    }
    fn menu_entry(&self, ui: &mut egui::Ui, command: &str, clicked: &mut Option<(String, Value)>) {
        crate::menus::entry_for(self, ui, command, clicked);
    }
    fn request_dialog(&mut self, request: DialogRequest) -> Result<Value, String> {
        match request {
            DialogRequest::Marker(m) => crate::panels::markers_ui::open_dialog(self, m).map(|()| Value::Null),
            DialogRequest::CompSettings { tab } => {
                crate::panels::dialogs::open_comp_settings(self)?;
                if let Some(tab) = tab {
                    self.dialog_state.comp.tab = tab;
                }
                Ok(Value::Null)
            }
            DialogRequest::Gradient(params) => crate::panels::gradient_editor::open(self, &params),
        }
    }
    fn waveform(&self, ctx: &egui::Context, item: ItemId) -> Option<std::sync::Arc<crate::panels::waveform::Summary>> {
        crate::panels::waveform::summary(self, ctx, item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A host with only what the trait requires: no `EffectcraftApp`, no playback, frame cache,
    /// dialogs or GPU, as another egui app (FigCraft) would host the panel.
    struct FakeHost {
        session: Session,
        ui: UiState,
        auto: Registry,
        tokens: Tokens,
        commands: Vec<String>,
        pointer: Option<PointerColor>,
    }

    impl TimelineHost for FakeHost {
        fn session(&self) -> &Session {
            &self.session
        }
        fn session_mut(&mut self) -> &mut Session {
            &mut self.session
        }
        fn ui(&self) -> &UiState {
            &self.ui
        }
        fn ui_mut(&mut self) -> &mut UiState {
            &mut self.ui
        }
        fn auto(&self) -> &Registry {
            &self.auto
        }
        fn auto_mut(&mut self) -> &mut Registry {
            &mut self.auto
        }
        fn tokens(&self) -> Tokens {
            self.tokens
        }
        fn parts(&mut self) -> TimelineParts<'_> {
            TimelineParts { session: &mut self.session, ui: &mut self.ui, auto: &mut self.auto, tokens: self.tokens }
        }
        fn invoke(&mut self, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
            self.commands.push(id.to_string());
            // As a host overriding `invoke` does (FigCraft): edits of an expanded precomp, the
            // Timeline's UI commands, then the engine (redirected while an inline edit runs).
            if let Some(r) = super::super::hosted::inline(self, ctx, id, &params) {
                return r;
            }
            if let Some(r) = super::super::hosted::run(self, ctx, id, &params) {
                return r;
            }
            let (cmd, params) = super::super::hosted::engine_command(self, id, params);
            let r = self.session.execute(&cmd, params).map_err(|e| e.to_string());
            super::super::hosted::handle_events(self);
            r
        }
        fn pointer_color(&mut self, _ctx: &egui::Context) -> Option<PointerColor> {
            self.pointer
        }
    }

    fn host() -> FakeHost {
        let mut session = Session::default();
        session.execute("comp.new", json!({"name": "Hosted", "width": 320, "height": 180, "duration": 4})).unwrap();
        session.execute("layer.newSolid", json!({"name": "Plate", "color": "#406080", "width": 320, "height": 180})).unwrap();
        session.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 40, "height": 40})).unwrap();
        let ui = UiState { focused: crate::dock::PanelKind::Timeline, ..UiState::default() };
        FakeHost { session, ui, auto: Registry::default(), tokens: Tokens::for_kind(crate::theme::ThemeKind::Dark), commands: vec![], pointer: None }
    }

    fn frame(ctx: &egui::Context, host: &mut FakeHost, input: egui::RawInput) {
        let _ = ctx.run_ui(input, |ui| {
            host.auto.begin_frame();
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 400.0));
            crate::panels::timeline::show(host, ui, rect);
        });
    }

    /// EMBED1: the Timeline draws a frame (header, ruler, layer rows) against a host that is not
    /// EffectCraft's app, and its clicks reach the session through the host.
    #[test]
    fn the_timeline_draws_in_a_host_without_the_app() {
        let mut host = host();
        let ctx = egui::Context::default();
        // A host with egui's own fonts: only the families the panel names are added (EMBED2).
        crate::theme::install_panel_fonts(&ctx);
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 400.0))), ..Default::default() };
        frame(&ctx, &mut host, input());
        frame(&ctx, &mut host, input());
        assert!(host.auto.find_current("timeline.timecode").is_some(), "the header drew");
        assert!(host.auto.find_current("timeline.search").is_some(), "the search field drew");
        let names: Vec<String> = host.auto.query_current("timeline.layer.").iter().map(|e| e.id.clone()).collect();
        assert!(!names.is_empty(), "the layer rows drew: {names:?}");
        // The Motion Blur comp switch is a host-independent command.
        let [x, y, w, h] = host.auto.find_current("timeline.switch.motionBlur").map(|e| e.rect).unwrap();
        let mb = egui::pos2(x + w / 2.0, y + h / 2.0);
        let before = host.session.active_comp().unwrap().enable_motion_blur;
        let mut click = input();
        click.events = vec![
            egui::Event::PointerMoved(mb),
            egui::Event::PointerButton { pos: mb, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
        ];
        frame(&ctx, &mut host, click);
        let mut release = input();
        release.events = vec![egui::Event::PointerButton { pos: mb, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }];
        frame(&ctx, &mut host, release);
        assert_ne!(host.session.active_comp().unwrap().enable_motion_blur, before, "the switch toggled through the host's session");
    }

    /// EMBED3: Effect Controls draws the selected layer's effect against a host without the app.
    #[test]
    fn effect_controls_draw_in_a_host_without_the_app() {
        let mut host = host();
        let comp = host.session.active_comp().unwrap().clone();
        let bx = comp.layers.iter().find(|l| l.name == "Box").unwrap().id.0;
        host.session.execute("layer.select", json!({"layers": [bx]})).unwrap();
        host.session.execute("effect.apply", json!({"effect": "ec.blur.gaussian"})).unwrap();
        host.ui.focused = crate::dock::PanelKind::EffectControls;
        let ctx = egui::Context::default();
        crate::theme::install_panel_fonts(&ctx);
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                host.auto.begin_frame();
                let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 600.0));
                crate::panels::effect_controls::show(&mut host, ui, rect);
            });
        }
        let props = host.auto.query_current("effectControls.prop.");
        assert!(!props.is_empty(), "the effect's properties drew");
    }

    /// EMBED3: a hosted panel's keys run EffectCraft's bindings through the host.
    #[test]
    fn hosted_shortcuts_run_effectcrafts_bindings() {
        let mut host = host();
        let (id, _) = crate::menus::shortcut_command(&host.session, crate::dock::PanelKind::Timeline, false, egui::Key::F9, egui::Modifiers::NONE).unwrap();
        assert_eq!(id, "keys.easyEase");
        assert!(
            crate::menus::shortcut_command(&host.session, crate::dock::PanelKind::Timeline, false, egui::Key::F9, egui::Modifiers::ALT | egui::Modifiers::CTRL)
                .is_none()
        );
        let ctx = egui::Context::default();
        let press = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let input = egui::RawInput { events: vec![press(egui::Key::F9)], ..Default::default() };
        let mut ran = vec![];
        let _ = ctx.run_ui(input, |ui| ran = super::super::keys::forward_shortcuts(&mut host, ui.ctx(), crate::dock::PanelKind::Timeline));
        assert_eq!(ran, ["keys.easyEase"]);
        assert_eq!(host.commands, ["keys.easyEase"]);
    }

    /// EMBED5: the Graph Editor in a host without the app: Shift+F3 opens it (once the host
    /// installed the Timeline's UI commands), it draws the selected property's keys, and F fits
    /// its graphs.
    #[test]
    fn the_graph_editor_opens_and_draws_in_a_host_without_the_app() {
        let mut host = host();
        assert!(!super::super::hosted::installed(&host.session));
        super::super::hosted::install(&mut host.session);
        assert!(super::super::hosted::installed(&host.session));
        let comp = host.session.active_comp().unwrap().clone();
        let bx = comp.layers.iter().find(|l| l.name == "Box").unwrap();
        let (lid, uid) = (bx.id.0, bx.props.prop("transform/opacity").unwrap().uid);
        for (t, v) in [(0.0, 0.0), (2.0, 100.0)] {
            host.session.execute("prop.addKey", json!({"layer": lid, "prop": uid, "time": t, "value": v})).unwrap();
        }
        host.session.execute("layer.select", json!({"layers": [lid]})).unwrap();
        host.session.execute("prop.select", json!({"layer": lid, "prop": uid})).unwrap();
        let ctx = egui::Context::default();
        crate::theme::install_panel_fonts(&ctx);
        let screen = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 400.0))), ..Default::default() };
        let key = |key, modifiers| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let mut ran = vec![];
        let mut input = screen();
        input.events = vec![key(egui::Key::F3, egui::Modifiers::SHIFT)];
        let _ = ctx.run_ui(input, |ui| ran = super::super::keys::forward_shortcuts(&mut host, ui.ctx(), crate::dock::PanelKind::Timeline));
        assert_eq!(ran, ["timeline.graphEditor"]);
        assert!(host.ui.timeline.graph_editor, "Shift+F3 opened the Graph Editor");
        for _ in 0..3 {
            frame(&ctx, &mut host, screen());
        }
        assert!(host.auto.find_current("timeline.graph").is_some(), "the graph plot drew");
        assert!(!host.auto.query_current("timeline.graph.key.").is_empty(), "the opacity keys drew in the graph");
        let mut input = screen();
        input.events = vec![key(egui::Key::F, egui::Modifiers::NONE)];
        let _ = ctx.run_ui(input, |ui| ran = super::super::keys::forward_shortcuts(&mut host, ui.ctx(), crate::dock::PanelKind::Timeline));
        assert_eq!(ran, ["timeline.graph.fit"]);
        {
            let _ids = super::super::ids::IdScope::enter(host.id_scope());
            assert!(crate::panels::graph::take_fit_all_request(&ctx), "F asked the Graph Editor to fit, in the host's id scope");
        }
        // Closed again, F fits nothing.
        assert_eq!(super::super::hosted::run(&mut host, &ctx, "timeline.graphEditor", &json!({"value": false})), Some(Ok(json!({"graphEditor": false}))));
        assert!(super::super::hosted::run(&mut host, &ctx, "timeline.graph.fit", &json!({})).is_some_and(|r| r.is_err()));
        assert!(super::super::hosted::run(&mut host, &ctx, "layer.select", &json!({})).is_none());
    }

    /// EMBED5: the reveal keys and the time zoom keys (physical - / =) run in a host without the
    /// app: P reveals Position, Shift+T adds Opacity, Collapse All clears, = zooms, D scrolls.
    #[test]
    fn hosted_reveal_and_zoom_keys_run_without_the_app() {
        let mut host = host();
        super::super::hosted::install(&mut host.session);
        let ctx = egui::Context::default();
        let press = |key, modifiers, physical_key| egui::Event::Key { key, physical_key, pressed: true, repeat: false, modifiers };
        let mut ran = vec![];
        let input = egui::RawInput { events: vec![press(egui::Key::P, egui::Modifiers::NONE, None)], ..Default::default() };
        let _ = ctx.run_ui(input, |ui| ran = super::super::keys::forward_shortcuts(&mut host, ui.ctx(), crate::dock::PanelKind::Timeline));
        assert_eq!(ran, ["timeline.reveal.position"]);
        assert_eq!(host.ui.timeline.reveal, ["position"]);
        host.invoke(&ctx, "timeline.revealAdd.opacity", json!({})).unwrap();
        let layers: Vec<u64> = host.session.active_comp().unwrap().layers.iter().map(|l| l.id.0).collect();
        assert!(layers.iter().all(|l| host.ui.timeline.layer_reveal.get(l).is_some_and(|k| k.iter().any(|k| k == "opacity"))));
        host.invoke(&ctx, "timeline.collapseAll", json!({})).unwrap();
        assert!(host.ui.timeline.reveal.is_empty() && host.ui.timeline.layer_reveal.is_empty());
        let input = egui::RawInput { events: vec![press(egui::Key::Equals, egui::Modifiers::NONE, Some(egui::Key::Equals))], ..Default::default() };
        let _ = ctx.run_ui(input, |ui| ran = super::super::keys::forward_shortcuts(&mut host, ui.ctx(), crate::dock::PanelKind::Timeline));
        assert_eq!(ran, ["timeline.zoomIn"]);
        assert!(host.invoke(&ctx, "timeline.scrollToTime", json!({})).is_ok());
        assert!(host.ui.timeline.scroll_to.is_some());
    }

    /// EMBED5: in a host, edits of a precomp expanded in the Timeline (its source row focused)
    /// go to the precomp, as in EffectCraft; a layer command that can't be redirected is refused
    /// and the parent stays as it was.
    #[test]
    fn hosted_inline_edits_go_to_the_expanded_precomp() {
        let mut session = Session::default();
        let child = ItemId(session.execute("comp.new", json!({"name": "Source"})).unwrap()["comp"].as_u64().unwrap());
        let leaf = session.execute("layer.newSolid", json!({"name": "Leaf"})).unwrap()["layer"].as_u64().unwrap();
        let root = ItemId(session.execute("comp.new", json!({"name": "Parent"})).unwrap()["comp"].as_u64().unwrap());
        let a = session.execute("layer.addItem", json!({"item": child.0})).unwrap()["layer"].as_u64().unwrap();
        session.execute("comp.inlineEdit", json!({"root": root.0, "path": [a], "command": "layer.select", "params": {"layers": [leaf]}})).unwrap();
        let ui = UiState { focused: crate::dock::PanelKind::Timeline, ..UiState::default() };
        let mut host =
            FakeHost { session, ui, auto: Registry::default(), tokens: Tokens::for_kind(crate::theme::ThemeKind::Dark), commands: vec![], pointer: None };
        host.ui.timeline.inline_focus = Some(vec![root.0, a]);
        let ctx = egui::Context::default();
        let layers = |h: &FakeHost, c: ItemId| h.session.project.comp(c).unwrap().layers.len();
        host.invoke(&ctx, "edit.copy", json!({})).unwrap();
        assert_eq!(host.session.state.clipboard.len(), 1);
        assert_eq!(host.session.state.clipboard[0].id, effectcraft_engine::project::LayerId(leaf));
        host.invoke(&ctx, "edit.paste", json!({})).unwrap();
        assert_eq!(layers(&host, child), 2, "pasted into the precomp");
        assert_eq!(layers(&host, root), 1, "not into the parent");
        assert_eq!(host.session.active_comp_id(), Some(root), "the parent stays the Timeline's comp");
        assert!(host.invoke(&ctx, "layer.newSolid", json!({"color": "#00ff00"})).is_err(), "no target: refused");
        assert_eq!(layers(&host, root), 1);
        assert_eq!(layers(&host, child), 2);
    }

    /// EMBED4: a hosted eyedropper takes the colour the host reports under the pointer, and the
    /// panels' dialogs are requests a host may decline.
    #[test]
    fn hosted_eyedropper_and_dialog_requests() {
        let mut host = host();
        let comp = host.session.active_comp().unwrap().clone();
        let bx = comp.layers.iter().find(|l| l.name == "Box").unwrap().id.0;
        host.session.execute("layer.select", json!({"layers": [bx]})).unwrap();
        host.session.execute("effect.apply", json!({"effect": "ec.generate.fill"})).unwrap();
        let prop = host.session.active_comp().unwrap().layer(effectcraft_engine::project::LayerId(bx)).unwrap().props.prop("effects/#1/color").unwrap().uid;
        host.ui.fx_pick = Some(crate::state::FxPick { kind: "color".into(), layer: bx, prop, name: "Color".into() });
        let ctx = egui::Context::default();
        let mut armed = vec![];
        for pointer in [None, Some(PointerColor { rgb: [1.0, 0.0, 0.0], clicked: false }), Some(PointerColor { rgb: [1.0, 0.0, 0.0], clicked: true })] {
            host.pointer = pointer;
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| armed.push(crate::panels::effect_controls::hosted_pick(&mut host, ui.ctx())));
        }
        assert_eq!(armed, [true, true, false], "armed until the click takes the sample");
        assert_eq!(host.commands, ["prop.set"]);
        assert!(host.ui.fx_pick.is_none());
        let marker = crate::panels::markers_ui::MarkerRef { layer: None, index: 0 };
        assert!(host.request_dialog(DialogRequest::Marker(marker)).is_err(), "a host without dialogs declines");
    }
}
