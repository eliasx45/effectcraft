//! The full app offscreen: [`EffectcraftApp`] in an egui_kittest harness rendered with wgpu, on a
//! thread of its own, driven by control-channel requests ([`ControlRequest`],
//! `docs/control-protocol.md`). No window, no focus stealing, no display needed, and several can
//! run side by side. `effectcraft-cli mcp --ui` serves MCP through it.
//!
//! The UI thread is the event loop. While a request is pending it runs frames back to back until
//! the app replies, then lets the UI settle (no immediate repaint pending) so the next
//! `ui.elements` or screenshot sees the result. When idle it steps only when the app asked for a
//! repaint (background frame renders, playback). Synthetic input from `ui.click`, `ui.drag`,
//! `ui.key` and `ui.type` goes into the harness as real input events, one frame per event, as the
//! desktop app's `raw_input_hook` feeds them. `ui.screenshot` renders the harness's frame
//! (egui_kittest answers the app's `ViewportCommand::Screenshot`). Frame time follows the wall
//! clock, so the app's timeouts and screenshot settling behave as in the desktop app.
//!
//! [`OffscreenUi::on_session`] runs code on the app's [`Session`] on the UI thread, between
//! frames and in order with requests (`mcp --ui --autosave` checkpoints the project that way).
//!
//! wgpu renders offscreen from any thread (no surface, no main-thread requirement on macOS).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use effectcraft_engine::Session;
use effectcraft_engine::prefs::Prefs;
use eframe::egui_wgpu;
use egui_kittest::Harness;
use egui_kittest::wgpu::WgpuTestRenderer;
use serde_json::{Value, json};

use crate::{ControlRequest, EffectcraftApp};

/// Size and scale of the offscreen window.
#[derive(Clone, Copy, Debug)]
pub struct OffscreenOptions {
    /// Window size in points (what `ui.click {x, y}` and `ui.elements` rects use).
    pub size: [f32; 2],
    /// Pixels per point of screenshots.
    pub pixels_per_point: f32,
}

impl Default for OffscreenOptions {
    fn default() -> Self {
        Self { size: [1680.0, 1020.0], pixels_per_point: 2.0 }
    }
}

/// Longest a request may take (as the desktop app's control server).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// After a reply: at most this long (and [`SETTLE_FRAMES`]) for the UI to stop asking for frames.
const SETTLE_TIME: Duration = Duration::from_millis(1500);
const SETTLE_FRAMES: usize = 60;
/// Idle frame pacing: never faster than this, and look for due repaints at least this often.
const MIN_FRAME: Duration = Duration::from_millis(16);
const IDLE_POLL: Duration = Duration::from_millis(250);

const STOPPED: &str = "the offscreen UI stopped (restart the MCP server)";

/// Code run on the app's session on the UI thread ([`OffscreenUi::on_session`]).
pub type SessionJob = Box<dyn FnOnce(&mut Session) + Send>;

/// What the UI thread is asked to do.
enum Job {
    Control(ControlRequest),
    Session(SessionJob),
}

/// Whether the app's first frames are done (they compile the GPU compositor's shaders, which can
/// take minutes on a cold shader cache under load).
enum Startup {
    Pending(Receiver<Result<(), String>>),
    Ready,
    Failed(String),
}

/// A handle to an offscreen app. Dropping it ends the UI thread.
pub struct OffscreenUi {
    jobs: Sender<Job>,
    adapter: String,
    prefs: Prefs,
    startup: Mutex<Startup>,
}

impl OffscreenUi {
    /// Start the app offscreen with the session `make_session` builds (on the UI thread). Returns
    /// once the session is built and a GPU adapter is found (failing when there's none); the app's
    /// first frames run in the background and the first [`OffscreenUi::call`] waits for them.
    pub fn start(opts: OffscreenOptions, make_session: impl FnOnce() -> Result<Session, String> + Send + 'static) -> Result<Self, String> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(String, Prefs), String>>();
        let (started_tx, started_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("offscreen-ui".into())
            // The desktop app runs on the main thread's large stack; match it.
            .stack_size(64 << 20)
            .spawn(move || run(opts, make_session, inbox, ready_tx, started_tx))
            .map_err(|e| format!("cannot start the offscreen UI thread: {e}"))?;
        let (adapter, prefs) = ready_rx.recv().map_err(|_| "the offscreen UI stopped while starting".to_string())??;
        Ok(Self { jobs, adapter, prefs, startup: Mutex::new(Startup::Pending(started_rx)) })
    }

    /// The session's preferences as `make_session` built them (before the app started).
    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// Queue `job` to run on the app's session on the UI thread, after the app's first frames and
    /// before any later request (it doesn't wait). A panic in it is caught and dropped (the job's
    /// own reply channel, if any, disconnects). `Err` when the UI thread is gone.
    pub fn on_session(&self, job: SessionJob) -> Result<(), String> {
        self.jobs.send(Job::Session(job)).map_err(|_| STOPPED.to_string())
    }

    /// The wgpu adapter the UI renders with (`name (backend)`).
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Wait for the app's first frames; `Err` if it failed to start.
    pub fn wait_started(&self) -> Result<(), String> {
        let mut st = self.startup.lock().unwrap_or_else(PoisonError::into_inner);
        if let Startup::Pending(rx) = &*st {
            *st = match rx.recv() {
                Ok(Ok(())) => Startup::Ready,
                Ok(Err(e)) => Startup::Failed(e),
                Err(_) => Startup::Failed(STOPPED.to_string()),
            };
        }
        match &*st {
            Startup::Failed(e) => Err(e.clone()),
            _ => Ok(()),
        }
    }

    /// Send a control request and wait for the app's reply (`{"ok": true, "result": …}` or
    /// `{"ok": false, "error": …}`); `Err` when the UI thread is gone.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.wait_started()?;
        let (req, rx) = ControlRequest::new(method, params);
        self.jobs.send(Job::Control(req)).map_err(|_| STOPPED.to_string())?;
        // The UI thread always replies within REQUEST_TIMEOUT plus settling; the margin covers a
        // GPU readback stuck in the driver.
        rx.recv_timeout(REQUEST_TIMEOUT + Duration::from_secs(30)).map_err(|e| match e {
            RecvTimeoutError::Timeout => format!("no reply to `{method}` from the offscreen UI"),
            RecvTimeoutError::Disconnected => STOPPED.to_string(),
        })
    }
}

/// The harness and its control channel, on the UI thread.
struct Ui {
    h: Harness<'static, EffectcraftApp>,
    app: Sender<ControlRequest>,
    clock: Instant,
    last_step: Instant,
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// A wgpu renderer on the default headless setup, failing (instead of panicking, as
/// `HarnessBuilder::wgpu` does) when there's no adapter.
fn renderer() -> Result<(WgpuTestRenderer, String), String> {
    let setup = egui_kittest::wgpu::default_wgpu_setup();
    let instance = pollster::block_on(setup.new_instance());
    let config = egui_wgpu::WgpuConfiguration { wgpu_setup: setup, ..Default::default() };
    let rs = pollster::block_on(egui_wgpu::RenderState::create(&config, &instance, None, egui_wgpu::RendererOptions::PREDICTABLE))
        .map_err(|e| format!("no usable GPU adapter for the offscreen UI ({e})"))?;
    let info = rs.adapter.get_info();
    Ok((WgpuTestRenderer::from_render_state(rs), format!("{} ({:?})", info.name, info.backend)))
}

/// The fast, fallible part of starting: options, the session and a GPU adapter.
fn prepare(opts: OffscreenOptions, make_session: impl FnOnce() -> Result<Session, String>) -> Result<(Session, WgpuTestRenderer, String), String> {
    let [w, h] = opts.size;
    if !(w.is_finite() && h.is_finite() && w >= 200.0 && h >= 150.0 && w <= 16384.0 && h <= 16384.0) {
        return Err(format!("offscreen UI size {w}x{h} is out of range (200x150 to 16384x16384)"));
    }
    let session = make_session()?;
    let (renderer, adapter) = renderer()?;
    Ok((session, renderer, adapter))
}

/// The app in a harness; runs its first frames.
fn build(opts: OffscreenOptions, session: Session, renderer: WgpuTestRenderer) -> Ui {
    let (app, rx) = mpsc::channel::<ControlRequest>();
    let ppp = if opts.pixels_per_point.is_finite() { opts.pixels_per_point.clamp(0.5, 4.0) } else { 2.0 };
    let h = Harness::builder()
        .with_size(egui::vec2(opts.size[0], opts.size[1]))
        // Ui::step supplies wall time. Extra frames for queued input must not advance a
        // synthetic clock, especially while a background job makes us poll rapidly.
        .with_step_dt(0.0)
        .with_pixels_per_point(ppp)
        .renderer(renderer)
        .build_eframe(move |cc| {
            let mut app = EffectcraftApp::new(session).with_control(rx);
            // MEM3: no memory polling in this hidden copy; its caches are budgeted once from the
            // machine's memory (two kernel calls), so they never take what the desktop app needs.
            app.watch_system = false;
            app.session.sys_memory = effectcraft_engine::sysinfo::memory();
            app.session.apply_cache_budgets();
            // CR1: a compositor that fails (out of video memory) is retired and set up again on
            // a new device, as in the desktop app. Its own health check reports it; the
            // harness's device has no handlers to forward.
            app.set_gpu_failure_bridge(crate::gpu_failure::GpuFailureBridge::new(&cc.egui_ctx));
            // RT10e: `EFFECTCRAFT_ZERO_COPY=1` draws GPU frames from their textures here too
            // (no window surface, so no traffic light).
            if crate::zero_copy::env_choice() == Some(true) {
                app.share_display_device(None);
            }
            app
        });
    let now = Instant::now();
    let mut ui = Ui { h, app, clock: now, last_step: now };
    ui.settle();
    ui
}

fn run(
    opts: OffscreenOptions,
    make_session: impl FnOnce() -> Result<Session, String>,
    inbox: Receiver<Job>,
    ready: Sender<Result<(String, Prefs), String>>,
    started: Sender<Result<(), String>>,
) {
    // Last-resort guard: the harness panics on failures it doesn't expect (AGENTS.md).
    let (session, renderer) = match catch_unwind(AssertUnwindSafe(|| prepare(opts, make_session))) {
        Ok(Ok((session, renderer, adapter))) => {
            let _ = ready.send(Ok((adapter, session.prefs.clone())));
            (session, renderer)
        }
        Ok(Err(e)) => {
            let _ = ready.send(Err(e));
            return;
        }
        Err(p) => {
            let _ = ready.send(Err(format!("the offscreen UI failed to start: {}", panic_message(&*p))));
            return;
        }
    };
    let mut ui = match catch_unwind(AssertUnwindSafe(|| build(opts, session, renderer))) {
        Ok(ui) => {
            let _ = started.send(Ok(()));
            ui
        }
        Err(p) => {
            let _ = started.send(Err(format!("the offscreen UI failed to start: {}", panic_message(&*p))));
            return;
        }
    };
    loop {
        match inbox.recv_timeout(ui.idle_wait()) {
            Ok(Job::Control(req)) => ui.serve(req),
            Ok(Job::Session(job)) => {
                let _ = catch_unwind(AssertUnwindSafe(|| job(&mut ui.h.state_mut().session)));
            }
            Err(RecvTimeoutError::Timeout) => {
                if ui.repaint_due() {
                    let _ = catch_unwind(AssertUnwindSafe(|| ui.step()));
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = catch_unwind(AssertUnwindSafe(|| eframe::App::on_exit(ui.h.state_mut())));
}

impl Ui {
    /// One frame (one per queued input event), with the app's synthetic input fed in as real
    /// input and the frame time on the wall clock.
    fn step(&mut self) {
        let now = self.clock.elapsed().as_secs_f64();
        let last = self.h.ctx.input(|i| i.time);
        self.h.input_mut().time = Some(now.max(last));
        for e in self.h.state_mut().take_synthetic_input() {
            // As a windowing backend reports them: the modifiers a scripted click, drag or key
            // carries are held (`i.modifiers`), not only attached to that event.
            if let egui::Event::PointerButton { modifiers, .. } | egui::Event::Key { modifiers, .. } = &e {
                self.h.event(egui::Event::ModifiersChanged(*modifiers));
            }
            self.h.event(e);
        }
        self.h.step();
        self.last_step = Instant::now();
    }

    /// How soon the app asked for the next frame (`Duration::MAX`: not at all).
    fn repaint_delay(&self) -> Duration {
        self.h.output().viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay)
    }

    /// The app wants another frame right away (input still queued, a popup sizing, an animation).
    fn busy(&self) -> bool {
        self.repaint_delay().is_zero() || !self.h.state().synthetic.is_empty()
    }

    fn repaint_due(&self) -> bool {
        self.busy() || self.last_step.elapsed() >= self.repaint_delay()
    }

    fn idle_wait(&self) -> Duration {
        let due = self.repaint_delay().max(MIN_FRAME).saturating_sub(self.last_step.elapsed());
        due.clamp(Duration::from_millis(1), IDLE_POLL)
    }

    /// Step until the UI stops asking for immediate frames (at least two frames, so state changed
    /// by the last request is laid out and its elements are registered).
    fn settle(&mut self) {
        let end = Instant::now() + SETTLE_TIME;
        for i in 0..SETTLE_FRAMES {
            self.step();
            if (i >= 1 && !self.busy()) || Instant::now() > end {
                break;
            }
        }
    }

    fn serve(&mut self, req: ControlRequest) {
        let ControlRequest { method, params, reply, .. } = req;
        let v = catch_unwind(AssertUnwindSafe(|| self.request(&method, params)))
            .unwrap_or_else(|p| json!({"ok": false, "error": format!("internal error in the offscreen UI: {}", panic_message(&*p))}));
        let _ = reply.send(v);
    }

    /// Hand a request to the app and run frames until it replies.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let (req, rx) = ControlRequest::new(method, params);
        if self.app.send(req).is_err() {
            return json!({"ok": false, "error": STOPPED});
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let reply = loop {
            self.step();
            match rx.try_recv() {
                Ok(v) => break v,
                Err(TryRecvError::Disconnected) => break json!({"ok": false, "error": format!("`{method}` got no reply from the app")}),
                Err(TryRecvError::Empty) => {}
            }
            if Instant::now() > deadline {
                break json!({"ok": false, "error": format!("no reply to `{method}` within {} s", REQUEST_TIMEOUT.as_secs())});
            }
            if !self.busy() {
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        self.settle();
        reply
    }
}
