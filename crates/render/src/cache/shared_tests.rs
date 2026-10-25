//! The shared cache budget's arbiter (`cargo test -p effectcraft-render cache::shared`).

use super::*;
use std::collections::VecDeque;

/// A fake cache: entries `(stamp, bytes)` oldest first; `pinned` entries at the front can't go
/// (a protected working set).
struct Fake {
    entries: Mutex<VecDeque<(u64, u64)>>,
    pinned: usize,
    link: Mutex<Option<ShareLink>>,
}

impl Fake {
    fn new(pinned: usize) -> Arc<Fake> {
        Arc::new(Fake { entries: Mutex::new(VecDeque::new()), pinned, link: Mutex::new(None) })
    }
    fn join(self: &Arc<Self>, b: &Arc<SharedBudget>, kind: ShareKind) {
        let link = b.join(kind, Arc::new(FakeMember(Arc::downgrade(self))));
        *self.link.lock().unwrap() = link;
        self.report();
    }
    fn report(&self) {
        let e = self.entries.lock().unwrap();
        let bytes = e.iter().map(|x| x.1).sum();
        // The oldest entry it could let go of (pinned ones stay first in line).
        let oldest = e.get(self.pinned.min(e.len())).or(e.front()).map(|x| x.0);
        drop(e);
        if let Some(l) = self.link.lock().unwrap().as_ref() {
            l.report(bytes, oldest);
        }
    }
    fn put(&self, bytes: u64) -> u64 {
        let s = stamp();
        self.entries.lock().unwrap().push_back((s, bytes));
        self.report();
        s
    }
    fn bytes(&self) -> u64 {
        self.entries.lock().unwrap().iter().map(|x| x.1).sum()
    }
    fn stamps(&self) -> Vec<u64> {
        self.entries.lock().unwrap().iter().map(|x| x.0).collect()
    }
}

struct FakeMember(Weak<Fake>);

impl Member for FakeMember {
    fn evict_oldest(&self) -> u64 {
        let Some(f) = self.0.upgrade() else { return 0 };
        let got = {
            let mut e = f.entries.lock().unwrap();
            if e.len() <= f.pinned { 0 } else { e.remove(f.pinned).map_or(0, |x| x.1) }
        };
        f.report();
        got
    }
    fn alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

fn budget(split: Split) -> Arc<SharedBudget> {
    let b = SharedBudget::new().without_thread();
    b.configure(Some(split));
    b
}

const MB: u64 = 1 << 20;

#[test]
fn evicts_the_globally_least_recently_used_entry() {
    let b = budget(Split { total: 100 * MB, floors: [0; 3], shares: [40 * MB, 20 * MB, 40 * MB] });
    let (layer, media, preview) = (Fake::new(0), Fake::new(0), Fake::new(0));
    layer.join(&b, ShareKind::Layer);
    media.join(&b, ShareKind::Media);
    preview.join(&b, ShareKind::Preview);
    // Uses interleave across the caches: preview's first frame is the oldest of all.
    let p0 = preview.put(30 * MB);
    let l0 = layer.put(30 * MB);
    let m0 = media.put(10 * MB);
    let p1 = preview.put(30 * MB);
    assert!(!b.over(), "100 MB fit");
    // One more 20 MB layer buffer: 120 MB, so the oldest entry anywhere (preview's p0) goes,
    // not the layer cache's own oldest.
    let l1 = layer.put(20 * MB);
    assert!(b.over());
    assert_eq!(b.settle(), 30 * MB);
    assert_eq!(preview.stamps(), vec![p1]);
    assert_eq!(layer.stamps(), vec![l0, l1]);
    assert_eq!(media.stamps(), vec![m0]);
    // Next: 50 MB more in the preview (140 MB) evicts l0 (now the oldest), then m0.
    preview.put(50 * MB);
    b.settle();
    assert_eq!(layer.stamps(), vec![l1]);
    assert_eq!(media.stamps(), Vec::<u64>::new());
    assert!(b.usage().used <= 100 * MB);
    let _ = (p0, l0, m0);
    assert_eq!(b.usage().preview.evicted, 30 * MB);
    assert_eq!(b.usage().layer.evicted, 30 * MB);
}

#[test]
fn floors_are_never_taken() {
    let split = Split::of(40 * MB, 20 * MB, 40 * MB);
    // A total of 100 MB: floors of 12.5 MB, the footage cache's capped by its own 20 MB.
    assert_eq!(split.total, 100 * MB);
    assert_eq!(split.floors, [100 * MB / 8, 20 * MB, 100 * MB / 8]);
    let b = budget(split);
    let (layer, media, preview) = (Fake::new(0), Fake::new(0), Fake::new(0));
    layer.join(&b, ShareKind::Layer);
    media.join(&b, ShareKind::Media);
    preview.join(&b, ShareKind::Preview);
    // The footage cache's frames are the oldest of all, but it holds no more than its floor.
    for _ in 0..4 {
        media.put(5 * MB);
    }
    for _ in 0..10 {
        layer.put(4 * MB);
    }
    // The preview fills everything else and then some: the layer cache gives way down to its
    // floor, never the footage cache.
    for _ in 0..20 {
        preview.put(4 * MB);
        b.settle();
    }
    assert_eq!(media.bytes(), 20 * MB, "the footage cache keeps its floor");
    // A floor holds to within one entry: the arbiter asks a cache only while it is above it.
    assert!(layer.bytes() + 4 * MB > split.floor(ShareKind::Layer), "layer {} under its floor", layer.bytes());
    assert!(layer.bytes() <= split.floor(ShareKind::Layer), "the layer cache gave way down to its floor");
    assert!(b.usage().used <= 100 * MB);
}

#[test]
fn protected_entries_stay_and_the_next_oldest_cache_gives_way() {
    let b = budget(Split { total: 60 * MB, floors: [0; 3], shares: [30 * MB, 0, 30 * MB] });
    // Every preview frame is pinned (the playing preview reaches all of them).
    let (layer, preview) = (Fake::new(0), Fake::new(usize::MAX));
    layer.join(&b, ShareKind::Layer);
    preview.join(&b, ShareKind::Preview);
    preview.put(20 * MB);
    layer.put(20 * MB);
    preview.put(20 * MB);
    layer.put(20 * MB);
    b.settle();
    assert_eq!(preview.bytes(), 40 * MB, "pinned frames stay");
    assert_eq!(layer.bytes(), 20 * MB, "the layer cache gave its oldest");
}

#[test]
fn no_cache_may_grow_past_the_total() {
    let split = Split::of(8 << 30, 1 << 30, 8 << 30);
    for kind in ShareKind::ALL {
        assert!(split.limit(kind) <= split.total);
        assert!(split.floor(kind) <= split.share(kind));
        assert!(split.limit(kind) >= split.share(kind), "{kind:?}: the limit is at least its own budget");
    }
    let floors: u64 = split.floors.iter().sum();
    assert!(floors <= split.total);
    // The footage cache's floor is at least 512 MB (an eighth of 3 GB is 384 MB).
    let gb = Split::of(1 << 30, 1 << 30, 1 << 30);
    assert_eq!(gb.floors, [384 << 20, 512 << 20, 384 << 20]);
    assert_eq!(gb.limit(ShareKind::Preview), (3 << 30) - (896 << 20));
    // Tiny budgets: still consistent.
    let small = Split::of(64 << 20, 64 << 20, 64 << 20);
    assert_eq!(small.floor(ShareKind::Media), 64 << 20, "never above its own budget");
    assert!(ShareKind::ALL.iter().all(|k| small.limit(*k) <= small.total));
}

#[test]
fn off_or_empty_does_nothing_and_room_follows_the_others() {
    let b = SharedBudget::new().without_thread();
    let layer = Fake::new(0);
    layer.join(&b, ShareKind::Layer);
    layer.put(500 * MB);
    assert!(!b.over(), "off: no total");
    assert_eq!(b.settle(), 0);
    b.configure(Some(Split { total: 100 * MB, floors: [10 * MB, 10 * MB, 10 * MB], shares: [30 * MB, 30 * MB, 40 * MB] }));
    // Configuring settles at once: the layer cache's one entry is its oldest and goes.
    assert_eq!(layer.bytes(), 0);
    let preview = Fake::new(0);
    preview.join(&b, ShareKind::Preview);
    layer.put(50 * MB);
    let link = preview.link.lock().unwrap().clone().unwrap();
    // 100 − 50 held by the layer cache: 50 MB, more than the preview's 40 MB share.
    assert_eq!(link.room(), Some(50 * MB));
    layer.put(30 * MB);
    assert_eq!(link.room(), Some(40 * MB), "at least its own share");
    b.configure(None);
    assert_eq!(link.room(), None);
    assert!(!b.over());
}

#[test]
fn a_dropped_cache_stops_counting_and_leave_only_removes_its_own_link() {
    let b = budget(Split { total: 10 * MB, floors: [0; 3], shares: [5 * MB, 0, 5 * MB] });
    let layer = Fake::new(0);
    layer.join(&b, ShareKind::Layer);
    layer.put(8 * MB);
    let old = layer.link.lock().unwrap().clone().unwrap();
    let again = Fake::new(0);
    again.join(&b, ShareKind::Layer);
    old.leave();
    assert!(b.usage().layer.joined, "a stale link doesn't remove the new member");
    drop(again);
    b.settle();
    assert!(!b.usage().layer.joined);
}

#[test]
fn stamps_increase_across_threads() {
    let hs: Vec<_> = (0..4).map(|_| std::thread::spawn(|| (0..1000).map(|_| stamp()).collect::<Vec<_>>())).collect();
    let mut all: Vec<u64> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let n = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), n, "unique");
    assert_eq!(stamp_after(u64::MAX - 1), u64::MAX);
}

/// A real layer cache in the shared budget beside a fake RAM preview.
mod layer_cache {
    use super::*;
    use crate::cache::LayerCache;

    fn buf() -> Arc<effectcraft_effects::Buf> {
        Arc::new(effectcraft_effects::Buf { img: effectcraft_raster::Image::new(8, 8), offset: [0.0; 2], scale: 1.0 })
    }

    /// Kept in the cache proper (a second render admits it, see the cache's module docs).
    fn admit(c: &LayerCache, key: u64) {
        c.insert(key, buf());
        c.insert(key, buf());
    }

    fn setup() -> (Arc<SharedBudget>, LayerCache, Arc<Fake>, u64) {
        let c = LayerCache::new(1 << 20);
        c.insert(99, buf());
        let one = c.stats().bytes as u64;
        c.clear();
        let b = budget(Split { total: 3 * one, floors: [0; 3], shares: [2 * one, 0, one] });
        c.share(Some(&b));
        let preview = Fake::new(0);
        preview.join(&b, ShareKind::Preview);
        (b, c, preview, one)
    }

    #[test]
    fn the_globally_oldest_layer_buffer_goes_before_newer_preview_frames() {
        let (b, c, preview, one) = setup();
        admit(&c, 1);
        admit(&c, 2);
        preview.put(one);
        assert!(!b.over());
        admit(&c, 3);
        assert!(b.over());
        b.settle();
        assert!(!c.holds(1), "the oldest entry of all went");
        assert!(c.holds(2) && c.holds(3));
        assert_eq!(preview.bytes(), one, "the newer preview frame stayed");
        assert_eq!(b.usage().layer.bytes, c.stats().bytes as u64);
        assert!(b.usage().used <= 3 * one);
    }

    #[test]
    fn the_frames_being_rendered_keep_their_layer_buffers() {
        let (b, c, preview, one) = setup();
        admit(&c, 1);
        admit(&c, 2);
        let mark = c.begin_frame(8, 8);
        // Both buffers belong to the frame being rendered: the preview gives way instead.
        preview.put(one);
        preview.put(one);
        b.settle();
        assert!(c.holds(1) && c.holds(2), "the working set stays");
        assert_eq!(preview.bytes(), one);
        drop(mark);
        // The frame finished: now the layer cache's oldest buffer is fair game.
        preview.put(one);
        b.settle();
        assert!(!c.holds(1));
        assert!(c.holds(2));
        assert!(b.usage().used <= 3 * one);
    }

    #[test]
    fn leaving_the_shared_budget_stops_reporting() {
        let (b, c, _preview, _one) = setup();
        admit(&c, 1);
        assert!(b.usage().layer.joined && b.usage().layer.bytes > 0);
        c.share(None);
        assert!(!b.usage().layer.joined);
        assert_eq!(b.usage().layer.bytes, 0);
    }
}
