//! The persistent disk cache (Settings ▸ Media & Disk Cache ▸ Disk Cache): RAM-preview frames
//! and layer cache entries kept on disk between sessions, like After Effects' disk cache.
//!
//! * Entries are files keyed by a 128-bit content hash (`<folder>/v1/<kind>/<2 hex>/<32 hex>.ecc`):
//!   frames by their fingerprint ([`print_content`]: what the frame shows at its time, so an
//!   edit changes the keys of only the frames it changes) or, for a frame without one, the hash
//!   of the composition's content (the comp and everything it uses, footage files' size and
//!   modification time included), plus frame, scale and view; layer buffers by
//!   the layer cache's content key salted with the project's footage fingerprint. A changed
//!   project therefore never reads stale pixels: its keys simply change.
//! * Payloads are LZ4-compressed (`lz4_flex`, MIT): frames as 8-bit premultiplied RGBA, layer
//!   buffers as `f32` RGBA with the bytes shuffled into planes (exact, so cached and fresh
//!   renders are bit-identical), Roto Brush / Object Matte segmentations as
//!   `FrameSeg::to_bytes` (keyed by their content-keyed chain key and scale, so they are
//!   reused whenever the same frames, settings and prompt come back, also after a restart). Every file ends in a checksum; damaged files read as misses and
//!   are deleted.
//! * Size limit with least-recently-used eviction. The in-memory index is rebuilt from the
//!   folder when the cache opens (file modification times order it), so the cache survives
//!   restarts; reads refresh a file's time.
//! * Never fills the disk: the cache keeps a reserve free on its volume (the larger of 5 GB and
//!   10% of the volume, at most 20 GB, [`reserve_bytes`]). Its effective limit is the smaller of the size
//!   setting and what the volume can spare ([`effective_cap`], recomputed on every write and
//!   eviction); below it, the oldest entries are evicted and writes that cannot fit are dropped,
//!   so project saves and auto-saves keep their room even with a size setting copied from a
//!   bigger machine.
//! * Safe concurrent access: writes go to a unique temporary file that is renamed into place
//!   (atomic on every desktop OS), so readers in this or another process never see a partial
//!   entry; a missing or damaged file is just a miss. Writes run on a background thread with a
//!   bounded queue (a full queue drops the write rather than stalling rendering). Frames are
//!   queued uncompressed (shared with the caller, no copy) and compressed on that thread, so a
//!   frame worker spends no time on them. Frames are stored in strips ([`crate::frame_pack`]):
//!   the RAM preview keeps the same bytes when it compresses its frames and writes them as they
//!   are ([`DiskCache::put_packed_frame`]); frames in the older single-block layout still read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use effectcraft_effects::Buf;
use effectcraft_raster::Image;

/// File format version (part of the folder layout).
const VERSION: &str = "v1";
const MAGIC: &[u8; 4] = b"ECDC";
/// Pending writes before new ones are dropped.
#[cfg(not(target_arch = "wasm32"))]
const QUEUE_LIMIT: usize = 64;
/// Bytes of pending writes before new ones are dropped (compressed layer payloads, uncompressed
/// frames): a 4K `f32` layer can compress to tens of MB, so 64 of them could hold gigabytes. A
/// single larger write still goes when nothing else is pending.
#[cfg(not(target_arch = "wasm32"))]
const QUEUE_BYTES: usize = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Frame,
    Layer,
    /// A Roto Brush / Object Matte segmentation.
    Matte,
}

impl Kind {
    /// The kind's folder name.
    pub fn dir(self) -> &'static str {
        match self {
            Kind::Frame => "frames",
            Kind::Layer => "layers",
            Kind::Matte => "mattes",
        }
    }
    fn tag(self) -> u8 {
        match self {
            Kind::Frame => 1,
            Kind::Layer => 2,
            Kind::Matte => 3,
        }
    }
}

/// A cached viewer frame: 8-bit premultiplied RGBA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame8 {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The smallest reserve of free space the cache leaves on its volume.
pub const RESERVE_MIN_BYTES: u64 = 5 << 30;
/// The largest: 10% of a multi-terabyte disk would leave a nearly full one no cache at all.
pub const RESERVE_MAX_BYTES: u64 = 20 << 30;

/// Bytes the cache keeps free on a volume of `total` bytes: 10% of the volume, between
/// [`RESERVE_MIN_BYTES`] and [`RESERVE_MAX_BYTES`].
pub fn reserve_bytes(total: u64) -> u64 {
    (total / 10).clamp(RESERVE_MIN_BYTES, RESERVE_MAX_BYTES)
}

/// The cache's limit given the size setting, the bytes it holds (`used`, which eviction frees)
/// and its volume's free and total bytes: it may grow until only the reserve is left free.
/// Saturating: a volume already below its reserve gives 0 (evict everything), never an
/// underflow.
pub fn effective_cap(max_bytes: u64, used: u64, free: u64, total: u64) -> u64 {
    max_bytes.min(used.saturating_add(free).saturating_sub(reserve_bytes(total)))
}

/// Free and total bytes of the volume holding a folder.
pub trait VolumeSpace: Send + Sync {
    /// `(free, total)`, or `None` when unknown (then the cache only follows its size setting).
    fn space(&self, folder: &Path) -> Option<(u64, u64)>;
}

/// The operating system's answer (none in the browser).
struct SystemVolume;

impl VolumeSpace for SystemVolume {
    fn space(&self, folder: &Path) -> Option<(u64, u64)> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let st = fs4::statvfs(folder).ok()?;
            // A file system that reports no size (some network and virtual ones) is unknown.
            (st.total_space() > 0).then(|| (st.available_space(), st.total_space()))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = folder;
            None
        }
    }
}

/// Counters for tests, the Info panel and `cache.diskStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStats {
    pub entries: usize,
    pub bytes: u64,
    pub max_bytes: u64,
    /// The limit in force: `max_bytes` capped by what the volume can spare ([`effective_cap`]).
    pub effective_max_bytes: u64,
    /// Free bytes on the cache's volume (0 when unknown).
    pub free_bytes: u64,
    /// Bytes the cache keeps free on its volume (0 when unknown).
    pub reserve_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub evictions: u64,
    pub dropped: u64,
}

struct Meta {
    bytes: u64,
    last_use: u64,
}

/// The cache's index: entry sizes and least-recently-used order under a size limit. Portable
/// (no file system): [`DiskCache`] keeps one over its folder, and the browser keeps one over the
/// Origin Private File System (`apps/effectcraft-web`, where the files are written by workers).
#[derive(Default)]
pub struct DiskIndex {
    map: HashMap<(Kind, u128), Meta>,
    total: u64,
    clock: u64,
}

impl DiskIndex {
    /// Record an entry (`bytes` on disk) as the most recently used; replaces an older record.
    pub fn insert(&mut self, kind: Kind, key: u128, bytes: u64) {
        self.clock += 1;
        let c = self.clock;
        if let Some(old) = self.map.insert((kind, key), Meta { bytes, last_use: c }) {
            self.total -= old.bytes;
        }
        self.total += bytes;
    }

    /// Mark an entry as just used (a read). `false` when it is not indexed.
    pub fn touch(&mut self, kind: Kind, key: u128) -> bool {
        self.clock += 1;
        let c = self.clock;
        match self.map.get_mut(&(kind, key)) {
            Some(m) => {
                m.last_use = c;
                true
            }
            None => false,
        }
    }

    /// Forget an entry; its size, if it was indexed.
    pub fn remove(&mut self, kind: Kind, key: u128) -> Option<u64> {
        let m = self.map.remove(&(kind, key))?;
        self.total -= m.bytes;
        Some(m.bytes)
    }

    pub fn contains(&self, kind: Kind, key: u128) -> bool {
        self.map.contains_key(&(kind, key))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bytes of every indexed entry.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Keys of one kind (unordered).
    pub fn keys(&self, kind: Kind) -> Vec<u128> {
        self.map.keys().filter(|k| k.0 == kind).map(|k| k.1).collect()
    }

    /// Entries from least to most recently used.
    pub fn lru_order(&self) -> Vec<(Kind, u128)> {
        let mut v: Vec<(u64, (Kind, u128))> = self.map.iter().map(|(k, m)| (m.last_use, *k)).collect();
        v.sort_unstable_by_key(|e| e.0);
        v.into_iter().map(|e| e.1).collect()
    }

    /// Remove least recently used entries until the total fits `max` bytes; returns them (the
    /// caller deletes their files).
    pub fn evict(&mut self, max: u64) -> Vec<(Kind, u128)> {
        if self.total <= max {
            return vec![];
        }
        let mut out = vec![];
        for k in self.lru_order() {
            if self.total <= max {
                break;
            }
            self.remove(k.0, k.1);
            out.push(k);
        }
        out
    }

    pub fn clear(&mut self) {
        *self = DiskIndex::default();
    }
}

/// Which layer buffers a browser frame worker should fetch from the disk cache before its next
/// frame (its lookups mid-render are synchronous, the Origin Private File System is not): the
/// layer-cache misses the workers reported for recent frames, most recent first, that are on
/// disk ([`DiskIndex`], kind [`Kind::Layer`]).
#[derive(Default)]
pub struct LayerPrefetch {
    recent: std::collections::VecDeque<u128>,
}

/// Misses [`LayerPrefetch`] remembers.
const PREFETCH_RECENT: usize = 512;

impl LayerPrefetch {
    /// A frame's layer-cache misses (salted keys).
    pub fn note_misses(&mut self, keys: impl IntoIterator<Item = u128>) {
        for k in keys {
            self.recent.retain(|x| *x != k);
            self.recent.push_front(k);
        }
        self.recent.truncate(PREFETCH_RECENT);
    }

    /// Up to `max` keys to fetch before the next frame.
    pub fn plan(&self, index: &DiskIndex, max: usize) -> Vec<u128> {
        self.recent.iter().copied().filter(|k| index.contains(Kind::Layer, *k)).take(max).collect()
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }
}

/// Frame pixels handed to [`DiskCache::put_frame`]: owned or shared with the caller (an `Arc`
/// of the RAM cache's frame), so queueing a frame copies nothing.
pub type FrameBytes = Box<dyn AsRef<[u8]> + Send>;

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum Job {
    /// An encoded payload.
    Write(Kind, u128, Vec<u8>),
    GuardedLayer(u128, Vec<u8>, WriteGuard),
    /// A frame to encode on the writer thread (key, width, height, 8-bit premultiplied RGBA).
    Frame(u128, u32, u32, FrameBytes),
    /// A frame compressed already (the RAM preview's bytes, shared: written as they are).
    Packed(u128, Arc<crate::frame_pack::PackedFrame>),
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl Job {
    /// Bytes the job counts against [`QUEUE_BYTES`] while queued.
    fn bytes(&self) -> usize {
        match self {
            Job::Write(_, _, p) => p.len(),
            Job::GuardedLayer(_, p, _) => p.capacity(),
            Job::Frame(_, _, _, rgba) => (**rgba).as_ref().len(),
            Job::Packed(_, p) => p.payload().len(),
        }
    }
}

struct Queue {
    jobs: std::collections::VecDeque<Job>,
    /// Payload bytes in `jobs`.
    bytes: usize,
    busy: bool,
}

impl Queue {
    /// No room for another write of `bytes` (see [`QUEUE_LIMIT`], [`QUEUE_BYTES`]).
    #[cfg(not(target_arch = "wasm32"))]
    fn full(&self, bytes: usize) -> bool {
        self.jobs.len() >= QUEUE_LIMIT || (!self.jobs.is_empty() && self.bytes.saturating_add(bytes) > QUEUE_BYTES)
    }
}

pub struct DiskCache {
    publication: Mutex<(Arc<()>, bool)>,
    purge: Mutex<()>,
    #[cfg(test)]
    before_publish: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    dir: PathBuf,
    index: Mutex<DiskIndex>,
    max_bytes: AtomicU64,
    volume: Arc<dyn VolumeSpace>,
    hits: AtomicU64,
    misses: AtomicU64,
    writes: AtomicU64,
    evictions: AtomicU64,
    dropped: AtomicU64,
    tmp_counter: AtomicU64,
    queue: Arc<(Mutex<Queue>, Condvar)>,
    /// Layer buffers that took less than this to render are not written (ms).
    min_layer_ms: AtomicU64,
    /// See [`Self::generation`].
    generation: AtomicU64,
}

#[derive(Clone)]
struct WriteGuard {
    binding: crate::cache::StorageToken,
    purge: Weak<()>,
}
/// Captured native disk destination for a later CPU demand. It never owns CPU pixels.
pub struct LayerWriteIntent {
    disk: Arc<DiskCache>,
    key: u128,
    guard: WriteGuard,
    started: web_time::Instant,
    producer_ms: u128,
}
impl LayerWriteIntent {
    pub(crate) fn produced(mut self) -> Self {
        self.producer_ms = self.started.elapsed().as_millis();
        self
    }
    pub(crate) fn put(&self, buf: &Buf) {
        if self.producer_ms < u128::from(self.disk.min_layer_ms())
            || !self.guard.binding.valid()
            || !self.disk.valid_guard(&self.guard)
            || self.disk.contains(Kind::Layer, self.key)
            || self.disk.queue_full()
        {
            return;
        }
        // Encode from borrowed pixels. Only compressed bytes enter the bounded queue;
        // a denied CPU admission never creates a hidden strong Arc<Buf> here.
        let payload = encode_layer(buf);
        if !self.guard.binding.valid() || !self.disk.valid_guard(&self.guard) {
            return;
        }
        self.disk.enqueue(Job::GuardedLayer(self.key, payload, self.guard.clone()));
    }
}

/// FNV-1a 128-bit (content keys).
#[derive(Clone, Copy)]
pub struct Hash128(u128);

impl Default for Hash128 {
    fn default() -> Self {
        Hash128(0x6c62_272e_07bb_0142_62b8_2175_6295_c58d)
    }
}

impl Hash128 {
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u128;
            self.0 = self.0.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        }
    }
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    pub fn finish(&self) -> u128 {
        self.0
    }
}

fn checksum(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The platform's default disk cache folder.
pub fn default_folder() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos")
        && let Some(h) = &home
    {
        return h.join("Library/Caches/EffectCraft/Disk Cache");
    }
    if cfg!(windows)
        && let Some(l) = std::env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(l).join("EffectCraft").join("Disk Cache");
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("effectcraft/disk-cache");
    }
    home.map(|h| h.join(".cache/effectcraft/disk-cache")).unwrap_or_else(|| std::env::temp_dir().join("effectcraft-disk-cache"))
}

impl DiskCache {
    /// Open (or create) the cache in `folder`, holding at most `max_bytes`.
    pub fn open(folder: impl AsRef<Path>, max_bytes: u64) -> std::io::Result<Arc<DiskCache>> {
        Self::open_with_volume(folder, max_bytes, Arc::new(SystemVolume))
    }

    /// [`Self::open`] with another source of the volume's free space (tests).
    pub fn open_with_volume(folder: impl AsRef<Path>, max_bytes: u64, volume: Arc<dyn VolumeSpace>) -> std::io::Result<Arc<DiskCache>> {
        let dir = folder.as_ref().join(VERSION);
        std::fs::create_dir_all(&dir)?;
        let dc = Arc::new(DiskCache {
            dir,
            publication: Mutex::new((Arc::new(()), false)),
            purge: Mutex::new(()),
            #[cfg(test)]
            before_publish: Mutex::new(None),
            index: Mutex::new(DiskIndex::default()),
            max_bytes: AtomicU64::new(max_bytes),
            volume,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            tmp_counter: AtomicU64::new(0),
            queue: Arc::new((Mutex::new(Queue { jobs: Default::default(), bytes: 0, busy: false }), Condvar::new())),
            min_layer_ms: AtomicU64::new(30),
            generation: AtomicU64::new(0),
        });
        dc.rescan();
        dc.evict();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let weak = Arc::downgrade(&dc);
            let q = dc.queue.clone();
            std::thread::Builder::new().name("ec-disk-cache".into()).spawn(move || writer(weak, q))?;
        }
        Ok(dc)
    }

    pub(crate) fn layer_intent(self: &Arc<Self>, key: u128, binding: crate::cache::StorageToken) -> Option<LayerWriteIntent> {
        let publication = self.publication.lock().ok()?;
        if publication.1 {
            return None;
        }
        Some(LayerWriteIntent {
            disk: self.clone(),
            key,
            guard: WriteGuard { binding, purge: Arc::downgrade(&publication.0) },
            started: web_time::Instant::now(),
            producer_ms: 0,
        })
    }
    fn valid_guard(&self, guard: &WriteGuard) -> bool {
        self.publication.lock().is_ok_and(|p| !p.1 && Weak::ptr_eq(&guard.purge, &Arc::downgrade(&p.0)))
    }

    pub fn folder(&self) -> &Path {
        self.dir.parent().unwrap_or(&self.dir)
    }

    /// Rebuild the index from the folder (oldest files first).
    pub fn rescan(&self) {
        let mut found: Vec<(std::time::SystemTime, Kind, u128, u64)> = vec![];
        for kind in [Kind::Frame, Kind::Layer, Kind::Matte] {
            let Ok(shards) = std::fs::read_dir(self.dir.join(kind.dir())) else { continue };
            for shard in shards.flatten() {
                let Ok(files) = std::fs::read_dir(shard.path()) else { continue };
                for f in files.flatten() {
                    let p = f.path();
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.contains(".tmp") {
                        // A writer that died mid-write; old temporaries are removed.
                        if f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e.as_secs() > 600)) {
                            let _ = std::fs::remove_file(&p);
                        }
                        continue;
                    }
                    let Some(hex) = name.strip_suffix(".ecc") else { continue };
                    let Ok(key) = u128::from_str_radix(hex, 16) else { continue };
                    let Ok(md) = f.metadata() else { continue };
                    found.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), kind, key, md.len()));
                }
            }
        }
        found.sort_by_key(|e| e.0);
        let mut idx = DiskIndex::default();
        for (_, kind, key, bytes) in found {
            idx.insert(kind, key, bytes);
        }
        if let Ok(mut g) = self.index.lock() {
            *g = idx;
        }
        self.changed();
    }

    fn path(&self, kind: Kind, key: u128) -> PathBuf {
        let hex = format!("{key:032x}");
        self.dir.join(kind.dir()).join(&hex[..2]).join(format!("{hex}.ecc"))
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    /// Change the size limit (evicts the least recently used entries when shrinking).
    pub fn set_max_bytes(&self, b: u64) {
        self.max_bytes.store(b, Ordering::Relaxed);
        self.evict();
    }

    /// Layer buffers faster to render than this are not worth a disk write.
    pub fn set_min_layer_ms(&self, ms: u64) {
        self.min_layer_ms.store(ms, Ordering::Relaxed);
    }

    pub fn min_layer_ms(&self) -> u64 {
        self.min_layer_ms.load(Ordering::Relaxed)
    }

    pub fn contains(&self, kind: Kind, key: u128) -> bool {
        self.index.lock().is_ok_and(|g| g.contains(kind, key))
    }

    /// Changes whenever an entry is added or removed (so callers can cache what they derive
    /// from [`Self::contains`], like the Timeline's cache bar).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Keys of one kind (unordered).
    pub fn keys(&self, kind: Kind) -> Vec<u128> {
        self.index.lock().map(|g| g.keys(kind)).unwrap_or_default()
    }

    pub fn stats(&self) -> DiskStats {
        let (entries, bytes) = self.index.lock().map(|g| (g.len(), g.total())).unwrap_or_default();
        let (free_bytes, reserve) = self.volume.space(self.folder()).map(|(f, t)| (f, reserve_bytes(t))).unwrap_or_default();
        DiskStats {
            entries,
            bytes,
            max_bytes: self.max_bytes(),
            effective_max_bytes: self.limit(),
            free_bytes,
            reserve_bytes: reserve,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Delete every entry (Edit ▸ Purge ▸ All Disk Cache / Empty Disk Cache).
    pub fn clear(&self) {
        let Ok(_purge) = self.purge.lock() else { return };
        {
            let Ok(mut publication) = self.publication.lock() else { return };
            publication.0 = Arc::new(());
            publication.1 = true;
        }
        // Never hold the final-publication barrier while waiting for the writer.
        self.flush();
        let Ok(mut publication) = self.publication.lock() else { return };
        if let Ok(mut g) = self.index.lock() {
            g.clear();
        }
        self.changed();
        for kind in [Kind::Frame, Kind::Layer, Kind::Matte] {
            let _ = std::fs::remove_dir_all(self.dir.join(kind.dir()));
        }
        publication.1 = false;
    }

    /// Wait until queued writes are on disk.
    pub fn flush(&self) {
        let (m, cv) = &*self.queue;
        let Ok(mut q) = m.lock() else { return };
        #[cfg(target_arch = "wasm32")]
        {
            q.jobs.clear();
            q.bytes = 0;
        }
        while !q.jobs.is_empty() || q.busy {
            q = match cv.wait(q) {
                Ok(q) => q,
                Err(_) => return,
            };
        }
    }

    fn read(&self, kind: Kind, key: u128) -> Option<Vec<u8>> {
        if !self.contains(kind, key) {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let path = self.path(kind, key);
        let data = std::fs::read(&path).ok();
        let ok = data.as_deref().and_then(|d| verify(d, kind));
        match ok {
            Some(payload) => {
                let payload = payload.to_vec();
                self.hits.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut g) = self.index.lock() {
                    g.touch(kind, key);
                }
                // Persist the recency for the next session (best effort).
                if let Ok(f) = std::fs::File::options().append(true).open(&path) {
                    let _ = f.set_modified(std::time::SystemTime::now());
                }
                Some(payload)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                // Gone (another process evicted it) or damaged: forget it.
                if data.is_some() {
                    let _ = std::fs::remove_file(&path);
                }
                self.forget(kind, key);
                None
            }
        }
    }

    fn forget(&self, kind: Kind, key: u128) {
        if let Ok(mut g) = self.index.lock() {
            g.remove(kind, key);
        }
        self.changed();
    }

    /// Queue a write; `false` when it was dropped (the queue is full).
    fn enqueue(&self, job: Job) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            self.run(job);
            true
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (m, cv) = &*self.queue;
            let Ok(mut q) = m.lock() else { return false };
            let bytes = job.bytes();
            if q.full(bytes) || (matches!(&job, Job::GuardedLayer(..)) && bytes > QUEUE_BYTES) || q.jobs.try_reserve(1).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let Some(total) = q.bytes.checked_add(bytes) else { return false };
            q.bytes = total;
            q.jobs.push_back(job);
            cv.notify_all();
            true
        }
    }

    /// Carry out a queued write (on the writer thread).
    fn run(&self, job: Job) {
        match job {
            Job::Write(kind, key, payload) => self.write_now(kind, key, &payload),
            Job::GuardedLayer(key, payload, guard) => self.write_guarded(Kind::Layer, key, &payload, Some(&guard)),
            Job::Frame(key, w, h, rgba) => {
                // Queued twice, or written by another instance meanwhile: skip the compression.
                if !self.contains(Kind::Frame, key) {
                    self.write_now(Kind::Frame, key, &encode_frame(w, h, (*rgba).as_ref()));
                }
            }
            Job::Packed(key, packed) => self.write_now(Kind::Frame, key, packed.payload()),
        }
    }

    /// Write an entry now (atomic rename), then evict down to the limit.
    fn write_now(&self, kind: Kind, key: u128, payload: &[u8]) {
        self.write_guarded(kind, key, payload, None);
    }
    fn write_guarded(&self, kind: Kind, key: u128, payload: &[u8], guard: Option<&WriteGuard>) {
        if self.contains(kind, key) {
            return;
        }
        let path = self.path(kind, key);
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let data = seal(kind, payload);
        // Never fill the disk: make room under the volume's limit, or drop the write.
        let need = data.len() as u64;
        let limit = self.limit();
        if need > limit {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.evict_to(limit);
            return;
        }
        self.evict_to(limit - need);
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!("{key:032x}.tmp.{}.{n}", std::process::id()));
        if std::fs::write(&tmp, &data).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        #[cfg(test)]
        {
            let hook = self.before_publish.lock().ok().and_then(|mut h| h.take());
            if let Some(hook) = hook {
                hook();
            }
        }
        let publish = || {
            let Ok(publication) = self.publication.lock() else { return false };
            if publication.1 || guard.is_some_and(|g| !Weak::ptr_eq(&g.purge, &Arc::downgrade(&publication.0))) {
                return false;
            }
            if std::fs::rename(&tmp, &path).is_err() {
                return false;
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = self.index.lock() {
                g.insert(kind, key, data.len() as u64);
            }
            self.changed();
            true
        };
        // Lock order is binding -> disk publication -> index. No owner or queue lock.
        let published = match guard {
            Some(g) => g.binding.with_valid(publish).unwrap_or(false),
            None => publish(),
        };
        if !published {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        self.evict();
    }

    /// Remove least recently used entries until the cache fits its limit.
    fn evict(&self) {
        self.evict_to(self.limit());
    }

    /// The limit in force now: the size setting, capped so the volume keeps its reserve free
    /// (see [`effective_cap`]). Asks the volume each time (a cheap call) so other writers (a
    /// project save, another program) are noticed.
    fn limit(&self) -> u64 {
        let max = self.max_bytes();
        let Some((free, total)) = self.volume.space(self.folder()) else { return max };
        let used = self.index.lock().map(|g| g.total()).unwrap_or_default();
        effective_cap(max, used, free, total)
    }

    fn evict_to(&self, max: u64) {
        let victims = match self.index.lock() {
            Ok(mut g) => g.evict(max),
            Err(_) => return,
        };
        if !victims.is_empty() {
            self.changed();
        }
        for (kind, key) in victims {
            let _ = std::fs::remove_file(self.path(kind, key));
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ------------------------------------------------------------ typed entries

    pub fn get_frame(&self, key: u128) -> Option<Frame8> {
        let p = self.read(Kind::Frame, key)?;
        decode_frame(&p)
    }

    /// A frame as it is stored (compressed, not decoded): what the RAM preview keeps. A frame
    /// written in the older single-block layout is decoded and compressed into strips.
    pub fn get_packed_frame(&self, key: u128) -> Option<crate::frame_pack::PackedFrame> {
        let p = self.read(Kind::Frame, key)?;
        if crate::frame_pack::is_packed_payload(&p) {
            return crate::frame_pack::PackedFrame::from_payload(p);
        }
        let f = decode_frame(&p)?;
        crate::frame_pack::PackedFrame::encode(f.width, f.height, &f.rgba)
    }

    /// Queue a compressed frame for writing (shared, not copied: the same bytes the RAM preview
    /// keeps). `false` when it was not queued: on disk already, or the write queue is full
    /// (the frame is then simply not on disk; nothing waits for room).
    pub fn put_packed_frame(&self, key: u128, packed: Arc<crate::frame_pack::PackedFrame>) -> bool {
        if self.contains(Kind::Frame, key) {
            return false;
        }
        self.enqueue(Job::Packed(key, packed))
    }

    /// Queue a frame for writing (8-bit premultiplied RGBA, `width * height * 4` bytes). The
    /// pixels are taken as they are (pass an `Arc`-backed wrapper to share them without a copy)
    /// and compressed on the writer thread: the caller returns at once.
    pub fn put_frame(&self, key: u128, width: u32, height: u32, rgba: impl AsRef<[u8]> + Send + 'static) {
        let n = rgba.as_ref().len();
        if self.contains(Kind::Frame, key) || Some(n) != (width as usize).checked_mul(height as usize).and_then(|p| p.checked_mul(4)) {
            return;
        }
        self.enqueue(Job::Frame(key, width, height, Box::new(rgba)));
    }

    pub fn get_layer(&self, key: u128) -> Option<Buf> {
        let p = self.read(Kind::Layer, key)?;
        decode_layer(&p)
    }

    /// Queue a layer buffer for writing.
    pub fn put_layer(&self, key: u128, buf: &Buf) {
        if self.contains(Kind::Layer, key) || self.queue_full() {
            return;
        }
        self.enqueue(Job::Write(Kind::Layer, key, encode_layer(buf)));
    }

    /// The write queue has no room (checked before encoding a layer, which costs a pass over
    /// its pixels and a compressed copy).
    fn queue_full(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let full = self.queue.0.lock().map(|q| q.full(0)).unwrap_or(true);
            if full {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            full
        }
        #[cfg(target_arch = "wasm32")]
        false
    }

    /// The write queue has room for `bytes` more without dropping a write (not counted as a
    /// drop): background producers (Cache Frames When Idle past the RAM preview) pace on it.
    pub fn has_room(&self, bytes: usize) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.queue.0.lock().is_ok_and(|q| !q.full(bytes))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = bytes;
            true
        }
    }

    /// Write a layer buffer synchronously (tests).
    pub fn put_layer_now(&self, key: u128, buf: &Buf) {
        self.write_now(Kind::Layer, key, &encode_layer(buf));
    }

    /// A decoded still (footage pixels, see [`still_key`]) written by [`DiskCache::put_still`].
    pub fn get_still(&self, key: u128) -> Option<Image> {
        self.get_layer(key).map(|b| b.img)
    }

    /// Queue a decoded still for writing (exact: the floats as they are). The encoding runs on a
    /// thread of its own, not the caller's (a render waiting for its footage).
    pub fn put_still(self: &Arc<Self>, key: u128, img: Arc<Image>) {
        if self.contains(Kind::Layer, key) || self.queue_full() {
            return;
        }
        let dc = self.clone();
        let spawned = std::thread::Builder::new().name("still to disk".into()).spawn(move || {
            let bytes = encode_parts(&img, [0.0; 2], 1.0);
            dc.enqueue(Job::Write(Kind::Layer, key, bytes));
        });
        if let Err(e) = spawned {
            log::debug!("disk cache: still not written ({e})");
        }
    }

    /// A segmentation (chain key, scale) written by [`DiskCache::put_seg`].
    pub fn get_seg(&self, key: u64, scale: f64) -> Option<effectcraft_effects::roto::FrameSeg> {
        let disk_key = seg_key(key, scale);
        let payload = self.read(Kind::Matte, disk_key)?;
        let decoded = decode_seg(&payload);
        if decoded.is_none() {
            let _ = std::fs::remove_file(self.path(Kind::Matte, disk_key));
            self.forget(Kind::Matte, disk_key);
        }
        decoded
    }

    /// Queue a segmentation for writing.
    pub fn put_seg(&self, key: u64, scale: f64, seg: &effectcraft_effects::roto::FrameSeg) {
        let k = seg_key(key, scale);
        if self.contains(Kind::Matte, k) || self.queue_full() {
            return;
        }
        self.enqueue(Job::Write(Kind::Matte, k, lz4_flex::compress_prepend_size(&seg.to_bytes())));
    }

    /// Write a frame synchronously (tests).
    pub fn put_frame_now(&self, key: u128, width: u32, height: u32, rgba: &[u8]) {
        self.write_now(Kind::Frame, key, &encode_frame(width, height, rgba));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn writer(dc: std::sync::Weak<DiskCache>, q: Arc<(Mutex<Queue>, Condvar)>) {
    let (m, cv) = &*q;
    loop {
        let job = {
            let Ok(mut g) = m.lock() else { return };
            loop {
                if let Some(j) = g.jobs.pop_front() {
                    g.bytes = g.bytes.saturating_sub(j.bytes());
                    g.busy = true;
                    break j;
                }
                if dc.strong_count() == 0 {
                    return;
                }
                g = match cv.wait_timeout(g, std::time::Duration::from_millis(500)) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
        };
        let Some(cache) = dc.upgrade() else { return };
        cache.run(job);
        drop(cache);
        if let Ok(mut g) = m.lock() {
            g.busy = false;
        }
        cv.notify_all();
    }
}

/// An entry file's bytes: header (magic, kind), payload, checksum.
pub fn seal(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(payload.len() + 16);
    data.extend_from_slice(MAGIC);
    data.push(kind.tag());
    data.extend_from_slice(&[0, 0, 0]);
    data.extend_from_slice(payload);
    let sum = checksum(&data);
    data.extend_from_slice(&sum.to_le_bytes());
    data
}

/// A sealed frame entry ([`seal`] of the encoded frame): what the browser's frame workers
/// write to the Origin Private File System.
pub fn frame_entry(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    seal(Kind::Frame, &encode_frame(width, height, rgba))
}

/// A frame from a sealed entry's bytes (`None` when damaged).
pub fn read_frame_entry(data: &[u8]) -> Option<Frame8> {
    decode_frame(verify(data, Kind::Frame)?)
}

/// A sealed layer-buffer entry: what the browser's frame workers write to the Origin Private
/// File System (`layers/<32 hex>.ecc`).
pub fn layer_entry(buf: &Buf) -> Vec<u8> {
    seal(Kind::Layer, &encode_layer(buf))
}

/// A layer buffer from a sealed entry's bytes (`None` when damaged).
pub fn read_layer_entry(data: &[u8]) -> Option<Buf> {
    decode_layer(verify(data, Kind::Layer)?)
}

/// Bound the new cache ingress before decompression or the segmentation parser allocates.
fn decode_seg(payload: &[u8]) -> Option<effectcraft_effects::roto::FrameSeg> {
    const MAX_RAW_BYTES: usize = 256 << 20;
    const MAX_PIXELS: usize = 64 << 20;
    let prefix: [u8; 4] = payload.get(..4)?.try_into().ok()?;
    let bytes = usize::try_from(u32::from_le_bytes(prefix)).ok()?;
    if !(16..=MAX_RAW_BYTES).contains(&bytes) {
        return None;
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(bytes).ok()?;
    raw.resize(bytes, 0);
    if lz4_flex::decompress_into(payload.get(4..)?, &mut raw).ok()? != bytes || raw.get(..4)? != b"ECSG" {
        return None;
    }
    let width = usize::try_from(u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?)).ok()?;
    let height = usize::try_from(u32::from_le_bytes(raw.get(8..12)?.try_into().ok()?)).ok()?;
    let pixels = width.checked_mul(height)?;
    if pixels == 0 || pixels > MAX_PIXELS {
        return None;
    }
    effectcraft_effects::roto::FrameSeg::from_bytes(&raw)
}

/// The disk key of a segmentation: its chain key and scale.
fn seg_key(key: u64, scale: f64) -> u128 {
    let mut h = Hash128::default();
    h.write(b"segmentation-v1");
    h.write_u64(key);
    h.write_u64(scale.to_bits());
    h.finish()
}

impl effectcraft_effects::roto::SegStore for DiskCache {
    fn get_seg(&self, key: u64, scale: f64) -> Option<effectcraft_effects::roto::FrameSeg> {
        DiskCache::get_seg(self, key, scale)
    }
    fn put_seg(&self, key: u64, scale: f64, seg: &effectcraft_effects::roto::FrameSeg) {
        DiskCache::put_seg(self, key, scale, seg);
    }
}

impl crate::cache::LayerStore for DiskCache {
    fn get_layer(&self, key: u128) -> Option<Arc<Buf>> {
        DiskCache::get_layer(self, key).map(Arc::new)
    }
    fn put_layer(&self, key: u128, buf: &Buf) {
        DiskCache::put_layer(self, key, buf);
    }
    fn min_layer_ms(&self) -> u64 {
        DiskCache::min_layer_ms(self)
    }
}

/// An entry's file name in its kind's folder (`<32 hex>.ecc`).
pub fn entry_name(key: u128) -> String {
    format!("{key:032x}.ecc")
}

/// The key of an entry file name (`None` for temporaries and other files).
pub fn parse_entry_name(name: &str) -> Option<u128> {
    let hex = name.strip_suffix(".ecc")?;
    (hex.len() == 32).then(|| u128::from_str_radix(hex, 16).ok()).flatten()
}

/// Check magic, kind and checksum; returns the payload.
fn verify(d: &[u8], kind: Kind) -> Option<&[u8]> {
    if d.len() < 16 || &d[..4] != MAGIC || d[4] != kind.tag() {
        return None;
    }
    let (body, sum) = d.split_at(d.len() - 8);
    let mut s = [0u8; 8];
    s.copy_from_slice(sum);
    (checksum(body) == u64::from_le_bytes(s)).then_some(&body[8..])
}

/// A frame payload: strips ([`crate::frame_pack`], decoded in parallel and shared with the RAM
/// preview), or for a frame that can't be cut (empty) the older single LZ4 block.
fn encode_frame(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    if let Some(p) = crate::frame_pack::PackedFrame::encode(w, h, rgba) {
        return p.into_payload();
    }
    let mut out = Vec::with_capacity(rgba.len() / 2 + 16);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&lz4_flex::compress_prepend_size(rgba));
    out
}

fn decode_frame(p: &[u8]) -> Option<Frame8> {
    if crate::frame_pack::is_packed_payload(p) {
        let packed = crate::frame_pack::PackedFrame::from_payload(p.to_vec())?;
        let rgba = packed.decode(1)?;
        return Some(Frame8 { width: packed.width(), height: packed.height(), rgba });
    }
    if p.len() < 8 {
        return None;
    }
    let w = u32::from_le_bytes(p[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let rgba = lz4_flex::decompress_size_prepended(&p[8..]).ok()?;
    (rgba.len() == w as usize * h as usize * 4).then_some(Frame8 { width: w, height: h, rgba })
}

fn encode_layer(b: &Buf) -> Vec<u8> {
    encode_parts(&b.img, b.offset, b.scale)
}

fn encode_parts(img: &Image, offset: [f64; 2], scale: f64) -> Vec<u8> {
    let (w, h) = (img.width, img.height);
    let n = img.data.len();
    // Byte planes: all first bytes of every float, then all second bytes… (compresses well).
    let mut planes = vec![0u8; n * 16];
    for (i, px) in img.data.iter().enumerate() {
        for c in 0..4 {
            let bytes = px[c].to_le_bytes();
            for (k, byte) in bytes.iter().enumerate() {
                planes[(k * 4 + c) * n + i] = *byte;
            }
        }
    }
    let mut out = Vec::with_capacity(planes.len() / 4 + 40);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&offset[0].to_le_bytes());
    out.extend_from_slice(&offset[1].to_le_bytes());
    out.extend_from_slice(&scale.to_le_bytes());
    out.extend_from_slice(&lz4_flex::compress_prepend_size(&planes));
    out
}

fn decode_layer(p: &[u8]) -> Option<Buf> {
    if p.len() < 32 {
        return None;
    }
    let w = u32::from_le_bytes(p[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let f = |i: usize| -> Option<f64> { Some(f64::from_le_bytes(p[i..i + 8].try_into().ok()?)) };
    let offset = [f(8)?, f(16)?];
    let scale = f(24)?;
    let planes = lz4_flex::decompress_size_prepended(&p[32..]).ok()?;
    let n = w as usize * h as usize;
    if planes.len() != n * 16 {
        return None;
    }
    let mut img = Image::new(w, h);
    for (i, px) in img.data.iter_mut().enumerate() {
        for c in 0..4 {
            let mut bytes = [0u8; 4];
            for (k, byte) in bytes.iter_mut().enumerate() {
                *byte = planes[(k * 4 + c) * n + i];
            }
            px[c] = f32::from_le_bytes(bytes);
        }
    }
    Some(Buf { img, offset, scale })
}

// ---------------------------------------------------------------- content keys

/// Bump when rendering changes in a way that makes old disk entries wrong.
const RENDER_VERSION: &str = "effectcraft-render-2";

/// Bump when decoding stills (PNG, JPEG, PSD, SVG, PDF, …) changes their pixels: decoded stills
/// on disk ([`still_key`]) are named by it.
const STILL_VERSION: &str = "effectcraft-still-1";

/// Disk key of a decoded still: the decoder version, the file (path, size, modification time:
/// an edited file gets a new key) and `how` it was decoded (layer, page, alpha and float
/// interpretation…, as the footage pool keys it). `None` when the file can't be looked at.
pub fn still_key(path: &str, how: &str) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    let mut h = Hash128::default();
    h.write(b"ECSTILL");
    h.write(STILL_VERSION.as_bytes());
    h.write_u64(path.len() as u64);
    h.write(path.as_bytes());
    h.write_u64(meta.len());
    h.write_u64(modified.as_secs());
    h.write_u64(u64::from(modified.subsec_nanos()));
    h.write(how.as_bytes());
    Some(h.finish())
}

/// Feeds formatted text straight into a hash (no intermediate `String`).
struct HashWriter<'a>(&'a mut Hash128);

impl std::fmt::Write for HashWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
        Ok(())
    }
}

/// The `Debug` text of `v` and a terminator (the hash is byte-wise, so streaming it gives the
/// same hash as formatting it into a string first).
fn hash_debug(h: &mut Hash128, v: &impl std::fmt::Debug) {
    use std::fmt::Write;
    let _ = write!(HashWriter(h), "{v:?}");
    h.write(&[0xff]);
}

/// A file's size and modification time, when it can be read.
type Stamp = Option<(u64, Option<u64>)>;

/// File stamps are read again after this long (headless sessions, which get no focus events).
const STAMP_TTL: web_time::Duration = web_time::Duration::from_secs(2);

/// Cached file stamps (see [`refresh_file_stamps`]): the stamp, the refresh generation and the
/// time it was read.
static STAMPS: std::sync::LazyLock<Mutex<HashMap<String, (Stamp, u64, web_time::Instant)>>> = std::sync::LazyLock::new(Mutex::default);
static STAMP_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Stamps cached at most (a long image sequence has one per frame); past it the cache restarts.
const MAX_STAMPS: usize = 1 << 20;

/// Forget the cached file stamps: footage files are stat-ed again the next time a key is taken
/// (the app regaining focus, Reload Footage). Between refreshes a stamp is reused for
/// [`STAMP_TTL`], so an edit doesn't stat every footage file and every sequence frame again.
pub fn refresh_file_stamps() {
    STAMP_GENERATION.fetch_add(1, Ordering::Relaxed);
}

fn read_stamp(path: &str) -> Stamp {
    let md = std::fs::metadata(path).ok()?;
    let t = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64);
    Some((md.len(), t))
}

/// File stamps through the cache, locked once for many files.
struct Stamps<'a> {
    map: Option<std::sync::MutexGuard<'a, HashMap<String, (Stamp, u64, web_time::Instant)>>>,
    generation: u64,
    now: web_time::Instant,
}

impl Stamps<'_> {
    fn new() -> Self {
        let map = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
        Stamps { map: Some(map), generation: STAMP_GENERATION.load(Ordering::Relaxed), now: web_time::Instant::now() }
    }

    fn get(&mut self, path: &str) -> Stamp {
        let Some(map) = self.map.as_mut() else { return read_stamp(path) };
        if let Some((s, g, at)) = map.get(path)
            && *g == self.generation
            && self.now.saturating_duration_since(*at) < STAMP_TTL
        {
            return *s;
        }
        let s = read_stamp(path);
        if map.len() >= MAX_STAMPS {
            map.clear();
        }
        map.insert(path.to_string(), (s, self.generation, self.now));
        s
    }

    /// A file's size and modification time (so edited footage gets new keys).
    fn file(&mut self, h: &mut Hash128, path: &str) {
        if path.is_empty() {
            return;
        }
        if let Some((len, t)) = self.get(path) {
            h.write_u64(len);
            if let Some(t) = t {
                h.write_u64(t);
            }
        }
    }

    fn footage(&mut self, h: &mut Hash128, f: &effectcraft_project::Footage) {
        self.file(h, &f.path);
        for p in &f.sequence {
            self.file(h, p);
        }
    }
}

/// A file's size through the stamp cache (see [`refresh_file_stamps`]): for panels that show
/// footage sizes every frame.
pub fn file_len(path: &str) -> Option<u64> {
    Stamps::new().get(path).map(|(len, _)| len)
}

/// Salt for layer cache keys on disk: the renderer version and every footage file's identity
/// (layer keys name footage items, not file contents).
pub fn footage_salt(project: &effectcraft_project::Project) -> u64 {
    let mut h = Hash128::default();
    h.write(RENDER_VERSION.as_bytes());
    let mut stamps = Stamps::new();
    for (id, it) in &project.items {
        if let effectcraft_project::ItemKind::Footage(f) = &it.kind {
            h.write_u64(id.0);
            hash_debug(&mut h, f);
            stamps.footage(&mut h, f);
        }
    }
    let v = h.finish();
    (v as u64) ^ ((v >> 64) as u64)
}

/// What [`comp_content_key`] needs of one layer: the hash of its pixel form (the Audio, Lock and
/// Shy switches at their defaults) and whether it has an expression.
#[derive(Clone, Copy)]
struct LayerKey {
    hash: u128,
    without_markers: u128,
    expressions: bool,
}

/// [`LayerKey`]s by layer allocation. An entry holds a `Weak` to its layer, so the address can't
/// be reused while the entry exists, and it is current while that layer lives: layers are
/// immutable behind their `Arc` (an edit copies a shared layer, and `Arc::make_mut` moves a
/// layer only `Weak`s point at to a new allocation), so a live entry describes the layer at
/// that address.
struct LayerKeys {
    map: HashMap<usize, (std::sync::Weak<effectcraft_project::Layer>, LayerKey)>,
    /// Entries before dead ones are swept.
    sweep_at: usize,
}

static LAYER_KEYS: std::sync::LazyLock<Mutex<LayerKeys>> = std::sync::LazyLock::new(|| Mutex::new(LayerKeys { map: HashMap::new(), sweep_at: 1024 }));

fn layer_key(l: &Arc<effectcraft_project::Layer>) -> LayerKey {
    let addr = Arc::as_ptr(l) as usize;
    if let Ok(m) = LAYER_KEYS.lock()
        && let Some((w, k)) = m.map.get(&addr)
        && w.strong_count() > 0
        && std::ptr::eq(w.as_ptr(), Arc::as_ptr(l))
    {
        return *k;
    }
    let mut h = Hash128::default();
    if l.switches == l.switches.pixels() {
        hash_debug(&mut h, &**l);
    } else {
        let mut px = (**l).clone();
        px.switches = px.switches.pixels();
        hash_debug(&mut h, &px);
    }
    let mut expressions = false;
    l.props.walk("", &mut |_, p| expressions |= p.expr.is_some());
    let hash = h.finish();
    let without_markers = if l.markers.is_empty() {
        hash
    } else {
        let mut px = (**l).clone();
        px.switches = px.switches.pixels();
        px.markers.clear();
        let mut h = Hash128::default();
        hash_debug(&mut h, &px);
        h.finish()
    };
    let k = LayerKey { hash, without_markers, expressions };
    if let Ok(mut m) = LAYER_KEYS.lock() {
        if m.map.len() >= m.sweep_at {
            m.map.retain(|_, (w, _)| w.strong_count() > 0);
            m.sweep_at = (m.map.len() * 2).max(1024);
        }
        m.map.insert(addr, (Arc::downgrade(l), k));
    }
    k
}

/// Hash of a comp's pixel form ([`effectcraft_project::Comp::pixel_form`]): its settings with
/// Hide Shy Layers off, then each layer's [`LayerKey`] (memoised, so an edit formats only the
/// layers it changed).
fn hash_comp(h: &mut Hash128, c: &effectcraft_project::Comp, layers: &[LayerKey], with_markers: bool) {
    let shell = effectcraft_project::Comp { layers: effectcraft_project::Layers::new(), hide_shy: false, ..c.clone() };
    hash_debug(h, &shell.pixel_form_with(with_markers));
    h.write_u64(layers.len() as u64);
    for k in layers {
        h.write(&if with_markers { k.hash } else { k.without_markers }.to_le_bytes());
    }
}

/// Content key of a composition's pixels: project render settings, the comp and everything it
/// uses (nested comps, footage, solids, their proxies and Use Proxy switches, footage and proxy
/// files' size and time). With expressions in play (which can read any comp) the whole project
/// counts. Nonvisual switches and plain markers do not count without expressions. Protected
/// duration markers always count because they retime nested compositions.
pub fn comp_content_key(project: &effectcraft_project::Project, comp: effectcraft_project::ItemId) -> u128 {
    use effectcraft_project::{ItemKind, LayerSource};
    let mut h = Hash128::default();
    h.write(RENDER_VERSION.as_bytes());
    hash_debug(&mut h, &project.settings);
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![comp];
    let mut expressions = false;
    let mut comp_layers: HashMap<effectcraft_project::ItemId, Vec<LayerKey>> = HashMap::new();
    let keys_of = |c: &effectcraft_project::Comp| -> Vec<LayerKey> { c.layers.arcs().iter().map(layer_key).collect() };
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(it) = project.item(id) else { continue };
        if let ItemKind::Comp(c) = &it.kind {
            let keys = keys_of(c);
            for (l, k) in c.layers.iter().zip(&keys) {
                if let LayerSource::Comp { item } | LayerSource::Footage { item } | LayerSource::Solid { item } = l.source {
                    stack.push(item);
                }
                expressions |= k.expressions;
            }
            comp_layers.insert(id, keys);
        }
    }
    let ids: Vec<effectcraft_project::ItemId> = if expressions { project.items.keys().copied().collect() } else { seen.into_iter().collect() };
    let mut stamps = Stamps::new();
    for id in ids {
        let Some(it) = project.item(id) else { continue };
        h.write_u64(id.0);
        h.write(it.name.as_bytes());
        match &it.kind {
            ItemKind::Comp(c) => match comp_layers.get(&id) {
                Some(keys) => hash_comp(&mut h, c, keys, expressions),
                None => hash_comp(&mut h, c, &keys_of(c), expressions),
            },
            k => hash_debug(&mut h, k),
        }
        if let ItemKind::Footage(f) = &it.kind {
            stamps.footage(&mut h, f);
        }
        hash_debug(&mut h, &it.proxy);
        if let Some(px) = &it.proxy {
            stamps.footage(&mut h, &px.footage);
        }
    }
    h.write_u64(comp.0);
    h.finish()
}

/// The per-frame content of a viewer frame named by its fingerprint
/// (`Renderer::frame_fingerprint`) instead of its comp's [`comp_content_key`]: the fingerprint,
/// the footage files' identities ([`footage_salt`]: the fingerprint names footage items, not
/// file contents) and the comp. An edit changes it only for the frames it changes.
pub fn print_content(print: u64, footage_salt: u64, comp: u64) -> u128 {
    let mut h = Hash128::default();
    h.write(b"ECFP");
    h.write_u64(print);
    h.write_u64(footage_salt);
    h.write_u64(comp);
    h.finish()
}

/// Disk key of one viewer frame.
pub fn frame_key(content: u128, frame: i64, scale_milli: u32, view: u64) -> u128 {
    let mut h = Hash128::default();
    h.write(&content.to_le_bytes());
    h.write_u64(frame as u64);
    h.write_u64(scale_milli as u64);
    h.write_u64(view);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("effectcraft-disk-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn buf(seed: u32, w: u32, h: u32) -> Buf {
        let mut img = Image::new(w, h);
        for (i, p) in img.data.iter_mut().enumerate() {
            let v = ((i as u32).wrapping_mul(2654435761u32).wrapping_add(seed) % 1000) as f32 / 999.0;
            *p = [v, v * 0.5, 1.0 - v, if i % 7 == 0 { 0.0 } else { 1.0 }];
        }
        Buf { img, offset: [1.5, -2.25], scale: 0.5 }
    }

    #[test]
    fn captured_layer_intent_rejects_binding_cycles_replacement_clear_and_purge() {
        let dir = tmpdir("captured-intent");
        let disk = DiskCache::open(&dir, 1 << 20).unwrap();
        disk.set_min_layer_ms(0);
        let cache = crate::cache::LayerCache::new(1024);
        cache.set_disk(Some((disk.clone(), 7)));
        let pixels = buf(3, 3, 2);
        let old = cache.disk_intent(11).unwrap();
        cache.set_disk_salt(8);
        cache.set_disk_salt(7);
        old.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        let old = cache.disk_intent(11).unwrap();
        cache.set_disk(None);
        cache.set_disk(Some((disk.clone(), 7)));
        old.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        let old = cache.disk_intent(11).unwrap();
        cache.set_gate(Some(Arc::new(std::sync::atomic::AtomicBool::new(false))));
        old.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        cache.set_gate(Some(gate.clone()));
        let gated = cache.disk_intent(11).unwrap();
        gate.store(true, Ordering::Relaxed);
        gated.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        gate.store(false, Ordering::Relaxed);
        let old = cache.disk_intent(11).unwrap();
        cache.clear();
        old.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        let old = cache.disk_intent(11).unwrap();
        disk.clear();
        old.put(&pixels);
        disk.flush();
        assert_eq!(disk.stats().entries, 0);
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        cache.set_gate(Some(gate.clone()));
        let fresh = cache.disk_intent(11).unwrap();
        cache.set_gate(Some(gate)); // unchanged identity is not cancellation
        cache.set_disk(Some((disk.clone(), 7)));
        cache.set_disk_salt(7);
        fresh.put(&pixels);
        disk.flush();
        assert_eq!(disk.get_layer((7_u128 << 64) | 11).unwrap().img.data, pixels.img.data);
        disk.clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn captured_layer_jobs_revalidate_after_encoding_and_before_rename() {
        let dir = tmpdir("captured-final-publish");
        let disk = DiskCache::open(&dir, 1 << 20).unwrap();
        disk.set_min_layer_ms(0);
        let cache = crate::cache::LayerCache::new(1024);
        cache.set_disk(Some((disk.clone(), 9)));
        let pixels = buf(4, 3, 2);
        for purge in [false, true] {
            let (entered, entered_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            *disk.before_publish.lock().unwrap() = Some(Box::new(move || {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
            }));
            let old = cache.disk_intent(12).unwrap();
            old.put(&pixels);
            entered_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            // A second queued job must also be rejected by a purge.
            cache.disk_intent(13).unwrap().put(&pixels);
            let clear = if purge {
                let dc = disk.clone();
                let worker = std::thread::spawn(move || dc.clear());
                let start = std::time::Instant::now();
                while !disk.publication.lock().unwrap().1 {
                    assert!(start.elapsed() < std::time::Duration::from_secs(2));
                    std::thread::yield_now();
                }
                Some(worker)
            } else {
                cache.set_disk_salt(10);
                cache.set_disk_salt(9);
                None
            };
            release.send(()).unwrap();
            if let Some(worker) = clear {
                worker.join().unwrap();
            }
            disk.flush();
            assert_eq!(disk.stats().entries, 0);
            assert!(!disk.path(Kind::Layer, (9_u128 << 64) | 12).exists());
            assert!(!disk.path(Kind::Layer, (9_u128 << 64) | 13).exists());
        }
        cache.disk_intent(14).unwrap().put(&pixels);
        disk.flush();
        assert!(disk.get_layer((9_u128 << 64) | 14).is_some());
        disk.clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn guarded_queue_counts_compressed_allocation_capacity() {
        let dir = tmpdir("captured-capacity");
        let disk = DiskCache::open(&dir, 1 << 20).unwrap();
        disk.set_min_layer_ms(0);
        let cache = crate::cache::LayerCache::new(1024);
        cache.set_disk(Some((disk.clone(), 0)));
        let intent = cache.disk_intent(1).unwrap();
        let mut payload = Vec::with_capacity(128);
        payload.push(1);
        let capacity = payload.capacity();
        let job = Job::GuardedLayer(1, payload, intent.guard);
        assert_eq!(job.bytes(), capacity);
        drop(job);
        disk.clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The write queue is bounded by bytes as well as by count; one oversized write still goes
    /// when nothing else is pending.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn write_queue_is_bounded_by_bytes() {
        let mut q = Queue { jobs: Default::default(), bytes: 0, busy: false };
        assert!(!q.full(QUEUE_BYTES * 2), "empty: one big write goes");
        q.jobs.push_back(Job::Write(Kind::Layer, 1, vec![]));
        q.bytes = QUEUE_BYTES - 10;
        assert!(!q.full(10));
        assert!(q.full(11));
        q.bytes = 0;
        for k in 1..QUEUE_LIMIT {
            q.jobs.push_back(Job::Write(Kind::Layer, k as u128, vec![]));
        }
        assert!(q.full(0), "count limit");
    }

    /// A queued frame counts its uncompressed bytes against the byte budget.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn queued_frames_count_uncompressed_bytes() {
        let job = Job::Frame(1, 64, 32, Box::new(vec![0u8; 64 * 32 * 4]));
        assert_eq!(job.bytes(), 64 * 32 * 4);
        assert_eq!(Job::Write(Kind::Layer, 1, vec![0; 10]).bytes(), 10);
    }

    /// Frames shared with the caller (an `Arc`, as the viewer's frame cache hands them over) are
    /// compressed on the writer thread and read back exactly; a wrong size is not queued.
    #[test]
    fn shared_frames_are_written_by_the_writer() {
        struct Shared(Arc<Vec<u8>>);
        impl AsRef<[u8]> for Shared {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        let dir = tmpdir("shared");
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        let rgba: Arc<Vec<u8>> = Arc::new((0..64 * 48 * 4).map(|i| (i * 7 % 253) as u8).collect());
        dc.put_frame(3, 64, 48, Shared(rgba.clone()));
        dc.put_frame(4, 64, 47, Shared(rgba.clone()));
        dc.flush();
        assert_eq!(dc.get_frame(3).unwrap(), Frame8 { width: 64, height: 48, rgba: (*rgba).clone() });
        assert!(dc.get_frame(4).is_none());
        assert_eq!(Arc::strong_count(&rgba), 1, "the writer released the frame");
        // Already on disk: not queued again.
        dc.put_frame(3, 64, 48, Shared(rgba.clone()));
        dc.flush();
        assert_eq!(dc.stats().writes, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A comp with two solid layers.
    fn keyed_project() -> (effectcraft_project::Project, effectcraft_project::ItemId) {
        use effectcraft_project::*;
        let mut p = Project::default();
        let mut c = Comp::new(64, 64, effectcraft_time::FrameRate::FPS_30, effectcraft_time::Tick::from_seconds_f64(1.0));
        for (i, color) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]].into_iter().enumerate() {
            let sid =
                p.add_item(&format!("S{i}"), effectcraft_color::Label::Red, None, ItemKind::Solid(Solid { color, width: 8, height: 8, pixel_aspect: 1.0 }));
            let l = build::layer(&mut p, &c, "S", LayerSource::Solid { item: sid }, (8, 8), None);
            c.layers.push(l);
        }
        let cid = p.add_item("C", effectcraft_color::Label::Red, None, ItemKind::Comp(Arc::new(c)));
        (p, cid)
    }

    /// Memoised layer keys (#G5): a layer edited in place (its `Arc` unique, so `make_mut` reuses
    /// it unless a memo entry points at it) gets a new key, and memoised keys equal fresh ones.
    #[test]
    fn content_keys_follow_layer_edits() {
        let (mut p, cid) = keyed_project();
        let k1 = comp_content_key(&p, cid);
        assert_eq!(comp_content_key(&p, cid), k1, "memoised");
        // A deep copy (new layer allocations) has the same key.
        let deep: effectcraft_project::Project = effectcraft_project::Project::from_json(&p.to_json()).unwrap();
        assert_eq!(comp_content_key(&deep, cid), k1);
        // Edit a layer in place: nothing else shares the project, its comp or the layer.
        let l = p.comp_mut(cid).unwrap().layers.get_mut(1).unwrap();
        l.props.prop_mut("transform/opacity").unwrap().value = effectcraft_project::Value::Scalar(50.0);
        let k2 = comp_content_key(&p, cid);
        assert_ne!(k2, k1);
        let deep: effectcraft_project::Project = effectcraft_project::Project::from_json(&p.to_json()).unwrap();
        assert_eq!(comp_content_key(&deep, cid), k2);
        // Shy doesn't change pixels.
        p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap().switches.shy = true;
        assert_eq!(comp_content_key(&p, cid), k2);
        // An expression makes every item count: renaming an unrelated item changes the key.
        p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/opacity").unwrap().expr =
            Some(effectcraft_project::Expression { text: "value".into(), enabled: true });
        let k3 = comp_content_key(&p, cid);
        assert_ne!(k3, k2);
        let other = p.add_item("Other", effectcraft_color::Label::Red, None, effectcraft_project::ItemKind::Folder);
        assert_ne!(comp_content_key(&p, cid), k3);
        p.items.remove(&other);
        assert_eq!(comp_content_key(&p, cid), k3);
    }

    /// File stamps are reused until a refresh.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn file_stamps_refresh() {
        let d = tmpdir("stamps");
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("a.bin");
        std::fs::write(&f, b"1").unwrap();
        let path = f.to_string_lossy().to_string();
        let one = Stamps::new().get(&path);
        assert_eq!(one.map(|s| s.0), Some(1));
        std::fs::write(&f, b"22").unwrap();
        refresh_file_stamps();
        assert_eq!(Stamps::new().get(&path).map(|s| s.0), Some(2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn index_evicts_least_recently_used() {
        let mut ix = DiskIndex::default();
        ix.insert(Kind::Frame, 1, 100);
        ix.insert(Kind::Frame, 2, 100);
        ix.insert(Kind::Layer, 3, 100);
        assert_eq!((ix.len(), ix.total()), (3, 300));
        assert!(ix.touch(Kind::Frame, 1));
        assert!(!ix.touch(Kind::Frame, 3));
        assert_eq!(ix.lru_order(), vec![(Kind::Frame, 2), (Kind::Layer, 3), (Kind::Frame, 1)]);
        assert_eq!(ix.evict(250), vec![(Kind::Frame, 2)]);
        assert!(ix.evict(250).is_empty());
        ix.insert(Kind::Frame, 1, 50);
        assert_eq!(ix.total(), 150);
        assert_eq!(ix.remove(Kind::Layer, 3), Some(100));
        assert_eq!(ix.keys(Kind::Frame), vec![1]);
        ix.clear();
        assert!(ix.is_empty() && ix.total() == 0);
    }

    #[test]
    fn frame_entries_round_trip_and_names_parse() {
        let rgba: Vec<u8> = (0..4 * 6 * 3).map(|i| (i * 7 % 256) as u8).collect();
        let e = frame_entry(6, 3, &rgba);
        assert_eq!(read_frame_entry(&e), Some(Frame8 { width: 6, height: 3, rgba: rgba.clone() }));
        let mut bad = e.clone();
        bad[20] ^= 1;
        assert!(read_frame_entry(&bad).is_none());
        let key = 0x0123_4567_89ab_cdef_0011_2233_4455_6677u128;
        assert_eq!(parse_entry_name(&entry_name(key)), Some(key));
        assert_eq!(parse_entry_name("abc.ecc"), None);
        assert_eq!(parse_entry_name(&format!("{key:032x}.tmp.1")), None);
    }

    /// Segmentations round-trip by (chain key, scale), survive a reopen (rescan), are bounded by
    /// the size limit like every entry, and a damaged file is a miss.
    #[test]
    fn segmentations_round_trip_and_reopen() {
        use effectcraft_effects::roto::FrameSeg;
        let dir = tmpdir("seg");
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        let seg = FrameSeg { w: 7, h: 5, matte: (0..35).map(|i| (i % 3 == 0) as u8).collect(), refine: vec![0; 35], ..Default::default() };
        assert!(dc.get_seg(42, 1.0).is_none());
        dc.put_seg(42, 1.0, &seg);
        dc.flush();
        assert_eq!(dc.get_seg(42, 1.0), Some(seg.clone()));
        assert!(dc.get_seg(42, 0.5).is_none() && dc.get_seg(43, 1.0).is_none());
        assert_eq!(dc.keys(Kind::Matte).len(), 1);
        drop(dc);
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        assert_eq!(dc.get_seg(42, 1.0), Some(seg.clone()));
        // Damaged on disk: a miss, and the entry is forgotten.
        let path = dc.path(Kind::Matte, seg_key(42, 1.0));
        let mut d = std::fs::read(&path).unwrap();
        d[18] ^= 0x55;
        std::fs::write(&path, d).unwrap();
        assert!(dc.get_seg(42, 1.0).is_none());
        assert!(!dc.contains(Kind::Matte, seg_key(42, 1.0)));
        // The size limit evicts segmentations too.
        dc.put_seg(1, 1.0, &seg);
        dc.flush();
        dc.set_max_bytes(1);
        assert!(dc.keys(Kind::Matte).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_miss_and_exact_round_trip() {
        let dir = tmpdir("hit");
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        assert!(dc.get_layer(42).is_none());
        let b = buf(1, 33, 17);
        dc.put_layer(42, &b);
        dc.flush();
        let back = dc.get_layer(42).unwrap();
        assert_eq!(back.img, b.img);
        assert_eq!((back.offset, back.scale), (b.offset, b.scale));
        let rgba: Vec<u8> = (0..20 * 10 * 4).map(|i| (i % 251) as u8).collect();
        dc.put_frame(7, 20, 10, rgba.clone());
        dc.flush();
        assert_eq!(dc.get_frame(7).unwrap().rgba, rgba);
        let st = dc.stats();
        assert_eq!((st.entries, st.hits, st.misses, st.writes), (2, 2, 1, 2));
        // Wrong kind is a miss.
        assert!(dc.get_frame(42).is_none());
    }

    #[test]
    fn persists_across_restarts_and_clears() {
        let dir = tmpdir("persist");
        {
            let dc = DiskCache::open(&dir, 1 << 30).unwrap();
            dc.put_layer_now(1, &buf(1, 8, 8));
            dc.put_frame_now(2, 2, 2, &[9; 16]);
        }
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        assert_eq!(dc.stats().entries, 2);
        assert!(dc.contains(Kind::Layer, 1) && dc.contains(Kind::Frame, 2));
        assert_eq!(dc.get_frame(2).unwrap().rgba, vec![9; 16]);
        dc.clear();
        assert_eq!(dc.stats().entries, 0);
        assert!(dc.get_frame(2).is_none());
        let again = DiskCache::open(&dir, 1 << 30).unwrap();
        assert_eq!(again.stats().entries, 0);
    }

    #[test]
    fn lru_eviction_respects_the_limit() {
        let dir = tmpdir("lru");
        let dc = DiskCache::open(&dir, u64::MAX).unwrap();
        for k in 0..6u128 {
            dc.put_layer_now(k, &buf(k as u32, 64, 64));
        }
        let per = dc.stats().bytes / 6;
        // Touch 0 so it is the most recently used.
        assert!(dc.get_layer(0).is_some());
        dc.set_max_bytes(per * 3 + per / 2);
        let st = dc.stats();
        assert!(st.bytes <= per * 3 + per / 2, "{st:?}");
        assert_eq!(st.entries, 3);
        assert!(dc.contains(Kind::Layer, 0), "recently used survives");
        assert!(!dc.contains(Kind::Layer, 1) && !dc.contains(Kind::Layer, 2) && !dc.contains(Kind::Layer, 3));
        assert_eq!(st.evictions, 3);
        // Files are really gone.
        let files = walk(&dir);
        assert_eq!(files, 3);
        // New writes keep evicting.
        dc.put_layer_now(99, &buf(9, 64, 64));
        assert!(dc.stats().bytes <= per * 3 + per / 2);
    }

    fn walk(d: &Path) -> usize {
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    n += walk(&p);
                } else if p.extension().is_some_and(|x| x == "ecc") {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn damaged_files_are_misses() {
        let dir = tmpdir("damaged");
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        dc.put_frame_now(5, 4, 4, &[1; 64]);
        let p = dc.path(Kind::Frame, 5);
        let mut d = std::fs::read(&p).unwrap();
        let n = d.len();
        d[n / 2] ^= 0xff;
        std::fs::write(&p, d).unwrap();
        assert!(dc.get_frame(5).is_none());
        assert!(!dc.contains(Kind::Frame, 5));
        assert!(!p.exists());
    }

    #[test]
    fn concurrent_readers_and_writers() {
        let dir = tmpdir("concurrent");
        let a = DiskCache::open(&dir, 1 << 30).unwrap();
        // A second instance on the same folder (another process, in effect).
        let b = DiskCache::open(&dir, 1 << 30).unwrap();
        std::thread::scope(|s| {
            for t in 0..4u32 {
                let (a, b) = (a.clone(), b.clone());
                s.spawn(move || {
                    for i in 0..20u128 {
                        let k = i % 5;
                        let w = if t % 2 == 0 { &a } else { &b };
                        w.put_layer_now(k, &buf(k as u32, 16, 16));
                        if let Some(x) = w.get_layer(k) {
                            assert_eq!(x.img, buf(k as u32, 16, 16).img);
                        }
                    }
                });
            }
        });
        b.rescan();
        for k in 0..5u128 {
            assert_eq!(b.get_layer(k).unwrap().img, buf(k as u32, 16, 16).img);
        }
    }

    // ------------------------------------------------------------ free-space guard

    #[test]
    fn reserve_and_effective_cap_math() {
        const GB: u64 = 1 << 30;
        // The reserve is 10% of the volume, between 5 GB and 20 GB.
        assert_eq!(reserve_bytes(0), 5 * GB);
        assert_eq!(reserve_bytes(60 * GB), 6 * GB);
        assert_eq!(reserve_bytes(40 * GB), 5 * GB);
        assert_eq!(reserve_bytes(120 * GB), 12 * GB);
        assert_eq!(reserve_bytes(1000 * GB), 20 * GB);
        assert_eq!(reserve_bytes(u64::MAX), 20 * GB);
        // Plenty of room: the size setting rules.
        assert_eq!(effective_cap(100 * GB, 0, 500 * GB, 1000 * GB), 100 * GB);
        // 1,200 GB setting on a 60 GB machine with 50 GB free and 4 GB cached: 48 GB (reserve 6).
        assert_eq!(effective_cap(1200 * GB, 4 * GB, 50 * GB, 60 * GB), 48 * GB);
        // Under the reserve already: nothing may stay, and nothing underflows.
        assert_eq!(effective_cap(1200 * GB, 0, 2 * GB, 60 * GB), 0);
        assert_eq!(effective_cap(1200 * GB, GB, 2 * GB, 60 * GB), 0);
        assert_eq!(effective_cap(1200 * GB, 0, 0, 0), 0);
        // Exactly at the reserve.
        assert_eq!(effective_cap(1200 * GB, 3 * GB, 3 * GB, 60 * GB), 0);
        assert_eq!(effective_cap(1200 * GB, 3 * GB, 4 * GB, 60 * GB), GB);
        // Huge inputs saturate instead of overflowing.
        assert_eq!(effective_cap(u64::MAX, u64::MAX, u64::MAX, u64::MAX), u64::MAX - 20 * GB);
        assert_eq!(effective_cap(10, u64::MAX, 5, 100 * GB), 10);
        assert_eq!(effective_cap(0, 5, 5, 0), 0);
    }

    /// A disk whose free space is its reserve plus `spare` minus what the cache folder holds.
    struct FakeDisk {
        dir: PathBuf,
        total: u64,
        spare: AtomicU64,
    }

    impl FakeDisk {
        fn new(dir: &Path, spare: u64) -> Arc<FakeDisk> {
            Arc::new(FakeDisk { dir: dir.to_path_buf(), total: 100 << 30, spare: AtomicU64::new(spare) })
        }
        fn held(d: &Path) -> u64 {
            let mut n = 0;
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        n += Self::held(&p);
                    } else {
                        n += e.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
            n
        }
    }

    impl VolumeSpace for FakeDisk {
        fn space(&self, _: &Path) -> Option<(u64, u64)> {
            let free = (reserve_bytes(self.total) + self.spare.load(Ordering::Relaxed)).saturating_sub(Self::held(&self.dir));
            Some((free, self.total))
        }
    }

    /// A disk that is nearly full stops the cache writing and makes it evict the oldest
    /// entries; the size setting alone (here far larger than the disk) never fills it.
    #[test]
    fn low_free_space_stops_writes_and_evicts_oldest() {
        let dir = tmpdir("freeguard");
        let disk = FakeDisk::new(&dir, u64::MAX / 2);
        let dc = DiskCache::open_with_volume(&dir, 1200 << 30, disk.clone()).unwrap();
        dc.put_layer_now(0, &buf(0, 64, 64));
        let per = dc.stats().bytes;
        assert!(per > 0);
        // Room for three and a half entries beyond the reserve.
        let spare = per * 3 + per / 2;
        disk.spare.store(spare, Ordering::Relaxed);
        for k in 1..8u128 {
            dc.put_layer_now(k, &buf(k as u32, 64, 64));
            assert!(FakeDisk::held(&dir) <= spare, "the cache never eats into the reserve");
        }
        let st = dc.stats();
        assert_eq!(st.entries, 3, "{st:?}");
        assert_eq!((st.effective_max_bytes, st.max_bytes), (st.bytes + st.free_bytes - st.reserve_bytes, 1200 << 30));
        assert!(st.effective_max_bytes <= spare && st.reserve_bytes == 10 << 30);
        assert!(st.evictions >= 5);
        // The newest survive, the oldest went.
        assert!(dc.contains(Kind::Layer, 7) && dc.contains(Kind::Layer, 6) && dc.contains(Kind::Layer, 5));
        assert!(!dc.contains(Kind::Layer, 0) && !dc.contains(Kind::Layer, 1));

        // Something else (a project save, a download) takes the space: the next write notices,
        // evicts until the reserve is back, and is itself dropped when nothing is left to spare.
        disk.spare.store(0, Ordering::Relaxed);
        let (writes, dropped) = (st.writes, st.dropped);
        dc.put_layer_now(100, &buf(9, 64, 64));
        let st = dc.stats();
        assert_eq!((st.entries, st.bytes), (0, 0), "{st:?}");
        assert_eq!(st.effective_max_bytes, 0);
        assert_eq!((st.writes, st.dropped), (writes, dropped + 1), "the write was dropped");
        assert!(!dc.contains(Kind::Layer, 100));
        assert_eq!(walk(&dir), 0, "files are really gone");

        // Room again: writes resume.
        disk.spare.store(spare, Ordering::Relaxed);
        dc.put_layer_now(101, &buf(1, 64, 64));
        assert!(dc.contains(Kind::Layer, 101));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opening with a size setting from a bigger machine evicts down to what the disk can
    /// spare, and the queued (background) write path obeys the same limit.
    #[test]
    fn open_on_a_small_disk_evicts_and_queued_writes_obey() {
        let dir = tmpdir("freeguard-open");
        let roomy = FakeDisk::new(&dir, u64::MAX / 2);
        let dc = DiskCache::open_with_volume(&dir, 1200 << 30, roomy).unwrap();
        for k in 0..6u128 {
            dc.put_layer_now(k, &buf(k as u32, 64, 64));
        }
        let per = dc.stats().bytes / 6;
        drop(dc);
        let spare = per * 2 + per / 2;
        let tight = FakeDisk::new(&dir, spare);
        let dc = DiskCache::open_with_volume(&dir, 1200 << 30, tight.clone()).unwrap();
        assert_eq!(dc.stats().entries, 2);
        assert!(FakeDisk::held(&dir) <= spare);
        // The newest two (by file time) stay when the times differ; at least the limit holds.
        for k in 10..16u128 {
            dc.put_layer(k, &buf(k as u32, 64, 64));
            dc.flush();
            assert!(FakeDisk::held(&dir) <= spare);
        }
        assert_eq!(dc.stats().entries, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A volume that cannot say how much is free leaves the size setting in charge.
    #[test]
    fn unknown_free_space_follows_the_size_setting() {
        struct Unknown;
        impl VolumeSpace for Unknown {
            fn space(&self, _: &Path) -> Option<(u64, u64)> {
                None
            }
        }
        let dir = tmpdir("freeguard-unknown");
        let dc = DiskCache::open_with_volume(&dir, 7 << 20, Arc::new(Unknown)).unwrap();
        dc.put_layer_now(1, &buf(1, 16, 16));
        let st = dc.stats();
        assert_eq!((st.entries, st.effective_max_bytes, st.free_bytes, st.reserve_bytes), (1, 7 << 20, 0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
