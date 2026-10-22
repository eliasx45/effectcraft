//! Programmatic control of the running app (agents, tests, MCP bridge).
//!
//! Transport-agnostic: a transport thread (TCP in the desktop app) sends [`ControlRequest`]s; the
//! UI thread handles them between frames and replies with `{"ok":…, "result"|"error":…}`.
//!
//! Methods:
//! - `engine.execute {command, params}` / `ui.menu.invoke {id, params}`: run any engine or UI command
//! - `engine.commands`: list engine commands; `ui.menu.list`: menu tree with enablement
//! - `ui.inspect`: UI state + summary; `ui.elements {prefix?}`: registered widgets (id, label, rect)
//! - `ui.set {tool?, workspace?, theme?, focused?, viewer?: {zoom, res, pan, grid, …}, timeline?: {pps, start, graphEditor, showModes}}`
//! - `ui.panel.show {panel}` / `ui.panel.close {panel}`
//! - `ui.click {id | x,y, button?, count?, modifiers?}` / `ui.move {x,y}` / `ui.scroll {x,y,dx,dy,unit?,pinch?}`
//! - `ui.drag {from:{id|x,y}, to:{id|x,y}, steps?, modifiers?}`: synthetic press-move-release
//! - `ui.key {key, command?, shift?, alt?, ctrl?, down?, up?}` (`down`/`up`: hold, then release) / `ui.type {text}`
//! - `ui.viewer.locate {x, y}`: screen point of a comp pixel; `ui.viewer.hit {x, y}`: comp pixel at a screen point
//! - `ui.timeline.locate {layer, time?}`: screen point of a layer bar (at a comp time)
//! - `ui.playback {action: play|stop|toggle}`
//! - `ui.screenshot {path?, panel?, id?, max_side?}`: PNG of the window (or one panel / element),
//!   longest side at most `max_side` pixels (default 1600, 0 = full size)
//! - `app.revision`: the session's `{revision}` (a cheap change check)
//! - `app.sync {follower, have?}`: what a render box needs to mirror the project (a patch since
//!   the app revision `have`, or the whole project) and the cursors ([`effectcraft_engine::remote_follow`])
//! - `render.frame {comp?, item?, layer?, time?, max_side?, transparent?, region?, overlays?, path?, base64?}`: PNG of a frame rendered by the session
//! - `ui.figmaPair {client}`: the one-time Figma pairing prompt; replies `{allowed}` once answered
//! - `figcraft.frames {comp, frames, maxSide, quality?}` / `figcraft.audio {comp, start, end, rate?}` /
//!   `figcraft.tree {comp, start?, end?, depth?}` / `figcraft.reveal {comp, time}`: the FigCraft link
//!   ([`crate::figcraft_link`])
//! - `ui.resize {width, height}` / `ui.focus` / `app.quit {force?}` (a modified project asks to save first unless `force`)
//!
//! The full reference with examples is `docs/control-protocol.md`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::dock::PanelKind;
use crate::state::{Resolution, Tool};

/// Prefix marking errors that may resolve after another frame.
pub const RETRY: &str = "\u{1}";

/// `ui.screenshot`'s default longest side in pixels (a full 4K window is a 30 MB PNG otherwise).
pub const SCREENSHOT_MAX_SIDE: u32 = 1600;

pub struct ControlRequest {
    pub method: String,
    pub params: Value,
    pub reply: Sender<Value>,
    /// Shared with the transport: a request it gave up on (timed out) before the UI thread
    /// started it is skipped, so a client's retry can't make it run twice.
    pub state: RequestState,
}

impl ControlRequest {
    pub fn new(method: impl Into<String>, params: Value) -> (Self, Receiver<Value>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { method: method.into(), params, reply: tx, state: RequestState::default() }, rx)
    }
}

/// Whether a queued [`ControlRequest`] was started or cancelled, shared by the transport thread
/// and the UI thread (whichever acts first wins).
#[derive(Clone, Debug, Default)]
pub struct RequestState(Arc<AtomicU8>);

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

impl RequestState {
    /// The UI thread calls this before handling the request: `false` when it was cancelled (skip
    /// it). A deferred request that already started may be handled again.
    pub fn start(&self) -> bool {
        match self.0.compare_exchange(QUEUED, STARTED, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => true,
            Err(now) => now == STARTED,
        }
    }

    /// The transport calls this when it gives up waiting: `true` when the request had not
    /// started, so it never will; `false` when it is already running (it may still complete).
    pub fn cancel(&self) -> bool {
        self.0.compare_exchange(QUEUED, CANCELLED, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst) == CANCELLED
    }
}

/// Change events for control-channel connections that called `events.subscribe`: the transport
/// subscribes, the app publishes `project.changed` and `selection.changed` (coalesced, at most ten
/// bursts a second). Subscribers that went away are dropped on the next publish.
#[derive(Clone, Default)]
pub struct ControlEvents(Arc<Mutex<EventsInner>>);

#[derive(Default)]
struct EventsInner {
    subscribers: Vec<Sender<Value>>,
    /// The session revision at the last frame.
    revision: u64,
}

impl ControlEvents {
    /// A new subscriber's event stream, and the current revision.
    pub fn subscribe(&self) -> (Receiver<Value>, u64) {
        let (tx, rx) = std::sync::mpsc::channel();
        (rx, self.attach(tx))
    }

    /// Subscribe `tx` (whose owner may keep a clone, e.g. to wake its reader when the connection
    /// closes); returns the current revision.
    pub fn attach(&self, tx: Sender<Value>) -> u64 {
        let mut g = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        g.subscribers.push(tx);
        g.revision
    }

    pub fn subscribers(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).subscribers.len()
    }

    pub(crate) fn set_revision(&self, revision: u64) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).revision = revision;
    }

    /// Send `event` to every subscriber, dropping those that went away.
    pub fn publish(&self, event: &Value) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).subscribers.retain(|s| s.send(event.clone()).is_ok());
    }
}

pub enum Outcome {
    Done(Value),
    /// Try again on a later frame (e.g. the element is not on screen yet).
    Retry(String),
    /// Reply once queued synthetic input has been processed.
    AfterInput,
    Screenshot {
        path: Option<String>,
        crop: Option<[f32; 4]>,
        /// Longest side in pixels (0 = full size).
        max_side: u32,
    },
    /// Reply once background jobs end (a `wait: true` command whose job runs in a worker).
    AwaitJobs(JobWaiter),
    /// The handler kept the request's reply and sends it later.
    Held,
}

/// A `wait: true` engine command (or `jobs.wait`) whose job runs in the background instead of
/// blocking the UI thread (shared with the windowless host).
pub use effectcraft_engine::job_waiter::JobWaiter;

/// The most time one frame spends running control requests (the rest wait for the next frame).
pub const CONTROL_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// Requests in turns by client (`engine.execute`'s `client`; the user and UI requests count as
/// one): each client's first request, then each one's second, and so on, in arrival order.
pub fn fair_order<T>(reqs: Vec<(ControlRequest, T)>) -> Vec<(ControlRequest, T)> {
    let client = |r: &ControlRequest| r.params.get("params").and_then(|p| p.get("client")).and_then(Value::as_str).unwrap_or_default().to_string();
    let mut queues: Vec<(String, std::collections::VecDeque<(ControlRequest, T)>)> = vec![];
    for r in reqs {
        let c = client(&r.0);
        match queues.iter_mut().find(|(k, _)| *k == c) {
            Some((_, q)) => q.push_back(r),
            None => queues.push((c, std::collections::VecDeque::from([r]))),
        }
    }
    let mut out = vec![];
    while queues.iter().any(|(_, q)| !q.is_empty()) {
        for (_, q) in queues.iter_mut() {
            if let Some(r) = q.pop_front() {
                out.push(r);
            }
        }
    }
    out
}

/// Run an engine command. A waiting command (`wait: true`) runs with `wait: false` instead, and
/// the reply waits for the job it started ([`Outcome::AwaitJobs`]), so the UI keeps drawing and
/// other clients' requests run while it works. `jobs.wait` likewise waits for the running tasks.
/// An engine command that only reads (not journaled, e.g. `speech.get`, `comp.info`).
fn is_query(id: &str) -> bool {
    effectcraft_engine::find_command(id).is_some_and(|c| !c.journal)
}

fn execute_engine(app: &mut EffectcraftApp, id: &str, params: Value) -> Result<Outcome, String> {
    Ok(match effectcraft_engine::job_waiter::start(&mut app.session, id, params)? {
        effectcraft_engine::job_waiter::Started::Done(v) => ok(v),
        effectcraft_engine::job_waiter::Started::Waiting(w) => Outcome::AwaitJobs(w),
    })
}

fn ok(v: Value) -> Outcome {
    Outcome::Done(json!({"ok": true, "result": v}))
}
fn err(e: impl std::fmt::Display) -> Outcome {
    Outcome::Done(json!({"ok": false, "error": e.to_string()}))
}

fn modifiers(p: &Value) -> egui::Modifiers {
    let m = p.get("modifiers").unwrap_or(p);
    let b = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    egui::Modifiers { alt: b("alt"), ctrl: b("ctrl"), shift: b("shift"), mac_cmd: cfg!(target_os = "macos") && b("command"), command: b("command") }
}

/// Resolve a point from `{id}` (element centre) or `{x, y}`.
fn point(app: &EffectcraftApp, p: &Value) -> Result<egui::Pos2, String> {
    if let Some(id) = p.get("id").and_then(Value::as_str) {
        let e = app.auto.find_current(id).ok_or_else(|| format!("{RETRY}no element `{id}` (see ui.elements)"))?;
        let fx = p.get("fx").and_then(Value::as_f64).unwrap_or(0.5) as f32;
        let fy = p.get("fy").and_then(Value::as_f64).unwrap_or(0.5) as f32;
        return Ok(egui::pos2(e.rect[0] + e.rect[2] * fx, e.rect[1] + e.rect[3] * fy));
    }
    let x = p.get("x").and_then(Value::as_f64).ok_or("need `id` or `x`,`y`")?;
    let y = p.get("y").and_then(Value::as_f64).ok_or("need `y`")?;
    Ok(egui::pos2(x as f32, y as f32))
}

/// The undocked OS window a point is in: its element's window (`id`), else `window` (a floating
/// group id, see `ui.elements`); `None` for the main window.
fn point_window(app: &EffectcraftApp, p: &Value) -> Option<u64> {
    match p.get("id").and_then(Value::as_str) {
        Some(id) => app.auto.find_current(id).and_then(|e| e.window),
        None => p.get("window").and_then(Value::as_u64),
    }
}

pub fn handle(app: &mut EffectcraftApp, ctx: &egui::Context, req: &ControlRequest) -> Outcome {
    let p = &req.params;
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match req.method.as_str() {
        "engine.execute" | "ui.menu.invoke" => {
            let Some(id) = s("command").or(s("id")) else { return err("missing `command`") };
            let params = p.get("params").cloned().filter(|v| !v.is_null()).unwrap_or(json!({}));
            // `engine.execute` runs engine commands exactly as headless (never opens a dialog, e.g.
            // `comp.new {}` creates a default comp); UI-only ids (`tool.*`, `view.*`, `window.*`,
            // `playback.*` …) and `ui.menu.invoke` go through the menu dispatcher like a click.
            let r = if req.method == "engine.execute" && effectcraft_engine::find_command(id).is_some() {
                execute_engine(app, id, params)
            } else {
                crate::menus::invoke(app, ctx, id, params).map(ok)
            };
            match r {
                Ok(o) => o,
                Err(e) => {
                    // A failed query (a read: `speech.get`, `comp.info`, …) changed nothing; only its
                    // caller hears about it. Background readers (the learning loop asks `speech.get`
                    // on every comp, and most comps have no speech map) must not toast the user.
                    if !is_query(id) {
                        app.ui.status = e.clone();
                    }
                    err(e)
                }
            }
        }
        "engine.commands" => match app.session.execute("command.list", json!({"filter": p.get("filter"), "enabledOnly": p.get("enabledOnly")})) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        "ui.menu.list" => ok(serde_json::to_value(crate::menus::menu_items(app)).unwrap_or_default()),
        "ui.inspect" => ok(inspect(app, ctx)),
        "ui.elements" => {
            let prefix = s("prefix").unwrap_or("");
            ok(serde_json::to_value(app.auto.query_current(prefix)).unwrap_or_default())
        }
        "ui.set" => {
            if let Some(t) = s("tool") {
                match Tool::from_name(t) {
                    Some(t) => app.ui.tool = t,
                    None => return err(format!("unknown tool `{t}`")),
                }
            }
            if let Some(w) = s("workspace") {
                match crate::dock::WORKSPACES.iter().find(|x| x.eq_ignore_ascii_case(w)) {
                    Some(w) => app.set_workspace(w),
                    None => return err(format!("unknown workspace `{w}`")),
                }
            }
            if let Some(th) = s("theme") {
                match crate::theme::ThemeKind::from_name(th) {
                    Some(k) => app.set_theme(ctx, k),
                    None => return err("unknown theme (dark, darker, light)"),
                }
            }
            if let Some(f) = s("focused").and_then(PanelKind::from_name) {
                app.ui.focused = f;
            }
            if let Some(v) = p.get("viewer") {
                if let Some(z) = v.get("zoom") {
                    app.ui.viewer.zoom = z.as_f64().map(|z| z as f32);
                }
                // The active comp's Resolution (`view.res.*`), saved with the project.
                if let Some(r) = v.get("res").and_then(Value::as_str) {
                    let Some(res) = Resolution::ALL.into_iter().find(|x| x.label().eq_ignore_ascii_case(r)) else {
                        return err(format!("unknown resolution `{r}` (auto, full, half, third, quarter)"));
                    };
                    if let Some(cid) = app.session.active_comp_id()
                        && let Err(e) = app.session.set_resolution(cid, res)
                    {
                        return err(e);
                    }
                }
                if let Some(Value::Array(a)) = v.get("pan") {
                    app.ui.viewer.pan = [a.first().and_then(Value::as_f64).unwrap_or(0.0) as f32, a.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32];
                }
                for (k, slot) in [
                    ("grid", &mut app.ui.viewer.grid),
                    ("rulers", &mut app.ui.viewer.rulers),
                    ("safeMargins", &mut app.ui.viewer.safe_margins),
                    ("transparencyGrid", &mut app.ui.viewer.transparency_grid),
                ] {
                    if let Some(b) = v.get(k).and_then(Value::as_bool) {
                        *slot = b;
                    }
                }
            }
            if let Some(tl) = p.get("timeline") {
                if let Some(v) = tl.get("pps") {
                    app.ui.timeline.pps = v.as_f64();
                }
                if let Some(v) = tl.get("start").and_then(Value::as_f64) {
                    app.ui.timeline.start = v;
                }
                if let Some(mode) = tl.get("graphMode").and_then(Value::as_str) {
                    if !matches!(mode, "auto" | "value" | "speed") {
                        return err("graphMode must be auto, value or speed");
                    }
                    app.ui.timeline.graph_mode = mode.to_owned();
                }
                if let Some(value) = tl.get("graphShowSet").and_then(Value::as_bool) {
                    app.ui.timeline.graph_show_set = value;
                }
                if let Some(value) = tl.get("graphAutoZoom").and_then(Value::as_bool) {
                    app.ui.timeline.graph_auto_zoom = value;
                }
                if let Some(v) = tl.get("graphEditor").and_then(Value::as_bool) {
                    app.ui.timeline.graph_editor = v;
                }
                if let Some(v) = tl.get("showModes").and_then(Value::as_bool) {
                    app.ui.timeline.show_modes = v;
                }
                if let Some(Value::Array(a)) = tl.get("openLayers") {
                    app.ui.timeline.open_layers = a.iter().filter_map(Value::as_u64).collect();
                }
                if let Some(Value::Array(a)) = tl.get("openGroups") {
                    app.ui.timeline.open_groups = a.iter().filter_map(Value::as_u64).collect();
                }
            }
            if let Some(b) = p.get("menuBar").and_then(Value::as_bool) {
                app.ui.show_menu_bar = b;
            }
            // Project panel optional columns, e.g. ["type", "duration", "path", "comment"].
            if let Some(Value::Array(a)) = p.get("projectColumns") {
                let mut cols = vec![];
                for k in a.iter().filter_map(Value::as_str) {
                    crate::panels::project::set_column(&mut cols, k, true);
                }
                app.ui.project_columns = cols;
            }
            // Render Queue column widths, e.g. {"status": 140, "name": 300}; null resets a column
            // to its default (and Comp Name to filling the panel).
            if let Some(Value::Object(m)) = p.get("renderQueueColumns") {
                for (k, v) in m {
                    if v.is_null() {
                        app.ui.render_queue_columns.remove(k);
                    } else if !v.as_f64().is_some_and(|w| crate::panels::render_queue::set_column_width(&mut app.ui.render_queue_columns, k, w as f32)) {
                        return err(format!("renderQueueColumns: `{k}` needs a width in points (columns: render, label, num, name, status, started, time)"));
                    }
                }
            }
            // Wiggler / Smoother / Motion Sketch settings (fields of `AnimToolsState`).
            if let Some(Value::Object(m)) = p.get("animTools") {
                let mut cur = serde_json::to_value(&app.ui.anim_tools).unwrap_or_default();
                for (k, v) in m {
                    cur[k] = v.clone();
                }
                match serde_json::from_value(cur) {
                    Ok(v) => app.ui.anim_tools = v,
                    Err(e) => return err(format!("animTools: {e}")),
                }
            }
            if let Some(b) = p.get("home").and_then(Value::as_bool) {
                app.ui.start_screen = b;
            }
            // Home screen tab: "home", "templates" or "learn".
            if let Some(tab) = p.get("homeTab").and_then(Value::as_str) {
                app.ui.home_learn = tab == "learn";
                app.ui.home_templates = tab == "templates";
            }
            ok(Value::Null)
        }
        "ui.panel.show" | "ui.panel.close" => {
            let Some(panel) = s("panel").and_then(PanelKind::from_name) else { return err("unknown `panel`") };
            if req.method == "ui.panel.show" {
                app.show_panel(panel);
            } else {
                app.close_panel(panel);
            }
            ok(Value::Null)
        }
        "ui.click" | "ui.move" => {
            let pos = match point(app, p) {
                Ok(p) => p,
                Err(e) if e.starts_with(RETRY) => return Outcome::Retry(e.trim_start_matches(RETRY).to_string()),
                Err(e) => return err(e),
            };
            let m = modifiers(p);
            let button = match s("button") {
                Some("right") => egui::PointerButton::Secondary,
                Some("middle") => egui::PointerButton::Middle,
                _ => egui::PointerButton::Primary,
            };
            // egui keeps the modifiers from `ModifiersChanged` (not the ones on a pointer event)
            // and the synthetic events arrive one per frame: hold them for the whole gesture.
            let mut events = vec![egui::Event::ModifiersChanged(m), egui::Event::PointerMoved(pos)];
            if req.method == "ui.click" {
                let count = p.get("count").and_then(Value::as_u64).unwrap_or(1);
                for _ in 0..count {
                    events.push(egui::Event::PointerButton { pos, button, pressed: true, modifiers: m });
                    events.push(egui::Event::PointerButton { pos, button, pressed: false, modifiers: m });
                }
            } else {
                // A drag in steps: `down` presses and holds the button here, later moves drag,
                // `up` releases (look at the UI mid-drag in between).
                for (key, pressed) in [("down", true), ("up", false)] {
                    if p.get(key).and_then(Value::as_bool) == Some(true) {
                        events.push(egui::Event::PointerButton { pos, button, pressed, modifiers: m });
                    }
                }
            }
            events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
            crate::float_windows::send_pointer(app, ctx, point_window(app, p), events);
            Outcome::AfterInput
        }
        "ui.drag" => {
            let (Some(from), Some(to)) = (p.get("from"), p.get("to")) else { return err("need `from` and `to`") };
            let (a, b) = match (point(app, from), point(app, to)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) if e.starts_with(RETRY) => return Outcome::Retry(e.trim_start_matches(RETRY).to_string()),
                (Err(e), _) | (_, Err(e)) => return err(e),
            };
            let steps = p.get("steps").and_then(Value::as_u64).unwrap_or(12).max(2);
            let m = modifiers(p);
            let mut events = vec![
                egui::Event::ModifiersChanged(m),
                egui::Event::PointerMoved(a),
                egui::Event::PointerButton { pos: a, button: egui::PointerButton::Primary, pressed: true, modifiers: m },
            ];
            for i in 1..=steps {
                let f = i as f32 / steps as f32;
                events.push(egui::Event::PointerMoved(a + (b - a) * f));
            }
            events.push(egui::Event::PointerButton { pos: b, button: egui::PointerButton::Primary, pressed: false, modifiers: m });
            events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
            // (In the window the drag starts in, which has the pointer until it is released.)
            crate::float_windows::send_pointer(app, ctx, point_window(app, from), events);
            Outcome::AfterInput
        }
        "ui.scroll" => {
            let pos = match point(app, p) {
                Ok(p) => p,
                Err(e) => return err(e),
            };
            let dx = p.get("dx").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let dy = p.get("dy").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            // `unit: "line"` is a mouse wheel (it zooms the viewers); points are a trackpad
            // scroll (the viewers pan once it moves on both axes) or a pixel-precise wheel. `pinch` is a trackpad pinch's zoom factor.
            let unit = if p.get("unit").and_then(Value::as_str) == Some("line") { egui::MouseWheelUnit::Line } else { egui::MouseWheelUnit::Point };
            let mut events = vec![egui::Event::PointerMoved(pos)];
            if dx != 0.0 || dy != 0.0 {
                events.push(egui::Event::MouseWheel { unit, delta: egui::vec2(dx, dy), modifiers: modifiers(p), phase: egui::TouchPhase::Move });
            }
            if let Some(pinch) = p.get("pinch").and_then(Value::as_f64).map(|z| z as f32).filter(|z| z.is_finite() && *z > 0.0) {
                events.push(egui::Event::Zoom(pinch));
            }
            events.push(egui::Event::PointerButton {
                pos: egui::pos2(-100.0, -100.0),
                button: egui::PointerButton::Extra1,
                pressed: false,
                modifiers: Default::default(),
            });
            crate::float_windows::send_pointer(app, ctx, point_window(app, p), events);
            Outcome::AfterInput
        }
        "ui.key" => {
            let Some(name) = s("key") else { return err("missing `key`") };
            let Some((mut m, key)) = crate::menus::parse_shortcut(name) else { return err(format!("unknown key `{name}`")) };
            let extra = modifiers(p);
            m |= extra;
            // As a US keyboard sends it: the key is also the physical key, so by-position
            // shortcuts (`Physical+=`, Zoom In Time) answer `=` too.
            let physical_key = Some(key);
            // `down: true` presses and holds the key, `up: true` releases it (a hold in steps: the
            // Graph / Layer wheels on G / A, Claude's version on `\`); neither = a tap.
            let (down, up) = (p.get("down").and_then(Value::as_bool) == Some(true), p.get("up").and_then(Value::as_bool) == Some(true));
            let mut events = vec![];
            if down || !up {
                events.push(egui::Event::Key { key, physical_key, pressed: true, repeat: false, modifiers: m });
            }
            if up || !down {
                events.push(egui::Event::Key { key, physical_key, pressed: false, repeat: false, modifiers: m });
            }
            // (To the window the last pointer input went to: an undocked one, or the main window.)
            crate::float_windows::send_keys(app, ctx, events);
            Outcome::AfterInput
        }
        "ui.type" => {
            let Some(text) = s("text") else { return err("missing `text`") };
            let events = vec![
                egui::Event::Text(text.to_string()),
                egui::Event::Key { key: egui::Key::F35, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() },
            ];
            crate::float_windows::send_keys(app, ctx, events);
            Outcome::AfterInput
        }
        "ui.viewer.locate" => {
            let (Some(x), Some(y)) = (p.get("x").and_then(Value::as_f64), p.get("y").and_then(Value::as_f64)) else {
                return err("need `x`, `y` (comp pixels)");
            };
            match crate::panels::viewer::comp_to_screen(ctx, [x as f32, y as f32]) {
                Some(pt) => ok(json!({"x": pt.x, "y": pt.y})),
                None => Outcome::Retry("the composition viewer is not visible".into()),
            }
        }
        "ui.viewer.hit" => {
            let pos = match point(app, p) {
                Ok(p) => p,
                Err(e) => return err(e),
            };
            match crate::panels::viewer::screen_to_comp(ctx, pos) {
                Some(c) => ok(json!({"x": c[0], "y": c[1]})),
                None => err("the composition viewer is not visible"),
            }
        }
        "ui.timeline.locate" => {
            let Some(l) = p.get("layer").and_then(Value::as_u64) else { return err("need `layer` (id)") };
            match crate::panels::timeline::locate(app, l, p.get("time").and_then(Value::as_f64)) {
                Some((x, y)) => {
                    // (A Timeline in an undocked OS window: click with this `window`.)
                    let mut r = json!({"x": x, "y": y});
                    if let Some(w) = app.auto.find_current("timeline.ruler").and_then(|e| e.window) {
                        r["window"] = json!(w);
                    }
                    ok(r)
                }
                None => Outcome::Retry("layer not visible in the timeline".into()),
            }
        }
        "ui.playback" => {
            let now = ctx.input(|i| i.time);
            match s("action").unwrap_or("toggle") {
                "play" => app.play(now),
                "stop" => app.stop(),
                "status" => {}
                _ => app.toggle_play(now),
            }
            ok(
                json!({"playing": app.playback.playing, "time": app.session.time().seconds(), "audio": app.audio.as_ref().is_some_and(|a| !a.held()), "levelsDb": app.meter.level_db, "peaksDb": app.meter.peak_db}),
            )
        }
        "ui.screenshot" => {
            let crop = s("panel").and_then(PanelKind::from_name).and_then(|pk| app.auto.find_current(&format!("panel.{}", pk.id())));
            let crop = crop.or_else(|| s("id").and_then(|id| app.auto.find_current(id)));
            // The capture is of the main window.
            if let Some(w) = crop.and_then(|e| e.window) {
                return err(format!(
                    "that is in an undocked panel window ({w}): ui.screenshot captures the main window only (dock the panel with window.dockPanel first)"
                ));
            }
            let crop = crop.map(|e| e.rect);
            // Bring the window on screen so a frame is presented, without taking keyboard focus.
            app.raise_for_control(ctx);
            let max_side = p.get("max_side").or(p.get("maxSide")).and_then(Value::as_u64).map_or(SCREENSHOT_MAX_SIDE, |m| u32::try_from(m).unwrap_or(u32::MAX));
            Outcome::Screenshot { path: s("path").map(str::to_string), crop, max_side }
        }
        "app.revision" => ok(json!({"revision": app.session.revision})),
        // A render box's pull: the diff is cheap here; its bytes (the whole project the first
        // time) are made on another thread so playback keeps its frames.
        "app.sync" => match effectcraft_engine::remote_follow::pull(&mut app.session, p) {
            Ok(pull) if pull.heavy() && !cfg!(target_arch = "wasm32") => {
                let reply = req.reply.clone();
                let spawned = std::thread::Builder::new().name("ec-app-sync".into()).spawn(move || {
                    let _ = reply.send(json!({"ok": true, "result": pull.into_reply()}));
                });
                match spawned {
                    Ok(_) => Outcome::Held,
                    Err(e) => err(format!("app.sync: {e}")),
                }
            }
            Ok(pull) => ok(pull.into_reply()),
            Err(e) => err(e),
        },
        // Desktop: rendered off the UI thread like the engine command (the reply waits for it).
        "render.frame" if !cfg!(target_arch = "wasm32") => match execute_engine(app, "render.frame", p.clone()) {
            Ok(o) => o,
            Err(e) => err(e),
        },
        "render.frame" => render_frame(app, p),
        // The FigCraft link (crate::figcraft_link): frames, audio, the precomp tree, reveal.
        "figcraft.frames" | "figcraft.audio" | "figcraft.tree" | "figcraft.reveal" => crate::figcraft_link::handle(app, ctx, req),
        "ui.resize" => {
            let w = p.get("width").and_then(Value::as_f64).unwrap_or(1600.0) as f32;
            let h = p.get("height").and_then(Value::as_f64).unwrap_or(1000.0) as f32;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
            ok(Value::Null)
        }
        "ui.focus" => {
            // Explicit request: activate the app and take keyboard focus.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ok(Value::Null)
        }
        "ui.figmaPair" => {
            // The Figma bridge asks once whether the plug-in may send layers; the reply goes out
            // when the person clicks Allow or Deny (`{allowed}`).
            let client = s("client").unwrap_or("Figma plug-in").to_string();
            if crate::panels::figma_pair::ask(app, ctx, &client, req.reply.clone()) {
                Outcome::Held
            } else {
                err("EffectCraft is already asking to connect Figma")
            }
        }
        "app.quit" => {
            // A modified project asks to save first unless `force: true`.
            if p.get("force").and_then(Value::as_bool).unwrap_or(false) {
                app.dialog_state.unsaved.quitting = true;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ok(Value::Null)
        }
        m => err(format!("unknown method `{m}`")),
    }
}

/// `render.frame {comp?, item?, layer?, time?, frame?, max_side?, transparent?, region?, overlays?,
/// path?, base64?}`: render through the session (independent of the viewer zoom/resolution) with
/// the engine's `render.frame` code, so the MCP bridge matches headless renders. The PNG is
/// written to `path` (default a temp file) or returned as base64 with `base64: true` (the web
/// has no file system: inline unless a path is given).
fn render_frame(app: &EffectcraftApp, p: &Value) -> Outcome {
    match effectcraft_engine::render_view::render_frame_reply(&app.session, p, cfg!(target_arch = "wasm32")) {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

/// Standard base64 (RFC 4648, padded).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn inspect(app: &EffectcraftApp, ctx: &egui::Context) -> Value {
    let size = ctx.content_rect().size();
    let now = ctx.input(|i| i.time);
    let frame = app.session.active_comp_id().and_then(|id| app.session.project.comp_arc(id)).map(|c| c.frame_rate.frame_at(app.session.time()));
    // Existing playback bookkeeping can be stale during a stall and does not measure scanout.
    // Expose observation age and timeline position so control clients can detect advancement.
    let last_shown = app.playback.shown_at.back().copied();
    let keyboard_focus = ctx.memory(|m| m.focused()).map(|id| {
        let response = ctx.read_response(id);
        let rect = response.as_ref().map(|r| [r.rect.min.x, r.rect.min.y, r.rect.width(), r.rect.height()]);
        // The registry names screen elements independently of egui's internal ids. Report
        // exact rectangle matches rather than guessing which enclosing panel owns focus.
        let candidates: Vec<&str> = app
            .auto
            .elements
            .iter()
            .filter(|e| rect.is_some_and(|r| e.rect.iter().zip(r).all(|(a, b)| (*a - b).abs() < 0.01)))
            .map(|e| e.id.as_str())
            .collect();
        let automation_id = app.auto.elements.iter().chain(app.auto.previous.iter()).find(|e| e.response_id == Some(id)).map(|e| e.id.as_str());
        json!({"id": format!("{:016x}", id.value()), "rect": rect, "automationId": automation_id, "registryCandidates": candidates})
    });
    let mut ui = json!(app.ui);
    // The viewer's Resolution is the active comp's (kept in the project).
    if let Some(v) = ui.get_mut("viewer").and_then(Value::as_object_mut) {
        v.insert("res".into(), json!(app.viewer_res()));
    }
    // CR5: where the viewer's preview frames come from.
    #[cfg(not(target_arch = "wasm32"))]
    let (preview_source, cloud) = {
        let source = if app.frames.remote_active() { "cloud" } else { "mac" };
        (source, app.cloud.status())
    };
    #[cfg(target_arch = "wasm32")]
    let (preview_source, cloud): (&str, Option<()>) = ("browser", None);
    #[cfg(not(target_arch = "wasm32"))]
    let (rtt_ms, cloud_queue) = (cloud.as_ref().and_then(|c| c.rtt_ms), cloud.as_ref().map_or(0, |c| c.queue));
    #[cfg(target_arch = "wasm32")]
    let (rtt_ms, cloud_queue): (Option<f64>, usize) = (None, 0);
    // CR6b: the cloud playback stream (warm-up, late chunks, stalls, kbit/s), the running one's or
    // the last one's.
    #[cfg(not(target_arch = "wasm32"))]
    let stream = app.cloud_play.stats();
    #[cfg(target_arch = "wasm32")]
    let stream: Option<()> = None;
    json!({
        "window": [size.x, size.y],
        "pixelsPerPoint": ctx.pixels_per_point(),
        "fps": app.fps,
        "ui": ui,
        "previewOn": app.session.prefs.previews.preview_on,
        "previewSource": preview_source,
        "rttMs": rtt_ms,
        "cloudQueue": cloud_queue,
        "cloud": cloud,
        "playing": app.playback.playing,
        "playback": {"achievedFps":app.playback.achieved_fps(),"realTime":app.playback.real_time(),
            "shown":app.playback.shown,"caching":app.playback.caching,"waiting":app.playback.waiting,"targetFps":app.playback.fps},
        "preview": {
            "time": app.session.time().seconds(),
            "frame": frame,
            "playing": app.playback.playing,
            "shown": app.playback.shown,
            "achievedFpsDiagnostic": app.playback.achieved_fps(),
            "realTimeDiagnostic": app.playback.real_time(),
            "caching": app.playback.caching,
            "waiting": app.playback.waiting,
            "targetFps": app.playback.fps,
            "resolutionStep": app.playback.res,
            "frameStep": app.playback.plan.map_or(1, |p| p.step),
            "observationTimeSeconds": now,
            "lastShownTimeSeconds": last_shown,
            "lastShownAgeSeconds": last_shown.map(|t| (now - t).max(0.0)),
            "intervals": shown_intervals(&app.playback.log),
            "stream": stream,
        },
        // Edit → viewer frame: `shownAt - editAt` once `shownRevision >= editRevision`.
        "latency": {
            "editRevision": app.latency.edit_revision,
            "editAt": app.latency.edit_at,
            "shownRevision": app.latency.shown.map(|s| s.0),
            "shownFrame": app.latency.shown.map(|s| s.1),
            "shownScale": app.latency.shown.map(|s| s.2),
            "shownAt": app.latency.shown_at,
        },
        "time": app.session.time().seconds(),
        "activeComp": app.session.state.active_comp.map(|i| i.0),
        "selectedLayers": app.session.state.selected_layers.iter().map(|c| c.0).collect::<Vec<_>>(),
        "selectedKeys": app.session.state.selected_keys.len(),
        "elements": app.auto.elements.len(),
        "keyboardFocus": keyboard_focus,
        "windowFocused": ctx.input(|i| i.focused),
        "workspaceStorageError": app.workspace_store.error,
        "dialog": app.dialog.map(|d| format!("{d:?}")),
        "renderMs": app.frames.last_ms.lock().map(|v| *v).unwrap_or(0.0),
        "renderQueue": app.frames.queue_state(),
        // RT10e: GPU viewer frames drawn from their textures, without a copy.
        "zeroCopy": {"active": app.zero_copy_active(), "frames": app.zero_copy_frames(), "viewerOnGpu": app.viewer_on_gpu(), "compositor": app.gpu_adapter()},
    })
}

/// Since the preview started: how many frames were shown and skipped, and the percentiles of
/// the time between frames shown (ms).
fn shown_intervals(log: &[(f64, i64)]) -> Value {
    let mut ms: Vec<f64> = log.windows(2).map(|w| (w[1].0 - w[0].0) * 1000.0).collect();
    let skipped: i64 = log.windows(2).map(|w| (w[1].1 - w[0].1 - 1).max(0)).sum();
    ms.sort_by(f64::total_cmp);
    let pct = |p: usize| ms.get((ms.len().saturating_mul(p).div_ceil(100)).saturating_sub(1)).copied();
    json!({"shown": log.len(), "skipped": skipped, "p50Ms": pct(50), "p95Ms": pct(95), "p99Ms": pct(99), "maxMs": ms.last().copied(),
        "seconds": log.first().zip(log.last()).map(|(a, b)| b.0 - a.0)})
}

/// Save a screenshot (optionally cropped to a rect in points) as PNG; returns the reply JSON.
pub fn save_screenshot(ctx: &egui::Context, image: &egui::ColorImage, path: Option<&str>, crop: Option<[f32; 4]>, max_side: u32) -> Value {
    let ppp = ctx.pixels_per_point();
    let [w, h] = image.size;
    let (x0, y0, cw, ch) = match crop {
        Some([x, y, cw, ch]) => {
            let x0 = ((x * ppp) as usize).min(w);
            let y0 = ((y * ppp) as usize).min(h);
            (x0, y0, ((cw * ppp) as usize).min(w - x0), ((ch * ppp) as usize).min(h - y0))
        }
        None => (0, 0, w, h),
    };
    let mut rgba = Vec::with_capacity(cw * ch * 4);
    for y in y0..y0 + ch {
        for x in x0..x0 + cw {
            let c = image.pixels[y * w + x];
            rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
        }
    }
    let (rgba, cw, ch) = shrink(rgba, cw as u32, ch as u32, max_side);
    if path.is_none() && cfg!(target_arch = "wasm32") {
        // no file system on the web: inline PNG
        return match encode_png_fast(&rgba, cw, ch) {
            Ok(png) => json!({"ok": true, "result": {"png": base64(&png), "width": cw, "height": ch}}),
            Err(e) => json!({"ok": false, "error": e}),
        };
    }
    let path = path.map(str::to_string).unwrap_or_else(|| std::env::temp_dir().join("effectcraft-screenshot.png").to_string_lossy().to_string());
    match encode_png_fast(&rgba, cw, ch) {
        Ok(png) => match std::fs::write(&path, png) {
            Ok(()) => json!({"ok": true, "result": {"path": path, "width": cw, "height": ch}}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// Downscale RGBA8 pixels so the longest side is at most `max_side` (0 = keep), as the MCP
/// bridge's `screenshot` tool does. Returns the pixels and their size.
pub fn shrink(rgba: Vec<u8>, w: u32, h: u32, max_side: u32) -> (Vec<u8>, u32, u32) {
    if max_side == 0 || w.max(h) <= max_side {
        return (rgba, w, h);
    }
    let Some(img) = image::RgbaImage::from_raw(w, h, rgba.clone()) else { return (rgba, w, h) };
    let s = f64::from(max_side) / f64::from(w.max(h));
    let (nw, nh) = (((f64::from(w) * s).round() as u32).max(1), ((f64::from(h) * s).round() as u32).max(1));
    let img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    (img.into_raw(), nw, nh)
}

/// Compressed PNG (fast deflate, opaque RGB): screenshots are a few hundred KB instead of the
/// stored encoder's raw size.
pub fn encode_png_fast(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("png: {e}"))?;
    Ok(out)
}

/// Minimal PNG encoder (stored deflate blocks, no compression dependency).
pub fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    fn chunk(out: &mut Vec<u8>, ty: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = Vec::with_capacity(4 + data.len());
        c.extend_from_slice(ty);
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc(&c).to_be_bytes());
    }
    fn crc(d: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in d {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    // zlib stream with stored blocks
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// Playback's frame log: frames skipped between shown ones, interval percentiles in ms.
    #[test]
    fn shown_intervals_count_skips_and_percentiles() {
        let log: Vec<(f64, i64)> = vec![(0.0, 10), (0.033, 11), (0.066, 12), (0.166, 15), (0.2, 16)];
        let v = super::shown_intervals(&log);
        assert_eq!((v["shown"].as_u64(), v["skipped"].as_i64()), (Some(5), Some(2)));
        let ms = |k: &str| v[k].as_f64().map(|x| x.round());
        assert_eq!((ms("p50Ms"), ms("p99Ms"), ms("maxMs")), (Some(33.0), Some(100.0), Some(100.0)));
        assert!(super::shown_intervals(&[])["p99Ms"].is_null());
    }

    #[test]
    fn between_frame_controls_use_current_geometry_and_do_not_resurrect_removed_widgets() {
        use super::*;
        use egui::{Rect, pos2, vec2};
        let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let old = Rect::from_min_size(pos2(10.0, 10.0), vec2(10.0, 10.0));
        let current = Rect::from_min_size(pos2(40.0, 60.0), vec2(10.0, 12.0));
        app.auto.window = Some(7);
        app.auto.add("handle", old, "old");
        app.auto.add("removed", old, "gone");
        app.auto.begin_frame();
        app.auto.window = None;
        app.auto.add("handle", current, "current");
        app.auto.add("timeline.ruler", Rect::from_min_size(pos2(100.0, 100.0), vec2(100.0, 20.0)), "2,20");
        app.auto.add("timeline.layer.1.bar", Rect::from_min_size(pos2(100.0, 200.0), vec2(50.0, 20.0)), "layer");
        assert_eq!(app.auto.find("handle").unwrap().label, "old", "during-paint lookup remains unchanged");
        let request = |method, params| ControlRequest::new(method, params).0;
        assert!(matches!(handle(&mut app, &ctx, &request("ui.click", json!({"id": "handle"}))), Outcome::AfterInput));
        let events = app.take_synthetic_input();
        // Modifiers held, move, press, release, modifiers released.
        assert_eq!(events.len(), 5, "new main-window target must not route to the old floating window");
        assert!(matches!(events.get(1), Some(egui::Event::PointerMoved(p)) if *p == current.center()));
        let Outcome::Done(list) = handle(&mut app, &ctx, &request("ui.elements", json!({"prefix": "handle"}))) else { panic!("expected elements") };
        assert_eq!(list["result"][0]["label"], "current");
        assert_eq!(list["result"][0]["rect"], json!([40.0, 60.0, 10.0, 12.0]));
        assert_eq!(inspect(&app, &ctx)["elements"], 3);
        let Outcome::Done(located) = handle(&mut app, &ctx, &request("ui.timeline.locate", json!({"layer": 1, "time": 3}))) else {
            panic!("expected location")
        };
        assert_eq!(located["result"], json!({"x": 126.0, "y": 210.0}));
        let Outcome::Screenshot { crop, .. } = handle(&mut app, &ctx, &request("ui.screenshot", json!({"id": "handle"}))) else {
            panic!("expected screenshot crop")
        };
        assert_eq!(crop, Some([40.0, 60.0, 10.0, 12.0]));
        assert!(matches!(handle(&mut app, &ctx, &request("ui.click", json!({"id": "removed"}))), Outcome::Retry(_)));
        assert!(app.auto.find_current("removed").is_none());
        assert!(app.auto.query_current("removed").is_empty());
    }

    #[test]
    fn base64_rfc4648_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn ui_key_holds_with_down_then_up() {
        // BUILD2: the G / A wheels and `\` (Claude's version) open on a hold; agents test them
        // with `down` and a later `up`.
        use super::*;
        let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let request = |method, params| ControlRequest::new(method, params).0;
        let keys = |app: &mut EffectcraftApp| -> Vec<bool> {
            app.take_synthetic_input().into_iter().filter_map(|e| if let egui::Event::Key { pressed, .. } = e { Some(pressed) } else { None }).collect()
        };
        assert!(matches!(handle(&mut app, &ctx, &request("ui.key", json!({"key": "G"}))), Outcome::AfterInput));
        assert_eq!(keys(&mut app), [true, false], "a tap");
        assert!(matches!(handle(&mut app, &ctx, &request("ui.key", json!({"key": "G", "down": true}))), Outcome::AfterInput));
        assert_eq!(keys(&mut app), [true], "held down");
        assert!(matches!(handle(&mut app, &ctx, &request("ui.key", json!({"key": "G", "up": true}))), Outcome::AfterInput));
        assert_eq!(keys(&mut app), [false], "released");
    }
}
