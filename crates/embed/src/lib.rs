//! EMB1: EffectCraft's whole app inside another app's native view, in the same process.
//!
//! Ghostex's Editing mode shows EffectCraft's own panels (viewer, timeline, effect controls, the
//! menus and dialogs) beside its chat, in one window: Ghostex adds a child `NSView` with a
//! `CAMetalLayer` where the editor goes, makes a [`wgpu::Surface`] for that layer (the one
//! `unsafe` step, on the host's side) and hands it here with the view's input. This crate does
//! what eframe does for the desktop app, without winit's event loop (the host owns the run loop):
//!
//! - builds the [`Session`](effectcraft_engine::Session) and [`EffectcraftApp`] as `apps/effectcraft`
//!   does (settings, plug-ins, the project given, the person's shortcuts, sound, file dialogs);
//! - starts the same control server (`127.0.0.1:<port>`, `docs/control-protocol.md`), so agents
//!   and the MCP bridge drive this copy exactly like the desktop app;
//! - runs one egui pass per [`Embedded::run_frame`] (`App::logic` then `App::ui`, as eframe 0.36)
//!   and paints it with egui-wgpu into the host's surface; the GPU compositor shares that device
//!   (zero-copy viewer frames, RT10e) with a traffic light held red around every configure;
//! - answers `ViewportCommand::Screenshot` (the control channel's `ui.screenshot`) by reading the
//!   frame back, as eframe's painter does.
//!
//! Text and pictures are drawn by the same egui renderer at the view's backing scale, so they are
//! pixel-identical to the desktop app at the same size and scale. Child viewports (floating
//! panels, the video preview window) are embedded as egui windows inside the view
//! (`Context::set_embed_viewports`).
//!
//! Must be used from one thread (the host's main thread on macOS, where the view's events
//! arrive). No `unsafe` here: the host creates the surface.
//!
//! The host must give Rust an allocator of its own when it also runs Chromium (CEF): Chromium's
//! allocator shim takes over `malloc` and sends it to PartitionAlloc, whose address-space pool
//! runs out long before the machine's memory under EffectCraft's frame and RAM caches (27 Oct
//! 2026, Ghostex froze at ~11 GB footprint on a 64 GB Mac: PartitionAlloc's out-of-memory trap
//! fired on render threads and the crash handler then deadlocked on its lock). Ghostex sets
//! mimalloc as `#[global_allocator]` in builds with this crate.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod keys;

// The desktop app's own control server and sound output, compiled here as well instead of
// copied (neither touches the window). Moving them into a library crate is EMB2.
#[allow(dead_code)]
#[path = "../../../apps/effectcraft/src/audio_out.rs"]
mod audio_out;
#[allow(dead_code)]
#[path = "../../../apps/effectcraft/src/control_server.rs"]
mod control_server;

use std::sync::Arc;
use std::time::{Duration, Instant};

pub use eframe::{egui_wgpu, wgpu};
pub use egui;
pub use keys::{MacKey, mac_modifiers};

use effectcraft_ui_egui::EffectcraftApp;
use effectcraft_ui_egui::zero_copy::TrafficLight;
use egui_wgpu::capture::{CaptureReceiver, CaptureSender, CaptureState, capture_channel};
use serde_json::json;

/// What the host asks for.
#[derive(Clone, Debug)]
pub struct EmbedOptions {
    /// A project to open (`.ecproj` / `.ecprojx`), as `effectcraft project.ecproj` does.
    pub project: Option<String>,
    /// The control server's port. `None`: the person's setting (Settings ▸ Scripting &
    /// Expressions ▸ Let Agents Control EffectCraft, port 9877 by default), as the desktop app.
    pub control_port: Option<u16>,
    /// Let the GPU compositor share the view's device (RT10e zero-copy viewer frames).
    pub zero_copy: bool,
    /// Offer crash recovery from the shared settings folder (off while the desktop app may run
    /// beside the host: both would claim the same recovery sentinel).
    pub recovery: bool,
}

impl Default for EmbedOptions {
    fn default() -> Self {
        Self { project: None, control_port: None, zero_copy: crate::zero_copy_default(), recovery: false }
    }
}

fn zero_copy_default() -> bool {
    effectcraft_ui_egui::zero_copy::env_choice() != Some(false)
}

/// What one frame asks of the host.
#[derive(Debug, Default)]
pub struct FrameOutput {
    /// The mouse cursor to show over the view.
    pub cursor: egui::CursorIcon,
    /// Text to put on the clipboard (Copy, Cut).
    pub copied_text: Option<String>,
    /// A link to open in the browser (Help menu, Discord).
    pub open_url: Option<String>,
    /// When the app wants its next frame (`Duration::MAX`: only on input).
    pub repaint_after: Duration,
    /// The app asked to close (after a lost graphics device, or File ▸ Quit).
    pub close_requested: bool,
    /// Seconds spent waiting for a drawable (vsync) this frame.
    pub vsync_wait: f32,
}

/// The viewer spot under a point ([`Embedded::point_at`]).
#[derive(Clone, Debug, PartialEq)]
pub struct PointAt {
    pub comp: u64,
    pub comp_name: String,
    /// Comp time shown, in seconds.
    pub time: f64,
    /// Comp pixel.
    pub point: [f64; 2],
    /// The topmost layer drawn there: id and name.
    pub layer: Option<(u64, String)>,
}

/// A wgpu instance for the host to make its surface with (Metal only on macOS).
pub fn new_instance() -> wgpu::Instance {
    let setup = egui_wgpu::WgpuSetup::CreateNew(setup());
    pollster::block_on(setup.new_instance())
}

/// The device setup the desktop app uses (`apps/effectcraft/src/main.rs`, `wgpu_options`): the
/// adapter's own limits (textures capped at 16384 px), memory hints for the compositor, no
/// indirect-call validation.
fn setup() -> egui_wgpu::WgpuSetupCreateNew {
    // macOS needs no display handle (only Wayland with GLES does).
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    #[cfg(target_os = "macos")]
    {
        setup.instance_descriptor.backends = wgpu::Backends::METAL;
    }
    setup.instance_descriptor.flags = setup.instance_descriptor.flags.difference(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL).with_env();
    setup.device_descriptor = Arc::new(|adapter| wgpu::DeviceDescriptor {
        label: Some("EffectCraft device (embedded)"),
        required_limits: wgpu::Limits { max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d.min(16384), ..adapter.limits() },
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    });
    setup
}

/// EffectCraft running inside a host view. See the crate docs.
pub struct Embedded {
    app: EffectcraftApp,
    frame: eframe::Frame,
    ctx: egui::Context,
    surface: wgpu::Surface<'static>,
    rs: egui_wgpu::RenderState,
    light: Option<Arc<TrafficLight>>,
    needs_configure: bool,
    /// Drawable size in pixels and pixels per point.
    size: [u32; 2],
    ppp: f32,
    events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    focused: bool,
    system_theme: Option<egui::Theme>,
    beginning: Instant,
    capture: Option<CaptureState>,
    capture_tx: CaptureSender,
    capture_rx: CaptureReceiver,
    control_port: Option<u16>,
    status: String,
    /// False while the host hides the view: frames still run (the control channel, playback
    /// state and agents keep working) but nothing is tessellated or drawn.
    painting: bool,
    /// Texture changes from frames that weren't painted, uploaded with the next painted one.
    pending_textures: egui::TexturesDelta,
}

impl Embedded {
    /// Start EffectCraft drawing into `surface` (made from `instance`, see [`new_instance`]),
    /// `size` pixels at `pixels_per_point`. `wake` is called from any thread when the app wants a
    /// frame (`delay` from now): the host then calls [`Embedded::run_frame`] on its thread.
    pub fn start(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        size: [u32; 2],
        pixels_per_point: f32,
        opts: EmbedOptions,
        wake: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Result<Self, String> {
        // Process-wide setup the desktop app's main does first. Each is a no-op when already done
        // (a logger the host installed keeps its place). No session log file: it would rotate
        // the desktop app's `Logs/effectcraft.log`.
        effectcraft_engine::logging::install();
        effectcraft_engine::render::concurrency::init_global_pool();
        effectcraft_engine::text::fonts::scan_system_in_background();
        let config = egui_wgpu::WgpuConfiguration { wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup()), ..Default::default() };
        let rs = pollster::block_on(egui_wgpu::RenderState::create(&config, &instance, Some(&surface), egui_wgpu::RendererOptions::default()))
            .map_err(|e| format!("EffectCraft found no usable GPU for the embedded view: {e}"))?;
        log::info!("embedded EffectCraft: graphics {:?}, surface format {:?}", rs.adapter.get_info(), rs.target_format);

        let ctx = egui::Context::default();
        // Floating panels and the video preview window become windows inside the view: the host
        // has no second native window for them.
        ctx.set_embed_viewports(true);
        ctx.set_request_repaint_callback(move |info| wake(info.delay));

        let gpu_failures = effectcraft_ui_egui::gpu_failure::GpuFailureBridge::new(&ctx);
        {
            let errors = gpu_failures.clone();
            rs.device.on_uncaptured_error(Arc::new(move |error| {
                let message = format!("uncaptured GPU error: {error}");
                if errors.report(&message, false) {
                    log::error!("{message}");
                }
            }));
            let lost = gpu_failures.clone();
            rs.device.set_device_lost_callback(move |reason, message| {
                lost.report(&format!("GPU device lost ({reason:?}): {message}"), true);
            });
        }

        let mut session = effectcraft_host::session();
        session.check_footage_on_open = true;
        session.autosave.background = true;
        if let Some(dir) = effectcraft_host::config_dir() {
            let plugins = dir.join("Plug-ins");
            if plugins.is_dir() {
                let _ = session.execute("effect.plugins.load", json!({"folder": plugins.to_string_lossy()}));
            }
            session.config = Some(Arc::new(effectcraft_engine::config::DirConfig::new(dir)));
        }
        session.load_settings();
        let agent = &session.prefs.scripting;
        let listen_port = opts.control_port.or_else(|| agent.agent_control.then(|| u16::try_from(agent.agent_control_port).ok()).flatten());
        let recovery = if opts.recovery { session.begin_recovery() } else { None };
        if let Some(p) = &opts.project
            && let Err(e) = session.execute("file.open", json!({"path": p}))
        {
            log::warn!("{p}: {e}");
        }
        let _ = session.execute("window.restoreScriptPanels", json!({}));

        let mut app = EffectcraftApp::new(session);
        app.set_gpu_failure_bridge(gpu_failures);
        let light = opts.zero_copy.then(TrafficLight::new);
        if let Some(light) = &light {
            app.share_display_device(Some(light.clone()));
        }
        app.ui.start_screen = false;
        if let Some(r) = recovery {
            app.offer_recovery(r);
        }
        hooks(&mut app);

        let mut status = String::new();
        let mut control_port = None;
        if let Some(port) = listen_port {
            let (tx, rx) = std::sync::mpsc::channel();
            match control_server::start(port, ctx.clone(), tx) {
                Ok(started) => {
                    log::info!("embedded EffectCraft: control server on 127.0.0.1:{}", started.port);
                    if let Some(dir) = effectcraft_host::config_dir() {
                        effectcraft_engine::hosts::announce(&dir, "embedded", Some(started.port));
                    }
                    control_port = Some(started.port);
                    app = app.with_control_events(started.events);
                }
                Err(e) => {
                    log::error!("{e}");
                    app.ui.status = e.clone();
                    status = e;
                }
            }
            app = app.with_control(rx);
        }

        let mut frame = eframe::Frame::_new_kittest();
        frame.wgpu_render_state = Some(rs.clone());
        let (capture_tx, capture_rx) = capture_channel();
        let ppp = if pixels_per_point.is_finite() { pixels_per_point.clamp(0.5, 8.0) } else { 2.0 };
        Ok(Self {
            app,
            frame,
            ctx,
            surface,
            rs,
            light,
            needs_configure: true,
            size: clamp_size(size),
            ppp,
            events: Vec::new(),
            modifiers: egui::Modifiers::default(),
            focused: false,
            system_theme: None,
            beginning: Instant::now(),
            capture: None,
            capture_tx,
            capture_rx,
            control_port,
            status,
            painting: true,
            pending_textures: egui::TexturesDelta::default(),
        })
    }

    /// The port the control server listens on (`None`: off, or the port was taken, see
    /// [`Embedded::status`]).
    pub fn control_port(&self) -> Option<u16> {
        self.control_port
    }

    /// Why the control server didn't start, or empty.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The egui context (for the host's diagnostics; input goes through [`Embedded::push`]).
    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// The app's measured UI frame rate (the desktop app's fps counter).
    pub fn fps(&self) -> f32 {
        self.app.fps
    }

    /// The modifiers held, as last reported.
    pub fn modifiers(&self) -> egui::Modifiers {
        self.modifiers
    }

    /// What is under `pos` (points in the view) in the Composition viewer: the comp, the time
    /// shown and the comp pixel, and the topmost layer drawn there (`layer.at`, ED1), read in
    /// process (no control round trip, well under a frame). `None` off the comp picture. For the
    /// host's "point at the frame" chips.
    pub fn point_at(&mut self, pos: egui::Pos2) -> Option<PointAt> {
        let [x, y] = effectcraft_ui_egui::panels::viewer::screen_to_comp(&self.ctx, pos)?;
        let session = &mut self.app.session;
        let comp_id = session.active_comp_id()?;
        let comp = session.active_comp()?;
        if !(x >= 0.0 && y >= 0.0 && x < f64::from(comp.width) && y < f64::from(comp.height)) {
            return None;
        }
        let comp_name = session.project.item(comp_id).map(|i| i.name.clone()).unwrap_or_default();
        let time = session.time().seconds();
        let hit = session.execute("layer.at", json!({"comp": comp_id.0, "time": time, "x": x, "y": y})).ok();
        let layer = hit.as_ref().and_then(|v| Some((v["layer"]["id"].as_u64()?, v["layer"]["name"].as_str()?.to_string())));
        Some(PointAt { comp: comp_id.0, comp_name, time, point: [x, y], layer })
    }

    /// The view changed size or moved to a display with another scale.
    pub fn resize(&mut self, size: [u32; 2], pixels_per_point: f32) {
        let size = clamp_size(size);
        let ppp = if pixels_per_point.is_finite() { pixels_per_point.clamp(0.5, 8.0) } else { self.ppp };
        if size != self.size || (ppp - self.ppp).abs() > f32::EPSILON {
            self.size = size;
            self.ppp = ppp;
            self.needs_configure = true;
            self.ctx.request_repaint();
        }
    }

    /// Keyboard focus moved to (or away from) the view.
    pub fn set_focused(&mut self, focused: bool) {
        if focused != self.focused {
            self.focused = focused;
            self.events.push(egui::Event::WindowFocused(focused));
            self.ctx.request_repaint();
        }
    }

    /// The host's light/dark appearance (Settings ▸ Appearance ▸ Sync with System follows it).
    pub fn set_system_theme(&mut self, theme: Option<egui::Theme>) {
        self.system_theme = theme;
    }

    /// One input event, in points relative to the view's top-left corner.
    pub fn push(&mut self, event: egui::Event) {
        match &event {
            egui::Event::ModifiersChanged(m) => self.modifiers = *m,
            egui::Event::Key { modifiers, .. } | egui::Event::PointerButton { modifiers, .. } | egui::Event::MouseWheel { modifiers, .. } => {
                self.modifiers = *modifiers;
            }
            _ => {}
        }
        self.events.push(event);
        self.ctx.request_repaint();
    }

    /// A key from AppKit, turned into egui events as egui-winit does (logical key from the
    /// layout, so AZERTY letters are their own letters; ⌘C/⌘X/⌘V also as Copy/Cut/Paste).
    /// `clipboard` is read only for ⌘V.
    pub fn push_mac_key(&mut self, key: MacKey<'_>, clipboard: impl FnOnce() -> Option<String>) {
        for e in keys::events(&key, clipboard) {
            self.push(e);
        }
    }

    /// Pause (`false`) or resume drawing while the host hides the view. A paused
    /// [`Embedded::run_frame`] still runs the app's logic and UI pass (control requests, agents,
    /// playback state) but draws nothing, and keeps egui's texture changes for the next drawn
    /// frame; a screenshot request is still drawn. The view comes back exactly as it was.
    pub fn set_painting(&mut self, painting: bool) {
        self.painting = painting;
    }

    /// Run one egui pass and draw it. Call when [`FrameOutput::repaint_after`] (or the `wake`
    /// callback) says so, or after input.
    pub fn run_frame(&mut self) -> FrameOutput {
        for (viewport_id, data, image) in self.capture_rx.try_iter() {
            let image = Arc::new(image);
            for user_data in data {
                self.events.push(egui::Event::Screenshot { viewport_id, user_data, image: image.clone() });
            }
        }
        let [w, h] = self.size;
        let points = egui::vec2(w as f32 / self.ppp, h as f32 / self.ppp);
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, points)),
            time: Some(self.beginning.elapsed().as_secs_f64()),
            focused: self.focused,
            events: std::mem::take(&mut self.events),
            max_texture_side: Some(self.rs.device.limits().max_texture_dimension_2d as usize),
            system_theme: self.system_theme,
            ..Default::default()
        };
        {
            let vp = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
            vp.native_pixels_per_point = Some(self.ppp);
            vp.focused = Some(self.focused);
            vp.inner_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, points));
        }
        eframe::App::raw_input_hook(&mut self.app, &self.ctx, &mut raw);
        let (app, frame) = (&mut self.app, &mut self.frame);
        let full = self.ctx.run_ui(raw, |ui| {
            eframe::App::logic(app, ui.ctx(), frame);
            eframe::App::ui(app, ui, frame);
        });
        let mut out = FrameOutput { cursor: full.platform_output.cursor_icon, repaint_after: Duration::MAX, ..Default::default() };
        for c in &full.platform_output.commands {
            match c {
                egui::OutputCommand::CopyText(t) => out.copied_text = Some(t.clone()),
                egui::OutputCommand::OpenUrl(u) => out.open_url = Some(u.url.clone()),
                _ => {}
            }
        }
        let mut capture_data = Vec::new();
        if let Some(vp) = full.viewport_output.get(&egui::ViewportId::ROOT) {
            out.repaint_after = vp.repaint_delay;
            for cmd in &vp.commands {
                match cmd {
                    egui::ViewportCommand::Screenshot(data) => capture_data.push(data.clone()),
                    egui::ViewportCommand::Close => out.close_requested = true,
                    _ => {}
                }
            }
        }
        let mut textures = std::mem::take(&mut self.pending_textures);
        textures.append(full.textures_delta);
        if !self.painting && capture_data.is_empty() {
            self.pending_textures = textures;
            return out;
        }
        let primitives = self.ctx.tessellate(full.shapes, full.pixels_per_point);
        out.vsync_wait = self.paint(full.pixels_per_point, &primitives, &mut textures, capture_data);
        out
    }

    /// The app is going away (the host closes the Editing view or quits): save the workspace
    /// layout, end crash recovery and leave the host list.
    pub fn shutdown(&mut self) {
        eframe::App::on_exit(&mut self.app);
        if self.control_port.is_some() {
            effectcraft_engine::hosts::withdraw();
        }
    }

    fn configure(&mut self) {
        let [w, h] = self.size;
        let Some(mut config) = self.surface.get_default_config(&self.rs.adapter, w, h) else {
            log::error!("embedded EffectCraft: the view's surface isn't supported by the adapter");
            return;
        };
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        config.format = self.rs.target_format;
        config.view_formats = vec![self.rs.target_format];
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 2;
        // The compositor may be submitting on this device: hold it while the surface configures.
        let red = self.light.as_ref().map(|l| l.red());
        self.surface.configure(&self.rs.device, &config);
        drop(red);
        self.needs_configure = false;
    }

    /// Upload textures and buffers, draw into the surface (through the capture texture when a
    /// screenshot was asked for) and present. Returns the seconds spent waiting for a drawable.
    fn paint(&mut self, ppp: f32, primitives: &[egui::ClippedPrimitive], textures: &mut egui::TexturesDelta, capture_data: Vec<egui::UserData>) -> f32 {
        let rs = self.rs.clone();
        let mut encoder = rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("embedded egui") });
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: self.size, pixels_per_point: ppp };
        let user_cmd_bufs = {
            let mut renderer = rs.renderer.write();
            #[allow(clippy::iter_over_hash_type)] // order doesn't matter
            for (id, deltas) in textures.set.drain() {
                for delta in deltas {
                    renderer.update_texture(&rs.device, &rs.queue, id, &delta);
                }
            }
            renderer.update_buffers(&rs.device, &rs.queue, &mut encoder, primitives, &screen)
        };
        if self.needs_configure {
            self.configure();
        }
        let started = Instant::now();
        let acquired = self.surface.get_current_texture();
        let vsync = started.elapsed().as_secs_f32();
        let output = match acquired {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.needs_configure = true;
                t
            }
            other => {
                log::debug!("embedded EffectCraft: no drawable ({other:?}), reconfiguring");
                self.needs_configure = true;
                self.ctx.request_repaint();
                rs.queue.submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));
                self.free(textures);
                return vsync;
            }
        };
        let capture = !capture_data.is_empty();
        let mut capture_buffer = None;
        {
            let renderer = rs.renderer.read();
            let target = if capture {
                let state = self.capture.get_or_insert_with(|| CaptureState::new(&rs.device, &output.texture));
                state.update(&rs.device, &output.texture);
                &state.texture
            } else {
                &output.texture
            };
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("embedded egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&mut pass.forget_lifetime(), primitives, &screen);
            if capture && let Some(state) = &mut self.capture {
                capture_buffer = Some(state.copy_textures(&rs.device, &output, &mut encoder));
            }
        }
        rs.queue.submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));
        self.free(textures);
        if let (Some(buffer), Some(state)) = (capture_buffer, &self.capture) {
            state.read_screen_rgba(self.ctx.clone(), buffer, capture_data, self.capture_tx.clone(), egui::ViewportId::ROOT);
        }
        rs.queue.present(output);
        vsync
    }

    fn free(&self, textures: &mut egui::TexturesDelta) {
        let mut renderer = self.rs.renderer.write();
        #[allow(clippy::iter_over_hash_type)] // order doesn't matter
        for id in textures.free.drain() {
            renderer.free_texture(&id);
        }
    }
}

/// A drawable of at least 1 × 1 and at most 16384 px a side.
fn clamp_size([w, h]: [u32; 2]) -> [u32; 2] {
    [w.clamp(1, 16384), h.clamp(1, 16384)]
}

/// The desktop app's hooks that work in any host process: file dialogs (rfd shows its own
/// panels), sound, the cursor position for drops. The macOS sheet dialogs and the native menu
/// bar are the desktop window's own and stay off.
fn hooks(app: &mut EffectcraftApp) {
    app.hooks.pick_files = Some(Box::new(|exts: &[&str]| {
        rfd::FileDialog::new().add_filter("Media", exts).pick_files().unwrap_or_default().into_iter().map(|p| p.to_string_lossy().to_string()).collect()
    }));
    app.hooks.pick_save = Some(Box::new(|name: &str| {
        rfd::FileDialog::new().add_filter("EffectCraft Project", &["ecproj"]).set_file_name(name).save_file().map(|p| p.to_string_lossy().to_string())
    }));
    app.hooks.pick_open_project = Some(Box::new(|| {
        rfd::FileDialog::new().add_filter("EffectCraft Project", &["ecproj", "ecprojx"]).pick_file().map(|p| p.to_string_lossy().to_string())
    }));
    app.hooks.pick_folder = Some(Box::new(|| rfd::FileDialog::new().pick_folder().map(|p| p.to_string_lossy().to_string())));
    app.hooks.pick_save_file = Some(Box::new(|name: &str, ext: &str| {
        let path = std::path::Path::new(name);
        let mut d = if ext.is_empty() { rfd::FileDialog::new() } else { rfd::FileDialog::new().add_filter(ext, &[ext]) };
        if let Some(dir) = path.parent().filter(|d| d.is_dir()) {
            d = d.set_directory(dir);
        }
        let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        d.set_file_name(file).save_file().map(|p| p.to_string_lossy().to_string())
    }));
    app.hooks.audio_device = Some(Box::new(audio_out::open));
    app.hooks.audio_devices = Some(Box::new(audio_out::devices));
}
