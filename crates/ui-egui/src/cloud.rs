//! CR5 (`plan/cloud-render.md`): the viewer's preview frames from a cloud frame server.
//!
//! Elias edits in the native app (timeline, panels, typing and audio stay local and instant);
//! the Composition viewer's frames can come from `effectcraft-cli serve --gpu --frames PORT` on a
//! rented GPU, reached through an SSH tunnel (`127.0.0.1:9878` here).
//!
//! - [`CloudFrames`] is a [`RemoteFrames`]: frames queued by [`crate::frames::Frames`] go to the
//!   server over the binary frame channel ([`FrameClient`], CR2), after a comp-level project
//!   patch (CR3, [`Mirror`]). Replies are JPEG while scrubbing, refined to lossless when the
//!   server is idle; they decode on the frame pool and land in the same RAM cache as local
//!   frames, so playback and the cache bar need nothing new.
//! - Frames in flight follow Little's law: enough requests to cover a round trip at the comp's
//!   frame rate (`fps × (RTT + render)`), not the CPU count.
//! - The connection is checked with an echo every second (the round-trip time shown in the
//!   viewer's bar). A lost connection, a refused token or a silent link fails the frames in
//!   flight, [`RemoteFrames::slots`] drops to 0 and the frames render on this Mac again: never
//!   an error dialog, never a change to the document. The client reconnects on its own.
//! - [`CloudPreview`] is the app's side: **Preview on: This Mac / Cloud / Auto** (Settings ▸
//!   Previews, the viewer's bar), the status note when the source changes, and Auto's pick
//!   (the cloud when this Mac's frame cost is above the cloud's round trip).
//! - Footage paths: the server mirrors media to the same absolute paths (CR7), so frames go with
//!   the paths as they are; [`PathRemap`] rewrites them for a server that keeps them elsewhere.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use effectcraft_engine::project::{ItemKind, Project};
use effectcraft_engine::remote::{FrameMsg, FrameReply, Mirror};
use effectcraft_engine::remote_encode::{self, Codec, Want};
use effectcraft_engine::remote_wire::{Backoff, Down, Event, FrameClient, Up};
use effectcraft_engine::render::Backend;
use serde::Serialize;
use serde_json::{Value, json};

use crate::frames::times::FrameTimes;
use crate::frames::{RemoteDone, RemoteFrame, RemoteFrames, RemoteJob, RemoteRefine, RemoteTimes};

/// Rewrites a footage path for the server (`None`: keep it).
pub type PathRemap = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The config entry holding the server's token (copied from the server's `frame-server-token`).
pub const TOKEN_ENTRY: &str = "cloud-frame-token";

/// How often the link is checked.
const PING: Duration = Duration::from_secs(1);
/// An echo (or a whole project) not answered in this long means a dead link.
const DEAD: Duration = Duration::from_secs(4);
/// A whole project may take this long to arrive and load before the link counts as dead.
const PROJECT_DEAD: Duration = Duration::from_secs(120);
/// How long before asking the server again whether it has the project's files.
const PREFLIGHT_RETRY: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(10) };
/// Refines kept waiting at most this long.
const REFINE_WAIT: Duration = Duration::from_secs(30);

/// What the client knows about its connection (`ui.inspect` `cloud`).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudStatus {
    pub address: String,
    pub connected: bool,
    /// The server holds this Mac's project: frames can go to it.
    pub ready: bool,
    /// Echo round trip (ms), smoothed.
    pub rtt_ms: Option<f64>,
    /// The server's render + encode time per frame (ms), smoothed.
    pub server_ms: Option<f64>,
    /// Request → decoded frame (ms), smoothed.
    pub frame_ms: Option<f64>,
    /// Frames asked for and not back yet.
    pub queue: usize,
    /// Frames it may have in flight now.
    pub slots: usize,
    pub gpu: Option<String>,
    pub server: Option<String>,
    /// Why the last connection ended or failed.
    pub error: Option<String>,
    /// What the cloud machine lacks (CR7 preflight's summary): frames render here until it has it.
    pub missing: Option<String>,
    /// Files the mirror hasn't uploaded yet (CR7 `cloud.pendingUploads`): frames render here
    /// meanwhile.
    pub pending_uploads: usize,
    /// The server refused the token (the client stopped).
    pub refused: bool,
    pub frames: u64,
    pub bytes: u64,
    pub reconnects: u64,
    /// Whole projects sent (the first, after a reconnect, after a resync).
    pub projects: u64,
}

struct Shared {
    status: Mutex<CloudStatus>,
    /// Frames it renders at once (0 while not ready).
    slots: AtomicUsize,
    /// The comp's frame rate (f32 bits), for the frames in flight.
    fps: AtomicU32,
    /// The newest project, sent whole after a (re)connection.
    latest: Mutex<Option<(u64, Arc<Project>)>>,
    /// Where the project is saved (its `.cloud.json` sidecar holds the Drive links, CR7).
    path: Mutex<Option<String>>,
    stop: AtomicBool,
}

enum Cmd {
    Start(Box<RemoteJob>, RemoteDone),
    Purge,
    /// This Mac's manifest for the server's preflight (CR7), for connection `generation`.
    Manifest(u64, Value),
    /// Files the project needs that the mirror hasn't confirmed uploaded, at `revision`.
    Pending {
        revision: u64,
        count: usize,
    },
}

/// Whether the server has every file and font the project needs (CR7 `cloud.preflight`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Preflight {
    /// Not asked yet on this connection.
    Unknown,
    /// The manifest is being made, or the request is on its way (`id`).
    Asking(Option<u64>),
    Ok,
    /// Something is missing there: ask again at `retry`.
    Missing {
        retry: Instant,
    },
}

/// Viewer frames from a cloud frame server (see the module docs).
pub struct CloudFrames {
    shared: Arc<Shared>,
    cmd: Mutex<Sender<Cmd>>,
}

impl CloudFrames {
    /// Connect to `addr` (`host:port`, the local end of the SSH tunnel) in the background.
    pub fn connect(addr: &str, token: &str, remap: Option<PathRemap>) -> Arc<CloudFrames> {
        let shared = Arc::new(Shared {
            status: Mutex::new(CloudStatus { address: addr.to_string(), ..CloudStatus::default() }),
            slots: AtomicUsize::new(0),
            fps: AtomicU32::new(30f32.to_bits()),
            latest: Mutex::new(None),
            path: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let client = FrameClient::connect(addr, token, "effectcraft-app", Backoff::default());
        let link = Link {
            me: tx.clone(),
            preflight: Preflight::Unknown,
            generation: 0,
            uploads: None,
            checking_uploads: false,
            uploads_checked: None,
            shared: shared.clone(),
            client,
            remap,
            mirror: Mirror::default(),
            pending: HashMap::new(),
            refining: HashMap::new(),
            next_id: 1,
            ping: None,
            last_ping: None,
            syncing: None,
        };
        let spawned = std::thread::Builder::new().name("ec-cloud-frames".into()).spawn(move || link.run(rx));
        if let Err(e) = spawned {
            shared.status.lock().unwrap_or_else(PoisonError::into_inner).error = Some(format!("cannot start the cloud link: {e}"));
        }
        Arc::new(CloudFrames { shared, cmd: Mutex::new(tx) })
    }

    /// The project as it is now (sent whole on the next connection; render requests carry
    /// their own revision).
    pub fn offer(&self, revision: u64, project: &Arc<Project>) {
        let mut l = self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner);
        if l.as_ref().is_none_or(|(r, _)| *r != revision) {
            *l = Some((revision, project.clone()));
        }
    }

    /// Where the project is saved (`None`: never saved).
    pub fn set_path(&self, path: Option<&str>) {
        let mut p = self.shared.path.lock().unwrap_or_else(PoisonError::into_inner);
        if p.as_deref() != path {
            *p = path.map(str::to_string);
        }
    }

    /// The previewed comp's frame rate (frames in flight follow it).
    pub fn set_fps(&self, fps: f64) {
        if fps.is_finite() && fps > 0.0 {
            self.shared.fps.store((fps as f32).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn status(&self) -> CloudStatus {
        self.shared.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Frames can go to the server now.
    pub fn ready(&self) -> bool {
        self.shared.slots.load(Ordering::Acquire) > 0
    }
}

impl Drop for CloudFrames {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

impl RemoteFrames for CloudFrames {
    fn slots(&self) -> usize {
        self.shared.slots.load(Ordering::Acquire)
    }

    fn start(&self, job: RemoteJob, done: RemoteDone) {
        let sent = self.cmd.lock().unwrap_or_else(PoisonError::into_inner).send(Cmd::Start(Box::new(job), done));
        if let Err(std::sync::mpsc::SendError(Cmd::Start(_, done))) = sent {
            // The link thread is gone: the frame renders here.
            done(Err("the cloud link stopped".into()));
        }
    }

    fn purge(&self) {
        let _ = self.cmd.lock().unwrap_or_else(PoisonError::into_inner).send(Cmd::Purge);
    }

    fn gpu(&self) -> bool {
        true
    }

    fn eager(&self) -> bool {
        true
    }
}

/// A render request on its way.
struct Pending {
    done: Option<RemoteDone>,
    refined: Option<RemoteRefine>,
    timed: Option<RemoteTimes>,
    cancel: Arc<AtomicBool>,
    sent: Instant,
}

/// The link thread's state.
struct Link {
    shared: Arc<Shared>,
    client: FrameClient,
    remap: Option<PathRemap>,
    mirror: Mirror,
    pending: HashMap<u64, Pending>,
    /// Refines still expected, by request id.
    refining: HashMap<u64, (RemoteRefine, Instant)>,
    next_id: u64,
    /// The echo in flight: (id, sent).
    ping: Option<(u64, Instant)>,
    last_ping: Option<Instant>,
    /// The echo that follows a whole project: (id, sent). The server holds the project once it
    /// comes back (the server handles a connection's messages in order).
    syncing: Option<(u64, Instant)>,
    /// The link's own command channel (background checks report back through it).
    me: Sender<Cmd>,
    preflight: Preflight,
    /// Bumps on every connection: a manifest made for an older one is not sent.
    generation: u64,
    /// Files waiting for the mirror at a revision: (revision, count).
    uploads: Option<(u64, usize)>,
    checking_uploads: bool,
    uploads_checked: Option<Instant>,
}

/// Exponential smoothing (a quarter of each new sample).
fn smooth(old: Option<f64>, sample: f64) -> Option<f64> {
    Some(old.map_or(sample, |o| o * 0.75 + sample * 0.25))
}

impl Link {
    fn status(&self) -> std::sync::MutexGuard<'_, CloudStatus> {
        self.shared.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            if self.shared.stop.load(Ordering::Acquire) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(c) => self.command(c),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(c) = rx.try_recv() {
                self.command(c);
            }
            while let Some(ev) = self.client.try_recv() {
                self.event(ev);
            }
            self.housekeeping();
        }
        self.lost("the cloud preview was switched off".into());
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Purge => {
                if self.ready() {
                    self.client.send(Up::Msg { msg: FrameMsg::Purge, want: Want::default(), urgent: false }, vec![]);
                }
            }
            Cmd::Start(job, done) => self.start(*job, done),
            Cmd::Manifest(generation, manifest) => {
                if generation != self.generation || !self.client.connected() || !matches!(self.preflight, Preflight::Asking(None)) {
                    return;
                }
                let id = self.id();
                let params = json!({"command": "cloud.preflight", "params": {"manifest": manifest}});
                if self.client.send(Up::Control { id, method: "engine.execute".into(), params }, vec![]) {
                    self.preflight = Preflight::Asking(Some(id));
                } else {
                    self.preflight = Preflight::Unknown;
                }
            }
            Cmd::Pending { revision, count } => {
                self.checking_uploads = false;
                self.uploads = Some((revision, count));
                self.status().pending_uploads = count;
                self.update_slots();
            }
        }
    }

    /// Frames can go to the server: it holds this Mac's project and every file and font it needs,
    /// and the mirror has nothing left to upload.
    fn ready(&self) -> bool {
        self.client.connected()
            && self.syncing.is_none()
            && self.mirror.revision().is_some()
            && self.preflight == Preflight::Ok
            && self.uploads.is_none_or(|(_, n)| n == 0)
    }

    /// Ask the server whether it has what the project needs: this Mac's manifest (made on a
    /// thread of its own: it stats and hashes files) goes to `cloud.preflight` there.
    fn ask_preflight(&mut self) {
        let latest = self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let Some((_, project)) = latest else { return };
        self.preflight = Preflight::Asking(None);
        let (me, generation, remap) = (self.me.clone(), self.generation, self.remap.clone());
        let path = self.shared.path.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let spawned = std::thread::Builder::new().name("ec-cloud-manifest".into()).spawn(move || {
            use effectcraft_engine::cloud_assets::{Hash, manifest, read_sidecar};
            let project = remap_project(&project, remap.as_ref());
            let read = |p: &str| std::fs::read(p).ok();
            let sidecar = read_sidecar(&read, path.as_deref());
            let m = manifest(&project, &read, Hash::Small, &sidecar, path.as_deref());
            let _ = me.send(Cmd::Manifest(generation, serde_json::to_value(m).unwrap_or(Value::Null)));
        });
        if spawned.is_err() {
            self.preflight = Preflight::Unknown;
        }
    }

    /// The server's `cloud.preflight` answer (`reply`: the control reply `{ok, result | error}`).
    fn preflight_answer(&mut self, reply: &Value) {
        let error = reply.get("error").and_then(Value::as_str).unwrap_or("");
        let (ok, summary) = if reply.get("ok").and_then(Value::as_bool) == Some(true) {
            let r = reply.get("result").cloned().unwrap_or(Value::Null);
            (r.get("ok").and_then(Value::as_bool) != Some(false), r.get("summary").and_then(Value::as_str).map(str::to_string))
        } else if error.contains("still starting") {
            // Ask again shortly.
            self.preflight = Preflight::Missing { retry: Instant::now() + Duration::from_secs(2) };
            return self.update_slots();
        } else {
            // An older server (no `cloud.preflight`, no control host): it can't say; its own guard
            // still refuses frames with missing files.
            log::info!("cloud preview: no preflight on the server ({error})");
            (true, None)
        };
        self.preflight = if ok { Preflight::Ok } else { Preflight::Missing { retry: Instant::now() + PREFLIGHT_RETRY } };
        self.status().missing = if ok { None } else { Some(summary.unwrap_or_else(|| "files are missing on the cloud machine".into())) };
        self.update_slots();
    }

    /// Ask (on a thread) which files the mirror still has to upload, when the project changed
    /// or a few seconds passed.
    fn check_uploads(&mut self) {
        if self.checking_uploads || self.uploads_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        let latest = self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let Some((revision, project)) = latest else { return };
        if self.uploads.is_some_and(|(r, n)| r == revision && n == 0) && self.uploads_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            return;
        }
        self.checking_uploads = true;
        self.uploads_checked = Some(Instant::now());
        let me = self.me.clone();
        let path = self.shared.path.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let spawned = std::thread::Builder::new().name("ec-cloud-uploads".into()).spawn(move || {
            use effectcraft_engine::cloud_assets::{pending_uploads, read_sidecar};
            let read = |p: &str| std::fs::read(p).ok();
            let v = pending_uploads(&project, &read, &read_sidecar(&read, path.as_deref()));
            let count = v.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
            let _ = me.send(Cmd::Pending { revision, count });
        });
        if spawned.is_err() {
            self.checking_uploads = false;
        }
    }

    fn start(&mut self, job: RemoteJob, done: RemoteDone) {
        if !self.ready() {
            return done(Err("the cloud is not ready".into()));
        }
        if job.cancel.load(Ordering::Relaxed) {
            return done(Err("called off".into()));
        }
        if self.mirror.revision().is_some_and(|r| job.revision < r) {
            return done(Err("an older revision than the cloud's".into()));
        }
        let project = remap_project(&job.project, self.remap.as_ref());
        match self.mirror.sync(job.revision, &project) {
            None => {}
            Some(msg @ FrameMsg::Patch(_)) => {
                if !self.client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]) {
                    self.mirror.reset();
                    return done(Err("the cloud connection dropped".into()));
                }
            }
            Some(msg) => {
                // The whole project (the patch could not be made): frames render here until the
                // server holds it.
                self.send_project(msg);
                return done(Err("the cloud is loading the project".into()));
            }
        }
        let mut opts = job.opts;
        // The server's default renderer is its CPU; Auto picks its GPU (CR4 hand-over).
        opts.backend = Backend::Auto;
        let id = self.id();
        let render =
            FrameMsg::Render { id, revision: job.revision, comp: job.comp, time: job.t, opts: Box::new(opts), disk: None, layers: false, prefetch: vec![] };
        let want = Want { codec: Codec::Jpeg, quality: 85, max_size: None, refine: job.urgent };
        if !self.client.send(Up::Msg { msg: render, want, urgent: job.urgent }, vec![]) {
            return done(Err("the cloud connection dropped".into()));
        }
        self.pending.insert(id, Pending { done: Some(done), refined: job.refined, timed: job.timed, cancel: job.cancel, sent: Instant::now() });
        self.status().queue = self.pending.len();
    }

    /// Send the whole project (a [`FrameMsg::Project`]) and the echo that says it arrived.
    fn send_project(&mut self, msg: FrameMsg) {
        let id = self.id();
        let ok = self.client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]) && self.client.send(Up::Echo { id }, vec![]);
        if ok {
            self.syncing = Some((id, Instant::now()));
            self.status().projects += 1;
        } else {
            self.mirror.reset();
        }
        self.update_slots();
    }

    /// Bring the server to the newest project, whole.
    fn resync(&mut self) {
        self.mirror.reset();
        let latest = self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let Some((rev, project)) = latest else { return };
        let project = remap_project(&project, self.remap.as_ref());
        if let Some(msg) = self.mirror.sync(rev, &project) {
            self.send_project(msg);
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Connected { welcome } => {
                {
                    let mut s = self.status();
                    if s.server.is_some() {
                        s.reconnects += 1;
                    }
                    if let Down::Welcome { gpu, server, .. } = welcome {
                        s.gpu = gpu;
                        s.server = Some(server);
                    }
                    s.connected = true;
                    s.refused = false;
                }
                self.fail_all("reconnected");
                self.ping = None;
                self.syncing = None;
                self.generation = self.generation.wrapping_add(1);
                self.preflight = Preflight::Unknown;
                self.resync();
            }
            Event::Disconnected { error } => self.lost(error),
            Event::Refused { error } => {
                self.lost(format!("the cloud refused this Mac: {error}"));
                self.status().refused = true;
            }
            Event::Down(down, payload) => self.down(down, payload),
        }
    }

    /// The connection is gone: every frame in flight renders here instead.
    fn lost(&mut self, error: String) {
        self.mirror.reset();
        self.syncing = None;
        self.preflight = Preflight::Unknown;
        self.ping = None;
        self.fail_all(&error);
        {
            let mut s = self.status();
            s.connected = false;
            s.ready = false;
            s.error = Some(error);
        }
        self.update_slots();
    }

    fn fail_all(&mut self, why: &str) {
        for (_, mut p) in self.pending.drain() {
            if let Some(done) = p.done.take() {
                done(Err(why.to_string()));
            }
        }
        self.refining.clear();
        self.status().queue = 0;
    }

    fn down(&mut self, down: Down, payload: Vec<u8>) {
        match down {
            Down::Echo { id } => {
                if self.syncing.is_some_and(|(s, _)| s == id) {
                    self.syncing = None;
                    self.ping = None;
                    // The server holds the project: does it have its files? (Again after a resync.)
                    self.ask_preflight();
                    self.update_slots();
                } else if let Some((p, sent)) = self.ping
                    && p == id
                {
                    self.ping = None;
                    let ms = sent.elapsed().as_secs_f64() * 1000.0;
                    let mut s = self.status();
                    s.rtt_ms = smooth(s.rtt_ms, ms);
                    drop(s);
                    self.update_slots();
                }
            }
            Down::Reply { reply, pixels } => match reply {
                FrameReply::Frame { id, ms, layer_times, .. } => {
                    let Some(px) = pixels else { return self.fail(id, "a frame without pixels".into()) };
                    {
                        let mut s = self.status();
                        s.frames += 1;
                        s.bytes += payload.len() as u64;
                        if !px.refine {
                            s.server_ms = smooth(s.server_ms, ms);
                        }
                    }
                    if px.refine {
                        if let Some((refined, _)) = self.refining.remove(&id) {
                            decode_then(px, payload, ms, move |r| {
                                if let Ok(f) = r {
                                    refined(f);
                                }
                            });
                        }
                        return;
                    }
                    let Some(mut p) = self.pending.remove(&id) else { return };
                    self.status().queue = self.pending.len();
                    // The server's times, for the Render Time pane (before the frame lands, so the
                    // pane has them when it shows the frame).
                    if let Some(timed) = p.timed.take() {
                        let layers = layer_times.into_iter().map(|(c, l, ms)| ((c, l), f64::from(ms))).collect();
                        timed(FrameTimes { frame_ms: ms, layers, remote: true });
                    }
                    if let Some(refined) = p.refined.take() {
                        self.refining.insert(id, (refined, Instant::now()));
                    }
                    let (shared, sent) = (self.shared.clone(), p.sent);
                    if let Some(done) = p.done.take() {
                        decode_then(px, payload, ms, move |r| {
                            if r.is_ok() {
                                let mut s = shared.status.lock().unwrap_or_else(PoisonError::into_inner);
                                s.frame_ms = smooth(s.frame_ms, sent.elapsed().as_secs_f64() * 1000.0);
                            }
                            done(r)
                        });
                    }
                }
                FrameReply::Failed { id, error, .. } => {
                    self.refining.remove(&id);
                    // The server's guard (CR7): files or fonts went missing there. Render here and
                    // ask again soon.
                    if let Some(what) = error.strip_prefix("missing on the cloud machine") {
                        self.preflight = Preflight::Missing { retry: Instant::now() + PREFLIGHT_RETRY / 2 };
                        self.status().missing = Some(what.trim_start_matches([':', ' ']).to_string());
                        self.update_slots();
                    }
                    self.fail(id, error);
                }
                FrameReply::Resync { error } => {
                    log::info!("cloud preview: resync ({error})");
                    self.resync();
                }
                // Playback stream replies: the Mac half (CR6b) decodes them.
                FrameReply::Stored { .. } | FrameReply::Info { .. } | FrameReply::Stream(_) => {}
            },
            Down::Control { id, reply } => {
                if self.preflight == Preflight::Asking(Some(id)) {
                    self.preflight_answer(&reply);
                }
            }
            Down::Welcome { .. } | Down::Refused { .. } => {}
        }
    }

    fn fail(&mut self, id: u64, error: String) {
        if let Some(mut p) = self.pending.remove(&id)
            && let Some(done) = p.done.take()
        {
            done(Err(error));
        }
        self.status().queue = self.pending.len();
    }

    /// Call off frames no longer wanted, check the link, forget old refines.
    fn housekeeping(&mut self) {
        let called_off: Vec<u64> = self.pending.iter().filter(|(_, p)| p.cancel.load(Ordering::Relaxed)).map(|(id, _)| *id).collect();
        if !called_off.is_empty() {
            self.client.send(Up::Cancel { ids: called_off.clone() }, vec![]);
            for id in called_off {
                self.fail(id, "called off".into());
            }
        }
        self.refining.retain(|_, (_, at)| at.elapsed() < REFINE_WAIT);
        if !self.client.connected() {
            return;
        }
        // Connected before the app offered a project: send it once there is one.
        if self.mirror.revision().is_none() && self.syncing.is_none() {
            self.resync();
        }
        if self.syncing.is_none() && self.mirror.revision().is_some() {
            if let Preflight::Missing { retry } = self.preflight
                && Instant::now() >= retry
            {
                self.ask_preflight();
            }
            self.check_uploads();
        }
        // A silent link (a sleeping laptop, a stalled tunnel) never reports an error: an echo
        // or a whole project that doesn't come back means it is dead. While a whole project is
        // on its way, echoes queue behind it: only its own deadline counts then.
        let dead = match self.syncing {
            Some((_, at)) => at.elapsed() > PROJECT_DEAD,
            None => self.ping.is_some_and(|(_, at)| at.elapsed() > DEAD),
        };
        if dead {
            self.client.drop_connection();
            self.lost("the cloud stopped answering".into());
            return;
        }
        if self.syncing.is_none() && self.ping.is_none() && self.last_ping.is_none_or(|t| t.elapsed() >= PING) {
            let id = self.id();
            if self.client.send(Up::Echo { id }, vec![]) {
                let now = Instant::now();
                self.ping = Some((id, now));
                self.last_ping = Some(now);
            }
        }
    }

    /// Frames in flight: enough to cover a round trip at the comp's frame rate (Little's law),
    /// 0 while the server isn't ready.
    fn update_slots(&self) {
        let ready = self.ready();
        let mut s = self.status();
        s.ready = ready;
        let fps = f32::from_bits(self.shared.fps.load(Ordering::Relaxed)) as f64;
        let n = if ready { slots_for(fps, s.rtt_ms.unwrap_or(100.0), s.server_ms.unwrap_or(20.0)) } else { 0 };
        s.slots = n;
        drop(s);
        self.shared.slots.store(n, Ordering::Release);
    }
}

/// Frames in flight to keep `fps` frames a second coming over a link of `rtt_ms` with
/// `render_ms` per frame on the server (and ~10 ms to decode here): `fps × latency`, plus one so
/// a slot is free while a frame decodes. 2–16.
pub fn slots_for(fps: f64, rtt_ms: f64, render_ms: f64) -> usize {
    let fps = if fps.is_finite() && fps > 0.0 { fps.min(120.0) } else { 30.0 };
    let latency = (rtt_ms.max(0.0) + render_ms.max(0.0) + 10.0) / 1000.0;
    ((fps * latency).ceil() as usize).saturating_add(1).clamp(2, 16)
}

/// Decode a reply's pixels on the frame pool (a 1080p JPEG is ~10–30 ms), then call `then`.
fn decode_then(px: remote_encode::Pixels, payload: Vec<u8>, ms: f64, then: impl FnOnce(Result<RemoteFrame, String>) + Send + 'static) {
    rayon::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| remote_encode::decode(&px, &payload)))
            .unwrap_or_else(|_| Err("decoding the frame panicked".into()))
            .map(|rgba| RemoteFrame { width: px.width, height: px.height, rgba, ms });
        then(r);
    });
}

/// `project` with its footage paths rewritten by `remap` (the same project when nothing changes).
pub fn remap_project(project: &Arc<Project>, remap: Option<&PathRemap>) -> Arc<Project> {
    let Some(remap) = remap else { return project.clone() };
    let mut out: Option<Project> = None;
    for (id, item) in &project.items {
        let ItemKind::Footage(f) = &item.kind else { continue };
        let path = remap(&f.path).filter(|p| *p != f.path);
        let seq: Vec<Option<String>> = f.sequence.iter().map(|s| remap(s).filter(|p| p != s)).collect();
        if path.is_none() && seq.iter().all(Option::is_none) {
            continue;
        }
        let p = out.get_or_insert_with(|| (**project).clone());
        if let Some(ItemKind::Footage(f)) = p.items.get_mut(id).map(|i| &mut i.kind) {
            if let Some(path) = path {
                f.path = path;
            }
            for (s, new) in f.sequence.iter_mut().zip(seq) {
                if let Some(new) = new {
                    *s = new;
                }
            }
        }
    }
    out.map_or_else(|| project.clone(), Arc::new)
}

/// Where the viewer's frames come from (Settings ▸ Previews ▸ Preview On).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Mac,
    Cloud,
}

/// The app's side of the cloud preview: the client while Preview On isn't This Mac, which
/// renderer the viewer uses, and the status notes.
#[derive(Default)]
pub struct CloudPreview {
    client: Option<Arc<CloudFrames>>,
    /// The address and token the client connected with.
    target: (String, String),
    pub source: Option<Source>,
    /// This Mac's render time per comp (ms, smoothed), for Auto.
    local_ms: HashMap<u64, f64>,
    last_seen_ms: f64,
    /// The cloud was ready at the last tick (to note it going away and coming back).
    was_ready: bool,
    /// The viewer's frames go to this client now (the app set it as the remote renderer).
    pub installed: bool,
    /// Why the cloud waits, as last noted (missing files, uploads), so it is noted once.
    waiting_for: Option<String>,
}

/// What [`CloudPreview::tick`] follows.
#[derive(Clone, Copy)]
pub struct Now<'a> {
    /// Preview On: `local`, `cloud` or `auto`.
    pub mode: &'a str,
    pub address: &'a str,
    pub token: &'a str,
    pub revision: u64,
    pub project: &'a Arc<Project>,
    /// Where the project is saved.
    pub path: Option<&'a str>,
    /// The previewed comp and its frame rate.
    pub comp: Option<(u64, f64)>,
    /// The viewer frame's last render time (ms, [`crate::frames::Frames::last_ms`]).
    pub last_ms: f64,
}

/// Preview On values.
pub const MODES: [&str; 3] = ["local", "cloud", "auto"];

impl CloudPreview {
    pub fn client(&self) -> Option<&Arc<CloudFrames>> {
        self.client.as_ref()
    }

    pub fn status(&self) -> Option<CloudStatus> {
        self.client.as_ref().map(|c| c.status())
    }

    /// Follow the setting: connect (or disconnect) and pick the renderer. Returns the
    /// [`RemoteFrames`] the viewer should use (`None`: this Mac) and a status note when the
    /// source changed on its own (a lost or regained connection).
    pub fn tick(&mut self, now: &Now<'_>) -> (Option<Arc<CloudFrames>>, Option<String>) {
        let Now { mode, address, token, revision, project, path, comp, last_ms } = *now;
        let mode = if MODES.contains(&mode) { mode } else { "local" };
        if mode == "local" || address.trim().is_empty() {
            self.client = None;
            self.was_ready = false;
            self.source = Some(Source::Mac);
            return (None, None);
        }
        let target = (address.trim().to_string(), token.to_string());
        if self.client.is_none() || self.target != target {
            self.client = Some(CloudFrames::connect(&target.0, &target.1, None));
            self.target = target;
            self.was_ready = false;
        }
        let Some(client) = self.client.clone() else { return (None, None) };
        client.offer(revision, project);
        client.set_path(path);
        if let Some((_, fps)) = comp {
            client.set_fps(fps);
        }
        // This Mac's cost, measured while it renders the viewer's frames.
        if self.source != Some(Source::Cloud)
            && last_ms > 0.0
            && last_ms != self.last_seen_ms
            && let Some((c, _)) = comp
        {
            let e = self.local_ms.entry(c).or_insert(last_ms);
            *e = *e * 0.75 + last_ms * 0.25;
        }
        self.last_seen_ms = last_ms;
        let ready = client.ready();
        let status = client.status();
        let use_cloud = ready
            && match mode {
                "cloud" => true,
                _ => auto_picks_cloud(comp.and_then(|(c, _)| self.local_ms.get(&c).copied()), &status, self.source == Some(Source::Cloud)),
            };
        let mut note = None;
        if ready != self.was_ready {
            note = Some(if ready {
                let gpu = status.gpu.clone().unwrap_or_else(|| "the server".into());
                format!("Cloud preview connected: {gpu}{}", status.rtt_ms.map(|r| format!(", {r:.0} ms")).unwrap_or_default())
            } else if status.refused {
                "Cloud preview refused this Mac's token: previewing on this Mac".to_string()
            } else {
                "Cloud preview lost: previewing on this Mac (reconnecting…)".to_string()
            });
            // No note for a first connection that never came up.
            if !ready && !self.was_ready {
                note = None;
            }
            self.was_ready = ready;
        }
        // Connected but held back by missing files or uploads (CR7): say why, once.
        let waiting = (!ready && status.connected)
            .then(|| match (&status.missing, status.pending_uploads) {
                (Some(m), _) => Some(format!("Cloud preview waits for the cloud machine: {m} — previewing on this Mac")),
                (None, n) if n > 0 => Some(format!("Cloud preview waits for {n} uploads — previewing on this Mac")),
                _ => None,
            })
            .flatten();
        if waiting != self.waiting_for {
            if waiting.is_some() {
                note = waiting.clone();
            }
            self.waiting_for = waiting;
        }
        self.source = Some(if use_cloud { Source::Cloud } else { Source::Mac });
        (use_cloud.then_some(client), note)
    }
}

/// Auto: the cloud when this Mac's frame cost is above the cloud's (round trip + the server's
/// render), with some hysteresis so it doesn't flip between frames. With no local measurement
/// yet it stays here.
pub fn auto_picks_cloud(local_ms: Option<f64>, cloud: &CloudStatus, on_cloud: bool) -> bool {
    let Some(local) = local_ms else { return on_cloud };
    let cost = cloud.rtt_ms.unwrap_or(100.0) + cloud.server_ms.unwrap_or(20.0) + 10.0;
    if on_cloud { local > cost * 0.85 } else { local > cost * 1.15 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::project::{ItemId, LayerId};

    #[test]
    fn slots_cover_a_round_trip() {
        // 30 fps over 86 ms + 20 ms render + 10 ms decode ≈ 3.5 → 4 + 1.
        assert_eq!(slots_for(30.0, 86.0, 20.0), 5);
        assert_eq!(slots_for(60.0, 160.0, 30.0), 13);
        assert_eq!(slots_for(24.0, 1.0, 1.0), 2);
        assert_eq!(slots_for(f64::NAN, 10_000.0, 0.0), 16);
    }

    #[test]
    fn auto_has_hysteresis() {
        let s = CloudStatus { rtt_ms: Some(86.0), server_ms: Some(20.0), ..CloudStatus::default() };
        assert!(!auto_picks_cloud(None, &s, false));
        assert!(!auto_picks_cloud(Some(40.0), &s, false));
        assert!(!auto_picks_cloud(Some(120.0), &s, false), "116 ms here vs 116 ms there: stay");
        assert!(auto_picks_cloud(Some(140.0), &s, false));
        assert!(auto_picks_cloud(Some(110.0), &s, true), "on the cloud it stays until clearly faster here");
        assert!(!auto_picks_cloud(Some(90.0), &s, true));
    }

    #[test]
    fn remap_is_identity_without_a_hook() {
        let p = Arc::new(Project::default());
        assert!(Arc::ptr_eq(&remap_project(&p, None), &p));
        let same: PathRemap = Arc::new(|_| None);
        assert!(Arc::ptr_eq(&remap_project(&p, Some(&same)), &p));
    }
    /// A stand-in for `effectcraft-cli serve --frames`: answers echoes, and render requests with
    /// an opaque red frame (until `stall`), and can be "unplugged" like a pulled SSH tunnel.
    struct FakeServer {
        addr: String,
        kill: Arc<AtomicBool>,
        stall: Arc<AtomicBool>,
        renders: Arc<AtomicUsize>,
        streams: Arc<Mutex<Vec<std::net::TcpStream>>>,
        /// What `cloud.preflight` says is missing (`None`: nothing).
        missing: Arc<Mutex<Option<String>>>,
    }

    impl FakeServer {
        fn start(token: &'static str) -> FakeServer {
            use effectcraft_engine::remote_wire::{MAX_PART, VERSION, read_frame, write_frame};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let (kill, stall, renders, streams) =
                (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(vec![])));
            let (k, st, r, ss) = (kill.clone(), stall.clone(), renders.clone(), streams.clone());
            let missing: Arc<Mutex<Option<String>>> = Arc::default();
            let mi = missing.clone();
            std::thread::spawn(move || {
                while !k.load(Ordering::Acquire) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    ss.lock().unwrap().push(stream.try_clone().unwrap());
                    let (st, r, mi) = (st.clone(), r.clone(), mi.clone());
                    std::thread::spawn(move || {
                        let (mut rd, mut wr) = (stream.try_clone().unwrap(), stream);
                        let Ok((Up::Hello { token: t, .. }, _)) = read_frame::<Up>(&mut rd, MAX_PART) else { return };
                        if t != token {
                            let _ = write_frame(&mut wr, &Down::Refused { error: "wrong token".into() }, &[]);
                            return;
                        }
                        let welcome = Down::Welcome { version: VERSION, server: "fake".into(), gpu: Some("Fake GPU".into()), threads: 1, revision: None };
                        write_frame(&mut wr, &welcome, &[]).unwrap();
                        while let Ok((up, _)) = read_frame::<Up>(&mut rd, MAX_PART) {
                            let reply = match up {
                                Up::Echo { id } => (Down::Echo { id }, vec![]),
                                Up::Control { id, .. } => {
                                    let summary = mi.lock().unwrap().clone();
                                    let reply = json!({"ok": true, "result": {"ok": summary.is_none(), "summary": summary.unwrap_or_default()}});
                                    (Down::Control { id, reply }, vec![])
                                }
                                Up::Msg { msg: FrameMsg::Render { id, .. }, .. } if !st.load(Ordering::Acquire) => {
                                    r.fetch_add(1, Ordering::AcqRel);
                                    let rgba: Vec<u8> = [255u8, 0, 0, 255].repeat(32 * 18);
                                    let (px, payload) = remote_encode::encode(&rgba, 32, 18, Codec::Raw, 85).unwrap();
                                    let frame = FrameReply::Frame {
                                        id,
                                        width: 32,
                                        height: 18,
                                        ms: 1.0,
                                        passes: 1,
                                        gpu: true,
                                        layer_misses: vec![],
                                        layer_hits: vec![],
                                        layer_times: vec![(ItemId(7), LayerId(1), 12.5)],
                                    };
                                    (Down::Reply { reply: frame, pixels: Some(px) }, payload)
                                }
                                _ => continue,
                            };
                            if write_frame(&mut wr, &reply.0, &reply.1).is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            FakeServer { addr, kill, stall, renders, streams, missing }
        }

        /// Pull the tunnel: every connection drops and nothing answers on the port again.
        fn unplug(&self) {
            self.kill.store(true, Ordering::Release);
            for s in self.streams.lock().unwrap().iter() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn red(frames: &crate::frames::Frames, k: &crate::frames::FrameKey) -> bool {
        match frames.get(k) {
            Some(crate::frames::FrameImage::Cpu(img)) => img.pixels.first().is_some_and(|p| p.r() == 255 && p.a() == 255),
            _ => false,
        }
    }

    /// CR5 acceptance: frames come from the cloud while it is there; pulling the tunnel fails
    /// the frame in flight, and that frame and the next render on this Mac, with no error for
    /// the document.
    #[test]
    fn a_pulled_tunnel_falls_back_to_rendering_here() {
        use crate::frames::{FrameKey, Frames, RenderSource, opts_hash};
        use effectcraft_engine::render::RenderOpts;
        use effectcraft_engine::time::Tick;
        let server = FakeServer::start("secret");
        let cloud = CloudFrames::connect(&server.addr, "secret", None);
        let mut s = effectcraft_engine::Session::default();
        s.execute("comp.new", serde_json::json!({"width": 32, "height": 18, "duration": 1})).unwrap();
        let cid = s.active_comp_id().unwrap();
        cloud.offer(s.revision, &s.project);
        wait_until("the cloud to hold the project", || cloud.ready());
        assert!(cloud.status().gpu.as_deref() == Some("Fake GPU"));

        let mut frames = Frames::default();
        frames.set_remote(Some(cloud.clone()));
        assert!(frames.remote_active());
        let src = RenderSource {
            freeze_feedback: Default::default(),
            project: s.project.clone(),
            footage: s.footage.clone(),
            expr: None,
            layer_cache: Arc::default(),
            gpu: None,
            gpu_display: false,
            disk: None,
        };
        let opts = RenderOpts::default();
        let content = frames.content_of(&s.project, s.revision, cid);
        let key = |frame| FrameKey { revision: s.revision, content, comp: cid.0, frame, scale: 1000, view: 0, opts: opts_hash(&opts) };
        // From the cloud.
        frames.request_urgent(&src, key(0), cid, Tick(0), opts);
        wait_until("the cloud frame", || frames.is_cached(&key(0)));
        assert!(red(&frames, &key(0)), "frame 0 came from the (fake) cloud");
        // Its render times are the server's, for the Render Time pane.
        let t = frames.times.get(&key(0)).expect("the cloud frame's times");
        assert!(t.remote && t.frame_ms == 1.0, "{t:?}");
        assert_eq!(t.layer(ItemId(7), LayerId(1)), Some(12.5));
        assert_eq!(server.renders.load(Ordering::Acquire), 1);

        // A frame in flight when the tunnel goes.
        server.stall.store(true, Ordering::Release);
        frames.request_urgent(&src, key(1), cid, Tick(0), opts);
        wait_until("the frame to be in flight", || cloud.status().queue == 1);
        server.unplug();
        wait_until("the cloud to be gone", || !cloud.ready());
        frames.tick_remote();
        assert!(!frames.remote_active());
        let st = cloud.status();
        assert!(!st.connected && st.queue == 0 && st.error.is_some(), "{st:?}");
        wait_until("the lost frame to be released", || frames.inflight() == 0);

        // The viewer asks again: rendered here.
        for f in [1, 2] {
            frames.request_urgent(&src, key(f), cid, Tick(0), opts);
            wait_until("the local frame", || frames.is_cached(&key(f)));
            assert!(!red(&frames, &key(f)), "frame {f} rendered on this Mac");
        }
        assert_eq!(server.renders.load(Ordering::Acquire), 1, "nothing more went to the cloud");
    }

    /// CR7: while the cloud machine lacks a file or font, frames render here and the status
    /// says what is missing; once it has them, frames go to the cloud.
    #[test]
    fn missing_files_on_the_cloud_keep_the_preview_here() {
        let server = FakeServer::start("secret");
        *server.missing.lock().unwrap() = Some("1 missing: font “X”".into());
        let cloud = CloudFrames::connect(&server.addr, "secret", None);
        let mut s = effectcraft_engine::Session::default();
        s.execute("comp.new", serde_json::json!({"width": 32, "height": 18, "duration": 1})).unwrap();
        cloud.offer(s.revision, &s.project);
        wait_until("the preflight answer", || cloud.status().missing.is_some());
        assert!(!cloud.ready(), "not ready while something is missing there");
        assert_eq!(cloud.status().missing.as_deref(), Some("1 missing: font “X”"));
        let mut preview = CloudPreview::default();
        let now =
            Now { mode: "cloud", address: &server.addr, token: "secret", revision: s.revision, project: &s.project, path: None, comp: None, last_ms: 0.0 };
        // (A preview of its own: it connects on its own and gets the same answer.)
        let mut note = None;
        wait_until("the waiting note", || {
            let (remote, n) = preview.tick(&now);
            assert!(remote.is_none());
            note = n.or(note.take());
            note.as_deref().is_some_and(|n| n.contains("1 missing"))
        });
        // The mirror uploads it: the next preflight says ok.
        *server.missing.lock().unwrap() = None;
        wait_until("the cloud to be ready", || cloud.ready());
        assert!(cloud.status().missing.is_none());
    }
}
