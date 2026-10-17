//! Playback warm-up: the layers a playing preview reaches soon have their sources ready before
//! it gets there.
//!
//! Prefetch renders whole frames a little ahead of the playhead (twice the frame threads, about
//! half a second). The first frames of a new section still waited on cold work that no earlier
//! frame had done: a still or PDF decoded (a phone PDF took 2 s), text and shapes rasterised, a
//! movie opened and sought. That made 0.2–1.6 s hitches at section entries in playback that
//! otherwise kept 30 fps (perf-3090, 17 Oct 2026).
//!
//! While a preview plays, one background thread walks the comp frame by frame ahead of the
//! playhead, into its precomps at their nested times, up to a horizon, and notes the layers that
//! become visible. For each entering footage, text or shape layer it renders what the frame's
//! walk will look up: the layer's content (source → masks → effects) on the CPU into the layer
//! cache, admitted straight away ([`effectcraft_render::cache::set_warming_thread`]). A layer
//! with effects only has its source read (decoded, rasterised), because its effects run on the
//! GPU in the frame. Footage still being read by a layer on screen is left alone (its decoder is
//! busy with the frames being shown). The footage and layer caches keep what it makes, within
//! their usual budgets; nothing is uploaded to the GPU, so the VRAM gate ([`super::vram`], CR1)
//! is untouched.
//!
//! The warm-up runs on its own two-thread Rayon pool, parallel pixel loops included: never on the
//! frame threads, nor on the global pool their pixel loops use. Its horizon follows what warming
//! has cost: a second plus twice the slowest recent warm-up, at most 5 s.
//! `EFFECTCRAFT_WARMUP=0` turns it off (for A/B measurements).

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use effectcraft_engine::project::{ItemId, LayerId, LayerSource};
use effectcraft_engine::render::{RenderOpts, Renderer};
use effectcraft_engine::time::Tick;

use super::RenderSource;

const MIN_HORIZON_S: f64 = 1.0;
const MAX_HORIZON_S: f64 = 5.0;
/// Threads of the warm-up pool.
const THREADS: usize = 2;
/// Precomp nesting the walk follows, and layers it lists per frame, at most.
const MAX_DEPTH: usize = 12;
const MAX_ACTIVE: usize = 4096;
/// Layers warmed per frame of the walk, at most.
const MAX_PER_STEP: usize = 32;
/// A position not refreshed for this long belongs to a preview that stopped.
const STALE: Duration = Duration::from_secs(1);

/// Where the playing preview is (refreshed every UI frame while it plays).
#[derive(Clone)]
pub struct Ahead {
    pub src: RenderSource,
    pub comp: ItemId,
    /// The viewer frames' render options (the layer cache keys depend on them).
    pub opts: RenderOpts,
    /// Comp time on screen.
    pub now: Tick,
    /// Comp time of the preview's last frame.
    pub end: Tick,
    /// Comp frame duration: the walk's step.
    pub step: Tick,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    ahead: Option<(Ahead, Instant)>,
    stop: bool,
    started: bool,
    /// For `ui.inspect`: layers warmed, the time it took (ms), the slowest recent warm-up (ms)
    /// and the horizon (s).
    warmed: u64,
    spent_ms: f64,
    slow_ms: f64,
    horizon_s: f64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The warm-up thread and what it is told (see the module docs).
pub(crate) struct Warmup {
    shared: Arc<Shared>,
    enabled: bool,
}

impl Default for Warmup {
    fn default() -> Self {
        Warmup { shared: Arc::default(), enabled: std::env::var("EFFECTCRAFT_WARMUP").map_or(true, |v| v != "0") }
    }
}

impl Drop for Warmup {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.wake.notify_all();
    }
}

impl Warmup {
    /// The playing preview's position, or `None` when it stopped. The first position starts the
    /// warm-up thread.
    pub(crate) fn set(&self, ahead: Option<Ahead>) {
        if !self.enabled {
            return;
        }
        let mut s = lock(&self.shared.state);
        let start = ahead.is_some() && !s.started;
        s.ahead = ahead.map(|a| (a, Instant::now()));
        if start {
            // Once: if the OS refuses the threads, warm-up stays off.
            s.started = true;
            drop(s);
            spawn(self.shared.clone());
            return;
        }
        drop(s);
        self.shared.wake.notify_one();
    }

    pub(crate) fn stats(&self) -> serde_json::Value {
        let s = lock(&self.shared.state);
        serde_json::json!({
            "enabled": self.enabled,
            "warmed": s.warmed,
            "spentMs": s.spent_ms.round(),
            "slowMs": s.slow_ms.round(),
            "horizonS": s.horizon_s,
        })
    }
}

fn spawn(shared: Arc<Shared>) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(THREADS)
        .thread_name(|i| format!("ec-warmup-{i}"))
        .start_handler(|_| effectcraft_render::cache::set_warming_thread(true))
        .build();
    let pool = match pool {
        Ok(p) => p,
        Err(e) => {
            log::warn!("playback warm-up is off: {e}");
            return;
        }
    };
    if let Err(e) = std::thread::Builder::new().name("ec-warmup".into()).spawn(move || run(&shared, &pool)) {
        log::warn!("playback warm-up is off: {e}");
    }
}

/// The warm-up thread: walks and warms while a preview plays, sleeps otherwise.
fn run(shared: &Shared, pool: &rayon::ThreadPool) {
    let mut plan = Plan::default();
    loop {
        let ahead = {
            let mut s = lock(&shared.state);
            loop {
                if s.stop {
                    return;
                }
                match &s.ahead {
                    Some((a, at)) if at.elapsed() < STALE => break a.clone(),
                    // Stopped (or the UI went quiet): let go of the project and wait.
                    _ => {
                        s.ahead = None;
                        plan = Plan::default();
                        s = shared.wake.wait_timeout(s, Duration::from_millis(500)).map_or_else(|e| e.into_inner().0, |(g, _)| g);
                    }
                }
            }
        };
        let step = ahead.step;
        let plan_ref = &mut plan;
        let done = pool.install(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plan_ref.step(&ahead))));
        match done {
            Ok(Step::Warmed(n, ms)) => {
                let mut s = lock(&shared.state);
                s.warmed = s.warmed.saturating_add(n as u64);
                s.spent_ms += ms;
                s.slow_ms = plan.slow_ms;
                s.horizon_s = plan.horizon().seconds();
            }
            Ok(Step::Walked) => {}
            Ok(Step::Ahead) => {
                // Caught up with the horizon: the next UI frame moves it.
                let s = lock(&shared.state);
                let wait = Duration::from_secs_f64(step.seconds().clamp(0.005, 0.05));
                drop(shared.wake.wait_timeout(s, wait));
            }
            Err(_) => {
                log::error!("playback warm-up panicked; it starts over");
                plan = Plan::default();
            }
        }
    }
}

/// What one [`Plan::step`] did.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// Warmed this many layers in this many ms.
    Warmed(usize, f64),
    /// Walked a frame (or started over), nothing to warm.
    Walked,
    /// The walk is at the horizon.
    Ahead,
}

/// The walk ahead of the playhead.
#[derive(Default)]
pub(crate) struct Plan {
    /// The comp and render options the walk belongs to.
    ident: Option<(ItemId, u64)>,
    /// Comp time walked up to.
    frontier: Tick,
    /// The layers drawn at the frontier ([`Active::key`]).
    seen: HashSet<u64>,
    /// The slowest recent warm-up (ms), decaying.
    slow_ms: f64,
}

impl Plan {
    /// How far ahead of the playhead the walk goes.
    pub(crate) fn horizon(&self) -> Tick {
        Tick::from_seconds_f64((MIN_HORIZON_S + 2.0 * self.slow_ms / 1000.0).clamp(MIN_HORIZON_S, MAX_HORIZON_S))
    }

    /// Walk one more frame ahead and warm the layers that enter there. Starts over (without
    /// warming what is on screen) when the comp or options change or the playhead jumped.
    pub(crate) fn step(&mut self, a: &Ahead) -> Step {
        let r = renderer(a);
        let ident = (a.comp, super::opts_hash(&a.opts));
        let step = Tick(a.step.0.max(1));
        let horizon = self.horizon();
        let far = Tick(a.now.0.saturating_add(horizon.0).saturating_add(step.0));
        if self.ident != Some(ident) || self.frontier < a.now || self.frontier > far {
            self.ident = Some(ident);
            self.frontier = a.now;
            self.seen = walk(&r, a.comp, a.now).iter().map(|x| x.key).collect();
            return Step::Walked;
        }
        let limit = Tick(a.now.0.saturating_add(horizon.0)).min(a.end);
        let next = Tick(self.frontier.0.saturating_add(step.0));
        if next > limit {
            return Step::Ahead;
        }
        self.frontier = next;
        let now = walk(&r, a.comp, next);
        let enter = entering(&self.seen, &now);
        self.seen = now.iter().map(|x| x.key).collect();
        if enter.is_empty() {
            return Step::Walked;
        }
        let t0 = Instant::now();
        let n = warm(&r, &enter);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        self.slow_ms = (self.slow_ms * 0.9).max(ms);
        Step::Warmed(n, ms)
    }
}

fn renderer(a: &Ahead) -> Renderer<'_> {
    let mut r = Renderer::new(&a.src.project, a.src.footage.as_ref(), a.opts);
    r.expr = a.src.expr.as_deref();
    r.cache = Some(a.src.layer_cache.as_ref());
    r
}

/// A layer drawn at a time.
#[derive(Debug)]
pub(crate) struct Active {
    /// The layer and the precomp layers it is reached through (a comp nested twice is two).
    key: u64,
    comp: ItemId,
    layer: LayerId,
    /// Its comp's time.
    t: Tick,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Precomp,
    Footage(ItemId),
    /// Text and shapes: rasterised.
    Drawn,
    Other,
}

/// The layers drawn at comp time `t`, nested comps' included.
fn walk(r: &Renderer, comp: ItemId, t: Tick) -> Vec<Active> {
    let mut out = Vec::new();
    walk_into(r, comp, t, 0, 0, &mut out);
    out
}

fn walk_into(r: &Renderer, comp: ItemId, t: Tick, parent: u64, depth: usize, out: &mut Vec<Active>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(ctx) = r.eval_ctx(comp, t) else { return };
    for l in r.visible_layers(&ctx) {
        if out.len() >= MAX_ACTIVE {
            return;
        }
        let mut h = DefaultHasher::new();
        (parent, comp, l.id).hash(&mut h);
        let key = h.finish();
        let kind = match l.source {
            LayerSource::Comp { .. } => Kind::Precomp,
            LayerSource::Footage { item } => Kind::Footage(item),
            LayerSource::Text | LayerSource::Shape => Kind::Drawn,
            _ => Kind::Other,
        };
        out.push(Active { key, comp, layer: l.id, t, kind });
        if let LayerSource::Comp { item } = l.source {
            walk_into(r, item, ctx.nested_time(l), key, depth + 1, out);
        }
    }
}

/// The layers of `now` that weren't drawn a frame earlier (`seen`) and are worth warming. A
/// precomp is walked into instead: its own buffer composites the layers below it on the GPU.
fn entering<'w>(seen: &HashSet<u64>, now: &'w [Active]) -> Vec<&'w Active> {
    // Footage a layer on screen keeps reading: decoded already, or its decoder busy with the
    // frames being shown.
    let busy: HashSet<ItemId> = now
        .iter()
        .filter(|a| seen.contains(&a.key))
        .filter_map(|a| match a.kind {
            Kind::Footage(item) => Some(item),
            _ => None,
        })
        .collect();
    now.iter()
        .filter(|a| !seen.contains(&a.key))
        .filter(|a| match a.kind {
            Kind::Footage(item) => !busy.contains(&item),
            Kind::Drawn => true,
            Kind::Precomp | Kind::Other => false,
        })
        .take(MAX_PER_STEP)
        .collect()
}

/// Render what the frames will look up for `layers` (see the module docs); how many were warmed.
fn warm(r: &Renderer, layers: &[&Active]) -> usize {
    let mut n = 0;
    for a in layers {
        let Some(ctx) = r.eval_ctx(a.comp, a.t) else { continue };
        let Some(layer) = ctx.comp.layer(a.layer) else { continue };
        // A frame got there first: it renders the layer, no use doing it twice.
        if r.resident_content(&ctx, layer).is_some_and(|(k, _)| r.cache.is_some_and(|c| c.in_flight(k))) {
            continue;
        }
        if layer.effects().is_some_and(|fx| fx.groups().any(|e| e.enabled)) {
            let _ = r.layer_source(&ctx, layer);
        } else {
            let _ = r.content_buf(&ctx, layer);
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::Session;
    use effectcraft_engine::project::{Footage, FootageKind, ItemKind};
    use effectcraft_engine::render::{FootageSource, Image, LayerCache};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Footage that counts its decodes and remembers the thread that asked last.
    #[derive(Default)]
    struct Counting {
        decodes: AtomicUsize,
        thread: Mutex<String>,
    }

    impl FootageSource for Counting {
        fn frame(&self, _: ItemId, f: &Footage, _: Tick) -> Option<Arc<Image>> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            *lock(&self.thread) = std::thread::current().name().unwrap_or_default().to_string();
            Some(Arc::new(Image::new(f.width, f.height)))
        }
    }

    fn still(s: &mut Session, name: &str) -> ItemId {
        let f = Footage { kind: FootageKind::Still, width: 64, height: 64, has_video: true, ..Default::default() };
        Arc::make_mut(&mut s.project).add_item(name, Default::default(), None, ItemKind::Footage(f))
    }

    /// Main comp "A" (10 s): a still entering at 2 s, and a precomp "B" from 0 s whose still
    /// enters at 3 s of B's time (3 s in A).
    fn project() -> (Session, ItemId) {
        let mut s = Session::default();
        let b = ItemId(s.execute("comp.new", json!({"name": "B", "width": 64, "height": 64, "duration": 10})).unwrap()["comp"].as_u64().unwrap());
        let inner = still(&mut s, "inner.png");
        s.execute("layer.addItem", json!({"item": inner.0})).unwrap();
        let a = ItemId(s.execute("comp.new", json!({"name": "A", "width": 64, "height": 64, "duration": 10})).unwrap()["comp"].as_u64().unwrap());
        s.execute("layer.addItem", json!({"item": b.0})).unwrap();
        let outer = still(&mut s, "outer.png");
        s.execute("layer.addItem", json!({"item": outer.0})).unwrap();
        let p = Arc::make_mut(&mut s.project);
        for (comp, from) in [(b, 3.0), (a, 2.0)] {
            let c = p.comp_mut(comp).unwrap();
            for l in &mut c.layers {
                l.out_point = Tick::from_seconds_f64(10.0);
                if matches!(l.source, LayerSource::Footage { .. }) {
                    l.in_point = Tick::from_seconds_f64(from);
                }
            }
        }
        (s, a)
    }

    fn ahead(s: &Session, comp: ItemId, footage: Arc<Counting>, cache: Arc<LayerCache>, now: f64) -> Ahead {
        let fr = s.project.comp(comp).unwrap().frame_rate;
        Ahead {
            src: RenderSource {
                freeze_feedback: Default::default(),
                project: s.project.clone(),
                footage,
                expr: None,
                layer_cache: cache,
                gpu: None,
                gpu_display: false,
                disk: None,
            },
            comp,
            opts: RenderOpts { scale: 1.0, ..Default::default() },
            now: Tick::from_seconds_f64(now),
            end: Tick::from_seconds_f64(9.9),
            step: fr.frame_duration(),
        }
    }

    fn run_to_horizon(plan: &mut Plan, a: &Ahead) -> usize {
        let mut warmed = 0;
        for _ in 0..10_000 {
            match plan.step(a) {
                Step::Warmed(n, _) => warmed += n,
                Step::Walked => {}
                Step::Ahead => return warmed,
            }
        }
        panic!("the walk never reached its horizon");
    }

    /// Section entries: a layer (top level or inside a precomp) is warmed once, when its in-point
    /// comes within the horizon ahead of the playhead, not before, and what is on screen when
    /// playback starts is left to the frame threads.
    #[test]
    fn layers_are_warmed_once_when_their_in_point_comes_within_the_horizon() {
        let (s, a) = project();
        let footage = Arc::new(Counting::default());
        let cache = Arc::new(LayerCache::default());
        let mut plan = Plan::default();
        assert_eq!(plan.horizon(), Tick::from_seconds_f64(1.0));
        // At 0.5 s the horizon reaches 1.5 s: nothing enters before 2 s.
        assert_eq!(run_to_horizon(&mut plan, &ahead(&s, a, footage.clone(), cache.clone(), 0.5)), 0);
        assert_eq!(footage.decodes.load(Ordering::SeqCst), 0);
        // At 1.2 s the horizon passes 2 s: the top-level still is decoded, ahead of the playhead.
        assert_eq!(run_to_horizon(&mut plan, &ahead(&s, a, footage.clone(), cache.clone(), 1.2)), 1);
        let after_outer = footage.decodes.load(Ordering::SeqCst);
        assert!(after_outer >= 1);
        // Further on, the precomp's still entering at 3 s, once; the outer one isn't warmed again.
        let mut warmed = 0;
        for now in [1.5, 2.0, 2.5] {
            warmed += run_to_horizon(&mut plan, &ahead(&s, a, footage.clone(), cache.clone(), now));
        }
        assert_eq!(warmed, 1, "the precomp's layer, once");
        assert!(footage.decodes.load(Ordering::SeqCst) > after_outer);
        // Seeking back starts over from the new position: what is on screen there isn't warmed.
        let mut fresh = Plan::default();
        assert_eq!(run_to_horizon(&mut fresh, &ahead(&s, a, footage.clone(), cache.clone(), 3.5)), 0);
    }

    /// A frame never waits for the warm-up: its threads render without claiming a key (their
    /// pool is a fraction of the frame's), so the key stays free for a frame thread to claim.
    #[test]
    fn warm_up_threads_never_claim_a_layer() {
        use effectcraft_render::cache::Claim;
        let cache = Arc::new(LayerCache::default());
        let c = cache.clone();
        let warm = std::thread::spawn(move || {
            effectcraft_render::cache::set_warming_thread(true);
            let claim = c.claim(7);
            (matches!(claim, Claim::Owner(_)), c.in_flight(7))
        });
        assert_eq!(warm.join().unwrap(), (false, false));
        let claim = cache.claim(7);
        assert!(matches!(claim, Claim::Owner(_)) && cache.in_flight(7), "a frame thread claims it");
    }

    /// The warm-up thread decodes on its own pool, never a frame thread, and what it renders
    /// stays in the layer cache past the probation window (the frames in between would flush it
    /// otherwise), so the section's first frame finds it.
    #[test]
    fn warm_up_runs_on_its_own_threads_and_its_layers_skip_probation() {
        let (s, a) = project();
        let footage = Arc::new(Counting::default());
        let cache = Arc::new(LayerCache::default());
        let w = Warmup { shared: Arc::default(), enabled: true };
        for _ in 0..400 {
            w.set(Some(ahead(&s, a, footage.clone(), cache.clone(), 1.5)));
            if lock(&w.shared.state).warmed >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lock(&w.shared.state).warmed >= 1, "warmed in the background");
        assert!(lock(&footage.thread).starts_with("ec-warmup-"), "on the warm-up pool, not {:?}", lock(&footage.thread));
        w.set(None);
        // Frames rendered meanwhile fill probation (16 MB at least) and flush it.
        for k in 0..4u64 {
            let img = Image::new(1024, 1024);
            cache.insert(u64::MAX - k, Arc::new(effectcraft_engine::effects::Buf { img, offset: [0.0; 2], scale: 1.0 }));
        }
        let decodes = footage.decodes.load(Ordering::SeqCst);
        let hits = cache.stats().hits;
        let a2 = ahead(&s, a, footage.clone(), cache.clone(), 2.0);
        let r = renderer(&a2);
        let ctx = r.eval_ctx(a, Tick::from_seconds_f64(2.0)).unwrap();
        let layer = ctx.comp.layers.iter().find(|l| matches!(l.source, LayerSource::Footage { .. })).unwrap();
        assert!(r.content_buf(&ctx, layer).is_some());
        assert_eq!(cache.stats().hits, hits + 1, "the warmed content is a hit");
        assert_eq!(footage.decodes.load(Ordering::SeqCst), decodes, "not decoded again");
    }
}
