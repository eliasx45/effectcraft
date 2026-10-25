//! The EffectCraft engine façade.
//!
//! Every user-visible action is a command with a stable id (`layer.newSolid`, `prop.set`,
//! `keys.easyEase`…) and JSON parameters, dispatched through [`Session::execute`]. The egui UI,
//! the CLI, the control channel and the MCP server all use this one entry point; that is what
//! makes the UI swappable and the whole app agent-drivable.
//!
//! The project is an `Arc<Project>` edited copy-on-write; undo keeps whole-project snapshots
//! (compositions are `Arc`s, so untouched comps are shared).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Every `src/*.rs` file is a module (`tests*.rs` under `#[cfg(test)]`), declared by build.rs;
// declare one here only for another visibility or a submodule folder. See docs/architecture.md
// "Self-registering modules".
include!(concat!(env!("OUT_DIR"), "/modules.rs"));
// By hand: its `cmd!` / `query!` macros are `#[macro_export]`ed (not allowed from an `include!`).
pub mod commands;
pub mod menus;
mod session_settings;

use std::sync::Arc;

use effectcraft_project::{Comp, ItemId, LayerId, Project, Uid};
use effectcraft_raster::Image;
use effectcraft_render::{ExprHost, FootageSource, LayerCache, NoFootage, RenderOpts, Renderer};
use effectcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use commands::{CommandSpec, command_specs, find as find_command};
pub use effectcraft_color as color;
pub use effectcraft_effects as effects;
pub use effectcraft_geom as geom;
pub use effectcraft_keyframe as keyframe;
pub use effectcraft_motionlib as motionlib;
pub use effectcraft_project as project;
pub use effectcraft_raster as raster;
pub use effectcraft_render as render;
pub use effectcraft_segment as segment;
pub use effectcraft_text as text;
pub use effectcraft_time as time;
pub use effectcraft_track as track;
pub use history::{Branch, History, HistoryNode};
pub use render_queue::{ExportJob, ExportResult, Exporter, JobState};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("unknown command `{0}`{hint}", hint = suggest::command_hint(.0))]
    UnknownCommand(String),
    #[error("command `{0}` is not available right now: {1}")]
    Disabled(String, String),
    #[error("invalid parameters for `{cmd}`: {msg}")]
    BadParams { cmd: String, msg: String },
    #[error("no active composition")]
    NoComp,
    /// A `comp` reference (name or id) that names no composition.
    #[error("no composition {0}")]
    NoSuchComp(String),
    #[error("{0}")]
    Project(#[from] effectcraft_project::ProjectError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Host services (file access) injected by the frontend.
pub trait Services: Send + Sync {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>>;
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()>;
    /// Copy media without requiring a complete movie-sized allocation on native hosts.
    fn copy_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        self.write_file(destination, &self.read_file(source)?)
    }
    /// Collection owns a native directory; browser/virtual hosts must not create disk output.
    fn supports_native_collection(&self) -> bool {
        false
    }
    /// Convert an After Effects project to the versioned AEP import document.
    /// Browser hosts and hosts without the optional importer can return Unsupported.
    fn export_aep_ir(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "AEP importer is not configured on this host"))
    }
    /// Whether a file exists (the background footage check after open).
    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }
    /// A file's size and modification time (nanoseconds since 1970), to notice that it changed
    /// on disk (Settings ▸ Import ▸ Reload Footage Changed on Disk); `None` when it can't be read.
    fn stamp(&self, path: &str) -> Option<(u64, u128)> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some((meta.len(), modified.as_nanos()))
    }
    /// Keep a file the app made for itself (footage extracted from a template): like
    /// [`Services::write_file`], creating its folder, but never offered as a download (web: it
    /// goes to browser storage only).
    fn store_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if let Some(d) = std::path::Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(d);
        }
        self.write_file(path, data)
    }
    /// The files in folder `dir` (full paths; no folders, no hidden files), for image-sequence
    /// detection on import. Empty when it can't be read.
    fn list_dir(&self, dir: &str) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(if dir.is_empty() { "." } else { dir }) else { return vec![] };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path().to_string_lossy().to_string())
            .collect()
    }
    /// Whether `path` is a folder (dropped or picked folders import their files).
    fn is_dir(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }
}

/// Native filesystem.
pub struct FsServices;
impl Services for FsServices {
    fn copy_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        std::fs::copy(source, destination).map(|_| ())
    }
    fn supports_native_collection(&self) -> bool {
        true
    }
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        config::atomic_write(std::path::Path::new(path), data)
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn export_aep_ir(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let script = std::env::var_os("EFFECTCRAFT_AEP_EXPORTER")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let exe = std::env::current_exe().ok()?;
                let mac_resource = exe.parent()?.join("../Resources/aep_ir_export.py");
                if mac_resource.is_file() {
                    return Some(mac_resource);
                }
                exe.ancestors().map(|dir| dir.join("packaging/aep/aep_ir_export.py")).find(|candidate| candidate.is_file())
            })
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "AEP exporter script missing; set EFFECTCRAFT_AEP_EXPORTER"))?;
        let python = std::env::var_os("EFFECTCRAFT_AEP_PYTHON").unwrap_or_else(|| "python3".into());
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let output = std::env::temp_dir().join(format!("effectcraft-aep-{}-{stamp}.json", std::process::id()));
        let mut command = std::process::Command::new(python);
        command.arg(script).arg(path).arg("-o").arg(&output);
        if let Some(root) = std::env::var_os("EFFECTCRAFT_AEP_FONT_ROOT").filter(|root| !root.is_empty()) {
            command.arg("--font-search-root").arg(root);
        }
        let run = command.output()?;
        if !run.status.success() {
            let _ = std::fs::remove_file(&output);
            let error = String::from_utf8_lossy(&run.stderr);
            return Err(std::io::Error::other(format!(
                "AEP export failed (set EFFECTCRAFT_AEP_PYTHON to an interpreter with py-aep installed): {}",
                error.trim()
            )));
        }
        let bytes = std::fs::read(&output);
        let _ = std::fs::remove_file(&output);
        bytes
    }
}

/// Imports media files into the project (implemented by the media layer).
pub trait Importer: Send + Sync {
    /// Probe one file and return footage metadata. Image sequences are the engine's
    /// ([`sequence`]): it groups numbered stills and probes their first file here.
    fn probe(&self, path: &str) -> std::result::Result<effectcraft_project::Footage, String>;
    /// A footage file's bytes are now at `path` (extracted from a template): hosts whose media
    /// layer reads from memory (the browser) register them; file-based ones need nothing.
    fn register(&self, _path: &str, _data: &[u8]) {}
    /// An image sequence's other frames (its first was probed) are footage now: hosts whose
    /// media layer reads from memory (the browser) register them; file-based ones need nothing.
    fn add_frames(&self, _paths: &[String]) {}
}

/// A keyframe reference: layer, property uid, key time (layer time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyRef {
    pub layer: LayerId,
    pub prop: Uid,
    pub time: Tick,
}

/// A keyframe drag in progress (`keys.move` steps sharing a merge key): the selection it
/// started from and the total offset so far, so every step is applied to the keys as they were
/// before the drag. A key passed over on the way is never overwritten; only the drop position
/// counts, as in After Effects.
#[derive(Clone, Debug)]
pub struct KeyMove {
    pub merge: String,
    pub from: Vec<KeyRef>,
    pub total: Tick,
}

/// Keyframes of one property on the keyframe clipboard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyClip {
    pub layer: LayerId,
    /// Match-id path relative to the layer (`transform/position`), portable between layers.
    pub path: String,
    /// Keys with times relative to the earliest copied key (layer time).
    pub keys: Vec<effectcraft_keyframe::Keyframe>,
}

/// The temporal graph of one keyframe: how it eases in and out.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyGraph {
    pub in_interp: effectcraft_keyframe::Interp,
    pub out_interp: effectcraft_keyframe::Interp,
    pub in_ease: effectcraft_keyframe::Eases,
    pub out_ease: effectcraft_keyframe::Eases,
    pub continuous: bool,
    pub auto_bezier: bool,
}

/// The graph Edit ▸ Paste puts on the selected keyframes: one key's graph for all of them (the
/// copied keys all had the same graph), else the graphs of the last selected property's keys
/// in time order, repeated segment by segment over any number of keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphClip {
    pub keys: Vec<KeyGraph>,
    /// The keys that were copied: pasting with exactly these selected pastes the keys themselves.
    pub source: Vec<KeyRef>,
}

/// A selected mask vertex: layer, mask group uid, vertex index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VertexRef {
    pub layer: LayerId,
    pub mask: Uid,
    pub index: usize,
}

/// One composition's selection. The active comp's copy lives in [`EditorState::selected_layers`]
/// (and the property, key, and vertex lists); every other comp keeps its own here (#1001).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompSelection {
    pub layers: Vec<LayerId>,
    pub props: Vec<(LayerId, Uid)>,
    pub keys: Vec<KeyRef>,
    pub vertices: Vec<VertexRef>,
}

impl CompSelection {
    fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.props.is_empty() && self.keys.is_empty() && self.vertices.is_empty()
    }
}

/// Editing state that commands depend on (headless-relevant, serde for agents).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditorState {
    pub active_comp: Option<ItemId>,
    /// Open compositions (viewer/timeline tabs), in tab order.
    pub open_comps: Vec<ItemId>,
    /// Compositions that were active before the current one, most recent first (Shift+Esc).
    #[serde(default)]
    pub recent_comps: Vec<ItemId>,
    /// Current time per comp (the CTI).
    pub times: std::collections::BTreeMap<ItemId, Tick>,
    /// Layer selection of the active comp. Other comps' selections are in [`Self::comp_selections`].
    pub selected_layers: Vec<LayerId>,
    /// Selections of comps that are not active, keyed by comp id (#1001).
    #[serde(default)]
    pub comp_selections: std::collections::BTreeMap<ItemId, CompSelection>,
    /// Selected properties / groups (by uid) of selected layers.
    pub selected_props: Vec<(LayerId, Uid)>,
    pub selected_keys: Vec<KeyRef>,
    pub project_selection: Vec<ItemId>,
    /// Layer clipboard (serialized layers) and keyframe clipboard. Shared layers: cloning the
    /// state for an edit doesn't copy them.
    #[serde(skip)]
    pub clipboard: effectcraft_project::Layers,
    /// Copied keyframes (times relative to the earliest copied key).
    #[serde(skip)]
    pub key_clipboard: Vec<KeyClip>,
    /// The graph of the copied keyframes (Paste onto other selected keyframes pastes it).
    #[serde(skip)]
    pub graph_clipboard: GraphClip,
    /// The last copy was keyframes (Paste pastes keys at the CTI).
    #[serde(skip)]
    pub clip_is_keys: bool,
    /// Values of selected unanimated properties (match path, value), copied with Edit ▸ Copy;
    /// Paste sets the same properties of the selected layers.
    #[serde(skip)]
    pub value_clipboard: Vec<(String, effectcraft_keyframe::Value)>,
    /// Copied effect instances, with comp-time key offsets from the earliest copied key.
    /// Paste adds them to the selected layers at the destination comp's CTI.
    #[serde(skip)]
    pub effect_clipboard: Vec<effectcraft_project::PropGroup>,
    /// Copied shape items (Edit ▸ Copy with groups, paths, paints or path operations selected in
    /// a shape layer's Contents); Paste adds them to the selected shape layers.
    #[serde(skip)]
    pub contents_clipboard: Vec<effectcraft_project::PropGroup>,
    /// Selected mask vertices (viewer Selection tool / pen).
    #[serde(default)]
    pub selected_vertices: Vec<VertexRef>,
    pub snapping: bool,
    /// Tools bar ▸ Snapping options (which features snap, Snap Edges Extended).
    #[serde(default)]
    pub snap_features: viewer::SnapFeatures,
    /// Last applied effect id (Effect ▸ last effect).
    pub last_effect: Option<String>,
    /// Viewer region of interest `[x, y, w, h]` in comp pixels (Composition ▸ Crop Comp to Region
    /// of Interest).
    #[serde(default)]
    pub region_of_interest: Option<[f64; 4]>,
    /// Property-link clipboard (Edit ▸ Copy with Property Links / Copy Expression Only).
    #[serde(skip)]
    pub link_clipboard: Option<LinkClip>,
    /// File ▸ Interpret Footage ▸ Remember Interpretation.
    #[serde(skip)]
    pub interpretation: Option<effectcraft_project::Footage>,
    /// Viewer 3D view per comp (Active Camera / Front / … / Custom View 3 and edited view cameras).
    #[serde(default)]
    pub views3d: std::collections::BTreeMap<ItemId, effectcraft_render::three_d::Views3D>,
    /// Tracker panel ▸ Current Track: (tracked layer, tracker group uid) in the active comp.
    #[serde(default)]
    pub current_track: Option<(LayerId, Uid)>,
    /// Tracker panel ▸ Method in mask mode (Track Mask).
    #[serde(default)]
    pub mask_track_method: mask_track::MaskMethod,
    /// 3D Camera Tracker: the selected track points (ids) in the viewer.
    #[serde(default)]
    pub camera_points: Vec<u32>,
    /// Mask Interpolation panel options.
    #[serde(default)]
    pub mask_interp: commands::mask_interp::MaskInterpOptions,
    /// Paint and Brushes panel options (Brush, Clone Stamp and Eraser tools).
    #[serde(default)]
    pub paint: commands::paint::PaintOptions,
    /// Roto Brush and Refine Edge tool options.
    #[serde(default)]
    pub roto: roto::RotoOptions,
    /// Puppet tool options for new meshes.
    #[serde(default)]
    pub puppet: commands::puppet::PuppetOptions,
    /// The shape tools' and the Pen's options (Tool Creates Shape / Mask, Fill and Stroke).
    #[serde(default)]
    pub shape_tool: commands::shape_tool::ShapeTool,
    /// View ▸ Switch View Layout: 1, 2 or 4 views side by side in the Composition viewer.
    #[serde(default = "one_view")]
    pub view_layout: u8,
    /// View ▸ Switch View Layout ▸ Share View Options (grid, guides… in every view).
    #[serde(default)]
    pub share_view_options: bool,
    /// Layer ▸ Mask ▸ Hide Locked Masks (viewer outlines).
    #[serde(default)]
    pub hide_locked_masks: bool,
    /// On-canvas text editing (Type tool): the edited layer, selection and insertion style.
    #[serde(default)]
    pub text_edit: Option<commands::text_edit::TextEdit>,
    /// Text copied while editing (with its formatting), for Paste / Paste Text Formatting Only.
    #[serde(skip)]
    pub text_clipboard: Option<effectcraft_keyframe::TextDoc>,
    /// Composition viewer display options (Show Channel, exposure, snapshot, Fast Previews).
    #[serde(default)]
    pub viewer: commands::viewer_cmds::ViewOptions,
    /// Essential Graphics panel ▸ Primary composition (`None` = the active comp).
    #[serde(default)]
    pub essential_primary: Option<ItemId>,
    /// Essential Graphics panel ▸ Solo Supported Properties (the timeline shows only properties
    /// Essential Graphics can expose).
    #[serde(default)]
    pub essential_solo: bool,
    /// File ▸ Watch Folder: the folder being watched for projects to render (frontends call
    /// `file.watchFolder.poll` periodically while it is set).
    #[serde(default)]
    pub watch_folder: Option<String>,
    /// The Footage panel: the footage shown, its time and In/Out marks (source time).
    #[serde(default)]
    pub footage_panel: Option<commands::footage_panel::FootageView>,
    /// Media Browser: the folder shown and the favourites.
    #[serde(default)]
    pub media_browser: media_browser::BrowserState,
    /// Content-Aware Fill panel settings.
    #[serde(default)]
    pub content_fill: commands::content_fill::FillSettings,
    /// View ▸ Split with New Locked Viewer: the second viewer's comp and 3D view.
    #[serde(default)]
    pub locked_viewer: Option<commands::viewer_cmds::LockedViewer>,
}

fn one_view() -> u8 {
    1
}

/// What Copy with Property Links / Copy Expression Only put on the clipboard.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkClip {
    /// Expressions that link back to the source properties: (match path, expression text).
    Links { relative: bool, links: Vec<(String, String)> },
    /// Expressions only: (match path, expression).
    Expressions(Vec<(String, effectcraft_project::Expression)>),
}

/// Events for frontends (drained each frame).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Event {
    ProjectChanged {
        revision: u64,
    },
    Toast {
        message: String,
        error: bool,
    },
    OpenComp(ItemId),
    OpenUrl(String),
    /// A frontend-only command (viewer zoom, panels, dialogs…) ran: the UI performs it. Headless
    /// sessions ignore these.
    Frontend {
        command: String,
        params: Value,
    },
    /// Drop cached frames / renders (Edit ▸ Purge).
    PurgeCaches,
    /// Put an image on the system clipboard (Composition ▸ Copy Frame to Clipboard): `rgba` is
    /// `width` × `height` straight-alpha RGBA8 rows. Headless sessions ignore it; the pixels are
    /// not serialized.
    CopyImage {
        width: u32,
        height: u32,
        #[serde(skip)]
        rgba: Vec<u8>,
    },
}

pub struct Session {
    pub freeze_runtime: freeze_runtime::Runtime,
    pub project: Arc<Project>,
    pub history: History,
    /// Bumped on every change (edits, undo, redo, open). Replies of `project.summary` and
    /// `editor.state` carry it; an `expectRevision` execute parameter refuses to run on another.
    pub revision: u64,
    /// Who is executing the current command (an agent's or script's `client` execute parameter;
    /// `None` = the user). Undo steps remember it, and `edit.undo` with a `client` only undoes
    /// that client's own last step.
    pub client: Option<String>,
    /// The clients' own cursors (selection, active comp, current time), swapped into
    /// [`Session::state`] while one of their commands runs (see [`agents`]).
    pub cursors: agents::Cursors,
    /// Compositions claimed by clients (`comp.claim`): other clients' edits to them are refused.
    pub claims: agents::Claims,
    /// Bumped whenever another project replaces this one ([`Session::replace_project`]).
    pub epoch: u64,
    /// Who changed what, revision by revision ([`feed`]): `history.feed`, `agent.notices`.
    pub feed: feed::Feed,
    /// Identity of this in-memory session, stable across project replacement.
    pub session_id: String,
    /// The highest `next_id` any state has reached: ids stay unique across undo / redo, so an
    /// id an agent saw is never handed to a different object later.
    pub id_floor: u64,
    pub saved_revision: u64,
    /// The project as last opened or saved: the project is unmodified while it is this very
    /// snapshot, so undoing back to the saved state clears the modified mark.
    pub saved_project: Arc<Project>,
    pub path: Option<String>,
    pub state: EditorState,
    pub services: Arc<dyn Services>,
    pub footage: Arc<dyn FootageSource>,
    pub expr: Option<Arc<dyn ExprHost>>,
    /// GPU compositor (Mercury GPU Acceleration), when the frontend has one: used by renders
    /// that ask for [`effectcraft_render::Backend::Gpu`]/`Auto`, and by Render Queue exports
    /// when the project's renderer is the GPU.
    pub accel: Option<Arc<dyn effectcraft_render::Accelerator>>,
    /// Why there is no [`Session::accel`] (`render.backend`'s `why`): the host's reason, e.g. a
    /// headless start without `--gpu` or the compositor's setup error.
    pub accel_note: Option<String>,
    /// The graphics adapter and backend the window draws with (`NVIDIA GeForce RTX 4070
    /// (Vulkan)`), as the host reports it: the System Compatibility Report shows it.
    pub window_adapter: Option<String>,
    /// Expression syntax checker (set by the host that links the expression engine).
    pub expr_check: Option<fn(&str) -> std::result::Result<(), String>>,
    pub importer: Option<Arc<dyn Importer>>,
    /// Render Queue encoder (the export layer); `None` = export unavailable.
    pub exporter: Option<Arc<dyn Exporter>>,
    /// Free-space hook for Render Settings ▸ Use Storage Overflow (`None`: never full).
    pub storage_quota: Option<Arc<dyn effectcraft_project::render_queue::StorageQuota>>,
    /// The running (or finished, not yet polled) render.
    pub render_job: Option<render_queue::RenderJob>,
    /// The running (or finished, not yet polled) track analysis.
    pub track_job: Option<tracking::TrackJob>,
    /// The running (or finished, not yet polled) mask track.
    pub mask_job: Option<mask_track::MaskTrackJob>,
    /// The running (or finished, not yet polled) Warp Stabilizer analysis.
    pub warp_job: Option<warp::WarpJob>,
    /// Warp Stabilizers waiting for (re-)analysis: (comp, layer, effect uid).
    pub warp_pending: Vec<(ItemId, LayerId, Uid)>,
    /// The last state whose analyses (Warp Stabilizer, 3D Camera Tracker, Roto Brush) were all
    /// checked after an edit: the next edit from it checks only the layers it changed.
    pub analyses_checked: std::sync::Weak<Project>,
    /// The running (or finished, not yet polled) 3D Camera Tracker analysis.
    pub camera_job: Option<camera_track::CameraJob>,
    /// 3D Camera Trackers waiting for (re-)analysis: (comp, layer, effect uid).
    pub camera_pending: Vec<(ItemId, LayerId, Uid)>,
    /// The running (or finished, not yet polled) Roto Brush propagation / Freeze.
    pub roto_job: Option<roto::RotoJob>,
    /// Trained models for Roto Brush and face tracking (Settings ▸ Roto Brush / Face Tracking).
    pub models: models::Models,
    /// Where installed models live (hosts set it; else `models` next to the settings).
    pub models_dir: Option<std::path::PathBuf>,
    /// Roto Brush instances edited since their last propagation: (comp, layer, effect uid).
    pub roto_pending: Vec<(ItemId, LayerId, Uid)>,
    pub events: Vec<Event>,
    /// Commands executed: (id, params).
    pub journal: Vec<(String, Value)>,
    /// Processed-layer pixels reused across frames and edits (content-keyed, never stale).
    pub layer_cache: Arc<LayerCache>,
    /// One memory budget for the layer cache, the footage frame cache and the frontend's RAM
    /// preview (Settings ▸ Memory & CPU ▸ Share One Budget Between the Caches; see
    /// [`effectcraft_render::cache::shared`]). Applied with the cache budgets.
    pub cache_share: Arc<effectcraft_render::cache::shared::SharedBudget>,
    /// The persistent disk cache (Settings ▸ Media & Disk Cache), when enabled.
    pub disk_cache: Option<Arc<effectcraft_render::disk_cache::DiskCache>>,
    /// Settings (Preferences).
    pub prefs: prefs::Prefs,
    /// Bumped whenever settings change (frontends re-apply theme, labels…).
    pub prefs_revision: u64,
    /// Where settings, shortcut presets and the crash-recovery sentinel are stored (`None` =
    /// nothing persists).
    pub config: Option<Arc<dyn config::ConfigStore>>,
    /// Keyboard shortcut presets.
    pub keymaps: shortcuts::Keymaps,
    /// Scripts' `app.settings`, kept in the settings store (see
    /// [`commands::scripts::SCRIPT_SETTINGS_FILE`]).
    pub script_settings: commands::scripts::ScriptSettings,
    /// Frontend-only commands offered for binding.
    pub ui_commands: Vec<shortcuts::UiCommand>,
    /// Cache of the resolved active preset (read it with [`Session::shortcuts`]).
    pub shortcut_table: std::sync::OnceLock<shortcuts::ShortcutTable>,
    /// Auto-save bookkeeping.
    pub autosave: autosave::AutoSaveState,
    /// Host-owned isolation for all auto-saves (e.g. one MCP session). Independent of settings
    /// and project bookkeeping, so opening/saving a project or changing preferences keeps it.
    pub autosave_folder_override: Option<std::path::PathBuf>,
    /// The viewer snapshot (Take Snapshot / Show Snapshot).
    pub snapshot: Option<viewer::Snapshot>,
    /// Named rendered frames for `render.compare` (`render.snapshot`; bounded, oldest dropped).
    pub frame_snapshots: commands::render_compare::FrameSnapshots,
    /// The JavaScript scripting engine (set by the host that links `effectcraft-script`):
    /// `script.run`, File ▸ Scripts ▸ Run Script File… (`.jsx`/`.js`) and the Script Console.
    pub script: Option<ScriptRunner>,
    /// Stops the script that runs next (set by `script.run {timeout}` and by hosts that cancel
    /// scripts, e.g. MCP `notifications/cancelled`); see [`ScriptInterrupt`].
    pub script_interrupt: Option<ScriptInterrupt>,
    /// Loads WebAssembly effect plug-ins (set by the host that links `effectcraft-plugin` with
    /// its runtime): `effect.plugins.load`.
    pub plugin_loader: Option<PluginLoader>,
    /// ScriptUI windows and panels opened by scripts (see [`scriptui`]).
    pub script_ui: scriptui::ScriptUi,
    /// The last physical-memory reading (Settings ▸ Memory & CPU budgets); `None` = unknown
    /// (headless sessions don't query it, see [`Session::memory_tick`]).
    pub sys_memory: Option<sysinfo::SysMemory>,
    /// The keyframe drag in progress (`keys.move` with a merge key).
    pub key_move: Option<KeyMove>,
    /// Reads the system's memory in the background for [`Session::memory_tick`].
    pub memory_watch: sysinfo::MemoryWatch,
    /// The live memory map's process reading and the frontend's probes (Window ▸ Memory, the
    /// status bar's memory chip, `memory.stats`; see [`memory_map`]).
    pub memory_map: memory_map::Hooks,
    /// Switches Affect Nested Comps as last applied (a change empties the layer cache).
    pub applied_nested_switches: Option<bool>,
    /// Runs renders and analyses off the UI thread where there are no threads (the web app's
    /// Web Workers, see [`offload`]); `None` = threads (desktop) or inline (wasm32).
    pub offload: Option<Arc<dyn offload::Offload>>,
    /// Analyses running in the [`Session::offload`] worker.
    pub offloaded: Vec<offload::OffloadedJob>,
    /// What the Media Browser browses (`None`: the local file system where there is one).
    pub browser: Option<Arc<dyn media_browser::Browser>>,
    /// The browser's storage (the web app; Settings ▸ Disk ▸ Browser Storage, `storage.*`).
    pub storage: Option<Arc<dyn storage::StorageHost>>,
    /// Offloaded jobs that ended, newest last (kind, error): how a caller waiting for one
    /// (`wait: true` over the control channel in the browser) learns its outcome.
    pub offload_log: Vec<(offload::JobKind, Option<String>)>,
    /// Number of older completion entries evicted from the bounded offload log.
    pub offload_log_start: usize,
    /// Generic background tasks (Content-Aware Fill, Scene Edit Detection), see [`jobs`].
    pub tasks: Vec<jobs::Task>,
    /// Finished tasks, newest last (Progress panel, `jobs.list`).
    pub job_log: Vec<jobs::JobRecord>,
    pub next_task_id: u64,
    /// The running Learn tutorial (Home ▸ Learn), see [`learn`].
    pub learn: Option<learn::Progress>,
    /// Check footage in the background after File ▸ Open ([`footage_check`]): frontends with an
    /// event loop (the desktop app) turn this on; headless sessions run `footage.check` when
    /// they want it.
    pub check_footage_on_open: bool,
    /// Each footage file's size and modification time when last seen (Settings ▸ Import ▸
    /// Reload Footage Changed on Disk, [`footage_reload`]).
    pub footage_stamps: std::collections::HashMap<String, footage_reload::Stamp>,
    /// The stamps being taken on a worker thread ([`footage_reload::start_scan`]).
    pub footage_scan: Option<footage_reload::Scan>,
    /// The disk cache's footage salt and the footage it was taken for ([`Session::refresh_disk_salt`]).
    pub disk_salt: Option<(u64, u64)>,
    /// How many script-issued commands are running ([`Session::execute_for_script`]): while one
    /// is, settings that guard what scripts may do can't change (see
    /// [`prefs::Scripting::same_permissions`]).
    pub script_calls: ScriptCalls,
    /// The Motion Library (cards in a remote bucket, cached locally): made on first use from
    /// `motion-library.json` or the environment, or set by the host (the web app's fetch).
    pub motion_library: motion_library::Handle,
    /// Render boxes mirroring this session (`app.sync`, [`remote_follow`]).
    pub followers: remote_follow::Followers,
}

/// The count behind [`Session::running_for_script`]; only the engine changes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScriptCalls(u32);

/// A thumbnail render ([`Session::thumbnail_job`]): (width, height, RGBA8).
pub type ThumbnailJob = Box<dyn FnOnce() -> Option<(u32, u32, Vec<u8>)> + Send>;

/// A script to run (see [`Session::script`]).
#[derive(Clone, Copy, Debug)]
pub struct ScriptRequest<'a> {
    pub code: &'a str,
    /// File name shown in errors and `$.fileName` (`console` for the Script Console).
    pub name: &'a str,
    /// Run in the Script Console's persistent context (variables survive between runs).
    pub console: bool,
}

/// Stops a running script from outside: a cancel flag another thread sets and/or a deadline. The
/// scripting runtime checks it between batches of bytecode and in every object-model call, and
/// then ends the script with an error that `try`/`catch` can't intercept.
#[derive(Clone, Debug, Default)]
pub struct ScriptInterrupt {
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When the time limit runs out (with the limit in seconds, for the message).
    pub deadline: Option<(web_time::Instant, f64)>,
}

impl ScriptInterrupt {
    /// A time limit of `seconds` from now (`None` for no limit, or a limit that is not a
    /// positive finite number).
    pub fn with_limit(mut self, seconds: Option<f64>) -> Self {
        self.deadline = seconds
            .filter(|s| s.is_finite() && *s > 0.0)
            .and_then(|s| Some((web_time::Instant::now().checked_add(std::time::Duration::try_from_secs_f64(s).ok()?)?, s)));
        self
    }
    /// Why the script must stop now, if it must.
    pub fn stopped(&self) -> Option<String> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Some("the script was cancelled".into());
        }
        match self.deadline {
            Some((at, s)) if web_time::Instant::now() >= at => Some(format!("the script ran longer than its time limit ({s} s)")),
            _ => None,
        }
    }
}

/// Runs a script against the session and reports `{ok, result, output, error: {message, line,
/// column, file} | null}`.
pub type ScriptRunner = fn(&mut Session, &ScriptRequest) -> Value;

/// Loads one WebAssembly effect plug-in (`bytes`, `source` path) into the effect registry and
/// describes it (`{id, name, category, version, api, params}`).
pub type PluginLoader = fn(&[u8], &str) -> std::result::Result<Value, String>;

impl Default for Session {
    fn default() -> Self {
        let project = Arc::new(Project::default());
        Session {
            saved_project: project.clone(),
            project,
            history: History::default(),
            revision: 0,
            freeze_runtime: Default::default(),
            client: None,
            cursors: Default::default(),
            claims: Default::default(),
            epoch: 0,
            feed: Default::default(),
            session_id: build_info::next_session_id(),
            id_floor: 0,
            saved_revision: 0,
            path: None,
            state: EditorState { snapping: true, view_layout: 1, ..Default::default() },
            services: Arc::new(FsServices),
            footage: Arc::new(NoFootage),
            expr: None,
            accel: None,
            accel_note: None,
            window_adapter: None,
            expr_check: None,
            importer: None,
            exporter: None,
            storage_quota: None,
            render_job: None,
            track_job: None,
            mask_job: None,
            warp_job: None,
            warp_pending: vec![],
            analyses_checked: std::sync::Weak::new(),
            camera_job: None,
            camera_pending: vec![],
            roto_job: None,
            models: Default::default(),
            models_dir: None,
            roto_pending: vec![],
            events: vec![],
            journal: vec![],
            layer_cache: Arc::new(LayerCache::default()),
            cache_share: effectcraft_render::cache::shared::SharedBudget::new(),
            disk_cache: None,
            prefs: prefs::Prefs::default(),
            prefs_revision: 0,
            config: None,
            keymaps: shortcuts::Keymaps::default(),
            script_settings: Default::default(),
            ui_commands: vec![],
            shortcut_table: std::sync::OnceLock::new(),
            autosave: autosave::AutoSaveState::default(),
            autosave_folder_override: None,
            snapshot: None,
            frame_snapshots: Default::default(),
            script: None,
            script_interrupt: None,
            plugin_loader: None,
            script_ui: scriptui::ScriptUi::default(),
            sys_memory: None,
            key_move: None,
            memory_watch: Default::default(),
            memory_map: Default::default(),
            applied_nested_switches: None,
            offload: None,
            offloaded: vec![],
            offload_log: vec![],
            offload_log_start: 0,
            browser: None,
            storage: None,
            motion_library: Default::default(),
            followers: Default::default(),
            tasks: vec![],
            job_log: vec![],
            next_task_id: 1,
            learn: None,
            check_footage_on_open: false,
            footage_stamps: Default::default(),
            disk_salt: None,
            footage_scan: None,
            script_calls: ScriptCalls::default(),
        }
    }
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Run a command by id.
    ///
    /// Two execute parameters apply to every command (and are removed before it runs):
    /// `expectRevision` (refuse with an error, changing nothing, when the document's
    /// [`Session::revision`] is another) and `client` (who runs it: the undo step it makes is
    /// tagged with it, and `edit.undo` only undoes that client's own last step). Nested commands
    /// (batch steps, scripts) inherit the caller's client. A client works on its own cursor
    /// (selection, active comp, time) and can't change compositions other clients claimed (see
    /// [`agents`]).
    pub fn execute(&mut self, id: &str, mut params: Value) -> Result<Value> {
        let (expect, client) = match params.as_object_mut() {
            Some(o) => (o.remove("expectRevision"), o.remove("client")),
            None => (None, None),
        };
        if let Some(e) = expect.filter(|e| !e.is_null()) {
            let want = e.as_u64().ok_or_else(|| commands::bad(id, format!("`expectRevision` must be a revision number, got {e}")))?;
            if want != self.revision {
                return Err(EngineError::Other(format!("document changed (revision {}, expected {want}); read it again and retry", self.revision)));
            }
        }
        let client = match client {
            Some(Value::String(c)) if !c.is_empty() => Some(c),
            None | Some(Value::Null) => None,
            Some(c) => return Err(commands::bad(id, format!("`client` must be a non-empty string, got {c}"))),
        };
        match client {
            // Already running as this client (a nested step): its cursor is in place.
            Some(c) if self.client.as_deref() != Some(c.as_str()) => agents::run_as(self, c, |s| s.execute_inner(id, params)),
            _ => self.execute_inner(id, params),
        }
    }

    fn execute_inner(&mut self, id: &str, mut params: Value) -> Result<Value> {
        let spec = commands::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        // Times as the Timeline shows them ("6:20", {"frame": 200}) become seconds of the comp.
        readout::resolve_time_params(self, id, &mut params)?;
        let resolved = commands::validate_target_refs(self, id, &params)?;
        if let Err(why) = (spec.enabled)(self) {
            // Explicit targets (agents, scripts) don't need a UI selection.
            let explicit = (["layer", "layers", "prop", "keys"].iter().any(|k| params.get(k).is_some()) && commands::has_comp(self).is_ok())
                || ["item", "items"].iter().any(|k| params.get(k).is_some())
                // An explicit composition (the enablement looks at the active one).
                || params.get("comp").is_some_and(|c| !c.is_null() && self.resolve_comp(Some(c)).is_ok());
            if !explicit {
                return Err(EngineError::Disabled(id.to_string(), why));
            }
        }
        // Last-resort guard: a panicking command reports an error instead of crashing the app
        // (and saves a repro of the steps that led there, see `replay::after_command`).
        let nesting = replay::Nesting::enter(spec.journal);
        let caught = guard::caught();
        let r = guard::guarded(&format!("command `{id}`"), || (spec.run)(self, &params));
        let mut r = replay::after_command(self, id, &params, &nesting, caught, r)?;
        if spec.journal
            && resolved.as_object().is_some_and(|o| !o.is_empty())
            && let Some(reply) = r.as_object_mut()
        {
            reply.insert("resolved".into(), resolved);
        }
        self.learn_observe(id, &params);
        // Commands run by a journaled command (a script's steps) replay with it, not again.
        if spec.journal && !nesting.inside_journaled() {
            self.journal.push((id.to_string(), params));
            if self.journal.len() > 10_000 {
                self.journal.drain(..1000);
            }
        }
        Ok(r)
    }

    /// [`Session::execute`] for agents and scripts (MCP, CLI, control channel): unknown top-level
    /// parameters are rejected with the accepted keys instead of being silently ignored.
    pub fn execute_checked(&mut self, id: &str, params: Value) -> Result<Value> {
        let spec = commands::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        commands::check_params(spec, &params)?;
        self.execute(id, params)
    }

    /// [`Session::execute`] for a command a script asked for (the JavaScript engine's
    /// `app.run`, ScriptUI callbacks…). The command and everything it runs count as the script's:
    /// it may not change the settings that limit scripts (Settings ▸ Scripting & Expressions ▸
    /// Application Scripting), which only the user can, from the UI, CLI or control channel.
    pub fn execute_for_script(&mut self, id: &str, params: Value) -> Result<Value> {
        self.script_calls.0 = self.script_calls.0.saturating_add(1);
        let r = self.execute(id, params);
        self.script_calls.0 = self.script_calls.0.saturating_sub(1);
        r
    }

    /// Whether the command running was issued by a script ([`Session::execute_for_script`]).
    pub fn running_for_script(&self) -> bool {
        self.script_calls.0 > 0
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        commands::find(id).is_some_and(|c| (c.enabled)(self).is_ok())
    }

    /// Apply an undoable edit. `merge`: consecutive edits with the same key fold into one step.
    pub fn edit<T>(&mut self, label: &str, merge: Option<&str>, f: impl FnOnce(&mut Project, &mut EditorState) -> Result<T>) -> Result<T> {
        let before = self.project.clone();
        let mut p = (*self.project).clone();
        // Ids continue from the highest ever handed out, also after undo.
        let floor = p.next_id.max(self.id_floor);
        p.next_id = floor;
        let mut st = self.state.clone();
        let r = f(&mut p, &mut st)?;
        if p.next_id == floor {
            // Nothing allocated: keep the state's own counter (no spurious change).
            p.next_id = before.next_id;
        }
        // Layer styles: one Global Light per comp, whichever layer edited it.
        effectcraft_project::styles::sync_global_light(&before, &mut p);
        // Essential Properties of precomp layers follow their comps' Essential Graphics.
        effectcraft_project::essential::sync_project(&before, &mut p);
        // Speech maps follow their voice layers (moved, precomposed, copied: SX1).
        effectcraft_project::speech::sync_project(&before, &mut p);
        // Another client's claimed composition stays as it is (checked before analyses are queued again).
        if let Some(e) = self.claim_conflict(&before, &p) {
            return Err(e);
        }
        // Warp Stabilizer analyses made from other frames are cleared (and queued again). From a
        // checked state only the layers the edit changed are looked at (the rest are shared).
        let full = !std::ptr::eq(self.analyses_checked.as_ptr(), Arc::as_ptr(&before));
        for w in warp::invalidate(&before, &mut p, full) {
            if !self.warp_pending.contains(&w) {
                self.warp_pending.push(w);
            }
        }
        // So are 3D Camera Tracker tracks (and solves made with other settings).
        for w in camera_track::invalidate(&before, &mut p, full) {
            if !self.camera_pending.contains(&w) {
                self.camera_pending.push(w);
            }
        }
        // Roto Brush Input Keys follow the frames; edited instances propagate again.
        for r in roto::sync(&before, &mut p, full) {
            if !self.roto_pending.contains(&r) {
                self.roto_pending.push(r);
            }
        }
        // Nothing changed (a value set to what it already was): no undo step, not modified.
        if p.same_content(&before) {
            self.analyses_checked = Arc::downgrade(&before);
            self.state = st;
            return Ok(r);
        }
        // Footage removed: its files are closed (below).
        let removed = p.items.len() < before.items.len();
        // A continuous gesture folds into one step, unless another client made the last step.
        let same = merge.is_some() && merge.map(str::to_string) == self.history.merge_key && self.history.author(&before) == self.client.as_deref();
        if !same {
            // Undone steps aren't lost: they stay in the History panel as a branch.
            let levels = self.prefs.general.undo_levels.max(1) as usize;
            self.history.record(label, before, levels);
        } else {
            self.history.abandon_redo(&before);
        }
        self.history.merge_key = merge.map(str::to_string);
        self.feed.label = Some(label.to_string());
        self.feed.merge = same;
        self.project = Arc::new(p);
        if removed {
            self.release_unused_footage();
        }
        self.analyses_checked = Arc::downgrade(&self.project);
        self.history.set_author(&self.project, self.client.clone());
        self.state = st;
        self.bump();
        Ok(r)
    }

    pub fn bump(&mut self) {
        self.freeze_runtime.changed();
        self.revision += 1;
        self.feed.record(self.revision, self.client.clone(), &self.project, self.epoch, &self.history);
        *self.freeze_runtime.latest_source.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (self.epoch, self.project.clone());
        self.id_floor = self.id_floor.max(self.project.next_id);
        // Back at the saved state (undo, redo, a history jump or a rolled-back batch).
        if Arc::ptr_eq(&self.project, &self.saved_project) {
            self.saved_revision = self.revision;
        }
        self.refresh_disk_salt(false);
        self.events.push(Event::ProjectChanged { revision: self.revision });
    }

    /// Salt the disk cache's layer keys with the project's footage files
    /// ([`effectcraft_render::disk_cache::footage_salt`]). Taking it reads the size and time of
    /// every file, every frame of an image sequence included, so it is taken again only when the
    /// footage items changed, or with `force` (a project opened, footage reloaded, the disk cache
    /// attached), not at every edit: with a few large sequences that cost each keystroke in a
    /// text field over half a second (#1926).
    pub fn refresh_disk_salt(&mut self, force: bool) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        if self.disk_cache.is_none() {
            self.disk_salt = None;
            return None;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (id, it) in &self.project.items {
            if let effectcraft_project::ItemKind::Footage(f) = &it.kind {
                id.hash(&mut h);
                format!("{f:?}").hash(&mut h);
            }
        }
        let footage = h.finish();
        if let Some((seen, salt)) = self.disk_salt
            && seen == footage
            && !force
        {
            return Some(salt);
        }
        let salt = effectcraft_render::disk_cache::footage_salt(&self.project);
        self.disk_salt = Some((footage, salt));
        self.layer_cache.set_disk_salt(salt);
        Some(salt)
    }

    /// Set a comp's Resolution. As in After Effects it is saved with the project (which becomes
    /// modified) but is no undo step, and it changes no pixels: the comp's cached frames stay
    /// valid ([`effectcraft_project::Comp::same_pixels`]).
    pub fn set_resolution(&mut self, comp: ItemId, r: effectcraft_project::Resolution) -> Result<()> {
        if self.project.comp(comp).ok_or(EngineError::NoComp)?.resolution == r {
            return Ok(());
        }
        Arc::make_mut(&mut self.project).comp_mut(comp).ok_or(EngineError::NoComp)?.resolution = r;
        self.bump();
        Ok(())
    }

    /// Keep what lives outside the undo history (each comp's Resolution) as it is in `from`, the
    /// project before undo, redo or a history jump restored another state.
    pub(crate) fn keep_view_settings(&mut self, from: &Project) {
        let keep: Vec<(ItemId, effectcraft_project::Resolution)> =
            self.project.comps().filter_map(|(id, c)| from.comp(*id).map(|f| (*id, f.resolution)).filter(|(_, r)| *r != c.resolution)).collect();
        if keep.is_empty() {
            return;
        }
        let p = Arc::make_mut(&mut self.project);
        for (id, r) in keep {
            if let Some(c) = p.comp_mut(id) {
                c.resolution = r;
            }
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some((label, p)) = self.history.undo.pop() else { return false };
        self.feed.label = Some(format!("Undo {label}"));
        let cur = std::mem::replace(&mut self.project, p);
        self.keep_view_settings(&cur);
        self.history.redo.push((label, cur));
        self.history.merge_key = None;
        self.sanitize_state();
        self.bump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some((label, p)) = self.history.redo.pop() else { return false };
        self.feed.label = Some(format!("Redo {label}"));
        let cur = std::mem::replace(&mut self.project, p);
        let restored_comp = self.project.comps().find(|(id, _)| cur.comp(**id).is_none()).map(|(id, _)| *id);
        self.keep_view_settings(&cur);
        self.history.undo.push((label, cur));
        self.history.merge_key = None;
        self.sanitize_state();
        // Undoing a new composition closes its tab; redo should not leave an empty viewer.
        if self.active_comp_id().is_none()
            && let Some(id) = restored_comp
        {
            self.open_comp(id);
        }
        self.bump();
        true
    }

    /// Drop selections that no longer exist. Each comp keeps its own selection (#1001); a layer id
    /// counts only inside the comp that owns it, so the active comp never sees another comp's layers.
    pub fn sanitize_state(&mut self) {
        let p = self.project.clone();
        self.state.open_comps.retain(|c| p.comp(*c).is_some());
        self.state.comp_selections.retain(|cid, sel| {
            let Some(c) = p.comp(*cid) else { return false };
            let ids: Vec<LayerId> = c.layers.iter().map(|l| l.id).collect();
            sel.layers.retain(|l| ids.contains(l));
            sel.props.retain(|(l, _)| ids.contains(l));
            sel.keys.retain(|k| ids.contains(&k.layer));
            sel.vertices.retain(|v| ids.contains(&v.layer));
            !sel.is_empty()
        });
        if self.state.active_comp.is_some_and(|c| p.comp(c).is_none()) {
            self.clear_live_selection();
            self.state.active_comp = self.state.open_comps.first().copied();
            if let Some(id) = self.state.active_comp {
                self.recall_selection(id);
            }
        }
        if let Some(c) = self.active_comp() {
            let ids: Vec<LayerId> = c.layers.iter().map(|l| l.id).collect();
            self.state.selected_layers.retain(|l| ids.contains(l));
            self.state.selected_props.retain(|(l, _)| ids.contains(l));
            self.state.selected_keys.retain(|k| ids.contains(&k.layer));
            self.state.selected_vertices.retain(|v| ids.contains(&v.layer));
        }
        if let Some((l, t)) = self.state.current_track
            && self.active_comp().and_then(|c| c.layer(l)).and_then(|l| l.tracker(t)).is_none()
        {
            self.state.current_track = None;
        }
        self.state.project_selection.retain(|i| p.item(*i).is_some());
        commands::text_edit::sanitize(self);
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// The project as it is now is the saved one (after opening or saving it).
    pub fn mark_saved(&mut self) {
        self.saved_project = self.project.clone();
        self.saved_revision = self.revision;
    }

    /// The project has changes its file doesn't (a session restored with unsaved changes): it
    /// stays modified until it is saved, whatever is undone.
    pub fn mark_unsaved(&mut self) {
        self.saved_project = Arc::new(Project::default());
        self.bump();
    }

    pub fn active_comp(&self) -> Option<&Comp> {
        self.project.comp(self.state.active_comp?)
    }
    /// [`Session::active_comp`] as a shared snapshot (an `Arc` clone, not a deep copy).
    pub fn active_comp_arc(&self) -> Option<std::sync::Arc<Comp>> {
        self.project.comp_arc(self.state.active_comp?)
    }
    pub fn active_comp_id(&self) -> Option<ItemId> {
        self.state.active_comp.filter(|c| self.project.comp(*c).is_some())
    }

    /// Current time of a comp.
    pub fn time_of(&self, comp: ItemId) -> Tick {
        self.state.times.get(&comp).copied().unwrap_or(Tick::ZERO)
    }

    /// Current time of the active comp.
    pub fn time(&self) -> Tick {
        self.state.active_comp.and_then(|c| self.state.times.get(&c).copied()).unwrap_or(Tick::ZERO)
    }

    pub fn set_time(&mut self, t: Tick) {
        if let Some(c) = self.state.active_comp {
            let comp = self.project.comp(c);
            let t = match comp {
                Some(comp) => comp.frame_rate.snap_nearest(t.clamp(Tick::ZERO, comp.duration - comp.frame_duration())),
                None => t,
            };
            self.state.times.insert(c, t);
            if self.prefs.general.sync_time_related_items {
                self.sync_related_times(c, t);
            }
        }
    }

    /// Settings ▸ General ▸ Synchronize Time of All Related Items: moving the current time of
    /// `comp` moves the current time of the comps nested in it (through their precomp layers'
    /// timing) and of the comps it is nested in, so every related viewer shows the same moment.
    pub fn sync_related_times(&mut self, comp: ItemId, t: Tick) {
        // Who nests whom, from one pass over the layers: each nested comp's parents in project
        // order, with the first precomp layer of it in each. Searching the whole project for the
        // parents of every related comp took ~20 ms per call on a project with 380 comps, and
        // playback calls this on every frame (the UI thread then drew 25 frames a second).
        let mut parents: std::collections::BTreeMap<ItemId, Vec<(ItemId, usize)>> = std::collections::BTreeMap::new();
        for (pid, pc) in self.project.comps() {
            for (i, l) in pc.layers.iter().enumerate() {
                if let effectcraft_project::LayerSource::Comp { item } = &l.source {
                    let of = parents.entry(*item).or_default();
                    if of.last().is_none_or(|(p, _)| p != pid) {
                        of.push((*pid, i));
                    }
                }
            }
        }
        let mut seen = std::collections::BTreeSet::from([comp]);
        let mut todo = vec![(comp, t)];
        let mut found = vec![];
        while let Some((cid, ct)) = todo.pop() {
            if cid != comp {
                found.push((cid, ct));
            }
            let Some(c) = self.project.comp(cid) else { continue };
            // Down: the comps this one nests.
            for l in &c.layers {
                if let effectcraft_project::LayerSource::Comp { item } = &l.source
                    && seen.insert(*item)
                {
                    todo.push((*item, l.layer_time(ct)));
                }
            }
            // Up: the comps nesting this one.
            for &(pid, i) in parents.get(&cid).into_iter().flatten() {
                if let Some(l) = self.project.comp(pid).and_then(|pc| pc.layers.get(i))
                    && seen.insert(pid)
                {
                    todo.push((pid, l.comp_time(ct)));
                }
            }
        }
        for (cid, ct) in found {
            if let Some(c) = self.project.comp(cid) {
                let ct = c.frame_rate.snap_nearest(ct.clamp(Tick::ZERO, (c.duration - c.frame_duration()).max(Tick::ZERO)));
                self.state.times.insert(cid, ct);
            }
        }
    }

    /// Make `id` the active comp, parking the previous comp's selection and restoring this one's.
    pub fn activate_comp(&mut self, id: ItemId) {
        if self.project.comp(id).is_none() || self.state.active_comp == Some(id) {
            return;
        }
        self.remember_selection();
        self.state.active_comp = Some(id);
        self.recall_selection(id);
        self.state.current_track = None;
    }

    /// Leave no comp active, keeping the one that was active in [`EditorState::comp_selections`].
    pub fn activate_comp_none(&mut self) {
        self.remember_selection();
        self.state.active_comp = None;
        self.clear_live_selection();
        self.state.current_track = None;
    }

    fn remember_selection(&mut self) {
        let Some(id) = self.state.active_comp else { return };
        let sel = CompSelection {
            layers: self.state.selected_layers.clone(),
            props: self.state.selected_props.clone(),
            keys: self.state.selected_keys.clone(),
            vertices: self.state.selected_vertices.clone(),
        };
        if sel.is_empty() {
            self.state.comp_selections.remove(&id);
        } else {
            self.state.comp_selections.insert(id, sel);
        }
    }

    fn recall_selection(&mut self, id: ItemId) {
        match self.state.comp_selections.remove(&id) {
            Some(sel) => {
                self.state.selected_layers = sel.layers;
                self.state.selected_props = sel.props;
                self.state.selected_keys = sel.keys;
                self.state.selected_vertices = sel.vertices;
            }
            None => self.clear_live_selection(),
        }
    }

    fn clear_live_selection(&mut self) {
        self.state.selected_layers.clear();
        self.state.selected_props.clear();
        self.state.selected_keys.clear();
        self.state.selected_vertices.clear();
    }

    /// Open a comp in the viewer/timeline and make it active.
    pub fn open_comp(&mut self, id: ItemId) {
        if self.project.comp(id).is_none() {
            return;
        }
        if !self.state.open_comps.contains(&id) {
            self.state.open_comps.push(id);
        }
        if self.state.active_comp != Some(id) {
            let previous = self.state.active_comp;
            self.state.recent_comps.retain(|c| *c != id && Some(*c) != previous);
            if let Some(previous) = previous {
                self.state.recent_comps.insert(0, previous);
                self.state.recent_comps.truncate(32);
            }
        }
        self.activate_comp(id);
        self.events.push(Event::OpenComp(id));
    }

    /// The open compositions, as a project file records them ([`effectcraft_project::SavedView`]).
    pub fn saved_view(&self) -> effectcraft_project::SavedView {
        effectcraft_project::SavedView { active_comp: self.state.active_comp, open_comps: self.state.open_comps.clone() }
    }

    /// Reopen the compositions a project file was saved with (#748). Ids that no longer name a
    /// comp are skipped; with none left the project stays on the comp [`Session::replace_project`]
    /// opened (its first), as files from before the view was saved do.
    pub fn restore_view(&mut self, view: &effectcraft_project::SavedView) {
        let mut open: Vec<ItemId> = Vec::new();
        for id in view.open_comps.iter().copied().filter(|id| self.project.comp(*id).is_some()) {
            if !open.contains(&id) {
                open.push(id);
            }
        }
        let Some(active) = view.active_comp.filter(|id| self.project.comp(*id).is_some()).or_else(|| open.first().copied()) else { return };
        self.state.open_comps = open;
        self.open_comp(active);
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.events.push(Event::Toast { message: msg.into(), error: false });
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The 3D view camera of a comp's viewer (None = the active camera).
    pub fn view_camera(&self, comp: ItemId) -> Option<effectcraft_render::three_d::CameraState> {
        let c = self.project.comp(comp)?;
        if !c.has_3d() {
            return None;
        }
        self.state.views3d.get(&comp)?.override_camera(c.width as f64, c.height as f64)
    }

    /// Render a comp frame with the session's footage + expression hosts.
    pub fn render(&self, comp: ItemId, t: Tick, opts: RenderOpts) -> Image {
        let opts = RenderOpts { nested_switches: self.prefs.general.switches_affect_nested_comps, draft_shadows: self.prefs.three_d.realtime_shadows, ..opts };
        let mut r = Renderer::new(&self.project, self.footage.as_ref(), opts);
        r.expr = self.expr.as_deref();
        r.cache = Some(&self.layer_cache);
        r.accel = self.accel.as_deref();
        r.comp_frame(comp, t)
    }

    /// Resolve a comp reference (item id number or name; `None`/null = the active comp).
    pub fn resolve_comp(&self, comp: Option<&Value>) -> Result<ItemId> {
        let p = match comp {
            Some(v) if !v.is_null() => serde_json::json!({ "comp": v }),
            _ => serde_json::json!({}),
        };
        commands::comp_id(self, &p)
    }

    /// Render a comp frame (comp time `t`) as opaque 8-bit RGBA over the comp background, scaled
    /// so the longest side is at most `max_side` pixels (0 = full size). Returns (width, height, rgba).
    pub fn render_rgba8(&self, comp: ItemId, t: Tick, max_side: u32) -> Result<(u32, u32, Vec<u8>)> {
        let c = self.project.comp(comp).ok_or(EngineError::NoComp)?;
        let long = c.width.max(c.height).max(1) as f64;
        let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
        let img = self.render(comp, t, RenderOpts { scale, backend: effectcraft_render::Backend::Auto, ..Default::default() });
        Ok((img.width, img.height, img.to_rgba8_over(c.background)))
    }

    /// A Project panel thumbnail of an item (a comp at `t`, footage at its first frame), longest
    /// side at most `max_side`, as a job that owns everything it reads: frontends run it off the
    /// UI thread (a big comp takes far longer than a frame). `None` for items without pixels.
    pub fn thumbnail_job(&self, item: ItemId, t: Tick, max_side: u32) -> Option<ThumbnailJob> {
        let it = self.project.item(item)?;
        let footage = self.footage.clone();
        match &it.kind {
            effectcraft_project::ItemKind::Comp(c) => {
                let (project, expr, cache) = (self.project.clone(), self.expr.clone(), self.layer_cache.clone());
                let long = c.width.max(c.height).max(1) as f64;
                let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
                let opts = RenderOpts {
                    scale,
                    nested_switches: self.prefs.general.switches_affect_nested_comps,
                    draft_shadows: self.prefs.three_d.realtime_shadows,
                    ..Default::default()
                };
                let bg = c.background;
                Some(Box::new(move || {
                    let mut r = Renderer::new(&project, footage.as_ref(), opts);
                    r.expr = expr.as_deref();
                    r.cache = Some(&cache);
                    let img = r.comp_frame(item, t);
                    Some((img.width, img.height, img.to_rgba8_over(bg)))
                }))
            }
            effectcraft_project::ItemKind::Footage(f) if f.has_video => {
                let f = f.clone();
                Some(Box::new(move || {
                    let img = footage.frame(item, &f, Tick(0))?;
                    // Point-sample down to at most `max_side` on the long side.
                    let step = (img.width.max(img.height) as f32 / max_side.max(1) as f32).ceil().max(1.0) as u32;
                    let (w, h) = (img.width.div_ceil(step), img.height.div_ceil(step));
                    let full = img.to_rgba8();
                    let mut px = Vec::with_capacity((w * h * 4) as usize);
                    for y in 0..h {
                        for x in 0..w {
                            let i = (((y * step) * img.width + x * step) * 4) as usize;
                            px.extend_from_slice(&full[i..i + 4]);
                        }
                    }
                    Some((w, h, px))
                }))
            }
            _ => None,
        }
    }

    /// [`Session::render_rgba8`], or with `transparent` the frame's own straight alpha (what a
    /// render with RGB + Alpha channels writes) instead of compositing over the background.
    pub fn render_rgba8_alpha(&self, comp: ItemId, t: Tick, max_side: u32, transparent: bool) -> Result<(u32, u32, Vec<u8>)> {
        if !transparent {
            return self.render_rgba8(comp, t, max_side);
        }
        let c = self.project.comp(comp).ok_or(EngineError::NoComp)?;
        let long = c.width.max(c.height).max(1) as f64;
        let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
        let img = self.render(comp, t, RenderOpts { scale, backend: effectcraft_render::Backend::Auto, ..Default::default() });
        Ok((img.width, img.height, img.to_rgba8()))
    }

    /// Close the files of footage the project no longer uses (decoders, parsed models, cached
    /// frames): footage removed from the project, or the previous project's.
    fn release_unused_footage(&self) {
        let mut used = std::collections::HashSet::new();
        for it in self.project.items.values() {
            if let effectcraft_project::ItemKind::Footage(f) = &it.kind {
                used.insert(f.path.as_str());
            }
            if let Some(px) = it.proxy.as_deref() {
                used.insert(px.footage.path.as_str());
            }
        }
        self.footage.retain_files(&|p| used.contains(p));
    }

    /// Make the media layer and the layer cache let go of what they decoded from `footage`'s
    /// files (it keys frames by path, so an overwritten file would keep showing its old pixels).
    pub(crate) fn forget_decoded(&self, footage: &effectcraft_project::Footage) {
        for p in std::iter::once(&footage.path).chain(&footage.sequence) {
            self.footage.forget(p);
        }
        self.layer_cache.clear();
    }

    /// Replace the whole project (open/new), resetting history and state.
    pub fn replace_project(&mut self, p: Project, path: Option<String>) {
        self.replace_project_with(p, path, None);
    }

    /// [`Session::replace_project`], reopening the comps that were open when it was saved
    /// ([`Session::restore_open_comps`]); without any (or none left), the first comp opens.
    pub fn replace_project_with(&mut self, mut p: Project, path: Option<String>, open: Option<&effectcraft_project::OpenComps>) {
        if self.stop_render()
            && let Some(mut job) = self.render_job.take()
        {
            // Let the worker notice the cancel; its updates refer to the old project.
            job.wait();
        }
        self.render_job = None;
        self.stop_track();
        self.track_job = None;
        self.stop_mask_track();
        self.mask_job = None;
        self.stop_warp();
        self.warp_job = None;
        self.warp_pending.clear();
        self.stop_camera();
        self.camera_job = None;
        self.camera_pending.clear();
        self.stop_roto();
        self.roto_job = None;
        self.roto_pending.clear();
        self.drop_tasks();
        self.freeze_runtime.proofs.clear();
        self.freeze_runtime.pixel_epochs.clear();
        self.freeze_runtime.bad_versions.clear();
        self.freeze_runtime.reasons.clear();
        self.freeze_runtime.retry_after.clear();
        self.freeze_runtime.cost.clear();
        self.script_ui.project_epoch = self.script_ui.project_epoch.wrapping_add(1);
        p.fix_next_id();
        upgrade_effects(&mut p);
        // The media layer keys decoded frames by path: files of the new project may have changed
        // since an earlier project decoded them (#1070).
        for it in p.items.values() {
            let footage = match &it.kind {
                effectcraft_project::ItemKind::Footage(f) => Some(f),
                _ => None,
            };
            for f in footage.into_iter().chain(it.proxy.as_ref().map(|p| &p.footage)) {
                self.forget_decoded(f);
            }
        }
        self.project = Arc::new(p);
        self.release_unused_footage();
        // The old document's rendered layers and host GPU caches must not survive reopening
        // a project (#591). Notify before restoring tabs, including the early-return path.
        self.layer_cache.clear();
        self.events.push(Event::PurgeCaches);
        // Its files may have changed since they were last stamped.
        self.disk_salt = None;
        self.history = History::default();
        self.id_floor = 0;
        self.state = EditorState { snapping: true, view_layout: 1, ..Default::default() };
        // Cursors and claims point into the old project.
        self.cursors.clear();
        self.claims.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.path = path;
        self.bump();
        self.mark_saved();
        if open.is_some_and(|o| self.restore_open_comps(o)) {
            return;
        }
        let first = self.project.comps().next().map(|(id, _)| *id);
        if let Some(c) = first {
            self.open_comp(c);
        }
    }

    /// The comps open in the user's Timeline/Composition tabs, the front one and their current
    /// times, for the project file (`openComps`, see [`effectcraft_project::OpenComps`]). Always
    /// the user's view: while an agent's command runs (a `client`, see [`agents`]) its own
    /// cursor is in [`Session::state`] and the user's waits in [`Session::cursors`]. `None`
    /// when no comp is open.
    pub fn open_comps_to_save(&self) -> Option<effectcraft_project::OpenComps> {
        let (active, times) = match self.cursors.user() {
            Some(u) => (u.active_comp, &u.times),
            None => (self.state.active_comp, &self.state.times),
        };
        let comps: Vec<effectcraft_project::OpenComp> = self
            .state
            .open_comps
            .iter()
            .filter(|c| self.project.comp(**c).is_some())
            .map(|c| effectcraft_project::OpenComp { comp: *c, time: times.get(c).copied().unwrap_or(Tick::ZERO) })
            .collect();
        if comps.is_empty() {
            return None;
        }
        let active = active.filter(|a| comps.iter().any(|c| c.comp == *a));
        Some(effectcraft_project::OpenComps { comps, active })
    }

    /// Reopen the comps a project file says were open (After Effects reopens a project's
    /// Timeline tabs): in their tab order, each at its saved current time (clamped to the comp
    /// and snapped to a frame), the saved front one active. Ids of comps that no longer exist
    /// (or repeat) are skipped. Returns whether any comp opened. Not an edit: no undo step and
    /// the project stays unmodified.
    pub fn restore_open_comps(&mut self, open: &effectcraft_project::OpenComps) -> bool {
        let mut comps: Vec<ItemId> = Vec::new();
        for oc in &open.comps {
            let Some(c) = self.project.comp(oc.comp) else { continue };
            if comps.contains(&oc.comp) {
                continue;
            }
            let last = (c.duration - c.frame_duration()).max(Tick::ZERO);
            self.state.times.insert(oc.comp, c.frame_rate.snap_nearest(oc.time.clamp(Tick::ZERO, last)));
            comps.push(oc.comp);
        }
        let Some(active) = open.active.filter(|a| comps.contains(a)).or_else(|| comps.first().copied()) else { return false };
        self.state.open_comps = comps;
        self.open_comp(active);
        true
    }
}

/// Bring effect instances saved by earlier versions up to date with the effect registry
/// (renamed / regrouped / added parameters, reordered popups; see
/// [`effectcraft_effects::migrate`]), and a project from before
/// [`effectcraft_project::SCHEMA_EFFECT_SPACE`] into effect space
/// ([`effectcraft_effects::migrate::to_effect_space`]). Effects imported from After Effects as
/// plain groups before EffectCraft knew them become native effects
/// ([`effectcraft_aep::plain::native_effect`]). Stamps the current schema.
pub fn upgrade_effects(p: &mut Project) {
    let to_effect_space = p.schema < effectcraft_project::SCHEMA_EFFECT_SPACE;
    // The layers with an instance to change (any registered effect when moving into effect
    // space): only those are made unique, so a project of current instances isn't copied.
    let mut todo = std::collections::HashSet::new();
    for (cid, c) in p.comps() {
        for l in &c.layers {
            let Some(fx) = l.props.sub("effects") else { continue };
            let stale = fx.groups().any(|g| match &g.kind {
                effectcraft_project::GroupKind::Effect { effect } => {
                    effectcraft_effects::find(effect).is_some_and(|spec| to_effect_space || !effectcraft_effects::migrate::is_current(spec, g))
                }
                effectcraft_project::GroupKind::Plain => effectcraft_aep::plain::is_convertible(g),
                _ => false,
            });
            if stale {
                todo.insert((*cid, l.id));
            }
        }
    }
    if todo.is_empty() {
        p.schema = effectcraft_project::SCHEMA_VERSION;
        return;
    }
    // Effect bounds per layer: (size, top-left in layer coordinates).
    let mut bounds = std::collections::HashMap::new();
    for (cid, c) in p.comps() {
        for l in &c.layers {
            let (size, origin) = effectcraft_render::effect_bounds(p, c, l);
            bounds.insert((*cid, l.id), (size, origin, matches!(l.source, effectcraft_project::LayerSource::Shape)));
        }
    }
    let mut next = p.next_id;
    for (cid, it) in p.items.iter_mut() {
        let effectcraft_project::ItemKind::Comp(c) = &mut it.kind else { continue };
        let cid = *cid;
        if !c.layers.iter().any(|l| todo.contains(&(cid, l.id))) {
            continue;
        }
        for l in std::sync::Arc::make_mut(c).layers.iter_mut_where(|l| todo.contains(&(cid, l.id))) {
            let (size, origin, _) = bounds.get(&(cid, l.id)).copied().unwrap_or(([100.0, 100.0], [0.0; 2], false));
            // Effect space starts at the bounds' top-left: positions move by minus that corner.
            // Adjustment layers run their effects on the comp below as it is: nothing moves.
            // Shape layers already measured effect points from their anchor-centred comp-sized
            // canvas before schema 2 (as After Effects does): nothing moves either.
            let shift = |o: [f64; 2]| [-o[0], -o[1]];
            let d = if l.switches.adjustment || matches!(l.source, effectcraft_project::LayerSource::Shape) { [0.0; 2] } else { shift(origin) };
            let clone_d = |src: Option<u64>| match src {
                Some(id) => bounds.get(&(cid, effectcraft_project::LayerId(id))).map_or([0.0; 2], |b| if b.2 { [0.0; 2] } else { shift(b.1) }),
                None => d,
            };
            let Some(fx) = l.props.sub_mut("effects") else { continue };
            for n in &mut fx.children {
                let effectcraft_project::Node::Group(g) = n else { continue };
                if let Some(native) = effectcraft_aep::plain::native_effect(g, &mut effectcraft_project::build::Ids(&mut next), size) {
                    *g = native;
                }
                let effectcraft_project::GroupKind::Effect { effect } = &g.kind else { continue };
                if let Some(spec) = effectcraft_effects::find(effect) {
                    effectcraft_effects::migrate::upgrade_instance(spec, g, &mut effectcraft_project::build::Ids(&mut next), size);
                    if to_effect_space {
                        effectcraft_effects::migrate::to_effect_space(spec, g, size, d, &clone_d);
                    }
                }
            }
        }
    }
    p.next_id = next;
    p.schema = effectcraft_project::SCHEMA_VERSION;
}

/// Font families available to text layers (bundled + scanned system fonts).
/// Text animation presets: (id, name), for menus.
pub fn text_presets() -> Vec<(String, String)> {
    commands::text_preset_list()
}

/// Built-in animation presets (Effects & Presets ▸ * Animation Presets): (id, name, category).
pub fn animation_presets() -> Vec<(String, String, String)> {
    commands::builtin_preset_list()
}

pub fn text_families() -> Vec<String> {
    effectcraft_text::families().into_iter().map(|(f, _)| f).collect()
}

/// The styles a family offers (its own faces and a variable face's named instances, in weight
/// order), for the style menus. A PostScript or full name (`SFPro-Regular`, how After Effects
/// names fonts) offers its family's styles. A family that isn't installed offers the usual
/// styles, which resolve to Inter's.
pub fn font_styles(family: &str) -> Vec<String> {
    let family = effectcraft_text::fonts::family_of(family).unwrap_or_else(|| family.to_string());
    effectcraft_text::families()
        .into_iter()
        .find(|(f, _)| f.eq_ignore_ascii_case(&family))
        .map(|(_, st)| st)
        .filter(|st| !st.is_empty())
        .unwrap_or_else(|| ["Regular", "Medium", "SemiBold", "Bold", "Italic"].iter().map(|s| s.to_string()).collect())
}

/// The OpenType feature tags (GSUB / GPOS) of the face a family + style resolves to.
pub fn font_features(family: &str, style: &str) -> Vec<String> {
    effectcraft_text::fonts::face(effectcraft_text::resolve(family, style).face).features()
}

/// One row of the Character panel's font menu.
#[derive(Clone, Debug, PartialEq)]
pub struct FontRow {
    /// The family to apply (empty for the separator after the recent fonts).
    pub family: String,
    /// What the menu shows: the English family name, or the font's own-language name when
    /// Settings ▸ Type ▸ Show Font Names in English is off.
    pub display: String,
    pub recent: bool,
    /// A variable font (the menu's Variable Fonts filter keeps only these).
    pub variable: bool,
}

/// Outline polylines of "Sample" set in `family` at `size` px (baseline at y = 0, y down), for
/// the font menu's preview (Settings ▸ Type ▸ Show Font Preview).
pub fn font_preview(family: &str, size: f64) -> Vec<Vec<[f32; 2]>> {
    use kurbo::PathEl;
    let st = effectcraft_keyframe::text_doc::CharStyle { font: family.to_string(), size, ..Default::default() };
    let mut out = vec![];
    let mut x = 0.0;
    for ch in "Sample".chars() {
        let (path, adv) = effectcraft_text::char_glyph_style(&st, ch);
        let mut cur: Vec<[f32; 2]> = vec![];
        let pt = |p: kurbo::Point| [(p.x + x) as f32, p.y as f32];
        kurbo::flatten(&path, 0.2, |el| match el {
            PathEl::MoveTo(p) => {
                if cur.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                }
                cur = vec![pt(p)];
            }
            PathEl::LineTo(p) => cur.push(pt(p)),
            PathEl::ClosePath => {
                if let Some(f) = cur.first().copied() {
                    cur.push(f);
                }
                if cur.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => {}
        });
        if cur.len() > 1 {
            out.push(cur);
        }
        x += adv;
    }
    out
}

/// The Character panel's font menu: the recent fonts (Settings ▸ Type ▸ Number of Recent Fonts
/// to Display), a separator, then every family. Each row says whether the family is variable,
/// for the menu's Variable Fonts filter.
pub fn font_menu(prefs: &prefs::Prefs) -> Vec<FontRow> {
    let all = text_families();
    let native = if prefs.type_.font_names_in_english { Default::default() } else { effectcraft_text::fonts::native_families() };
    let variable = effectcraft_text::fonts::variable_families();
    let row = |f: &String, recent: bool| FontRow {
        family: f.clone(),
        display: native.get(f).cloned().unwrap_or_else(|| f.clone()),
        recent,
        variable: variable.contains(f),
    };
    let mut out: Vec<FontRow> = prefs.recent_fonts_shown().iter().filter(|f| all.contains(f)).map(|f| row(f, true)).collect();
    if !out.is_empty() {
        out.push(FontRow { family: String::new(), display: "-".into(), recent: false, variable: false });
    }
    out.extend(all.iter().map(|f| row(f, false)));
    out
}
