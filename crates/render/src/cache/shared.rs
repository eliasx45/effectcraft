//! One memory budget shared by the layer cache, the footage frame cache and the RAM preview
//! (Settings ▸ Memory & CPU ▸ Share One Budget Between the Caches, `memory.sharedCacheBudget`).
//!
//! Before, each cache had its own budget and they never traded memory: an edit that made every
//! layer render again filled the layer cache to its budget and evicted its own buffers while the
//! RAM preview sat on frames of the comp before the edit (`plan/memory/cache-prototypes.md`,
//! experiment 2: edit + replay 1149 s → 436 s of render wait with one budget and plain LRU).
//!
//! **The rule.** The total is what the three budgets add up to (so RAM Reserved for Other
//! Applications still caps it). Each cache keeps a floor nobody else can take ([`Split`]) and
//! may grow into the rest up to its limit (the total less the others' floors). When the caches
//! together hold more than the total, the least recently used entry *across* the caches goes:
//! every cache stamps uses from one process-wide clock ([`stamp`]) and publishes the stamp of
//! its oldest entry with its bytes ([`ShareLink::report`]: two relaxed atomic stores), and the
//! arbiter asks the cache with the oldest stamp to evict one entry ([`Member::evict_oldest`]),
//! by that cache's own rules (the layer cache's probation and working set, the RAM preview's
//! playback range and protected window, the footage cache's newest frame). A cache that can't
//! let anything go is skipped for the rest of that pass; a cache at its floor is never asked.
//!
//! **Locks.** The arbiter holds no cache lock while it asks a cache to evict, and a cache never
//! calls the arbiter while it holds its own lock: a report only stores atomics and, when the
//! total is exceeded, wakes the arbiter thread (`ec-cache-arbiter`, started the first time it
//! is needed). So no thread ever holds two cache locks, and eviction for the shared budget runs
//! off the render and UI threads. Each cache still evicts for its own limit inline, as before.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use serde::Serialize;

/// The process-wide use clock (see [`stamp`]).
static CLOCK: AtomicU64 = AtomicU64::new(0);

/// A last-use stamp: unique and increasing across every cache of the process, so the caches'
/// oldest entries can be compared.
pub fn stamp() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

/// The next stamp for a cache whose latest stamp is `last`: from the shared clock, and always
/// after `last`.
pub fn stamp_after(last: u64) -> u64 {
    stamp().max(last.saturating_add(1))
}

/// The caches that share the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ShareKind {
    /// Processed layer buffers ([`super::LayerCache`]).
    Layer,
    /// Decoded footage frames (the media pool).
    Media,
    /// RAM preview frames (the viewer's frame cache).
    Preview,
}

impl ShareKind {
    pub const ALL: [ShareKind; 3] = [ShareKind::Layer, ShareKind::Media, ShareKind::Preview];

    pub const fn index(self) -> usize {
        match self {
            ShareKind::Layer => 0,
            ShareKind::Media => 1,
            ShareKind::Preview => 2,
        }
    }
}

/// How the shared budget is split: the total, and per cache ([`ShareKind::index`] order) the
/// floor the others can't take and the budget it had on its own (its fair share: what plans
/// such as how many frames render at once keep using, so they don't change with the setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Split {
    pub total: u64,
    pub floors: [u64; 3],
    pub shares: [u64; 3],
}

impl Split {
    /// Each cache's floor is an eighth of the total ([`FLOOR_DIVISOR`]), the footage cache's at
    /// least [`MEDIA_FLOOR`] (decoding a frame again costs more than finding it), and never more
    /// than the cache's own budget, so the floors always fit in the total.
    pub fn of(layer: u64, media: u64, preview: u64) -> Split {
        let total = layer.saturating_add(media).saturating_add(preview);
        let part = total / FLOOR_DIVISOR;
        Split { total, floors: [part.min(layer), part.max(MEDIA_FLOOR).min(media), part.min(preview)], shares: [layer, media, preview] }
    }

    pub fn floor(&self, kind: ShareKind) -> u64 {
        self.floors.get(kind.index()).copied().unwrap_or(0)
    }

    pub fn share(&self, kind: ShareKind) -> u64 {
        self.shares.get(kind.index()).copied().unwrap_or(0)
    }

    /// The most `kind` may hold: the total less the other caches' floors (never above the total).
    pub fn limit(&self, kind: ShareKind) -> u64 {
        let others = ShareKind::ALL.iter().filter(|k| **k != kind).map(|k| self.floor(*k)).fold(0u64, u64::saturating_add);
        self.total.saturating_sub(others)
    }
}

/// A cache's floor is at least this share (1 / n) of the total.
pub const FLOOR_DIVISOR: u64 = 8;
/// The footage cache's floor is at least this (when its own budget is).
pub const MEDIA_FLOOR: u64 = 512 << 20;

/// A cache taking part in the shared budget.
pub trait Member: Send + Sync {
    /// Evict this cache's least recently used entry that its own rules let go (a protected
    /// working set stays). Returns the bytes freed, 0 when nothing can go now. Called with no
    /// lock held; takes only the cache's own lock.
    fn evict_oldest(&self) -> u64;
    /// The cache still exists (a member over a dropped cache stops counting).
    fn alive(&self) -> bool {
        true
    }
}

/// `oldest` of an empty cache.
const EMPTY: u64 = u64::MAX;
/// Evictions in one pass at most (each frees an entry; the bound only guards a member whose
/// bytes don't go down).
const MAX_EVICTIONS: usize = 1 << 16;
/// Passes a settle runs at most when reports keep coming in while it works.
const MAX_PASSES: usize = 4;

struct Slot {
    bytes: AtomicU64,
    oldest: AtomicU64,
    floor: AtomicU64,
    limit: AtomicU64,
    share: AtomicU64,
    /// Bytes the arbiter evicted from this cache so far.
    evicted: AtomicU64,
    /// The member and the id of the link it joined with ([`SharedBudget::join`]).
    member: Mutex<Option<(u64, Arc<dyn Member>)>>,
    /// That id, lock-free (0: none): a report through an older link (a cache that was replaced
    /// but is still alive somewhere) is ignored.
    current: AtomicU64,
}

impl Slot {
    fn new() -> Slot {
        Slot {
            bytes: AtomicU64::new(0),
            oldest: AtomicU64::new(EMPTY),
            floor: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            share: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            member: Mutex::new(None),
            current: AtomicU64::new(0),
        }
    }

    fn member(&self) -> Option<Arc<dyn Member>> {
        self.member.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|(_, m)| m.clone())
    }
}

#[derive(Default)]
struct Signal {
    pending: Mutex<bool>,
    cv: Condvar,
}

/// The shared budget and its arbiter (one per session; see the module docs).
pub struct SharedBudget {
    on: AtomicBool,
    total: AtomicU64,
    slots: [Slot; 3],
    next_link: AtomicU64,
    settling: AtomicBool,
    again: AtomicBool,
    /// A wake-up is on its way to the arbiter thread.
    nudged: AtomicBool,
    thread: AtomicBool,
    signal: Arc<Signal>,
    /// The arbiter's thread runs settles; `false` (tests): only explicit [`SharedBudget::settle`].
    background: AtomicBool,
    me: Weak<SharedBudget>,
}

/// What one cache holds of the shared budget (`memory.stats` → `sharedBudget.layer`, `.media`,
/// `.preview`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareUsage {
    pub bytes: u64,
    pub floor: u64,
    pub limit: u64,
    /// Its own budget when the caches don't share (the fair share).
    pub share: u64,
    /// Bytes the arbiter evicted from it to make room for the other caches.
    pub evicted: u64,
    /// The cache takes part (it joined and still exists).
    pub joined: bool,
}

/// The shared budget now (`memory.stats` → `sharedBudget`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedUsage {
    pub on: bool,
    pub total: u64,
    /// The caches' bytes together.
    pub used: u64,
    pub layer: ShareUsage,
    pub media: ShareUsage,
    pub preview: ShareUsage,
}

impl SharedBudget {
    /// An arbiter that is off until [`SharedBudget::configure`] turns it on.
    pub fn new() -> Arc<SharedBudget> {
        Arc::new_cyclic(|me| SharedBudget {
            on: AtomicBool::new(false),
            total: AtomicU64::new(0),
            slots: [Slot::new(), Slot::new(), Slot::new()],
            next_link: AtomicU64::new(1),
            settling: AtomicBool::new(false),
            again: AtomicBool::new(false),
            nudged: AtomicBool::new(false),
            thread: AtomicBool::new(false),
            signal: Arc::new(Signal::default()),
            background: AtomicBool::new(cfg!(not(target_arch = "wasm32"))),
            me: me.clone(),
        })
    }

    /// Settle only when [`SharedBudget::settle`] is called (no arbiter thread): tests.
    pub fn without_thread(self: Arc<Self>) -> Arc<Self> {
        self.background.store(false, Ordering::Relaxed);
        self
    }

    fn slot(&self, kind: ShareKind) -> Option<&Slot> {
        self.slots.get(kind.index())
    }

    /// Share `split` between the caches (`None`: off, each cache keeps to its own budget).
    /// Evicts right away when the caches now hold more than the new total. Call it with no
    /// cache lock held (the session applies the budgets this way).
    pub fn configure(&self, split: Option<Split>) {
        match split {
            Some(s) => {
                for kind in ShareKind::ALL {
                    if let Some(slot) = self.slot(kind) {
                        slot.floor.store(s.floor(kind), Ordering::Relaxed);
                        slot.limit.store(s.limit(kind), Ordering::Relaxed);
                        slot.share.store(s.share(kind), Ordering::Relaxed);
                    }
                }
                self.total.store(s.total, Ordering::Relaxed);
                self.on.store(true, Ordering::Release);
                self.settle();
            }
            None => self.on.store(false, Ordering::Release),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// `kind`'s cache takes part from now on, evicting through `member` (replacing the cache
    /// that joined as `kind` before). The cache reports its bytes through the returned link.
    pub fn join(&self, kind: ShareKind, member: Arc<dyn Member>) -> Option<ShareLink> {
        let slot = self.slot(kind)?;
        let id = self.next_link.fetch_add(1, Ordering::Relaxed);
        *slot.member.lock().unwrap_or_else(PoisonError::into_inner) = Some((id, member));
        slot.current.store(id, Ordering::Release);
        slot.bytes.store(0, Ordering::Relaxed);
        slot.oldest.store(EMPTY, Ordering::Relaxed);
        Some(ShareLink { budget: self.me.upgrade()?, kind, id })
    }

    fn used(&self) -> u64 {
        self.slots.iter().map(|s| s.bytes.load(Ordering::Relaxed)).fold(0u64, u64::saturating_add)
    }

    /// The caches together hold more than the total (and sharing is on).
    pub fn over(&self) -> bool {
        self.is_on() && self.used() > self.total.load(Ordering::Relaxed)
    }

    /// Wake the arbiter thread (starting it the first time). Cheap and lock-free when a wake-up
    /// is already on its way; otherwise one short lock of the arbiter's own (a leaf: nothing is
    /// locked under it).
    fn nudge(&self) {
        if !self.background.load(Ordering::Relaxed) || self.nudged.swap(true, Ordering::AcqRel) {
            return;
        }
        if !self.thread.swap(true, Ordering::AcqRel) {
            self.spawn();
        }
        let mut p = self.signal.pending.lock().unwrap_or_else(PoisonError::into_inner);
        *p = true;
        self.signal.cv.notify_one();
    }

    fn spawn(&self) {
        let (me, signal) = (self.me.clone(), self.signal.clone());
        let started = std::thread::Builder::new().name("ec-cache-arbiter".into()).spawn(move || {
            loop {
                {
                    let mut p = signal.pending.lock().unwrap_or_else(PoisonError::into_inner);
                    while !*p {
                        p = signal.cv.wait(p).unwrap_or_else(PoisonError::into_inner);
                    }
                    *p = false;
                }
                let Some(budget) = me.upgrade() else { return };
                budget.nudged.store(false, Ordering::Release);
                budget.settle();
            }
        });
        if let Err(e) = started {
            // Each cache still keeps to its own limit; only the cross-cache trade waits for the
            // next explicit settle (a change of the budgets).
            log::warn!("cache arbiter thread: {e}");
            self.background.store(false, Ordering::Relaxed);
        }
    }

    /// While the caches hold more than the total, evict the least recently used entry across
    /// them (see the module docs). Returns the bytes freed. Holds no cache lock between
    /// evictions; a second caller while one runs only asks it to look again.
    pub fn settle(&self) -> u64 {
        if !self.over() {
            return 0;
        }
        if self.settling.swap(true, Ordering::AcqRel) {
            self.again.store(true, Ordering::Release);
            return 0;
        }
        let mut freed = 0u64;
        for _ in 0..MAX_PASSES {
            self.again.store(false, Ordering::Release);
            freed = freed.saturating_add(self.pass());
            if !self.again.load(Ordering::Acquire) {
                break;
            }
        }
        self.settling.store(false, Ordering::Release);
        freed
    }

    fn pass(&self) -> u64 {
        // The members, cloned out one slot lock at a time (no lock is held while they evict).
        let members: [Option<Arc<dyn Member>>; 3] = std::array::from_fn(|i| {
            let slot = self.slots.get(i)?;
            let m = slot.member().filter(|m| m.alive());
            if m.is_none() {
                slot.current.store(0, Ordering::Release);
                slot.bytes.store(0, Ordering::Relaxed);
                slot.oldest.store(EMPTY, Ordering::Relaxed);
            }
            m
        });
        let mut stuck = [false; 3];
        let mut freed = 0u64;
        for _ in 0..MAX_EVICTIONS {
            if !self.over() {
                break;
            }
            let pick = (0..3)
                .filter(|i| !stuck.get(*i).copied().unwrap_or(true) && members.get(*i).is_some_and(Option::is_some))
                .filter_map(|i| {
                    let s = self.slots.get(i)?;
                    let bytes = s.bytes.load(Ordering::Relaxed);
                    (bytes > s.floor.load(Ordering::Relaxed)).then(|| (s.oldest.load(Ordering::Relaxed), i))
                })
                .min();
            let Some((_, i)) = pick else { break };
            let Some(Some(member)) = members.get(i) else { break };
            let got = member.evict_oldest();
            if got == 0 {
                if let Some(s) = stuck.get_mut(i) {
                    *s = true;
                }
                continue;
            }
            if let Some(s) = self.slots.get(i) {
                s.evicted.fetch_add(got, Ordering::Relaxed);
            }
            freed = freed.saturating_add(got);
        }
        freed
    }

    /// The split and what each cache holds now (atomic loads and one short slot lock each).
    pub fn usage(&self) -> SharedUsage {
        let one = |kind: ShareKind| {
            self.slot(kind).map_or_else(ShareUsage::default, |s| ShareUsage {
                bytes: s.bytes.load(Ordering::Relaxed),
                floor: s.floor.load(Ordering::Relaxed),
                limit: s.limit.load(Ordering::Relaxed),
                share: s.share.load(Ordering::Relaxed),
                evicted: s.evicted.load(Ordering::Relaxed),
                joined: s.member().is_some_and(|m| m.alive()),
            })
        };
        SharedUsage {
            on: self.is_on(),
            total: self.total.load(Ordering::Relaxed),
            used: self.used(),
            layer: one(ShareKind::Layer),
            media: one(ShareKind::Media),
            preview: one(ShareKind::Preview),
        }
    }
}

impl Drop for SharedBudget {
    /// The arbiter thread (if any) wakes, finds the budget gone and ends (it sleeps otherwise:
    /// an idle app stays idle).
    fn drop(&mut self) {
        let mut p = self.signal.pending.lock().unwrap_or_else(PoisonError::into_inner);
        *p = true;
        self.signal.cv.notify_all();
    }
}

/// A cache's handle on the shared budget (see [`SharedBudget::join`]). Cheap to clone.
#[derive(Clone)]
pub struct ShareLink {
    budget: Arc<SharedBudget>,
    kind: ShareKind,
    id: u64,
}

impl ShareLink {
    pub fn kind(&self) -> ShareKind {
        self.kind
    }

    /// The cache holds `bytes` now and its least recently used entry was last used at `oldest`
    /// ([`stamp`]; `None`: empty). Two relaxed stores; when the caches together are over the
    /// total, the arbiter thread is woken. Safe to call under the cache's own lock.
    pub fn report(&self, bytes: u64, oldest: Option<u64>) {
        let Some(slot) = self.budget.slot(self.kind) else { return };
        if slot.current.load(Ordering::Acquire) != self.id {
            return;
        }
        slot.bytes.store(bytes, Ordering::Relaxed);
        slot.oldest.store(oldest.unwrap_or(EMPTY), Ordering::Relaxed);
        if self.budget.over() {
            self.budget.nudge();
        }
    }

    /// What this cache may plan to fill now: the total less what the other caches hold, at
    /// least its fair share and at most its limit (`None`: sharing is off).
    pub fn room(&self) -> Option<u64> {
        let b = &self.budget;
        if !b.is_on() {
            return None;
        }
        let slot = b.slot(self.kind)?;
        let others = b.used().saturating_sub(slot.bytes.load(Ordering::Relaxed));
        let free = b.total.load(Ordering::Relaxed).saturating_sub(others);
        Some(free.max(slot.share.load(Ordering::Relaxed)).min(slot.limit.load(Ordering::Relaxed)))
    }

    /// The cache's own budget when the caches don't share (`None`: sharing is off).
    pub fn share(&self) -> Option<u64> {
        let b = &self.budget;
        b.is_on().then(|| b.slot(self.kind).map(|s| s.share.load(Ordering::Relaxed))).flatten()
    }

    /// Stop taking part (only if this link is still the one that joined as its kind).
    pub fn leave(&self) {
        let Some(slot) = self.budget.slot(self.kind) else { return };
        let mut m = slot.member.lock().unwrap_or_else(PoisonError::into_inner);
        if m.as_ref().is_some_and(|(id, _)| *id == self.id) {
            *m = None;
            slot.current.store(0, Ordering::Release);
            drop(m);
            slot.bytes.store(0, Ordering::Relaxed);
            slot.oldest.store(EMPTY, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
