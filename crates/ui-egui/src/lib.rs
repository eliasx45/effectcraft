//! The EffectCraft egui frontend (swappable): docking panels, the Composition viewer, the
//! Timeline with property twirl-downs and keyframes, Effect Controls, Effects & Presets, Preview,
//! Info, Character/Paragraph/Align, menus, dialogs and the control channel.
//!
//! Everything goes through `effectcraft_engine::Session::execute` (or `menus::invoke` for
//! frontend-only commands) so every gesture is also available to agents.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Every `src/*.rs` file is a module, declared by build.rs (docs/architecture.md,
// "Self-registering modules"); declare one here only for another visibility or a submodule folder.
include!(concat!(env!("OUT_DIR"), "/modules.rs"));
// By hand: private (as upstream declares them).
mod expression_diagnostics;
// By hand: submodules in `i18n/`.
pub mod i18n;
// By hand: UI command area files in `menus/` (`AREAS`, see menus.rs).
pub mod frames;
mod local_assets;
pub mod menus;
// By hand: behind a feature.
#[cfg(all(feature = "offscreen", not(target_arch = "wasm32")))]
pub mod offscreen;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

pub use control::ControlRequest;
use dock::PanelKind;
use effectcraft_engine::Session;
use effectcraft_engine::project::ItemId;
use effectcraft_engine::render::RenderOpts;
use effectcraft_engine::time::Tick;
use frames::{FrameKey, Frames, RenderSource};
use serde_json::json;
use state::UiState;
use theme::Tokens;

const SCREENSHOT_TIMEOUT_S: f64 = 4.0;

/// The GPU compositor for the window's adapter. Natively on a device of its own unless the host
/// offers egui's (`share`, RT10e: then with the window's traffic light, see [`zero_copy`]).
/// `off_window`: when the window's adapter is OpenGL or a CPU device, the compositor may use a
/// better adapter next to it (frames then reach the window through the CPU).
/// Compositors set up again after failing on a device of their own, at most (CR1): a device that
/// keeps failing composites on the CPU after that.
const MAX_GPU_REBUILDS: u8 = 3;

/// A replacement compositor waits at most this long for the failed one's device to be freed.
const REBUILD_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

fn compositor_gpu(
    adapter: &eframe::wgpu::Adapter,
    device: eframe::wgpu::Device,
    queue: eframe::wgpu::Queue,
    share: Option<Option<Arc<zero_copy::TrafficLight>>>,
    off_window: bool,
) -> Result<effectcraft_gpu::Gpu, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let window = adapter.get_info();
        if off_window && effectcraft_gpu::adapter_choice::window_adapter_unfit(window.backend, window.device_type) {
            match pollster::block_on(effectcraft_gpu::Gpu::request_off_window(&window)) {
                Some(Ok(gpu)) => {
                    log::info!(
                        "GPU compositor on {} apart from the window's {} ({:?}, {:?}): frames reach the window through a CPU copy",
                        effectcraft_engine::render::Accelerator::name(&gpu),
                        window.name,
                        window.backend,
                        window.device_type
                    );
                    return Ok(gpu);
                }
                Some(Err(e)) => log::info!("GPU compositor: no device apart from the window's: {e}"),
                None => log::info!("GPU compositor: no adapter better than the window's {} ({:?}, {:?})", window.name, window.backend, window.device_type),
            }
        }
        if let Some(light) = share {
            let gpu = effectcraft_gpu::Gpu::new(adapter, device, queue)?;
            if let Some(light) = light {
                gpu.context().set_traffic_light(light);
            }
            return Ok(gpu);
        }
        let _ = (device, queue);
        pollster::block_on(effectcraft_gpu::Gpu::request_on_adapter(adapter))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (share, off_window);
        effectcraft_gpu::Gpu::new(adapter, device, queue)
    }
}

/// The compositor runs on the window's device, so its textures can be drawn without a copy.
/// A device from another `wgpu::Instance` (the compositor apart from an OpenGL or software
/// window) can compare equal to the window's: its ids are only unique within its instance.
fn shares_window_device(gpu: &effectcraft_gpu::Gpu, window_device: &eframe::wgpu::Device) -> bool {
    !gpu.context().own_instance() && gpu.device() == window_device
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[test]
fn compositor_on_its_own_instance_never_shares_the_window_device() {
    // A compositor on its own instance next to a window on llvmpipe compared equal to the
    // window's device (both the first device of their instance), so viewer frames were drawn
    // from its textures by the window's renderer: a wgpu panic on a non-existent texture view.
    let instance = eframe::wgpu::Instance::default();
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else { return };
    let Ok((device, _queue)) = pollster::block_on(adapter.request_device(&Default::default())) else { return };
    let Some(gpu) = effectcraft_gpu::Gpu::headless() else { return };
    assert!(gpu.context().own_instance());
    assert!(!shares_window_device(&gpu, &device));
    let Ok((own, own_queue)) = pollster::block_on(adapter.request_device(&Default::default())) else { return };
    let shared = effectcraft_gpu::Gpu::new(&adapter, own.clone(), own_queue).unwrap();
    assert!(shares_window_device(&shared, &own));
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[test]
fn the_window_renderer_skips_a_frame_it_cannot_stage() {
    // Out of video memory (Elias's app on a shared 3090), egui-wgpu panicked on the main thread
    // when it couldn't stage a frame's index data (renderer.rs:984) and the app died. The
    // patched copy in vendor/egui-wgpu skips that frame. A lost device fails staging the same way.
    use eframe::{egui_wgpu, wgpu};
    let instance = wgpu::Instance::default();
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else { return };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&Default::default())) else { return };
    device.on_uncaptured_error(Arc::new(|_| {}));
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut mesh = egui::Mesh::default();
    mesh.add_colored_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(10.0, 10.0)), egui::Color32::WHITE);
    let jobs = [egui::epaint::ClippedPrimitive { clip_rect: egui::Rect::EVERYTHING, primitive: egui::epaint::Primitive::Mesh(mesh) }];
    let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [64, 64], pixels_per_point: 1.0 };
    let mut encoder = device.create_command_encoder(&Default::default());
    device.destroy();
    let _ = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
    let mut pass = encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        })
        .forget_lifetime();
    renderer.render(&mut pass, &jobs, &screen);
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn compositor_shares_the_window_device_only_with_a_traffic_light() {
    // Surface::configure waits for its device to go idle. Frame workers submitting to that same
    // device during a window move made wgpu panic with GpuWaitTimeout: without the host's offer
    // the compositor gets a device of its own; with it, its submissions go through the light.
    let instance = eframe::wgpu::Instance::default();
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else { return };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&Default::default())) else { return };
    let gpu = compositor_gpu(&adapter, device.clone(), queue.clone(), None, true).unwrap();
    assert_ne!(gpu.device(), &device);
    assert!(gpu.context().traffic_light().is_none());
    let light = zero_copy::TrafficLight::new();
    let shared = compositor_gpu(&adapter, device.clone(), queue, Some(Some(light.clone())), true).unwrap();
    assert_eq!(shared.device(), &device);
    assert!(shared.context().traffic_light().is_some_and(|l| Arc::ptr_eq(l, &light)));
}
/// Seconds without a project change before paused prefetch resumes (see
/// [`EffectcraftApp::editing`]).
const EDIT_QUIET: f64 = 0.3;

/// A frame slower than this (ms) stops paused prefetch: a running background frame can't be
/// preempted, so the next frame you move to would share the CPU and GPU with it for that long.
const PAUSED_PREFETCH_MAX_MS: f64 = 250.0;

/// The longest a playing preview waits for its next UI frame (the meters, the end of the
/// preview, a frame that finished rendering without waking the UI).
const PLAYBACK_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The longest a preview with sound holds its first frame while the frames ahead render (see
/// [`Playback::preroll`]), in seconds.
const PREROLL_MAX: f64 = 2.0;

/// The repaint delay for a playing preview whose next frame is due in `secs`: the UI rebuilds
/// at the preview's frame rate instead of the display's refresh rate. A hair late rather than
/// early (an early wake changes nothing and needs another), never a sub-millisecond spin.
fn until_next_frame(secs: f64) -> std::time::Duration {
    const MIN: f64 = 0.001;
    if !secs.is_finite() {
        return PLAYBACK_MAX_WAIT;
    }
    std::time::Duration::from_secs_f64((secs + 0.0005).clamp(MIN, PLAYBACK_MAX_WAIT.as_secs_f64()))
}

/// Modal dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialog {
    About,
    NewComp,
    CompSettings,
    SolidSettings,
    CommandPalette,
    /// Settings (Preferences) on `dialog_state.settings_page`.
    Settings,
    /// Edit ▸ Keyboard Shortcuts.
    Shortcuts,
    /// A generic parameter form for a command (`dialog_state.form`).
    Form,
    /// A message (`dialog_state.info`).
    Info,
    /// View ▸ View Options.
    ViewOptions,
    CameraSettings,
    LightSettings,
    KeyVelocity,
    KeyInterpolation,
    TimeStretch,
    /// Tracker ▸ Options…
    TrackOptions,
    /// Tracker ▸ Edit Target…
    TrackTarget,
    /// Tracker ▸ Apply (Transform / Stabilize): Apply Dimensions.
    TrackApply,
    /// Crash recovery: offer the latest auto-save (`EffectcraftApp::recovery`).
    Recovery,
    /// Composition/Layer Marker (double-click a marker).
    Marker,
    /// Layer ▸ Pre-compose.
    Precompose,
    /// Layer ▸ Layer Styles ▸ Layer Style Options…
    LayerStyles,
    /// Edit ▸ Templates ▸ Render Settings… / Output Module….
    RenderTemplates,
    /// Save changes before closing the project? (`panels::unsaved`).
    UnsavedChanges,
    /// Delete Project items that compositions use? (`panels::delete_items`).
    DeleteItems,
    /// A gradient property's Gradient Editor (`panels::gradient_editor`, `prop.editGradient`).
    GradientEditor,
    /// Delete a user template from Home? (`panels::templates`).
    DeleteTemplate,
}

/// The desktop's light or dark appearance where egui cannot see it (Wayland compositors that send
/// winit no theme). `None` = the host does not know.
pub type SystemThemeFn = Box<dyn Fn(&egui::Context) -> Option<egui::Theme>>;

/// A file dialog a host shows without blocking the frame ([`Hooks::file_dialog_async`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileDialog {
    /// Files to open or import with these extensions; several when `many`.
    Open { exts: Vec<String>, many: bool },
    /// An EffectCraft project to open.
    OpenProject,
    /// A folder.
    Folder,
    /// Where to save `default` (a file name or path) with extension `ext` (empty: no filter);
    /// `project`: the project save dialog, given the file name.
    Save { default: String, ext: String, project: bool },
}

impl FileDialog {
    /// The save dialog for `default`, as [`Hooks::save_dialog`] picks it.
    pub fn save(default: &str) -> Self {
        let ext = std::path::Path::new(default).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        Self::Save { default: default.to_string(), project: ext.eq_ignore_ascii_case("ecproj"), ext }
    }
}

/// Called with the paths a [`FileDialog`] picked (empty: cancelled), from any thread.
pub type DialogDone = Box<dyn FnOnce(Vec<String>) + Send>;

/// A non-blocking file dialog's answer for the command that asked (`menus::dialog_answered`).
pub(crate) struct DialogAnswer {
    pub command: String,
    pub key: &'static str,
    pub params: serde_json::Value,
    pub paths: Vec<String>,
}

/// A sound to hear while the pointer is over its Motion Library card.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundHover {
    /// The card's id (the same key keeps playing; another restarts).
    pub key: String,
    /// The audio file on this machine (the library folder's, or the bucket's cached copy).
    pub path: String,
    /// SFX preview volume, linear 0–1.
    pub gain: f32,
}

/// Host hooks provided by the native app (file pickers etc.).
#[derive(Default)]
pub struct Hooks {
    /// The system appearance for Settings ▸ Appearance ▸ Sync with System (the desktop app reads
    /// the Linux desktop portal); without it, what egui reports.
    pub system_theme: Option<SystemThemeFn>,
    pub pick_files: Option<Box<dyn Fn(&[&str]) -> Vec<String>>>,
    pub pick_save: Option<Box<dyn Fn(&str) -> Option<String>>>,
    pub pick_open_project: Option<Box<dyn Fn() -> Option<String>>>,
    /// The OS cursor in physical screen pixels, given the window's native pixels per point
    /// (macOS reports points and scales them by it). winit reports no pointer while files are
    /// dragged in from the Finder, so a drop is placed where this says it landed
    /// ([`os_drop::drop_pointer`]); without it, where egui last saw the pointer in the window.
    pub cursor_screen_position: Option<Box<dyn Fn(f32) -> Option<(i32, i32)>>>,
    /// Opens the audio output for preview playback (the desktop app uses cpal).
    pub audio_device: Option<audio::AudioDeviceFactory>,
    /// Lists audio output devices (Settings ▸ Audio).
    pub audio_devices: Option<Box<dyn Fn() -> Vec<String>>>,
    /// Picks a folder (Settings paths).
    pub pick_folder: Option<Box<dyn Fn() -> Option<String>>>,
    /// Save dialog for other file kinds: (default name or path, extension; empty: no filter). A
    /// default path opens the dialog in its folder.
    pub pick_save_file: Option<Box<dyn Fn(&str, &str) -> Option<String>>>,
    /// The system clipboard's text (native menu Edit ▸ Paste into a text field).
    pub clipboard_text: Option<Box<dyn Fn() -> Option<String>>>,
    /// Application actions the OS performs (`app.hide`, `app.hideOthers`, `app.showAll` on
    /// macOS). Returns false when the host doesn't handle the id.
    pub app_action: Option<Box<dyn Fn(&str) -> bool>>,
    /// Shows a command's file dialog without blocking the frame and calls `done` with the
    /// answer (the host then asks for a repaint). Used in place of the blocking pickers above for
    /// the commands' dialogs (File ▸ Import, Open, Save As…) where a modal dialog's nested event
    /// loop inside a frame is unsafe: on macOS a drag over the window then aborted the app (#511).
    pub file_dialog_async: Option<Box<dyn Fn(FileDialog, DialogDone)>>,
    /// Tells the user the graphics device was lost and the window is closing, once. `saved` is
    /// the auto-save of unsaved changes (`None`: nothing was unsaved), its path or why it failed.
    pub device_lost: Option<Box<dyn Fn(Option<Result<String, String>>)>>,
}

impl Hooks {
    /// The host's save dialog for `default` (a file name or path), offering its extension: the
    /// project dialog for project files, else [`Hooks::pick_save_file`]. The project dialog is
    /// the fallback for hosts without that (the web, whose dialog only names the file): on macOS
    /// a dialog filtered to projects appends `.ecproj` to any other name (#293). `None` when the
    /// host has no save dialog; `Some(None)` when it was cancelled.
    pub fn save_dialog(&self, default: &str) -> Option<Option<String>> {
        let path = std::path::Path::new(default);
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        match (&self.pick_save_file, &self.pick_save) {
            (Some(pick), _) if !ext.eq_ignore_ascii_case("ecproj") => Some(pick(default, &ext)),
            (_, Some(pick)) => Some(pick(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
            (Some(pick), None) => Some(pick(default, &ext)),
            (None, None) => None,
        }
    }
}

/// When the project last changed and when the viewer last showed a different frame (UI seconds,
/// at the start of the UI frame that made the change): the edit → frame latency is
/// `shown_at - edit_at` once the shown frame's revision reaches the edit's.
#[derive(Default)]
pub(crate) struct EditLatency {
    pub edit_revision: u64,
    pub edit_at: f64,
    /// Revision, frame and scale (× 1000) of the frame on screen.
    pub shown: Option<(u64, i64, u32)>,
    pub shown_at: f64,
}

#[derive(Default)]
pub struct Playback {
    pub playing: bool,
    /// Wall-clock start and the comp time it corresponds to.
    pub start_wall: f64,
    pub start_time: Tick,
    pub start_frame: i64,
    /// Frames shown / dropped (for the Info/Preview readout).
    pub shown: u64,
    pub waiting: bool,
    pub fps: f64,
    /// The Preview panel shortcut that started this preview and what it plays.
    pub shortcut: effectcraft_engine::preview::PreviewShortcut,
    pub plan: Option<effectcraft_engine::preview::PreviewPlan>,
    /// Cache Before Playback: rendering the range before the clock starts.
    pub caching: bool,
    /// Resolution override while playing (pixel step), `None` = the viewer's.
    pub res: Option<u32>,
    /// Include Overlays / Include Layer Controls.
    pub overlays: bool,
    pub layer_controls: bool,
    /// Full Screen: the maximized panel to restore when the preview stops.
    pub restore_max: Option<Option<PanelKind>>,
    /// Audio-only previews: the frame under the audible sample (the current time stays put).
    pub audio_frame: Option<i64>,
    /// The preview's sound waits for frames: while the frames ahead aren't cached, every frame
    /// shows as it renders, silently; the sound starts once the rest of the preview (or what
    /// fits in the RAM preview) is cached, so it never plays over skipped frames (#103), or
    /// sooner once the silent preview keeps real time with the frames ahead cached (#208).
    pub audio_held: bool,
    /// Pre-roll (UI seconds it may last until): a preview with sound holds its first frame
    /// while the frames ahead render, then starts picture and sound together at that frame.
    /// Playing the first frames silently instead meant the sound joined late and the start of
    /// the preview was never heard. Past the deadline (frames too slow to get ahead) the held
    /// sound falls back to [`Self::audio_held`]'s silent playback.
    pub preroll: Option<f64>,
    /// When the last frames were shown (seconds, the last second's worth): the achieved rate.
    pub shown_at: std::collections::VecDeque<f64>,
    /// When playback last looked at the clock (UI seconds, [`Self::observe`]): a hold since
    /// the last frame counts against the achieved rate.
    pub observed: f64,
    /// Every frame shown since the preview started (UI seconds, frame), for `ui.inspect`'s
    /// frame-interval percentiles; capped at [`SHOWN_LOG_MAX`].
    pub log: Vec<(f64, i64)>,
    /// Play Faster (FilmCraft's Shuttle Right): ×2, ×4 or ×8 the preview's rate, its sound
    /// compressed in time at the same pitch (`audio::Stretch`); 0 = normal speed. The plan's rate and frame step are multiplied, so as many frames a second
    /// show as at normal speed.
    pub speed: u32,
}

/// Play Faster's top speed (×8, as FilmCraft's Shuttle Right).
const MAX_PLAY_SPEED: u32 = 8;

/// Frames [`Playback::log`] keeps (about 9 minutes at 30 fps).
const SHOWN_LOG_MAX: usize = 16_384;

impl Playback {
    /// A new frame went on screen at `now`.
    pub fn frame_shown(&mut self, now: f64, frame: i64) {
        self.shown += 1;
        if self.log.len() < SHOWN_LOG_MAX {
            self.log.push((now, frame));
        }
        self.shown_at.push_back(now);
        while self.shown_at.front().is_some_and(|t| now - t > 1.0) {
            self.shown_at.pop_front();
        }
    }

    /// Playback looked at the clock at `now`: frames shown more than a second ago no longer
    /// count, so the rate readout does not freeze at its last value while playback holds.
    pub fn observe(&mut self, now: f64) {
        self.observed = now;
        while self.shown_at.front().is_some_and(|t| now - t > 1.0) {
            self.shown_at.pop_front();
        }
    }

    /// Frames per second shown over the last second while the preview plays (`None` before two
    /// frames were shown, while caching, or stopped). A hold longer than a frame and a half
    /// since the last frame shown counts up to [`Self::observe`]'s time.
    pub fn achieved_fps(&self) -> Option<f64> {
        let (a, b) = (*self.shown_at.front()?, *self.shown_at.back()?);
        let frame = self.plan.map_or(1, |p| p.step).max(1) as f64 / self.fps.max(1.0);
        let end = if self.observed - b > 1.5 * frame { self.observed } else { b };
        (self.playing && !self.caching && self.shown_at.len() > 1 && b > a).then(|| (self.shown_at.len() - 1) as f64 / (end - a))
    }

    /// The preview keeps up with its frame rate (within 5 %; previews that skip frames count
    /// the frames they mean to show).
    pub fn real_time(&self) -> Option<bool> {
        let step = self.plan.map_or(1, |p| p.step).max(1) as f64;
        self.achieved_fps().map(|f| f * step >= self.fps * 0.95)
    }

    /// The preview has kept real time for the last quarter second (at least two frames) up to
    /// `now`: rendering keeps up, so sound can start without waiting for the whole preview.
    pub fn keeps_up(&self, now: f64) -> bool {
        let step = self.plan.map_or(1, |p| p.step).max(1) as f64;
        let frame = step / self.fps.max(1.0);
        let (Some(a), Some(b)) = (self.shown_at.front(), self.shown_at.back()) else { return false };
        self.shown_at.len() >= 3 && b - a >= (2.0 * frame).max(0.25) && now - b <= 2.0 * frame && self.real_time() == Some(true)
    }
}

pub struct EffectcraftApp {
    pub session: Session,
    pub ui: UiState,
    workspace_store: workspace_state::WorkspaceStore,
    pub tokens: Tokens,
    pub auto: automation::Registry,
    pub frames: Frames,
    pub playback: Playback,
    pub dialog: Option<Dialog>,
    pub hooks: Hooks,
    pub fps: f32,
    last_time: f64,
    styled: bool,
    fonts_ready: bool,
    /// The interface language the fonts were installed for; `""` until the first frame.
    styled_language: &'static str,
    pub(crate) control_rx: Option<Receiver<ControlRequest>>,
    pub(crate) deferred: Vec<(ControlRequest, f64)>,
    pub(crate) synthetic: Vec<egui::Event>,
    /// Undocked panel groups shown as OS windows (desktop multi-viewport).
    pub(crate) os_windows: float_windows::OsWindows,
    pub(crate) input_waiters: Vec<Sender<serde_json::Value>>,
    /// Control requests waiting for an offloaded job (`wait: true` in the browser).
    pub(crate) job_waiters: Vec<(control::JobWaiter, Sender<serde_json::Value>)>,
    queued_screenshots: Vec<(u64, f64, u32)>,
    /// (token, path, crop, max_side, reply, deadline)
    pending_screenshots: Vec<(u64, Option<String>, Option<[f32; 4]>, u32, Sender<serde_json::Value>, f64)>,
    /// `events.subscribe` connections (desktop control server).
    control_events: Option<control::ControlEvents>,
    /// (time, revision, selected layers) last published to [`Self::control_events`].
    events_sent: (f64, u64, Vec<effectcraft_engine::project::LayerId>),
    next_token: u64,
    pub(crate) last_ui_time: f64,
    pub(crate) scrub_preview: crate::scrub_preview::ScrubPreview,
    /// CR5: the viewer's preview from a cloud frame server (Settings ▸ Previews ▸ Preview On).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) cloud: cloud::CloudPreview,
    /// The cloud server's token and when it was read (at most every few seconds).
    #[cfg(not(target_arch = "wasm32"))]
    cloud_token: (f64, String),
    /// CR6b: a cloud preview's playback from the server's H.264 stream.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) cloud_play: cloud_stream::CloudPlayback,
    /// When the user last did something (input or an edit): Cache Frames When Idle waits for
    /// a second of quiet. (time, project revision then)
    last_activity: (f64, u64),
    /// Cache Frames When Idle waits for memory ([`idle_cache::memory_wait`]), and when it last
    /// said so.
    idle_memory_wait: (bool, f64),
    /// When the project last changed, and its revision then (see [`Self::editing`]).
    last_edit: (f64, u64),
    /// Edit → viewer frame latency, for `ui.inspect`'s `latency`.
    pub(crate) latency: EditLatency,
    pub(crate) toast: Option<(String, f64)>,
    /// Texture of the last CPU frame shown in the viewer, the key it came from and the factor it
    /// was averaged down by for the magnification (`panels::viewer::minify_factor`).
    pub(crate) viewer_tex: Option<(egui::TextureHandle, FrameKey, usize)>,
    /// View ▸ Sharp Zoom: the visible region rendered at the magnification above 100 %.
    pub(crate) sharp_zoom: sharp_zoom::SharpZoom,
    /// The frames the other (passive) Composition viewers show, by viewer id.
    pub(crate) passive_tex: std::collections::HashMap<u32, (FrameKey, panels::viewers::PassiveTexture)>,
    /// The last GPU frame shown: its egui texture id (registered with egui-wgpu), key and texture.
    pub(crate) viewer_native: Option<(egui::TextureId, FrameKey, Arc<effectcraft_gpu::DisplayFrame>)>,
    /// UP1: the FSR1 upscale of low-resolution GPU frames shown magnified (`fsr_upscale`).
    pub(crate) viewer_fsr: fsr_upscale::Upscaler,
    /// The texture the viewer draws and its frame key (CPU or GPU frame).
    pub(crate) viewer_shown: Option<(egui::TextureId, FrameKey)>,
    /// Last displayed image (Info panel colour, eyedroppers, histograms). GPU frames fill it on
    /// demand ([`EffectcraftApp::viewer_pixels`]).
    pub(crate) viewer_image: Option<Arc<egui::ColorImage>>,
    /// wasm32: an asynchronous readback of a GPU viewer frame (key asked for, result).
    viewer_readback: (Option<FrameKey>, Arc<std::sync::Mutex<Option<(FrameKey, Arc<egui::ColorImage>)>>>),
    /// egui-wgpu's device, once the first frame arrives (desktop and WebGPU).
    pub(crate) wgpu: Option<eframe::egui_wgpu::RenderState>,
    /// The GPU compositor on that device (None: no usable adapter → CPU only).
    pub(crate) gpu: Option<effectcraft_gpu::Gpu>,
    /// RT10e: whether the compositor shares egui's device (frames drawn without a copy).
    pub(crate) zero_copy: zero_copy::ZeroCopy,
    /// The compositor may leave a window adapter that is OpenGL or a CPU device for a better one
    /// (see [`Self::keep_compositor_on_window_adapter`]).
    compositor_off_window: bool,
    gpu_checked: bool,
    #[cfg(not(target_arch = "wasm32"))]
    gpu_startup_deferred: bool,
    #[cfg(not(target_arch = "wasm32"))]
    gpu_startup: Option<gpu_startup::Pending<effectcraft_gpu::Gpu>>,
    #[cfg(not(target_arch = "wasm32"))]
    gpu_prewarm: gpu_startup::Latest<(u64, effectcraft_project::ItemId)>,
    #[cfg(not(target_arch = "wasm32"))]
    gpu_startup_key: Option<(u64, effectcraft_project::ItemId)>,
    gpu_failures: Option<gpu_failure::GpuFailureBridge>,
    gpu_failure_applied: Option<bool>,
    /// CR1: compositors set up again after a failure on a device of their own (at most
    /// [`MAX_GPU_REBUILDS`]), and whether the next [`Self::init_gpu`] replaces a failed one.
    gpu_rebuilds: u8,
    gpu_rebuilding: bool,
    /// The failed compositor's device and when it failed: its replacement waits until it is
    /// gone (its video memory freed), at most [`REBUILD_WAIT`].
    gpu_failed_device: Option<(std::sync::Weak<effectcraft_gpu::GpuContext>, web_time::Instant)>,
    /// The graphics device was lost and the window is closing (see `close_after_device_loss`);
    /// `true` keeps crash recovery for the next launch.
    device_lost_close: Option<bool>,
    /// Answers of file dialogs shown without blocking ([`Hooks::file_dialog_async`]).
    pub(crate) dialog_answers: (Sender<DialogAnswer>, Receiver<DialogAnswer>),
    /// Commands from the native menu bar.
    pub command_inbox: Option<Receiver<String>>,
    /// Pointer position in comp pixels (Info panel).
    pub(crate) pointer_comp: Option<[f32; 2]>,
    pub(crate) dialog_state: panels::dialogs::DialogState,
    pub integrated_titlebar: bool,
    /// Audio preview while playing (audio clock drives playback).
    pub audio: Option<audio::AudioPlayback>,
    /// Audio scrubbing's open output (Ctrl/Cmd-drag the current time, `playback.scrubAudio`).
    pub scrub: Option<audio::AudioScrub>,
    /// Hover-to-hear in the Motion Library: its open output (see [`audio::SoundPreview`]).
    pub sound_preview: Option<audio::SoundPreview>,
    /// The Sound card under the pointer this frame (the panel sets it; each frame takes it).
    pub sound_hover: Option<SoundHover>,
    /// Audio panel VU meters.
    pub meter: audio::Meter,
    /// Waveform peak summaries per footage item (see `panels::waveform`).
    pub(crate) expression_diagnostics: expression_diagnostics::Diagnostics,
    pub(crate) waveforms: panels::waveform::Cache,
    /// Last reveal shortcut and when (double-press shortcuts such as LL).
    pub(crate) last_reveal: Option<(String, f64)>,
    /// Settings revision applied to the theme, tooltips and caches.
    /// The settings revision and system appearance (light?) last applied.
    applied_prefs: Option<(u64, Option<bool>)>,
    /// A previous run that didn't exit cleanly (shown by `Dialog::Recovery`).
    pub recovery: Option<effectcraft_engine::autosave::Recovery>,
    /// The Figma bridge's one-time pairing prompt (`ui.figmaPair`).
    pub(crate) figma_pair: Option<panels::figma_pair::Prompt>,
    /// Docked groups laid out last frame: (active panel, group rect) — `~` maximizes the one
    /// under the pointer.
    pub(crate) dock_rects: Vec<(PanelKind, egui::Rect)>,
    /// Home screen: recent-project thumbnail textures by path (None = no thumbnail).
    pub(crate) home_thumbs: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    /// The (project path, saved revision) whose thumbnail was stored last.
    pub(crate) home_thumb_saved: Option<(String, u64)>,
    /// The window title last sent (project name, `*` while modified).
    window_title: String,
    /// Home ▸ Templates: thumbnail textures by template id (None = not renderable).
    pub(crate) template_thumbs: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    /// Home ▸ Templates: the gallery, and the user template files it was listed from.
    pub(crate) template_list: Option<(Vec<String>, Vec<effectcraft_engine::templates::TemplateInfo>)>,
    /// The Timeline column layout last kept in the settings store (see
    /// [`panels::timeline_columns::store_layout`]).
    pub(crate) timeline_layout_saved: Option<panels::timeline_columns::ColumnLayout>,
    /// The Motion Library display settings last written to the settings store
    /// ([`panels::motion_library::store_display`]).
    pub(crate) motion_library_saved: Option<panels::motion_library::ViewState>,
}

/// The saved workspaces in the settings store ([`effectcraft_engine::config::ConfigStore`]).
pub const WORKSPACES_FILE: &str = "workspaces.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedWorkspaces {
    #[serde(default)]
    workspaces: std::collections::BTreeMap<String, dock::DockNode>,
    #[serde(default)]
    floating: std::collections::BTreeMap<String, Vec<dock::Floating>>,
}

impl EffectcraftApp {
    pub fn new(session: Session) -> Self {
        let mut ui = UiState::default();
        // The Graph Editor opens on the graph type last chosen (Preferences `graphType`).
        ui.timeline.graph_mode = session.prefs.graph_type.clone();
        let workspace_store = workspace_state::WorkspaceStore::load(session.config.as_deref(), &mut ui);
        EffectcraftApp {
            session,
            ui,
            workspace_store,
            tokens: Tokens::for_kind(Default::default()),
            auto: Default::default(),
            frames: Frames::default(),
            playback: Playback::default(),
            dialog: None,
            hooks: Hooks::default(),
            fps: 60.0,
            last_time: 0.0,
            styled: false,
            fonts_ready: false,
            styled_language: "",
            control_rx: None,
            deferred: vec![],
            synthetic: vec![],
            os_windows: Default::default(),
            input_waiters: vec![],
            job_waiters: vec![],
            queued_screenshots: vec![],
            pending_screenshots: vec![],
            control_events: None,
            events_sent: (f64::NEG_INFINITY, 0, vec![]),
            next_token: 1,
            scrub_preview: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            cloud: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            cloud_token: (f64::NEG_INFINITY, String::new()),
            #[cfg(not(target_arch = "wasm32"))]
            cloud_play: Default::default(),
            last_ui_time: 0.0,
            last_activity: (0.0, 0),
            idle_memory_wait: (false, f64::NEG_INFINITY),
            last_edit: (0.0, 0),
            latency: Default::default(),
            toast: None,
            viewer_tex: None,
            sharp_zoom: Default::default(),
            passive_tex: std::collections::HashMap::new(),
            viewer_native: None,
            viewer_fsr: Default::default(),
            viewer_shown: None,
            viewer_image: None,
            viewer_readback: Default::default(),
            wgpu: None,
            gpu: None,
            zero_copy: Default::default(),
            compositor_off_window: true,
            gpu_checked: false,
            #[cfg(not(target_arch = "wasm32"))]
            gpu_startup_deferred: false,
            #[cfg(not(target_arch = "wasm32"))]
            gpu_startup: None,
            #[cfg(not(target_arch = "wasm32"))]
            gpu_prewarm: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            gpu_startup_key: None,
            gpu_failures: None,
            gpu_failure_applied: None,
            gpu_rebuilds: 0,
            gpu_rebuilding: false,
            gpu_failed_device: None,
            device_lost_close: None,
            dialog_answers: std::sync::mpsc::channel(),
            command_inbox: None,
            pointer_comp: None,
            dialog_state: Default::default(),
            integrated_titlebar: false,
            audio: None,
            scrub: None,
            sound_preview: None,
            sound_hover: None,
            meter: Default::default(),
            expression_diagnostics: Default::default(),
            waveforms: Default::default(),
            last_reveal: None,
            applied_prefs: None,
            recovery: None,
            figma_pair: None,
            dock_rects: vec![],
            home_thumbs: Default::default(),
            home_thumb_saved: None,
            window_title: String::new(),
            template_thumbs: Default::default(),
            template_list: None,
            timeline_layout_saved: None,
            motion_library_saved: None,
        }
        .with_ui_commands()
        .with_timeline_layout()
        .with_memory_map()
    }

    /// Lend the RAM preview to the engine's memory map (Window ▸ Memory, `memory.stats`).
    fn with_memory_map(mut self) -> Self {
        panels::memory_panel::install(&mut self);
        self
    }

    /// The Timeline column layout (columns shown, order, widths, splitter) of the last session.
    fn with_timeline_layout(mut self) -> Self {
        panels::timeline_columns::load_layout(&mut self);
        panels::motion_library::load_display(&mut self);
        self
    }

    /// Keep the saved workspaces for the next session (see [`EffectcraftApp::new`]): After
    /// Effects' saved workspaces last until they are deleted (#320). `Err` when they weren't
    /// written (#696): the change applies for this session and a later save retries it, but until
    /// then a restart brings back what is on disk.
    pub(crate) fn store_saved_workspaces(&mut self) -> Result<(), String> {
        let Err(error) = self.workspace_store.save_now(self.session.config.as_deref(), &self.ui) else { return Ok(()) };
        let message = format!("Workspace changes couldn't be saved and may be lost after a restart: {error}");
        log::warn!("{message}");
        self.ui.status = message.clone();
        // The raw error, so the background retry doesn't report the same failure again.
        self.workspace_store.error = Some(error);
        Err(message)
    }

    /// Offer the frontend's commands (tools, timeline navigation…) for keyboard shortcuts.
    fn with_ui_commands(mut self) -> Self {
        let cmds = menus::UI_COMMANDS
            .iter()
            .map(|c| effectcraft_engine::shortcuts::UiCommand { id: c.id.into(), label: c.label.into(), shortcut: c.shortcut.map(str::to_string) })
            .collect();
        self.session.set_ui_commands(cmds);
        self
    }

    /// Show the crash-recovery dialog for a previous run that didn't exit cleanly.
    pub fn offer_recovery(&mut self, r: effectcraft_engine::autosave::Recovery) {
        if r.autosave.is_some() {
            self.recovery = Some(r);
            self.dialog = Some(Dialog::Recovery);
        }
    }

    /// Apply changed settings: theme and brightness, label colours, tool tips, preview caches.
    pub fn apply_prefs(&mut self, ctx: &egui::Context) {
        // Sync with System re-applies when the system appearance changes (the host wakes the UI;
        // nothing polls).
        let system = if self.session.prefs.appearance.appearance_mode == "auto" { self.system_light(ctx) } else { None };
        if self.applied_prefs == Some((self.session.prefs_revision, system)) {
            return;
        }
        self.applied_prefs = Some((self.session.prefs_revision, system));
        let p = &self.session.prefs;
        self.ui.theme = theme::ThemeKind::from_name(p.appearance.resolved_theme(system)).unwrap_or_default();
        self.tokens = Tokens::from_prefs(p, self.ui.theme);
        theme::apply_visuals(ctx, &self.tokens);
        ctx.set_theme(theme::theme_preference(&p.appearance.appearance_mode, self.ui.theme.is_light()));
        let tips = p.general.show_tool_tips;
        ctx.all_styles_mut(|s| s.interaction.tooltip_delay = if tips { 0.5 } else { f32::INFINITY });
        // Settings ▸ Appearance ▸ UI Scale (#284). egui's own Ctrl+= / Ctrl+- / Ctrl+0 zoom stays
        // off: Ctrl+0 is Window ▸ Project, and the scale is a setting.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        ctx.set_zoom_factor(p.appearance.ui_scale as f32 / 100.0);
        self.ui.cache_when_idle = p.previews.cache_frames_when_idle;
        self.frames.set_idle_threads(frames::idle_threads(effectcraft_engine::sysinfo::cpu_cores(), p.memory.idle_cache_cores));
        self.ui.viewer.fast_preview = p.previews.fast_previews;
        let budgets = p.cache_budgets(self.session.sys_memory);
        // Compress first: the budget then counts compressed bytes (`frames::codec`).
        self.frames.set_compress(p.memory.preview_compress, p.memory.preview_raw_window);
        // Share One Budget Between the Caches: the RAM preview may grow to its limit in the
        // shared budget and joins it beside the engine's layer and footage caches.
        self.frames.set_budget(budgets.limit(effectcraft_render::cache::shared::ShareKind::Preview));
        self.frames.share(budgets.shared.is_some().then_some(&self.session.cache_share));
        if self.zero_copy.active && self.zero_copy.unified {
            self.frames.set_gpu_budget(budgets.preview);
        }
        if let Some(g) = &self.gpu {
            g.context().set_cache_budget(budgets.layer);
            g.context().set_half_intermediates(p.previews.half_float_effects);
        }
    }

    /// Change settings from the UI (applied next frame and saved).
    pub fn set_pref(&mut self, key: &str, value: serde_json::Value) -> Result<(), String> {
        self.session.prefs.set(key, value)?;
        self.session.prefs_changed();
        self.session.save_prefs();
        Ok(())
    }

    pub fn with_control(mut self, rx: Receiver<ControlRequest>) -> Self {
        self.control_rx = Some(rx);
        self
    }

    /// Publish change events to the control channel's `events.subscribe` connections.
    pub fn with_control_events(mut self, events: control::ControlEvents) -> Self {
        self.events_sent = (f64::NEG_INFINITY, self.session.revision, self.session.state.selected_layers.clone());
        events.set_revision(self.session.revision);
        self.control_events = Some(events);
        self
    }

    /// Show theme `k`: the appearance mode becomes its family's (Dark or Light), and `k` that
    /// family's theme.
    pub fn set_theme(&mut self, ctx: &egui::Context, k: theme::ThemeKind) {
        if let Err(e) = self.set_pref("appearance.theme", json!(k.name())) {
            self.ui.status = e;
        }
        self.apply_prefs(ctx);
    }

    /// The system appearance: light (`Some(true)`), dark, or unknown. The host's answer (the Linux
    /// desktop portal) wins over egui's, which Wayland compositors often leave empty.
    pub fn system_light(&self, ctx: &egui::Context) -> Option<bool> {
        self.hooks.system_theme.as_ref().and_then(|read| read(ctx)).or_else(|| ctx.system_theme()).map(|t| t == egui::Theme::Light)
    }

    /// The Tools bar's appearance button (`view.theme.toggle`): Sync with System, then Light, then
    /// Dark, then Sync with System again. The light and dark theme choices are kept.
    pub fn cycle_appearance(&mut self, ctx: &egui::Context) {
        let next = match self.session.prefs.appearance.appearance_mode.as_str() {
            "auto" => "light",
            "light" => "dark",
            _ => "auto",
        };
        if let Err(e) = self.set_pref("appearance.appearanceMode", json!(next)) {
            self.ui.status = e;
        }
        self.apply_prefs(ctx);
    }

    pub fn set_workspace(&mut self, name: &str) {
        // Learn: the Home screen's Learn tab (tutorials) in the Composition panel. Leaving it for
        // another workspace closes the Home screen again: it stayed over every workspace until a
        // project was created (#272).
        if name == "Learn" {
            self.ui.start_screen = true;
            self.ui.home_learn = true;
            self.ui.home_templates = false;
        } else if self.ui.workspace == "Learn" {
            self.ui.start_screen = false;
            self.ui.home_learn = false;
        }
        self.os_windows = Default::default();
        self.ui.workspace = name.to_string();
        self.ui.dock = self.ui.saved_workspaces.get(name).cloned().unwrap_or_else(|| dock::workspace(name));
        self.ui.floating = self.ui.saved_floating.get(name).cloned().unwrap_or_else(|| dock::workspace_floating(name));
        self.ui.maximized = None;
    }

    /// Built-in workspaces followed by the saved ones (Window ▸ Workspace ▸ Save as New Workspace).
    pub fn workspace_names(&self) -> Vec<String> {
        let mut v: Vec<String> = dock::WORKSPACES.iter().map(|s| s.to_string()).collect();
        v.extend(self.saved_workspace_names());
        v
    }

    /// The Tools bar's workspace switcher, left to right: After Effects' bar workspaces
    /// ([`dock::WORKSPACE_BAR`]), then the saved ones (a new one is added as it is saved), then
    /// the current workspace ([`UiState::workspace`]) if it is none of these (one
    /// chosen from the » menu or Window ▸ Workspace), so the current one is always in the bar.
    pub fn workspace_bar(&self) -> Vec<String> {
        let mut v: Vec<String> = dock::WORKSPACE_BAR.iter().map(|s| s.to_string()).collect();
        v.extend(self.saved_workspace_names());
        if !self.ui.workspace.is_empty() && !v.contains(&self.ui.workspace) {
            v.push(self.ui.workspace.clone());
        }
        v
    }

    /// The workspaces the user saved under a new name (built-ins whose changes were saved are
    /// not repeated), in name order.
    pub fn saved_workspace_names(&self) -> Vec<String> {
        self.ui.saved_workspaces.keys().filter(|k| !dock::WORKSPACES.contains(&k.as_str())).cloned().collect()
    }

    /// The open Gradient Editor's draft (`panels::gradient_editor`): what OK would set.
    pub fn gradient_draft(&self) -> &panels::gradient_editor::Draft {
        &self.dialog_state.gradient
    }

    /// The open Gradient Editor's draft, to edit (tests and automation; the editor's own fields
    /// do the same).
    pub fn gradient_draft_mut(&mut self) -> &mut panels::gradient_editor::Draft {
        &mut self.dialog_state.gradient
    }

    /// Feed this frame's share of the queued synthetic input (control channel `ui.click`,
    /// `ui.drag`…) into `raw_input`. Pointer events go one per frame so egui sees press → moves
    /// → release as a real gesture.
    pub fn feed_synthetic(&mut self, raw_input: &mut egui::RawInput) {
        let n = synthetic_batch(&self.synthetic);
        raw_input.events.extend(self.synthetic.drain(..n));
    }

    /// Show panel `p` (opening it in its usual place if it is closed), bring it to the front and
    /// give it the focus: Window ▸ <panel>.
    pub fn show_panel(&mut self, p: PanelKind) {
        if !self.ui.floating.iter().any(|f| f.panels.contains(&p)) && self.ui.maximized.is_some_and(|m| m != p) && !self.group_maximized(p) {
            self.ui.maximized = None;
        }
        if let Some(f) = self.ui.floating.iter().find(|f| f.panels.contains(&p)) {
            // Its OS window comes to the front too.
            self.os_windows.raise = Some(f.id);
        }
        self.raise_panel(p);
        self.ui.focused = p;
    }

    /// Bring panel `p` to the front of its group, opening it in its usual place if it is closed,
    /// without moving the focus (a comp opening brings up its viewer, an effect applied brings up
    /// Effect Controls).
    pub fn raise_panel(&mut self, p: PanelKind) {
        if self.ui.floating.iter().any(|group| group.panels.contains(&p)) {
            self.edit_layout(|layout| layout.apply(craft_ui::docking::Action::Activate { panel: p }).is_ok());
            return;
        }
        if !self.ui.dock.contains(p) {
            // The Composition panel lives in the centre, with the Layer panel and other viewers;
            // with none of those left, above the Timeline.
            let centre = |d: &dock::DockNode| {
                let mut v = Vec::new();
                d.panels(&mut v);
                v.into_iter().find(|q| matches!(q, PanelKind::Layer | PanelKind::Viewer(_) | PanelKind::Flowchart | PanelKind::Footage))
            };
            let near = match p {
                PanelKind::Composition => match centre(&self.ui.dock) {
                    Some(q) => q,
                    None if self.ui.dock.dock_panel(p, PanelKind::Timeline, dock::Zone::Top) => {
                        self.ui.dock.activate(p);
                        return;
                    }
                    None => PanelKind::EffectsPresets,
                },
                PanelKind::Layer | PanelKind::Flowchart | PanelKind::Viewer(_) => PanelKind::Composition,
                PanelKind::RenderQueue => PanelKind::Timeline,
                PanelKind::EffectControls | PanelKind::History => PanelKind::Project,
                _ => PanelKind::EffectsPresets,
            };
            self.ui.dock.open_near(p, near);
        }
        self.ui.dock.activate(p);
    }

    pub fn render_source(&self) -> RenderSource {
        RenderSource {
            freeze_feedback: self.session.freeze_runtime.feedback.clone(),
            project: self.session.freeze_preview_project(),
            footage: self.session.footage.clone(),
            expr: self.session.expr.clone(),
            layer_cache: self.session.layer_cache.clone(),
            gpu: self.gpu.clone(),
            gpu_display: self.gpu_display(),
            disk: self.session.disk_cache.clone(),
        }
    }

    /// Prepare plain viewer pixels on the compositor. The browser keeps its
    /// texture; native workers read back RGBA8 for the separate egui device.
    fn gpu_display(&self) -> bool {
        let v = &self.session.state.viewer;
        (cfg!(target_arch = "wasm32") || !self.frames.remote_active())
            && v.channel == effectcraft_engine::viewer::Channel::Rgb
            && v.exposure == 0.0
            && self.session.state.region_of_interest.is_none()
            && self.ui.viewer.extended.is_none()
    }

    /// The UI draws its panels and handles input such as dropped files. Until the fonts are
    /// installed (the first frames) it draws nothing, and that input goes unhandled.
    pub fn is_ready(&self) -> bool {
        self.fonts_ready
    }

    /// RT10e: let the GPU compositor share egui's wgpu device, so viewer frames are drawn
    /// straight from their textures (see [`zero_copy`]). A desktop host passes its window's
    /// traffic light, held red wherever a surface may be configured; a host without a surface
    /// passes `None`. Call before the first frame.
    pub fn share_display_device(&mut self, light: Option<Arc<zero_copy::TrafficLight>>) {
        self.zero_copy.offer(light);
    }

    /// Keep the GPU compositor on the window's adapter even when that is OpenGL or a CPU device:
    /// Window Graphics ▸ OpenGL was chosen because this machine's Vulkan or DirectX 12 driver
    /// crashes as a device is created (#243), which the compositor's device would too. Call
    /// before the first frame.
    pub fn keep_compositor_on_window_adapter(&mut self) {
        self.compositor_off_window = false;
    }

    /// Viewer frames shown from their GPU textures without a copy so far (RT10e).
    pub fn zero_copy_frames(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        return self.frames.resident_frames();
        #[cfg(target_arch = "wasm32")]
        0
    }

    /// The compositor shares egui's device (RT10e).
    pub fn zero_copy_active(&self) -> bool {
        self.zero_copy.active
    }

    /// Provide the executable/embedding host's device notifications. The host installs the
    /// handlers once; this frontend never replaces handlers on a borrowed device.
    pub fn set_gpu_failure_bridge(&mut self, bridge: gpu_failure::GpuFailureBridge) {
        if let Some(gpu) = &self.gpu {
            bridge.forward_to(gpu.context().device_failure_notifier());
        }
        self.gpu_failures = Some(bridge);
    }

    fn apply_gpu_failure(&mut self, ctx: &egui::Context) {
        let Some(bridge) = &self.gpu_failures else { return };
        // Readback deadlines can retire the compositor without a host device notification.
        let own = self.gpu.as_ref().and_then(|gpu| gpu.context().check_health().err());
        if let Some(reason) = &own {
            bridge.report(reason, false);
        }
        let Some(mut failure) = bridge.failure() else { return };
        if self.gpu_failure_applied == Some(failure.presentation_lost) {
            // The bridge latches the first failure: a replacement compositor that fails too
            // (CR1) is retired by its own health check.
            match own.filter(|_| !failure.presentation_lost) {
                Some(reason) => failure.reason = reason,
                None => return,
            }
        }
        let had_gpu = self.gpu.is_some();
        let failed_device = self.gpu.as_ref().map(effectcraft_gpu::Gpu::downgrade);
        self.gpu_failure_applied = Some(failure.presentation_lost);
        self.gpu_checked = true;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.gpu_startup = None;
        }
        self.frames.retire_gpu();
        self.session.accel = None;
        self.session.accel_note = Some(format!("GPU preview retired: {}", failure.reason));
        self.session.layer_cache.clear();
        self.gpu = None;
        if let Some(rs) = &self.wgpu {
            let mut renderer = rs.renderer.write();
            if let Some((id, _, _)) = &self.viewer_native {
                renderer.free_texture(id);
            }
            for (_, texture) in self.passive_tex.values() {
                if let panels::viewers::PassiveTexture::Gpu(id, _) = texture {
                    renderer.free_texture(id);
                }
            }
        }
        self.viewer_native = None;
        self.passive_tex.retain(|_, (_, texture)| matches!(texture, panels::viewers::PassiveTexture::Cpu(_)));
        self.viewer_shown = self.viewer_tex.as_ref().map(|(texture, key, _)| (texture.id(), *key));
        self.viewer_image = None;
        // Old completion closures retain only their detached slot, not the replacement.
        self.viewer_readback = Default::default();
        self.wgpu = None;
        // RT10e: a compositor on egui's device retired, the window still draws: the next one
        // gets a device of its own and frames are read back as before.
        let fallback = self.zero_copy.active && !failure.presentation_lost;
        if self.zero_copy.active {
            self.zero_copy.active = false;
            self.zero_copy.failed = true;
            self.frames.set_gpu_budget(frames::GPU_BUDGET);
        }
        if fallback {
            self.gpu_checked = false;
        }
        // CR1: a compositor on a device of its own (out of video memory, a readback that timed
        // out) is set up again on a new device, with at most half as many GPU frames at once
        // (`Frames::retire_gpu`), instead of compositing on the CPU until a restart.
        let rebuild = had_gpu && !fallback && !failure.presentation_lost && self.gpu_rebuilds < MAX_GPU_REBUILDS;
        if rebuild {
            self.gpu_rebuilds += 1;
            self.gpu_rebuilding = true;
            self.gpu_failed_device = failed_device.map(|d| (d, web_time::Instant::now()));
            self.gpu_checked = false;
        }
        self.ui.status = if failure.presentation_lost {
            "Graphics device lost. The project is still open; presentation requires restarting the application.".into()
        } else if rebuild {
            "The GPU preview failed (out of video memory?); it restarts on a new device with fewer frames at once.".into()
        } else if fallback {
            "GPU frames are copied for display again after a GPU error; the GPU preview continues on a device of its own.".into()
        } else {
            "Shared GPU preview disabled; CPU compositing remains available.".into()
        };
        log::warn!("GPU preview retired: {}", failure.reason);
        ctx.request_repaint();
    }

    /// After the graphics device is lost, eframe's renderer cannot draw again: any mesh makes
    /// egui-wgpu panic on the main thread when it cannot stage its index buffer (#591). Draw
    /// nothing from then on, auto-save a project with unsaved changes (keeping the crash
    /// recovery marker, so the next launch offers it) and close the window.
    fn close_after_device_loss(&mut self, ctx: &egui::Context) {
        if self.device_lost_close.is_some() {
            return;
        }
        let keep_recovery = self.session.is_dirty();
        let saved = keep_recovery.then(|| match self.session.autosave_now().and_then(|path| self.session.autosave_wait().map(|()| path)) {
            Ok(path) => {
                log::warn!("graphics device lost: auto-saved {path}; the next launch offers to recover it");
                Ok(path)
            }
            Err(e) => {
                log::error!("graphics device lost: {e}");
                Err(e.to_string())
            }
        });
        self.device_lost_close = Some(keep_recovery);
        log::warn!("graphics device lost: closing the window");
        if let Some(tell) = &self.hooks.device_lost {
            tell(saved);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Run the commands whose non-blocking file dialogs have been answered.
    pub(crate) fn run_dialog_answers(&mut self) {
        while let Ok(answer) = self.dialog_answers.1.try_recv() {
            menus::dialog_answered(self, answer);
        }
    }

    /// Set up the GPU compositor (once). Desktop frame workers use their own device so their
    /// submissions cannot race egui-wgpu's surface reconfiguration during window moves/resizes.
    /// Without an adapter that runs compute shaders, everything stays on the CPU.
    pub(crate) fn init_gpu(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        if self.gpu_checked {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.frames.set_resident(self.zero_copy.resident(&self.session.prefs));
                self.prewarm_gpu_project(ctx);
            }
            return;
        }
        // A shared-device compositor's failure leaves the window drawing: its replacement gets a
        // device of its own (RT10e).
        let replacing = (self.zero_copy.failed || self.gpu_rebuilding)
            && self.gpu_failures.as_ref().is_some_and(|bridge| bridge.failure().is_some_and(|f| !f.presentation_lost));
        // A device that ran out of video memory still holds it until its last frames are done:
        // a replacement made meanwhile ran out too (and the window could not draw), so it waits.
        if replacing
            && let Some((old, since)) = &self.gpu_failed_device
            && old.strong_count() > 0
            && since.elapsed() < REBUILD_WAIT
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }
        self.gpu_failed_device = None;
        if !replacing && self.gpu_failures.as_ref().is_some_and(|bridge| bridge.failure().is_some()) {
            self.gpu_checked = true;
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.gpu_startup = None;
            }
            return;
        }
        let Some(rs) = frame.wgpu_render_state() else {
            self.gpu_checked = true;
            return;
        };
        // Help ▸ System Compatibility Report names the adapter and backend the window got.
        let info = rs.adapter.get_info();
        self.session.window_adapter = Some(format!(
            "{} ({:?}, {:?}{})",
            info.name,
            info.backend,
            info.device_type,
            if info.driver.is_empty() { String::new() } else { format!(", {} {}", info.driver, info.driver_info) }
        ));
        #[cfg(not(target_arch = "wasm32"))]
        let gpu = {
            // Return the first UI update before launching shader compilation.
            if !self.gpu_startup_deferred {
                self.gpu_startup_deferred = true;
                ctx.request_repaint();
                return;
            }
            if let Some(pending) = &self.gpu_startup {
                let Some(result) = pending.poll() else {
                    return;
                };
                self.gpu_startup = None;
                result
            } else {
                let (adapter, device, queue, wake) = (rs.adapter.clone(), rs.device.clone(), rs.queue.clone(), ctx.clone());
                let share = self.zero_copy.share(&self.session.prefs);
                let off_window = self.compositor_off_window;
                let project = self.session.freeze_preview_project();
                let comp = self.session.active_comp_id();
                self.gpu_startup_key = comp.map(|comp| (self.session.revision, comp));
                let time = self.session.time();
                match gpu_startup::Pending::start(
                    move || {
                        let gpu = compositor_gpu(&adapter, device, queue, share, off_window)?;
                        // Unified memory sizes its texture pool by RAM (RT9a). Off the UI thread;
                        // not on Windows, where reading it starts PowerShell (the memory watch
                        // passes it on there).
                        #[cfg(not(target_os = "windows"))]
                        gpu.context().set_system_memory(effectcraft_engine::sysinfo::memory().map_or(0, |m| m.total));
                        gpu.warm_pipelines()?;
                        if let Some(comp) = comp {
                            log::debug!("GPU startup project warmup: {:?}", gpu.warm_project(&project, comp, time));
                        }
                        Ok(gpu)
                    },
                    move || wake.request_repaint(),
                ) {
                    Ok(pending) => {
                        self.gpu_startup = Some(pending);
                        return;
                    }
                    Err(reason) => Err(reason),
                }
            }
        };
        #[cfg(target_arch = "wasm32")]
        let gpu = {
            let _ = ctx;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compositor_gpu(&rs.adapter, rs.device.clone(), rs.queue.clone(), None, false)))
                .unwrap_or_else(|_| Err("GPU compositor setup failed: backend panicked".into()))
        };
        self.gpu_checked = true;
        self.gpu_rebuilding = false;
        match gpu {
            Ok(g) => {
                // A replacement on a device of its own has its own handlers; the window device's
                // latched failure is not its.
                if let Some(bridge) = self.gpu_failures.as_ref().filter(|_| !replacing) {
                    bridge.forward_to(g.context().device_failure_notifier());
                    if bridge.failure().is_some() {
                        return;
                    }
                }
                if replacing {
                    self.frames.revive_gpu();
                }
                // RT10e: frames stay textures when the compositor is on egui's device.
                self.zero_copy.active = shares_window_device(&g, &rs.device) && !cfg!(target_arch = "wasm32");
                self.zero_copy.unified = info.device_type == eframe::wgpu::DeviceType::IntegratedGpu;
                #[cfg(not(target_arch = "wasm32"))]
                self.frames.set_resident(self.zero_copy.resident(&self.session.prefs));
                if self.zero_copy.active {
                    log::info!("GPU viewer frames are drawn without a copy (the compositor shares the window's device)");
                    if self.zero_copy.unified {
                        self.frames.set_gpu_budget(self.session.prefs.cache_budgets(self.session.sys_memory).preview);
                    }
                }
                log::info!("GPU compositor: {}", effectcraft_engine::render::Accelerator::name(&g));
                g.context().set_cache_budget(self.session.prefs.cache_budgets(self.session.sys_memory).layer);
                g.context().set_half_intermediates(self.session.prefs.previews.half_float_effects);
                self.session.accel = Some(Arc::new(g.clone()));
                self.gpu = Some(g);
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.gpu_prewarm = gpu_startup::Latest::completed(self.gpu_startup_key.take());
                }
                self.wgpu = Some(rs.clone());
            }
            Err(reason) => {
                log::info!("GPU compositor unavailable: {reason}");
                self.session.accel_note = Some(format!("GPU compositor unavailable: {reason}"));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn prewarm_gpu_project(&mut self, ctx: &egui::Context) {
        if self.gpu_failures.as_ref().is_some_and(|bridge| bridge.failure().is_some()) {
            return;
        }
        let (Some(gpu), Some(comp)) = (self.gpu.clone(), self.session.active_comp_id()) else { return };
        if gpu.context().check_health().is_err() {
            return;
        }
        // One captured time per revision/composition, not every future animation sequence.
        let key = (self.session.revision, comp);
        if !self.gpu_prewarm.needs(&key) {
            return;
        }
        let project = self.session.freeze_preview_project();
        let time = self.session.time();
        let wake = ctx.clone();
        self.gpu_prewarm.request(
            key,
            move || {
                log::debug!("GPU project warmup: {:?}", gpu.warm_project(&project, comp, time));
                Ok(())
            },
            move || wake.request_repaint(),
        );
    }

    /// The viewer shows a frame composited on the GPU (drawn from its wgpu texture).
    pub fn viewer_on_gpu(&self) -> bool {
        matches!((&self.viewer_shown, &self.viewer_native), (Some(s), Some(n)) if s.1 == n.1)
    }

    /// The GPU compositor's adapter, when the viewer has one.
    pub fn gpu_adapter(&self) -> Option<String> {
        self.gpu.as_ref().map(effectcraft_engine::render::Accelerator::name)
    }

    /// The size of the viewer frame's texture (CPU frames; fitted to the GPU's texture limit).
    pub fn viewer_texture_size(&self) -> Option<[usize; 2]> {
        self.viewer_tex.as_ref().map(|(t, _, _)| t.size())
    }

    /// The viewer's current pixels as 8-bit premultiplied RGBA, reading a GPU frame back the
    /// first time something asks for it.
    ///
    /// The browser can't wait for the GPU: there the first call starts an asynchronous readback
    /// and returns `None`; a later call (the next frame) has the pixels.
    pub fn viewer_pixels(&mut self) -> Option<Arc<egui::ColorImage>> {
        let to_image = |bytes: Vec<u8>, w: u32, h: u32| {
            let px = bytes.as_chunks::<4>().0.iter().map(|c| egui::Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3])).collect();
            Arc::new(egui::ColorImage::new([w as usize, h as usize], px))
        };
        if self.viewer_image.is_none()
            && let (Some((_, key, f)), Some(g)) = (&self.viewer_native, &self.gpu)
            && self.viewer_shown.as_ref().map(|s| s.1) == Some(*key)
        {
            if cfg!(target_arch = "wasm32") {
                let ready = self.viewer_readback.1.lock().ok().and_then(|mut r| r.take_if(|r| r.0 == *key));
                if let Some((_, img)) = ready {
                    self.viewer_image = Some(img);
                } else if self.viewer_readback.0 != Some(*key) {
                    self.viewer_readback.0 = Some(*key);
                    let (slot, k, (w, h)) = (self.viewer_readback.1.clone(), *key, (f.width, f.height));
                    let ctx = self.frames.context();
                    g.read_display_async(f, move |bytes| {
                        if let (Some(b), Ok(mut s)) = (bytes, slot.lock()) {
                            *s = Some((k, to_image(b, w, h)));
                        }
                        if let Some(c) = ctx {
                            c.request_repaint();
                        }
                    });
                }
            } else if let Some(bytes) = g.read_display(f) {
                self.viewer_image = Some(to_image(bytes, f.width, f.height));
            }
        }
        self.viewer_image.clone()
    }

    /// The viewer's Resolution: the active composition's (saved with the project).
    pub fn viewer_res(&self) -> state::Resolution {
        self.session.active_comp().map(|c| c.resolution).unwrap_or_default()
    }

    fn scrub_enabled(&self) -> bool {
        !self.playback.playing
            && self.viewer_res().is_auto()
            && self.session.state.viewer.fast_previews == effectcraft_engine::commands::viewer_cmds::FastPreviews::AdaptiveResolution
            && !self.ui.viewer.interacting
            && !self.ui.viewer.property_interacting
            && !self.frames.remote_active()
    }

    fn scrubbing(&self) -> bool {
        self.scrub_enabled()
            && self
                .session
                .active_comp_id()
                .zip(self.session.active_comp())
                .is_some_and(|(id, c)| self.scrub_preview.active(id.0, self.session.revision, c.frame_rate.frame_at(self.session.time())))
    }

    pub(crate) fn record_scrub(&mut self, now: f64) {
        if let (Some(id), Some(c)) = (self.session.active_comp_id(), self.session.active_comp()) {
            self.scrub_preview.observe(id.0, self.session.revision, c.frame_rate.frame_at(self.session.time()), now);
        }
    }

    pub(crate) fn frame_for_viewer(&self, key: &FrameKey, zoom: f32, ppp: f32) -> Option<(FrameKey, frames::FrameImage)> {
        if let Some(img) = self.frames.get(key) {
            return Some((*key, img));
        }
        // Prefer exact current content, even coarse, to an older edited frame.
        if self.scrub_enabled() && self.scrub_preview.matches(key.comp, self.session.revision, key.frame) {
            let full = self.viewer_res().scale(zoom, ppp);
            let coarse = full.min((full * 0.25).max(self.session.prefs.adaptive_limit()));
            let k = self.frame_key(ItemId(key.comp), key.frame, coarse);
            if k.scale < key.scale
                && k.content == key.content
                && k.view == key.view
                && key.revision == self.session.revision
                && let Some(img) = self.frames.get(&k)
            {
                return Some((k, img));
            }
        }
        self.frames
            .get_for_viewer(key, self.viewer_shown.as_ref().map(|(_, s)| s))
            // A cloud frame takes a round trip: while scrubbing, the newest one that landed.
            .or_else(|| self.frames.latest_remote_for(key))
    }

    /// Follow Settings ▸ Previews ▸ Preview On (CR5): connect to the cloud frame server or not,
    /// hand the viewer's frames to it or to this Mac, and note a lost or regained connection in
    /// the status line (never a dialog; the document is never touched).
    #[cfg(not(target_arch = "wasm32"))]
    fn tick_cloud(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let mode = self.session.prefs.previews.preview_on.clone();
        if mode != "local" && now - self.cloud_token.0 > 3.0 {
            let token = std::env::var("EFFECTCRAFT_FRAME_TOKEN")
                .ok()
                .or_else(|| self.session.config.as_ref().and_then(|c| c.read(cloud::TOKEN_ENTRY)))
                .unwrap_or_default();
            self.cloud_token = (now, token.trim().to_string());
        }
        let comp = self.session.active_comp_id().zip(self.session.active_comp()).map(|(id, c)| (id.0, c.frame_rate.as_f64()));
        let last_ms = self.frames.last_ms.lock().map(|m| *m).unwrap_or(0.0);
        let address = self.session.prefs.previews.cloud_address.clone();
        let tick = cloud::Now {
            mode: &mode,
            address: &address,
            token: &self.cloud_token.1,
            revision: self.session.revision,
            project: &self.session.project,
            path: self.session.path.as_deref(),
            comp,
            last_ms,
        };
        let (remote, note) = self.cloud.tick(&tick);
        // Only the cloud's own renderer is swapped in and out (tests and hosts may set another).
        match remote {
            Some(c) => {
                let c: Arc<dyn frames::RemoteFrames> = c;
                if !self.frames.remote().is_some_and(|r| Arc::ptr_eq(r, &c)) {
                    self.frames.set_remote(Some(c));
                }
                self.cloud.installed = true;
            }
            None if self.cloud.installed => {
                self.frames.set_remote(None);
                self.cloud.installed = false;
            }
            None => {}
        }
        if let Some(note) = note {
            self.ui.status = note;
        }
        self.frames.tick_remote();
        if mode != "local" {
            // The link's state changes without input (a frame landing, the tunnel dropping).
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// The render scale used by the viewer right now.
    pub fn viewer_scale(&self, zoom: f32, ppp: f32) -> f64 {
        // The preview's Resolution (when not Auto) while it plays.
        if self.playback.playing
            && let Some(f) = self.playback.res
        {
            let r = state::Resolution::from_factor(f.into());
            self.frames.adapt_playback(None);
            return r.scale(zoom, ppp);
        }
        // Auto follows the zoom alike while playing and paused, so frames cached while scrubbing
        // play back, and frames cached by a preview stay on screen when it stops.
        let res = self.viewer_res();
        let full = res.scale(zoom, ppp);
        // Native Auto playback measures actual render service times, never cache hits, and
        // ladders Full → Half → Third → Quarter while frames miss their deadline (see
        // playback_budget): Settings ▸ Previews ▸ Adaptive Resolution While Playing, or Fast
        // Previews ▸ Adaptive Resolution.
        let adaptive = self.playback.playing
            && !self.playback.caching
            && res.is_auto()
            && (self.session.prefs.previews.adaptive_playback
                || self.session.state.viewer.fast_previews == effectcraft_engine::commands::viewer_cmds::FastPreviews::AdaptiveResolution)
            && !self.ui.viewer.interacting
            && !self.ui.viewer.property_interacting
            && !self.frames.remote_active()
            && self.playback.fps.is_finite()
            && self.playback.fps > 0.0;
        let half = full.min((full * 0.5).max(self.session.prefs.adaptive_limit()));
        let third = full.min((full / 3.0).max(self.session.prefs.adaptive_limit()));
        let quarter = full.min((full * 0.25).max(self.session.prefs.adaptive_limit()));
        let config = self.session.active_comp_id().filter(|_| adaptive).map(|comp| {
            let step = self.playback.plan.map_or(1, |p| p.step).max(1) as f64;
            crate::playback_budget::Config {
                full: self.frame_series(comp, full),
                half: self.frame_series(comp, half),
                third: self.frame_series(comp, third),
                quarter: self.frame_series(comp, quarter),
                budget_ms: 1000.0 * step / self.playback.fps,
                gpu_context: self.gpu.as_ref().map_or(0, |g| std::ptr::from_ref(g.context()) as usize),
                parallel: self.frames_parallelism(),
            }
        });
        match self.frames.adapt_playback(config) {
            0 => {}
            1 => return half,
            2 => return third,
            _ => return quarter,
        }
        if self.scrubbing() {
            return full.min((full * 0.25).max(self.session.prefs.adaptive_limit()));
        }
        // Fast Previews ▸ Adaptive Resolution / Fast Draft: lower resolution while dragging.
        let (_, k) = self.session.state.viewer.fast_previews.render(self.ui.viewer.interacting || self.ui.viewer.property_interacting);
        if k < 1.0 && res.is_auto() {
            return full.min((full * 0.5).max(self.session.prefs.adaptive_limit()));
        }
        full
    }

    /// The cache key of viewer frame `frame` of `comp` at `scale` (see [`Self::frame_series`]).
    pub fn frame_key(&self, comp: ItemId, frame: i64, scale: f64) -> FrameKey {
        FrameKey { frame, ..self.frame_series(comp, scale) }
    }

    /// The key of frame 0 of `comp`'s viewer frames at `scale` (the content, view and render
    /// options are the same for every frame: loops compute this once).
    pub fn frame_series(&self, comp: ItemId, scale: f64) -> FrameKey {
        let generation = self.session.freeze_preview_revision();
        let mut content = self.frames.content_of_preview(&self.session.freeze_preview_project(), self.session.revision, generation, comp);
        if let Some(epoch) = self.session.freeze_runtime.pixel_epoch(&self.session.project, comp) {
            // Derived cache verification can change pixels without an authoring edit.
            // Keep that in content identity: revision remains the document revision used
            // by progressive scrubbing and playback's stale-completion checks.
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            (content, epoch).hash(&mut hash);
            content = hash.finish();
        }
        FrameKey {
            revision: self.session.revision,
            content,
            comp: comp.0,
            frame: 0,
            scale: (scale * 1000.0).round() as u32,
            view: self.view_hash(comp),
            opts: frames::opts_hash(&self.frame_opts(comp, scale)),
        }
    }

    /// The frame series the viewer of `comp` shows now (its scale, view and options, for the
    /// comp's current content): what the cache bars count.
    pub fn shown_series(&self, comp: ItemId) -> FrameKey {
        let scale = self.viewer_shown.as_ref().filter(|(_, k)| k.comp == comp.0).map_or(1000, |(_, k)| k.scale);
        self.frame_series(comp, scale as f64 / 1000.0)
    }

    /// Hash of the comp viewer's 3D view camera (0 for the active camera view).
    pub fn view_hash(&self, comp: ItemId) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // The region of interest changes what is rendered too.
        let roi = self.viewer_region(comp);
        if let Some(r) = roi {
            r.map(f64::to_bits).hash(&mut h);
        }
        let Some(cam) = self.session.view_camera(comp) else { return if roi.is_some() { h.finish() | 1 } else { 0 } };
        for row in cam.view.0 {
            for v in row {
                v.to_bits().hash(&mut h);
            }
        }
        cam.zoom.to_bits().hash(&mut h);
        cam.ortho.hash(&mut h);
        h.finish() | 1
    }

    /// The comp-space region viewer frames of `comp` cover: the region of interest, else the
    /// Extended Viewer's area (frame plus pasteboard), else `None` (the whole frame).
    pub fn viewer_region(&self, comp: ItemId) -> Option<[f64; 4]> {
        if self.session.active_comp_id() != Some(comp) {
            return None;
        }
        self.session.state.region_of_interest.or(self.ui.viewer.extended)
    }

    /// Request a prefetch frame render (no-op if cached/in flight).
    pub fn request_frame(&self, comp: ItemId, frame: i64, scale: f64) {
        self.request_frame_with(comp, frame, scale, false);
    }

    /// Request a frame for Cache Frames When Idle ([`Frames::request_idle`]).
    fn request_frame_idle(&self, comp: ItemId, frame: i64, scale: f64) {
        let Some(c) = self.session.project.comp(comp) else { return };
        let key = self.frame_key(comp, frame, scale);
        let opts = self.frame_opts(comp, scale);
        self.frames.request_idle(&self.render_source(), key, comp, c.frame_rate.tick_of(frame), opts);
    }

    /// Request the frame on screen: rendered before any queued prefetch.
    pub fn request_frame_urgent(&self, comp: ItemId, frame: i64, scale: f64) {
        self.request_frame_with(comp, frame, scale, true);
    }

    /// Request the paused viewer's frame: also over a draft of the cloud's stream (CR6c).
    pub fn request_frame_exact(&self, comp: ItemId, frame: i64, scale: f64) {
        let Some(c) = self.session.project.comp(comp) else { return };
        let key = self.frame_key(comp, frame, scale);
        let opts = self.frame_opts(comp, scale);
        self.frames.request_exact(&self.render_source(), key, comp, c.frame_rate.tick_of(frame), opts);
    }

    /// Render options of viewer frames of `comp` at `scale`.
    pub fn frame_opts(&self, comp: ItemId, scale: f64) -> RenderOpts {
        let (draft, _) = self.session.state.viewer.fast_previews.render(self.ui.viewer.interacting || self.ui.viewer.property_interacting);
        let roi = self.viewer_region(comp);
        RenderOpts {
            scale,
            motion_blur: true,
            guides: true,
            draft: self.ui.viewer.fast_preview || draft,
            view: self.session.view_camera(comp),
            roi,
            backend: effectcraft_engine::render::Backend::Auto,
            nested_switches: self.session.prefs.general.switches_affect_nested_comps,
            draft_shadows: self.session.prefs.three_d.realtime_shadows,
            sharp: false,
            proxy: Default::default(),
        }
    }

    fn request_frame_with(&self, comp: ItemId, frame: i64, scale: f64, urgent: bool) {
        let Some(c) = self.session.project.comp(comp) else { return };
        let key = self.frame_key(comp, frame, scale);
        let t = c.frame_rate.tick_of(frame);
        let opts = self.frame_opts(comp, scale);
        if urgent {
            self.frames.request_urgent(&self.render_source(), key, comp, t, opts);
        } else {
            self.frames.request(&self.render_source(), key, comp, t, opts);
        }
    }

    // ---------------------------------------------------------------- playback

    pub fn play(&mut self, now: f64) {
        let sc = self.session.prefs.preview.current;
        self.play_with(now, sc);
    }

    /// Start the preview of a Preview panel shortcut (its range, play-from, rate, skip,
    /// resolution, loop, caching, full screen and include options).
    pub fn play_with(&mut self, now: f64, sc: effectcraft_engine::preview::PreviewShortcut) {
        let Some(c) = self.session.active_comp_arc() else { return };
        let preset = self.session.prefs.preview.get(sc).clone();
        let plan = effectcraft_engine::preview::plan(&preset, &c, self.session.time());
        let fr = c.frame_rate;
        if plan.video {
            self.session.set_time(fr.tick_of(plan.first));
        }
        let restore_max = preset.full_screen.then_some(self.ui.maximized);
        let viewer = panels::viewers::active_panel(self);
        if preset.full_screen && self.ui.dock.contains(viewer) {
            self.ui.maximized = Some(viewer);
            self.ui.maximized_solo = true;
        }
        self.playback = Playback {
            playing: true,
            start_wall: now,
            start_time: fr.tick_of(plan.first),
            start_frame: plan.first,
            shown: 0,
            waiting: false,
            fps: plan.fps,
            shortcut: sc,
            plan: Some(plan),
            caching: plan.cache_first && plan.video,
            res: preset.resolution.factor(preset.custom_resolution),
            overlays: preset.include_overlays,
            layer_controls: preset.include_layer_controls,
            restore_max,
            audio_frame: (!plan.video).then_some(plan.first),
            audio_held: false,
            preroll: None,
            shown_at: Default::default(),
            observed: now,
            log: Vec::new(),
            speed: 0,
        };
        if !self.playback.caching {
            if plan.video {
                self.playback.audio_held = self.audio_plays();
                self.playback.preroll = self.playback.audio_held.then_some(now + PREROLL_MAX);
            } else {
                self.start_audio(fr.tick_of(plan.first));
            }
        }
    }

    /// Play Faster (FilmCraft's Shuttle Right, Shift+E in the Elias keymap): plays at ×2 (from
    /// the current time when stopped), each further press doubles the speed up to ×8. Stopping
    /// or [`Self::play_slower`] back to ×1 returns to normal speed.
    pub fn play_faster(&mut self, now: f64) {
        if !self.playback.playing {
            self.play(now);
        }
        if !self.playback.playing {
            return;
        }
        // At speed the preview starts at once: no Cache Before Playback.
        self.playback.caching = false;
        let speed = self.playback.speed.max(1).saturating_mul(2).min(MAX_PLAY_SPEED);
        self.set_play_speed(now, speed);
    }

    /// Play Slower: halves Play Faster's speed, down to normal speed (with sound again). Whether
    /// a fast preview was playing.
    pub fn play_slower(&mut self, now: f64) -> bool {
        if !self.playback.playing || self.playback.speed < 2 {
            return false;
        }
        self.set_play_speed(now, self.playback.speed / 2);
        true
    }

    /// Play on from the current frame at `speed` × the preview's rate (1 = normal).
    fn set_play_speed(&mut self, now: f64, speed: u32) {
        let Some(mut pl) = self.playback.plan else { return };
        let (old, new) = (self.playback.speed.max(1), speed.clamp(1, MAX_PLAY_SPEED));
        if old == new {
            return;
        }
        // Powers of two: the rate comes back exactly at ×1 (the sound needs the comp's rate).
        pl.fps = pl.fps / f64::from(old) * f64::from(new);
        pl.step = (pl.step / i64::from(old)).max(1).saturating_mul(i64::from(new));
        let cur = if pl.video {
            self.session.active_comp().map_or(pl.first, |c| c.frame_rate.frame_at(self.session.time()))
        } else {
            self.playback.audio_frame.unwrap_or(pl.first)
        };
        self.playback.plan = Some(pl);
        self.playback.fps = pl.fps;
        self.playback.speed = if new > 1 { new } else { 0 };
        self.playback.start_wall = now;
        self.playback.start_frame = cur;
        self.playback.waiting = false;
        self.playback.shown_at.clear();
        // The sound starts over at the new speed (same pitch) once the frames ahead are cached.
        self.audio = None;
        self.playback.preroll = None;
        let sound = self.audio_plays();
        self.playback.audio_held = sound && pl.video;
        if sound && !pl.video {
            self.start_audio(self.session.active_comp().map(|c| c.frame_rate.tick_of(cur)).unwrap_or_default());
        }
        let msg = if new > 1 { format!("Playing at {new}×") } else { "Playing at normal speed".to_string() };
        self.toast = Some((msg, now));
    }

    /// The preview includes audio at the comp's frame rate and the comp has something audible.
    fn audio_plays(&self) -> bool {
        let (Some(pl), Some(cid)) = (self.playback.plan, self.session.active_comp_id()) else { return false };
        let Some(c) = self.session.project.comp(cid) else { return false };
        // A preview at another frame rate plays slower or faster than real time: silent. Play
        // Faster's rate is the speed times the preview's (its sound keeps its pitch).
        let speed = f64::from(self.playback.speed.max(1));
        pl.audio && (pl.fps / speed - c.frame_rate.as_f64()).abs() <= 0.01 && effectcraft_engine::render::audio::comp_has_audio(&self.session.project, cid)
    }

    /// Start audio preview from comp time `t` when [`Self::audio_plays`] and an output device
    /// opens.
    fn start_audio(&mut self, t: Tick) {
        let held = self.audio.take();
        self.meter.clipped = [false; 2];
        let (Some(pl), Some(cid)) = (self.playback.plan, self.session.active_comp_id()) else { return };
        let Some(c) = self.session.project.comp_arc(cid).filter(|_| self.audio_plays()) else { return };
        let fr = c.frame_rate;
        // Held while rendering caught up (same preview): play on, without reopening the device.
        if let Some(mut a) = held.filter(|a| {
            let span = audio::PlaySpan::new(t, fr.tick_of(pl.start), fr.tick_of(pl.end + 1), pl.looping, a.rate);
            a.held() && a.comp == cid && a.speed == self.playback.speed.max(1) && (a.span.wa, a.span.wb, a.span.looping) == (span.wa, span.wb, span.looping)
        }) {
            a.resume(t);
            self.audio = Some(a);
            return;
        }
        let out = audio::AudioOutput::from_prefs(&self.session.prefs);
        let Some(dev) = self.hooks.audio_device.as_ref().and_then(|f| f(&out)) else { return };
        let mix = self.session.prefs.audio.preview_sample_rate;
        let speed = self.playback.speed.max(1);
        match audio::AudioPlayback::start_at_speed(dev, self.render_source(), cid, t, fr.tick_of(pl.start), fr.tick_of(pl.end + 1), pl.looping, mix, speed) {
            Ok(a) => self.audio = Some(a),
            Err(e) => log::warn!("audio preview: {e}"),
        }
    }

    /// Audio scrubbing: play the frame of `comp` at `t` (see [`audio::AudioScrub`]), opening the
    /// output on first use. Silent comps and missing devices do nothing.
    pub fn scrub_audio(&mut self, comp: ItemId, t: Tick, now: f64) {
        if self.playback.playing || !effectcraft_engine::render::audio::comp_has_audio(&self.session.project, comp) {
            return;
        }
        if self.scrub.is_none() {
            let out = audio::AudioOutput::from_prefs(&self.session.prefs);
            let Some(dev) = self.hooks.audio_device.as_ref().and_then(|f| f(&out)) else { return };
            match audio::AudioScrub::open(dev, now) {
                Ok(s) => self.scrub = Some(s),
                Err(e) => {
                    log::warn!("audio scrubbing: {e}");
                    return;
                }
            }
        }
        let src = self.render_source();
        let mix = self.session.prefs.audio.preview_sample_rate;
        if let Some(s) = &mut self.scrub {
            s.play(&src, comp, t, mix, now);
        }
    }

    /// Hover-to-hear, once a frame: play the Sound card the Motion Library's pointer is over
    /// ([`Self::sound_hover`]), stop when it leaves. Silent while the Timeline plays or scrubs
    /// audio (the comp's sound comes first); an idle output closes after two seconds.
    fn tick_sound_preview(&mut self, ctx: &egui::Context, now: f64) {
        let want = self.sound_hover.take().filter(|_| !self.playback.playing && self.scrub.is_none());
        let Some(want) = want else {
            if let Some(sp) = &mut self.sound_preview {
                sp.stop();
                sp.pump();
                if now - sp.last_used > 2.0 {
                    self.sound_preview = None;
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                }
            }
            return;
        };
        if self.sound_preview.is_none() {
            let out = audio::AudioOutput::from_prefs(&self.session.prefs);
            let Some(dev) = self.hooks.audio_device.as_ref().and_then(|f| f(&out)) else { return };
            match audio::SoundPreview::open(dev, now) {
                Ok(sp) => self.sound_preview = Some(sp),
                Err(e) => {
                    log::warn!("sound preview: {e}");
                    return;
                }
            }
        }
        let Some(sp) = &mut self.sound_preview else { return };
        sp.set_gain(want.gain);
        if sp.playing() != Some(want.key.as_str()) && sp.failed().as_deref() != Some(want.key.as_str()) {
            let path = want.path;
            sp.play(&want.key, Box::new(move |rate| audio::decode_sound_file(&path, rate)), true, now);
        }
        sp.last_used = now;
        sp.pump();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    #[track_caller]
    pub fn stop(&mut self) {
        let was = std::mem::replace(&mut self.playback.playing, false);
        self.frames.set_playhead(None);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let shown = self.viewer_shown.as_ref().map(|(_, k)| *k);
            self.cloud_play.stop(&self.frames, shown, self.session.revision);
        }
        self.audio = None;
        self.playback.caching = false;
        if !was {
            return;
        }
        // Which path ended the preview (a preview that stops by itself is otherwise a mystery).
        log::info!("preview stopped at {:.3} s after {} frames (from {})", self.session.time().seconds(), self.playback.shown, std::panic::Location::caller());
        if let Some(m) = self.playback.restore_max.take() {
            self.ui.maximized = m;
            self.ui.maximized_solo = false;
        }
        let Some(pl) = self.playback.plan else { return };
        let move_time = self.session.prefs.preview.get(self.playback.shortcut).move_time_to_preview_time;
        if !move_time {
            self.session.set_time(pl.origin);
        } else if let Some(f) = self.playback.audio_frame
            && let Some(c) = self.session.active_comp()
        {
            let t = c.frame_rate.tick_of(f);
            self.session.set_time(t);
        }
    }

    pub fn toggle_play(&mut self, now: f64) {
        let sc = self.session.prefs.preview.current;
        self.toggle_play_with(now, sc);
    }

    /// A preview shortcut pressed: start its preview, or stop the running one. While Cache
    /// Before Playback is still rendering, "If caching, play cached frames" plays what is
    /// cached so far instead of stopping.
    pub fn toggle_play_with(&mut self, now: f64, sc: effectcraft_engine::preview::PreviewShortcut) {
        if !self.playback.playing {
            self.play_with(now, sc);
            return;
        }
        if self.playback.caching && self.session.prefs.preview.get(self.playback.shortcut).play_cached_frames && self.play_cached(now) {
            return;
        }
        self.stop();
    }

    /// Cut the caching preview down to the frames cached from its first frame on and play them.
    fn play_cached(&mut self, now: f64) -> bool {
        let (Some(mut pl), Some(cid)) = (self.playback.plan, self.session.active_comp_id()) else { return false };
        let scale = self.viewer_shown.as_ref().map(|(_, k)| k.scale as f64 / 1000.0).unwrap_or(1.0);
        let series = self.frame_series(cid, scale);
        let mut last = None;
        let mut f = pl.first;
        while f <= pl.end && self.frames.is_cached(&FrameKey { frame: f, ..series }) {
            last = Some(f);
            f += pl.step;
        }
        let Some(last) = last else { return false };
        pl.end = last;
        pl.start = pl.first;
        self.playback.plan = Some(pl);
        self.playback.caching = false;
        self.playback.start_wall = now;
        self.playback.start_frame = pl.first;
        let t = self.session.active_comp().map(|c| c.frame_rate.tick_of(pl.first)).unwrap_or_default();
        self.start_audio(t);
        true
    }

    /// A preview plays with Video off in the Preview panel (audio only): the current time stays
    /// put, so the viewer and Timeline say so instead of looking stuck (#1118). The frame under
    /// the audible sample, when one is known.
    pub fn video_preview_off(&self) -> Option<Option<i64>> {
        let p = self.playback.plan.filter(|_| self.playback.playing)?;
        (!p.video).then_some(self.playback.audio_frame)
    }

    /// The viewer shows grids, guides and safe margins (hidden while a preview without
    /// Include Overlays plays).
    pub fn overlays_visible(&self) -> bool {
        !self.playback.playing || self.playback.overlays
    }

    /// The viewer shows layer handles and paths (View ▸ Show Layer Controls, and Include Layer
    /// Controls while a preview plays).
    pub fn layer_controls_visible(&self) -> bool {
        self.ui.viewer.show_layer_controls && (!self.playback.playing || self.playback.layer_controls)
    }

    /// RAM preview: advance at the preview's frame rate when frames are cached; otherwise hold
    /// and keep rendering ahead (the green cache bar fills, then playback runs in real time).
    fn advance_playback(&mut self, ctx: &egui::Context, scale: f64) {
        let Some(cid) = self.session.active_comp_id() else { return };
        let Some(c) = self.session.project.comp_arc(cid) else { return };
        let now = ctx.input(|i| i.time);
        if self.playback.playing {
            self.playback.observe(now);
        }
        let fr = c.frame_rate;
        let plan = self.playback.plan.filter(|_| self.playback.playing);
        let (wa, wb) = match plan {
            Some(p) => (p.start, p.end),
            None => (fr.frame_at(c.work_area.0), fr.frame_at(c.work_area.1 - c.frame_duration())),
        };
        let looping = plan.is_none_or(|p| p.looping);
        let video = plan.is_none_or(|p| p.video);
        // Prefetch ahead.
        let cur = fr.frame_at(self.session.time());
        if self.scrubbing() {
            let last = fr.frame_at(c.duration).saturating_sub(1).max(0);
            let nearby = self.scrub_preview.neighbors(cur, last);
            let series = self.frame_series(cid, scale);
            self.frames.set_playhead(None);
            self.frames.protect_frames(series, std::iter::once(cur).chain(nearby.iter().copied()));
            let spare = self.frames_parallelism().saturating_sub(1).min(2).min(self.frames_that_fit(&c, scale).clamp(0, 2) as usize);
            if !self.frames.urgent_pending() && self.editing(now).is_none() {
                for f in nearby.into_iter().take(spare) {
                    if self.frames.inflight() >= spare {
                        break;
                    }
                    self.request_frame(cid, f, scale);
                }
            }
            return;
        }
        // While paused (scrubbing, editing) the viewer's frame comes first: prefetch only a few
        // frames, only once it is done, and not while edits keep coming (a layer dragged in the
        // viewer): those frames would be stale at the next step and would hold the CPU and GPU
        // the next viewer frame needs.
        let ahead = if self.playback.playing {
            (self.frames_parallelism() * 2).max(4) as i64
        } else if self.frames.urgent_pending() {
            0
        } else if let Some(wait) = self.editing(now) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
            0
        } else if self.frames.last_ms.lock().is_ok_and(|ms| *ms > PAUSED_PREFETCH_MAX_MS) {
            // Slow frames: rendering ahead held up the frame the user moved to by seconds.
            0
        } else {
            // Keep workers available for a newly edited viewer frame. Running obsolete
            // background renders cannot be preempted by the priority queue.
            self.frames_parallelism().saturating_sub(1).min(2) as i64
        };
        let ahead = ahead.min(self.frames_that_fit(&c, scale));
        let series = self.frame_series(cid, scale);
        // CR6b: a cloud preview plays from the server's stream; nothing renders or is asked for
        // here meanwhile (the stream's first chunk is the pre-roll).
        let streamed = self.tick_cloud_stream(ctx, &c, series, cur, scale, now);
        let ahead = if streamed { i64::from(effectcraft_engine::remote_stream::DEFAULT_CHUNK_FRAMES).min(self.frames_that_fit(&c, scale)) } else { ahead };
        // The preview's position orders prefetch and eviction (#595); set before counting the
        // frames in flight, as it drops queued frames the preview no longer reaches.
        let playhead = plan.filter(|p| p.video).map(|p| frames::Playhead { series, cur, start: p.start, end: p.end, step: p.step, looping: p.looping });
        self.frames.set_playhead(playhead);
        // Playing, only this series counts: a ladder rung change leaves the old rung's prefetch
        // queued, which held playback at every Full ↔ Half switch (A4).
        let mut queued = if playhead.is_some() { self.frames.focus_series(&series) } else { self.frames.inflight() };
        // In-flight renders far ahead may finish after the near frames. Keep their completion
        // from evicting the very frames playback needs next (#595).
        self.frames.protect_frames(
            series,
            (0..ahead.max(2)).filter_map(|k| match plan {
                Some(p) => p.frame_after(cur, k),
                None => Some(wa + (cur.saturating_sub(wa).saturating_add(k)).rem_euclid(wb.saturating_sub(wa).saturating_add(1).max(1))),
            }),
        );
        if self.playback.caching
            && let Some(p) = plan
        {
            // Cache Before Playback: the range in play order, then start the clock. A range
            // larger than the RAM preview budget is cut to the frames that fit (caching more
            // would evict the first ones and never finish).
            let mut all = true;
            let n = (p.end - p.start) / p.step + 1;
            let fit = self.frames_that_fit(&c, scale);
            if n > fit {
                let mut cut = p;
                (cut.start, cut.end) = (p.first, (p.first + (fit - 1) * p.step).min(p.end));
                self.playback.plan = Some(cut);
                self.toast = Some((format!("Preview cut to {fit} of {n} frames: no more fit in the RAM preview (Settings ▸ Memory & CPU)"), now));
                ctx.request_repaint();
                return;
            }
            // One look at the cache per repaint, not a lock per frame of the range (each
            // finished frame repaints: that was quadratic in the range).
            let cached = self.frames.cached_frames(&series);
            for f in (0..n).filter_map(|k| p.frame_after(p.first, k)) {
                if cached.binary_search(&f).is_ok() {
                    continue;
                }
                all = false;
                if queued < ahead as usize {
                    self.request_frame(cid, f, scale);
                    queued += 1;
                }
            }
            self.playback.waiting = !all;
            if all {
                self.playback.caching = false;
                self.playback.start_wall = now;
                self.playback.start_frame = p.first;
                self.session.set_time(fr.tick_of(p.first));
                self.start_audio(fr.tick_of(p.first));
                ctx.request_repaint();
            } else {
                // Each finished frame wakes the UI (`Frames::release`) to queue the next ones;
                // this is only the fallback.
                ctx.request_repaint_after(PLAYBACK_MAX_WAIT);
            }
            return;
        }
        let step = plan.map_or(1, |p| p.step);
        // Never further ahead than the RAM preview holds: those frames would evict the ones the
        // preview shows first, or be evicted before it gets there (#595).
        let window = (ahead * 3).min(self.frames_that_fit(&c, scale) - 1);
        for k in 0..window {
            if queued >= ahead as usize || !video || streamed {
                break;
            }
            let f = match plan {
                Some(p) => match p.frame_after(cur, k) {
                    Some(f) => f,
                    None => break,
                },
                None => {
                    let mut f = cur + k;
                    if f > wb {
                        f = wa + (f - wb - 1).rem_euclid((wb - wa + 1).max(1));
                    }
                    f
                }
            };
            if !self.frames.is_cached(&FrameKey { frame: f, ..series }) {
                self.request_frame(cid, f, scale);
                queued += 1;
            }
        }
        if let Some(a) = &mut self.audio {
            a.pump();
            self.meter.update(a.feed.take_peaks(), now);
        } else if let Some(s) = &mut self.scrub {
            // Scrubbing: the meters follow the snippets; an idle second closes the output.
            s.pump();
            self.meter.update(s.feed.take_peaks(), now);
            if now - s.last_used > 1.0 {
                self.scrub = None;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if self.meter.active() {
            self.meter.update([0.0; 2], now);
            ctx.request_repaint();
        }
        if !self.playback.playing {
            // Idle caching covers the Preview panel's range, as in After Effects: Play Around
            // Current Time caches the pre-roll before the playhead too, not just the work area.
            let preset = self.session.prefs.preview.get(self.session.prefs.preview.current);
            let range = effectcraft_engine::preview::plan(preset, &c, self.session.time());
            self.cache_when_idle(ctx, &c, &series, now, (range.start, range.end));
            return;
        }
        let snap = |f: i64| wa + (f - wa).div_euclid(step) * step;
        // Held sound starts once the frames ahead are cached (see `Playback::audio_held`): the
        // prefetch window when the silent preview keeps real time, else the rest of the preview.
        let fit = self.frames_that_fit(&c, scale);
        // Pre-roll: the frames ahead are enough to start (the held first frame can't show
        // whether rendering keeps up).
        let preroll = self.playback.preroll.is_some_and(|until| now < until);
        if !preroll {
            self.playback.preroll = None;
        }
        if self.playback.audio_held
            && let Some(p) = plan
            && (((preroll || self.playback.keeps_up(now)) && self.cached_ahead(&p, &series, cur, ahead.min(fit))) || self.cached_ahead(&p, &series, cur, fit))
        {
            self.playback.audio_held = false;
            self.playback.preroll = None;
            self.playback.waiting = false;
            self.playback.start_wall = now;
            self.playback.start_frame = cur;
            self.start_audio(fr.tick_of(cur));
        }
        if preroll && self.playback.audio_held && video {
            // Hold the first frame (rendering ahead) rather than play it without its sound.
            self.playback.waiting = true;
            self.playback.start_wall = now;
            self.playback.start_frame = cur;
            if !streamed && !self.frames.is_cached(&FrameKey { frame: cur, ..series }) {
                self.request_frame_urgent(cid, cur, scale);
            }
            // Each finished frame wakes the UI (`Frames::release`); this is only the fallback.
            ctx.request_repaint_after(PLAYBACK_MAX_WAIT);
            return;
        }
        // Audio clock drives playback: show the frame under the audible sample.
        if let Some(a) = self.audio.as_ref().filter(|a| !a.held()) {
            if a.finished() {
                if video {
                    self.session.set_time(fr.tick_of(wb));
                } else {
                    self.playback.audio_frame = Some(wb);
                }
                self.stop();
                return;
            }
            // One clock observation for this decision: a loop wrap between target selection
            // and scheduling would otherwise turn the next-frame delay into a whole loop.
            let audible = a.clock();
            let target = snap(audio::frame_of_sample(audible, a.rate, fr).clamp(wa, wb));
            if !video {
                self.playback.audio_frame = Some(target);
            } else if self.frames.is_cached(&FrameKey { frame: target, ..series }) {
                self.playback.waiting = false;
                if target != cur {
                    self.session.set_time(fr.tick_of(target));
                    self.playback.frame_shown(now, target);
                }
            } else {
                // Rendering can't keep up: rather than skip frames, the sound holds (the device
                // stays open) and every frame shows as it renders until the frames ahead are
                // cached again.
                if let Some(a) = &mut self.audio {
                    a.hold();
                }
                self.playback.audio_held = true;
                self.playback.waiting = true;
                // The rate measured before the stall no longer says it keeps up.
                self.playback.shown_at.clear();
                self.playback.start_wall = now;
                self.playback.start_frame = cur;
                ctx.request_repaint();
                return;
            }
            // Wake when the audible sample reaches the next frame (not at the display's refresh
            // rate): a 24 fps preview rebuilds the UI 24 times a second, not 120. Play Faster's
            // sound covers that much comp time `speed` × sooner.
            let next = fr.tick_of(target.saturating_add(step.max(1))).seconds();
            ctx.request_repaint_after(until_next_frame((next - audio::sample_time(audible, a.rate).seconds()) / f64::from(a.speed.max(1))));
            return;
        }
        let elapsed = now - self.playback.start_wall;
        let n = ((elapsed * self.playback.fps) / step as f64).floor() as i64;
        // When the wall clock reaches frame `n + 1` (the next UI frame that changes anything).
        let next_due = self.playback.start_wall + (n.saturating_add(1) as f64 * step as f64) / self.playback.fps - now;
        let target = match plan {
            Some(p) => p.frame_after(self.playback.start_frame, n),
            None => {
                let t = self.playback.start_frame + n;
                if t <= wb {
                    Some(t)
                } else if looping {
                    Some(wa + (t - wa).rem_euclid((wb - wa + 1).max(1)))
                } else {
                    None
                }
            }
        };
        let Some(target) = target else {
            if video {
                self.session.set_time(fr.tick_of(wb));
            } else {
                self.playback.audio_frame = Some(wb);
            }
            self.stop();
            return;
        };
        if !video {
            // Audio-only preview without an audio device: the clock runs, the time stays.
            self.playback.audio_frame = Some(target);
            ctx.request_repaint_after(until_next_frame(next_due));
            return;
        }
        if self.frames.is_cached(&FrameKey { frame: target, ..series }) {
            self.playback.waiting = false;
            if target != cur {
                self.session.set_time(fr.tick_of(target));
                self.playback.frame_shown(now, target);
            }
        } else {
            // Not cached yet: hold the clock at the current frame (cache first, then play).
            self.playback.waiting = true;
            let next = plan.and_then(|p| p.frame_after(cur, 1)).unwrap_or(if cur + 1 > wb { wa } else { cur + 1 });
            if self.frames.is_cached(&FrameKey { frame: next, ..series }) {
                self.session.set_time(fr.tick_of(next));
            } else if !streamed {
                // The frame playback waits for renders before any prefetch (the prefetch window
                // can be full of frames further on, or of other comps').
                self.request_frame_urgent(cid, next, scale);
            }
            self.playback.start_wall = now;
            self.playback.start_frame = fr.frame_at(self.session.time());
            // Held at this frame: each finished frame wakes the UI (`Frames::release`).
            ctx.request_repaint_after(until_next_frame(step as f64 / self.playback.fps));
            return;
        }
        ctx.request_repaint_after(until_next_frame(next_due));
    }

    /// CR6b: while a cloud preview plays, its frames come from the server's playback stream
    /// ([`cloud_stream`]): start, feed and stop it, and publish its frames into the RAM cache.
    /// Whether the stream delivers this preview's frames (nothing renders or is asked for here).
    #[cfg(not(target_arch = "wasm32"))]
    fn tick_cloud_stream(&mut self, ctx: &egui::Context, c: &Arc<effectcraft_engine::project::Comp>, series: FrameKey, cur: i64, scale: f64, now: f64) -> bool {
        let plan = self.playback.plan.filter(|p| self.playback.playing && p.video && !self.playback.caching);
        let enabled = std::env::var("EFFECTCRAFT_CLOUD_STREAM").map_or(true, |v| v != "0");
        let Some(cid) = self.session.active_comp_id() else { return false };
        if plan.is_none() && !self.cloud_play.active() {
            return false;
        }
        let project = self.session.freeze_preview_project();
        let p = cloud_stream::PlayNow {
            playing: plan.is_some() && enabled && self.frames.remote_active(),
            now,
            comp: cid,
            rate: c.frame_rate,
            project: &project,
            revision: self.session.revision,
            series,
            opts: self.frame_opts(cid, scale),
            cur,
            start: plan.map_or(cur, |p| p.start),
            end: plan.map_or(cur, |p| p.end),
            looping: plan.is_some_and(|p| p.looping),
            waiting: self.playback.waiting,
            fit: self.frames_that_fit(c, scale),
            disk: self.session.disk_cache.clone(),
        };
        let client = self.cloud.client().cloned();
        let t = self.cloud_play.tick(client.as_ref(), &p, &self.frames, Some(ctx));
        if let Some(note) = t.note {
            self.ui.status = note;
        }
        t.streamed
    }

    #[cfg(target_arch = "wasm32")]
    fn tick_cloud_stream(&mut self, _: &egui::Context, _: &Arc<effectcraft_engine::project::Comp>, _: FrameKey, _: i64, _: f64, _: f64) -> bool {
        false
    }

    /// While the project keeps changing (an edit within [`EDIT_QUIET`] seconds), how long until
    /// it counts as settled.
    fn editing(&mut self, now: f64) -> Option<f64> {
        if self.last_edit.1 != self.session.revision {
            self.last_edit = (now, self.session.revision);
        }
        let left = EDIT_QUIET - (now - self.last_edit.0);
        (left > 0.0).then_some(left)
    }

    /// Composition ▸ Preview ▸ Cache Frames When Idle: after a second without input or edits,
    /// render the preview range's frames (from the current time on, then from its start) in the
    /// background, one at a time on a few low-priority threads (`frames::idle`), until it is
    /// cached or the RAM preview budget is full (then into the disk cache). It waits while the
    /// system is short of memory ([`idle_cache::memory_wait`]).
    fn cache_when_idle(&mut self, ctx: &egui::Context, comp: &effectcraft_engine::project::Comp, series: &FrameKey, now: f64, (wa, wb): (i64, i64)) {
        // Moving the pointer over the window isn't activity (After Effects keeps caching):
        // clicks, keys, wheel and drags are.
        let active = ctx.input(|i| {
            i.pointer.any_down()
                || i.events
                    .iter()
                    .any(|e| !matches!(e, egui::Event::PointerMoved(_) | egui::Event::MouseMoved(_) | egui::Event::PointerGone | egui::Event::WindowFocused(_)))
        });
        if active || self.last_activity.1 != self.session.revision {
            self.last_activity = (now, self.session.revision);
        }
        if !self.ui.cache_when_idle || self.frames.urgent_pending() {
            return;
        }
        let quiet = now - self.last_activity.0;
        if quiet < 1.0 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(1.0 - quiet));
            return;
        }
        // An empty work area has nothing to cache.
        let n = wb - wa + 1;
        if n < 1 {
            return;
        }
        let cur = comp.frame_rate.frame_at(self.session.time()).clamp(wa, wb);
        // The cloud (CR8): its server renders the frames into its own cache, nothing downloads.
        if self.cache_on_server(ctx, comp, series, (wa, wb), cur) {
            return;
        }
        let remote = cfg!(target_arch = "wasm32") || self.frames.remote_active();
        if !remote && self.idle_waits_for_memory(now) {
            ctx.request_repaint_after(std::time::Duration::from_secs(5));
            return;
        }
        let fit = self.frames_that_fit(comp, series.scale as f64 / 1000.0).min(n);
        // Rendered elsewhere (the cloud, the browser's workers), nothing here slows down; here,
        // one frame at a time on the idle threads, and one queued behind it.
        let slots = if remote { (self.frames_parallelism() / 2).max(1) } else { self.frames.idle_slots() };
        let mut queued = self.frames.inflight();
        let scale = series.scale as f64 / 1000.0;
        let mut pending = false;
        let cached = self.frames.cached_frames(series);
        for k in 0..fit {
            let f = wa + (cur - wa + k).rem_euclid(n);
            if cached.binary_search(&f).is_ok() {
                continue;
            }
            pending = true;
            if queued >= slots {
                break;
            }
            self.request_frame_idle(ItemId(series.comp), f, scale);
            queued += 1;
        }
        // The RAM preview is full: keep going into the disk cache (the blue bar), as After
        // Effects does, without evicting the frames in RAM. Paced by the disk writer's queue.
        if !pending && fit < n {
            pending = self.cache_to_disk(comp, series, (wa, n), cur, fit, slots, queued);
        }
        if pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Whether Cache Frames When Idle waits for memory now ([`idle_cache::memory_wait`]); says
    /// so in a toast when it starts waiting (at most every five minutes).
    fn idle_waits_for_memory(&mut self, now: f64) -> bool {
        let why = idle_cache::memory_wait(self.session.sys_memory, self.session.memory_watch.pressure(), self.session.prefs.memory.ram_reserved_gb);
        let (waiting, told) = self.idle_memory_wait;
        if let Some(why) = why
            && !waiting
        {
            log::info!("Cache Frames When Idle: paused, {why}");
            if now - told > 300.0 {
                self.session.toast(format!("Cache Frames When Idle paused: {why}"));
                self.idle_memory_wait.1 = now;
            }
        } else if why.is_none() && waiting {
            log::info!("Cache Frames When Idle: memory freed up, caching again");
        }
        self.idle_memory_wait.0 = why.is_some();
        why.is_some()
    }

    /// [`Self::cache_when_idle`] with the cloud renderer: ask its server to cache the work area
    /// from `cur`, then the rest of the comp, in its own frame cache. Whether the cloud took it
    /// (or will once its link is ready): then nothing is cached here.
    fn cache_on_server(&self, ctx: &egui::Context, comp: &effectcraft_engine::project::Comp, series: &FrameKey, (wa, wb): (i64, i64), cur: i64) -> bool {
        let last = comp.frame_rate.frame_at(comp.duration).saturating_sub(1).max(wb);
        let ranges: Vec<(i64, i64)> = [(cur, wb), (wa, cur - 1), (wb + 1, last), (0, wa - 1)].into_iter().filter(|(a, b)| a <= b).collect();
        let cid = ItemId(series.comp);
        let opts = self.frame_opts(cid, series.scale as f64 / 1000.0);
        match self.frames.precache_remote(&self.render_source(), series, cid, opts, ranges) {
            None => false,
            Some(sent) => {
                // Not sent yet (the content key is on its way), or the server is still caching:
                // look again (the cache bar follows its reports).
                #[cfg(not(target_arch = "wasm32"))]
                let busy = self.cloud.status().and_then(|s| s.server_cache).is_none_or(|c| c.done < c.total);
                #[cfg(target_arch = "wasm32")]
                let busy = false;
                if !sent || busy {
                    ctx.request_repaint_after(std::time::Duration::from_millis(if sent { 1000 } else { 200 }));
                }
                true
            }
        }
    }

    /// [`Self::cache_when_idle`] past the RAM preview budget: queue the work area's next frames
    /// (from the `fit`th after `cur` on) that are neither in RAM nor on disk as disk-only
    /// renders, while fewer than `slots` frames are rendering. Returns whether any remain.
    fn cache_to_disk(
        &self,
        comp: &effectcraft_engine::project::Comp,
        series: &FrameKey,
        (wa, n): (i64, i64),
        cur: i64,
        fit: i64,
        slots: usize,
        mut queued: usize,
    ) -> bool {
        let Some(dc) = self.session.disk_cache.clone() else { return false };
        let cid = ItemId(series.comp);
        let scale = series.scale as f64 / 1000.0;
        let frame_bytes = ((comp.width as f64 * scale).ceil() * (comp.height as f64 * scale).ceil() * 4.0) as usize;
        if !dc.has_room(frame_bytes) {
            // The writer is still busy with earlier frames: look again shortly.
            return true;
        }
        let src = self.render_source();
        let opts = self.frame_opts(cid, scale);
        let total = comp.frame_rate.frame_at(comp.duration);
        let on_disk = self.frames.disk_frames(&src, series, total, &opts);
        let mut pending = false;
        for k in fit..n {
            let f = wa + (cur - wa + k).rem_euclid(n);
            if on_disk.binary_search(&f).is_ok() || self.frames.is_cached(&FrameKey { frame: f, ..*series }) {
                continue;
            }
            if queued >= slots {
                return true;
            }
            if self.frames.request_to_disk(&src, self.frame_key(cid, f, scale), cid, comp.frame_rate.tick_of(f), opts) {
                queued += 1;
                pending = true;
            }
        }
        pending || queued > 0
    }

    /// How many viewer frames of `comp` at `scale` the RAM preview budget holds (90 % of it,
    /// at least one).
    /// The frames of `plan` from `cur` to its end, or as many as fit in the RAM preview, are
    /// cached: sound can play from `cur` in real time.
    fn cached_ahead(&self, plan: &effectcraft_engine::preview::PreviewPlan, series: &FrameKey, cur: i64, fit: i64) -> bool {
        let cached = self.frames.cached_frames(series);
        (0..fit).map_while(|k| plan.frame_after(cur, k)).take_while(|f| *f >= cur).all(|f| cached.binary_search(&f).is_ok())
    }

    fn frames_that_fit(&self, comp: &effectcraft_engine::project::Comp, scale: f64) -> i64 {
        let side = |n: u32| (n as f64 * scale).ceil().max(1.0);
        // Compressed frames take less (the ratio measured on the cached ones).
        let frame = self.frames.frame_bytes(side(comp.width) * side(comp.height) * 4.0).max(1.0);
        ((self.frames.plan_budget() as f64 * 0.9 / frame) as i64).max(1)
    }

    fn frames_parallelism(&self) -> usize {
        if cfg!(target_arch = "wasm32") {
            // frame workers render one frame each; without them frames render one at a time
            // on the UI thread (`Frames::pump`)
            return self.frames.remote_slots().max(1);
        }
        if self.frames.remote_active() {
            // The cloud: as many as cover its round trip (CR5).
            return self.frames.remote_slots();
        }
        self.frames.parallelism()
    }

    // ---------------------------------------------------------------- control channel

    pub(crate) fn raise_for_control(&mut self, ctx: &egui::Context) {
        ctx.request_repaint();
    }

    fn drain_control(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.control_rx.take() else { return };
        let now = ctx.input(|i| i.time);
        let mut reqs: Vec<(ControlRequest, f64)> = std::mem::take(&mut self.deferred);
        while let Ok(req) = rx.try_recv() {
            reqs.push((req, now + 3.0));
        }
        // Agents sharing the app take turns (one request per client before anyone's next), and a
        // frame runs at most CONTROL_FRAME_BUDGET of requests so the window keeps drawing; the
        // rest run on the next frame.
        let started = web_time::Instant::now();
        for (req, deadline) in control::fair_order(reqs) {
            if started.elapsed() > control::CONTROL_FRAME_BUDGET {
                self.deferred.push((req, deadline));
                ctx.request_repaint();
                continue;
            }
            // The transport gave up on it (timed out) and told the client it didn't run.
            if !req.state.start() {
                continue;
            }
            let reply = req.reply.clone();
            match control::handle(self, ctx, &req) {
                control::Outcome::Done(v) => {
                    let _ = reply.send(v);
                }
                control::Outcome::Retry(msg) => {
                    if now < deadline {
                        self.deferred.push((req, deadline));
                        ctx.request_repaint();
                    } else {
                        let _ = reply.send(json!({"ok": false, "error": msg}));
                    }
                }
                control::Outcome::AfterInput => self.input_waiters.push(reply),
                // The handler kept the reply and answers later (the Figma pairing prompt).
                control::Outcome::Held => {}
                control::Outcome::AwaitJobs(w) => {
                    self.job_waiters.push((w, reply));
                    ctx.request_repaint();
                }
                control::Outcome::Screenshot { path, crop, max_side } => {
                    // A covered or minimized window presents no frames (macOS skips its redraws), so
                    // a screenshot would never arrive and nothing would tick its timeout.
                    if ctx.input(|i| i.viewport().occluded == Some(true) || i.viewport().minimized == Some(true)) {
                        let _ = reply.send(json!({"ok": false, "error": "window is covered or minimized: call ui.focus first (render.frame renders comp pixels without the window)"}));
                        continue;
                    }
                    let token = self.next_token;
                    self.next_token += 1;
                    let settle = now + 0.35;
                    self.queued_screenshots.push((token, settle, 0));
                    self.pending_screenshots.push((token, path, crop, max_side, reply, settle + SCREENSHOT_TIMEOUT_S));
                }
            }
        }
        self.control_rx = Some(rx);
        self.finish_job_waiters(ctx);
    }

    /// Tell `events.subscribe` connections that the project (revision) or the layer selection
    /// changed: one `project.changed` / `selection.changed` line per change, at most ten bursts a
    /// second (later changes wait for the next slot and are coalesced).
    fn publish_control_events(&mut self, ctx: &egui::Context) {
        let Some(events) = &self.control_events else { return };
        let revision = self.session.revision;
        events.set_revision(revision);
        let selection = &self.session.state.selected_layers;
        let (project, selected) = (revision != self.events_sent.1, *selection != self.events_sent.2);
        if !project && !selected {
            return;
        }
        let now = ctx.input(|i| i.time);
        if events.subscribers() > 0 {
            let wait = self.events_sent.0 + 0.1 - now;
            if wait > 0.0 {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
                return;
            }
            if project {
                events.publish(&json!({"event": "project.changed", "revision": revision}));
            }
            if selected {
                let layers: Vec<u64> = selection.iter().map(|l| l.0).collect();
                events.publish(&json!({"event": "selection.changed", "revision": revision, "layers": layers}));
            }
        }
        self.events_sent = (now, revision, selection.clone());
    }

    /// Reply to the control requests whose offloaded jobs ended.
    fn finish_job_waiters(&mut self, ctx: &egui::Context) {
        if self.job_waiters.is_empty() {
            return;
        }
        self.session.poll_render();
        self.session.poll_offload();
        let waiters = std::mem::take(&mut self.job_waiters);
        for (w, reply) in waiters {
            if w.pending(&self.session) {
                self.job_waiters.push((w, reply));
            } else {
                let v = w.finish(&mut self.session);
                let _ = reply.send(v);
            }
        }
        if !self.job_waiters.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn issue_screenshots(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let mut any = false;
        let busy = self.frames.inflight() > 0;
        self.queued_screenshots.retain_mut(|(token, at, frames)| {
            *frames += 1;
            any = true;
            // Wait for the viewer's frame to finish rendering (bounded by the timeout below).
            if now >= *at && *frames >= 3 && (!busy || now > *at + 2.5) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(*token)));
                false
            } else {
                true
            }
        });
        self.pending_screenshots.retain(|(.., reply, deadline)| {
            if now > *deadline {
                let _ = reply.send(json!({"ok": false, "error": "no frame was presented (window hidden or display asleep)"}));
                false
            } else {
                true
            }
        });
        if any {
            ctx.request_repaint();
        } else if !self.pending_screenshots.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn collect_screenshots(&mut self, ctx: &egui::Context) {
        if self.pending_screenshots.is_empty() {
            return;
        }
        let events: Vec<_> = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Screenshot { user_data, image, .. } => {
                        let token = user_data.data.as_ref().and_then(|d| d.downcast_ref::<u64>()).copied()?;
                        Some((token, image.clone()))
                    }
                    _ => None,
                })
                .collect()
        });
        for (token, image) in events {
            if let Some(i) = self.pending_screenshots.iter().position(|(t, ..)| *t == token) {
                let (_, path, crop, max_side, reply, _) = self.pending_screenshots.remove(i);
                let r = control::save_screenshot(ctx, &image, path.as_deref(), crop, max_side);
                let _ = reply.send(r);
            }
        }
    }

    // ---------------------------------------------------------------- frame

    fn handle_events(&mut self, ctx: &egui::Context) {
        for ev in self.session.drain_events() {
            match ev {
                effectcraft_engine::Event::OpenComp(c) => {
                    // (The Timeline puts back the comp's own zoom: `TimelineState::comp_views`.)
                    panels::viewers::on_open_comp(self, c);
                }
                effectcraft_engine::Event::Toast { message, .. } => self.toast = Some((message, ctx.input(|i| i.time))),
                effectcraft_engine::Event::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                // Frames are keyed by content: an edit keeps those of comps it doesn't touch.
                effectcraft_engine::Event::ProjectChanged { .. } => {}
                effectcraft_engine::Event::Frontend { command, params } => {
                    if let Err(e) = crate::menus::frontend(self, ctx, &command, params) {
                        self.ui.status = e;
                    }
                }
                effectcraft_engine::Event::PurgeCaches => {
                    self.frames.clear();
                    if let Some(gpu) = &self.gpu {
                        gpu.context().clear_uploads();
                    }
                }
                effectcraft_engine::Event::CopyImage { width, height, rgba } => {
                    if let Err(e) = copy_image(ctx, width, height, &rgba) {
                        self.ui.status = e;
                    }
                }
            }
        }
    }

    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.playback.playing || ctx.input(|i| !i.events.is_empty()) {
            self.session.freeze_runtime.activity();
        }
        freeze_hint::feed(self, &ctx);
        self.session.poll_freeze();
        if self.session.project.freeze.sections.iter().any(|s| s.auto && s.enabled) {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        } else if self.session.prefs.previews.freeze_auto_heavy {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        self.scrub_preview.tick(ctx.input(|i| i.time), self.session.active_comp_id().map(|id| id.0), self.session.revision);
        if let Some(wait) = self.scrub_preview.remaining() {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
        }
        self.tick_sound_preview(&ctx, ctx.input(|i| i.time));
        self.apply_gpu_failure(&ctx);
        self.run_dialog_answers();
        self.auto.begin_frame();
        if let Some(rect) = crate::color_picker::take_automation(&ctx) {
            self.auto.add("colorPicker.eyedropper", rect, "Eyedropper");
        }
        self.frames.set_context(&ctx);
        if self.session.render_job.is_some() {
            self.session.poll_render();
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.session.track_job.is_some() {
            self.session.poll_track();
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if self.session.mask_job.is_some() {
            self.session.poll_mask_track();
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        // Background tasks (Content-Aware Fill, Scene Edit Detection, imports).
        if !self.session.tasks.is_empty() {
            self.session.poll_jobs();
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Analyses running in a worker (the web app).
        if !self.session.offloaded.is_empty() {
            self.session.poll_offload();
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Warp Stabilizer: finish analyses and start queued (re-)analyses in the background.
        if self.session.warp_job.is_some() || !self.session.warp_pending.is_empty() {
            self.session.poll_warp(true);
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Roto Brush propagation and Object Matte tracking / Freeze: finish jobs (a Freeze writes
        // its mattes) and start queued ones in the background.
        if self.session.roto_job.is_some() || !self.session.roto_pending.is_empty() {
            self.session.poll_roto(true);
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // 3D Camera Tracker: likewise (tracking, then solving).
        if self.session.camera_job.is_some() || !self.session.camera_pending.is_empty() {
            self.session.poll_camera(true);
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.run_script_tasks(&ctx);
        self.apply_prefs(&ctx);
        self.handle_events(&ctx);
        panels::unsaved::on_close_requested(self, &ctx);
        let title = panels::unsaved::window_title(self);
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
        self.tick_autosave(&ctx);
        prefs_live::frame(self, &ctx);
        panels::timeline_columns::store_layout(self, &ctx);
        panels::motion_library::store_display(self, &ctx);
        if let Some(rx) = self.command_inbox.take() {
            while let Ok(id) = rx.try_recv() {
                if let Err(e) = menus::invoke(self, &ctx, &id, json!({})) {
                    self.ui.status = e;
                }
            }
            self.command_inbox = Some(rx);
        }
        // Tab moves between fields only while typing in one; anywhere else plain Tab is the
        // Composition Mini-Flowchart, as in After Effects (Shift+Tab walks keyboard focus). Taken
        // from egui before any widget runs: its focus move would focus the first control in this
        // very pass, and that control would swallow the Tab.
        let tab_had_widget_focus = ctx.text_edit_focused();
        let plain_tab =
            ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::Tab, pressed: true, modifiers, .. } if !modifiers.any())));
        if plain_tab && !tab_had_widget_focus {
            ctx.memory_mut(|m| {
                m.move_focus(egui::FocusDirection::None);
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        // Batch 1 (AX2/AX4): a held G / A opens the Graph / Layer wheel; a tap keeps its shortcut.
        panels::timeline::wheels::hold_keys(self, &ctx);
        // Batch 1 lane C: ⌘V with an Instagram link opens the paste wheel (CX2).
        panels::cards::before_shortcuts(self, &ctx);
        menus::handle_shortcuts(self, &ctx);
        if !ctx.input(|i| i.pointer.primary_down()) {
            self.ui.viewer.property_interacting = false;
        }
        let t = self.tokens;
        let full = ui.max_rect();
        if self.dialog.is_some() {
            ui.disable();
        }
        ui.painter().rect_filled(full, 0.0, t.app_bg);
        let mut top = full.min.y;
        if self.ui.show_menu_bar {
            let mb = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 24.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(mb));
            child.painter().rect_filled(mb, 0.0, t.header_bg);
            menus::menu_bar(self, &mut child);
            top = mb.max.y;
        }
        let header_h = 40.0;
        let header = egui::Rect::from_min_size(egui::pos2(full.min.x, top), egui::vec2(full.width(), header_h));
        header::show(self, ui, header);
        let body = egui::Rect::from_min_max(egui::pos2(full.min.x + 4.0, header.max.y + 2.0), egui::pos2(full.max.x - 4.0, full.max.y - 4.0));
        self.dock_area(ui, body);
        #[cfg(not(target_arch = "wasm32"))]
        prefs_live::dropped_files(self, ui, full);
        if !tab_had_widget_focus {
            menus::handle_tab_shortcut(self, &ctx);
        }
        panels::precomp::mini_flowchart(self, &ctx);
        panels::home::capture_thumbnail(self);
        panels::dialogs::show(self, &ctx);
        panels::figma_pair::show(self, &ctx);
        panels::fx_console::show(self, &ctx);
        // Batch 1 wheels (AX1): drawn above the panels, then their slices registered.
        wheel::paint(&ctx);
        wheel::register_automation(&ctx, &mut self.auto);
        // Batch 1 lane C: card / paste wheel picks, link fetches (CX1, CX2).
        panels::cards::frame(self, &ctx);
        // Batch 1 lane B: finished transcriptions are imported (SX2).
        transcribe::poll(self, &ctx);
        // Send to FigCraft: toast the sends that finished.
        figcraft_link::poll(self, &ctx);
        panels::timeline::wheels::take_picks(self, &ctx);
        panels::scriptui_view::sync_panels(self);
        panels::scriptui_view::show_windows(self, &ctx);
        panels::learn::coach(self, &ctx);
        let lift = panels::media_panels::import_card(self, ui, full);
        self.draw_toast(ui, full, lift);
        if self.session.config.is_some() && self.workspace_store.has_changes(&self.ui) && !ctx.input(|i| i.pointer.any_down()) {
            if self.workspace_store.due(ctx.input(|i| i.time)) {
                self.save_workspace_state();
            }
            if self.workspace_store.has_changes(&self.ui) {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
        }
    }

    fn save_workspace_state(&mut self) {
        if let Err(error) = self.workspace_store.save(self.session.config.as_deref(), &self.ui) {
            if self.workspace_store.error.as_ref() != Some(&error) {
                log::warn!("{error}");
                self.ui.status = error.clone();
            }
            self.workspace_store.error = Some(error);
        }
    }

    /// The status message or the latest toast, bottom left (`lift` points higher while the
    /// Importing card is there).
    fn draw_toast(&mut self, ui: &mut egui::Ui, full: egui::Rect, lift: f32) {
        let now = ui.input(|i| i.time);
        let msg = if !self.ui.status.is_empty() {
            Some(self.ui.status.clone())
        } else {
            self.toast.as_ref().filter(|(_, at)| now - at < 4.0).map(|(m, _)| m.clone())
        };
        if let Some(m) = msg {
            let t = &self.tokens;
            let galley = ui.painter().layout_no_wrap(m.clone(), Tokens::ui(12.0), t.text);
            let r = egui::Rect::from_min_size(egui::pos2(full.min.x + 16.0, full.max.y - 44.0 - lift), galley.size() + egui::vec2(24.0, 14.0));
            let rows: Vec<egui::Rect> = panels::suggestions::TOAST_CLEAR_OF
                .iter()
                .filter_map(|id| self.auto.find(id).filter(|e| e.window.is_none()))
                .map(|e| egui::Rect::from_min_size(egui::pos2(e.rect[0], e.rect[1]), egui::vec2(e.rect[2], e.rect[3])))
                .collect();
            let r = panels::suggestions::toast_clear_of(r, &rows);
            ui.painter().rect_filled(r, 6.0, t.panel_bg);
            ui.painter().rect_stroke(r, 6.0, egui::Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
            ui.painter().galley(r.min + egui::vec2(12.0, 7.0), galley, t.text);
            self.auto.add("toast", r, &m);
            let resp = ui.interact(r, egui::Id::new("toast"), egui::Sense::click());
            if resp.clicked() {
                self.ui.status.clear();
                self.toast = None;
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            if !self.ui.status.is_empty() {
                let since: f64 = ui.data(|d| d.get_temp(egui::Id::new("status-since")).unwrap_or(now));
                ui.data_mut(|d| d.insert_temp(egui::Id::new("status-since"), since));
                if now - since > 5.0 {
                    self.ui.status.clear();
                    ui.data_mut(|d| d.remove::<f64>(egui::Id::new("status-since")));
                }
            }
        }
    }
}

impl EffectcraftApp {
    /// Scripts' `app.scheduleTask` tasks that are due (repeating ones keep a ScriptUI panel
    /// fresh): errors as a toast, the last line printed in the status bar, and a repaint when
    /// the next one is due.
    fn run_script_tasks(&mut self, ctx: &egui::Context) {
        if self.session.script_ui.task_hosts.is_empty() {
            return;
        }
        let Some(r) = effectcraft_engine::scriptui::ScriptUi::run_tasks(&mut self.session) else { return };
        if let Some(msg) = r["errors"].as_array().and_then(|e| e.first()).and_then(|e| e["message"].as_str()) {
            self.toast = Some((format!("Scheduled script task: {msg}"), ctx.input(|i| i.time)));
        } else if let Some(line) = r["output"].as_array().and_then(|o| o.last()).and_then(|l| l.as_str()) {
            self.ui.status = line.to_string();
        }
        if let Some(next) = r["next"].as_u64() {
            ctx.request_repaint_after(std::time::Duration::from_millis(next));
        }
    }

    /// Auto-save a dirty project when the interval has passed (Settings ▸ Project ▸ Auto-Save).
    fn tick_autosave(&mut self, ctx: &egui::Context) {
        let now = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if let Some(Err(e)) = self.session.autosave_tick(now) {
            self.ui.status = e.to_string();
        }
        if self.session.prefs.auto_save.enabled {
            ctx.request_repaint_after(std::time::Duration::from_secs(30));
        }
        // File ▸ Watch Folder: look for new projects every 10 seconds.
        if self.session.state.watch_folder.is_some() {
            let key = egui::Id::new("watchFolder.lastPoll");
            let last: f64 = ctx.data(|d| d.get_temp(key)).unwrap_or(0.0);
            if now - last >= 10.0 {
                ctx.data_mut(|d| d.insert_temp(key, now));
                match self.session.execute("file.watchFolder.poll", serde_json::json!({})) {
                    Ok(r) => {
                        if let Some(n) = r["rendered"].as_array().map(Vec::len).filter(|n| *n > 0) {
                            self.ui.status = format!("Watch Folder: rendered {n} project(s)");
                        }
                    }
                    Err(e) => self.ui.status = e.to_string(),
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(10));
        }
        self.tick_footage_reload(ctx, now);
    }

    /// Settings ▸ Import ▸ Reload Footage Changed on Disk: every two seconds, and as soon as the
    /// window comes back to the front (after saving in another app), stamp the footage files on a
    /// worker thread and reload the ones that changed.
    fn tick_footage_reload(&mut self, ctx: &egui::Context, now: f64) {
        use effectcraft_engine::footage_reload;
        if let Some(Err(e)) = footage_reload::poll_scan(&mut self.session) {
            self.ui.status = e.to_string();
        }
        let (last_key, focus_key) = (egui::Id::new("footageReload.lastScan"), egui::Id::new("footageReload.focused"));
        let focused = ctx.input(|i| i.focused);
        let was_focused: bool = ctx.data(|d| d.get_temp(focus_key)).unwrap_or(focused);
        ctx.data_mut(|d| d.insert_temp(focus_key, focused));
        if self.session.prefs.import.auto_reload_footage == "off" || self.session.project.items.is_empty() {
            return;
        }
        let last: f64 = ctx.data(|d| d.get_temp(last_key)).unwrap_or(0.0);
        if now - last >= 2.0 || (focused && !was_focused) {
            ctx.data_mut(|d| d.insert_temp(last_key, now));
            footage_reload::start_scan(&mut self.session);
        }
        if self.session.footage_scan.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
    }

    /// Take the pending synthetic input (from `ui.click`, `ui.key`, …). Hosts that don't call
    /// [`eframe::App::raw_input_hook`] (the headless test harness) feed these in themselves.
    pub fn take_synthetic_input(&mut self) -> Vec<egui::Event> {
        std::mem::take(&mut self.synthetic)
    }
}

impl eframe::App for EffectcraftApp {
    fn on_exit(&mut self) {
        self.save_workspace_state();
        // Clean exit: no crash recovery next launch (unless the graphics device was lost with
        // unsaved changes, which are in the auto-save it offers).
        if self.device_lost_close != Some(true) {
            self.session.end_recovery();
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let injected = !self.synthetic.is_empty();
        self.feed_synthetic(raw_input);
        if pointer_wrap::prepare_input(ctx, raw_input, injected) {
            self.session.history.merge_key = None;
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_gpu_failure(ctx);
        ctx.set_cursor_image(None);
        // The CJK fallback fonts depend on the interface language, so they follow a change of
        // Settings ▸ General ▸ Language instead of only the first frame.
        let language = crate::i18n::language(self);
        crate::i18n::set_current(language);
        if !self.styled || self.styled_language != language {
            theme::install(ctx, &self.tokens, language);
            self.styled_language = language;
            if !self.styled {
                fit_window(ctx);
                self.styled = true;
            }
            ctx.request_repaint();
        } else {
            self.fonts_ready = true;
        }
        let now = ctx.input(|i| i.time);
        let dt = (now - self.last_time) as f32;
        if dt > 0.0 {
            self.fps = self.fps * 0.9 + (1.0 / dt).min(480.0) * 0.1;
        }
        self.last_time = now;
        // Back in the app (footage may have been edited elsewhere): the disk cache's keys stat
        // footage files again.
        let focused = ctx.input(|i| i.focused);
        let focus_id = egui::Id::new("app-was-focused");
        if focused && !ctx.data(|d| d.get_temp::<bool>(focus_id)).unwrap_or(true) {
            effectcraft_engine::render::disk_cache::refresh_file_stamps();
        }
        ctx.data_mut(|d| d.insert_temp(focus_id, focused));
        self.drain_control(ctx);
        self.publish_control_events(ctx);
        if !self.synthetic.is_empty() || float_windows::input_pending(ctx) {
            ctx.request_repaint();
        } else if !self.input_waiters.is_empty() {
            for w in self.input_waiters.drain(..) {
                let _ = w.send(json!({"ok": true, "result": null}));
            }
        }
        self.issue_screenshots(ctx);
        self.collect_screenshots(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.gpu_failure_applied == Some(true) {
            self.close_after_device_loss(ui.ctx());
            return;
        }
        self.init_gpu(frame, ui.ctx());
        if !self.fonts_ready {
            ui.ctx().request_repaint();
            return;
        }
        let started = ui.ctx().input(|i| i.time);
        self.frame(ui);
        let ctx = ui.ctx().clone();
        if self.latency.edit_revision != self.session.revision {
            self.latency.edit_revision = self.session.revision;
            self.latency.edit_at = started;
        }
        let shown = self.viewer_shown.as_ref().map(|(_, k)| (k.revision, k.frame, k.scale));
        if shown != self.latency.shown {
            self.latency.shown = shown;
            self.latency.shown_at = started;
        }
        #[cfg(not(target_arch = "wasm32"))]
        pointer_wrap::finish_native(&ctx, frame);
        #[cfg(not(target_arch = "wasm32"))]
        self.tick_cloud(&ctx);
        // wasm32: no frame threads; render queued frames now, between UI frames.
        if cfg!(target_arch = "wasm32") && self.frames.pump(std::time::Duration::from_millis(if self.playback.playing { 24 } else { 40 })) {
            ctx.request_repaint();
        }
        self.last_ui_time = ctx.input(|i| i.time);
        if !self.synthetic.is_empty() || float_windows::input_pending(&ctx) {
            ctx.request_repaint();
        } else if !self.input_waiters.is_empty() {
            ctx.request_repaint();
            for w in self.input_waiters.drain(..) {
                let _ = w.send(json!({"ok": true, "result": null}));
            }
        }
        if self.frames.inflight() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        // Edits made by this frame's input.
        self.publish_control_events(&ctx);
    }
}

/// How many of the queued synthetic events (`ui.click`, `ui.key` …) go into the next frame:
/// pointer events one per frame, so egui sees press → moves → release as a real drag; keys up to
/// and including the next release.
pub(crate) fn synthetic_batch(queue: &[egui::Event]) -> usize {
    let pointer = |e: &egui::Event| matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. } | egui::Event::MouseWheel { .. });
    match queue.first() {
        None => 0,
        Some(e) if pointer(e) => 1,
        Some(_) => queue
            .iter()
            .position(|e| pointer(e) || matches!(e, egui::Event::Key { pressed: false, .. }))
            .map_or(queue.len(), |i| if queue.get(i).is_some_and(pointer) { i.max(1) } else { i + 1 }),
    }
}

/// Advance playback with the viewer's scale (called by the viewer panel each frame).
pub(crate) fn tick_playback(app: &mut EffectcraftApp, ctx: &egui::Context, scale: f64) {
    app.advance_playback(ctx, scale);
}

/// Put `width` × `height` straight-alpha RGBA8 pixels on the system clipboard (Composition ▸ Copy
/// Frame to Clipboard). The pixel count is checked first: egui's image constructor panics on a
/// size that doesn't match its buffer.
fn copy_image(ctx: &egui::Context, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Err(format!("cannot copy a malformed {width}×{height} image to the clipboard"));
    }
    ctx.copy_image(egui::ColorImage::from_rgba_unmultiplied([w, h], rgba));
    Ok(())
}

/// Maximize a first window that doesn't fit the screen, which fits it to the space beside the
/// taskbar or dock. The 1680 × 1020 window on a smaller screen is only shrunk to the monitor's
/// size, so with its title bar it ran under the taskbar and cut menus off at the bottom of the
/// screen (#269).
fn fit_window(ctx: &egui::Context) {
    let (monitor, window) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().outer_rect.or(i.viewport().inner_rect)));
    if let (Some(monitor), Some(window)) = (monitor, window)
        && overflows_screen(window.size(), monitor)
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }
}

/// Whether a window `size` leaves no room on a `monitor` for a taskbar or dock (points).
fn overflows_screen(size: egui::Vec2, monitor: egui::Vec2) -> bool {
    const TASKBAR: f32 = 64.0;
    size.x > monitor.x || size.y > monitor.y - TASKBAR
}

#[cfg(test)]
mod playback_pacing_tests {
    use super::{PLAYBACK_MAX_WAIT, until_next_frame};
    use std::time::Duration;

    #[test]
    fn audio_loop_boundary_schedules_the_next_frame_without_an_extra_pause() {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct WrappingClock(Arc<AtomicUsize>);
        impl audio::AudioDevice for WrappingClock {
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn start(&mut self, _: Arc<audio::AudioFeed>) -> Result<(), String> {
                Ok(())
            }
            fn stop(&mut self) {}
            fn audible_frames(&self) -> Option<u64> {
                // Cross a one-second loop between consecutive observations.
                Some(if self.0.fetch_add(1, Ordering::Relaxed) == 0 { 47_999 } else { 48_000 })
            }
        }
        let mut session = effectcraft_engine::Session::default();
        let cid = ItemId(session.execute("comp.new", json!({"width": 8, "height": 8, "duration": 1, "frameRate": 30})).unwrap()["comp"].as_u64().unwrap());
        let mut app = EffectcraftApp::new(session);
        let observations = Arc::new(AtomicUsize::new(0));
        app.audio = Some(
            audio::AudioPlayback::start(
                Box::new(WrappingClock(observations.clone())),
                app.render_source(),
                cid,
                Tick::ZERO,
                Tick::ZERO,
                Tick::from_seconds_f64(1.0),
                true,
                48_000,
            )
            .unwrap(),
        );
        app.playback.playing = true;
        app.playback.fps = 30.0;
        app.playback.plan = Some(effectcraft_engine::preview::PreviewPlan {
            start: 0,
            end: 29,
            first: 0,
            step: 1,
            fps: 30.0,
            looping: true,
            cache_first: false,
            video: false,
            audio: true,
            origin: Tick::ZERO,
        });
        let ctx = egui::Context::default();
        for frame in 0..3 {
            ctx.run_ui(egui::RawInput { time: Some(frame as f64 / 60.0), ..Default::default() }, |_| {}).textures_delta.clear();
        }
        let mut output = ctx.run_ui(egui::RawInput { time: Some(1.0), ..Default::default() }, |ui| app.advance_playback(ui.ctx(), 1.0));
        output.textures_delta.clear();
        assert_eq!(app.playback.audio_frame, Some(29));
        assert!(
            output.viewport_output.get(&egui::ViewportId::ROOT).unwrap().repaint_delay < Duration::from_millis(10),
            "the imminent loop must not acquire a 50ms pause"
        );
        assert_eq!(observations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_playing_preview_wakes_when_its_next_frame_is_due_not_every_display_refresh() {
        // 24 fps: the next frame is due in 41.7 ms, so no UI frame before then (not 120 Hz).
        let d = until_next_frame(1.0 / 24.0);
        assert!(d >= Duration::from_secs_f64(1.0 / 24.0) && d < Duration::from_millis(43), "{d:?}");
        // Late, overdue or nonsense waits stay bounded: no busy spin, no sleep past the cap.
        for overdue in [-1.0, 0.0] {
            let d = until_next_frame(overdue);
            assert!(d >= Duration::from_micros(999) && d <= Duration::from_millis(1), "{d:?}");
        }
        assert_eq!(until_next_frame(2.0), PLAYBACK_MAX_WAIT);
        assert_eq!(until_next_frame(f64::NAN), PLAYBACK_MAX_WAIT);
        assert_eq!(until_next_frame(f64::INFINITY), PLAYBACK_MAX_WAIT);
    }
}

#[cfg(test)]
mod fit_window_tests {
    use super::overflows_screen;
    use egui::vec2;

    #[test]
    fn a_window_taller_than_the_screen_beside_its_taskbar_overflows() {
        // The default window on a 1366 × 768 laptop (shrunk to the monitor) and on 1080p.
        assert!(overflows_screen(vec2(1366.0, 799.0), vec2(1366.0, 768.0)));
        assert!(overflows_screen(vec2(1680.0, 1051.0), vec2(1920.0, 1080.0)));
        assert!(!overflows_screen(vec2(1680.0, 1051.0), vec2(2560.0, 1440.0)));
    }
}

#[cfg(test)]
mod copy_frame_tests {
    use super::*;

    fn copied(output: &egui::FullOutput) -> Vec<&egui::ColorImage> {
        output
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyImage(image) => Some(image),
                _ => None,
            })
            .collect()
    }

    /// Composition ▸ Copy Frame to Clipboard puts the rendered frame on the system clipboard.
    #[test]
    fn copy_frame_puts_the_frame_on_the_clipboard() {
        let mut session = Session::default();
        session.execute("comp.new", json!({"name": "Copy", "width": 8, "height": 6, "duration": 1})).unwrap();
        session.execute("layer.newSolid", json!({"color": "#ff0000"})).unwrap();
        let mut app = EffectcraftApp::new(session);
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            crate::menus::invoke(&mut app, ui.ctx(), "comp.copyFrame", json!({})).unwrap();
            app.handle_events(ui.ctx());
        });
        output.textures_delta.clear();
        let images = copied(&output);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].size, [8, 6]);
        assert!(images[0].pixels.iter().all(|p| *p == egui::Color32::from_rgb(255, 0, 0)));
    }

    /// A malformed image never reaches egui's panicking constructor.
    #[test]
    fn a_malformed_image_is_an_error_not_a_crash() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(copy_image(ui.ctx(), 4, 4, &[0; 10]).is_err());
            assert!(copy_image(ui.ctx(), 0, 4, &[]).is_err());
            assert!(copy_image(ui.ctx(), u32::MAX, u32::MAX, &[0; 4]).is_err());
        });
        output.textures_delta.clear();
        assert!(copied(&output).is_empty());
    }
}

#[cfg(test)]
mod gpu_failure_tests {
    use super::*;

    #[test]
    fn purge_and_project_replacement_release_gpu_uploads() {
        let Some(gpu) = effectcraft_gpu::Gpu::headless() else {
            assert!(std::env::var_os("EFFECTCRAFT_REQUIRE_GPU").is_none(), "GPU required for upload purge test");
            eprintln!("no GPU adapter: skipping upload purge test");
            return;
        };
        let mut app = EffectcraftApp::new(Session::default());
        app.gpu = Some(gpu.clone());
        let ctx = egui::Context::default();
        for replace in [false, true] {
            app.session.execute("comp.new", json!({"width": 64, "height": 64, "duration": 1})).unwrap();
            app.session.execute("layer.newText", json!({"text": "A", "size": 24, "position": [8, 32]})).unwrap();
            app.handle_events(&ctx);
            let mut renderer = effectcraft_engine::render::Renderer::new(&app.session.project, app.session.footage.as_ref(), Default::default());
            renderer.cache = Some(&app.session.layer_cache);
            assert!(gpu.render_display(&renderer, app.session.active_comp_id().unwrap(), Tick::ZERO).is_some());
            // Since RT1 (194ada22) an ordinary layer stays resident through the layer cache's
            // retained ledger instead of the upload cache: either one holds the frame's content.
            assert!(gpu.context().upload_stats().0 > 0 || app.session.layer_cache.retained_stats().live_bytes > 0);
            if replace {
                app.session.replace_project(Default::default(), None);
            } else {
                app.session.execute("edit.purge", json!({"what": "memory"})).unwrap();
            }
            app.handle_events(&ctx);
            assert_eq!(gpu.context().upload_stats(), (0, 0));
            assert_eq!(app.session.layer_cache.retained_stats().live_bytes, 0);
        }
    }

    /// CR1: a compositor on a device of its own that fails (out of video memory) is set up again
    /// on a new device next frame instead of compositing on the CPU until a restart; a
    /// replacement that fails too is retired by its own health check (the bridge latches only
    /// the first failure); after [`MAX_GPU_REBUILDS`] the preview stays on the CPU.
    #[test]
    fn a_failed_compositor_of_its_own_is_set_up_again_a_few_times() {
        if effectcraft_gpu::Gpu::headless().is_none() {
            assert!(std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "GPU required");
            return;
        }
        let mut app = EffectcraftApp::new(Session::default());
        let ctx = egui::Context::default();
        let bridge = gpu_failure::GpuFailureBridge::new(&ctx);
        app.set_gpu_failure_bridge(bridge.clone());
        for round in 0..=MAX_GPU_REBUILDS {
            let Some(g) = effectcraft_gpu::Gpu::headless() else { return };
            app.gpu = Some(g.clone());
            app.session.accel = Some(Arc::new(g.clone()));
            app.gpu_checked = true;
            (g.context().device_failure_notifier())("uncaptured GPU error: Out of Memory".into());
            app.apply_gpu_failure(&ctx);
            assert!(app.gpu.is_none() && app.session.accel.is_none(), "round {round}: the failed compositor is retired");
            if round < MAX_GPU_REBUILDS {
                assert!(!app.gpu_checked && app.gpu_rebuilding, "round {round}: set up again next frame");
                assert!(app.ui.status.contains("restarts on a new device"), "{}", app.ui.status);
                // The replacement waits until the failed device (still held here) is freed.
                assert!(app.gpu_failed_device.as_ref().is_some_and(|(d, _)| d.strong_count() > 0));
                drop(g);
                assert!(app.gpu_failed_device.as_ref().is_some_and(|(d, _)| d.strong_count() == 0), "then it may start");
                // As `init_gpu` does once the replacement is up.
                app.gpu_rebuilding = false;
                app.frames.revive_gpu();
            } else {
                assert!(app.gpu_checked && !app.gpu_rebuilding, "a device that keeps failing leaves the preview on the CPU");
            }
        }
    }

    #[test]
    fn host_failure_preserves_document_undo_and_cpu_pixels_and_detaches_late_readbacks() {
        let mut session = Session::default();
        let comp = ItemId(session.execute("comp.new", json!({"name": "Recovery", "width": 4, "height": 4, "duration": 1})).unwrap()["comp"].as_u64().unwrap());
        session.execute("layer.newSolid", json!({"comp": comp.0, "color": "#ff0000"})).unwrap();
        let before = session.render(comp, Tick::ZERO, RenderOpts::default());
        let project = session.project.clone();
        let revision = session.revision;
        let prefs = serde_json::to_value(&session.prefs).unwrap();
        let mut app = EffectcraftApp::new(session);
        let ctx = egui::Context::default();
        let bridge = gpu_failure::GpuFailureBridge::new(&ctx);
        app.set_gpu_failure_bridge(bridge.clone());
        let late_slot = app.viewer_readback.1.clone();
        let key = FrameKey { revision, content: 1, comp: comp.0, frame: 0, scale: 1000, view: 0, opts: 0 };
        app.viewer_readback.0 = Some(key);
        bridge.report("synthetic host device failure", true);
        app.apply_gpu_failure(&ctx);
        assert!(Arc::ptr_eq(&project, &app.session.project));
        assert_eq!(app.session.revision, revision);
        assert_eq!(serde_json::to_value(&app.session.prefs).unwrap(), prefs);
        assert!(app.gpu_checked, "failed device cannot be initialized again next frame");
        assert!(app.session.accel.is_none());
        assert!(app.ui.status.contains("presentation requires restarting"));
        assert_eq!(app.session.render(comp, Tick::ZERO, RenderOpts::default()).data, before.data);
        *late_slot.lock().unwrap() = Some((key, Arc::new(egui::ColorImage::new([1, 1], vec![egui::Color32::RED]))));
        assert!(app.viewer_readback.0.is_none());
        assert!(app.viewer_readback.1.lock().unwrap().is_none(), "a late completion cannot refill the replacement slot");
        app.session.execute("edit.undo", json!({})).unwrap();
        assert!(app.session.project.comp(comp).unwrap().layers.is_empty());
        app.session.execute("edit.redo", json!({})).unwrap();
        assert_eq!(app.session.render(comp, Tick::ZERO, RenderOpts::default()).data, before.data);
    }
}

#[cfg(test)]
mod playback_budget_tests {
    use super::*;
    use effectcraft_engine::commands::viewer_cmds::FastPreviews;

    fn app() -> EffectcraftApp {
        let mut app = EffectcraftApp::new(Session::default());
        app.session.execute("comp.new", json!({"width": 64, "height": 64, "duration": 2})).unwrap();
        let comp = app.session.active_comp_id().unwrap();
        app.session.set_resolution(comp, state::Resolution::Auto).unwrap();
        app.session.state.viewer.fast_previews = FastPreviews::AdaptiveResolution;
        app.playback.playing = true;
        app.playback.fps = 30.0;
        app
    }
    fn render(app: &EffectcraftApp, ms: f64) {
        let scale = app.viewer_scale(1.0, 1.0);
        let key = app.frame_series(app.session.active_comp_id().unwrap(), scale);
        for _ in 0..16 {
            app.frames.playback_sample(key, ms);
        }
    }
    fn slow(app: &EffectcraftApp) {
        render(app, 80.0);
    }
    #[test]
    fn playback_budget_preserves_explicit_choices_and_idle_resolution() {
        let mut app = app();
        slow(&app);
        assert_eq!(app.viewer_scale(1.0, 1.0), 0.5);
        // Half still misses the deadline: Third, then Quarter, shown as "(Quarter)" by the popup.
        render(&app, 50.0);
        assert!((app.viewer_scale(1.0, 1.0) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(crate::panels::viewer_tools::resolution_label(state::Resolution::Auto, 1.0 / 3.0), "(Third)");
        render(&app, 40.0);
        assert_eq!(app.viewer_scale(1.0, 1.0), 0.25);
        assert_eq!(crate::panels::viewer_tools::resolution_label(state::Resolution::Auto, 0.25), "(Quarter)");
        app.playback.playing = false;
        assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
        app.playback.playing = true;
        for res in [state::Resolution::Full, state::Resolution::Half, state::Resolution::Quarter] {
            app.session.set_resolution(app.session.active_comp_id().unwrap(), res).unwrap();
            slow(&app);
            assert_eq!(app.viewer_scale(1.0, 1.0), res.scale(1.0, 1.0));
        }
        app.session.set_resolution(app.session.active_comp_id().unwrap(), state::Resolution::Auto).unwrap();
        app.playback.res = Some(1);
        slow(&app);
        assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
        app.playback.res = None;
        // Settings ▸ Previews ▸ Adaptive Resolution While Playing off: only Fast Previews ▸
        // Adaptive Resolution adapts.
        app.session.prefs.previews.adaptive_playback = false;
        for mode in [FastPreviews::Off, FastPreviews::Draft, FastPreviews::FastDraft, FastPreviews::Wireframe] {
            app.session.state.viewer.fast_previews = mode;
            slow(&app);
            assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
        }
    }

    /// Adaptive Resolution While Playing (on by default) drafts slow playback in any Fast
    /// Previews mode, and the paused viewer asks for full quality again.
    #[test]
    fn adaptive_playback_pref_drafts_in_every_mode_and_pause_restores_full() {
        assert!(Session::default().prefs.previews.adaptive_playback, "on by default");
        for mode in [FastPreviews::Off, FastPreviews::Draft, FastPreviews::Wireframe] {
            let mut app = app();
            app.session.state.viewer.fast_previews = mode;
            assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
            slow(&app);
            assert_eq!(app.viewer_scale(1.0, 1.0), 0.5, "{mode:?}");
            app.playback.playing = false;
            assert_eq!(app.viewer_scale(1.0, 1.0), 1.0, "paused: full quality");
            app.playback.playing = true;
            assert_eq!(app.viewer_scale(1.0, 1.0), 1.0, "a new playback starts at full");
        }
        let mut app = app();
        app.session.execute("view.adaptivePlayback", json!({"on": false})).unwrap();
        app.session.state.viewer.fast_previews = FastPreviews::Off;
        slow(&app);
        assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
    }
    /// A4: after a ladder rung change the old rung's queued prefetch no longer counts as the
    /// new rung's frames in flight (it held playback at every Full ↔ Half switch): it is dropped.
    #[test]
    fn a_rung_change_drops_the_old_rungs_queued_prefetch() {
        /// Starts nothing, so every request stays queued.
        struct Idle;
        impl frames::RemoteFrames for Idle {
            fn slots(&self) -> usize {
                0
            }
            fn start(&self, _: frames::RemoteJob, _: frames::RemoteDone) {}
        }
        let mut app = app();
        app.frames.set_remote(Some(Arc::new(Idle)));
        let comp = app.session.active_comp_id().unwrap();
        let (full, half) = (app.frame_series(comp, 1.0), app.frame_series(comp, 0.5));
        let src = app.render_source();
        for frame in 0..8 {
            app.frames.request(&src, FrameKey { frame, ..full }, comp, Tick::ZERO, RenderOpts::default());
        }
        app.frames.request_urgent(&src, FrameKey { frame: 9, ..full }, comp, Tick::ZERO, RenderOpts::default());
        assert_eq!(app.frames.inflight(), 9);
        assert_eq!(app.frames.focus_series(&half), 0, "the old rung's frames are not the new rung's");
        assert_eq!(app.frames.inflight(), 1, "its queued prefetch is gone; the viewer's frame stays");
        app.frames.request(&src, FrameKey { frame: 0, ..half }, comp, Tick::ZERO, RenderOpts::default());
        assert_eq!(app.frames.focus_series(&half), 1);
        assert_eq!(app.frames.focus_series(&full), 1, "the urgent frame counts for its own series");
    }

    #[test]
    fn playback_budget_ignores_precache_bad_rates_and_respects_frame_skip() {
        let mut app = app();
        app.playback.caching = true;
        slow(&app);
        assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
        app.playback.caching = false;
        for fps in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            app.playback.fps = fps;
            slow(&app);
            assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
        }
        app.playback.fps = 30.0;
        app.playback.plan = Some(effectcraft_engine::preview::PreviewPlan {
            start: 0,
            end: 59,
            first: 0,
            step: 3,
            fps: 30.0,
            looping: true,
            cache_first: false,
            video: true,
            audio: false,
            origin: Tick::ZERO,
        });
        slow(&app); // 80ms fits within the 100ms displayed-frame budget.
        assert_eq!(app.viewer_scale(1.0, 1.0), 1.0);
    }
}

#[cfg(test)]
mod appearance_tests {
    use super::*;
    use crate::theme::ThemeKind;

    fn app() -> EffectcraftApp {
        EffectcraftApp::new(effectcraft_engine::Session::default())
    }

    /// The Tools bar's appearance button: Sync with System, Light, Dark, then Sync with System
    /// again, showing the saved theme of each family without losing either choice.
    #[test]
    fn appearance_button_cycles_modes_without_losing_theme_choices() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.hooks.system_theme = Some(Box::new(|_| Some(egui::Theme::Dark)));
        app.set_pref("appearance.darkTheme", json!("darker")).unwrap();
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Darker);
        for (mode, shown) in [("auto", ThemeKind::Darker), ("light", ThemeKind::Light), ("dark", ThemeKind::Darker), ("auto", ThemeKind::Darker)] {
            menus::invoke(&mut app, &ctx, "view.theme.toggle", json!({})).unwrap();
            assert_eq!(app.session.prefs.appearance.appearance_mode, mode);
            assert_eq!(app.ui.theme, shown, "{mode}");
            assert_eq!(app.tokens.kind, shown);
            assert_eq!(app.session.prefs.appearance.dark_theme, "darker");
            assert_eq!(app.session.prefs.appearance.light_theme, "light");
        }
    }

    /// Sync with System follows the host's answer (the Linux portal) when it changes, and falls
    /// back to Dark when nothing reports a system appearance.
    #[test]
    fn auto_follows_a_stubbed_system_theme() {
        use std::sync::{Arc, Mutex};
        let ctx = egui::Context::default();
        let mut app = app();
        let system = Arc::new(Mutex::new(Some(egui::Theme::Light)));
        let read = Arc::clone(&system);
        app.hooks.system_theme = Some(Box::new(move |_| *read.lock().unwrap_or_else(std::sync::PoisonError::into_inner)));
        app.set_pref("appearance.appearanceMode", json!("auto")).unwrap();
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Light);
        *system.lock().unwrap() = Some(egui::Theme::Dark);
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Dark, "a system change applies without a settings change");
        *system.lock().unwrap() = None;
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Dark);
        // A fixed mode ignores the system.
        *system.lock().unwrap() = Some(egui::Theme::Light);
        app.set_pref("appearance.appearanceMode", json!("dark")).unwrap();
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Dark);
    }

    /// Sync with System leaves egui's theme preference at System, so egui never gives the native
    /// window an appearance of its own (on macOS that stops system appearance changes reaching
    /// the app, #642); egui's light and dark styles both carry the resolved theme. Fixed modes
    /// pin the window to their family.
    #[test]
    fn auto_mode_leaves_the_native_window_following_the_system() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.hooks.system_theme = Some(Box::new(|_| Some(egui::Theme::Light)));
        app.set_pref("appearance.appearanceMode", json!("auto")).unwrap();
        app.apply_prefs(&ctx);
        assert_eq!(app.ui.theme, ThemeKind::Light);
        assert_eq!(ctx.options(|o| o.theme_preference), egui::ThemePreference::System);
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            assert_eq!(ctx.style_of(theme).visuals.panel_fill, app.tokens.panel_bg, "{theme:?}");
        }
        // egui asks the window to follow the system, never to take Light or Dark.
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let cmds = out.viewport_output.values().flat_map(|v| v.commands.iter()).collect::<Vec<_>>();
        assert!(!cmds.iter().any(|c| matches!(c, egui::ViewportCommand::SetTheme(t) if t != &egui::SystemTheme::SystemDefault)), "{cmds:?}");
        for (mode, pref) in [("light", egui::ThemePreference::Light), ("dark", egui::ThemePreference::Dark)] {
            app.set_pref("appearance.appearanceMode", json!(mode)).unwrap();
            app.apply_prefs(&ctx);
            assert_eq!(ctx.options(|o| o.theme_preference), pref, "{mode}");
            assert_eq!(ctx.global_style().visuals.panel_fill, app.tokens.panel_bg, "{mode}");
        }
    }

    /// Picking a theme (`view.theme.*`, `ui.set {theme}`) fixes the mode to its family.
    #[test]
    fn picking_a_theme_fixes_its_mode() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.set_pref("appearance.appearanceMode", json!("auto")).unwrap();
        menus::invoke(&mut app, &ctx, "view.theme.light", json!({})).unwrap();
        assert_eq!(app.session.prefs.appearance.appearance_mode, "light");
        assert_eq!(app.ui.theme, ThemeKind::Light);
        menus::invoke(&mut app, &ctx, "view.theme.darker", json!({})).unwrap();
        assert_eq!(app.session.prefs.appearance.appearance_mode, "dark");
        assert_eq!(app.session.prefs.appearance.dark_theme, "darker");
        assert_eq!(app.ui.theme, ThemeKind::Darker);
    }
}
