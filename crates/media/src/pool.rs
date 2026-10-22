//! [`MediaPool`]: decoded footage frames for the compositor, with a memory-budgeted LRU cache.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

use effectcraft_project::{AlphaMode, Footage, FootageKind, ItemId};
use effectcraft_raster::{Image, Px};
use effectcraft_render::cache::shared::{Member, ShareKind, ShareLink, SharedBudget};
use effectcraft_render::{FloatCurve, FootageFrame, FootageSource, SharedFrame, preview_size};
use effectcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use filmcraft_media::{FrameRequest, SharedSource};
use image::metadata::Orientation;

use crate::convert::{AlphaOp, Packed, frame_to_image, pack_dynamic, turned};
use crate::gif_anim::{self, Anim};
use crate::{MediaError, Result};

/// Default frame-cache budget: 1 GiB of decoded frames.
pub const DEFAULT_BUDGET: usize = 1 << 30;

/// The cache path of a missing image-sequence frame (no file can have this name).
const PLACEHOLDER: &str = "\0missing frame";

/// What a missing image-sequence frame shows: 75 % colour bars (white, yellow, cyan, green,
/// magenta, red, blue), the usual "no picture here" frame.
fn colour_bars(w: u32, h: u32) -> Image {
    const BARS: [[f32; 3]; 7] = [[1.0, 1.0, 1.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
    let (w, h) = (w.clamp(1, 16_384), h.clamp(1, 16_384));
    let row: Vec<Px> = (0..w)
        .map(|x| {
            let c = BARS.get((x as usize * BARS.len()) / w as usize).copied().unwrap_or([0.0; 3]);
            [c[0] * 0.75, c[1] * 0.75, c[2] * 0.75, 1.0]
        })
        .collect();
    Image { width: w, height: h, data: row.repeat(h as usize) }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    path: Arc<str>,
    frame: i64,
    /// The alpha interpretation (bits 0–1) and how float values are encoded (bits 2–3: sRGB,
    /// as stored, a 2.4 power curve): they change the decoded pixels.
    interp: u8,
    matte: [u32; 3],
    crop: Option<[u64; 4]>,
    /// The preview size a frame is resampled to (reduced-resolution previews).
    preview: Option<[u32; 2]>,
    /// Vector footage drawn at this scale (its bits; 0 = decoded at the native size): one entry
    /// per Continuously Rasterize scale step.
    raster: u64,
    /// Decoded by a hardware decoder (previews only, see [`crate::hw`]): not the exact pixels an
    /// export must read.
    hw: bool,
}

/// A layer drawn at reduced resolution: its nominal size `w`×`h` at render scale `s` (< 0.75).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Preview {
    w: u32,
    h: u32,
    s: f64,
}

/// A cached frame: float pixels, or the decoder's samples converted on use (8-bit 4:2:0 video
/// takes 1.5 bytes a pixel instead of 16; see [`Packed`]).
#[derive(Clone)]
pub(crate) enum Pixels {
    Float(Arc<Image>),
    Packed(Arc<Packed>),
    /// Resampled to the preview size its key names, as the renderer would.
    Preview(Arc<Image>),
}

impl Pixels {
    /// Bytes held (what the cache budget counts).
    fn bytes(&self) -> usize {
        64 + match self {
            Pixels::Float(img) | Pixels::Preview(img) => img.data.len() * std::mem::size_of::<Px>(),
            Pixels::Packed(p) => p.bytes(),
        }
    }
    /// The frame as compositor pixels (a packed frame is converted now).
    pub(crate) fn image(&self) -> Arc<Image> {
        match self {
            Pixels::Float(img) | Pixels::Preview(img) => img.clone(),
            Pixels::Packed(p) => Arc::new(p.to_image()),
        }
    }
    /// The frame for the renderer to convert into its own buffer.
    fn shared(&self) -> SharedFrame {
        match self {
            Pixels::Float(img) | Pixels::Preview(img) => img.clone(),
            Pixels::Packed(p) => p.clone(),
        }
    }
}

struct Entry {
    px: Pixels,
    stamp: u64,
    bytes: usize,
}

/// Conformed audio files written by builds before this version are ignored (and written again):
/// version 1 files of sources longer than about 12:36 at 48 kHz were silent from there on (#172);
/// version 2 files were written silent where the source failed to decode, and kept the footage
/// silent from then on (#324).
const CONFORM_VERSION: u32 = 3;

/// Footage longer than this (24 hours) is read from its decoder rather than conformed: its
/// duration comes from the file's header, and a damaged one claiming days of audio had the
/// conform thread allocate all of it at once (#408).
const MAX_CONFORM_SECONDS: i64 = 24 * 3600;

/// The sample frames of `footage`'s audio at `rate` Hz that a conformed file holds, `None` past
/// [`MAX_CONFORM_SECONDS`].
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn conform_frames(footage: &Footage, rate: u32) -> Option<usize> {
    let frames = footage.duration.to_units_floor(i64::from(rate)).max(0);
    if frames > MAX_CONFORM_SECONDS.saturating_mul(i64::from(rate)) {
        return None;
    }
    usize::try_from(frames).ok()
}

/// The source time of sample frame `at` at `rate` Hz. (`at` × ticks per second overflows `i64`
/// past about 12:36 at 48 kHz.)
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn sample_time(at: usize, rate: u32) -> Tick {
    Tick::from_units(i64::try_from(at).unwrap_or(i64::MAX), i64::from(rate))
}

/// Least-recently-used frames, bounded by bytes.
#[derive(Default)]
struct Lru {
    map: HashMap<Key, Entry>,
    order: BTreeMap<u64, Key>,
    /// The latest use stamp, from the process-wide clock the caches share
    /// ([`effectcraft_render::cache::shared::stamp`]).
    clock: u64,
    bytes: usize,
    /// The shared cache budget, when the caches share one ([`MediaPool::share`]): told the
    /// bytes and the oldest use after every change.
    share: Option<ShareLink>,
}

impl Lru {
    fn tick(&mut self) -> u64 {
        self.clock = effectcraft_render::cache::shared::stamp_after(self.clock);
        self.clock
    }
    /// Tell the shared budget the bytes held and the oldest use (two atomic stores).
    fn report(&self) {
        if let Some(l) = &self.share {
            l.report(self.bytes as u64, self.order.first_key_value().map(|(s, _)| *s));
        }
    }
    fn get(&mut self, k: &Key) -> Option<Pixels> {
        let now = self.tick();
        let e = self.map.get_mut(k)?;
        self.order.remove(&e.stamp);
        e.stamp = now;
        let px = e.px.clone();
        self.order.insert(now, k.clone());
        self.report();
        Some(px)
    }
    fn insert(&mut self, k: Key, px: Pixels, budget: usize) {
        let bytes = px.bytes();
        let now = self.tick();
        if let Some(old) = self.map.insert(k.clone(), Entry { px, stamp: now, bytes }) {
            self.order.remove(&old.stamp);
            self.bytes -= old.bytes;
        }
        self.order.insert(now, k);
        self.bytes += bytes;
        self.evict(budget);
        self.report();
    }
    fn remove(&mut self, k: &Key) {
        if let Some(e) = self.map.remove(k) {
            self.order.remove(&e.stamp);
            self.bytes -= e.bytes;
        }
        self.report();
    }
    /// Evict least-recently-used frames until within `budget` (the newest frame always stays).
    fn evict(&mut self, budget: usize) {
        while self.bytes > budget && self.map.len() > 1 {
            let Some((_, k)) = self.order.pop_first() else { break };
            if let Some(e) = self.map.remove(&k) {
                self.bytes -= e.bytes;
            }
        }
        self.report();
    }
    /// The least recently used frame, for the shared budget (the newest frame always stays):
    /// the bytes freed.
    fn evict_oldest(&mut self) -> usize {
        if self.map.len() <= 1 {
            return 0;
        }
        let Some((_, k)) = self.order.pop_first() else { return 0 };
        let freed = self.map.remove(&k).map_or(0, |e| e.bytes);
        self.bytes = self.bytes.saturating_sub(freed);
        self.report();
        freed
    }
}

/// The footage frame cache as a member of the shared budget ([`MediaPool::share`]).
struct MediaMember(std::sync::Weak<Inner>);

impl Member for MediaMember {
    fn evict_oldest(&self) -> u64 {
        let Some(inner) = self.0.upgrade() else { return 0 };
        let freed = lock(&inner.cache).lru.evict_oldest();
        u64::try_from(freed).unwrap_or(u64::MAX)
    }
    fn alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

/// Cache counters (see [`MediaPool::stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Requests answered from the frame cache.
    pub hits: u64,
    /// Requests that had to decode (or wait for a read-ahead decode).
    pub misses: u64,
    /// Frames decoded ahead of sequential reads (playback, export).
    pub prefetched: u64,
    /// Frames currently cached and their total size in bytes.
    pub frames: usize,
    pub bytes: usize,
    /// Movie and audio files opened (each path once, however many callers ask for it at once).
    pub opened: u64,
}

#[derive(Default)]
struct CacheState {
    lru: Lru,
    /// Frames being decoded right now and the thread decoding each (requests for them wait
    /// instead of decoding again).
    inflight: HashMap<Key, ThreadId>,
    /// Last frame index requested per movie (sequential-read detection).
    last: HashMap<Arc<str>, i64>,
    /// Sequential decode-ahead per movie.
    ahead: HashMap<Arc<str>, Ahead>,
    /// Decode-ahead runners started (each runner's id).
    runners: u64,
    /// Finished decodes per file: a waiter that sees no progress on the file it waits for for
    /// [`STALL`] stops waiting. Per file: while a movie's decode-ahead kept finishing frames, a
    /// waiter for a still that another worker was stuck decoding waited on, however long.
    progress: HashMap<Arc<str>, u64>,
}

/// A movie's sequential decode-ahead: a runner thread decodes the frames `next..until` (frame
/// indices at the footage's rate, before looping) in order, so readers going through the movie
/// (export workers, playback) find them cached instead of each decoding on its own.
struct Ahead {
    footage: Footage,
    preview: Option<Preview>,
    /// How float values are encoded (also in `stream.interp`).
    float: FloatCurve,
    /// The frames it decodes are keyed like this (frame 0): other interpretations or preview
    /// sizes of the file don't wait for it.
    stream: Key,
    next: i64,
    until: i64,
    /// Bytes of one decoded frame (sizes the window).
    frame_bytes: usize,
    /// The runner working on it (`None`: stopped).
    runner: Option<u64>,
}

/// Requests within this many frames of the previous request of the same movie read it in order
/// (parallel export workers ask for neighbouring frames out of order).
const SEQ_SLACK: i64 = 32;
/// At most this many frames are decoded ahead of a reader …
const MAX_AHEAD: i64 = 32;
/// … and at most this share (1 / n) of the frame-cache budget.
const AHEAD_SHARE: usize = 4;
/// At most this many movies are decoded ahead at once.
const MAX_RUNNERS: usize = 4;
/// Frames a decode-ahead runner of footage with this codec decodes at once: up to 4 (half the
/// cores) for intra-only codecs, whose frames decode independently (FilmCraft decodes them
/// outside its decoder lock); 1 for long-GOP codecs, which decode in order.
fn ahead_batch(codec: &str) -> usize {
    let c = codec.to_ascii_lowercase();
    if !["prores", "dnxh", "motion jpeg", "mjpeg"].iter().any(|n| c.contains(n)) {
        return 1;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / 2).clamp(1, 4)
}
/// A rayon worker waits for a frame being decoded elsewhere while decoding progresses, and decodes
/// it itself after this long without progress (see [`Inner::pixels_at`]).
const STALL: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits and decodes longer than this are logged (target `effectcraft_stall`, debug): a
/// diagnostic for preview playback that stops.
const SLOW: std::time::Duration = std::time::Duration::from_millis(150);

/// Log `what` of frame `raw` of `path` when it took longer than [`SLOW`].
fn slow(what: &str, path: &str, raw: i64, took: std::time::Duration) {
    if took >= SLOW {
        log::debug!(target: "effectcraft_stall", "media: {what} frame {raw} of {path} in {:.0} ms", took.as_secs_f64() * 1000.0);
    }
}

/// Logs a movie open (or the wait for another thread's) that took longer than [`SLOW`].
struct SlowOpen<'a>(&'a str, std::time::Instant);
impl Drop for SlowOpen<'_> {
    fn drop(&mut self) {
        slow("opened (or waited for)", self.0, -1, self.1.elapsed());
    }
}

/// A reader's wait for a frame another thread decodes ([`Inner::pixels_at`]), logged when long.
struct Waited<'a> {
    path: &'a str,
    raw: i64,
    start: std::time::Instant,
    on: std::cell::Cell<Option<&'static str>>,
}

impl<'a> Waited<'a> {
    fn new(path: &'a str, raw: i64) -> Self {
        Waited { path, raw, start: std::time::Instant::now(), on: Default::default() }
    }

    /// The reader waits for `whom`.
    fn on(&self, whom: &'static str) {
        self.on.set(Some(whom));
    }

    /// The wait ended with `how`.
    fn done(&self, how: &str) {
        if let Some(whom) = self.on.get() {
            let took = self.start.elapsed();
            if took >= SLOW {
                let thread = std::thread::current();
                log::debug!(
                    target: "effectcraft_stall",
                    "media: waited {:.0} ms for {whom} to decode frame {} of {} ({}): {how}",
                    took.as_secs_f64() * 1000.0,
                    self.raw,
                    self.path,
                    thread.name().unwrap_or("?")
                );
            }
        }
    }
}

/// Whether `a` and `b` key frames of the same stream (all but the frame index match).
fn same_stream(a: &Key, b: &Key) -> bool {
    a.path == b.path && a.interp == b.interp && a.matte == b.matte && a.crop == b.crop && a.preview == b.preview && a.raster == b.raster && a.hw == b.hw
}

/// A movie/audio source by path and the turn its picture is shown with
/// ([`crate::probe::video_turn`]), opened by the first caller while the others wait for it
/// (`None`: failed to open; not retried until `forget`).
type SourceSlot = Arc<OnceLock<Option<(SharedSource, Orientation)>>>;

/// At most this many movie/audio files stay open (least recently used closed first).
const MAX_SOURCES: usize = 24;

/// Open files whose decoders hold their whole contents (WAV bytes, decoded MP3 / FLAC samples)
/// are closed beyond this many bytes of such data (least recently used first).
const SOURCE_BUDGET: usize = 1 << 30;

struct SourceEntry {
    slot: SourceSlot,
    stamp: u64,
    /// Bytes the source keeps in memory (estimated once it is open).
    held: usize,
}

/// An open source: its path, and whether its video may decode on a hardware decoder (previews:
/// a source keeps decoded frames and decoders of its own, which an export must not read).
type SourceKey = (Arc<str>, bool);

/// Open movie/audio files by path, least recently used closed first beyond [`MAX_SOURCES`] or
/// [`SOURCE_BUDGET`].
#[derive(Default)]
struct Sources {
    map: HashMap<SourceKey, SourceEntry>,
    clock: u64,
}

impl Sources {
    /// The slot of `path` (created empty), marked as just used.
    fn slot(&mut self, path: &Arc<str>, hw: bool) -> SourceSlot {
        self.clock += 1;
        let clock = self.clock;
        let e = self.map.entry((path.clone(), hw)).or_insert_with(|| SourceEntry { slot: SourceSlot::default(), stamp: clock, held: 0 });
        e.stamp = clock;
        e.slot.clone()
    }
    /// Whether nothing but this map uses the source of `e` (no decode in progress, nobody waiting
    /// for it to open).
    fn idle(e: &SourceEntry) -> bool {
        Arc::strong_count(&e.slot) == 1 && e.slot.get().is_none_or(|s| s.as_ref().is_none_or(|s| Arc::strong_count(&s.0) == 1))
    }
    /// Close idle sources, least recently used first, until within the count and byte bounds
    /// (`keep` stays open).
    fn trim(&mut self, keep: &str) {
        loop {
            let held: usize = self.map.values().map(|e| e.held).fold(0, usize::saturating_add);
            if self.map.len() <= MAX_SOURCES && held <= SOURCE_BUDGET {
                return;
            }
            let oldest = self.map.iter().filter(|(p, e)| &*p.0 != keep && Self::idle(e)).min_by_key(|(_, e)| e.stamp).map(|(p, _)| p.clone());
            let Some(p) = oldest else { return };
            self.map.remove(&p);
        }
    }
    /// Close every idle source.
    fn close_idle(&mut self) {
        self.map.retain(|_, e| !Self::idle(e));
    }
}

/// At most this many parsed Photoshop documents are kept (each holds its file) …
const MAX_PSDS: usize = 4;
/// … and at most this many bytes of them (the most recent one always stays).
const PSD_BUDGET: usize = 1 << 29;

/// Parsed Photoshop documents, most recently used last: a document imported as a composition
/// has a footage item per layer, and each decoded frame re-read and re-parsed the whole file.
#[derive(Default)]
struct PsdCache {
    list: Vec<(Arc<str>, Arc<effectcraft_psd::Psd>, usize)>,
}

impl PsdCache {
    fn get(&mut self, path: &str) -> Option<Arc<effectcraft_psd::Psd>> {
        let i = self.list.iter().position(|(p, ..)| &**p == path)?;
        let e = self.list.remove(i);
        let psd = e.1.clone();
        self.list.push(e);
        Some(psd)
    }
    fn insert(&mut self, path: Arc<str>, psd: Arc<effectcraft_psd::Psd>, bytes: usize) {
        self.remove(&path);
        self.list.push((path, psd, bytes));
        while self.list.len() > 1 && (self.list.len() > MAX_PSDS || self.list.iter().map(|e| e.2).fold(0, usize::saturating_add) > PSD_BUDGET) {
            self.list.remove(0);
        }
    }
    fn remove(&mut self, path: &str) {
        self.list.retain(|(p, ..)| &**p != path);
    }
}

/// Bytes a source keeps in memory: decoders that read the file whole keep its bytes (WAV) or its
/// decoded samples (MP3, FLAC); containers read through the file keep their index.
fn held_bytes(src: &SharedSource, whole: bool) -> usize {
    if !whole {
        return 1 << 20;
    }
    let info = src.info();
    let file = info.file_size.unwrap_or(0);
    let decoded =
        info.audio.as_ref().map_or(0.0, |a| info.duration.0 as f64 / TICKS_PER_SECOND as f64 * f64::from(a.sample_rate) * f64::from(a.channels) * 4.0);
    usize::try_from(file).unwrap_or(usize::MAX).max(decoded.clamp(0.0, usize::MAX as f64) as usize)
}
/// Decoded stills that took at least this long are kept in the disk cache (faster ones decode
/// again quicker than their floats read back from disk).
#[cfg(not(test))]
const STILL_TO_DISK: std::time::Duration = std::time::Duration::from_millis(60);
#[cfg(test)]
const STILL_TO_DISK: std::time::Duration = std::time::Duration::ZERO;

#[cfg(test)]
thread_local! {
    /// Tests: stills this thread read back from the disk cache.
    static STILLS_FROM_DISK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// An animated GIF by path, as [`SourceSlot`] (the error message when it failed to load).
type GifSlot = Arc<OnceLock<std::result::Result<Arc<Anim>, String>>>;

struct Inner {
    /// Opened movie/audio sources by path.
    sources: Mutex<Sources>,
    /// Files opened into `sources` (see [`PoolStats::opened`]).
    opened: AtomicU64,
    /// In-memory files registered with `add_bytes` (web builds, tests).
    files: Mutex<HashMap<String, Arc<[u8]>>>,
    /// Parsed Photoshop documents by path.
    psds: Mutex<PsdCache>,
    /// Parsed 3D models by path (`None`: failed to load; not retried until `forget`).
    models: Mutex<HashMap<String, Option<Arc<effectcraft_model::Model>>>>,
    /// Decoded animated GIFs and WebPs by path, loaded by the first caller while the others wait (an
    /// `Err` is not retried until `forget`).
    gifs: Mutex<HashMap<Arc<str>, GifSlot>>,
    cache: Mutex<CacheState>,
    done: Condvar,
    budget: AtomicU64,
    read_ahead: AtomicBool,
    /// Settings ▸ Previews ▸ Hardware ProRes Decoding: preview reads of ProRes footage may use a
    /// registered hardware decoder (off by default; the exact view never does).
    hardware: AtomicBool,
    hits: AtomicU64,
    misses: AtomicU64,
    prefetched: AtomicU64,
    /// Settings ▸ Disk ▸ Conformed Audio Folder: decoded audio tracks written once per file and
    /// rate, read back instead of decoding again (`None` = off).
    conform: Mutex<Option<std::path::PathBuf>>,
    /// The thread pool decode-ahead runners decode in (started with the first runner; `None`:
    /// could not be started).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ahead_pool: OnceLock<Option<rayon::ThreadPool>>,
    /// Conformed files being written right now.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    conforming: Mutex<HashSet<std::path::PathBuf>>,
    /// Conformed files that failed to write (the source didn't decode) this session: their
    /// audio is read from the decoder instead of trying again on every read.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    conform_failed: Mutex<HashSet<std::path::PathBuf>>,
    /// Movies that failed to open or decode: path to (a readable reason, whether it is a decoder
    /// problem rather than an I/O one; see [`FootageSource::problem`]). The first failure of a
    /// file is logged, later ones are not (#886).
    problems: Mutex<HashMap<Arc<str>, (String, bool)>>,
    /// The disk cache decoded stills that are slow to decode are kept in between sessions
    /// ([`MediaPool::set_still_cache`]; `None` = off).
    stills: Mutex<Option<Arc<effectcraft_render::disk_cache::DiskCache>>>,
}

/// Decodes footage for the renderer: stills, image sequences and movies (FilmCraft codecs), plus
/// audio. Thread-safe and cheap to clone (clones share decoders and cache).
///
/// - One decoder per movie: FilmCraft's GOP cache keeps it positioned, so sequential frames
///   decode without re-seeking, and a seek decodes forward from the preceding sync sample.
/// - Converted frames live in an LRU cache bounded by a byte budget ([`DEFAULT_BUDGET`]).
/// - Reading a movie in order (playback, parallel export) starts a decode-ahead on a runner
///   thread (native builds): it decodes the next frames in order, up to a share of the cache
///   budget, so decoding overlaps with the callers' use of the current frames and they find the
///   next ones cached.
/// - Concurrent requests for the same frame decode it once: the others wait for it.
///
/// Hardware decoding (Settings ▸ Previews ▸ Hardware ProRes Decoding, [`crate::hw`]) is for
/// previews only: [`MediaPool::exact`] is a view of the same pool that always decodes exactly,
/// and exports read footage through it ([`FootageSource::exact`]). Frames, open sources and
/// decode-aheads are kept apart by that choice, so the exact view never sees a hardware frame.
#[derive(Clone)]
pub struct MediaPool {
    inner: Arc<Inner>,
    /// This view never uses hardware decoders.
    exact: bool,
    /// The exact view of the same pool (`None` on the exact view itself). One per pool, so jobs
    /// that compare footage sources by address see the same one.
    twin: Option<Arc<MediaPool>>,
}

impl Default for MediaPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a frame comes from.
struct Loc {
    key: Key,
    /// Media time for movies (`None`: an image file, `key.path`).
    media_t: Option<Tick>,
    /// Resampled for a reduced-resolution preview (`key.preview` is its size).
    preview: Option<Preview>,
    /// How float values are encoded (also in `key.interp`).
    float: FloatCurve,
}

/// Removes an in-flight key and wakes waiters, also when decoding panics.
struct Inflight<'a> {
    inner: &'a Inner,
    key: Key,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        let mut c = lock(&self.inner.cache);
        c.inflight.remove(&self.key);
        if c.progress.len() >= 1024 {
            // (a waiter sees this as progress once)
            c.progress.clear();
        }
        let n = c.progress.entry(self.key.path.clone()).or_default();
        *n = n.wrapping_add(1);
        drop(c);
        self.inner.done.notify_all();
    }
}

impl MediaPool {
    /// A pool with the [`DEFAULT_BUDGET`] frame cache.
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_BUDGET)
    }

    /// A pool whose frame cache holds at most `bytes` of decoded frames.
    pub fn with_budget(bytes: usize) -> Self {
        let inner = Arc::new(Inner {
            sources: Mutex::default(),
            files: Mutex::default(),
            models: Mutex::default(),
            psds: Mutex::default(),
            gifs: Mutex::default(),
            cache: Mutex::default(),
            done: Condvar::new(),
            budget: AtomicU64::new(bytes as u64),
            read_ahead: AtomicBool::new(cfg!(not(target_arch = "wasm32"))),
            hardware: AtomicBool::new(false),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            prefetched: AtomicU64::new(0),
            opened: AtomicU64::new(0),
            conform: Mutex::default(),
            ahead_pool: OnceLock::new(),
            conforming: Mutex::default(),
            conform_failed: Mutex::default(),
            problems: Mutex::default(),
            stills: Mutex::default(),
        });
        let twin = Some(Arc::new(Self { inner: inner.clone(), exact: true, twin: None }));
        Self { inner, exact: false, twin }
    }

    /// Settings ▸ Previews ▸ Hardware ProRes Decoding: let this pool's preview reads of ProRes
    /// footage use a registered hardware decoder ([`crate::hw`]). Off by default; the
    /// [`MediaPool::exact`] view never does.
    pub fn set_hardware_decoding(&self, on: bool) {
        self.inner.hardware.store(on, Ordering::Relaxed);
    }

    /// Keep decoded stills that are slow to decode (a PDF or SVG with big embedded pictures, a
    /// huge PNG: [`STILL_TO_DISK`]) in `disk` between sessions, read back instead of decoding
    /// again after the app restarts (`None` = off).
    pub fn set_still_cache(&self, disk: Option<Arc<effectcraft_render::disk_cache::DiskCache>>) {
        *lock(&self.inner.stills) = disk;
    }

    /// The view of this pool that always decodes exactly (exports, final renders): the same
    /// cache and files, never a hardware-decoded frame.
    pub fn exact(&self) -> &MediaPool {
        self.twin.as_deref().unwrap_or(self)
    }

    /// Whether this view decodes `footage` on a hardware decoder.
    fn hw_for(&self, footage: &Footage) -> bool {
        !self.exact
            && footage.kind == FootageKind::Video
            && self.inner.hardware.load(Ordering::Relaxed)
            && crate::hw::available()
            && crate::hw::takes_codec(&footage.codec)
    }

    pub fn budget(&self) -> usize {
        self.inner.budget()
    }

    /// Change the frame-cache budget (evicts immediately when shrinking).
    pub fn set_budget(&self, bytes: usize) {
        self.inner.budget.store(bytes as u64, Ordering::Relaxed);
        lock(&self.inner.cache).lru.evict(bytes);
    }

    /// Take part in the shared cache budget (`Some`, see
    /// [`effectcraft_render::cache::shared`]) or keep to this cache's own budget (`None`). The
    /// budget asks it to evict its least recently used frame (the newest always stays). Both
    /// views of the pool ([`MediaPool::exact`]) share the one cache.
    pub fn share(&self, budget: Option<&Arc<SharedBudget>>) {
        let link = budget.and_then(|b| b.join(ShareKind::Media, Arc::new(MediaMember(Arc::downgrade(&self.inner)))));
        let old = {
            let mut c = lock(&self.inner.cache);
            let old = std::mem::replace(&mut c.lru.share, link);
            c.lru.report();
            old
        };
        if let Some(old) = old {
            old.leave();
        }
    }

    /// Enable or disable decoding ahead of sequential reads (on by default on native builds;
    /// ignored on wasm32).
    pub fn set_read_ahead(&self, on: bool) {
        self.inner.read_ahead.store(on && cfg!(not(target_arch = "wasm32")), Ordering::Relaxed);
    }

    pub fn stats(&self) -> PoolStats {
        let i = &self.inner;
        let c = lock(&i.cache);
        PoolStats {
            hits: i.hits.load(Ordering::Relaxed),
            misses: i.misses.load(Ordering::Relaxed),
            prefetched: i.prefetched.load(Ordering::Relaxed),
            frames: c.lru.map.len(),
            bytes: c.lru.bytes,
            opened: i.opened.load(Ordering::Relaxed),
        }
    }

    /// The frame cache by file (the Memory panel): the lock is held to copy (path, bytes) out,
    /// the totals are made after it.
    pub fn cache_usage(&self) -> effectcraft_render::FootageCacheUsage {
        let entries: Vec<(Arc<str>, usize)> = lock(&self.inner.cache).lru.map.iter().map(|(k, e)| (k.path.clone(), e.bytes)).collect();
        let mut by: HashMap<Arc<str>, (usize, usize)> = HashMap::new();
        for (path, bytes) in &entries {
            let e = by.entry(path.clone()).or_default();
            e.0 = e.0.saturating_add(*bytes);
            e.1 += 1;
        }
        let mut by_file: Vec<(String, usize, usize)> = by.into_iter().map(|(p, (b, n))| (p.to_string(), b, n)).collect();
        by_file.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        effectcraft_render::FootageCacheUsage { bytes: entries.iter().map(|e| e.1).fold(0usize, usize::saturating_add), frames: entries.len(), by_file }
    }

    /// Drop every cached frame (decoders stay open).
    pub fn clear_frames(&self) {
        let mut c = lock(&self.inner.cache);
        // (the shared budget's link stays)
        c.lru = Lru { share: c.lru.share.take(), ..Lru::default() };
        c.lru.report();
        c.last.clear();
        // Running decode-aheads stop.
        c.ahead.clear();
        drop(c);
        // Decoded GIFs sit outside the frame budget; Purge frees them too.
        lock(&self.inner.gifs).clear();
    }

    /// Drop every cached frame and decoder.
    pub fn clear(&self) {
        self.clear_frames();
        lock(&self.inner.sources).map.clear();
        lock(&self.inner.problems).clear();
    }

    /// Movie and audio files open now.
    pub fn open_files(&self) -> usize {
        lock(&self.inner.sources).map.len()
    }

    /// Forget every file `keep` doesn't name: close its decoder, drop its parsed model and its
    /// cached frames (after footage is removed from the project, or another project is opened).
    pub fn retain_files(&self, keep: &dyn Fn(&str) -> bool) {
        let mut gone: Vec<Arc<str>> = lock(&self.inner.sources).map.keys().filter(|p| !keep(&p.0)).map(|p| p.0.clone()).collect();
        gone.sort();
        gone.dedup();
        for p in &gone {
            self.forget(p);
        }
        lock(&self.inner.models).retain(|p, _| keep(p));
        lock(&self.inner.psds).list.retain(|(p, ..)| keep(p));
    }

    /// Forget a file (after it changed on disk, or to retry a file that failed to open).
    pub fn forget(&self, path: &str) {
        lock(&self.inner.sources).map.retain(|k, _| &*k.0 != path);
        lock(&self.inner.models).remove(path);
        lock(&self.inner.psds).remove(path);
        lock(&self.inner.gifs).remove(path);
        lock(&self.inner.problems).remove(path);
        let mut c = lock(&self.inner.cache);
        let keys: Vec<Key> = c.lru.map.keys().filter(|k| &*k.path == path).cloned().collect();
        for k in keys {
            c.lru.remove(&k);
        }
        c.last.remove(path);
        c.ahead.remove(path);
        drop(c);
        crate::exr_channels::forget(path);
    }

    /// Provide the contents of `path` from memory (used instead of the file system; web builds).
    pub fn add_bytes(&self, path: &str, bytes: Arc<[u8]>) {
        lock(&self.inner.files).insert(path.to_string(), bytes);
        self.forget(path);
    }

    /// Decode (or fetch from cache) the frame of `footage` at source time `t`.
    pub fn frame_at(&self, footage: &Footage, t: Tick) -> Result<Arc<Image>> {
        Ok(Inner::pixels_at(&self.inner, footage, t, true, None, self.hw_for(footage), FloatCurve::Srgb)?.image())
    }

    /// Settings ▸ Disk ▸ Conformed Audio Folder: write each footage file's decoded audio there
    /// once per sample rate (raw interleaved stereo `f32`) and read it back instead of decoding
    /// again. `None` turns it off.
    pub fn set_conform_folder(&self, folder: Option<std::path::PathBuf>) {
        *lock(&self.inner.conform) = folder;
    }

    /// The conformed-audio file of `path` at `rate` (named by a hash of the path, size and
    /// modification time, so an edited file conforms again, and of [`CONFORM_VERSION`]).
    pub fn conformed_path(&self, path: &str, rate: u32) -> Option<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let folder = lock(&self.inner.conform).clone()?;
        let meta = std::fs::metadata(path).ok()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (CONFORM_VERSION, path, meta.len(), meta.modified().ok()).hash(&mut h);
        Some(folder.join(format!("{:016x}_{rate}.ecaf", h.finish())))
    }

    /// Samples from a conformed file: `Some` when it exists and covers the request.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn read_conformed(file: &std::path::Path, s0: usize, frames: usize) -> Option<Vec<f32>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(file).ok()?;
        let len = f.metadata().ok()?.len() as usize / 8;
        let mut out = vec![0.0f32; frames * 2];
        if s0 >= len {
            return Some(out);
        }
        let n = frames.min(len - s0);
        f.seek(SeekFrom::Start(s0 as u64 * 8)).ok()?;
        let mut bytes = vec![0u8; n * 8];
        f.read_exact(&mut bytes).ok()?;
        for (o, c) in out.iter_mut().zip(bytes.as_chunks::<4>().0.iter()) {
            *o = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        Some(out)
    }

    /// Conform `frames` sample frames of `footage`'s audio at `rate` into `file` on a worker
    /// thread (once).
    #[cfg(not(target_arch = "wasm32"))]
    fn conform(&self, footage: &Footage, file: std::path::PathBuf, frames: usize, rate: u32) {
        if lock(&self.inner.conform_failed).contains(&file) || !lock(&self.inner.conforming).insert(file.clone()) {
            return;
        }
        let (pool, f) = (self.clone(), footage.clone());
        let spawned = std::thread::Builder::new().name("ec-conform-audio".into()).spawn(move || {
            let tmp = file.with_extension("tmp");
            let written = match file.parent().map(std::fs::create_dir_all) {
                Some(Ok(())) => pool.write_conformed(&f, &tmp, frames, rate).and_then(|()| std::fs::rename(&tmp, &file)),
                Some(Err(e)) => Err(e),
                None => Err(std::io::Error::other("no folder")),
            };
            // A failed decode leaves no file: one written silent would keep the footage silent
            // in every later session (#324).
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                log::warn!("media: could not write conformed audio {} of {}: {e}", file.display(), f.path);
                lock(&pool.inner.conform_failed).insert(file.clone());
            }
            lock(&pool.inner.conforming).remove(&file);
        });
        if spawned.is_err() {
            lock(&self.inner.conforming).clear();
        }
    }

    /// Write `frames` sample frames of `footage`'s audio at `rate` to `path` (raw interleaved
    /// stereo `f32`) a chunk at a time: the whole track was held in memory before it was written.
    /// Fails when any chunk doesn't decode.
    #[cfg(not(target_arch = "wasm32"))]
    fn write_conformed(&self, footage: &Footage, path: &std::path::Path, frames: usize, rate: u32) -> std::io::Result<()> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut at = 0usize;
        while at < frames {
            let n = (frames - at).min(1 << 16);
            for v in self.try_decode_audio(footage, sample_time(at, rate), n, rate).map_err(std::io::Error::other)? {
                out.write_all(&v.to_le_bytes())?;
            }
            at += n;
        }
        out.flush()
    }

    /// `frames` stereo sample frames of `footage`'s audio starting at source time `start`, at
    /// `rate` Hz, interleaved (L R L R …). Mono is duplicated to both channels; multichannel
    /// sources are downmixed to stereo ([`crate::downmix`]). Silence where there is no audio (or before the media starts).
    /// With a conformed audio folder, reads come from the conformed file once it is written.
    pub fn audio_samples(&self, footage: &Footage, start: Tick, frames: usize, rate: u32) -> Vec<f32> {
        if !footage.has_audio || footage.missing || rate == 0 || frames == 0 {
            return vec![0.0; frames * 2];
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(conform) = conform_frames(footage, rate)
            && let Some(file) = self.conformed_path(&footage.path, rate)
        {
            let s0 = start.to_units_floor(rate as i64);
            if file.exists() && !lock(&self.inner.conforming).contains(&file) {
                let skip = (-s0).max(0) as usize;
                if skip >= frames {
                    return vec![0.0; frames * 2];
                }
                if let Some(v) = Self::read_conformed(&file, s0.max(0) as usize, frames - skip) {
                    let mut out = vec![0.0; skip * 2];
                    out.extend(v);
                    return out;
                }
            } else {
                self.conform(footage, file, conform, rate);
            }
        }
        self.decode_audio(footage, start, frames, rate)
    }

    /// [`MediaPool::audio_samples`] straight from the decoder. The source is read at its own
    /// rate and resampled here (linear, by absolute sample position, so a range comes out the
    /// same however it is split into reads). FilmCraft's MP4 reader, asked for another rate,
    /// applied the edit list's priming offset twice and ramped the start of every read: AAC
    /// footage not at the mix rate stuttered in previews and exports (#274).
    fn decode_audio(&self, footage: &Footage, start: Tick, frames: usize, rate: u32) -> Vec<f32> {
        self.try_decode_audio(footage, start, frames, rate).unwrap_or_else(|e| {
            log::warn!("media: audio of {}: {e}", footage.path);
            vec![0.0; frames * 2]
        })
    }

    /// [`MediaPool::decode_audio`], failing when the source can't be opened or read.
    fn try_decode_audio(&self, footage: &Footage, start: Tick, frames: usize, rate: u32) -> std::result::Result<Vec<f32>, String> {
        let path: Arc<str> = footage.path.as_str().into();
        let src = self.inner.source(&path, false).ok_or("the file can't be opened")?;
        let s0 = start.to_units_floor(rate as i64);
        let native = src.info().audio.as_ref().map_or(rate, |a| a.sample_rate);
        if native == 0 || rate == 0 || native == rate {
            return read_stereo(&src, s0, frames, rate);
        }
        // Output sample `n` sits at source position n × native / rate.
        let (native_i, rate_i) = (i128::from(native), i128::from(rate));
        let first = (i128::from(s0) * native_i).div_euclid(rate_i);
        let last = ((i128::from(s0) + frames as i128) * native_i).div_euclid(rate_i) + 1;
        let (Ok(first64), Ok(count)) = (i64::try_from(first), usize::try_from(last - first + 1)) else { return Ok(vec![0.0; frames * 2]) };
        let buf = read_stereo(&src, first64, count, native)?;
        let mut out = Vec::with_capacity(frames * 2);
        for k in 0..frames as i128 {
            let pos = (i128::from(s0) + k) * native_i;
            let i = usize::try_from(pos.div_euclid(rate_i) - first).unwrap_or(0) * 2;
            let f = (pos.rem_euclid(rate_i) as f64 / rate as f64) as f32;
            for c in 0..2 {
                let a = buf.get(i + c).copied().unwrap_or(0.0);
                let b = buf.get(i + 2 + c).copied().unwrap_or(a);
                out.push(a + (b - a) * f);
            }
        }
        Ok(out)
    }

    /// A thumbnail of `footage` (its first frame) fitting in `max_side` × `max_side`, aspect kept.
    /// A cached frame is reused; otherwise the frame is decoded without being cached and a movie
    /// not open already is closed again (browsing a folder doesn't fill memory with its files).
    pub fn thumbnail(&self, footage: &Footage, max_side: u32) -> Result<Image> {
        if !footage.has_video {
            return Err(MediaError::Unsupported(format!("{}: no video", footage.path)));
        }
        let loc = Inner::locate(footage, Tick::ZERO, FloatCurve::Srgb);
        let cached = lock(&self.inner.cache).lru.get(&loc.key);
        let img = match cached {
            Some(px) => px.image(),
            None => self.inner.decode(&loc, footage, true)?.image(),
        };
        let (w, h) = (img.width.max(1), img.height.max(1));
        let s = (max_side.max(1) as f64 / w.max(h) as f64).min(1.0);
        if s >= 1.0 {
            return Ok((*img).clone());
        }
        let (tw, th) = (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1));
        Ok(effectcraft_raster::resample(&img, tw, th))
    }
}

impl Inner {
    fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed) as usize
    }

    /// A 3D model file and its sibling resources (buffers, textures, MTL files), parsed once.
    fn model(&self, path: &str) -> Option<Arc<effectcraft_model::Model>> {
        if let Some(m) = lock(&self.models).get(path) {
            return m.clone();
        }
        let loaded = self.read(path).map_err(|e| e.to_string()).and_then(|bytes| {
            let dir = std::path::Path::new(path).parent().map(|d| d.to_path_buf()).unwrap_or_default();
            let resolve = |uri: &str| -> Option<Vec<u8>> {
                let p = dir.join(uri);
                self.read(&p.to_string_lossy()).ok().map(|b| b.to_vec())
            };
            effectcraft_model::load(path, &bytes, &resolve).map_err(|e| e.to_string())
        });
        let m = match loaded {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                log::warn!("media: cannot load model {path}: {e}");
                None
            }
        };
        lock(&self.models).entry(path.to_string()).or_insert(m).clone()
    }

    fn read(&self, path: &str) -> Result<Arc<[u8]>> {
        if let Some(b) = lock(&self.files).get(path) {
            return Ok(b.clone());
        }
        crate::open::read_file(path)
    }

    /// The opened movie/audio source for `path`. The first caller opens it, outside the map's
    /// lock (parsing a large file takes a moment; FLAC, MP3 and AIFF files are decoded whole);
    /// callers asking for the same file meanwhile wait for that instead of opening it again: a
    /// preview's frame workers, audio feeder, conform and waveform threads each decoded a long
    /// FLAC in full at once (#408). Containers are read through the file (index now, samples on
    /// demand), not held in memory.
    fn source(&self, path: &Arc<str>, hw: bool) -> Option<SharedSource> {
        self.opened(path, hw).map(|(src, _)| src)
    }

    /// [`Self::source`] and the turn the movie's picture is shown with
    /// ([`crate::probe::video_turn`]).
    fn opened(&self, path: &Arc<str>, hw: bool) -> Option<(SharedSource, Orientation)> {
        let slot = lock(&self.sources).slot(path, hw);
        let mut held = None;
        let t0 = std::time::Instant::now();
        let _slow = SlowOpen(path, t0);
        let src = slot
            .get_or_init(|| {
                self.opened.fetch_add(1, Ordering::Relaxed);
                let mem = lock(&self.files).get(&**path).cloned();
                let turn = movie_turn(path, mem.as_deref());
                match crate::open::open_source(path, mem) {
                    Ok((s, whole)) => {
                        held = Some(held_bytes(&s, whole));
                        Some((s, turn))
                    }
                    Err(e) => {
                        self.note_problem(path, "", &e);
                        None
                    }
                }
            })
            .clone();
        if let Some(h) = held {
            let mut sources = lock(&self.sources);
            if let Some(e) = sources.map.get_mut(&(path.clone(), hw))
                && Arc::ptr_eq(&e.slot, &slot)
            {
                e.held = h;
            }
            drop(slot);
            sources.trim(path);
        }
        src
    }

    /// Remember why the movie at `path` shows no picture (`codec` is the footage's, when known)
    /// and log it the first time: a decoder problem replaces an I/O one (a file that can't be
    /// opened reads as "cannot open" on every frame after the first, more telling, error).
    fn note_problem(&self, path: &Arc<str>, codec: &str, e: &MediaError) {
        let decoder = !matches!(e, MediaError::Io(_) | MediaError::Cancelled);
        if matches!(e, MediaError::Cancelled) {
            return;
        }
        let mut p = lock(&self.problems);
        if p.get(path).is_some_and(|(_, d)| *d || !decoder) {
            return;
        }
        let reason = if decoder { describe_problem(codec, e) } else { e.to_string() };
        log::warn!("media: {path}: {reason}");
        p.insert(path.clone(), (reason, decoder));
    }

    /// The source of `path` when it is open already.
    fn open_source(&self, path: &Arc<str>, hw: bool) -> Option<(SharedSource, Orientation)> {
        lock(&self.sources).map.get(&(path.clone(), hw)).and_then(|e| e.slot.get().cloned().flatten())
    }

    /// The decoded frames of the animated GIF or WebP at `path`. The first caller decodes it, outside the
    /// map's lock; callers asking for the same file meanwhile wait for that.
    fn gif(&self, path: &Arc<str>) -> Result<Arc<Anim>> {
        let slot = lock(&self.gifs).entry(path.clone()).or_default().clone();
        let loaded = slot.get_or_init(|| {
            let loaded = self.read(path).and_then(|bytes| Anim::load(&bytes));
            loaded.map(Arc::new).map_err(|e| {
                log::warn!("media: cannot decode {path}: {e}");
                e.to_string()
            })
        });
        loaded.clone().map_err(|e| MediaError::Decode(format!("{path}: {e}")))
    }

    /// The frame index (in the footage's interpreted rate) shown at source time `t`, after
    /// looping (`loop_count`) and clamping to the footage's frames; `n` = frames per loop.
    fn frame_index(rate: FrameRate, t: Tick, n: i64, loops: u32) -> i64 {
        let n = n.max(1);
        let total = n.saturating_mul(loops.max(1) as i64);
        rate.frame_at(t).clamp(0, total - 1) % n
    }

    /// Number of frames in `d` at `rate` (rounded).
    fn frame_count(rate: FrameRate, d: Tick) -> i64 {
        let den = TICKS_PER_SECOND as i128 * rate.den as i128;
        ((d.0 as i128 * rate.num as i128 + den / 2) / den) as i64
    }

    fn locate(footage: &Footage, t: Tick, float: FloatCurve) -> Loc {
        let alpha = match footage.alpha {
            AlphaMode::Straight => 0,
            AlphaMode::Premultiplied => 1,
            AlphaMode::Ignore => 2,
        };
        let float = AlphaOp::of(footage, float).float_curve();
        let interp = alpha
            | (match float {
                FloatCurve::Srgb => 0,
                FloatCurve::Linear => 1,
                FloatCurve::Gamma24 => 2,
            } << 2);
        let matte = if footage.alpha == AlphaMode::Premultiplied { footage.premul_color.map(f32::to_bits) } else { [0; 3] };
        let crop = footage.layer.as_ref().and_then(|l| l.crop_bounds).map(|b| b.map(f64::to_bits));
        let key = |path: &str, frame| Key { path: path.into(), frame, interp, matte, crop, preview: None, raster: 0, hw: false };
        match footage.kind {
            FootageKind::Sequence if !footage.sequence.is_empty() => {
                let i = Self::frame_index(footage.frame_rate, t, footage.sequence_frames(), footage.loop_count);
                match footage.sequence_file(i) {
                    Some(path) => Loc { key: key(path, 0), media_t: None, preview: None, float },
                    // A gap in the numbering: colour bars, as in After Effects.
                    None => Loc { key: key(PLACEHOLDER, ((footage.width as i64) << 32) | footage.height as i64), media_t: None, preview: None, float },
                }
            }
            // A layer of a layered still is keyed by its layer index and size mode (smart objects
            // by their embedded file; PDF pages by their number).
            FootageKind::Still if footage.layer.is_some() || footage.page != 0 => {
                let l = footage.layer.as_ref().map_or(0, |l| 1 + l.index as i64 * 2 + l.layer_size as i64 + if l.embedded.is_some() { 1 << 40 } else { 0 })
                    + ((footage.page as i64) << 42);
                Loc { key: key(&footage.path, l), media_t: None, preview: None, float }
            }
            FootageKind::Still | FootageKind::Sequence | FootageKind::Model | FootageKind::Data => {
                Loc { key: key(&footage.path, 0), media_t: None, preview: None, float }
            }
            FootageKind::Video | FootageKind::Audio => {
                let rate = footage.frame_rate;
                let n = Self::frame_count(rate, footage.duration);
                let i = Self::frame_index(rate, t, n, footage.loop_count);
                // Middle of the frame in the file's own timing (robust to timestamp rounding).
                let native = footage.native_rate.unwrap_or(rate);
                let mt = native.tick_of(i) + Tick(native.frame_duration().0 / 2);
                Loc { key: key(&footage.path, i), media_t: Some(mt), preview: None, float }
            }
        }
    }

    #[cfg(test)]
    fn frame_at(self: &Arc<Self>, footage: &Footage, t: Tick, playback: bool) -> Result<Arc<Image>> {
        Ok(self.pixels_at(footage, t, playback, None, false, FloatCurve::Srgb)?.image())
    }

    /// Where the frame of `footage` at `t` comes from, resampled for `preview` when the footage's
    /// frames are larger than the preview needs, decoded on a hardware decoder when `hw`.
    fn locate_for(footage: &Footage, t: Tick, preview: Option<Preview>, hw: bool, float: FloatCurve) -> Loc {
        let mut loc = Self::locate(footage, t, float);
        loc.key.hw = hw && loc.media_t.is_some();
        if let Some(p) = preview
            && let Some((w, h)) = preview_size(footage.width, p.w, p.h, p.s)
        {
            loc.key.preview = Some([w, h]);
            loc.preview = Some(p);
        }
        loc
    }

    /// The cached (or newly decoded) pixels of `footage` at source time `t` (resampled for
    /// `preview`; on a hardware decoder when `hw`).
    #[allow(clippy::too_many_arguments)]
    fn pixels_at(self: &Arc<Self>, footage: &Footage, t: Tick, playback: bool, preview: Option<Preview>, hw: bool, float: FloatCurve) -> Result<Pixels> {
        if !footage.has_video {
            return Err(MediaError::Unsupported(format!("{}: no video", footage.path)));
        }
        let loc = Self::locate_for(footage, t, preview, hw, float);
        let raw = footage.frame_rate.frame_at(t);
        let mut c = lock(&self.cache);
        // a movie read in order: decode the next frames ahead
        if playback && loc.media_t.is_some() && !footage.codec.eq_ignore_ascii_case("gif") && self.read_ahead.load(Ordering::Relaxed) {
            self.schedule(&mut c, footage, preview, float, &loc.key, raw);
        }
        let me = std::thread::current().id();
        // (last progress seen, when it was seen): rayon workers stop waiting without progress
        let mut seen: Option<(u64, std::time::Instant)> = None;
        let wait = Waited::new(&loc.key.path, raw);
        loop {
            if let Some(px) = c.lru.get(&loc.key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                wait.done("cached");
                return Ok(px);
            }
            let owner = c.inflight.get(&loc.key).copied();
            let scheduled = owner.is_none()
                && c.ahead.get(&loc.key.path).is_some_and(|a| a.runner.is_some() && same_stream(&a.stream, &loc.key) && (a.next..a.until).contains(&raw));
            if owner.is_none() && !scheduled {
                break;
            }
            // This thread is decoding the frame further down its stack (a rayon worker that took
            // this job while its decoder waited on a parallel join): waiting would wait on itself.
            if owner == Some(me) {
                self.misses.fetch_add(1, Ordering::Relaxed);
                drop(c);
                return self.decode(&loc, footage, false);
            }
            wait.on(if owner.is_some() { "another thread" } else { "the decode-ahead" });
            // Another thread or the decode-ahead is decoding the frame: wait for it. A rayon
            // worker waits only while decoding progresses: the decoding thread may wait on a
            // parallel job queued behind this worker's own stack, which it can't run while it
            // waits here. Without progress for a while it decodes a private copy instead.
            if rayon::current_thread_index().is_some() {
                let now = std::time::Instant::now();
                let progress = c.progress.get(&loc.key.path).copied().unwrap_or(0);
                let since = match seen {
                    Some((p, at)) if p == progress => at,
                    _ => now,
                };
                seen = Some((progress, since));
                let left = STALL.saturating_sub(now.saturating_duration_since(since));
                if left.is_zero() {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    drop(c);
                    wait.done("stalled, decoding it here");
                    // Kept: every layer showing the same still waited [`STALL`] again for the
                    // stuck decode (four layers of one JPEG: 4 s a frame). The owner's copy,
                    // when it comes, replaces it (the same pixels).
                    let px = self.decode(&loc, footage, false)?;
                    lock(&self.cache).lru.insert(loc.key, px.clone(), self.budget());
                    self.done.notify_all();
                    return Ok(px);
                }
                c = match self.done.wait_timeout(c, left) {
                    Ok((g, _)) => g,
                    Err(e) => e.into_inner().0,
                };
            } else {
                c = self.done.wait(c).unwrap_or_else(|e| e.into_inner());
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        c.inflight.insert(loc.key.clone(), me);
        drop(c);
        wait.done("not decoding, decoding it here");
        let _guard = Inflight { inner: self, key: loc.key.clone() };
        let t0 = std::time::Instant::now();
        let px = self.decode(&loc, footage, false)?;
        slow("decoded", &loc.key.path, raw, t0.elapsed());
        lock(&self.cache).lru.insert(loc.key, px.clone(), self.budget());
        Ok(px)
    }

    /// A read of frame `raw` (key `key`) of a movie: when it follows the previous read closely,
    /// extend the movie's decode-ahead past it (starting a runner when none is running).
    fn schedule(self: &Arc<Self>, c: &mut CacheState, footage: &Footage, preview: Option<Preview>, float: FloatCurve, key: &Key, raw: i64) {
        let prev = c.last.insert(key.path.clone(), raw);
        if !prev.is_some_and(|p| p != raw && p.abs_diff(raw) <= SEQ_SLACK.unsigned_abs()) {
            return;
        }
        let estimate = (footage.width as usize).saturating_mul(footage.height as usize).saturating_mul(3) / 2 + 64;
        let frame_bytes = c.ahead.get(&key.path).filter(|a| same_stream(&a.stream, key)).map_or(estimate, |a| a.frame_bytes).max(1);
        let window = i64::try_from(self.budget() / AHEAD_SHARE / frame_bytes).unwrap_or(MAX_AHEAD).min(MAX_AHEAD);
        if window < 1 {
            return;
        }
        let total = Self::frame_count(footage.frame_rate, footage.duration).max(1).saturating_mul(i64::from(footage.loop_count.max(1)));
        let until = raw.saturating_add(1).saturating_add(window).min(total);
        let running = c.ahead.values().filter(|a| a.runner.is_some()).count();
        let a = c.ahead.entry(key.path.clone()).or_insert_with(|| Ahead {
            footage: footage.clone(),
            preview,
            float,
            stream: Key { frame: 0, ..key.clone() },
            next: raw.saturating_add(1),
            until,
            frame_bytes,
            runner: None,
        });
        let jump = raw < a.next.saturating_sub(SEQ_SLACK) || raw >= a.until;
        if !same_stream(&a.stream, key) || a.footage != *footage || a.preview != preview || a.runner.is_none() || jump {
            // another stream of the file, a stopped runner or a jump: start over after this frame,
            // with a window from here (keeping the old far end after a backward jump or a loop
            // wrap decoded the whole clip past the window and the memory budget)
            if !same_stream(&a.stream, key) {
                a.frame_bytes = estimate;
            }
            a.footage = footage.clone();
            a.preview = preview;
            a.float = float;
            a.stream = Key { frame: 0, ..key.clone() };
            a.next = raw.saturating_add(1);
            a.until = until;
        }
        a.until = a.until.max(until).min(total);
        if a.runner.is_some() || a.next >= a.until || running >= MAX_RUNNERS {
            return;
        }
        c.runners = c.runners.wrapping_add(1);
        let id = c.runners;
        if self.start_runner(key.path.clone(), id)
            && let Some(a) = c.ahead.get_mut(&key.path)
        {
            a.runner = Some(id);
        }
    }

    /// Start the decode-ahead runner `id` of `path`: a plain thread, not a rayon job (rayon
    /// workers wait for the frames it decodes). It decodes in its own thread pool, so codecs that
    /// parallelise with rayon don't queue their work behind the global pool's waiting workers.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_runner(self: &Arc<Self>, path: Arc<str>, id: u64) -> bool {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let pool =
            self.ahead_pool.get_or_init(|| rayon::ThreadPoolBuilder::new().num_threads(threads).thread_name(|i| format!("ec-media-ahead-{i}")).build().ok());
        if pool.is_none() {
            return false;
        }
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("ec-media-decode-ahead".into())
            .spawn(move || {
                let _stop = RunnerStop { inner: weak.clone(), path: path.clone(), id };
                while let Some(inner) = weak.upgrade() {
                    if !inner.run_ahead(&path, id) {
                        return;
                    }
                }
            })
            .is_ok()
    }

    #[cfg(target_arch = "wasm32")]
    fn start_runner(self: &Arc<Self>, _path: Arc<str>, _id: u64) -> bool {
        false
    }

    /// One step of runner `id` of `path`: decode its next frames (skipping frames cached or being
    /// decoded already), [`ahead_batch`] of them at once for intra-only codecs, then cache them
    /// in frame order. `false` when the runner is done (window decoded, stopped or replaced).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn run_ahead(&self, path: &Arc<str>, id: u64) -> bool {
        let mut c = lock(&self.cache);
        let Some(a) = c.ahead.get_mut(path).filter(|a| a.runner == Some(id)) else { return false };
        if a.next >= a.until {
            a.runner = None;
            drop(c);
            self.done.notify_all();
            return false;
        }
        let (footage, preview, hw, float) = (a.footage.clone(), a.preview, a.stream.hw, a.float);
        let batch = ahead_batch(&footage.codec);
        let rate = footage.frame_rate;
        // (frame index, where it comes from) of the frames this step decodes
        let mut todo: Vec<(i64, Loc)> = Vec::with_capacity(batch);
        while todo.len() < batch {
            let Some(a) = c.ahead.get_mut(path) else { break };
            if a.next >= a.until {
                break;
            }
            let raw = a.next;
            a.next = a.next.saturating_add(1);
            let t = rate.tick_of(raw) + Tick(rate.frame_duration().0 / 2);
            let loc = Self::locate_for(&footage, t, preview, hw, float);
            if c.lru.map.contains_key(&loc.key) || c.inflight.contains_key(&loc.key) || todo.iter().any(|(_, l)| l.key == loc.key) {
                continue;
            }
            todo.push((raw, loc));
        }
        if todo.is_empty() {
            return true;
        }
        let me = std::thread::current().id();
        for (_, loc) in &todo {
            c.inflight.insert(loc.key.clone(), me);
        }
        drop(c);
        let guards: Vec<Inflight> = todo.iter().map(|(_, loc)| Inflight { inner: self, key: loc.key.clone() }).collect();
        let decode_all = || -> Vec<Result<Pixels>> {
            match todo.len() {
                1 => todo.iter().map(|(_, loc)| self.decode(loc, &footage, false)).collect(),
                _ => {
                    use rayon::prelude::*;
                    todo.par_iter().map(|(_, loc)| self.decode(loc, &footage, false)).collect()
                }
            }
        };
        let t0 = std::time::Instant::now();
        let decoded = match self.ahead_pool.get().and_then(Option::as_ref) {
            Some(pool) => pool.install(decode_all),
            None => decode_all(),
        };
        slow("decoded ahead (a batch)", path, todo.first().map_or(-1, |(raw, _)| *raw), t0.elapsed());
        let mut c = lock(&self.cache);
        // in frame order, up to the first frame that failed (as decoding one at a time would)
        for ((raw, loc), px) in todo.into_iter().zip(decoded) {
            match px {
                Ok(px) => {
                    let bytes = px.bytes();
                    c.lru.insert(loc.key, px, self.budget());
                    if let Some(a) = c.ahead.get_mut(path).filter(|a| a.runner == Some(id)) {
                        a.frame_bytes = bytes;
                    }
                    self.prefetched.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    log::debug!("media: decode-ahead of {path} at frame {raw}: {e}");
                    if let Some(a) = c.ahead.get_mut(path).filter(|a| a.runner == Some(id)) {
                        a.until = a.next;
                    }
                    break;
                }
            }
        }
        drop(c);
        drop(guards);
        true
    }

    /// Decode the frame at `loc`: movies and 8 / 16-bit stills are kept as their samples
    /// ([`Packed`]), float stills, layered and vector documents as float pixels.
    ///
    /// `detached`: a movie not open already is opened for this frame only (thumbnails).
    fn decode(&self, loc: &Loc, footage: &Footage, detached: bool) -> Result<Pixels> {
        let _profile = effectcraft_render::profile::span("decode");
        // A reduced-resolution preview: the full frame (cached, or decoded and not kept)
        // resampled exactly as the renderer resamples it.
        if let (Some(p), Some([rw, rh])) = (loc.preview, loc.key.preview) {
            let base = Loc { key: Key { preview: None, ..loc.key.clone() }, media_t: loc.media_t, preview: None, float: loc.float };
            let cached = lock(&self.cache).lru.get(&base.key);
            let full = match cached {
                Some(px) => px,
                // The full frame, decoded and not kept, then resampled exactly as the renderer
                // resamples it (bit-identical to a full-size render scaled down; the faster
                // quarter-pixel 4:2:0 conversion differs where colours clip).
                None => match base.media_t.filter(|_| !gif_anim::is_animated(footage)) {
                    Some(mt) => {
                        let (frame, turn) = self.video_frame(&base.key.path, base.key.hw, mt, detached)?;
                        video_pixels(frame, AlphaOp::of(footage, loc.float), turn)
                    }
                    None => self.decode(&base, footage, detached)?,
                },
            };
            let img = full.image();
            if preview_size(img.width, p.w, p.h, p.s).is_none() {
                return Ok(full);
            }
            return Ok(Pixels::Preview(Arc::new(effectcraft_raster::resample(&img, rw, rh))));
        }
        let op = AlphaOp::of(footage, loc.float);
        let path = &loc.key.path;
        let float = |img: Image| Pixels::Float(Arc::new(img));
        match loc.media_t {
            None if &**path == PLACEHOLDER => Ok(float(colour_bars(footage.width, footage.height))),
            None => {
                // A still that was slow to decode comes from the disk cache (exact pixels).
                let disk = lock(&self.stills).clone().filter(|_| !lock(&self.files).contains_key(&**path)).and_then(|dc| {
                    let how = format!(
                        "{:?} {:?} {:?} {:?} {:?} {} {:?}",
                        loc.key.frame, loc.key.interp, loc.key.matte, loc.key.crop, footage.layer, footage.page, op
                    );
                    Some((dc, effectcraft_render::disk_cache::still_key(path, &how)?))
                });
                if let Some((dc, k)) = &disk
                    && let Some(img) = dc.get_still(*k)
                {
                    #[cfg(test)]
                    STILLS_FROM_DISK.with(|c| c.set(c.get() + 1));
                    return Ok(float(img));
                }
                let t0 = std::time::Instant::now();
                let px = self.decode_still(path, footage, op, detached)?;
                if let (Some((dc, k)), Pixels::Float(img)) = (&disk, &px)
                    && t0.elapsed() >= STILL_TO_DISK
                {
                    dc.put_still(*k, img.clone());
                }
                Ok(px)
            }
            Some(mt) if gif_anim::is_animated(footage) => {
                // A thumbnail decodes the GIF without keeping it.
                let anim = if detached { Arc::new(Anim::load(&self.read(path)?)?) } else { self.gif(path)? };
                let frame = anim.frame_at(Tick(mt.0)).ok_or_else(|| MediaError::Decode(format!("{path}: no frames")))?;
                Ok(match pack_dynamic(frame.clone(), op) {
                    Ok(img) => float(img),
                    Err(packed) => Pixels::Packed(Arc::new(packed)),
                })
            }
            Some(mt) => {
                let (frame, turn) = self.video_frame(path, loc.key.hw, mt, detached)?;
                Ok(video_pixels(frame, op, turn))
            }
        }
    }

    /// Decode the image file `path` (a still or one image of a sequence).
    fn decode_still(&self, path: &Arc<str>, footage: &Footage, op: AlphaOp, detached: bool) -> Result<Pixels> {
        let float = |img: Image| Pixels::Float(Arc::new(img));
        // Photoshop documents are parsed once per file, not once per layer item.
        if let Some(psd) = lock(&self.psds).get(path) {
            return Ok(float(crate::layered::decode_psd(path, &psd, footage, op)?));
        }
        let bytes = self.read(path)?;
        if effectcraft_psd::is_psd(&bytes) {
            let psd = Arc::new(crate::layered::parse_psd(path, &bytes)?);
            if !detached {
                lock(&self.psds).insert(path.clone(), psd.clone(), bytes.len());
            }
            return Ok(float(crate::layered::decode_psd(path, &psd, footage, op)?));
        }
        // DPX / Cineon film scans (our own reader; `image` has none).
        if let Some(img) = crate::dpx::decode(path, &bytes)? {
            return Ok(match pack_dynamic(img, op) {
                Ok(img) => float(img),
                Err(packed) => Pixels::Packed(Arc::new(packed)),
            });
        }
        if let Some(img) = crate::layered::decode(path, &bytes, footage, op)? {
            return Ok(float(img));
        }
        if let Some(img) = crate::avif::decode(path, &bytes, op)? {
            return Ok(float(img));
        }
        // (an OpenEXR compression the decoder lacks came out transparent or as a missing
        // channel error)
        if path.to_ascii_lowercase().ends_with(".exr")
            && let Some(why) = crate::exr_channels::unsupported_compression(&bytes)
        {
            return Err(MediaError::Decode(format!("{path}: {why}")));
        }
        let loaded = crate::probe::load_oriented(&bytes, crate::still_format(path, &bytes));
        let img = match loaded {
            Ok(img) => img,
            // A multi-layer OpenEXR file without an unnamed RGB layer: its colour layer.
            Err(e) => crate::exr_channels::cached_layered_image(path, bytes.clone()).ok_or_else(|| MediaError::Decode(format!("{path}: {e}")))?,
        };
        drop(bytes);
        Ok(match pack_dynamic(img, op) {
            Ok(img) => float(img),
            Err(packed) => Pixels::Packed(Arc::new(packed)),
        })
    }

    /// The decoded movie frame of `path` at media time `mt`, from the source that may decode on
    /// a hardware decoder (`hw`, previews) or the one that decodes exactly. `detached`: a movie
    /// not open already is opened for this frame only.
    fn video_frame(&self, path: &Arc<str>, hw: bool, mt: Tick, detached: bool) -> Result<(Arc<filmcraft_frame::VideoFrame>, Orientation)> {
        // Every decoder FilmCraft creates for this read (the source's first, its spare decoders
        // for parallel intra frames, a private one for a nested request) is made on this thread
        // inside this scope: the hardware factory declines outside a preview read.
        let _hw = crate::hw::scope(hw);
        let (src, turn) = match detached {
            false => self.opened(path, hw),
            true => self.open_source(path, hw).or_else(|| {
                let mem = lock(&self.files).get(&**path).cloned();
                let turn = movie_turn(path, mem.as_deref());
                crate::open::open_source(path, mem).ok().map(|(s, _)| (s, turn))
            }),
        }
        .ok_or_else(|| MediaError::Io(format!("{path}: cannot open")))?;
        let frame = src.video_frame(FrameRequest::full(filmcraft_time::Tick(mt.0))).map_err(MediaError::from)?;
        Ok((frame, turn))
    }
}

impl FootageSource for MediaPool {
    fn set_conform_folder(&self, folder: Option<std::path::PathBuf>) {
        MediaPool::set_conform_folder(self, folder);
    }
    fn model(&self, _item: ItemId, footage: &Footage) -> Option<Arc<effectcraft_model::Model>> {
        if footage.missing || footage.kind != FootageKind::Model {
            return None;
        }
        self.inner.model(&footage.path)
    }
    fn set_cache_budget(&self, bytes: usize) {
        self.set_budget(bytes);
    }
    fn share_cache_budget(&self, budget: Option<&Arc<SharedBudget>>) {
        self.share(budget);
    }
    fn purge(&self) {
        self.clear_frames();
        lock(&self.inner.sources).close_idle();
    }
    fn retain_files(&self, keep: &dyn Fn(&str) -> bool) {
        MediaPool::retain_files(self, keep);
    }
    fn forget(&self, path: &str) {
        MediaPool::forget(self, path);
    }
    fn thumbnail(&self, footage: &Footage, max_side: u32) -> Option<Image> {
        if footage.missing {
            return None;
        }
        MediaPool::thumbnail(self, footage, max_side).map_err(|e| log::debug!("media: thumbnail of {}: {e}", footage.path)).ok()
    }
    fn cache_budget(&self) -> Option<usize> {
        Some(self.budget())
    }
    fn cache_usage(&self) -> Option<effectcraft_render::FootageCacheUsage> {
        Some(MediaPool::cache_usage(self))
    }
    fn set_hardware_decoding(&self, on: bool) {
        MediaPool::set_hardware_decoding(self, on);
    }
    fn set_still_cache(&self, disk: Option<Arc<effectcraft_render::disk_cache::DiskCache>>) {
        MediaPool::set_still_cache(self, disk)
    }
    fn exact(&self) -> Option<&dyn FootageSource> {
        self.twin.as_deref().map(|t| t as &dyn FootageSource)
    }
    /// A movie's decoder gives up on a seek (before it, and between samples while it decodes
    /// forward from the keyframe) once `cancel` is set: a scrub's superseded frames no longer
    /// hold the decoder the frame under the pointer waits for (#329).
    fn cancellable(&self, cancel: &Arc<AtomicBool>, f: &mut dyn FnMut()) {
        // (a panic in `f` leaves no stale flag on this thread for its later reads)
        let r = filmcraft_media::cancel::with_cancel(cancel, || std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut *f)));
        if let Err(p) = r {
            std::panic::resume_unwind(p);
        }
    }
    fn frame(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<Arc<Image>> {
        self.frame_with(item, footage, t, None, FloatCurve::Srgb)
    }
    fn frame_pixels(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<SharedFrame> {
        self.frame_pixels_with(item, footage, t, FloatCurve::Srgb)
    }
    fn preview_frame(&self, item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64) -> Option<FootageFrame> {
        self.preview_frame_with(item, footage, t, w, h, scale, FloatCurve::Srgb)
    }
    /// (`size` is not used: reduced-resolution frames come through [`Self::preview_frame_with`].)
    fn frame_with(&self, _item: ItemId, footage: &Footage, t: Tick, _size: Option<(u32, u32)>, float: FloatCurve) -> Option<Arc<Image>> {
        self.pixels(footage, t, None, float).map(|px| px.image())
    }
    fn frame_pixels_with(&self, _item: ItemId, footage: &Footage, t: Tick, float: FloatCurve) -> Option<SharedFrame> {
        self.pixels(footage, t, None, float).map(|px| px.shared())
    }
    fn preview_frame_with(&self, _item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64, float: FloatCurve) -> Option<FootageFrame> {
        let px = self.pixels(footage, t, Some(Preview { w, h, s: scale }), float)?;
        Some(FootageFrame { preview: matches!(px, Pixels::Preview(_)), px: px.shared() })
    }

    fn problem(&self, footage: &Footage) -> Option<String> {
        if footage.missing || footage.kind != FootageKind::Video {
            return None;
        }
        lock(&self.inner.problems).get(footage.path.as_str()).filter(|(_, decoder)| *decoder).map(|(why, _)| why.clone())
    }

    fn vector_frame(&self, _item: ItemId, footage: &Footage, scale: f64) -> Option<Arc<Image>> {
        if footage.missing || !effectcraft_render::is_vector_footage(footage) {
            return None;
        }
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        // Cached per scale (the renderer asks in quarter-octave steps), with the decoded frames:
        // the same budget, and `forget` drops them when the file changes.
        let key = Key { raster: scale.to_bits(), ..Inner::locate(footage, Tick::ZERO, FloatCurve::Srgb).key };
        if let Some(px) = lock(&self.inner.cache).lru.get(&key) {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            return Some(px.image());
        }
        self.inner.misses.fetch_add(1, Ordering::Relaxed);
        let bytes = self.inner.read(&footage.path).ok()?;
        let img = Arc::new(crate::layered::rasterize_vector(&footage.path, &bytes, footage.layer.as_ref(), footage.page, scale)?);
        lock(&self.inner.cache).lru.insert(key, Pixels::Float(img.clone()), self.inner.budget());
        Some(img)
    }

    fn aux(&self, _item: ItemId, footage: &Footage, t: Tick) -> Option<Arc<effectcraft_raster::AuxChannels>> {
        if footage.missing || !footage.has_video {
            return None;
        }
        let loc = Inner::locate(footage, t, FloatCurve::Srgb);
        if loc.media_t.is_some() || !loc.key.path.to_ascii_lowercase().ends_with(".exr") {
            return None;
        }
        let path = loc.key.path.clone();
        crate::exr_channels::cached(&path, || self.inner.read(&path).ok())
    }

    fn audio(&self, _item: ItemId, footage: &Footage, t: Tick, frames: usize, rate: u32) -> Option<Vec<f32>> {
        if !footage.has_audio || footage.missing {
            return None;
        }
        Some(self.audio_samples(footage, t, frames, rate))
    }
}

/// Marks a decode-ahead runner stopped when its thread ends (also when a decode panics), and
/// wakes the readers waiting for its frames.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
struct RunnerStop {
    inner: std::sync::Weak<Inner>,
    path: Arc<str>,
    id: u64,
}

impl Drop for RunnerStop {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else { return };
        if let Some(a) = lock(&inner.cache).ahead.get_mut(&self.path).filter(|a| a.runner == Some(self.id)) {
            a.runner = None;
        }
        inner.done.notify_all();
    }
}

impl MediaPool {
    /// The pixels of `footage` at `t` for the renderer (playback read-ahead on), logged and
    /// `None` when they can't be decoded.
    fn pixels(&self, footage: &Footage, t: Tick, preview: Option<Preview>, float: FloatCurve) -> Option<Pixels> {
        if footage.missing {
            return None;
        }
        match Inner::pixels_at(&self.inner, footage, t, true, preview, self.hw_for(footage), float) {
            Ok(px) => Some(px),
            // nobody wants the frame any more (`cancellable`): not an error
            Err(MediaError::Cancelled) => None,
            // a movie's failure is logged once per file (#886), a still's each time it is read
            Err(e) if footage.kind == FootageKind::Video => {
                self.inner.note_problem(&Arc::from(footage.path.as_str()), &footage.codec, &e);
                None
            }
            Err(e) => {
                log::warn!("media: frame of {} at {:?}: {e}", footage.path, t);
                None
            }
        }
    }
}

/// A decoded movie frame as cached pixels: the decoder's samples, or float pixels turned the way
/// the file says it is shown (a phone held upright).
fn video_pixels(frame: Arc<filmcraft_frame::VideoFrame>, op: AlphaOp, turn: Orientation) -> Pixels {
    match turn {
        Orientation::NoTransforms => Pixels::Packed(Arc::new(Packed::Video { frame, op })),
        _ => Pixels::Float(Arc::new(turned(frame_to_image(&frame, op), turn))),
    }
}

/// The turn the movie at `path` is shown with, from its header (in memory, or read from disk).
fn movie_turn(path: &str, mem: Option<&[u8]>) -> Orientation {
    match mem {
        Some(bytes) => crate::probe::video_turn(bytes),
        None => crate::stream::movie_header(std::path::Path::new(path)).map_or(Orientation::NoTransforms, |h| crate::probe::video_turn(&h)),
    }
}

/// `frames` stereo sample frames (interleaved) of `src` from sample `s0` at `rate` Hz, which
/// FilmCraft serves as decoded when it is the source's own rate. Mono is duplicated to both
/// channels, multichannel sources are downmixed ([`crate::downmix`]); silence before the start. An error where decoding fails.
fn read_stereo(src: &SharedSource, s0: i64, frames: usize, rate: u32) -> std::result::Result<Vec<f32>, String> {
    let mut out = vec![0.0; frames * 2];
    let skip = usize::try_from(s0.saturating_neg()).unwrap_or(0);
    if skip >= frames {
        return Ok(out);
    }
    let buf = src.audio(s0.max(0), frames - skip, rate).map_err(|e| e.to_string())?;
    let Some(dst) = skip.checked_mul(2).and_then(|i| out.get_mut(i..)) else { return Ok(out) };
    let aac = src.info().audio.as_ref().is_some_and(|a| a.codec.starts_with("AAC"));
    crate::downmix::downmix_into(&buf.channels, aac, dst);
    Ok(out)
}

/// The one-line reason a movie shows no picture: its `codec` (the probe's, e.g. "H.264 (High
/// 4:2:2)") and what the decoder said, without the error wrappers it passed through.
fn describe_problem(codec: &str, e: &MediaError) -> String {
    let s = match e {
        MediaError::Io(s) | MediaError::Unsupported(s) | MediaError::Decode(s) => s.as_str(),
        MediaError::Cancelled => "cancelled",
    };
    let mut detail = s;
    let mut unsupported = matches!(e, MediaError::Unsupported(_));
    while let Some(rest) = ["decode error: ", "unsupported media: ", "unsupported: ", "I/O error: "].iter().find_map(|p| detail.strip_prefix(p)) {
        unsupported |= detail.starts_with("unsupported");
        detail = rest;
    }
    let what = if codec.is_empty() { "This video" } else { codec };
    if unsupported { format!("{what} isn't supported by the built-in decoder: {detail}") } else { format!("{what} can't be decoded: {detail}") }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A still that is slow to decode (vector artwork) is kept in the disk cache: a new pool
    /// (the app reopened) reads it back instead of decoding it, with the same pixels, and an
    /// edited file (another size or modification time) decodes again.
    #[test]
    fn slow_stills_come_back_from_the_disk_cache_after_a_restart() {
        use effectcraft_render::disk_cache::DiskCache;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-still-disk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svg = |fill: &str| {
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="48"><defs><linearGradient id="g"><stop offset="0" stop-color="{fill}"/><stop offset="1" stop-color="#2040ff" stop-opacity="0.5"/></linearGradient></defs><rect x="3" y="4" width="57" height="39" rx="9" fill="url(#g)"/></svg>"##
            )
        };
        let path = dir.join("art.svg");
        std::fs::write(&path, svg("#ff8000")).unwrap();
        let dc = DiskCache::open(dir.join("cache"), 1 << 30).unwrap();
        let read = |dc: &Arc<DiskCache>| {
            let pool = MediaPool::new();
            pool.set_still_cache(Some(dc.clone()));
            let footage = crate::probe::probe(&path).unwrap();
            let before = STILLS_FROM_DISK.with(std::cell::Cell::get);
            let img = pool.inner.frame_at(&footage, Tick::ZERO, false).unwrap();
            (img, STILLS_FROM_DISK.with(std::cell::Cell::get) - before)
        };
        let (first, from_disk) = read(&dc);
        assert_eq!(from_disk, 0, "decoded the first time");
        // Written in the background: a new pool finds it once it is on disk.
        let mut again = None;
        for _ in 0..500 {
            dc.flush();
            let (img, from_disk) = read(&dc);
            if from_disk == 1 {
                again = Some(img);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let again = again.expect("read back from the disk cache");
        assert_eq!((again.width, again.height), (first.width, first.height));
        assert!(again.data == first.data, "the same pixels as the decode");
        // The file changes: decoded again, and the new pixels show.
        std::fs::write(&path, svg("#00ff40ff")).unwrap();
        let (edited, from_disk) = read(&dc);
        assert_eq!(from_disk, 0, "an edited file decodes again");
        assert!(edited.data != first.data, "the edit shows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Conforming a long file's audio reads every chunk at its own time: the sample index times
    /// ticks per second overflowed `i64` past 12:36 at 48 kHz, and later chunks came out silent
    /// (#172).
    #[test]
    fn conform_chunks_keep_their_times_on_long_files() {
        for rate in [24_000u32, 44_100, 48_000, 96_000] {
            let mut last = Tick(-1);
            // Every chunk start of a 2-hour file.
            for at in (0..rate as usize * 7200).step_by(1 << 16) {
                let t = sample_time(at, rate);
                assert!(t > last, "{rate} Hz, frame {at}: {t:?} after {last:?}");
                assert!((t.seconds() - at as f64 / rate as f64).abs() < 1e-6, "{rate} Hz, frame {at}");
                last = t;
            }
        }
        assert!((sample_time(36_372_480, 48_000).seconds() - 757.76).abs() < 1e-9);
    }

    /// #408: footage whose header claims a month of audio isn't conformed: the conform thread
    /// allocated the whole conformed track up front (a terabyte here), and the failed allocation
    /// aborted the app. Its audio is read from the decoder instead (silence: this file doesn't
    /// decode).
    #[test]
    fn a_header_claiming_a_month_of_audio_is_not_conformed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-conform-month");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("month.flac");
        std::fs::write(&path, b"fLaC, not really").unwrap();
        let footage = Footage {
            path: path.to_string_lossy().into(),
            kind: FootageKind::Audio,
            duration: Tick::from_seconds_f64(30.0 * 86_400.0),
            has_audio: true,
            ..Default::default()
        };
        assert_eq!(conform_frames(&footage, 48_000), None);
        assert_eq!(conform_frames(&Footage { duration: Tick::from_seconds_f64(60.0), ..footage.clone() }, 48_000), Some(2_880_000));
        let pool = MediaPool::new();
        let conformed = dir.join("conformed");
        pool.set_conform_folder(Some(conformed.clone()));
        let s = pool.audio_samples(&footage, Tick::from_seconds_f64(3600.0), 1024, 48_000);
        assert_eq!(s.len(), 2048);
        assert!(s.iter().all(|v| *v == 0.0));
        assert!(lock(&pool.inner.conforming).is_empty(), "no conform thread started");
        assert!(!conformed.exists(), "nothing was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Open files are bounded (least recently used closed first), purging closes the idle ones,
    /// and files the project no longer names are closed.
    #[test]
    fn open_files_are_bounded_and_released() {
        let pool = MediaPool::new();
        let wav: Arc<[u8]> = filmcraft_media::wav::write_wav16(&[0.25; 9600], 2, 48_000).into();
        let footage = |i: usize| {
            let path = format!("/mem/tone{i}.wav");
            pool.add_bytes(&path, wav.clone());
            Footage { path, kind: FootageKind::Audio, duration: Tick::from_seconds_f64(0.1), has_audio: true, ..Default::default() }
        };
        let files: Vec<Footage> = (0..MAX_SOURCES + 6).map(footage).collect();
        for f in &files {
            assert!(pool.audio_samples(f, Tick::ZERO, 16, 48_000).iter().all(|v| (*v - 0.25).abs() < 1e-3));
        }
        assert_eq!(pool.open_files(), MAX_SOURCES);
        let key = |i: usize| (Arc::<str>::from(files[i].path.as_str()), false);
        assert!(lock(&pool.inner.sources).map.contains_key(&key(MAX_SOURCES + 5)), "the newest stays open");
        assert!(!lock(&pool.inner.sources).map.contains_key(&key(0)), "the oldest was closed");
        // Reopened on demand.
        assert!(pool.audio_samples(&files[0], Tick::ZERO, 16, 48_000).iter().all(|v| (*v - 0.25).abs() < 1e-3));
        pool.retain_files(&|p| p == files[1].path || p == files[2].path || p == files[0].path);
        assert!(pool.open_files() <= 3);
        // A source in use isn't closed by a purge.
        let busy = pool.inner.source(&Arc::from(files[0].path.as_str()), false);
        FootageSource::purge(&pool);
        assert_eq!(pool.open_files(), 1);
        drop(busy);
        FootageSource::purge(&pool);
        assert_eq!(pool.open_files(), 0);
    }

    /// A rayon worker waiting for a still another thread is decoding stops waiting after
    /// [`STALL`] without progress on that file: other files' decodes finishing (a movie's
    /// decode-ahead) kept it waiting for as long as they went on.
    #[test]
    fn a_waiter_for_a_stuck_decode_ignores_other_files_progress() {
        let pool = MediaPool::new();
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(8, 4, image::Rgba([10, 20, 30, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        pool.add_bytes("/matte.png", png.into());
        let f = Footage { path: "/matte.png".into(), kind: FootageKind::Still, width: 8, height: 4, has_video: true, ..Default::default() };
        // Another thread holds the still's decode and never finishes it.
        let stuck = std::thread::spawn(|| std::thread::current().id()).join().unwrap();
        let key = Inner::locate_for(&f, Tick::ZERO, None, false, FloatCurve::Srgb).key;
        lock(&pool.inner.cache).inflight.insert(key.clone(), stuck);
        // Meanwhile another file's frames keep finishing.
        let stop = Arc::new(AtomicBool::new(false));
        let other = {
            let (inner, stop) = (pool.inner.clone(), stop.clone());
            std::thread::spawn(move || {
                let movie: Arc<str> = "/movie.mov".into();
                while !stop.load(Ordering::Relaxed) {
                    let k = Key { path: movie.clone(), ..key.clone() };
                    lock(&inner.cache).inflight.insert(k.clone(), std::thread::current().id());
                    drop(Inflight { inner: &inner, key: k });
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
        };
        let rayon = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let t0 = std::time::Instant::now();
        let img = rayon.install(|| pool.frame_at(&f, Tick::ZERO)).unwrap();
        let took = t0.elapsed();
        stop.store(true, Ordering::Relaxed);
        other.join().unwrap();
        assert_eq!((img.width, img.height), (8, 4));
        assert!(took >= STALL && took < STALL * 3, "decoded it itself after {took:?}");
        // The next layer showing the still finds that copy instead of waiting again.
        let t0 = std::time::Instant::now();
        rayon.install(|| pool.frame_at(&f, Tick::ZERO)).unwrap();
        assert!(t0.elapsed() < STALL / 2, "waited again: {:?}", t0.elapsed());
    }

    /// A Photoshop document imported as a composition: every layer item decodes from one parsed
    /// document (each re-read and re-parsed the whole file), with the same pixels; parsed
    /// documents are bounded and forgotten with their file.
    #[test]
    fn psd_layers_share_one_parsed_document() {
        use effectcraft_psd::Rect;
        use effectcraft_psd::write::{WDoc, WLayer};
        let pool = MediaPool::new();
        let mut doc = WDoc::new(32, 16);
        doc.layers =
            vec![WLayer::solid("Back", Rect::new(0, 0, 32, 16), [0.2, 0.3, 0.4, 1.0]), WLayer::solid("Red", Rect::new(4, 2, 10, 8), [1.0, 0.0, 0.0, 1.0])];
        let bytes: Arc<[u8]> = effectcraft_psd::write::write(&doc).into();
        let psd_footage = |path: &str, index: Option<u32>| Footage {
            path: path.into(),
            kind: FootageKind::Still,
            width: 32,
            height: 16,
            has_video: true,
            alpha: AlphaMode::Straight,
            layer: index.map(|index| effectcraft_project::SourceLayer { index, name: String::new(), ..Default::default() }),
            ..Default::default()
        };
        pool.add_bytes("/doc.psd", bytes.clone());
        for index in [None, Some(0), Some(1)] {
            let f = psd_footage("/doc.psd", index);
            let got = pool.frame_at(&f, Tick::ZERO).unwrap();
            let fresh =
                crate::layered::decode_psd("/doc.psd", &crate::layered::parse_psd("/doc.psd", &bytes).unwrap(), &f, AlphaOp::of(&f, FloatCurve::Srgb)).unwrap();
            assert_eq!(*got, fresh, "layer {index:?}");
        }
        assert_eq!(lock(&pool.inner.psds).list.len(), 1, "parsed once");
        pool.forget("/doc.psd");
        assert!(lock(&pool.inner.psds).list.is_empty());
        for i in 0..MAX_PSDS + 2 {
            let path = format!("/doc{i}.psd");
            pool.add_bytes(&path, bytes.clone());
            pool.frame_at(&psd_footage(&path, Some(1)), Tick::ZERO).unwrap();
        }
        let psds = lock(&pool.inner.psds);
        assert_eq!(psds.list.len(), MAX_PSDS);
        assert_eq!(&*psds.list[MAX_PSDS - 1].0, format!("/doc{}.psd", MAX_PSDS + 1));
    }

    /// #324: audio that fails to decode isn't conformed. A conformed file of silence was written
    /// instead, and as it is found by the file's path, size and date the footage stayed silent
    /// in every later session (a renamed copy played).
    #[test]
    fn audio_that_fails_to_decode_is_not_conformed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-conform-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.mp3");
        std::fs::write(&path, b"ID3, not really").unwrap();
        let footage = Footage {
            path: path.to_string_lossy().into(),
            kind: FootageKind::Audio,
            duration: Tick::from_seconds_f64(2.0),
            has_audio: true,
            ..Default::default()
        };
        let pool = MediaPool::new();
        let conformed = dir.join("conformed");
        pool.set_conform_folder(Some(conformed.clone()));
        let file = pool.conformed_path(&footage.path, 48_000).unwrap();
        assert!(pool.audio_samples(&footage, Tick::ZERO, 1024, 48_000).iter().all(|v| *v == 0.0));
        for _ in 0..500 {
            if lock(&pool.inner.conforming).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(lock(&pool.inner.conforming).is_empty(), "the conform thread finished");
        assert!(!file.exists() && !file.with_extension("tmp").exists(), "no conformed file was left");
        assert!(lock(&pool.inner.conform_failed).contains(&file));
        // Later reads go to the decoder without trying again.
        pool.audio_samples(&footage, Tick::ZERO, 1024, 48_000);
        assert!(lock(&pool.inner.conforming).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An intra-only (ProRes) movie of `n` frames, each a different gradient.
    fn prores_movie(w: u32, h: u32, n: u32) -> Vec<u8> {
        use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
        use filmcraft_prores::{ChromaFormat, Encoder, Frame, Profile};
        let mut enc = Encoder::new(Profile::Standard, w, h);
        let mut mux = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).unwrap();
        let vt = mux.add_track(TrackConfig::new(SampleEntry::prores(FourCc(*b"apcn"), w as u16, h as u16), 24)).unwrap();
        for i in 0..n {
            let mut fr = Frame::new(w, h, ChromaFormat::Yuv422, 10, false);
            for (k, y) in fr.y.iter_mut().enumerate() {
                *y = 64 + ((k as u32 % w) * 7 + (k as u32 / w) * 3 + i * 40) as u16 % 876;
            }
            for (k, c) in fr.cb.iter_mut().enumerate() {
                *c = 64 + (k as u32 * 5 + i * 60) as u16 % 896;
            }
            let data = enc.encode(&fr).unwrap();
            mux.write_sample(vt, WriteSample { data: &data, duration: 1, composition_offset: 0, is_sync: true }).unwrap();
        }
        mux.finish().unwrap().into_inner()
    }

    /// Decode-ahead of an intra-only movie decodes several frames at once and caches them in
    /// frame order, with the same pixels as decoding each frame on its own.
    #[test]
    fn intra_decode_ahead_matches_serial_decode() {
        let n = 24;
        let bytes: Arc<[u8]> = prores_movie(64, 48, n).into();
        let path = "/mem/intra.mov";
        let footage = crate::probe::probe_bytes(path, bytes.clone()).unwrap();
        assert!(footage.codec.contains("ProRes"), "{}", footage.codec);
        assert_eq!(ahead_batch(&footage.codec), (std::thread::available_parallelism().map_or(1, |n| n.get()) / 2).clamp(1, 4));
        assert_eq!(ahead_batch("H.264 High"), 1);
        let at = |i: u32| footage.frame_rate.tick_of(i64::from(i)) + Tick(footage.frame_rate.frame_duration().0 / 2);

        // serial: every frame decoded on its own, no decode-ahead
        let serial = MediaPool::new();
        serial.set_read_ahead(false);
        serial.add_bytes(path, bytes.clone());
        let want: Vec<Arc<Image>> = (0..n).map(|i| serial.inner.frame_at(&footage, at(i), true).unwrap()).collect();
        assert!(want.windows(2).all(|p| p[0].data != p[1].data), "the frames differ");

        // two reads in order start the decode-ahead of the frames after them
        let pool = MediaPool::new();
        pool.add_bytes(path, bytes);
        for i in 0..2 {
            assert_eq!(pool.inner.frame_at(&footage, at(i), true).unwrap().data, want[i as usize].data);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let c = lock(&pool.inner.cache);
            if c.ahead.get(path).is_some_and(|a| a.runner.is_none()) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "decode-ahead finishes");
            drop(pool.inner.done.wait_timeout(c, std::time::Duration::from_millis(20)));
        }
        let ahead = pool.stats().prefetched;
        assert!(ahead >= 16, "decoded ahead: {ahead}");
        // cached in frame order (each newer than the one before), and identical to serial decode
        let c = lock(&pool.inner.cache);
        let stamps: Vec<u64> = (2..2 + ahead as u32)
            .map(|i| c.lru.map.get(&Key { frame: i64::from(i), ..Inner::locate(&footage, at(i), FloatCurve::Srgb).key }).unwrap().stamp)
            .collect();
        assert!(stamps.windows(2).all(|p| p[0] < p[1]), "in frame order: {stamps:?}");
        drop(c);
        for i in 2..n {
            assert_eq!(pool.inner.frame_at(&footage, at(i), true).unwrap().data, want[i as usize].data, "frame {i}");
        }
    }

    /// A6: after a backward jump (or a loop wrap) the decode-ahead window starts over from the
    /// new position: it kept its old far end, so the runner decoded everything in between (the
    /// whole clip on a wrap), past [`MAX_AHEAD`] and the memory budget.
    #[test]
    fn decode_ahead_window_shrinks_after_a_backward_jump() {
        let n = 160;
        let bytes: Arc<[u8]> = prores_movie(16, 16, n).into();
        let path = "/mem/jump.mov";
        let footage = crate::probe::probe_bytes(path, bytes.clone()).unwrap();
        let at = |i: u32| footage.frame_rate.tick_of(i64::from(i)) + Tick(footage.frame_rate.frame_duration().0 / 2);
        let pool = MediaPool::new();
        pool.add_bytes(path, bytes);
        let until = |pool: &MediaPool| lock(&pool.inner.cache).ahead.get(path).map(|a| a.until);
        for i in [120, 121] {
            pool.inner.frame_at(&footage, at(i), true).unwrap();
        }
        assert!(until(&pool).is_some_and(|u| u > 121), "decoding ahead of frame 121: {:?}", until(&pool));
        // a loop wrap / backward jump to the start, then reading on in order
        for i in [2, 3] {
            pool.inner.frame_at(&footage, at(i), true).unwrap();
        }
        let u = until(&pool).unwrap();
        assert!(u <= 4 + MAX_AHEAD, "the window ends at {u}, not near frame 3");
    }

    #[test]
    fn frame_index_loops_and_clamps() {
        let r = FrameRate::FPS_30;
        assert_eq!(Inner::frame_index(r, r.tick_of(-3), 10, 1), 0);
        assert_eq!(Inner::frame_index(r, r.tick_of(4), 10, 1), 4);
        assert_eq!(Inner::frame_index(r, r.tick_of(25), 10, 1), 9);
        assert_eq!(Inner::frame_index(r, r.tick_of(25), 10, 3), 5);
        assert_eq!(Inner::frame_index(r, r.tick_of(45), 10, 3), 9);
        assert_eq!(Inner::frame_count(r, Tick(2 * TICKS_PER_SECOND)), 60);
    }

    #[test]
    fn saved_vector_crop_has_distinct_media_cache_key() {
        let mut footage = Footage {
            path: "art.ai".into(),
            kind: FootageKind::Still,
            layer: Some(effectcraft_project::SourceLayer { index: 1, name: "Art".into(), crop_bounds: Some([0.0, 0.0, 20.0, 40.0]), ..Default::default() }),
            ..Default::default()
        };
        let before = Inner::locate(&footage, Tick::ZERO, FloatCurve::Srgb).key;
        footage.layer.as_mut().unwrap().crop_bounds = Some([1.0, 0.0, 21.0, 40.0]);
        assert!(before != Inner::locate(&footage, Tick::ZERO, FloatCurve::Srgb).key);
    }

    #[test]
    fn lru_respects_budget() {
        let mut c = Lru::default();
        let img = || Pixels::Float(Arc::new(Image::new(16, 16))); // 4 KiB + overhead
        let k = |i| Key { path: "a".into(), frame: i, interp: 0, matte: [0; 3], crop: None, preview: None, raster: 0, hw: false };
        let budget = 3 * (4096 + 64);
        for i in 0..10 {
            c.insert(k(i), img(), budget);
        }
        assert_eq!(c.map.len(), 3);
        assert!(c.get(&k(9)).is_some() && c.get(&k(6)).is_none());
        // touching 7 makes 8 the oldest
        c.get(&k(7));
        c.insert(k(10), img(), budget);
        assert!(c.get(&k(7)).is_some() && c.get(&k(8)).is_none());
    }

    /// #482: an OpenEXR file with HTJ2K compression (which the decoder lacks) says so on import
    /// and when its frame is read, instead of "no non-deep rgb channels" or a transparent frame.
    #[test]
    fn htj2k_exr_is_reported_as_unsupported() {
        use exr::prelude::*;
        let channels =
            AnyChannels::sort(vec![AnyChannel::new("R", FlatSamples::F32(vec![0.5; 4])), AnyChannel::new("G", FlatSamples::F32(vec![0.5; 4]))].into());
        let image = exr::image::Image::from_layer(Layer::new((2, 2), LayerAttributes::default(), Encoding::UNCOMPRESSED, channels));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write().to_buffered(&mut bytes).unwrap();
        let mut bytes = bytes.into_inner();
        assert_eq!(crate::exr_channels::unsupported_compression(&bytes), None);
        // The header's compression attribute (name, type, size 1, value) says HTJ2K32 (11).
        let tag = b"compression\0compression\0\x01\0\0\0";
        let at = bytes.windows(tag.len()).position(|w| w == tag).unwrap() + tag.len();
        bytes[at] = 11;
        let b: Arc<[u8]> = bytes.into();
        let e = crate::probe_bytes("/shot.exr", b.clone()).unwrap_err().to_string();
        assert!(e.contains("/shot.exr: OpenEXR HTJ2K compression isn't supported yet"), "{e}");
        let pool = MediaPool::new();
        pool.add_bytes("/shot.exr", b);
        let f = Footage { path: "/shot.exr".into(), kind: FootageKind::Still, width: 2, height: 2, has_video: true, ..Default::default() };
        let e = pool.frame_at(&f, Tick::ZERO).unwrap_err().to_string();
        assert!(e.contains("HTJ2K compression isn't supported yet"), "{e}");
    }

    /// #411: Interpret Footage ▸ Preserve RGB reads a float OpenEXR's values as the file stores
    /// them (no automatic linear → sRGB encode), so an OCIO effect is the only conversion; the
    /// default interpretation still encodes. Both interpretations of one file decode separately.
    #[test]
    fn preserve_rgb_reads_float_exr_values_unconverted() {
        use exr::prelude::*;
        let (w, h) = (2usize, 2usize);
        let ch = |n: &str, v: f32| AnyChannel::new(n, FlatSamples::F32(vec![v; w * h]));
        let channels = AnyChannels::sort(vec![ch("R", 0.18), ch("G", 0.5), ch("B", 2.0), ch("A", 1.0)].into());
        let image = exr::image::Image::from_layer(Layer::new((w, h), LayerAttributes::default(), Encoding::FAST_LOSSLESS, channels));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write().to_buffered(&mut bytes).unwrap();
        let b: Arc<[u8]> = bytes.into_inner().into();
        let pool = MediaPool::new();
        pool.add_bytes("/render.exr", b.clone());
        let f = crate::probe_bytes("/render.exr", b).unwrap();
        let encoded = pool.frame_at(&f, Tick::ZERO).unwrap().get(0, 0);
        assert!((encoded[0] - 0.4613).abs() < 1e-3 && (encoded[1] - 0.7354).abs() < 1e-3 && encoded[2] > 1.3, "sRGB-encoded: {encoded:?}");
        let raw = pool.frame_at(&Footage { preserve_rgb: true, ..f.clone() }, Tick::ZERO).unwrap().get(0, 0);
        assert_eq!(raw, [0.18, 0.5, 2.0, 1.0]);
        assert_eq!(pool.frame_at(&f, Tick::ZERO).unwrap().get(0, 0), encoded, "the default interpretation is cached apart");
    }

    /// #750: float footage is handed over as the renderer asks: sRGB-encoded, with Working
    /// Gamma 2.4's pure power curve, or as stored (Interpret As Linear Light encodes it then);
    /// Preserve RGB reads it as stored whatever is asked. Each decodes and caches apart.
    #[test]
    fn float_exr_is_encoded_with_the_requested_curve() {
        use exr::prelude::*;
        let (w, h) = (2usize, 2usize);
        let ch = |n: &str, v: f32| AnyChannel::new(n, FlatSamples::F32(vec![v; w * h]));
        let channels = AnyChannels::sort(vec![ch("R", 0.051), ch("G", 0.18), ch("B", 2.0), ch("A", 1.0)].into());
        let image = exr::image::Image::from_layer(Layer::new((w, h), LayerAttributes::default(), Encoding::FAST_LOSSLESS, channels));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write().to_buffered(&mut bytes).unwrap();
        let b: Arc<[u8]> = bytes.into_inner().into();
        let pool = MediaPool::new();
        pool.add_bytes("/linear.exr", b.clone());
        let f = crate::probe_bytes("/linear.exr", b).unwrap();
        let px = |f: &Footage, float| pool.frame_with(ItemId(1), f, Tick::ZERO, None, float).unwrap().get(0, 0);
        let near = |o: [f32; 4], want: [f32; 3]| (0..3).all(|i| (o[i] - want[i]).abs() < 1e-3);
        let srgb = px(&f, FloatCurve::Srgb);
        assert!(near(srgb, [0.2503, 0.4614, 1.3532]), "sRGB: {srgb:?}");
        assert_eq!(pool.frame(ItemId(1), &f, Tick::ZERO).unwrap().get(0, 0), srgb, "frame() is sRGB");
        let g24 = px(&f, FloatCurve::Gamma24);
        assert!(near(g24, [0.2898, 0.4898, 2.0f32.powf(1.0 / 2.4)]), "2.4: {g24:?}");
        assert_eq!(px(&f, FloatCurve::Linear), [0.051, 0.18, 2.0, 1.0]);
        let preserve = Footage { preserve_rgb: true, ..f.clone() };
        assert_eq!(px(&preserve, FloatCurve::Gamma24), [0.051, 0.18, 2.0, 1.0]);
        // Each curve is cached apart: asking again returns the same pixels.
        assert_eq!(px(&f, FloatCurve::Srgb), srgb);
        assert_eq!(px(&f, FloatCurve::Gamma24), g24);
    }
}
