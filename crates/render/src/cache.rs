//! Layer render cache.
//!
//! The processed pixels of a layer (source → masks → effects, in layer space) depend only on the
//! layer's non-transform properties, its source item, selected-layer dependencies and the render
//! options. Its own transform is normally applied later; referenced layers' transforms can
//! affect their sampled pixels. [`LayerCache`] keeps those buffers keyed
//! by a content hash of exactly those inputs, *evaluated at the frame time*, so:
//!
//! * a static layer (or one whose only animation is in Transform) renders once and is reused on
//!   every frame while scrubbing or playing;
//! * editing one layer re-renders only that layer — every other layer's cached pixels stay valid
//!   because their keys do not change;
//! * any change to a property value, keyframe, expression result, effect, mask, switch or source
//!   item changes the key, so stale pixels are never returned.
//!
//! Layers whose pixels depend on time in ways the property values do not capture (time-based
//! effects such as Noise or Wave Warp, Wiggle Paths, moving footage) fold the layer time (or the
//! source frame) into the key; a still is keyed by its item alone. A precomp is keyed by its
//! nested comp's content at the nested time ([`crate::precomp_key`]), so a static precomp
//! renders once.
//!
//! The static part of a key (the property tree's structure, formatted and hashed) is memoised
//! per shared layer ([`LayerInfo`]): layers are `Arc`-shared between project snapshots and an
//! edit makes a new allocation, so an allocation that is still alive still holds the same
//! layer. The memo keeps a `Weak` on each allocation it describes, so the address can't be
//! reused by another layer while the entry exists.
//!
//! Concurrent misses on one key (parallel frames reaching the same static layer) render it once:
//! the first claims the key ([`LayerCache::claim`]) and the others wait for its buffer.
//!
//! **Admission.** Most of what an animated comp renders is used once (a layer whose effect
//! animates has a new key every frame), and keeping it would fill the budget with buffers that
//! never hit. So a buffer whose key was never rendered before goes on probation: a small window
//! ([`LayerCache::window`], first in first out) that holds the most recent first renders. A hit
//! there (the next frame of a static layer, a precomp used again in the same frame, a waiter of
//! the in-flight render) admits it to the cache proper. A key rendered again after its probation
//! entry left the window (a busy frame flushed it, a RAM-preview loop came round) is remembered
//! by a bounded doorkeeper of recently rendered keys and admitted straight away.
//!
//! The window follows the frame: two frames' worth of pixels once the renderer has said how big
//! its frames are ([`LayerCache::begin_frame`]), so a Half-resolution render keeps a quarter of
//! what a Full one does. A buffer whose key changes every frame (an animated precomp,
//! [`LayerCache::insert_once`]) could only be reused within its frame (a precomp nested in one
//! used several times), which saves less than holding it costs: it isn't kept, unless it is
//! rendered again after the frame that first rendered it finished (scrubbing back, playing
//! again), when the doorkeeper admits it.

use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError, RwLock, Weak};

use effectcraft_effects::Buf;
use effectcraft_project::{Comp, FootageKind, ItemKind, Layer, LayerSource, Node, PropGroup};

use crate::eval::EvalCtx;
use crate::packed::PackedBuf;

mod dependencies;
pub(crate) mod free;

/// Thread-safe, memory-budgeted cache of processed layer buffers, optionally backed by the
/// persistent [`DiskCache`](crate::disk_cache::DiskCache): a memory miss reads the disk, and
/// buffers that were slow to render are written there (keys salted per project, see
/// [`LayerCache::set_disk`]).
mod gpu;
pub use gpu::{GpuOwner, GpuPending, GpuSlot, GpuStage, ResidentReadback};

pub struct LayerCache {
    storage_epoch: Arc<Mutex<Arc<()>>>,
    resident_lifetime: Mutex<Arc<()>>,
    inner: Arc<Mutex<Inner>>,
    disk: Mutex<Option<(Arc<crate::disk_cache::DiskCache>, u64)>>,
    /// Another persistent store when there is no disk cache (the browser's, [`PrefetchStore`]).
    store: Mutex<Option<(Arc<dyn LayerStore>, u64)>>,
    /// While set, inserts are dropped (see [`LayerCache::set_gate`]).
    gate: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
    /// Keys being rendered right now, by whom, and owners that stopped making progress (see
    /// [`LayerCache::claim`]).
    inflight: Arc<Mutex<Flights>>,
    /// Precomp layers' consecutive key misses (see [`LayerCache::precomp_try`]).
    streaks: Mutex<HashMap<u64, u32>>,
    /// Shape layers' whole raster buffers by content key (see [`LayerCache::shape_rect`]).
    shape_rects: Mutex<HashMap<u64, Option<ShapeRect>>>,
    /// Shape layers' last content key, when it was seen and for how many times since it held
    /// (see [`LayerCache::content_held`]).
    shape_keys: Mutex<HashMap<u64, (u64, effectcraft_time::Tick, u32)>>,
}

/// A shape layer's whole raster buffer: width, height and offset (see
/// [`crate::shapes::buffer_rect`]).
pub(crate) type ShapeRect = (u32, u32, [f64; 2]);

/// Weak lifetime of accelerator content associated with a layer cache. Clearing
/// or dropping the cache invalidates previously captured tokens, including late inserts.
#[derive(Clone)]
pub struct ResidentToken {
    lifetime: Weak<()>,
    gate: Option<Weak<std::sync::atomic::AtomicBool>>,
}
impl ResidentToken {
    pub fn identity(&self) -> usize {
        self.lifetime.as_ptr() as usize
    }
    pub fn valid(&self) -> bool {
        self.lifetime.strong_count() > 0 && self.gate.as_ref().is_none_or(|g| g.upgrade().is_some_and(|g| !g.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

/// A captured storage binding. Only native file publication runs inside this barrier.
#[derive(Clone)]
pub(crate) struct StorageToken {
    gate: Option<Weak<std::sync::atomic::AtomicBool>>,
    state: Weak<Mutex<Arc<()>>>,
    epoch: Weak<()>,
}
impl StorageToken {
    pub(crate) fn with_valid<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.upgrade()?;
        let epoch = state.lock().ok()?;
        if self.gate.as_ref().is_some_and(|g| g.upgrade().is_none_or(|g| g.load(std::sync::atomic::Ordering::Relaxed))) {
            return None;
        }
        if !Weak::ptr_eq(&self.epoch, &Arc::downgrade(&epoch)) {
            return None;
        }
        Some(f())
    }
    pub(crate) fn valid(&self) -> bool {
        self.with_valid(|| ()).is_some()
    }
}

struct Entry {
    buf: Stored,
    /// Bytes of pixel storage (packed size when packed).
    bytes: usize,
    last_use: u64,
    /// [`PackedBuf::pack`] declined this buffer (not on an integer grid): don't try again.
    unpackable: bool,
    /// A first render on probation (see the module docs): admitted on its first hit, dropped
    /// first in first out when the probation window is full.
    probation: bool,
}

/// How an entry keeps its pixels. Entries go in as `f32` buffers (a hit shares the `Arc`, and
/// the GPU compositor keeps their textures while it lives); under admission pressure
/// the least recently used ones are packed (lossless, see [`crate::packed`]) before anything
/// is evicted, and a hit on a packed entry unpacks it back to `f32`.
enum Stored {
    Full(Arc<Buf>),
    Packed(Arc<PackedBuf>),
}

struct Inner {
    gpu: HashMap<usize, gpu::GpuEntry>,
    physical: crate::retained::RetainedBudget,
    map: HashMap<u64, Entry>,
    bytes: usize,
    budget: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    /// Buffers inserted (rendered, or read from disk).
    inserts: u64,
    /// When each recent miss happened (to time the render that follows it).
    missed_at: HashMap<u64, web_time::Instant>,
    /// Bytes of the entries on probation (part of `bytes`).
    probation_bytes: usize,
    /// Keys rendered recently (admission, see the module docs).
    seen: Doorkeeper,
    /// Bytes of the frames being rendered (`f32` RGBA; 0 until [`LayerCache::begin_frame`]).
    frame_bytes: usize,
    /// Frames begun so far ([`LayerCache::begin_frame`]).
    frames: u64,
    /// The frames being rendered (their numbers, see [`FrameMark`]), each with the clock when
    /// the frame before it began: entries used since then are the working set of playback
    /// (see [`evict_one`]).
    active: std::collections::BTreeMap<u64, u64>,
    /// The clock when the latest frame began.
    last_begin: u64,
    /// Eviction order (see [`evict_one`]): `(last_use, key)` of every CPU entry, of the CPU
    /// entries on probation, and of every GPU slot. `last_use` stamps are unique (the clock
    /// advances on every touch), so these mirror the maps exactly; a stale pair is dropped
    /// where it is met.
    cpu_lru: BTreeSet<(u64, u64)>,
    probation_lru: BTreeSet<(u64, u64)>,
    gpu_lru: BTreeSet<(u64, usize)>,
    /// `(last_use, id)` of the GPU slots on probation (published, not used since; see
    /// [`evict_one`]).
    gpu_probation_lru: BTreeSet<(u64, usize)>,
    /// GPU slots by content key ([`GpuOwner::resident`] lookups without a scan).
    gpu_content: HashMap<u64, Vec<usize>>,
}

impl Inner {
    fn index_cpu(&mut self, key: u64, last_use: u64, probation: bool) {
        self.cpu_lru.insert((last_use, key));
        if probation {
            self.probation_lru.insert((last_use, key));
        }
    }

    fn unindex_cpu(&mut self, key: u64, last_use: u64) {
        self.cpu_lru.remove(&(last_use, key));
        self.probation_lru.remove(&(last_use, key));
    }

    /// Keep GPU slot `id` (replacing one with the same id, which is returned).
    fn gpu_insert(&mut self, id: usize, entry: gpu::GpuEntry) -> Option<gpu::GpuEntry> {
        let old = self.gpu_remove(id);
        self.gpu_lru.insert((entry.last_use, id));
        if entry.probation {
            self.gpu_probation_lru.insert((entry.last_use, id));
        }
        if let Some(content) = entry.slot.content() {
            self.gpu_content.entry(content).or_default().push(id);
        }
        self.gpu.insert(id, entry);
        old
    }

    fn gpu_remove(&mut self, id: usize) -> Option<gpu::GpuEntry> {
        let e = self.gpu.remove(&id)?;
        self.gpu_lru.remove(&(e.last_use, id));
        self.gpu_probation_lru.remove(&(e.last_use, id));
        if let Some(content) = e.slot.content()
            && let Some(ids) = self.gpu_content.get_mut(&content)
        {
            ids.retain(|i| *i != id);
            if ids.is_empty() {
                self.gpu_content.remove(&content);
            }
        }
        Some(e)
    }

    fn gpu_touch(&mut self, id: usize, now: u64) {
        let Some(e) = self.gpu.get_mut(&id) else { return };
        let old = std::mem::replace(&mut e.last_use, now);
        // Used again: admitted.
        e.probation = false;
        self.gpu_lru.remove(&(old, id));
        self.gpu_probation_lru.remove(&(old, id));
        self.gpu_lru.insert((now, id));
    }

    fn gpu_take_all(&mut self) -> HashMap<usize, gpu::GpuEntry> {
        self.gpu_lru.clear();
        self.gpu_probation_lru.clear();
        self.gpu_content.clear();
        std::mem::take(&mut self.gpu)
    }

    /// Entries used after this clock belong to the frames being rendered or the frame before
    /// them (see [`evict_one`]).
    fn working_since(&self) -> Option<u64> {
        self.active.values().next().copied()
    }

    /// The least recently used CPU entry, dropping stale order pairs on the way.
    fn oldest_cpu(&mut self, probation: bool) -> Option<(u64, u64)> {
        loop {
            let order = if probation { &self.probation_lru } else { &self.cpu_lru };
            let (age, key) = *order.first()?;
            if self.map.get(&key).is_some_and(|e| e.last_use == age && (!probation || e.probation)) {
                return Some((age, key));
            }
            if probation {
                self.probation_lru.remove(&(age, key));
            } else {
                self.cpu_lru.remove(&(age, key));
            }
        }
    }

    /// The least recently used GPU slot, dropping stale order pairs on the way.
    fn oldest_gpu(&mut self) -> Option<(u64, usize)> {
        loop {
            let (age, id) = *self.gpu_lru.first()?;
            if self.gpu.get(&id).is_some_and(|e| e.last_use == age) {
                return Some((age, id));
            }
            self.gpu_lru.remove(&(age, id));
        }
    }

    /// The least recently published GPU slot still on probation, dropping stale pairs.
    fn oldest_gpu_probation(&mut self) -> Option<(u64, usize)> {
        loop {
            let (age, id) = *self.gpu_probation_lru.first()?;
            if self.gpu.get(&id).is_some_and(|e| e.last_use == age && e.probation) {
                return Some((age, id));
            }
            self.gpu_probation_lru.remove(&(age, id));
        }
    }
}

/// Keys per doorkeeper generation: two generations are kept, so the last 16–32k rendered keys
/// are remembered (about a megabyte).
const DOOR_GEN: usize = 16 * 1024;

/// A bounded map of recently rendered keys to the frame that last rendered them (see
/// [`LayerCache::begin_frame`]): two generations, the older dropped when the newer fills (a cheap
/// approximation of least recently seen).
#[derive(Default)]
struct Doorkeeper {
    cur: HashMap<u64, u64>,
    old: HashMap<u64, u64>,
}

impl Doorkeeper {
    /// Note a render of `key` in `frame`; the frame that rendered it before, if one did.
    fn saw(&mut self, key: u64, frame: u64) -> Option<u64> {
        if let Some(f) = self.cur.get_mut(&key) {
            return Some(std::mem::replace(f, frame));
        }
        let before = self.old.get(&key).copied();
        if self.cur.len() >= DOOR_GEN {
            self.old = std::mem::take(&mut self.cur);
        }
        self.cur.insert(key, frame);
        before
    }

    fn clear(&mut self) {
        self.cur.clear();
        self.old.clear();
    }
}

/// Hit/miss counters and logical indexed bytes (aliases counted per key).
/// See LayerCache::retained_stats for unique allocated CPU/GPU bytes, including caller pins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
    /// GPU slots the cache owns (CPU mirrors, Processed and Blend textures).
    pub gpu_entries: usize,
}

impl Default for LayerCache {
    fn default() -> Self {
        LayerCache::new(1 << 30)
    }
}

impl LayerCache {
    /// A cache retaining at most `budget` CPU and GPU pixel bytes. Existing callers may
    /// pin retired storage above a subsequently reduced budget; new admission stops.
    pub fn new(budget: usize) -> LayerCache {
        LayerCache {
            storage_epoch: Arc::new(Mutex::new(Arc::new(()))),
            resident_lifetime: Mutex::new(Arc::new(())),
            inner: Arc::new(Mutex::new(Inner {
                gpu: HashMap::new(),
                physical: crate::retained::RetainedBudget::new(budget, crate::retained::MAX_RECORDS),
                map: HashMap::new(),
                bytes: 0,
                budget,
                clock: 0,
                hits: 0,
                misses: 0,
                inserts: 0,
                missed_at: HashMap::new(),
                probation_bytes: 0,
                seen: Doorkeeper::default(),
                frame_bytes: 0,
                frames: 0,
                active: std::collections::BTreeMap::new(),
                last_begin: 0,
                cpu_lru: BTreeSet::new(),
                probation_lru: BTreeSet::new(),
                gpu_probation_lru: BTreeSet::new(),
                gpu_lru: BTreeSet::new(),
                gpu_content: HashMap::new(),
            })),
            disk: Mutex::new(None),
            store: Mutex::new(None),
            gate: Mutex::new(None),
            inflight: Arc::default(),
            streaks: Mutex::new(HashMap::new()),
            shape_rects: Mutex::new(HashMap::new()),
            shape_keys: Mutex::new(HashMap::new()),
        }
    }

    /// Drop inserts while `gate` is set. A render whose GPU readbacks are still in flight (the
    /// browser's deferred readbacks, see [`crate::Accelerator::pass_missed`]) computes some
    /// buffers from placeholders; the accelerator raises the gate from its first missing
    /// readback to the end of the pass, so none of them is kept (in memory or on disk).
    pub fn set_gate(&self, gate: Option<Arc<std::sync::atomic::AtomicBool>>) {
        let mut storage_epoch = self.storage_epoch.lock().unwrap_or_else(PoisonError::into_inner);
        let mut g = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let same = match (&*g, &gate) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            *storage_epoch = Arc::new(());
            *self.resident_lifetime.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(());
        }
        *g = gate;
    }

    fn gated(&self) -> bool {
        self.gate.lock().ok().and_then(|g| g.as_ref().map(|g| g.load(std::sync::atomic::Ordering::Relaxed))).unwrap_or(false)
    }

    /// Attach (or detach) the disk cache. `salt` separates projects and footage versions (see
    /// `footage_salt`); keys on disk are `(salt, key)`.
    pub fn set_disk(&self, disk: Option<(Arc<crate::disk_cache::DiskCache>, u64)>) {
        let old = {
            let Ok(mut epoch) = self.storage_epoch.lock() else { return };
            let Ok(mut current) = self.disk.lock() else { return };
            let same = match (&*current, &disk) {
                (None, None) => true,
                (Some((a, x)), Some((b, y))) => x == y && Arc::ptr_eq(a, b),
                _ => false,
            };
            if same {
                return;
            }
            *epoch = Arc::new(());
            std::mem::replace(&mut *current, disk)
        };
        drop(old);
    }

    /// Change the disk salt (after footage changed) keeping the attached disk cache.
    pub fn set_disk_salt(&self, salt: u64) {
        let Ok(mut epoch) = self.storage_epoch.lock() else { return };
        if let Ok(mut d) = self.disk.lock()
            && let Some((_, s)) = d.as_mut()
            && *s != salt
        {
            *epoch = Arc::new(());
            *s = salt;
        }
    }

    pub fn disk(&self) -> Option<Arc<crate::disk_cache::DiskCache>> {
        self.disk.lock().ok()?.as_ref().map(|d| d.0.clone())
    }

    /// Attach (or detach) a [`LayerStore`] used when no disk cache is attached (the browser's
    /// Origin Private File System through a [`PrefetchStore`]); `salt` as in
    /// [`LayerCache::set_disk`].
    pub fn set_store(&self, store: Option<(Arc<dyn LayerStore>, u64)>) {
        let old = {
            let Ok(mut epoch) = self.storage_epoch.lock() else { return };
            let Ok(mut current) = self.store.lock() else { return };
            let same = match (&*current, &store) {
                (None, None) => true,
                (Some((a, x)), Some((b, y))) => x == y && Arc::ptr_eq(a, b),
                _ => false,
            };
            if same {
                return;
            }
            *epoch = Arc::new(());
            std::mem::replace(&mut *current, store)
        };
        drop(old);
    }

    /// Change the store's salt (a new project or footage), keeping the store.
    pub fn set_store_salt(&self, salt: u64) {
        let Ok(mut epoch) = self.storage_epoch.lock() else { return };
        if let Ok(mut d) = self.store.lock()
            && let Some((_, s)) = d.as_mut()
            && *s != salt
        {
            *epoch = Arc::new(());
            *s = salt;
        }
    }

    /// Capture at producer start. Legacy/browser stores decline lazy writes until
    /// their transport can enforce cancellation at final publication.
    pub fn disk_intent(&self, key: u64) -> Option<crate::disk_cache::LayerWriteIntent> {
        let epoch = self.storage_epoch.lock().ok()?;
        let (disk, salt) = self.disk.lock().ok()?.as_ref()?.clone();
        let binding = StorageToken {
            gate: self.gate.lock().ok()?.as_ref().map(Arc::downgrade),
            state: Arc::downgrade(&self.storage_epoch),
            epoch: Arc::downgrade(&epoch),
        };
        disk.layer_intent(((salt as u128) << 64) | key as u128, binding)
    }

    fn disk_key(&self, key: u64) -> Option<(Arc<dyn LayerStore>, u128)> {
        let salted = |salt: u64| ((salt as u128) << 64) | key as u128;
        if let Some((dc, salt)) = self.disk.lock().ok()?.as_ref() {
            return Some((dc.clone() as Arc<dyn LayerStore>, salted(*salt)));
        }
        let s = self.store.lock().ok()?;
        let (st, salt) = s.as_ref()?;
        Some((st.clone(), salted(*salt)))
    }

    pub fn get(&self, key: u64) -> Option<Arc<Buf>> {
        if let Some(b) = self.peek(key) {
            return Some(b);
        }
        {
            let mut g = self.inner.lock().ok()?;
            g.misses += 1;
            if g.missed_at.len() > 4096 {
                g.missed_at.clear();
            }
            g.missed_at.insert(key, web_time::Instant::now());
        }
        if let Some(buf) = self.materialize_resident(key) {
            return Some(buf);
        }
        // Disk cache: a hit comes back into memory.
        let token = self.resident_token();
        let (dc, dk) = self.disk_key(key)?;
        let buf = dc.get_layer(dk)?;
        // (Stored on disk: it was worth keeping.)
        if let Some(token) = token {
            self.insert_mem(key, buf.clone(), true, &token);
        }
        Some(buf)
    }

    /// A derived CPU representation produced from captured content, never a fresh
    /// token/salt at completion. Disk work remains outside the cache owner lock.
    pub(crate) fn insert_captured(&self, key: u64, buf: &Arc<Buf>, token: &ResidentToken, intent: Option<crate::disk_cache::LayerWriteIntent>) {
        if !token.valid() {
            return;
        }
        self.insert_mem(key, buf.clone(), true, token);
        if token.valid()
            && let Some(intent) = intent
        {
            intent.produced().put(buf);
        }
    }

    /// CPU demand for Processed pixels. Readback is single-flight and never runs under a lock.
    /// Native disk writes use the producer's captured binding and purge identity;
    /// legacy stores without guarded publication remain memory-only.
    fn materialize_resident(&self, key: u64) -> Option<Arc<Buf>> {
        self.materialize_resident_inner(key, true)
    }
    fn materialize_resident_inner(&self, key: u64, wait: bool) -> Option<Arc<Buf>> {
        let owner = self.gpu_owner()?;
        let slot = owner.resident(key, GpuStage::Processed)?;
        if let Some(buf) = slot.ready() {
            return Some(buf);
        }
        // Do not recursively materialize after a failed competing flight. The
        // outer CPU claim performs at most one demand attempt.
        let _flight = match self.claim_inner(key, false, wait) {
            Claim::Ready(buf) => return Some(buf),
            Claim::Owner(guard) => guard,
            Claim::Render => return self.peek(key).or_else(|| slot.ready()),
        };
        if let Some(buf) = self.peek(key) {
            return Some(buf);
        }
        if !owner.valid() || !slot.allocation().generation().valid() {
            return None;
        }
        let buf = Arc::new(slot.materialize()?);
        // Admission uses the producer's captured lifetime. A late result cannot resurrect
        // a cleared cache. The caller may still use successfully read transient pixels.
        if owner.valid() && slot.allocation().generation().valid() {
            self.insert_mem(key, buf.clone(), true, owner.token());
            slot.handoff(&buf);
            slot.persist(&buf);
        }
        Some(buf)
    }

    /// [`Self::get`] for a buffer that is only worth having from memory: no disk lookup, and a
    /// miss is not counted (the caller falls back to a path that looks it up properly).
    pub(crate) fn peek(&self, key: u64) -> Option<Arc<Buf>> {
        let mut g = self.inner.lock().ok()?;
        g.clock += 1;
        let now = g.clock;
        let e = g.map.get_mut(&key)?;
        let old = std::mem::replace(&mut e.last_use, now);
        let stored = match &e.buf {
            Stored::Full(b) => Ok(b.clone()),
            Stored::Packed(p) => Err(p.clone()),
        };
        // Used again: admitted.
        let admitted = std::mem::take(&mut e.probation).then_some(e.bytes);
        if let Some(b) = admitted {
            g.probation_bytes = g.probation_bytes.saturating_sub(b);
        }
        g.unindex_cpu(key, old);
        g.index_cpu(key, now, false);
        g.hits += 1;
        drop(g);
        Some(match stored {
            Ok(b) => b,
            Err(p) => self.unpacked(key, &p),
        })
    }

    /// Whether `key` is in memory, without counting a use (unlike [`Self::peek`]): the
    /// pre-pass asks before producing what the walk will look up anyway.
    pub(crate) fn holds(&self, key: u64) -> bool {
        self.inner.lock().is_ok_and(|g| g.map.contains_key(&key))
    }

    pub fn insert(&self, key: u64, buf: Arc<Buf>) {
        self.insert_with(key, buf, false);
    }

    /// Admit animated GPU content only after this exact key was produced before.
    /// Successful productions share the bounded CPU doorkeeper. Occurrences, not
    /// frame numbers, also cover direct native display calls and same-frame reuse.
    pub fn admit_repeated_gpu_content(&self, key: u64) -> bool {
        let Ok(mut g) = self.inner.lock() else { return false };
        let frame = g.frames;
        g.seen.saw(key, frame).is_some()
    }

    /// [`Self::insert`] for a buffer whose key changes every frame (an animated precomp): kept
    /// only when it is rendered again after the frame that first rendered it (see the module
    /// docs).
    pub(crate) fn insert_once(&self, key: u64, buf: Arc<Buf>) {
        self.insert_with(key, buf, true);
    }

    fn insert_with(&self, key: u64, buf: Arc<Buf>, once: bool) {
        let Some(token) = self.resident_token() else { return };
        let (started, again) = match self.inner.lock() {
            Ok(mut g) => {
                let g = &mut *g;
                let seen = g.seen.saw(key, g.frames);
                // (A buffer of one frame is seen again later when every frame rendering then
                // has finished.)
                let again = if once { seen.is_some_and(|f| g.active.keys().next().is_none_or(|oldest| *oldest > f)) } else { seen.is_some() };
                (g.missed_at.remove(&key), again)
            }
            Err(_) => return,
        };
        if let Some((dc, dk)) = self.disk_key(key) {
            let slow = started.is_some_and(|t| t.elapsed().as_secs_f64() * 1e3 >= dc.min_layer_ms() as f64);
            if slow {
                dc.put_layer(dk, &buf);
            }
        }
        if again || !once {
            self.insert_mem(key, buf, again, &token);
        }
    }

    /// Keep `buf` under `key`: in the cache proper when `admit`, else on probation (see the
    /// module docs).
    fn insert_mem(&self, key: u64, buf: Arc<Buf>, admit: bool, token: &ResidentToken) {
        let Some(bytes) = buf.img.data.capacity().checked_mul(std::mem::size_of::<effectcraft_raster::Px>()) else { return };
        let extra = {
            let Ok(g) = self.inner.lock() else { return };
            if !token.valid() || bytes > g.budget / 4 {
                return;
            }
            if g.physical.tracks_cpu(&buf) { 0 } else { bytes }
        };
        self.relieve_for(extra, token);
        let needs_slot = self.inner.lock().map(|g| !g.map.contains_key(&key)).unwrap_or(false);
        gpu::relieve(&self.inner, Some(token), extra, None, needs_slot);
        let Ok(mut g) = self.inner.lock() else { return };
        if !token.valid() || bytes > g.budget / 4 {
            return;
        }
        if !g.map.contains_key(&key) && g.map.len().saturating_add(g.gpu.len()) >= crate::retained::MAX_RECORDS {
            return;
        }
        if g.map.try_reserve(1).is_err() || g.physical.track_cpu(&buf).is_err() {
            return;
        }
        g.clock = g.clock.saturating_add(1);
        g.inserts = g.inserts.saturating_add(1);
        let now = g.clock;
        let probation = !admit && g.map.get(&key).is_none_or(|e| e.probation);
        if let Some(old) = g.map.insert(key, Entry { buf: Stored::Full(buf), bytes, last_use: now, unpackable: false, probation }) {
            g.bytes = g.bytes.saturating_sub(old.bytes);
            if old.probation {
                g.probation_bytes = g.probation_bytes.saturating_sub(old.bytes);
            }
            g.unindex_cpu(key, old.last_use);
        }
        g.index_cpu(key, now, probation);
        g.bytes = g.bytes.saturating_add(bytes);
        if probation {
            g.probation_bytes = g.probation_bytes.saturating_add(bytes);
            trim_probation(&mut g, Some(key));
        }
    }

    /// Bytes of first renders kept on probation, at most (the newest is kept whatever its size,
    /// so a big precomp used twice in a row still hits): two frames' worth (see
    /// [`Self::begin_frame`]) but at least 16 MB, and at most 1/16 of the budget and 128 MB.
    pub fn window(&self) -> usize {
        self.inner.lock().map(|g| window_of(g.budget, g.frame_bytes)).unwrap_or(0)
    }

    /// The renderer starts a `width × height` top-level frame, which lasts while the returned
    /// mark lives. The probation window follows the latest frame's size (a smaller one trims
    /// what is on probation now), and [`Self::insert_once`] tells a later frame from this one.
    pub(crate) fn begin_frame(&self, width: u32, height: u32) -> FrameMark<'_> {
        let bytes = (width as usize).saturating_mul(height as usize).saturating_mul(std::mem::size_of::<effectcraft_raster::Px>());
        let Ok(mut g) = self.inner.lock() else { return FrameMark { cache: self, frame: None } };
        g.frames = g.frames.wrapping_add(1);
        let frame = g.frames;
        let now = g.clock;
        let since = std::mem::replace(&mut g.last_begin, now);
        g.active.insert(frame, since);
        if g.frame_bytes != bytes {
            g.frame_bytes = bytes;
            trim_probation(&mut g, None);
        }
        FrameMark { cache: self, frame: Some(frame) }
    }

    /// A packed entry's pixels as `f32`: unpacked (outside the lock) and kept unpacked, as it is
    /// in use again.
    fn unpacked(&self, key: u64, p: &Arc<PackedBuf>) -> Arc<Buf> {
        let token = self.resident_token();
        let buf = Arc::new(p.unpack());
        let Some(bytes) = buf.img.data.capacity().checked_mul(std::mem::size_of::<effectcraft_raster::Px>()) else { return buf };
        let Ok(mut g) = self.inner.lock() else { return buf };
        if !token.is_some_and(|t| t.valid()) {
            return buf;
        }
        let Some(e) = g.map.get(&key) else { return buf };
        match &e.buf {
            Stored::Full(b) => return b.clone(),
            Stored::Packed(q) if !Arc::ptr_eq(p, q) => return buf,
            Stored::Packed(_) => {}
        }
        // Keep the packed source on denial; this caller still receives valid pixels.
        if g.physical.track_cpu(&buf).is_err() {
            return buf;
        }
        let Some(e) = g.map.get_mut(&key) else { return buf };
        let old = std::mem::replace(&mut e.bytes, bytes);
        e.buf = Stored::Full(buf.clone());
        g.bytes = g.bytes.saturating_sub(old).saturating_add(bytes);
        buf
    }

    /// Make room for an exact incoming allocation. Packing runs outside the cache
    /// lock and requires headroom for its pending output; otherwise evict or decline.
    /// The cache owns recency; its ledger only tracks weak physical allocations.
    fn relieve_for(&self, extra: usize, token: &ResidentToken) {
        loop {
            let picked = {
                let Ok(mut g) = self.inner.lock() else { return };
                if !token.valid() {
                    return;
                }
                let stats = g.physical.stats();
                if stats.live_bytes.saturating_add(stats.pending_bytes).saturating_add(extra) <= g.budget {
                    return;
                }
                let candidate =
                    g.cpu_lru.iter().map(|(_, k)| *k).find(|k| g.map.get(k).is_some_and(|e| matches!(e.buf, Stored::Full(_)) && !e.unpackable && !e.probation));
                let Some(k) = candidate else { break };
                let Some(e) = g.map.get(&k) else { break };
                let Stored::Full(b) = &e.buf else { break };
                let Some(max_bytes) = b.img.data.len().checked_mul(8) else { break };
                let Ok(reservation) = g.physical.reserve(max_bytes) else { break };
                let b = b.clone();
                if let Some(e) = g.map.get_mut(&k) {
                    e.unpackable = true;
                }
                (k, b, reservation)
            };
            let (key, full, reservation) = picked;
            let packed = PackedBuf::pack(&full).map(Arc::new);
            if let Some(packed) = packed {
                self.publish_packed(key, &full, packed, reservation, token);
            }
            // `full` drops before the next sweep, releasing its charge unless a caller pins it.
        }
        self.evict_for(extra, token);
    }

    fn evict_for(&self, extra: usize, token: &ResidentToken) {
        gpu::relieve(&self.inner, Some(token), extra, None, false);
    }

    fn publish_packed(&self, key: u64, full: &Arc<Buf>, packed: Arc<PackedBuf>, reservation: crate::retained::Reservation, token: &ResidentToken) {
        let Ok(mut g) = self.inner.lock() else { return };
        if !token.valid() {
            return;
        }
        if !g.map.get(&key).is_some_and(|e| matches!(&e.buf, Stored::Full(b) if Arc::ptr_eq(b, full))) {
            return;
        }
        let Some(bytes) = packed.capacity_bytes() else { return };
        if reservation.commit_packed(&packed).is_err() {
            return;
        }
        let Some(e) = g.map.get_mut(&key) else { return };
        let old = std::mem::replace(&mut e.bytes, bytes);
        e.buf = Stored::Packed(packed);
        e.unpackable = false;
        g.bytes = g.bytes.saturating_sub(old).saturating_add(bytes);
    }

    /// Physical retained CPU/GPU accounting, including caller pins and pending reservations.
    /// Texture-pool scratch and the disk writer queue retain their separate bounded allowances.
    pub fn retained_stats(&self) -> crate::retained::RetainedStats {
        self.inner.lock().map(|g| g.physical.stats()).unwrap_or_default()
    }

    /// Capture a cache lifetime for bounded accelerator content reuse.
    pub fn resident_token(&self) -> Option<ResidentToken> {
        if self.gated() {
            return None;
        }
        let lifetime = Arc::downgrade(&*self.resident_lifetime.lock().ok()?);
        let gate = self.gate.lock().ok()?.as_ref().map(Arc::downgrade);
        Some(ResidentToken { lifetime, gate })
    }

    /// The memory budget in bytes.
    pub fn budget(&self) -> usize {
        self.inner.lock().map(|g| g.budget).unwrap_or(0)
    }

    /// Change the memory budget (evicts least-recently used entries when shrinking).
    pub fn set_budget(&self, budget: usize) {
        {
            let Ok(mut g) = self.inner.lock() else { return };
            g.budget = budget;
            g.physical.set_budget(budget);
        }
        gpu::relieve(&self.inner, None, 0, None, false);
    }

    pub fn clear(&self) {
        let mut epoch = self.storage_epoch.lock().unwrap_or_else(PoisonError::into_inner);
        *epoch = Arc::new(());
        *self.resident_lifetime.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(());
        let retired = if let Ok(mut g) = self.inner.lock() {
            let retired = g.gpu_take_all();
            g.map.clear();
            g.cpu_lru.clear();
            g.probation_lru.clear();
            g.physical.retire();
            g.bytes = 0;
            g.probation_bytes = 0;
            g.seen.clear();
            Some(retired)
        } else {
            None
        };
        drop(epoch);
        drop(retired);
    }

    pub fn gpu_owner(&self) -> Option<GpuOwner> {
        self.resident_token().map(|token| GpuOwner::new(Arc::downgrade(&self.inner), token))
    }

    pub fn stats(&self) -> CacheStats {
        self.inner
            .lock()
            .map(|g| CacheStats { entries: g.map.len(), bytes: g.bytes, hits: g.hits, misses: g.misses, gpu_entries: g.gpu.len() })
            .unwrap_or_default()
    }

    /// Buffers inserted so far (each a render, or a disk read): tests count renders with it.
    #[cfg(test)]
    pub(crate) fn inserts(&self) -> u64 {
        self.inner.lock().map(|g| g.inserts).unwrap_or(0)
    }

    /// (Entries admitted, entries on probation, bytes on probation): tests check admission.
    #[cfg(test)]
    pub(crate) fn admission(&self) -> (usize, usize, usize) {
        self.inner
            .lock()
            .map(|g| {
                let probation = g.map.values().filter(|e| e.probation).count();
                (g.map.len() - probation, probation, g.probation_bytes)
            })
            .unwrap_or_default()
    }

    /// (GPU slots admitted, GPU slots on probation): tests check GPU admission.
    #[cfg(test)]
    pub(crate) fn gpu_admission(&self) -> (usize, usize) {
        self.inner
            .lock()
            .map(|g| {
                let probation = g.gpu.values().filter(|e| e.probation).count();
                (g.gpu.len() - probation, probation)
            })
            .unwrap_or_default()
    }

    /// After a miss on `key`: render it ([`Claim::Owner`], whose drop wakes the waiters), take
    /// the buffer another thread just rendered under it ([`Claim::Ready`]), or render it anyway
    /// ([`Claim::Render`]: this thread is already rendering it further up its stack, or the
    /// other render didn't leave a buffer in time).
    ///
    /// A waiter on a rayon worker waits at most [`WAIT_POOL`]: rayon may run a frame that waits
    /// here on top of a stolen piece of the very render it waits for (a join inside it), which
    /// would never finish; the timeout turns that into a duplicate render, as before.
    pub fn claim(&self, key: u64) -> Claim {
        self.claim_inner(key, true, true)
    }
    /// A GPU consumer retries its own exact-device resident lookup after a wake.
    pub fn claim_gpu(&self, key: u64) -> Claim {
        self.claim_inner(key, false, true)
    }
    fn claim_inner(&self, key: u64, cpu_demand: bool, wait: bool) -> Claim {
        let me = std::thread::current();
        let pool = rayon::current_thread_index().is_some();
        let flight = {
            let mut m = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            match m.map.get(&key) {
                Some(f) if f.owner.id() == me.id() || !wait => return Claim::Render,
                // Its owner is stuck (a wait for one of its renders ran out): render it here
                // rather than wait for each of its renders in turn.
                Some(f) if pool && m.stalled.contains_key(&f.owner.id()) => return Claim::Render,
                Some(f) => f.clone(),
                None => {
                    if m.map.len() >= 4096 || m.map.try_reserve(1).is_err() {
                        return Claim::Render;
                    }
                    let f = Arc::new(Flight { owner: me, done: Mutex::new(false), cv: Condvar::new() });
                    m.map.insert(key, f.clone());
                    return Claim::Owner(FlightGuard { inflight: Arc::downgrade(&self.inflight), key, flight: f });
                }
            }
        };
        if !flight.wait() {
            if pool {
                let mut m = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
                // Until this render of the owner's ends.
                if m.map.get(&key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
                    m.stalled.entry(flight.owner.id()).or_insert_with(|| flight.clone());
                }
            }
            return Claim::Render;
        }
        match self.peek(key).or_else(|| cpu_demand.then(|| self.materialize_resident_inner(key, false)).flatten()) {
            Some(b) => Claim::Ready(b),
            None => Claim::Render,
        }
    }

    /// Whether to key precomp layer `id` (a hash of its comp and layer) this time. A precomp
    /// whose key missed on several frames in a row is animated: keying it evaluates its nested
    /// comp's animated properties (expressions included) once more than rendering it does, so
    /// it is keyed only every eighth frame until a key hits again.
    pub(crate) fn precomp_try(&self, id: u64) -> bool {
        let Ok(mut m) = self.streaks.lock() else { return true };
        let n = m.get(&id).copied().unwrap_or(0);
        let attempt = n < 2 || n % 8 == 0;
        if !attempt {
            if m.len() > 4096 {
                m.clear();
            }
            m.insert(id, n.saturating_add(1));
        }
        attempt
    }

    /// The whole raster buffer of the shape layer whose content key is `key` (which fixes it),
    /// measured by `measure` once (a walk of the layer's shapes; see [`crate::clip`]).
    pub(crate) fn shape_rect(&self, key: u64, measure: impl FnOnce() -> Option<ShapeRect>) -> Option<ShapeRect> {
        if let Ok(m) = self.shape_rects.lock()
            && let Some(r) = m.get(&key)
        {
            return *r;
        }
        let r = measure();
        if let Ok(mut m) = self.shape_rects.lock() {
            if m.len() > 4096 {
                m.clear();
            }
            m.insert(key, r);
        }
        r
    }

    /// Whether layer `id`'s content key `key` (at time `t`) has held over the last two times it
    /// was asked at: static content (only its transform may change), which is better kept whole in
    /// the cache than clipped to where it shows (see [`crate::clip`]). Asking again at the same time
    /// (another instance of a precomp) counts once.
    pub(crate) fn content_held(&self, id: u64, key: u64, t: effectcraft_time::Tick) -> bool {
        let Ok(mut m) = self.shape_keys.lock() else { return false };
        if m.len() > 4096 && !m.contains_key(&id) {
            m.clear();
        }
        let e = m.entry(id).or_insert((key, t, 0));
        if e.0 != key {
            *e = (key, t, 0);
        } else if e.1 != t {
            *e = (key, t, e.2.saturating_add(1));
        }
        e.2 >= 2
    }

    /// The outcome of keying precomp layer `id` (see [`Self::precomp_try`]).
    pub(crate) fn precomp_note(&self, id: u64, hit: bool) {
        let Ok(mut m) = self.streaks.lock() else { return };
        if m.len() > 4096 {
            m.clear();
        }
        let n = m.entry(id).or_insert(0);
        *n = if hit { 0 } else { n.saturating_add(1) };
    }
}

/// A top-level frame being rendered ([`LayerCache::begin_frame`]): ends when dropped.
pub struct FrameMark<'c> {
    cache: &'c LayerCache,
    frame: Option<u64>,
}

impl Drop for FrameMark<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.frame {
            let mut g = self.cache.inner.lock().unwrap_or_else(PoisonError::into_inner);
            g.active.remove(&f);
        }
    }
}

/// How long a waiter on a rayon worker waits for another thread's render (see
/// [`LayerCache::claim`]). Short: when rayon has run the waiting frame on top of a stolen piece
/// of the render it waits for, the wait can never succeed, and the thread does nothing else
/// meanwhile. At 2 s, several of these in a row stopped RAM preview for seconds at a time in
/// precomp-heavy comps; a duplicate render costs less.
const WAIT_POOL: std::time::Duration = std::time::Duration::from_millis(250);
/// Waits that ran out of time (process-wide; see [`wait_timeouts`]).
static WAIT_TIMEOUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many waits for another thread's layer render have timed out (each one rendered the
/// layer again instead): a diagnostic for preview stalls.
pub fn wait_timeouts() -> u64 {
    WAIT_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed)
}
/// How long any other thread waits.
const WAIT_OTHER: std::time::Duration = std::time::Duration::from_secs(30);

/// A key being rendered.
struct Flight {
    owner: std::thread::Thread,
    done: Mutex<bool>,
    cv: Condvar,
}

/// The keys being rendered, and the owners that stopped making progress.
///
/// A rayon worker waiting for another thread's render can hold part of that render's parallel
/// work further down its own stack (stolen while it waited in a join of its own): the owner can't
/// finish until the waiter returns. Each such wait runs out after [`WAIT_POOL`], and the waiter
/// renders the key itself; but the stuck owner holds a flight for every layer above it in its
/// stack and for every layer it had started, and waiting [`WAIT_POOL`] for each of them in turn
/// froze playback for 10 to 20 seconds with the CPU idle (a 0.5 MB PNG whose decode was stolen
/// that way took 17 s on a 16-thread PC). So once a wait for an owner runs out, pool threads don't
/// wait for that owner again until the render they gave up on ends.
#[derive(Default)]
struct Flights {
    map: HashMap<u64, Arc<Flight>>,
    /// Owners a pool thread gave up waiting for, with the render it waited for.
    stalled: HashMap<std::thread::ThreadId, Arc<Flight>>,
}

impl Flight {
    /// Wait until the render is done (`true`) or the time is up (`false`).
    fn wait(&self) -> bool {
        // Single-threaded targets never get here (the owner is this thread, see `claim`).
        if cfg!(target_arch = "wasm32") {
            return false;
        }
        let limit = if rayon::current_thread_index().is_some() { WAIT_POOL } else { WAIT_OTHER };
        let start = web_time::Instant::now();
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        while !*done {
            let Some(left) = limit.checked_sub(start.elapsed()) else {
                let n = WAIT_TIMEOUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                log::debug!(
                    target: "effectcraft_stall",
                    "layer cache: waited {limit:?} for {}'s render, rendering it again ({n} so far)",
                    self.owner.name().unwrap_or("another thread")
                );
                return false;
            };
            done = match self.cv.wait_timeout(done, left) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        let took = start.elapsed();
        if took >= std::time::Duration::from_millis(150) {
            log::debug!(target: "effectcraft_stall", "layer cache: waited {:.0} ms for another thread's render", took.as_secs_f64() * 1000.0);
        }
        true
    }
}

/// The outcome of [`LayerCache::claim`].
pub enum Claim {
    /// Render the buffer and insert it; dropping the guard (after the insert, or on failure)
    /// wakes the waiters.
    Owner(#[allow(dead_code)] FlightGuard),
    /// Another thread rendered it.
    Ready(Arc<Buf>),
    /// Render it without waking anyone.
    Render,
}

/// Ends a [`LayerCache::claim`] when dropped (also when the render panics).
pub struct FlightGuard {
    inflight: Weak<Mutex<Flights>>,
    key: u64,
    flight: Arc<Flight>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        {
            if let Some(inflight) = self.inflight.upgrade() {
                let mut m = inflight.lock().unwrap_or_else(PoisonError::into_inner);
                if m.map.get(&self.key).is_some_and(|f| Arc::ptr_eq(f, &self.flight)) {
                    m.map.remove(&self.key);
                }
                // The render a waiter gave up on ended: its owner is moving again.
                let owner = self.flight.owner.id();
                if m.stalled.get(&owner).is_some_and(|f| Arc::ptr_eq(f, &self.flight)) {
                    m.stalled.remove(&owner);
                }
            }
        }
        *self.flight.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.flight.cv.notify_all();
    }
}

/// Evict one entry; a GPU payload is returned to the caller for disposal after unlocking.
/// First renders still on probation go first (oldest first): they were used once, and a
/// frame touching more keys than the cache holds must not push out the admitted entries that
/// the next frame reuses (a key rendered again is admitted by the doorkeeper). GPU slots not
/// used since they were published (a moving video's frame, a texture of a buffer rendered for
/// one frame) go next, once the frames being rendered and the one before them are done with
/// them: a run of such one-use full-frame results must not push out the stable layers the
/// next shot shows (the 4:29 cut, #A8). Then the least recently used entry, CPU or GPU.
/// O(log n): the order is kept, not searched.
///
/// `protect` (making room for a new entry): an admitted entry used by the frames being
/// rendered, or by the frame before them, is not evicted; the new entry is declined instead.
/// When a frame needs more than the budget, least recently used eviction is the worst policy
/// for playback: each frame evicts exactly what the next one needs first, so nothing ever
/// hits. Declining keeps a stable part of the working set, which hits on every frame. (Only
/// the current frame's would not do: its early misses would evict what the previous frame
/// used and this one needs later, alternating.) After a cut, the old content goes a frame
/// later.
fn evict_one(g: &mut Inner, protect: bool) -> (bool, Option<Arc<GpuSlot>>) {
    if let Some((_, key)) = g.oldest_cpu(true) {
        remove(g, key);
        return (true, None);
    }
    let since = if protect { g.working_since() } else { None };
    if let Some((age, id)) = g.oldest_gpu_probation()
        && since.is_none_or(|since| age <= since)
    {
        return (true, g.gpu_remove(id).map(|e| e.slot));
    }
    let cpu = g.oldest_cpu(false);
    let gpu = g.oldest_gpu();
    let (age, cpu_first) = match (cpu, gpu) {
        (Some((a, _)), Some((b, _))) => (a.min(b), a <= b),
        (Some((a, _)), None) => (a, true),
        (None, Some((b, _))) => (b, false),
        (None, None) => return (false, None),
    };
    if since.is_some_and(|since| age > since) {
        return (false, None);
    }
    match (cpu_first, cpu, gpu) {
        (true, Some((_, key)), _) => {
            remove(g, key);
            (true, None)
        }
        (false, _, Some((_, id))) => (true, g.gpu_remove(id).map(|e| e.slot)),
        _ => (false, None),
    }
}

fn remove(g: &mut Inner, key: u64) {
    if let Some(e) = g.map.remove(&key) {
        g.bytes = g.bytes.saturating_sub(e.bytes);
        if e.probation {
            g.probation_bytes = g.probation_bytes.saturating_sub(e.bytes);
        }
        g.unindex_cpu(key, e.last_use);
    }
}

/// Frames' worth of pixels the probation window holds (see [`LayerCache::window`]).
const WINDOW_FRAMES: usize = 2;
/// The probation window is at least this big (small frames whose layers reach past them).
const WINDOW_MIN: usize = 16 << 20;

/// See [`LayerCache::window`].
fn window_of(budget: usize, frame_bytes: usize) -> usize {
    let most = (budget / 16).min(128 << 20);
    if frame_bytes == 0 { most } else { most.min(frame_bytes.saturating_mul(WINDOW_FRAMES).max(WINDOW_MIN)) }
}

/// Drop the least recently used entries on probation (never `newest`) until they fit the window.
fn trim_probation(g: &mut Inner, newest: Option<u64>) {
    let window = window_of(g.budget, g.frame_bytes);
    if g.probation_bytes <= window {
        return;
    }
    // Oldest first, in the kept order (no scan of the whole map).
    let mut skipped = None;
    while g.probation_bytes > window {
        let Some((age, k)) = g.oldest_cpu(true) else { break };
        if Some(k) == newest {
            // Set aside while the older ones go, then put back.
            g.probation_lru.remove(&(age, k));
            skipped = Some((age, k));
            continue;
        }
        remove(g, k);
    }
    if let Some(pair) = skipped {
        g.probation_lru.insert(pair);
    }
}

/// Persistent storage of layer buffers behind a [`LayerCache`] (keys salted per project).
pub trait LayerStore: Send + Sync {
    /// A stored buffer (synchronous: mid-render).
    fn get_layer(&self, key: u128) -> Option<Arc<Buf>>;
    /// Keep a buffer that was slow to render.
    fn put_layer(&self, key: u128, buf: &Buf);
    /// Buffers faster to render than this (ms) are not stored.
    fn min_layer_ms(&self) -> u64 {
        0
    }
}

/// Layers buffers rendered slower than this go to the browser's disk cache (ms).
pub const PREFETCH_MIN_LAYER_MS: u64 = 20;

/// A [`LayerStore`] over storage that can't be read in the middle of a render (the browser's
/// Origin Private File System, read asynchronously). Lookups are served from buffers fetched
/// before the frame ([`PrefetchStore::provide`]); the keys a render looked up and didn't find
/// are recorded ([`PrefetchStore::take_misses`]) so the transport can fetch them before the next
/// frame; buffers to store queue up as sealed entries ([`PrefetchStore::take_writes`]) for the
/// transport to write after the frame. Fetched buffers are kept least recently used first, up
/// to a byte budget.
pub struct PrefetchStore {
    st: Mutex<Prefetched>,
    budget: usize,
    min_ms: u64,
}

#[derive(Default)]
struct Prefetched {
    ready: HashMap<u128, (Arc<Buf>, u64)>,
    bytes: usize,
    clock: u64,
    misses: Vec<u128>,
    hits: Vec<u128>,
    writes: Vec<(u128, Vec<u8>)>,
    /// Keys stored or queued (not written twice).
    known: std::collections::HashSet<u128>,
}

impl Default for PrefetchStore {
    fn default() -> Self {
        PrefetchStore::new(256 << 20)
    }
}

impl PrefetchStore {
    pub fn new(budget: usize) -> PrefetchStore {
        PrefetchStore { st: Mutex::new(Prefetched::default()), budget, min_ms: PREFETCH_MIN_LAYER_MS }
    }

    /// Store buffers that took at least `ms` to render (default [`PREFETCH_MIN_LAYER_MS`]).
    pub fn with_min_ms(mut self, ms: u64) -> PrefetchStore {
        self.min_ms = ms;
        self
    }

    /// Whether `key`'s buffer is here.
    pub fn has(&self, key: u128) -> bool {
        self.st.lock().is_ok_and(|s| s.ready.contains_key(&key))
    }

    /// A fetched entry (sealed bytes, [`crate::disk_cache::layer_entry`]) for `key`; `false`
    /// when damaged.
    pub fn provide(&self, key: u128, entry: &[u8]) -> bool {
        let Some(buf) = crate::disk_cache::read_layer_entry(entry) else { return false };
        let bytes = buf.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
        let Ok(mut s) = self.st.lock() else { return false };
        s.clock += 1;
        let now = s.clock;
        if let Some((old, _)) = s.ready.insert(key, (Arc::new(buf), now)) {
            s.bytes -= old.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
        }
        s.bytes += bytes;
        s.known.insert(key);
        while s.bytes > self.budget {
            let Some((&k, _)) = s.ready.iter().min_by_key(|(_, (_, t))| *t) else { break };
            if let Some((b, _)) = s.ready.remove(&k) {
                s.bytes -= b.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
            }
        }
        true
    }

    /// Keys looked up and not found since the last call (each once).
    pub fn take_misses(&self) -> Vec<u128> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.misses)).unwrap_or_default()
    }

    /// Keys served since the last call (each once).
    pub fn take_hits(&self) -> Vec<u128> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.hits)).unwrap_or_default()
    }

    /// Entries to write: (key, sealed bytes).
    pub fn take_writes(&self) -> Vec<(u128, Vec<u8>)> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.writes)).unwrap_or_default()
    }

    /// Forget everything (Empty Disk Cache, Purge).
    pub fn clear(&self) {
        if let Ok(mut s) = self.st.lock() {
            *s = Prefetched::default();
        }
    }
}

impl LayerStore for PrefetchStore {
    fn get_layer(&self, key: u128) -> Option<Arc<Buf>> {
        let mut s = self.st.lock().ok()?;
        s.clock += 1;
        let now = s.clock;
        match s.ready.get_mut(&key) {
            Some((b, t)) => {
                *t = now;
                let b = b.clone();
                if !s.hits.contains(&key) {
                    s.hits.push(key);
                }
                Some(b)
            }
            None => {
                if !s.misses.contains(&key) && s.misses.len() < 1024 {
                    s.misses.push(key);
                }
                None
            }
        }
    }

    fn put_layer(&self, key: u128, buf: &Buf) {
        let Ok(mut s) = self.st.lock() else { return };
        if s.known.insert(key) {
            s.writes.push((key, crate::disk_cache::layer_entry(buf)));
        }
    }

    fn min_layer_ms(&self) -> u64 {
        self.min_ms
    }
}

/// FNV-1a style 64-bit hasher fed by `Debug` output (exact float formatting) and raw values.
pub(crate) struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl std::fmt::Write for KeyHasher {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        Hasher::write(self, s.as_bytes());
        Ok(())
    }
}

pub(crate) fn hash_debug(h: &mut KeyHasher, v: &impl std::fmt::Debug) {
    use std::fmt::Write;
    let _ = write!(h, "{v:?}");
    Hasher::write_u8(h, 0xff);
}

/// Paths of an image sequence a key formats, at most (see [`FootageKey`]).
const SEQUENCE_SAMPLES: usize = 17;

/// Footage settings as a key formats them ([`hash_debug`]): every field, except that an image
/// sequence's file list counts by its length and a sample of its paths (the first, the last and
/// evenly spaced ones between) instead of every path: a 17k-frame sequence would print about a
/// megabyte per key, for every key, every frame. (The files' identity is the disk cache's
/// salt.) The fields are listed exhaustively, so a new one can't be left out of keys.
pub(crate) struct FootageKey<'a>(pub(crate) &'a effectcraft_project::Footage);

impl std::fmt::Debug for FootageKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let effectcraft_project::Footage {
            path,
            kind,
            width,
            height,
            pixel_aspect,
            frame_rate,
            native_rate,
            duration,
            has_video,
            has_audio,
            alpha,
            premul_color,
            loop_count,
            codec,
            missing,
            sequence,
            color_profile,
            layer,
            fields,
            invert_alpha,
            linear_light,
            preserve_rgb,
            data,
            page,
            alphabetical,
            start_timecode,
        } = self.0;
        f.debug_struct("Footage")
            .field("path", path)
            .field("kind", kind)
            .field("width", width)
            .field("height", height)
            .field("pixel_aspect", pixel_aspect)
            .field("frame_rate", frame_rate)
            .field("native_rate", native_rate)
            .field("duration", duration)
            .field("has_video", has_video)
            .field("has_audio", has_audio)
            .field("alpha", alpha)
            .field("premul_color", premul_color)
            .field("loop_count", loop_count)
            .field("codec", codec)
            .field("missing", missing)
            .field("sequence", &SequenceSample(sequence))
            .field("color_profile", color_profile)
            .field("layer", layer)
            .field("fields", fields)
            .field("invert_alpha", invert_alpha)
            .field("linear_light", linear_light)
            .field("preserve_rgb", preserve_rgb)
            .field("data", data)
            .field("page", page)
            .field("alphabetical", alphabetical)
            .field("start_timecode", start_timecode)
            .finish()
    }
}

/// An image sequence's length and sampled paths (see [`FootageKey`]).
struct SequenceSample<'a>(&'a [String]);

impl std::fmt::Debug for SequenceSample<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.0.len();
        write!(f, "{n}")?;
        let mut list = f.debug_list();
        if n <= SEQUENCE_SAMPLES {
            list.entries(self.0);
        } else {
            let last = n - 1;
            let gaps = (SEQUENCE_SAMPLES - 1) as u128;
            for i in 0..SEQUENCE_SAMPLES {
                let at = (last as u128).saturating_mul(i as u128) / gaps;
                if let Some(p) = usize::try_from(at).ok().and_then(|at| self.0.get(at)) {
                    list.entry(p);
                }
            }
        }
        list.finish()
    }
}

/// An item's proxy as a key formats it (its footage through [`FootageKey`]).
pub(crate) struct ProxyKey<'a>(pub(crate) &'a Option<Box<effectcraft_project::Proxy>>);

impl std::fmt::Debug for ProxyKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("None"),
            Some(p) => {
                let effectcraft_project::Proxy { footage, enabled } = &**p;
                f.debug_struct("Proxy").field("footage", &FootageKey(footage)).field("enabled", enabled).finish()
            }
        }
    }
}

/// An item's kind as a key formats it (footage through [`FootageKey`]).
pub(crate) struct KindKey<'a>(pub(crate) &'a ItemKind);

impl std::fmt::Debug for KindKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ItemKind::Footage(footage) => f.debug_tuple("Footage").field(&FootageKey(footage)).finish(),
            other => other.fmt(f),
        }
    }
}

/// A [`KeyHasher`] starting from `seed`.
pub(crate) fn hasher(seed: u64) -> KeyHasher {
    KeyHasher(seed)
}

/// Time-based layer content: does any part of the source/effects read the clock directly
/// (rather than through property values)?
/// `effects: false` ignores the effects (a key of the source and masks only).
fn time_dependent(layer: &Layer, effects: bool) -> bool {
    if effects
        && layer.switches.effects
        && let Some(fx) = layer.effects()
    {
        for g in fx.groups() {
            if let effectcraft_project::GroupKind::Effect { effect } = &g.kind
                && g.enabled
                && effectcraft_effects::is_time_dependent(effect)
            {
                return true;
            }
            // Clone strokes reading another layer or another time.
            if g.enabled && effectcraft_effects::paint::is_paint(g) && effectcraft_effects::paint::cache_key(g, 0.0).1 {
                return true;
            }
        }
    }
    if matches!(layer.source, LayerSource::Shape)
        && let Some(c) = layer.props.sub("contents")
    {
        return group_has(c, "wiggle");
    }
    // Wiggly selectors move with time; Expression selectors evaluate per character (their
    // property value alone doesn't capture the result).
    if matches!(layer.source, LayerSource::Text)
        && let Some(t) = layer.props.sub("text")
    {
        return group_has(t, "wigglySelector") || group_has(t, "expressionSelector");
    }
    false
}

fn group_has(g: &PropGroup, match_id: &str) -> bool {
    g.children.iter().any(|c| match c {
        Node::Group(sub) => sub.match_id == match_id || group_has(sub, match_id),
        Node::Prop(_) => false,
    })
}

/// Feed every property of `g` that can change over time (keyframes or expression) as its value
/// at the context time. The static structure is hashed separately.
pub(crate) fn hash_values(h: &mut KeyHasher, ctx: &EvalCtx, layer: &Layer, g: &PropGroup) {
    for c in &g.children {
        match c {
            Node::Prop(p) => {
                if !p.keys.is_empty() || p.has_expression() {
                    p.uid.hash(h);
                    hash_debug(h, &ctx.value(layer, p));
                }
            }
            Node::Group(sub) => hash_values(h, ctx, layer, sub),
        }
    }
}

/// Does any property of `g` have keyframes or an expression (other than a literal constant)?
fn has_animation(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => !p.keys.is_empty() || live_expression(p),
        Node::Group(sub) => has_animation(sub),
    })
}

/// An expression that can give different values: not a plain number or array of numbers
/// (`0.5`, `[100, 50]`), whose value the hashed structure (the expression text) fixes.
fn live_expression(p: &effectcraft_project::Property) -> bool {
    p.has_expression() && !p.expr.as_ref().is_some_and(|e| literal_constant(&e.text))
}

fn literal_constant(text: &str) -> bool {
    let s = text.trim();
    let s = s.strip_suffix(';').unwrap_or(s).trim();
    let number = |x: &str| {
        let x = x.trim();
        !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) && x.parse::<f64>().is_ok()
    };
    match s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(items) => items.split(',').all(number),
        None => number(s),
    }
}

// ------------------------------------------------------------------------------ key memo

/// What the keys need to know about a layer that changes only when the layer does: the hashed
/// structure of its property groups and what reads the clock. Memoised per shared layer
/// ([`layer_info`]), so a frame formats no unchanged layer's property tree.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LayerInfo {
    /// Layer id, source and pixel switches.
    head: u64,
    /// Static structure of the content (every top-level node but Transform and Layer Styles):
    /// values, keyframes, expressions, enabled flags, effect ids, modes.
    content: u64,
    /// The Layer Styles group's structure.
    styles: u64,
    /// Every field of the layer, every property included (a precomp's nested layers).
    pub(crate) whole: u64,
    /// [`Self::content`], [`Self::styles`] and [`Self::whole`] without keyframes: for keys
    /// that hash the animated values at the frame time instead (see [`free`]).
    content_free: u64,
    styles_free: u64,
    pub(crate) free: u64,
    /// The layer is drawn from its values at the frame time alone ([`free::pointwise`]).
    pub(crate) pointwise: bool,
    /// A content group has keyframes or expressions.
    content_animated: bool,
    styles_animated: bool,
    /// A property anywhere has keyframes or an expression.
    pub(crate) animated: bool,
    /// A property anywhere has an expression.
    pub(crate) expressions: bool,
    /// A group other than Transform has an expression.
    content_expressions: bool,
    pub(crate) time_dependent: bool,
    pub(crate) reads_other_times: bool,
    pub(crate) reads_comp_scene: bool,
    /// Every live expression reads only its property's value and the time
    /// ([`free::contained`]).
    pub(crate) contained: bool,
    /// [`Self::contained`] for what motion blur reads at other times ([`free::sampled_contained`]).
    pub(crate) contained_sampled: bool,
    /// [`Self::contained`] for what a layer's own other times read ([`free::inner_contained`]).
    pub(crate) contained_inner: bool,
    /// Enabled paint strokes (their Duration spans show and hide them).
    pub(crate) paint: bool,
    masks_or_effects: bool,
    /// An enabled effect has a layer parameter naming a layer (Displacement Map, Set Matte…).
    pub(crate) layer_refs: bool,
}

impl LayerInfo {
    fn of(layer: &Layer) -> LayerInfo {
        let mut head = KeyHasher(0x8422_2325_cbf2_9ce4);
        layer.id.hash(&mut head);
        hash_debug(&mut head, &layer.source);
        // (Audio, Lock and Shy don't change pixels.)
        hash_debug(&mut head, &layer.switches.pixels());
        let mut content = KeyHasher(0xcbf2_9ce4_8422_2325);
        let mut content_free = KeyHasher(0x2325_cbf2_9ce4_8422);
        let (mut content_animated, mut content_expressions) = (false, false);
        for c in &layer.props.children {
            if let Node::Group(g) = c {
                if g.match_id == "transform" {
                    continue;
                }
                content_expressions |= has_expression(g);
                if g.match_id == effectcraft_project::styles::GROUP {
                    continue;
                }
                content_animated |= has_animation(g);
            }
            hash_debug(&mut content, c);
            free::hash_node(&mut content_free, c);
        }
        let mut styles = KeyHasher(0x9e37_79b9_7f4a_7c15);
        let mut styles_free = KeyHasher(0x79b9_7f4a_7c15_9e37);
        let styles_animated = layer.layer_styles().is_some_and(|g| {
            hash_debug(&mut styles, g);
            free::hash_node(&mut styles_free, &Node::Group(g.clone()));
            has_animation(g)
        });
        let mut whole = KeyHasher(0x7c15_9e37_79b9_7f4a);
        hash_debug(&mut whole, layer);
        let mut free_whole = KeyHasher(0x9e37_7c15_7f4a_79b9);
        free::hash_layer(&mut free_whole, layer);
        let fx = layer.switches.effects.then(|| layer.effects()).flatten();
        let (time_dependent, reads_other_times) = (time_dependent(layer, true), reads_other_times(layer));
        let paint = fx.is_some_and(|fx| fx.groups().any(|g| g.enabled && effectcraft_effects::paint::is_paint(g)));
        LayerInfo {
            head: head.finish(),
            content: content.finish(),
            styles: styles.finish(),
            whole: whole.finish(),
            content_free: content_free.finish(),
            styles_free: styles_free.finish(),
            free: free_whole.finish(),
            pointwise: free::pointwise(layer, time_dependent, reads_other_times, paint),
            content_animated,
            styles_animated,
            animated: has_animation(&layer.props),
            expressions: has_expression(&layer.props),
            content_expressions,
            time_dependent,
            reads_other_times,
            reads_comp_scene: reads_comp_scene(layer),
            contained: free::contained(&layer.props),
            contained_sampled: free::sampled_contained(layer),
            contained_inner: free::inner_contained(layer),
            paint,
            masks_or_effects: layer.masks().is_some_and(|m| !m.children.is_empty()) || layer.effects().is_some_and(|fx| !fx.children.is_empty()),
            layer_refs: fx.is_some_and(|fx| fx.groups().any(|g| g.enabled && names_layers(g))),
        }
    }
}

/// Does a property of `g` name a layer (a layer parameter set to a layer, keyframed or driven by
/// an expression)?
fn names_layers(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => {
            matches!(p.value, effectcraft_keyframe::Value::Layer(_))
                && (matches!(p.value, effectcraft_keyframe::Value::Layer(Some(_))) || !p.keys.is_empty() || p.has_expression())
        }
        Node::Group(sub) => names_layers(sub),
    })
}

/// The layers `g`'s layer parameters name at the context time.
fn named_layers(ctx: &EvalCtx, layer: &Layer, g: &PropGroup, out: &mut Vec<u64>) {
    for c in &g.children {
        match c {
            Node::Prop(p) if matches!(p.value, effectcraft_keyframe::Value::Layer(_)) => {
                let v = if p.keys.is_empty() && !p.has_expression() { p.value.clone() } else { ctx.value(layer, p) };
                if let effectcraft_keyframe::Value::Layer(Some(id)) = v {
                    out.push(id);
                }
            }
            Node::Prop(_) => {}
            Node::Group(sub) => named_layers(ctx, layer, sub, out),
        }
    }
}

/// Fold in what `layer`'s effects read from other layers through layer parameters: each named
/// layer's structure, animated values and source at the context time, and the layers its own
/// effects name. `None` when one can't be keyed here (a precomp, an effect reading the comp
/// scene, too deep).
#[allow(clippy::too_many_arguments)]
fn hash_layer_refs(h: &mut KeyHasher, ctx: &EvalCtx, layer: &Layer, reads_other_times: bool, free: bool, depth: usize, limit: Option<usize>) -> Option<()> {
    if depth > 4 {
        return None;
    }
    let Some(fx) = layer.effects() else { return Some(()) };
    let mut ids = vec![];
    // A layer's input runs only its first `limit` effects: later ones' layers don't change it.
    for g in fx.groups().take(limit.unwrap_or(usize::MAX)).filter(|g| g.enabled) {
        named_layers(ctx, layer, g, &mut ids);
    }
    for id in ids {
        id.hash(h);
        let Some(o) = ctx.layer(effectcraft_project::LayerId(id)) else { continue };
        if o.id == layer.id {
            continue;
        }
        let oi = layer_info(ctx.comp, o);
        // Read at other times, an expression may read anything then. A scene reader's camera and
        // lights are keyed by `dependencies::hash` (#1252).
        if reads_other_times && oi.expressions {
            return None;
        }
        // Read at the frame time only: its values there, not its keyframes.
        let free = free && !reads_other_times && oi.pointwise && !(o.switches.motion_blur && ctx.comp.enable_motion_blur);
        free.hash(h);
        (if free { oi.free } else { oi.whole }).hash(h);
        if oi.animated {
            hash_values(h, ctx, o, &o.props);
        }
        match &o.source {
            LayerSource::Solid { item } => hash_debug(h, &ctx.project.item(*item).map(|i| &i.kind)),
            LayerSource::Footage { item } | LayerSource::Model { item } => {
                let it = ctx.project.item(*item)?;
                hash_debug(h, &KindKey(&it.kind));
                hash_debug(h, &ProxyKey(&it.proxy));
                ctx.source_time(o).0.hash(h);
            }
            // The nested comp's layers are hashed by `dependencies::hash` (in every key).
            LayerSource::Comp { .. } => {}
            _ => {}
        }
        if oi.time_dependent || oi.reads_other_times || oi.paint || reads_other_times {
            ctx.time.0.hash(h);
        }
        if o.switches.effects && oi.layer_refs {
            hash_layer_refs(h, ctx, o, reads_other_times || oi.reads_other_times, free, depth + 1, None)?;
        }
    }
    Some(())
}

/// Values memoised per shared allocation (`Arc<T>`), by address. Each entry keeps a `Weak` on
/// its allocation, so while it exists the address can't be reused by anything else, and the
/// value can't change in place (`Arc::get_mut` refuses and `Arc::make_mut` moves it to a new
/// allocation while a `Weak` exists). Entries whose value was dropped are pruned as the map
/// grows.
pub(crate) struct Memo<T, V> {
    map: HashMap<usize, (Weak<T>, V)>,
    cap: usize,
}

const MEMO_MIN: usize = 1024;

impl<T, V: Clone> Memo<T, V> {
    pub(crate) fn new() -> Memo<T, V> {
        Memo { map: HashMap::new(), cap: MEMO_MIN }
    }

    /// The value for the live allocation at `addr`.
    pub(crate) fn get(&self, addr: *const T) -> Option<V> {
        let (w, v) = self.map.get(&(addr as usize))?;
        (std::ptr::eq(w.as_ptr(), addr) && w.strong_count() > 0).then(|| v.clone())
    }

    pub(crate) fn put(&mut self, arc: &Arc<T>, v: V) {
        if self.map.len() >= self.cap {
            self.map.retain(|_, (w, _)| w.strong_count() > 0);
            self.cap = (self.map.len() * 2).max(MEMO_MIN);
        }
        self.map.insert(Arc::as_ptr(arc) as usize, (Arc::downgrade(arc), v));
    }
}

static LAYERS: LazyLock<RwLock<Memo<Layer, LayerInfo>>> = LazyLock::new(|| RwLock::new(Memo::new()));

/// `layer`'s [`LayerInfo`], memoised when `layer` is one of `comp`'s shared layers (a layer
/// from elsewhere is described afresh).
pub(crate) fn layer_info(comp: &Comp, layer: &Layer) -> LayerInfo {
    let addr: *const Layer = layer;
    if let Some(i) = LAYERS.read().ok().and_then(|m| m.get(addr)) {
        return i;
    }
    match comp.layers.arcs().iter().find(|a| std::ptr::eq(Arc::as_ptr(a), addr)) {
        Some(arc) => layer_info_arc(arc),
        None => LayerInfo::of(layer),
    }
}

/// Whether `layer`'s content (masks, effects, source settings) has keyframes or expressions or
/// reads the clock: its key changes over time.
pub(crate) fn content_animated(comp: &Comp, layer: &Layer) -> bool {
    let i = layer_info(comp, layer);
    i.content_animated || i.content_expressions || i.time_dependent || i.reads_other_times
}

/// [`layer_info`] of a shared layer.
pub(crate) fn layer_info_arc(arc: &Arc<Layer>) -> LayerInfo {
    if let Some(i) = LAYERS.read().ok().and_then(|m| m.get(Arc::as_ptr(arc))) {
        return i;
    }
    let info = LayerInfo::of(arc);
    if let Ok(mut m) = LAYERS.write() {
        m.put(arc, info);
    }
    info
}

// ------------------------------------------------------------------------------ keys

/// Cache key for the processed (source → masks → effects) buffer of `layer` at the context
/// time, or `None` when the layer must not be cached (precomps and moving footage without a
/// source key, see [`layer_key_src`]; cameras, adjustment layers…).
pub fn layer_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool) -> Option<u64> {
    layer_key_src(ctx, layer, scale, draft, blur, None)
}

/// [`layer_key`] with the renderer's key for the layer's source (`src`): a precomp's nested
/// content ([`crate::precomp_key`]), or for footage which proxy the render uses. Without it,
/// precomps and footage aren't cached.
pub(crate) fn layer_key_src(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, src: Option<u64>) -> Option<u64> {
    // Time effects see property values at other times. Keyframes are part of the hashed
    // structure (and the layer time is folded in), but expressions may read other layers at
    // those times, which the key cannot see: such layers are not cached.
    let info = layer_info(ctx.comp, layer);
    if info.reads_other_times && info.content_expressions {
        return None;
    }
    key_with(ctx, layer, scale, draft, blur, false, src)
}

/// Cache key for a layer's *input* at the context time: source → masks → its first `effects`
/// effects (see `Renderer::layer_input`). Unlike [`layer_key`], every footage frame is cached
/// here (keyed by item and source time), since Time effects read many neighbouring frames.
pub fn input_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, effects: usize) -> Option<u64> {
    input_key_src(ctx, layer, scale, draft, blur, effects, None)
}

/// [`input_key`] with the renderer's source key (see [`layer_key_src`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn input_key_src(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, effects: usize, src: Option<u64>) -> Option<u64> {
    if layer.switches.adjustment {
        return None;
    }
    let base = key_any(ctx, layer, scale, draft, blur, true, Some(effects), src)?;
    let mut h = KeyHasher(base ^ 0x5bd1_e995_7a3c_11d3);
    effects.hash(&mut h);
    Some(h.finish())
}

/// Does the layer run an effect that reads the layer at other times (Echo, Timewarp…)?
fn reads_other_times(layer: &Layer) -> bool {
    layer.switches.effects
        && layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| g.enabled && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effect.starts_with("ec.time.")))
        })
}

/// Does any property of `g` have an expression (other than a literal constant, which reads
/// nothing)?
fn has_expression(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => live_expression(p),
        Node::Group(sub) => has_expression(sub),
    })
}

/// Cache key for an adjustment layer's footprint (its source through its masks, see
/// `Renderer::adjustment_footprint`): the GPU compositor then uploads it once, not every frame.
pub fn footprint_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool) -> Option<u64> {
    Some(derive(key_any(ctx, layer, scale, draft, blur, false, Some(0), None)?, 0xf007_9417))
}

#[allow(clippy::too_many_arguments)]
fn key_with(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, footage: bool, src: Option<u64>) -> Option<u64> {
    if layer.switches.adjustment {
        return None;
    }
    key_any(ctx, layer, scale, draft, blur, footage, None, src)
}

/// `blur`: the layer is motion blurred in this render (`Renderer::mb_on`). `footage`: key every
/// footage frame (a layer's input); otherwise moving footage is kept only when masks or effects
/// process it, as each frame is used about once. `effects`: key only the first that many
/// effects (a layer's input, see [`input_key`]). `src`: the renderer's source key (see
/// [`layer_key_src`]).
#[allow(clippy::too_many_arguments)]
fn key_any(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, footage: bool, effects: Option<usize>, src: Option<u64>) -> Option<u64> {
    let info = layer_info(ctx.comp, layer);
    let mut h = KeyHasher(0xcbf2_9ce4_8422_2325);
    // Drawn from its values at the frame time alone: the structure leaves the keyframes out
    // (see [`free`]), so a keyframe edit keeps the key where it doesn't change a value.
    let free = info.pointwise && !blur;
    ctx.comp.frame_rate.as_f64().to_bits().hash(&mut h);
    ctx.comp.enable_frame_blending.hash(&mut h);
    dependencies::hash(&mut h, ctx, layer, effects, free)?;
    match &layer.source {
        LayerSource::Solid { item } => {
            let it = ctx.project.item(*item)?;
            let ItemKind::Solid(s) = &it.kind else { return None };
            hash_debug(&mut h, s);
        }
        LayerSource::Text | LayerSource::Shape => {}
        LayerSource::Footage { item } => {
            let it = ctx.project.item(*item)?;
            let ItemKind::Footage(f) = &it.kind else { return None };
            // A still (and a still proxy) shows the same picture at every time.
            let still = f.kind == FootageKind::Still && it.proxy.as_ref().is_none_or(|p| p.footage.kind == FootageKind::Still);
            if !footage && (src.is_none() || !(still || info.masks_or_effects)) {
                return None;
            }
            hash_debug(&mut h, &FootageKey(f));
            // A proxy (and its Use Proxy switch) changes the pixels.
            hash_debug(&mut h, &ProxyKey(&it.proxy));
            item.hash(&mut h);
            if !still {
                ctx.source_time(layer).0.hash(&mut h);
                // Frame blending mixes neighbouring source frames.
                ctx.comp.enable_frame_blending.hash(&mut h);
            }
        }
        // The nested comp's content, from the renderer.
        LayerSource::Comp { .. } if src.is_some() => {}
        _ => return None,
    }
    src.hash(&mut h);
    // Bit depth and colour management change the pixels.
    crate::color::Pipe::of(&ctx.project.settings).key().hash(&mut h);
    ctx.comp_id.hash(&mut h);
    ctx.comp.width.hash(&mut h);
    ctx.comp.height.hash(&mut h);
    scale.to_bits().hash(&mut h);
    // Effects sized in comp pixels follow the layer's transform (and a 3D layer's camera),
    // which the key otherwise leaves out.
    if crate::fx::sizes_in_comp_px(layer) {
        crate::fx::fx_comp_px(ctx, layer).map(|c| c.map(f64::to_bits)).hash(&mut h);
    }
    // Effects placed in comp space follow the whole transform, position included.
    if let Some(m) = crate::fx::comp_space_placement(ctx, layer) {
        m.0[..2].iter().flatten().map(|v| v.to_bits()).for_each(|b| b.hash(&mut h));
    }
    draft.hash(&mut h);
    info.head.hash(&mut h);
    // Mask motion blur and effects that read the shutter depend on whether the layer is motion
    // blurred in this render and on the comp's shutter.
    if info.masks_or_effects {
        blur.hash(&mut h);
        ctx.comp.enable_motion_blur.hash(&mut h);
        ctx.comp.shutter_angle.to_bits().hash(&mut h);
        ctx.comp.shutter_phase.to_bits().hash(&mut h);
        ctx.comp.motion_blur_samples.hash(&mut h);
    }
    match effects {
        // Static structure (memoised), then the values of what's animated.
        None => {
            free.hash(&mut h);
            (if free { info.content_free } else { info.content }).hash(&mut h);
            if info.content_animated {
                for c in &layer.props.children {
                    if let Node::Group(g) = c
                        && g.match_id != "transform"
                        && g.match_id != effectcraft_project::styles::GROUP
                    {
                        hash_values(&mut h, ctx, layer, g);
                    }
                }
            }
        }
        Some(n) => {
            for c in &layer.props.children {
                // Transform is applied later; Layer Styles are keyed separately (see [`styles_key`]).
                if let Node::Group(g) = c
                    && (g.match_id == "transform" || g.match_id == effectcraft_project::styles::GROUP)
                {
                    continue;
                }
                // A layer's input runs only its first effects: the later ones (a Puppet whose
                // pins are dragged, #212) don't change it, so it stays cached.
                if let Node::Group(g) = c
                    && g.match_id == "effects"
                {
                    let mut first = g.clone();
                    let mut seen = 0;
                    first.children.retain(|e| match e {
                        Node::Prop(_) => true,
                        Node::Group(_) => {
                            seen += 1;
                            seen <= n
                        }
                    });
                    hash_debug(&mut h, &first);
                    hash_values(&mut h, ctx, layer, &first);
                    continue;
                }
                hash_debug(&mut h, c);
                if let Node::Group(g) = c {
                    hash_values(&mut h, ctx, layer, g);
                }
            }
        }
    }
    // Paint strokes appear and disappear with their Duration spans (not property values).
    if effects != Some(0)
        && info.paint
        && let Some(fx) = layer.effects()
    {
        let lt = layer.layer_time(ctx.time).seconds();
        for g in fx.groups().filter(|g| g.enabled && effectcraft_effects::paint::is_paint(g)) {
            effectcraft_effects::paint::cache_key(g, lt).0.hash(&mut h);
        }
    }
    if info.time_dependent && (effects != Some(0) || time_dependent(layer, false)) {
        layer.layer_time(ctx.time).0.hash(&mut h);
        // Effects also see the layer's time mapping.
        layer.start_time.0.hash(&mut h);
        layer.stretch.to_bits().hash(&mut h);
    }
    // Effects that see the comp camera or lights (#397) change when they or the layer move.
    if effects != Some(0) && info.reads_comp_scene {
        hash_debug(&mut h, &crate::comp_scene(ctx, layer));
    }
    // Effects that read other layers (layer parameters) see their pixels.
    if info.layer_refs {
        hash_layer_refs(&mut h, ctx, layer, info.reads_other_times, free, 0, effects)?;
    }
    Some(h.finish())
}

/// Does the layer run an effect that reads the comp camera or lights (Card Dance, CC Particle
/// World…)?
fn reads_comp_scene(layer: &Layer) -> bool {
    layer.switches.effects
        && layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| {
                g.enabled
                    && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effectcraft_effects::READS_COMP_SCENE.contains(&effect.as_str()))
            })
        })
}

/// Cache key for the styled layer (content key + the Layer Styles group: structure, eye
/// switches, values, keyframes, expressions and their values at the context time — Global Light
/// included, as every layer mirrors it in its Blending Options).
pub fn styles_key(ctx: &EvalCtx, layer: &Layer, content_key: u64) -> u64 {
    let mut h = KeyHasher(content_key ^ 0x9e37_79b9_7f4a_7c15);
    if let Some(g) = layer.layer_styles() {
        let info = layer_info(ctx.comp, layer);
        // Styles draw at the frame time (a blurred layer's styled buffer is drawn once and
        // moved, see `content_key`).
        let free = info.pointwise && !(layer.switches.motion_blur && ctx.comp.enable_motion_blur);
        free.hash(&mut h);
        (if free { info.styles_free } else { info.styles }).hash(&mut h);
        if info.styles_animated {
            hash_values(&mut h, ctx, layer, g);
        }
    }
    h.finish()
}

/// A sub-key of `key` (styled layer passes, flattened buffer).
pub fn derive(key: u64, i: u64) -> u64 {
    let mut h = KeyHasher(key);
    i.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod retained_tests {
    use super::*;
    fn pixels() -> Arc<Buf> {
        Arc::new(Buf { img: effectcraft_raster::Image::new(8, 8), offset: [0.0; 2], scale: 1.0 })
    }
    fn keep(c: &LayerCache, key: u64, b: &Arc<Buf>) {
        c.insert(key, b.clone());
        c.insert(key, b.clone());
    }
    fn packed(c: &LayerCache, key: u64) -> bool {
        c.inner.lock().unwrap().map.get(&key).is_some_and(|e| matches!(e.buf, Stored::Packed(_)))
    }

    #[test]
    fn aliases_share_physical_charge_and_clear_keeps_callers_pinned() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        keep(&c, 2, &b);
        assert_eq!(c.retained_stats().live_bytes, 1024);
        assert_eq!(c.stats().entries, 2);
        let hit = c.get(1).unwrap();
        c.clear();
        assert_eq!(c.stats().entries, 0);
        assert_eq!(c.retained_stats().live_bytes, 1024);
        drop(b);
        drop(hit);
        assert_eq!(c.retained_stats().live_bytes, 0);
    }

    #[test]
    fn pinned_shrink_denies_admission_and_preserves_fallback() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        c.set_budget(512);
        assert_eq!(c.retained_stats().debt_bytes, 512);
        let fresh = pixels();
        c.insert(2, fresh.clone());
        assert_eq!(fresh.img.data.len(), 64);
        assert_eq!(c.stats().entries, 0);
        assert_eq!(c.retained_stats().live_bytes, 1024);
        drop(b);
        assert_eq!(c.retained_stats().live_bytes, 0);
    }

    #[test]
    fn denied_unpack_keeps_packed_source() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        let token = c.resident_token().unwrap();
        let reservation = c.inner.lock().unwrap().physical.reserve(512).unwrap();
        let p = Arc::new(PackedBuf::pack(&b).unwrap());
        c.publish_packed(1, &b, p, reservation, &token);
        drop(b);
        assert!(packed(&c, 1));
        c.set_budget(1024);
        let back = c.get(1).unwrap();
        assert_eq!(back.img.data.len(), 64);
        assert!(packed(&c, 1), "packed 256 + full 1024 cannot both be retained");
        assert_eq!(c.retained_stats().live_bytes, 256);
        let again = c.get(1).unwrap();
        assert_eq!(again.img.data, back.img.data);
        assert!(!Arc::ptr_eq(&back, &again));
    }

    #[test]
    fn clear_during_packing_cannot_replace_new_entry() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        let token = c.resident_token().unwrap();
        let reservation = c.inner.lock().unwrap().physical.reserve(512).unwrap();
        let p = Arc::new(PackedBuf::pack(&b).unwrap());
        c.clear();
        let fresh = pixels();
        keep(&c, 1, &fresh);
        c.publish_packed(1, &b, p, reservation, &token);
        assert!(!packed(&c, 1));
        assert!(Arc::ptr_eq(&c.get(1).unwrap(), &fresh));
        assert_eq!(c.retained_stats().pending_bytes, 0);
    }

    #[test]
    fn packing_charges_both_copies_while_original_is_pinned() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        let token = c.resident_token().unwrap();
        let reservation = c.inner.lock().unwrap().physical.reserve(512).unwrap();
        c.publish_packed(1, &b, Arc::new(PackedBuf::pack(&b).unwrap()), reservation, &token);
        assert_eq!(c.retained_stats().live_bytes, 1280);
        drop(b);
        assert_eq!(c.retained_stats().live_bytes, 256);
    }
    #[test]
    fn no_packing_headroom_evicts_without_overshoot() {
        let c = LayerCache::new(4096);
        for key in 0..4 {
            keep(&c, key, &pixels());
        }
        assert_eq!(c.retained_stats().live_bytes, 4096);
        keep(&c, 4, &pixels());
        assert!(c.get(0).is_none());
        assert!(c.get(4).is_some());
        assert_eq!(c.retained_stats().live_bytes, 4096);
        assert_eq!(c.retained_stats().pending_bytes, 0);
        assert!(!packed(&c, 1));
    }

    #[test]
    fn gate_during_packing_rejects_publication_and_releases_reservation() {
        let c = LayerCache::new(8192);
        let b = pixels();
        keep(&c, 1, &b);
        let token = c.resident_token().unwrap();
        let reservation = c.inner.lock().unwrap().physical.reserve(512).unwrap();
        let p = Arc::new(PackedBuf::pack(&b).unwrap());
        c.set_gate(Some(Arc::new(std::sync::atomic::AtomicBool::new(true))));
        c.publish_packed(1, &b, p, reservation, &token);
        assert!(!packed(&c, 1));
        assert_eq!(c.retained_stats().pending_bytes, 0);
        assert!(Arc::ptr_eq(&c.get(1).unwrap(), &b));
    }
    #[test]
    fn stale_relief_cannot_evict_new_generation() {
        let c = LayerCache::new(8192);
        let old = c.resident_token().unwrap();
        c.clear();
        let fresh = pixels();
        keep(&c, 1, &fresh);
        // Simulate clear after the packing loop released its lock, before final eviction.
        c.evict_for(8192, &old);
        assert!(Arc::ptr_eq(&c.get(1).unwrap(), &fresh));
        // An old producer also carries its original token into the start of relief.
        c.relieve_for(8192, &old);
        assert!(Arc::ptr_eq(&c.get(1).unwrap(), &fresh));
    }
}

#[cfg(test)]
mod tests_stalled_owner {
    use super::*;
    use std::time::Instant;

    /// A pool thread whose wait for another thread's render ran out doesn't wait for that
    /// owner's other renders until the one it gave up on ends: with a 250 ms wait per layer
    /// key, a stuck owner froze playback for 10 to 20 s with the CPU idle (16 Oct 2026).
    #[test]
    fn a_stuck_owner_is_waited_for_once() {
        let cache = Arc::new(LayerCache::new(1 << 20));
        // The owner claims three keys and holds them (stuck) until told to let go.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let owner = {
            let cache = cache.clone();
            std::thread::spawn(move || {
                let guards: Vec<Claim> = (1..=3).map(|k| cache.claim(k)).collect();
                assert!(guards.iter().all(|c| matches!(c, Claim::Owner(_))));
                held_tx.send(()).unwrap();
                rx.recv().unwrap();
                drop(guards);
                // A render of its own again: waited for as before.
                let again = cache.claim(4);
                held_tx.send(()).unwrap();
                rx.recv().unwrap();
                drop(again);
            })
        };
        held_rx.recv().unwrap();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let timed = |key: u64| {
            let cache = cache.clone();
            pool.install(move || {
                let t0 = Instant::now();
                let c = cache.claim(key);
                (matches!(c, Claim::Render), t0.elapsed())
            })
        };
        let (render, first) = timed(1);
        assert!(render && first >= WAIT_POOL, "the first wait runs out ({first:?})");
        for key in [2, 3] {
            let (render, t) = timed(key);
            assert!(render && t < WAIT_POOL / 2, "key {key}: no second wait for a stuck owner ({t:?})");
        }
        // Its renders end: it isn't stuck any more.
        tx.send(()).unwrap();
        held_rx.recv().unwrap();
        assert!(cache.inflight.lock().unwrap().stalled.is_empty());
        let (render, t) = timed(4);
        assert!(render && t >= WAIT_POOL, "waited for again ({t:?})");
        tx.send(()).unwrap();
        owner.join().unwrap();
        assert!(cache.inflight.lock().unwrap().map.is_empty());
    }
}

#[cfg(test)]
mod tests_literal {
    #[test]
    fn literal_constant_expressions() {
        for s in ["0.5", " 50; ", "-1e3", "[100, 50]", "[1,2,3];"] {
            assert!(super::literal_constant(s), "{s}");
        }
        for s in ["time", "50 + time", "[]", "[1, time]", "wiggle(2, 5)", "Infinity", "NaN", "", "1 2"] {
            assert!(!super::literal_constant(s), "{s}");
        }
    }
}
