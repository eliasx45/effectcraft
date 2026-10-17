//! The frame server's playback stream (`plan/cloud-render.md` CR6a; the protocol is
//! [`effectcraft_engine::remote_stream`]): render ahead of the app's playhead in play order,
//! encode each chunk of frames as one self-contained H.264 GOP on a CPU pool, send the chunks over
//! the connection.
//!
//! Per stream, one controller thread decides which chunk comes next and at which resolution, and
//! render workers take that chunk's frames (the oldest chunk's first, so chunks finish in order;
//! the GPU renderer caps how many render on the card at once). A chunk whose frames are all in goes
//! to the encode pool; its encoder runs on more threads while the app's lead is short (a chunk's
//! latency) and on fewer when it's ahead (throughput).
//!
//! - **Adaptive resolution.** Each resolution level (full, half, quarter) keeps a running estimate
//!   of how long one chunk takes from start to sent. A new chunk takes the largest level whose
//!   estimate fits in the app's lead (what it has beyond its playhead) with a margin; when the
//!   window is full, chunks still ahead that went out below full size are sent again at full size.
//! - **Edits.** After a sync, [`effectcraft_engine::remote_stream::first_change`] says from which
//!   frame the stream comp can look different; the chunks sent from there on are forgotten (and
//!   rendered again in play order), chunks in flight that overlap it are dropped, and the app gets
//!   [`StreamReply::Stale`].
//! - Nothing here panics or blocks the connection: a failed frame fails its chunk (logged; it is
//!   tried again), a stopped stream's threads end on their own.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use effectcraft_engine::project::{ItemId, Project};
use effectcraft_engine::remote::FrameReply;
use effectcraft_engine::remote_encode;
use effectcraft_engine::remote_stream::{self as rs, StreamCodec, StreamMsg, StreamReply};
use effectcraft_engine::remote_wire::Down;
use effectcraft_engine::render::{LayerCache, RenderOpts};
use effectcraft_engine::time::{FrameRate, Tick};
use effectcraft_media::preview_stream::{ChunkSpec, encode_chunk, even};

use crate::control_wire::lock;
use crate::frame_server::{FrameRender, LogFn, Out, RenderJob, Rendered};

/// Chunks rendering or encoding at once per stream (encoding a chunk takes longer than
/// rendering it on a busy CPU, so several encode at once).
const MAX_IN_FLIGHT: usize = 6;
/// Resolution levels: full, half, quarter of the asked size.
const LEVELS: [f32; 3] = [1.0, 0.5, 0.25];
/// Lead kept on top of a level's chunk latency before choosing it (network, decode).
const MARGIN_SECS: f64 = 0.35;
/// Below this lead (seconds) a playing stream lowers its resolution; above the high one it raises it.
const LOW_LEAD_SECS: f64 = 1.0;
const HIGH_LEAD_SECS: f64 = 2.5;
/// Chunks of a new stream sent below full size (the warm-up).
const WARM_CHUNKS: usize = MAX_IN_FLIGHT;
/// Most seconds added to the render-ahead window for a slow pipeline.
const MAX_EXTRA_AHEAD_SECS: f64 = 8.0;
/// Least time between two resolution changes.
const LEVEL_HOLD: Duration = Duration::from_millis(1200);
/// How often the controller looks again without being woken.
const TICK: Duration = Duration::from_millis(40);
/// Encoder threads for a chunk the app waits for, and for one sent ahead.
/// (One thread per encoder is the cheapest per frame; several chunks encode at once.)
const URGENT_ENCODE_THREADS: usize = 6;
const AHEAD_ENCODE_THREADS: usize = 1;

/// What every stream of a server shares.
pub(crate) struct StreamHost {
    pub render: Arc<dyn FrameRender>,
    pub cache: Arc<LayerCache>,
    pub log: LogFn,
    /// Render workers per stream.
    pub render_threads: usize,
}

/// The server's encode pool: up to 32 threads (the rest of the cores render and serve).
fn encode_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 32);
        rayon::ThreadPoolBuilder::new().num_threads(n).thread_name(|i| format!("ec-stream-enc-{i}")).build().ok()
    })
    .as_ref()
}

/// One connection's streams.
pub(crate) struct Streams {
    host: Arc<StreamHost>,
    out: Out,
    conn: u64,
    live: HashMap<u64, Arc<Shared>>,
    /// The replica: its revision and project.
    replica: Option<(u64, Arc<Project>)>,
}

impl Streams {
    pub(crate) fn new(host: Arc<StreamHost>, out: Out, conn: u64) -> Streams {
        Streams { host, out, conn, live: HashMap::new(), replica: None }
    }

    fn reply(&self, r: StreamReply) {
        let _ = self.out.send((Down::Reply { reply: FrameReply::Stream(r), pixels: None }, vec![]));
    }

    /// The replica moved to `revision` (after a project or a patch; `None`: it has none).
    pub(crate) fn synced(&mut self, replica: Option<(u64, Arc<Project>)>) {
        let Some((revision, project)) = replica.clone() else {
            self.replica = None;
            return;
        };
        self.replica = replica;
        for s in self.live.values() {
            if let Some(stale) = s.edit(revision, &project) {
                self.reply(stale);
            }
        }
    }

    pub(crate) fn message(&mut self, m: StreamMsg) {
        let id = m.stream();
        match m {
            StreamMsg::Start { stream, revision, comp, from, to, loop_from, looping, opts, codec, crf, chunk_frames, ahead_secs, adaptive, max_size } => {
                if let Some(old) = self.live.remove(&stream) {
                    old.stop();
                }
                let fail = |error: String| StreamReply::Failed { stream, error };
                let Some((rev, project)) = self.replica.clone() else { return self.reply(fail("no project".into())) };
                if rev != revision {
                    return self.reply(fail(format!("the replica is at {rev}, not {revision}")));
                }
                let Some(c) = project.comp(comp) else { return self.reply(fail(format!("no composition {}", comp.0))) };
                let rate = c.frame_rate;
                let n = chunk_frames.clamp(1, rs::MAX_CHUNK_FRAMES);
                let hi = rate.frame_at(to.unwrap_or(c.work_area.1).min(c.duration)).max(1);
                let lo = 0;
                let loop_lo = rate.frame_at(loop_from.unwrap_or(c.work_area.0)).clamp(0, hi - 1);
                let start = rate.frame_at(from).clamp(lo, hi - 1);
                let scale = if opts.scale > 0.0 { opts.scale } else { 1.0 };
                let mut full = ((f64::from(c.width) * scale).round().max(2.0) as u32, (f64::from(c.height) * scale).round().max(2.0) as u32);
                if let Some(m) = max_size {
                    full = remote_encode::fit_size(full.0, full.1, m);
                }
                let full = (even(full.0), even(full.1));
                let ahead = (f64::from(ahead_secs.clamp(0.5, 60.0)) * rate.as_f64()).ceil() as i64;
                let p = Params {
                    comp,
                    lo,
                    hi,
                    loop_lo,
                    looping,
                    opts: *opts,
                    codec,
                    crf,
                    n,
                    anchor: start,
                    ahead,
                    base_ahead: ahead,
                    adaptive,
                    full,
                    rate,
                    bg: c.background,
                };
                let shared = Arc::new(Shared::new(self.host.clone(), self.out.clone(), self.conn, stream, p, revision, project, start));
                if let Err(e) = shared.spawn() {
                    return self.reply(fail(e));
                }
                self.reply(StreamReply::Started { stream, width: full.0, height: full.1, frame_rate: rate, chunk_frames: n, codec });
                self.live.insert(stream, shared);
            }
            StreamMsg::Stop { .. } => {
                if let Some(s) = self.live.remove(&id) {
                    s.stop();
                }
            }
            StreamMsg::Seek { time, .. } => {
                if let Some(s) = self.live.get(&id) {
                    s.seek(time);
                }
            }
            StreamMsg::Playhead { time, playing, received, .. } => {
                if let Some(s) = self.live.get(&id) {
                    s.playhead(time, playing, received);
                }
            }
            StreamMsg::Invalidate { from, .. } => {
                if let Some(s) = self.live.get(&id)
                    && let Some(stale) = s.invalidate(from)
                {
                    self.reply(stale);
                }
            }
        }
    }

    /// The connection closed: stop every stream.
    pub(crate) fn close(&mut self) {
        for (_, s) in self.live.drain() {
            s.stop();
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.close();
    }
}

/// A stream's fixed settings.
#[derive(Clone)]
struct Params {
    comp: ItemId,
    /// Frames played: `[lo, hi)`; a loop goes back to `loop_lo`.
    lo: i64,
    hi: i64,
    loop_lo: i64,
    looping: bool,
    opts: RenderOpts,
    codec: StreamCodec,
    crf: f32,
    n: u32,
    /// The chunk grid's frame 0 (where playback started or last jumped to).
    anchor: i64,
    /// Frames kept ahead of the playhead: `base_ahead` (Start's `aheadSecs`) plus what a chunk
    /// takes from start to sent now (so the lead stays near `base_ahead` on a slow machine).
    ahead: i64,
    base_ahead: i64,
    adaptive: bool,
    /// The full-size frame (even).
    full: (u32, u32),
    rate: FrameRate,
    bg: [f32; 3],
}

impl Params {
    fn fps(&self) -> f64 {
        self.rate.as_f64().max(1.0)
    }
    /// The chunk's frames inside the played range.
    fn frames(&self, index: i64) -> Option<(i64, u32)> {
        rs::chunk_frames(index, self.n, self.anchor, self.lo, self.hi)
    }
    fn chunk_of(&self, frame: i64) -> i64 {
        rs::chunk_of(frame, self.n, self.anchor)
    }
    fn last_chunk(&self) -> i64 {
        self.chunk_of(self.hi - 1)
    }
    /// Chunks covering `ahead` frames from `frame`, in play order (wrapping when looping).
    fn window(&self, frame: i64) -> Vec<i64> {
        let mut out = vec![];
        let (mut f, mut left) = (frame.clamp(self.lo, self.hi - 1), self.ahead.max(1));
        for _ in 0..4096 {
            if left <= 0 {
                break;
            }
            let c = self.chunk_of(f);
            if !out.contains(&c) {
                out.push(c);
            }
            let Some((first, count)) = self.frames(c) else { break };
            let end = first + i64::from(count);
            left -= end - f;
            f = end;
            if f >= self.hi {
                if !self.looping {
                    break;
                }
                f = self.loop_lo;
            }
        }
        out
    }
}

/// A chunk that was sent: its resolution level.
#[derive(Clone, Copy, Debug)]
struct Sent {
    level: usize,
}

/// A chunk rendering or encoding.
struct Flight {
    epoch: u64,
}

/// One frame to render. (The state lock is never held while a chunk's lock is taken.)
struct FrameJob {
    chunk: Arc<Mutex<Chunk>>,
    index: i64,
    level: usize,
    project: Arc<Project>,
    slot: usize,
    time: Tick,
}

/// A chunk's frames as they come in.
struct Chunk {
    index: i64,
    epoch: u64,
    revision: u64,
    /// The edit count when it started (edits after it may overlap it).
    mark: u64,
    level: usize,
    started: Instant,
    frames: Vec<Option<(u32, u32, Vec<u8>)>>,
    left: usize,
    /// Why a frame of it failed.
    failed: Option<String>,
    /// The lead when it started (seconds): encode with more threads when short.
    lead: f64,
}

struct State {
    stop: bool,
    p: Params,
    project: Arc<Project>,
    revision: u64,
    /// Bumps on a seek or a restart: older work is dropped.
    epoch: u64,
    /// The app's playhead (comp frame) at `at`, and whether it plays.
    playhead: i64,
    playing: bool,
    at: Instant,
    /// The newest chunk sequence number the app has.
    received: Option<u64>,
    sent: BTreeMap<i64, Sent>,
    flying: BTreeMap<i64, Flight>,
    /// Render queue: (issue order, frame slot) → job.
    jobs: BTreeMap<(u64, usize), FrameJob>,
    issued: u64,
    /// Edits so far, and the first frame each can change.
    edits: Vec<(u64, i64)>,
    seq: u64,
    /// Seconds from a chunk's start to sent, per level (running average).
    latency: [Option<f64>; 3],
    /// The resolution level new chunks get, and when it last changed.
    cur_level: usize,
    level_at: Instant,
    ended: Option<(u64, u64)>,
}

impl State {
    /// The playhead now (extrapolated while playing).
    fn now_frame(&self) -> i64 {
        let p = &self.p;
        if !self.playing {
            return self.playhead;
        }
        let ahead = (self.at.elapsed().as_secs_f64() * p.fps()) as i64;
        let mut f = self.playhead.saturating_add(ahead);
        if f >= p.hi {
            f = if p.looping { p.loop_lo + (f - p.hi) % (p.hi - p.loop_lo).max(1) } else { p.hi - 1 };
        }
        f
    }

    /// Seconds of frames the app has (sent) beyond its playhead, along the play order.
    fn lead(&self) -> f64 {
        let now = self.now_frame();
        let mut have = 0i64;
        for c in self.p.window(now) {
            if !self.sent.contains_key(&c) {
                break;
            }
            let Some((first, count)) = self.p.frames(c) else { break };
            have += first + i64::from(count) - now.max(first);
        }
        have as f64 / self.p.fps()
    }

    /// The level for a new chunk with `lead` seconds in hand. The first chunks of a stream (the
    /// warm-up: the app waits for them, and caches are cold) go at quarter, then half size. While playing, a short lead lowers the resolution one level and a long one raises
    /// it, each at most once per [`LEVEL_HOLD`] (so a level gets a few chunks to show what it
    /// does); chunks sent below full size go again at full size when there is time. Before
    /// playback after the warm-up, and without `adaptive`: full size.
    fn level(&mut self, lead: f64) -> usize {
        if !self.p.adaptive {
            return 0;
        }
        // The very first chunk at quarter size, the rest of the first burst at half.
        if self.issued < WARM_CHUNKS as u64 {
            return if self.issued == 0 { 2 } else { 1 };
        }
        if !self.playing {
            return 0;
        }
        let held = self.level_at.elapsed() >= LEVEL_HOLD;
        if lead < LOW_LEAD_SECS && self.cur_level + 1 < LEVELS.len() && held {
            self.cur_level += 1;
            self.level_at = Instant::now();
        } else if lead > HIGH_LEAD_SECS && self.cur_level > 0 && held {
            self.cur_level -= 1;
            self.level_at = Instant::now();
        }
        self.cur_level
    }
}

struct Shared {
    host: Arc<StreamHost>,
    out: Out,
    conn: u64,
    id: u64,
    state: Mutex<State>,
    /// Wakes the controller.
    cv: Condvar,
    /// Wakes the render workers.
    work: Condvar,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    fn new(host: Arc<StreamHost>, out: Out, conn: u64, id: u64, p: Params, revision: u64, project: Arc<Project>, start: i64) -> Shared {
        let state = State {
            stop: false,
            p,
            project,
            revision,
            epoch: 0,
            playhead: start,
            playing: false,
            at: Instant::now(),
            received: None,
            sent: BTreeMap::new(),
            flying: BTreeMap::new(),
            jobs: BTreeMap::new(),
            issued: 0,
            edits: vec![],
            seq: 0,
            latency: [None; 3],
            // Playback starts on the warm-up's half-size chunks.
            cur_level: 1,
            level_at: Instant::now(),
            ended: None,
        };
        Shared { host, out, conn, id, state: Mutex::new(state), cv: Condvar::new(), work: Condvar::new() }
    }

    fn log(&self, s: &str) {
        (self.host.log)(&format!("stream {}/{}: {s}", self.conn, self.id));
    }

    fn reply(&self, r: StreamReply, payload: Vec<u8>) {
        let _ = self.out.send((Down::Reply { reply: FrameReply::Stream(r), pixels: None }, payload));
    }

    fn spawn(self: &Arc<Self>) -> Result<(), String> {
        let me = Arc::clone(self);
        std::thread::Builder::new().name("ec-stream".into()).spawn(move || me.control()).map_err(|e| e.to_string())?;
        for i in 0..self.host.render_threads.max(1) {
            let me = Arc::clone(self);
            std::thread::Builder::new().name(format!("ec-stream-r{i}")).spawn(move || me.render_worker()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn stop(&self) {
        lock(&self.state).stop = true;
        self.cv.notify_all();
        self.work.notify_all();
    }

    fn seek(&self, time: Tick) {
        let mut st = lock(&self.state);
        let f = st.p.rate.frame_at(time).clamp(st.p.lo, st.p.hi - 1);
        st.playhead = f;
        st.at = Instant::now();
        st.epoch += 1;
        // A new grid from there: chunks of the old one are sent again when needed.
        st.p.anchor = f;
        st.sent.clear();
        st.flying.clear();
        st.jobs.clear();
        st.ended = None;
        drop(st);
        self.cv.notify_all();
    }

    fn playhead(&self, time: Tick, playing: bool, received: Option<u64>) {
        let mut st = lock(&self.state);
        st.playhead = st.p.rate.frame_at(time).clamp(st.p.lo, st.p.hi - 1);
        st.playing = playing;
        st.at = Instant::now();
        if received.is_some() {
            st.received = received;
        }
        drop(st);
        self.cv.notify_all();
    }

    /// Frames from `from_frame` on are out of date: forget them and tell the app.
    fn forget_from(&self, st: &mut State, from_frame: i64) {
        let mark = st.edits.len() as u64 + 1;
        st.edits.push((mark, from_frame));
        let p = st.p.clone();
        let ends_after = |c: &i64| p.frames(*c).is_some_and(|(first, count)| first + i64::from(count) > from_frame);
        st.sent.retain(|c, _| !ends_after(c));
        st.flying.retain(|c, _| !ends_after(c));
        st.jobs.retain(|_, j| !ends_after(&j.index));
        st.ended = None;
    }

    /// The replica moved on: drop what the edit changed; the `Stale` to send, if any.
    fn edit(&self, revision: u64, project: &Arc<Project>) -> Option<StreamReply> {
        let mut st = lock(&self.state);
        if st.revision == revision {
            return None;
        }
        let from = rs::first_change(&st.project, project, st.p.comp);
        st.project = project.clone();
        st.revision = revision;
        let from = from?;
        let f = st.p.rate.frame_at(from).max(st.p.lo);
        self.forget_from(&mut st, f);
        drop(st);
        self.log(&format!("edit → revision {revision}: frames from {f} again"));
        self.cv.notify_all();
        Some(StreamReply::Stale { stream: self.id, revision, from })
    }

    fn invalidate(&self, from: Tick) -> Option<StreamReply> {
        let mut st = lock(&self.state);
        let f = st.p.rate.frame_at(from).max(st.p.lo);
        self.forget_from(&mut st, f);
        let revision = st.revision;
        drop(st);
        self.cv.notify_all();
        Some(StreamReply::Stale { stream: self.id, revision, from })
    }

    /// The controller: start the next chunk while fewer than [`MAX_IN_FLIGHT`] are in flight.
    fn control(self: Arc<Self>) {
        let (project, comp, time) = {
            let st = lock(&self.state);
            (st.project.clone(), st.p.comp, st.p.rate.tick_of(st.playhead))
        };
        let t0 = Instant::now();
        self.host.render.warm(&project, comp, time);
        self.log(&format!("warmed up in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0));
        let mut st = lock(&self.state);
        let mut said = Instant::now();
        loop {
            if st.stop {
                return;
            }
            let now = st.now_frame();
            let pipeline = st.latency[st.cur_level.min(LEVELS.len() - 1)].unwrap_or(1.0).clamp(0.0, MAX_EXTRA_AHEAD_SECS);
            st.p.ahead = st.p.base_ahead + (pipeline * st.p.fps()).ceil() as i64;
            let window = st.p.window(now);
            // Chunks the playhead has passed while they rendered: not wanted any more.
            if st.playing {
                let p = st.p.clone();
                let passed = |c: &i64| !window.contains(c) && p.frames(*c).is_some_and(|(f, n)| f + i64::from(n) <= now);
                let gone: Vec<i64> = st.flying.keys().copied().filter(|c| passed(c)).collect();
                if !gone.is_empty() {
                    st.flying.retain(|c, _| !gone.contains(c));
                    st.jobs.retain(|_, j| !gone.contains(&j.index));
                }
            }
            if said.elapsed() >= Duration::from_secs(2) {
                said = Instant::now();
                let flying: Vec<i64> = st.flying.keys().copied().collect();
                let head: Vec<i64> = window.iter().take(4).copied().collect();
                self.log(&format!(
                    "playhead {} (now {now}, {}), lead {:.2} s, level {}, in flight {flying:?}, {} frames queued, window {head:?}…",
                    st.playhead,
                    if st.playing { "playing" } else { "paused" },
                    st.lead(),
                    st.cur_level,
                    st.jobs.len()
                ));
            }
            if st.flying.len() < MAX_IN_FLIGHT {
                let lead = st.lead();
                let missing = window.iter().copied().find(|c| !st.sent.contains_key(c) && !st.flying.contains_key(c));
                let next = match missing {
                    Some(c) => Some((c, st.level(lead))),
                    // The window is full: send chunks below full size again at full size, if
                    // there is time for it before the app plays them (paused: always).
                    None if st.flying.is_empty() => {
                        let full = st.latency[0].unwrap_or(1.0) + MARGIN_SECS;
                        let fps = st.p.fps();
                        window
                            .iter()
                            .copied()
                            .find(|c| {
                                st.sent.get(c).is_some_and(|s| s.level > 0)
                                    && st.p.frames(*c).is_some_and(|(first, _)| {
                                        let until = if first >= now { first - now } else { first + (st.p.hi - now) };
                                        !st.playing || until as f64 / fps > full
                                    })
                            })
                            .map(|c| (c, 0))
                    }
                    None => None,
                };
                if let Some((c, level)) = next {
                    self.issue(&mut st, c, level, lead);
                    continue;
                }
            }
            // Everything to the end is sent: say so once.
            if !st.p.looping && st.ended != Some((st.epoch, st.revision)) {
                let last = st.p.last_chunk();
                let from = st.p.chunk_of(now);
                if (from..=last).all(|c| st.sent.contains_key(&c)) && st.flying.is_empty() {
                    st.ended = Some((st.epoch, st.revision));
                    let at = st.p.rate.tick_of(st.p.hi);
                    self.reply(StreamReply::End { stream: self.id, at }, vec![]);
                }
            }
            st = self.cv.wait_timeout(st, TICK).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        }
    }

    /// Queue chunk `c`'s frames at `level`.
    fn issue(&self, st: &mut State, c: i64, level: usize, lead: f64) {
        let Some((first, count)) = st.p.frames(c) else { return };
        st.issued += 1;
        let order = st.issued;
        let chunk = Arc::new(Mutex::new(Chunk {
            index: c,
            epoch: st.epoch,
            revision: st.revision,
            mark: st.edits.len() as u64,
            level,
            started: Instant::now(),
            frames: (0..count).map(|_| None).collect(),
            left: count as usize,
            failed: None,
            lead,
        }));
        for slot in 0..count as usize {
            let time = st.p.rate.tick_of(first + slot as i64);
            st.jobs.insert((order, slot), FrameJob { chunk: Arc::clone(&chunk), index: c, level, project: st.project.clone(), slot, time });
        }
        st.flying.insert(c, Flight { epoch: st.epoch });
        self.work.notify_all();
    }

    /// Whether `chunk` (of `epoch`, started at edit `mark`) is still wanted.
    fn current(st: &State, index: i64, epoch: u64, mark: u64) -> bool {
        let p = &st.p;
        let overlapped = st.edits.iter().any(|(m, from)| *m > mark && p.frames(index).is_some_and(|(f, n)| f + i64::from(n) > *from));
        !st.stop && st.epoch == epoch && st.flying.get(&index).is_some_and(|f| f.epoch == epoch) && !overlapped
    }

    fn render_worker(self: Arc<Self>) {
        loop {
            let (job, opts, comp, scale) = {
                let mut st = lock(&self.state);
                loop {
                    if st.stop {
                        return;
                    }
                    if let Some((_, j)) = st.jobs.pop_first() {
                        let scale = LEVELS.get(j.level).copied().unwrap_or(1.0);
                        break (j, st.p.opts, st.p.comp, scale);
                    }
                    st = self.work.wait_timeout(st, Duration::from_millis(200)).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
                }
            };
            let project = job.project.clone();
            let base = if opts.scale > 0.0 { opts.scale } else { 1.0 };
            let opts = RenderOpts { scale: base * f64::from(scale), ..opts };
            let rendered = self.host.render.render(&RenderJob { project: &project, comp, time: job.time, opts, cache: &self.host.cache, times: None });
            let frame = match rendered {
                Ok((Rendered::Image(img), _)) => Ok((img.width, img.height, remote_encode::rgba8_premultiplied_par(&img))),
                Ok((Rendered::Rgba8 { width, height, bytes }, _)) => Ok((width, height, bytes)),
                Err(e) => {
                    self.log(&format!("frame at {:.3} s: {e}", job.time.seconds()));
                    Err(e)
                }
            };
            let complete = {
                let mut c = lock(&job.chunk);
                match frame {
                    Ok(f) => {
                        if let Some(s) = c.frames.get_mut(job.slot) {
                            *s = Some(f);
                        }
                    }
                    Err(e) => c.failed = Some(e),
                }
                c.left = c.left.saturating_sub(1);
                c.left == 0
            };
            if complete {
                let me = Arc::clone(&self);
                let chunk = Arc::clone(&job.chunk);
                match encode_pool() {
                    Some(pool) => pool.spawn(move || me.finish(&chunk)),
                    None => me.finish(&chunk),
                }
            }
        }
    }

    /// Encode a rendered chunk and send it, unless it is no longer wanted.
    fn finish(&self, chunk: &Mutex<Chunk>) {
        let (index, epoch, mark, level, revision, failed, render_ms, lead, frames) = {
            let mut c = lock(chunk);
            let frames: Vec<(u32, u32, Vec<u8>)> = c.frames.iter_mut().filter_map(Option::take).collect();
            (c.index, c.epoch, c.mark, c.level, c.revision, c.failed.take(), c.started.elapsed().as_secs_f64() * 1000.0, c.lead, frames)
        };
        let p = {
            let mut st = lock(&self.state);
            if !Self::current(&st, index, epoch, mark) {
                return;
            }
            if let Some(error) = failed {
                // A frame that can't render (missing footage or fonts, a renderer error) would
                // fail again: the stream ends, and the app renders on its own.
                st.flying.remove(&index);
                st.stop = true;
                self.reply(StreamReply::Failed { stream: self.id, error: format!("a frame of chunk {index} failed: {error}") }, vec![]);
                drop(st);
                self.cv.notify_all();
                self.work.notify_all();
                return;
            }
            st.p.clone()
        };
        let t0 = Instant::now();
        let scale = LEVELS.get(level).copied().unwrap_or(1.0);
        let target = (even((p.full.0 as f32 * scale).round() as u32), even((p.full.1 as f32 * scale).round() as u32));
        // Frames at the target size (a renderer may round differently, or `maxSize` asks for less).
        let frames: Vec<(u32, u32, Vec<u8>)> = frames
            .into_iter()
            .map(|(w, h, px)| if w > target.0 || h > target.1 { remote_encode::fit(px, w, h, [target.0, target.1]) } else { (w, h, px) })
            .collect();
        let Some(&(w, h, _)) = frames.first().map(|f| (f.0, f.1, ())).as_ref() else { return };
        let (ew, eh) = (even(w), even(h));
        let threads = if lead < 1.0 { URGENT_ENCODE_THREADS } else { AHEAD_ENCODE_THREADS };
        let spec = ChunkSpec { crf: p.crf, threads, ..ChunkSpec::new(ew, eh, p.rate.num as u32, p.rate.den as u32) };
        let refs: Vec<&[u8]> = frames.iter().filter(|f| (f.0, f.1) == (w, h)).map(|f| f.2.as_slice()).collect();
        let encoded = encode_chunk(&refs, w, h, p.bg, &spec);
        let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = lock(&self.state);
        let bytes = match encoded {
            Ok(b) => b,
            Err(e) => {
                st.flying.remove(&index);
                drop(st);
                self.log(&format!("chunk {index}: {e}"));
                self.cv.notify_all();
                return;
            }
        };
        if !Self::current(&st, index, epoch, mark) {
            return;
        }
        st.flying.remove(&index);
        st.seq += 1;
        let seq = st.seq;
        st.sent.insert(index, Sent { level });
        let secs = (render_ms + encode_ms) / 1000.0;
        if let Some(e) = st.latency.get_mut(level) {
            *e = Some(e.map_or(secs, |old| old * 0.7 + secs * 0.3));
        }
        let first = p.frames(index).map_or(0, |f| f.0);
        let (lead_now, received) = (st.lead(), st.received);
        let frames_n = u32::try_from(refs.len()).unwrap_or(0);
        let kb = bytes.len() / 1024;
        let reply = StreamReply::Chunk {
            stream: self.id,
            seq,
            revision,
            index,
            first,
            start: p.rate.tick_of(first),
            frames: frames_n,
            frame_rate: p.rate,
            width: ew,
            height: eh,
            scale,
            codec: p.codec,
            render_ms,
            encode_ms,
        };
        // Out before the lock goes: the controller may send `End` as soon as it sees the chunk
        // sent, and that must not overtake the chunk.
        self.reply(reply, bytes);
        drop(st);
        self.cv.notify_all();
        self.log(&format!(
            "chunk {index} #{seq} frames {first}+{frames_n} {ew}×{eh} render {render_ms:.0} ms encode {encode_ms:.0} ms ({threads} threads) {kb} KB lead {lead_now:.2} s (app has #{})",
            received.map_or("–".to_string(), |r| r.to_string())
        ));
    }
}

#[cfg(test)]
#[path = "frame_stream_tests.rs"]
mod tests;
