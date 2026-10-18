//! CR6b (`plan/cloud-render.md` CR6): real-time playback from the cloud frame server's playback
//! stream (the server half is CR6a, `effectcraft_automation::frame_stream`; the protocol is
//! [`effectcraft_engine::remote_stream`]).
//!
//! While Preview On is the cloud and a preview plays, the app asks the server to *play* the comp
//! from the playhead instead of asking for frames one by one. The server renders ahead in play
//! order and sends self-contained H.264 chunks of 15 frames; they decode here on threads of
//! their own ([`StreamShared`]) into a [`StreamBuffer`], and every UI frame publishes what is new
//! into the RAM preview cache under the frame keys playback looks for. Playback itself is
//! unchanged: it waits at a frame that isn't cached (never drops), and the sound follows its
//! clock as for local frames.
//!
//! - **Adaptive resolution.** A chunk may come at half or quarter size while the lead is short;
//!   it is cached under the viewer's key all the same (the viewer scales it up) as a *draft*
//!   ([`crate::frames::streamed`]) and replaced when the full-size copy comes.
//! - **Kept on this Mac (CR6c).** When the preview stops, its frames stay in the RAM preview
//!   (drafts too; only stale ones go) and every current frame is written to this Mac's disk
//!   cache. Playing again only streams the frames this Mac doesn't hold: a stream plays one run
//!   of missing frames, and the frames on disk are read back on a thread of their own. The
//!   paused viewer still renders the exact frame on screen (CR5's path).
//! - **Edits while playing.** The edit goes to the server as a patch; it answers
//!   [`StreamReply::Stale`] from the first frame the edit can change. Frames from there on keep
//!   playing (marked stale) until the new revision's replace them, rather than holding playback
//!   while the server refills seconds of buffer.
//! - **Fallback.** A failed stream or a lost connection ends the stream; the preview goes on
//!   rendering here with a status note. Never a hang: playback only ever waits for frames, and
//!   the frames it waits for then render on this Mac.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use effectcraft_engine::project::{ItemId, Project};
use effectcraft_engine::render::RenderOpts;
use effectcraft_engine::time::{FrameRate, Tick};
use serde::Serialize;

use effectcraft_engine::render::disk_cache::DiskCache;

use crate::cloud::CloudFrames;
use crate::frames::streamed::{StreamedSink, is_draft_scale, read_back};
use crate::frames::{FrameKey, Frames, opts_hash};

/// A frame [`StreamBuffer::publish`] puts in the RAM cache.
pub struct Published {
    pub key: FrameKey,
    pub img: Arc<egui::ColorImage>,
    /// Its size relative to the viewer's.
    pub scale: f32,
    /// Cached as a draft: below full size, stale, or of an older revision than the key's.
    pub draft: bool,
    /// Rendered at the key's revision and not stale: it goes to the disk cache.
    pub current: bool,
}

/// Frames per chunk asked for (≈ 0.5 s at 29.97 fps).
pub const CHUNK_FRAMES: u32 = effectcraft_engine::remote_stream::DEFAULT_CHUNK_FRAMES;
/// Seconds of chunks the server keeps ahead of the playhead (CR6a: 8 covers its hitches).
pub const AHEAD_SECS: f32 = 8.0;
/// Threads decoding chunks, and decoder threads each.
const DECODERS: usize = 2;
const DECODER_THREADS: usize = 3;
/// How often the playhead goes to the server while playing (seconds).
const PLAYHEAD_EVERY: f64 = 0.15;

/// One chunk as it arrived.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkMeta {
    pub seq: u64,
    pub revision: u64,
    /// Comp frame of its first frame, and how many frames it has.
    pub first: i64,
    pub frames: u32,
    /// Its size relative to the full size.
    pub scale: f32,
}

/// What the stream did (`ui.inspect` `preview.stream`).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStats {
    /// `starting`, `playing`, `ended`, `failed`, `stopped`.
    pub state: String,
    pub error: Option<String>,
    /// Start request → first chunk decoded (ms).
    pub warmup_ms: Option<f64>,
    /// Chunks received, and of those, copies of frames it already had (upgrades, revisions).
    pub chunks: u64,
    pub upgrades: u64,
    /// Chunks decoded after playback reached their first frame (it waited for them), not counting
    /// the first.
    pub late: u64,
    /// Time playback waited for a frame after the warm-up (ms).
    pub stall_ms: f64,
    pub stalls: u64,
    pub bytes: u64,
    /// Received bits per second since the start (kbit/s).
    pub kbps: Option<f64>,
    /// Decode time per frame (ms, smoothed).
    pub decode_ms: Option<f64>,
    /// First copies of frames by resolution: full, half, quarter.
    pub full: u64,
    pub half: u64,
    pub quarter: u64,
    /// `stale` notices (edits while playing), and chunks dropped as broken.
    pub stale: u64,
    pub broken: u64,
    /// Full size of the stream's frames.
    pub size: Option<[u32; 2]>,
    /// CR6c, this preview: frames it found on this Mac (RAM or disk) instead of streaming them,
    /// of those read back from the disk cache, streams started (one per run of missing frames),
    /// and frames written to the disk cache.
    pub held: u64,
    pub from_disk: u64,
    pub streams: u64,
    pub to_disk: u64,
}

/// A decoded frame held for playback.
struct Held {
    img: Arc<egui::ColorImage>,
    revision: u64,
    seq: u64,
    scale: f32,
    /// An edit changed it after it was rendered (a newer copy is on its way).
    stale: bool,
    /// The key it was last put in the RAM cache under.
    published: Option<FrameKey>,
}

/// The decoded frames of one stream (the logic, without the network: see the tests).
pub struct StreamBuffer {
    frames: BTreeMap<i64, Held>,
    /// `stale` notices: (revision, first frame): older revisions' frames from there on are old.
    /// The app's own ones ([`StreamBuffer::edited`]) say `i64::MAX` for an edit that changed
    /// nothing the comp shows.
    notices: Vec<(u64, i64)>,
    /// `notices` hold every edit since this revision (the stream's start; older ones fell off):
    /// a frame of a revision from here on that no notice made old shows the newest revision.
    chain_from: u64,
    /// Keys this stream put in the cache that are stale: taken out again when the preview stops.
    dirty: HashSet<FrameKey>,
    /// The newest revision a chunk or a `stale` notice came at.
    acked: u64,
    /// Playback's frame and range (see [`StreamBuffer::set_playhead`]).
    cur: i64,
    range: (i64, i64, bool),
    /// Most frames held.
    cap: usize,
    started: Instant,
    pub stats: StreamStats,
    /// The newest chunk received (flow control).
    pub received: Option<u64>,
    /// The stream is over (`failed`): playback goes on here.
    pub failed: Option<String>,
}

impl StreamBuffer {
    pub fn new(cur: i64, range: (i64, i64, bool), cap: usize) -> StreamBuffer {
        StreamBuffer {
            frames: BTreeMap::new(),
            notices: vec![],
            chain_from: 0,
            dirty: HashSet::new(),
            acked: 0,
            cur,
            range,
            cap: cap.max(1),
            started: Instant::now(),
            stats: StreamStats { state: "starting".into(), ..StreamStats::default() },
            received: None,
            failed: None,
        }
    }

    /// The stream plays the project at `revision`: edits from there on are followed.
    pub fn at_revision(mut self, revision: u64) -> StreamBuffer {
        self.chain_from = revision;
        self.acked = self.acked.max(revision);
        self
    }

    fn notice(&mut self, revision: u64, from: i64) {
        self.notices.push((revision, from));
        if self.notices.len() > 64 {
            let (gone, _) = self.notices.remove(0);
            self.chain_from = self.chain_from.max(gone);
        }
    }

    /// The revision a frame of `revision` that no notice made old shows: the newest the notices
    /// know (it is identical there), else its own.
    fn shows(&self, revision: u64) -> u64 {
        if revision < self.chain_from {
            return revision;
        }
        self.notices.iter().map(|(r, _)| *r).filter(|r| *r > revision).max().unwrap_or(revision).max(revision)
    }

    /// Frames playback shows before it reaches `f` (`None`: it doesn't, from where it is).
    fn ahead(&self, f: i64) -> Option<i64> {
        let (start, end, looping) = self.range;
        if f < start || f > end {
            return None;
        }
        if f >= self.cur {
            return Some(f.saturating_sub(self.cur));
        }
        looping.then(|| end.saturating_sub(self.cur).saturating_add(1).saturating_add(f.saturating_sub(start)))
    }

    /// Playback is at `cur`: frames it passed (and won't reach again) go.
    pub fn set_playhead(&mut self, cur: i64) {
        self.cur = cur;
        let cap = self.cap as i64;
        let gone: Vec<i64> = self.frames.keys().copied().filter(|f| self.ahead(*f).is_none_or(|d| d > cap)).collect();
        for f in gone {
            self.frames.remove(&f);
        }
    }

    pub fn has(&self, f: i64) -> bool {
        self.frames.contains_key(&f)
    }

    /// Whether the frame is held and still current.
    pub fn fresh(&self, f: i64) -> bool {
        self.frames.get(&f).is_some_and(|h| !h.stale)
    }

    /// Whether the frame is held and an edit made it old.
    pub fn stale(&self, f: i64) -> bool {
        self.frames.get(&f).is_some_and(|h| h.stale)
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// A chunk was received (`bytes` of payload).
    pub fn arrived(&mut self, seq: u64, bytes: usize) {
        self.stats.chunks = self.stats.chunks.saturating_add(1);
        self.stats.bytes = self.stats.bytes.saturating_add(bytes as u64);
        self.received = Some(self.received.map_or(seq, |r| r.max(seq)));
        let secs = self.started.elapsed().as_secs_f64();
        if secs > 0.0 {
            self.stats.kbps = Some(self.stats.bytes as f64 * 8.0 / 1000.0 / secs);
        }
    }

    /// A chunk decoded into `imgs` (its frames in order; fewer than it should have when the tail
    /// didn't decode). The newest copy of a frame wins: a newer revision, else a later chunk; a
    /// copy an edit made old only fills a gap.
    pub fn insert(&mut self, meta: ChunkMeta, imgs: Vec<Arc<egui::ColorImage>>, decode_ms: f64) {
        if self.stats.warmup_ms.is_none() {
            self.stats.warmup_ms = Some(self.started.elapsed().as_secs_f64() * 1000.0);
            self.stats.state = "playing".into();
        } else if meta.first <= self.cur && meta.first.saturating_add(i64::from(meta.frames)) > self.cur && !self.fresh(self.cur) {
            // Playback reached it before it was here.
            self.stats.late = self.stats.late.saturating_add(1);
        }
        self.acked = self.acked.max(meta.revision);
        if !imgs.is_empty() {
            let per = decode_ms / imgs.len() as f64;
            self.stats.decode_ms = Some(self.stats.decode_ms.map_or(per, |o| o * 0.75 + per * 0.25));
        }
        let mut upgrade = false;
        let mut first_copies = 0u64;
        for (i, img) in imgs.into_iter().enumerate() {
            let f = meta.first.saturating_add(i as i64);
            if self.ahead(f).is_none_or(|d| d > self.cap as i64) {
                continue;
            }
            let stale = self.notices.iter().any(|(r, from)| meta.revision < *r && f >= *from);
            let revision = if stale { meta.revision } else { self.shows(meta.revision) };
            let new = Held { img, revision, seq: meta.seq, scale: meta.scale, stale, published: None };
            match self.frames.get(&f) {
                None => {
                    first_copies += 1;
                    self.frames.insert(f, new);
                }
                Some(old) => {
                    let better = if old.stale != stale { old.stale } else { (revision, meta.seq) > (old.revision, old.seq) };
                    if better {
                        upgrade = true;
                        self.frames.insert(f, new);
                    }
                }
            }
        }
        if upgrade && first_copies == 0 {
            self.stats.upgrades = self.stats.upgrades.saturating_add(1);
        }
        let n = match meta.scale {
            s if s >= 0.99 => &mut self.stats.full,
            s if s >= 0.49 => &mut self.stats.half,
            _ => &mut self.stats.quarter,
        };
        *n = n.saturating_add(first_copies);
    }

    /// An edit at `revision` changed the frames from `from` on.
    pub fn mark_stale(&mut self, revision: u64, from: i64) {
        self.stats.stale = self.stats.stale.saturating_add(1);
        self.acked = self.acked.max(revision);
        self.notice(revision, from);
        for h in self.frames.range_mut(from..).map(|(_, h)| h) {
            if h.revision < revision && !h.stale {
                h.stale = true;
                if let Some(k) = h.published {
                    self.dirty.insert(k);
                }
            }
        }
    }

    /// This Mac made the edit to `revision`, which changes the comp's frames from `from` on
    /// (`None`: nothing it shows; [`effectcraft_engine::remote_stream::first_change`], as the
    /// server works it out). Frames from there on are old at once; the ones before show the new
    /// revision as they are: they stay current (cache bar, disk cache) instead of turning into
    /// drafts until a server notice that never comes when nothing changed.
    pub fn edited(&mut self, revision: u64, from: Option<i64>) {
        let from = from.unwrap_or(i64::MAX);
        self.acked = self.acked.max(revision);
        self.notice(revision, from);
        let chain = self.chain_from;
        let dirty = &mut self.dirty;
        for (f, h) in &mut self.frames {
            if h.stale || h.revision >= revision {
                continue;
            }
            if *f >= from {
                h.stale = true;
                if let Some(k) = h.published {
                    dirty.insert(k);
                }
            } else if h.revision >= chain {
                h.revision = revision;
                // Published again as current (and kept on disk).
                h.published = None;
            }
        }
    }

    /// The frames to put in the RAM cache under `series` (with its frame set): new ones, and
    /// every one again when the series changed (an edit gives the comp a new content key).
    pub fn publish(&mut self, series: &FrameKey) -> Vec<Published> {
        let mut out = vec![];
        for (f, h) in &mut self.frames {
            let key = FrameKey { frame: *f, ..*series };
            if h.published == Some(key) {
                continue;
            }
            h.published = Some(key);
            if h.stale {
                self.dirty.insert(key);
            } else {
                self.dirty.remove(&key);
            }
            // Only a frame rendered at the series' revision is known to show it (a frame of an
            // older one shows until the server's `stale` notice says whether the edit changed it).
            let current = h.revision >= series.revision && !h.stale;
            out.push(Published { key, img: h.img.clone(), scale: h.scale, draft: h.stale || is_draft_scale(h.scale) || !current, current });
        }
        out
    }

    /// The keys to take out of the cache when the preview stops: stale frames, and frames of
    /// revisions before `revision` while the server hasn't answered the edit yet (it may change
    /// them). Below-full-size frames stay (drafts).
    pub fn take_dirty(&mut self, revision: u64) -> Vec<FrameKey> {
        let mut out: Vec<FrameKey> = self.dirty.drain().collect();
        if self.acked < revision {
            out.extend(self.frames.values().filter(|h| h.revision < revision).filter_map(|h| h.published));
        }
        out
    }

    pub fn fail(&mut self, error: String) {
        if self.failed.is_none() {
            self.stats.state = "failed".into();
            self.stats.error = Some(error.clone());
            self.failed = Some(error);
        }
    }
}

/// A stream's state shared by the link thread, its decoders and the app.
pub struct StreamShared {
    pub buffer: Mutex<StreamBuffer>,
    /// Woken when frames are in (the app's UI).
    repaint: Option<egui::Context>,
    /// The comp's frame rate (for `stale` times).
    pub rate: FrameRate,
}

impl StreamShared {
    pub fn new(buffer: StreamBuffer, rate: FrameRate, repaint: Option<egui::Context>) -> Arc<StreamShared> {
        Arc::new(StreamShared { buffer: Mutex::new(buffer), repaint, rate })
    }

    pub fn buffer(&self) -> std::sync::MutexGuard<'_, StreamBuffer> {
        self.buffer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
    }

    /// The stream is over (the server said so, or the link went).
    pub fn fail(&self, error: String) {
        self.buffer().fail(error);
        self.wake();
    }
}

/// Decodes a stream's chunks on [`DECODERS`] threads of its own, into its buffer. The threads end
/// when this is dropped (the stream is over).
pub struct Decoders {
    tx: Sender<(ChunkMeta, Vec<u8>)>,
}

impl Decoders {
    pub fn start(shared: &Arc<StreamShared>) -> Decoders {
        let (tx, rx) = channel::<(ChunkMeta, Vec<u8>)>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..DECODERS {
            let (rx, shared) = (rx.clone(), shared.clone());
            let spawned = std::thread::Builder::new().name(format!("ec-stream-dec-{i}")).spawn(move || decode_loop(&rx, &shared));
            if let Err(e) = spawned {
                log::warn!("cloud stream: no decoder thread: {e}");
            }
        }
        Decoders { tx }
    }

    pub fn push(&self, meta: ChunkMeta, payload: Vec<u8>) {
        let _ = self.tx.send((meta, payload));
    }
}

fn decode_loop(rx: &Arc<Mutex<Receiver<(ChunkMeta, Vec<u8>)>>>, shared: &Arc<StreamShared>) {
    loop {
        let job = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok((meta, payload)) = job else { return };
        let t = Instant::now();
        let decoded = decode(&payload);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match decoded {
            Ok(imgs) => {
                if imgs.len() != meta.frames as usize {
                    log::info!("cloud stream: chunk #{} decoded {} of {} frames", meta.seq, imgs.len(), meta.frames);
                }
                shared.buffer().insert(meta, imgs, ms);
            }
            Err(e) => {
                log::warn!("cloud stream: chunk #{}: {e}", meta.seq);
                let mut b = shared.buffer();
                b.stats.broken = b.stats.broken.saturating_add(1);
            }
        }
        shared.wake();
    }
}

/// A chunk's frames as viewer images (opaque, so premultiplied as they are).
pub fn decode(payload: &[u8]) -> Result<Vec<Arc<egui::ColorImage>>, String> {
    let frames = std::panic::catch_unwind(|| effectcraft_media::preview_stream::decode_chunk(payload, DECODER_THREADS))
        .unwrap_or_else(|_| Err("decoding the chunk panicked".into()))?;
    Ok(frames
        .into_iter()
        .filter(|f| f.rgba.len() == (f.width as usize).saturating_mul(f.height as usize).saturating_mul(4) && !f.rgba.is_empty())
        .map(|f| Arc::new(egui::ColorImage::from_rgba_premultiplied([f.width as usize, f.height as usize], &f.rgba)))
        .collect())
}

/// What [`CloudPlayback::tick`] needs from the app each UI frame.
pub struct PlayNow<'a> {
    /// A video preview plays (not caching first).
    pub playing: bool,
    pub now: f64,
    pub comp: ItemId,
    pub rate: FrameRate,
    pub project: &'a Arc<Project>,
    pub revision: u64,
    /// The viewer's frame keys (frame 0) and render options.
    pub series: FrameKey,
    pub opts: RenderOpts,
    /// The frame on screen, the preview's range (inclusive) and looping.
    pub cur: i64,
    pub start: i64,
    pub end: i64,
    pub looping: bool,
    /// Playback waits for a frame.
    pub waiting: bool,
    /// How many viewer frames the RAM preview holds.
    pub fit: i64,
    /// This Mac's disk cache: streamed frames are kept there and read back from it.
    pub disk: Option<Arc<DiskCache>>,
}

/// What the app does with this UI frame's playback.
#[derive(Debug, Default, PartialEq)]
pub struct PlayTick {
    /// The stream delivers this preview's frames: don't render or request them here.
    pub streamed: bool,
    /// A status note (the stream gave up: playing on this Mac).
    pub note: Option<String>,
}

/// How long a preview waits for its comp's content key before streaming frames the disk cache
/// may hold (seconds; it is hashed on a worker).
const KEY_WAIT: f64 = 0.5;

/// The app's side of the playback stream: start it when a cloud preview plays (for the frames
/// this Mac doesn't hold), feed it the playhead and edits, publish its frames, stop it and clean
/// up.
#[derive(Default)]
pub struct CloudPlayback {
    current: Option<Running>,
    /// The playing preview (across the streams it starts).
    preview: Option<Preview>,
    /// This preview gave up on the stream (it failed): it plays on here until the next one.
    given_up: bool,
    /// The last stream's numbers (kept after it stops, for `ui.inspect`).
    pub last: Option<StreamStats>,
}

/// One stream: it plays the run of missing frames `run` (inclusive).
struct Running {
    shared: Arc<StreamShared>,
    client: Arc<CloudFrames>,
    revision: u64,
    /// The project at `revision` (edits are compared with it).
    project: Arc<Project>,
    /// What the server's stream was set up with: another one needs a new stream.
    shape: Option<Shape>,
    run: (i64, i64),
    /// Chunks received when playback last had them all or got a new one, and when (see
    /// [`NO_CHUNKS_SECS`]).
    progress: (u64, f64),
}

/// What a running stream can't follow: the comp's frame grid and size, and the viewer's frame
/// series apart from its content (comp, resolution, view, render options).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shape {
    rate: FrameRate,
    size: (u32, u32),
    duration: Tick,
    series: (u64, u32, u64, u64),
}

fn shape(p: &PlayNow<'_>) -> Option<Shape> {
    let c = p.project.comp(p.comp)?;
    Some(Shape { rate: c.frame_rate, size: (c.width, c.height), duration: c.duration, series: (p.series.comp, p.series.scale, p.series.view, p.series.opts) })
}

/// Playback waits for frames and no chunk came for this long: the stream is stuck (a server that
/// stopped rendering; 17 Oct: chunks in flight forever at 14.5 s), so the preview plays on here
/// rather than hang. The first chunk of a heavy comp can take a few seconds (warm-up).
const NO_CHUNKS_SECS: f64 = 12.0;

/// The first comp frame an edit from `old` to `new` can change (`None`: nothing it shows).
pub(crate) fn first_changed_frame(old: &Project, new: &Project, comp: ItemId, rate: FrameRate) -> Option<i64> {
    let t = effectcraft_engine::remote_stream::first_change(old, new, comp)?;
    Some(if t.0 == i64::MIN { i64::MIN } else { rate.frame_at(t) })
}

/// A playing preview.
struct Preview {
    started: f64,
    cur: i64,
    range: (i64, i64, bool),
    playhead_at: f64,
    seen: f64,
    stalled: bool,
    /// Frames read back from the disk cache.
    loader: Option<Loader>,
    held: u64,
    streams: u64,
    to_disk: u64,
}

/// Reads a preview's frames back from the disk cache into the RAM preview, in play order.
struct Loader {
    series: FrameKey,
    /// Frames still to read (held: no stream asks for them).
    pending: Arc<Mutex<HashSet<i64>>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    loaded: Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Loader {
    fn start(dc: Arc<DiskCache>, sink: StreamedSink, series: FrameKey, jobs: Vec<(i64, u128, bool)>) -> Loader {
        let pending: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(jobs.iter().map(|j| j.0).collect()));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (p, c, l) = (pending.clone(), cancel.clone(), loaded.clone());
        let run = move || {
            for (f, k, draft) in jobs {
                if c.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                // A damaged entry is only a frame the stream asks for.
                if let Ok(Some(img)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_back(&dc, k))) {
                    sink.insert(FrameKey { frame: f, ..series }, img, draft);
                    l.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                p.lock().unwrap_or_else(PoisonError::into_inner).remove(&f);
            }
            p.lock().unwrap_or_else(PoisonError::into_inner).clear();
        };
        if let Err(e) = std::thread::Builder::new().name("ec-stream-disk".into()).spawn(run) {
            log::warn!("cloud stream: no disk reader: {e}");
            pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
        }
        Loader { series, pending, cancel, loaded }
    }

    fn pending(&self, f: i64) -> bool {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).contains(&f)
    }
}

/// The preview's frames from `cur` in play order, as many as `fit` (each once).
fn play_order(cur: i64, (start, end, looping): (i64, i64, bool), fit: i64) -> Vec<i64> {
    let n = end.saturating_sub(start).saturating_add(1).clamp(0, fit.clamp(0, 100_000));
    let cur = cur.clamp(start, end.max(start));
    (0..n)
        .map_while(|k| {
            let f = cur.saturating_add(k);
            if f <= end {
                Some(f)
            } else if looping {
                Some(start.saturating_add(f.saturating_sub(end).saturating_sub(1)))
            } else {
                None
            }
        })
        .collect()
}

/// The first run of frames in `order` that `held` says this Mac doesn't hold: its first and last
/// frame (consecutive frames, inclusive).
pub fn missing_run(order: &[i64], held: impl Fn(i64) -> bool) -> Option<(i64, i64)> {
    let i = order.iter().position(|f| !held(*f))?;
    let a = *order.get(i)?;
    let mut b = a;
    for f in order.iter().skip(i.saturating_add(1)) {
        if *f != b.saturating_add(1) || held(*f) {
            break;
        }
        b = *f;
    }
    Some((a, b))
}

impl CloudPlayback {
    /// The running stream's numbers, else the last one's, with this preview's.
    pub fn stats(&self) -> Option<StreamStats> {
        let mut s = self.current.as_ref().map(|r| r.shared.buffer().stats.clone()).or_else(|| self.last.clone());
        if let Some(p) = &self.preview {
            let s = s.get_or_insert_with(|| StreamStats { state: "held".into(), ..StreamStats::default() });
            s.held = p.held;
            s.streams = p.streams;
            s.to_disk = p.to_disk;
            s.from_disk = p.loader.as_ref().map_or(0, |l| l.loaded.load(std::sync::atomic::Ordering::Acquire));
        }
        s
    }

    /// A stream runs.
    pub fn active(&self) -> bool {
        self.current.is_some()
    }

    /// Follow this UI frame's playback (`client`: the cloud, when the viewer's frames go to it).
    pub fn tick(&mut self, client: Option<&Arc<CloudFrames>>, p: &PlayNow<'_>, frames: &Frames, ctx: Option<&egui::Context>) -> PlayTick {
        let client = client.filter(|c| c.ready());
        if !p.playing {
            self.given_up = false;
            self.halt(frames, Some(FrameKey { frame: p.cur, ..p.series }), p.revision);
            self.end_preview();
            return PlayTick::default();
        }
        let Some(client) = client else {
            // The cloud went away mid-preview (its own note says so): frames render here.
            self.halt(frames, None, p.revision);
            self.end_preview();
            return PlayTick::default();
        };
        if self.given_up {
            return PlayTick::default();
        }
        // Another server, or a change the server's stream can't follow (frame rate, comp size or
        // duration, the viewer's resolution, view or options, another comp): a new stream.
        let now_shape = shape(p);
        if self.current.as_ref().is_some_and(|r| !Arc::ptr_eq(&r.client, client) || r.shape != now_shape) {
            self.halt(frames, None, p.revision);
        }
        let range = (p.start, p.end, p.looping);
        let pv = self.preview.get_or_insert_with(|| Preview {
            started: p.now,
            cur: p.cur,
            range,
            playhead_at: p.now,
            seen: p.now,
            stalled: false,
            loader: None,
            held: 0,
            streams: 0,
            to_disk: 0,
        });
        // A jump (the playhead moved by hand, the range changed): plan again from there.
        let wrapped = p.looping && p.cur <= p.start.saturating_add(2) && pv.cur >= p.end.saturating_sub(2);
        let jump = (p.cur < pv.cur && !wrapped) || p.cur > pv.cur.saturating_add((p.rate.as_f64() * 2.0) as i64).saturating_add(2) || range != pv.range;
        pv.cur = p.cur;
        pv.range = range;
        if jump {
            pv.loader = None;
            self.halt(frames, None, p.revision);
        }
        let failed = self.current.as_ref().and_then(|r| r.shared.buffer().failed.clone());
        if let Some(e) = failed {
            self.given_up = true;
            self.halt(frames, None, p.revision);
            return PlayTick { streamed: false, note: Some(format!("Cloud playback stream stopped ({e}): playing on this Mac")) };
        }
        let cap = usize::try_from(p.fit.clamp(1, 100_000)).unwrap_or(1);
        // Feed the running stream and publish its frames.
        if let (Some(r), Some(pv)) = (&mut self.current, &mut self.preview) {
            // An edit: the server renders again from the first frame it changes. This Mac knows
            // which frames that is as well as the server does: they are old at once, the ones
            // before stay current (the server says nothing when nothing changed).
            if p.revision != r.revision {
                let from = first_changed_frame(&r.project, p.project, p.comp, p.rate);
                r.shared.buffer().edited(p.revision, from);
                r.revision = p.revision;
                r.project = p.project.clone();
                r.client.stream_sync(p.revision, p.project);
            }
            let mut b = r.shared.buffer();
            // A stuck stream: playback has waited for NO_CHUNKS_SECS without a chunk coming.
            if !p.waiting || b.stats.chunks != r.progress.0 {
                r.progress = (b.stats.chunks, p.now);
            } else if p.now - r.progress.1 >= NO_CHUNKS_SECS && b.failed.is_none() {
                log::warn!("cloud stream: no chunk for {NO_CHUNKS_SECS} s while playback waits at frame {}: playing on this Mac", p.cur);
                b.fail(format!("no frames from the cloud for {NO_CHUNKS_SECS:.0} s"));
            }
            b.range = range;
            b.cap = cap;
            b.set_playhead(p.cur);
            if b.stats.warmup_ms.is_some() {
                let dt = (p.now - pv.seen).max(0.0);
                if p.waiting {
                    if !pv.stalled {
                        b.stats.stalls = b.stats.stalls.saturating_add(1);
                    }
                    b.stats.stall_ms += dt * 1000.0;
                }
                pv.stalled = p.waiting;
            }
            let ready = b.publish(&p.series);
            drop(b);
            pv.to_disk = pv.to_disk.saturating_add(Self::keep(frames, p, ready));
            if p.now - pv.playhead_at >= PLAYHEAD_EVERY || jump {
                pv.playhead_at = p.now;
                r.client.stream_playhead(p.rate.tick_of(p.cur), true);
            }
        }
        if let Some(pv) = &mut self.preview {
            pv.seen = p.now;
        }
        self.plan(client, p, frames, ctx, range, cap)
    }

    /// Start a stream for the first run of frames ahead that this Mac doesn't hold (RAM, a
    /// stream's fresh frame, or the disk cache), unless the running one covers it.
    fn plan(
        &mut self,
        client: &Arc<CloudFrames>,
        p: &PlayNow<'_>,
        frames: &Frames,
        ctx: Option<&egui::Context>,
        range: (i64, i64, bool),
        cap: usize,
    ) -> PlayTick {
        let streamed = PlayTick { streamed: true, note: None };
        let Some(pv) = &mut self.preview else { return streamed };
        let order = play_order(p.cur, range, p.fit);
        if pv.loader.as_ref().is_none_or(|l| l.series != p.series) {
            pv.loader = None;
            if let Some(dc) = &p.disk {
                if !frames.content_key_ready(p.project, &p.series) {
                    if p.now - pv.started < KEY_WAIT && self.current.is_none() {
                        return streamed;
                    }
                } else {
                    let ram = frames.cached_frames(&p.series);
                    let oh = opts_hash(&p.opts);
                    let jobs: Vec<(i64, u128, bool)> = order
                        .iter()
                        .filter(|f| ram.binary_search(f).is_err())
                        .filter_map(|f| frames.streamed_on_disk(dc, p.project, &FrameKey { frame: *f, ..p.series }, oh).map(|(k, d)| (*f, k, d)))
                        .collect();
                    pv.loader = Some(Loader::start(dc.clone(), frames.streamed_sink(), p.series, jobs));
                }
            }
        }
        let ram = frames.cached_frames(&p.series);
        let buffer = self.current.as_ref().map(|r| r.shared.clone());
        let loader = pv.loader.as_ref().filter(|l| l.series == p.series);
        let held = |f: i64| {
            let stale = buffer.as_ref().is_some_and(|b| b.buffer().stale(f));
            (ram.binary_search(&f).is_ok() && !stale) || loader.is_some_and(|l| l.pending(f))
        };
        let next = missing_run(&order, held);
        if self.current.is_none() && pv.streams == 0 {
            pv.held = order.iter().filter(|f| held(**f)).count() as u64;
        }
        let Some((a, b)) = next else {
            // Everything ahead is here: no stream (a running one finishes its run).
            return streamed;
        };
        if let Some(r) = &self.current {
            let covered = a >= r.run.0 && a <= r.run.1;
            let done = (r.run.0..=r.run.1).all(&held);
            if covered || !done {
                return streamed;
            }
        }
        // A new stream for the run `a..=b`.
        self.halt(frames, None, p.revision);
        let Some(pv) = &mut self.preview else { return streamed };
        pv.streams = pv.streams.saturating_add(1);
        let ahead = AHEAD_SECS.min(((p.fit as f64 * 0.6) / p.rate.as_f64().max(1.0)) as f32).max(1.0);
        let shared = StreamShared::new(StreamBuffer::new(p.cur, range, cap).at_revision(p.revision), p.rate, ctx.cloned());
        let start = StreamStart {
            project: p.project.clone(),
            revision: p.revision,
            comp: p.comp,
            from: p.rate.tick_of(a),
            to: Some(p.rate.tick_of(b.saturating_add(1))),
            loop_from: None,
            looping: false,
            opts: p.opts,
            ahead_secs: ahead,
            cache: frames.cache_series(p.project, &p.series, &p.opts),
        };
        client.stream_start(start, shared.clone());
        client.stream_playhead(p.rate.tick_of(p.cur), true);
        pv.playhead_at = p.now;
        self.current = Some(Running {
            shared,
            client: client.clone(),
            revision: p.revision,
            project: p.project.clone(),
            shape: shape(p),
            run: (a, b),
            progress: (0, p.now),
        });
        streamed
    }

    /// Cache a stream's frames (drafts below full size) and write the current ones to the disk
    /// cache, when it has room. How many went to disk.
    fn keep(frames: &Frames, p: &PlayNow<'_>, ready: Vec<Published>) -> u64 {
        let oh = opts_hash(&p.opts);
        let mut stored = 0u64;
        for f in ready {
            if f.current
                && let Some(dc) = &p.disk
                && dc.has_room(f.img.as_raw().len())
                && let Some(k) = frames.streamed_disk_key(p.project, &f.key, oh, f.scale)
            {
                dc.put_frame(k, f.img.size[0] as u32, f.img.size[1] as u32, ImageBytes(f.img.clone()));
                stored = stored.saturating_add(1);
            }
            frames.insert_streamed(f.key, f.img, f.draft);
        }
        stored
    }

    /// The preview is over: its numbers stay with the last stream's (`ui.inspect`).
    fn end_preview(&mut self) {
        if self.preview.is_some() {
            self.last = self.stats();
        }
        self.preview = None;
    }

    /// The preview stopped: stop its stream (see [`CloudPlayback::halt`]) and forget it.
    pub fn stop(&mut self, frames: &Frames, shown: Option<FrameKey>, revision: u64) {
        self.halt(frames, shown, revision);
        self.end_preview();
        // The next preview tries the stream again (the app doesn't tick a stopped one, so this is
        // where a failed stream is forgotten; a dropped tunnel once kept every later preview here).
        self.given_up = false;
    }

    /// Stop the stream (if any) and take its stale frames out of the cache (below-full-size
    /// ones stay, as drafts); `shown`: the frame on screen, taken out too when it came full size
    /// from the stream, so the paused viewer gets its exact frame (a draft there is rendered over).
    pub fn halt(&mut self, frames: &Frames, shown: Option<FrameKey>, revision: u64) {
        let Some(r) = self.current.take() else { return };
        r.client.stream_stop();
        let mut b = r.shared.buffer();
        if b.failed.is_none() {
            b.stats.state = "stopped".into();
        }
        let dirty = b.take_dirty(revision);
        let streamed_shown = shown.filter(|k| b.frames.get(&k.frame).is_some_and(|h| h.published == Some(*k)) && !frames.is_draft(k));
        self.last = Some(b.stats.clone());
        drop(b);
        for k in dirty.iter().chain(streamed_shown.iter()) {
            frames.remove_frame(k);
        }
    }
}

/// A frame's pixels for the disk cache's writer (shared, not copied).
struct ImageBytes(Arc<egui::ColorImage>);

impl AsRef<[u8]> for ImageBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_raw()
    }
}

/// Where a stream plays from (see [`effectcraft_engine::remote_stream::StreamMsg::Start`]).
pub struct StreamStart {
    pub project: Arc<Project>,
    pub revision: u64,
    pub comp: ItemId,
    pub from: Tick,
    pub to: Option<Tick>,
    pub loop_from: Option<Tick>,
    pub looping: bool,
    pub opts: RenderOpts,
    pub ahead_secs: f32,
    /// The viewer's frame series (CR8): the server plays frames it cached and keeps the ones it
    /// renders.
    pub cache: Option<effectcraft_engine::remote::CacheSeries>,
}
