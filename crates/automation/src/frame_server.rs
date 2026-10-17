//! The cloud frame server (`effectcraft-cli serve --frames PORT`, `plan/cloud-render.md` CR2/CR4):
//! renders viewer frames for a desktop app over the binary frame channel
//! ([`effectcraft_engine::remote_wire`]).
//!
//! - Each connection holds its own replica of the client's project (a
//!   [`effectcraft_engine::remote::FrameServer`] that applies [`FrameMsg::Project`] /
//!   [`FrameMsg::Patch`]). The Mac stays the source of truth; the replica only renders.
//! - Render requests go to one queue shared by a pool of workers: urgent requests (the viewer
//!   waits) first, the newest first; then prefetch, oldest first; then, when nothing else waits,
//!   lossless refines of frames sent as JPEG. [`Up::Cancel`] drops requests by id.
//! - A request for another revision than the replica's answers `Failed {stale: true}`, also when
//!   the replica moved on while the frame rendered: an old frame is never shown.
//! - Frames are premultiplied RGBA8, scaled to the size the viewer asks for and encoded
//!   ([`effectcraft_engine::remote_encode`]).
//! - Logs (through [`FrameConfig::log`]): render ms, encode ms, bytes and queue depth per frame.
//!
//! The listener binds loopback only (v1 is reached through an SSH tunnel) and every connection
//! starts with the token ([`Up::Hello`]); wrong tokens lock out for a while, as the Figma bridge's
//! pairing codes do. A connection that drops loses nothing: its queued frames are dropped and the
//! client resends its project when it reconnects.

use std::collections::{HashSet, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use effectcraft_engine::project::{ItemId, Project};
use effectcraft_engine::remote::{FrameMsg, FrameReply, FrameServer};
use effectcraft_engine::remote_encode::{self, Codec, Want};
use effectcraft_engine::remote_wire::{Down, MAX_HELLO, MAX_PART, Up, VERSION, read_frame, write_frame};
use effectcraft_engine::render::{Backend, ExprHost, FootageSource, Image, LayerCache, LayerTimes, NoFootage, RenderOpts, Renderer};
use effectcraft_engine::time::Tick;
use serde_json::{Value, json};

use crate::control_wire::{self, lock};
use crate::figma_bridge::{new_code, same_code};
use crate::frame_stream::{StreamHost, Streams};

/// The token file in the config directory (mode 0600).
pub const TOKEN_FILE: &str = "frame-server-token";
/// Default frame workers (until CR1 sizes the pool by the GPU's memory).
pub const DEFAULT_THREADS: usize = 8;
/// Wrong tokens in a row before tokens are refused for [`LOCKOUT`].
const MAX_WRONG_TOKENS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long a new connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Lossless refines kept per connection (the newest; older ones are dropped).
const MAX_REFINES: usize = 2;

/// One frame to render.
pub struct RenderJob<'a> {
    pub project: &'a Project,
    pub comp: ItemId,
    pub time: Tick,
    pub opts: RenderOpts,
    /// The server's layer cache (shared by every worker and connection).
    pub cache: &'a LayerCache,
    /// Where each layer's render time goes (the Mac's Render Time pane), when timed.
    pub times: Option<&'a LayerTimes>,
}

/// A rendered frame.
pub enum Rendered {
    /// From the CPU compositor.
    Image(Image),
    /// Premultiplied RGBA8 as the viewer shows it (a GPU display frame read back).
    Rgba8 { width: u32, height: u32, bytes: Vec<u8> },
}

/// How the server renders a frame: [`CpuRender`], or the CLI's GPU renderer.
pub trait FrameRender: Send + Sync {
    /// The frame and whether the GPU rendered it.
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String>;
    /// The GPU adapter, when frames render on one.
    fn gpu_name(&self) -> Option<String> {
        None
    }
    /// Get ready to render `comp` around `time` (compile what its frames need), before a stream.
    fn warm(&self, _project: &Project, _comp: ItemId, _time: Tick) {}
}

/// Frames on the CPU.
pub struct CpuRender {
    pub footage: Arc<dyn FootageSource>,
    pub expr: Option<Arc<dyn ExprHost>>,
}

impl FrameRender for CpuRender {
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        let mut r = Renderer::new(job.project, self.footage.as_ref(), RenderOpts { backend: Backend::Cpu, ..job.opts });
        r.expr = self.expr.as_deref();
        r.cache = Some(job.cache);
        r.times = job.times;
        Ok((Rendered::Image(r.comp_frame_cpu(job.comp, job.time)), false))
    }
}

/// A control request run on the server's host: method, params → the control reply
/// (`{"ok": …}`).
pub type ControlFn = Arc<dyn Fn(&str, Value) -> Value + Send + Sync>;
/// Where log lines go.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// How to run the frame server.
pub struct FrameConfig {
    /// Port on 127.0.0.1 (0: any free one).
    pub port: u16,
    pub token: String,
    /// Frame workers.
    pub threads: usize,
    pub render: Arc<dyn FrameRender>,
    /// The layer cache the workers share.
    pub cache: Arc<LayerCache>,
    /// A project the server opened at start (JSON): every connection's replica starts with it
    /// at revision 0, so a client can render without sending its own.
    pub preload: Option<Arc<String>>,
    /// Runs [`Up::Control`] requests (the host's control channel); `None` refuses them.
    pub control: Option<ControlFn>,
    pub log: LogFn,
}

/// A running frame server.
pub struct FrameHandle {
    pub port: u16,
    stop: Arc<AtomicBool>,
    sched: Arc<Sched>,
}

impl FrameHandle {
    /// Stop accepting and stop the workers (open connections end when their peers go).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        lock(&self.sched.q).stop = true;
        self.sched.cv.notify_all();
        control_wire::wake(self.port);
    }
}

/// `explicit`, else `EFFECTCRAFT_FRAME_TOKEN`, else the token stored in `dir`, else a new one
/// (stored in `dir`, readable only by the user). Returns the token and where it is stored.
pub fn frame_token(dir: Option<&Path>, explicit: Option<&str>) -> Result<(String, Option<std::path::PathBuf>), String> {
    let env = std::env::var("EFFECTCRAFT_FRAME_TOKEN").ok();
    if let Some(t) = explicit.or(env.as_deref()).map(str::trim).filter(|t| !t.is_empty()) {
        if t.len() < 8 {
            return Err("the frame token needs at least 8 characters".into());
        }
        return Ok((t.to_string(), None));
    }
    let path = dir.map(|d| d.join(TOKEN_FILE));
    if let Some(p) = &path
        && let Ok(t) = std::fs::read_to_string(p)
        && t.trim().len() >= 8
    {
        return Ok((t.trim().to_string(), path));
    }
    let token = new_code()?;
    let mut stored = None;
    if let Some(p) = &path {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if effectcraft_engine::figma::write_private(p, token.as_bytes()).is_ok() {
            stored = path;
        }
    }
    Ok((token, stored))
}

/// Start the server: listen on 127.0.0.1, start the workers, accept on a thread of its own.
pub fn spawn(config: FrameConfig) -> Result<FrameHandle, String> {
    let listener = TcpListener::bind(("127.0.0.1", config.port)).map_err(|e| format!("cannot listen on 127.0.0.1:{} ({e})", config.port))?;
    let port = listener.local_addr().map(|a| a.port()).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let sched = Arc::new(Sched::default());
    let shared = Arc::new(Shared {
        token: config.token,
        threads: config.threads.max(1),
        render: config.render,
        cache: config.cache,
        preload: config.preload,
        control: config.control,
        log: config.log,
        guesses: Mutex::new(Guesses::default()),
        next_conn: AtomicU64::new(1),
        lockout: LOCKOUT,
    });
    for i in 0..shared.threads {
        let (sched, shared) = (Arc::clone(&sched), Arc::clone(&shared));
        std::thread::Builder::new().name(format!("ec-frame-{i}")).spawn(move || worker(&sched, &shared)).map_err(|e| e.to_string())?;
    }
    {
        let (stop, sched, shared) = (Arc::clone(&stop), Arc::clone(&sched), Arc::clone(&shared));
        std::thread::Builder::new()
            .name("ec-frames-accept".into())
            .spawn(move || control_wire::accept_loop(listener, stop, move |s| connection(s, &sched, &shared)))
            .map_err(|e| e.to_string())?;
    }
    Ok(FrameHandle { port, stop, sched })
}

struct Shared {
    token: String,
    threads: usize,
    render: Arc<dyn FrameRender>,
    cache: Arc<LayerCache>,
    preload: Option<Arc<String>>,
    control: Option<ControlFn>,
    log: LogFn,
    guesses: Mutex<Guesses>,
    next_conn: AtomicU64,
    lockout: Duration,
}

#[derive(Default)]
struct Guesses {
    wrong: u32,
    locked_until: Option<Instant>,
}

impl Shared {
    fn log(&self, s: &str) {
        (self.log)(s);
    }

    /// Check a hello's token, counting wrong ones (see the Figma bridge's pairing codes).
    fn auth(&self, token: &str) -> Result<(), String> {
        let mut g = lock(&self.guesses);
        if let Some(t) = g.locked_until {
            if Instant::now() < t {
                return Err("too many wrong tokens; try again in a minute".into());
            }
            *g = Guesses::default();
        }
        if same_code(token, &self.token) {
            g.wrong = 0;
            return Ok(());
        }
        g.wrong = g.wrong.saturating_add(1);
        if g.wrong >= MAX_WRONG_TOKENS {
            g.locked_until = Instant::now().checked_add(self.lockout);
        }
        Err("wrong token".into())
    }
}

/// A connection's outgoing messages (its writer thread).
pub(crate) type Out = Sender<(Down, Vec<u8>)>;

/// The replica revision a connection holds (`u64::MAX`: none).
const NO_REVISION: u64 = u64::MAX;

struct Job {
    conn: u64,
    id: u64,
    revision: u64,
    urgent: bool,
    seq: u64,
    project: Arc<Project>,
    comp: ItemId,
    time: Tick,
    opts: RenderOpts,
    want: Want,
    out: Out,
    /// The connection's replica revision now.
    current: Arc<AtomicU64>,
    queued: Instant,
}

/// A frame sent lossy, kept to send losslessly once the server is idle.
struct Refine {
    conn: u64,
    id: u64,
    revision: u64,
    reply: FrameReply,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    out: Out,
    current: Arc<AtomicU64>,
}

#[derive(Default)]
struct Queue {
    jobs: Vec<Job>,
    /// Newest first.
    refines: VecDeque<Refine>,
    /// Requests rendering now, and those of them cancelled meanwhile.
    running: HashSet<(u64, u64)>,
    cancelled: HashSet<(u64, u64)>,
    seq: u64,
    stop: bool,
}

#[derive(Default)]
struct Sched {
    q: Mutex<Queue>,
    cv: Condvar,
}

enum Work {
    Render(Box<Job>),
    Refine(Refine),
}

impl Sched {
    fn push(&self, mut job: Job) -> usize {
        let mut q = lock(&self.q);
        q.seq += 1;
        job.seq = q.seq;
        // A newer request for the same id replaces the old one.
        q.jobs.retain(|j| (j.conn, j.id) != (job.conn, job.id));
        q.jobs.push(job);
        let depth = q.jobs.len();
        drop(q);
        self.cv.notify_one();
        depth
    }

    fn cancel(&self, conn: u64, ids: &[u64]) {
        let mut q = lock(&self.q);
        let ids: HashSet<u64> = ids.iter().copied().collect();
        q.jobs.retain(|j| j.conn != conn || !ids.contains(&j.id));
        q.refines.retain(|r| r.conn != conn || !ids.contains(&r.id));
        let running: Vec<(u64, u64)> = q.running.iter().copied().filter(|(c, i)| *c == conn && ids.contains(i)).collect();
        q.cancelled.extend(running);
    }

    /// A connection closed: drop its work.
    fn close(&self, conn: u64) {
        let mut q = lock(&self.q);
        q.jobs.retain(|j| j.conn != conn);
        q.refines.retain(|r| r.conn != conn);
        q.cancelled.retain(|(c, _)| *c != conn);
    }

    fn refine(&self, r: Refine) {
        let mut q = lock(&self.q);
        q.refines.retain(|o| (o.conn, o.id) != (r.conn, r.id));
        q.refines.push_front(r);
        let conn = q.refines.front().map(|r| r.conn).unwrap_or_default();
        let mut kept = 0;
        q.refines.retain(|o| {
            if o.conn != conn {
                return true;
            }
            kept += 1;
            kept <= MAX_REFINES
        });
        drop(q);
        self.cv.notify_one();
    }

    /// The next work: the newest urgent request, else the oldest prefetch, else a refine.
    fn next(&self) -> Option<(Work, usize)> {
        let mut q = lock(&self.q);
        loop {
            if q.stop {
                return None;
            }
            let urgent = q.jobs.iter().enumerate().filter(|(_, j)| j.urgent).max_by_key(|(_, j)| j.seq).map(|(i, _)| i);
            let pick = urgent.or_else(|| q.jobs.iter().enumerate().min_by_key(|(_, j)| j.seq).map(|(i, _)| i));
            if let Some(i) = pick {
                let job = q.jobs.swap_remove(i);
                q.running.insert((job.conn, job.id));
                let depth = q.jobs.len();
                return Some((Work::Render(Box::new(job)), depth));
            }
            if let Some(r) = q.refines.pop_front() {
                q.running.insert((r.conn, r.id));
                return Some((Work::Refine(r), 0));
            }
            q = self.cv.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Work on (`conn`, `id`) ended; whether it was cancelled meanwhile.
    fn done(&self, conn: u64, id: u64) -> bool {
        let mut q = lock(&self.q);
        q.running.remove(&(conn, id));
        q.cancelled.remove(&(conn, id))
    }
}

fn worker(sched: &Sched, shared: &Shared) {
    while let Some((work, depth)) = sched.next() {
        match work {
            Work::Render(job) => {
                let (conn, id) = (job.conn, job.id);
                let out = render_job(*job, depth, sched, shared);
                if !sched.done(conn, id)
                    && let Some((out, down, payload)) = out
                {
                    let _ = out.send((down, payload));
                }
            }
            Work::Refine(r) => {
                let (conn, id) = (r.conn, r.id);
                let out = refine(r, shared);
                if !sched.done(conn, id)
                    && let Some((out, down, payload)) = out
                {
                    let _ = out.send((down, payload));
                }
            }
        }
    }
}

type Send3 = (Out, Down, Vec<u8>);

fn failed(out: &Out, id: u64, error: String, stale: bool) -> Option<Send3> {
    Some((out.clone(), Down::Reply { reply: FrameReply::Failed { id, error, stale }, pixels: None }, vec![]))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Render, check the revision again, encode.
fn render_job(job: Job, depth: usize, sched: &Sched, shared: &Shared) -> Option<Send3> {
    let Job { conn, id, revision, project, comp, time, opts, want, out, current, queued, .. } = job;
    let stale = |now: u64| format!("the replica is at {now}, the request at {revision}");
    if current.load(Ordering::Acquire) != revision {
        return failed(&out, id, stale(current.load(Ordering::Acquire)), true);
    }
    let wait = queued.elapsed();
    let t0 = Instant::now();
    let times = LayerTimes::default();
    let rendered = shared.render.render(&RenderJob { project: &project, comp, time, opts, cache: &shared.cache, times: Some(&times) });
    let (frame, gpu) = match rendered {
        Ok(r) => r,
        Err(e) => {
            shared.log(&format!("frame {conn}/{id}: {e}"));
            return failed(&out, id, e, false);
        }
    };
    let render_ms = ms(t0.elapsed());
    if current.load(Ordering::Acquire) != revision {
        return failed(&out, id, stale(current.load(Ordering::Acquire)), true);
    }
    let t1 = Instant::now();
    let (width, height, rgba) = match frame {
        Rendered::Image(img) => (img.width, img.height, remote_encode::rgba8_premultiplied_par(&img)),
        Rendered::Rgba8 { width, height, bytes } => (width, height, bytes),
    };
    let convert_ms = ms(t1.elapsed());
    let t1 = Instant::now();
    let (w, h, rgba) = match want.max_size {
        Some(max) => remote_encode::fit(rgba, width, height, max),
        None => (width, height, rgba),
    };
    let (pixels, payload) = match remote_encode::encode(&rgba, w, h, want.codec, want.quality) {
        Ok(p) => p,
        Err(e) => return failed(&out, id, e, false),
    };
    let encode_ms = ms(t1.elapsed());
    shared.log(&format!(
        "frame {conn}/{id} rev {revision} {w}×{h}{} {} render {render_ms:.1} ms convert {convert_ms:.1} ms encode {encode_ms:.1} ms {:?} {} KB wait {:.1} ms queue {depth}",
        if (w, h) != (width, height) { format!(" (of {width}×{height})") } else { String::new() },
        if gpu { "gpu" } else { "cpu" },
        pixels.codec,
        payload.len() / 1024,
        ms(wait),
    ));
    let layer_times = times.into_map().into_iter().map(|((c, l), ms)| (c, l, ms as f32)).collect();
    let reply = FrameReply::Frame { id, width: w, height: h, ms: render_ms, passes: 1, gpu, layer_misses: vec![], layer_hits: vec![], layer_times };
    if want.refine && want.codec == Codec::Jpeg {
        sched.refine(Refine { conn, id, revision, reply: reply.clone(), width: w, height: h, rgba, out: out.clone(), current });
    }
    Some((out, Down::Reply { reply, pixels: Some(pixels) }, payload))
}

/// The lossless version of a frame sent lossy (only while the replica is still at its revision).
fn refine(r: Refine, shared: &Shared) -> Option<Send3> {
    if r.current.load(Ordering::Acquire) != r.revision {
        return None;
    }
    let t0 = Instant::now();
    let (mut pixels, payload) = remote_encode::encode(&r.rgba, r.width, r.height, Codec::Qoi, 0).ok()?;
    pixels.refine = true;
    shared.log(&format!("refine {}/{} {}×{} encode {:.1} ms qoi {} KB", r.conn, r.id, r.width, r.height, ms(t0.elapsed()), payload.len() / 1024));
    Some((r.out, Down::Reply { reply: r.reply, pixels: Some(pixels) }, payload))
}

/// One client: handshake, then requests until it goes.
fn connection(stream: TcpStream, sched: &Arc<Sched>, shared: &Arc<Shared>) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let Ok(mut reader) = stream.try_clone() else { return };
    let mut writer = stream;
    writer.set_nodelay(true).ok();
    reader.set_read_timeout(Some(HELLO_TIMEOUT)).ok();
    let client = match read_frame::<Up>(&mut reader, MAX_HELLO) {
        Ok((Up::Hello { token, client, .. }, _)) => match shared.auth(&token) {
            Ok(()) => client,
            Err(error) => {
                shared.log(&format!("refused {peer}: {error}"));
                let _ = write_frame(&mut writer, &Down::Refused { error }, &[]);
                return;
            }
        },
        Ok(_) => {
            let _ = write_frame(&mut writer, &Down::Refused { error: "say hello first".into() }, &[]);
            return;
        }
        Err(e) => {
            shared.log(&format!("refused {peer}: {e}"));
            let _ = write_frame(&mut writer, &Down::Refused { error: format!("no hello ({e})") }, &[]);
            return;
        }
    };
    reader.set_read_timeout(None).ok();
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let mut replica = FrameServer::new(Arc::new(NoFootage), None);
    let current = Arc::new(AtomicU64::new(NO_REVISION));
    if let Some(p) = &shared.preload {
        let _ = replica.handle(FrameMsg::Project { revision: 0, project: p.as_str().to_string() });
        current.store(replica.revision().unwrap_or(NO_REVISION), Ordering::Release);
    }
    let welcome = Down::Welcome {
        version: VERSION,
        server: format!("effectcraft-cli {}", env!("CARGO_PKG_VERSION")),
        gpu: shared.render.gpu_name(),
        threads: u32::try_from(shared.threads).unwrap_or(u32::MAX),
        revision: replica.revision(),
    };
    if write_frame(&mut writer, &welcome, &[]).is_err() {
        return;
    }
    shared.log(&format!("connection {conn} from {peer} ({client})"));
    let (out, out_rx) = channel::<(Down, Vec<u8>)>();
    let mut streams = Streams::new(
        Arc::new(StreamHost { render: shared.render.clone(), cache: shared.cache.clone(), log: shared.log.clone(), render_threads: shared.threads }),
        out.clone(),
        conn,
    );
    streams.synced(replica.revision().zip(replica.project().cloned()));
    let writer_thread = std::thread::Builder::new()
        .name("ec-frames-write".into())
        .spawn(move || {
            for (down, payload) in out_rx {
                if write_frame(&mut writer, &down, &payload).is_err() {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
        })
        .ok();
    let why = loop {
        let (up, payload) = match read_frame::<Up>(&mut reader, MAX_PART) {
            Ok(m) => m,
            Err(e) => break e.to_string(),
        };
        match up {
            Up::Hello { .. } => {}
            Up::Echo { id } => {
                let _ = out.send((Down::Echo { id }, payload));
            }
            Up::Cancel { ids } => sched.cancel(conn, &ids),
            Up::Control { id, method, params } => {
                let (to, control) = (out.clone(), shared.control.clone());
                let spawned = std::thread::Builder::new().name("ec-frames-control".into()).spawn(move || {
                    let reply = match control {
                        Some(c) => c(&method, params),
                        None => json!({"ok": false, "error": "this frame server has no control host"}),
                    };
                    let _ = to.send((Down::Control { id, reply }, vec![]));
                });
                if spawned.is_err() {
                    let _ = out.send((Down::Control { id, reply: json!({"ok": false, "error": "cannot start the request"}) }, vec![]));
                }
            }
            Up::Msg { msg: FrameMsg::Stream(m), .. } => streams.message(m),
            Up::Msg { msg, want, urgent } => {
                let synced = matches!(msg, FrameMsg::Project { .. } | FrameMsg::Patch(_));
                message(conn, msg, want, urgent, &mut replica, &current, &out, sched, shared);
                if synced {
                    streams.synced(replica.revision().zip(replica.project().cloned()));
                }
            }
        }
    };
    drop(streams);
    sched.close(conn);
    current.store(NO_REVISION, Ordering::Release);
    drop(out);
    if let Some(t) = writer_thread {
        let _ = t.join();
    }
    shared.log(&format!("connection {conn} closed: {why}"));
}

#[allow(clippy::too_many_arguments)]
fn message(conn: u64, msg: FrameMsg, want: Want, urgent: bool, replica: &mut FrameServer, current: &Arc<AtomicU64>, out: &Out, sched: &Sched, shared: &Shared) {
    match msg {
        FrameMsg::Render { id, revision, comp, time, opts, .. } => {
            let fail = |error: String, stale: bool| {
                let _ = out.send((Down::Reply { reply: FrameReply::Failed { id, error, stale }, pixels: None }, vec![]));
            };
            if replica.revision() != Some(revision) {
                return fail(format!("the replica is at {:?}, not {revision}", replica.revision()), true);
            }
            let Some(project) = replica.project().cloned() else { return fail("no project".into(), true) };
            if project.comp(comp).is_none() {
                return fail(format!("no composition {}", comp.0), false);
            }
            let job = Job {
                conn,
                id,
                revision,
                urgent,
                seq: 0,
                project,
                comp,
                time,
                opts: *opts,
                want,
                out: out.clone(),
                current: Arc::clone(current),
                queued: Instant::now(),
            };
            let depth = sched.push(job);
            if depth > shared.threads * 4 {
                shared.log(&format!("connection {conn}: {depth} frames queued"));
            }
        }
        FrameMsg::Purge => shared.cache.clear(),
        sync => {
            let t0 = Instant::now();
            let kind = match &sync {
                FrameMsg::Project { project, .. } => format!("project {} KB", project.len() / 1024),
                FrameMsg::Patch(p) => format!("patch {} ops", p.ops.len()),
                _ => "segmentations".into(),
            };
            for (reply, _) in replica.handle(sync) {
                let _ = out.send((Down::Reply { reply, pixels: None }, vec![]));
            }
            current.store(replica.revision().unwrap_or(NO_REVISION), Ordering::Release);
            shared.log(&format!("connection {conn}: {kind} → revision {:?} in {:.1} ms", replica.revision(), ms(t0.elapsed())));
        }
    }
}

#[cfg(test)]
#[path = "frame_server_tests.rs"]
mod tests;
