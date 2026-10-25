//! The layer cache keeps precomps and footage, memoises key structure and renders concurrent
//! misses once.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, Expression, Footage, FootageKind, ItemId, ItemKind, Layer, LayerSource, Project, Property, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{EvalCtx, ExprHost, FootageSource, Image, LayerCache, NoFootage, RenderOpts, Renderer};

fn s(x: f64) -> Tick {
    Tick::from_seconds_f64(x)
}

fn comp(p: &mut Project, name: &str) -> (ItemId, Comp) {
    let c = Comp::new(100, 80, FrameRate::FPS_30, s(4.0));
    (p.add_item(name, Label::Sandstone, None, ItemKind::Comp(c.clone().into())), c)
}

fn project() -> Project {
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc32;
    p
}

fn solid(p: &mut Project, c: &Comp, color: [f32; 3]) -> Layer {
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color, width: 40, height: 30, pixel_aspect: 1.0 }));
    build::layer(p, c, "S", LayerSource::Solid { item: sid }, (40, 30), None)
}

fn blur(p: &mut Project, l: &mut Layer, amount: f64) {
    let spec = effectcraft_effects::find("ec.blur.gaussian").unwrap();
    let mut next = p.next_id;
    let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [40.0, 30.0]);
    p.next_id = next;
    g.prop_mut("blurriness").unwrap().value = Value::Scalar(amount);
    l.props.sub_mut("effects").unwrap().children.push(g.into());
}

fn moving(l: &mut Layer) {
    l.props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([20.0, 20.0, 0.0])), Keyframe::new(s(2.0), Value::Vec3([80.0, 60.0, 0.0]))];
}

/// An outer comp moving a precomp of a blurred solid. Returns (project, outer, inner).
fn precomp_scene() -> (Project, ItemId, ItemId) {
    let mut p = project();
    let (iid, inner) = comp(&mut p, "Inner");
    let mut l = solid(&mut p, &inner, [1.0, 0.5, 0.0]);
    blur(&mut p, &mut l, 6.0);
    p.comp_mut(iid).unwrap().layers.push(l);
    let (oid, outer) = comp(&mut p, "Outer");
    let mut pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (100, 80), None);
    moving(&mut pre);
    p.comp_mut(oid).unwrap().layers.push(pre);
    (p, oid, iid)
}

fn frame(p: &Project, cid: ItemId, t: f64, footage: &dyn FootageSource, expr: Option<&dyn ExprHost>, cache: Option<&LayerCache>) -> Image {
    let mut r = Renderer::new(p, footage, RenderOpts::default());
    r.cache = cache;
    r.expr = expr;
    r.comp_frame(cid, s(t))
}

fn assert_same(a: &Image, b: &Image, what: &str) {
    assert_eq!((a.width, a.height), (b.width, b.height), "{what}");
    for (i, (p, q)) in a.data.iter().zip(&b.data).enumerate() {
        assert!((0..4).all(|c| (p[c] - q[c]).abs() < 1e-6), "{what}: pixel {i}: {p:?} vs {q:?}");
    }
}

#[test]
fn a_static_precomp_renders_its_nested_comp_once() {
    let (p, oid, _) = precomp_scene();
    let cache = LayerCache::default();
    let first = frame(&p, oid, 0.0, &NoFootage, None, Some(&cache));
    assert_same(&first, &frame(&p, oid, 0.0, &NoFootage, None, None), "t=0");
    let rendered = cache.inserts();
    assert!(rendered >= 2, "the nested solid and the precomp were kept: {rendered}");
    for t in [0.5, 1.0, 1.5] {
        let before = cache.stats();
        let img = frame(&p, oid, t, &NoFootage, None, Some(&cache));
        assert_same(&img, &frame(&p, oid, t, &NoFootage, None, None), &format!("t={t}"));
        let after = cache.stats();
        // One lookup: the precomp's buffer (the nested comp isn't drawn again).
        assert_eq!((after.hits - before.hits, after.misses - before.misses), (1, 0), "t={t}");
    }
    assert_eq!(cache.inserts(), rendered, "nothing rendered again");
}

#[test]
fn an_animated_nested_layer_still_updates() {
    let (mut p, oid, iid) = precomp_scene();
    moving(&mut p.comp_mut(iid).unwrap().layers[0]);
    let cache = LayerCache::default();
    let mut last: Option<Image> = None;
    for t in [0.0, 0.5, 1.0, 0.5] {
        let img = frame(&p, oid, t, &NoFootage, None, Some(&cache));
        assert_same(&img, &frame(&p, oid, t, &NoFootage, None, None), &format!("t={t}"));
        if let Some(prev) = &last {
            assert_ne!(prev.data, img.data, "t={t}: the nested layer moved");
        }
        last = Some(img);
    }
}

/// An expression whose result comes from outside the nested comp (as one reading another
/// comp's layer would): the value it returns is part of the key.
struct Outside(AtomicU64);
impl ExprHost for Outside {
    fn eval(&self, _: &EvalCtx, _: &Layer, _: &Property, _: &Value) -> Result<Value, String> {
        Ok(Value::Scalar(f64::from_bits(self.0.load(Ordering::SeqCst))))
    }
}

#[test]
fn a_nested_expression_reading_outside_is_followed() {
    let (mut p, oid, iid) = precomp_scene();
    p.comp_mut(iid).unwrap().layers[0].props.prop_mut("effects/#1/blurriness").unwrap().expr =
        Some(Expression { text: "comp(\"Outer\").layer(1).transform.opacity".into(), enabled: true });
    let host = Outside(AtomicU64::new(2.0f64.to_bits()));
    let cache = LayerCache::default();
    let a = frame(&p, oid, 0.0, &NoFootage, Some(&host), Some(&cache));
    host.0.store(12.0f64.to_bits(), Ordering::SeqCst);
    let b = frame(&p, oid, 0.0, &NoFootage, Some(&host), Some(&cache));
    assert_same(&b, &frame(&p, oid, 0.0, &NoFootage, Some(&host), None), "after the outside value changed");
    assert_ne!(a.data, b.data, "the blur follows the expression");
}

#[test]
fn editing_a_nested_layer_in_place_rerenders_the_precomp() {
    let (mut p, oid, iid) = precomp_scene();
    let cache = LayerCache::default();
    frame(&p, oid, 0.5, &NoFootage, None, Some(&cache));
    // The same project edited in place (the layer's and the comp's allocations are not shared
    // with another snapshot): the memo must not take them for the old ones.
    p.comp_mut(iid).unwrap().layers[0].props.prop_mut("effects/#1/blurriness").unwrap().value = Value::Scalar(20.0);
    let img = frame(&p, oid, 0.5, &NoFootage, None, Some(&cache));
    assert_same(&img, &frame(&p, oid, 0.5, &NoFootage, None, None), "after the edit");
    let k = |p: &Project| {
        let c = p.comp(iid).unwrap();
        let ctx = EvalCtx::new(p, iid, c, s(0.5));
        crate::cache::layer_key(&ctx, &c.layers[0], 1.0, false, false)
    };
    let before = k(&p);
    assert_eq!(before, k(&p), "an unchanged layer keeps its key");
    p.comp_mut(iid).unwrap().layers[0].props.prop_mut("effects/#1/blurriness").unwrap().value = Value::Scalar(3.0);
    assert_ne!(before, k(&p), "the edit changes the key");
}

/// A still that counts its decodes (and takes a while, so concurrent renders overlap).
struct CountingStill {
    reads: AtomicUsize,
    delay_ms: u64,
}
impl FootageSource for CountingStill {
    fn frame(&self, _: ItemId, f: &Footage, _: Tick) -> Option<Arc<Image>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        Some(Arc::new(Image::filled(f.width, f.height, [0.2, 0.4, 0.8, 1.0])))
    }
}

/// A comp with a blurred still that moves. Returns (project, comp).
fn still_scene() -> (Project, ItemId) {
    let mut p = project();
    let (cid, c) = comp(&mut p, "Comp");
    let f = Footage {
        path: "still.png".into(),
        kind: FootageKind::Still,
        width: 40,
        height: 30,
        pixel_aspect: 1.0,
        has_video: true,
        loop_count: 1,
        ..Default::default()
    };
    let fid = p.add_item("Still", Label::Aqua, None, ItemKind::Footage(f));
    let mut l = build::layer(&mut p, &c, "Still", LayerSource::Footage { item: fid }, (40, 30), None);
    blur(&mut p, &mut l, 8.0);
    moving(&mut l);
    p.comp_mut(cid).unwrap().layers.push(l);
    (p, cid)
}

#[test]
fn a_blurred_still_is_blurred_once() {
    let (p, cid) = still_scene();
    let src = CountingStill { reads: AtomicUsize::new(0), delay_ms: 0 };
    let cache = LayerCache::default();
    for t in [0.0, 0.4, 0.8, 1.2] {
        let img = frame(&p, cid, t, &src, None, Some(&cache));
        let fresh = CountingStill { reads: AtomicUsize::new(0), delay_ms: 0 };
        assert_same(&img, &frame(&p, cid, t, &fresh, None, None), &format!("t={t}"));
    }
    assert_eq!(src.reads.load(Ordering::SeqCst), 1, "decoded and blurred on the first frame only");
    assert_eq!(cache.inserts(), 1);
}

#[test]
fn concurrent_misses_render_once() {
    let (p, cid) = still_scene();
    let src = CountingStill { reads: AtomicUsize::new(0), delay_ms: 150 };
    let cache = LayerCache::default();
    let barrier = std::sync::Barrier::new(4);
    let imgs: Vec<Image> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..4)
            .map(|i| {
                let (p, src, cache, barrier) = (&p, &src, &cache, &barrier);
                sc.spawn(move || {
                    barrier.wait();
                    frame(p, cid, 0.1 * i as f64, src, None, Some(cache))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(src.reads.load(Ordering::SeqCst), 1, "one thread rendered the layer, the others waited for it");
    assert_eq!(cache.inserts(), 1);
    for (i, img) in imgs.iter().enumerate() {
        let fresh = CountingStill { reads: AtomicUsize::new(0), delay_ms: 0 };
        assert_same(img, &frame(&p, cid, 0.1 * i as f64, &fresh, None, None), &format!("thread {i}"));
    }
}

#[test]
fn an_animated_precomp_is_keyed_on_every_render() {
    let (mut p, oid, iid) = precomp_scene();
    moving(&mut p.comp_mut(iid).unwrap().layers[0]);
    let cache = LayerCache::default();
    let mut lookups = vec![];
    for f in 0..12 {
        let before = cache.stats();
        frame(&p, oid, f as f64 / 30.0, &NoFootage, None, Some(&cache));
        let after = cache.stats();
        lookups.push(after.hits + after.misses - before.hits - before.misses);
    }
    // Every frame looks up the nested solid (its content is static) and the precomp: PC1 keys a
    // precomp on every render, missed or not (keying costs far less than drawing an unchanged
    // precomp again because its key was no longer tried; lab X1, 23 Oct 2026).
    assert_eq!(lookups, [2; 12], "{lookups:?}");
}

/// The blended layer (a cached solid) follows the layer its Blend effect reads (before, its
/// key named that layer but not its content).
#[test]
fn an_effect_reading_another_layer_follows_it() {
    let mut p = project();
    let (cid, c) = comp(&mut p, "C");
    let mut src = solid(&mut p, &c, [0.0, 0.0, 1.0]);
    src.switches.video = false;
    let src_id = src.id.0;
    let mut top = solid(&mut p, &c, [1.0, 0.0, 0.0]);
    let spec = effectcraft_effects::find("ec.channel.blend").unwrap();
    let mut next = p.next_id;
    let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [40.0, 30.0]);
    p.next_id = next;
    g.prop_mut("blendWithLayer").unwrap().value = Value::Layer(Some(src_id));
    top.props.sub_mut("effects").unwrap().children.push(g.into());
    p.comp_mut(cid).unwrap().layers.push(top);
    p.comp_mut(cid).unwrap().layers.push(src);
    let cache = LayerCache::default();
    let a = frame(&p, cid, 0.0, &NoFootage, None, Some(&cache));
    let LayerSource::Solid { item } = p.comp(cid).unwrap().layers[1].source else { panic!("a solid") };
    if let ItemKind::Solid(s) = &mut p.item_mut(item).unwrap().kind {
        s.color = [0.0, 1.0, 0.0];
    }
    let b = frame(&p, cid, 0.0, &NoFootage, None, Some(&cache));
    let fresh = frame(&p, cid, 0.0, &NoFootage, None, None);
    assert_same(&b, &fresh, "after the read layer changed");
    assert_ne!(a.get(50, 40), b.get(50, 40));
}

/// Motion blur samples the nested layers within the shutter, so the nested time is part of the
/// key (and with an expression in the nested comp, the precomp isn't keyed at all).
#[test]
fn nested_motion_blur_is_keyed_by_time() {
    for expr in [false, true] {
        let (mut p, oid, iid) = precomp_scene();
        let inner = p.comp_mut(iid).unwrap();
        inner.enable_motion_blur = true;
        let l = &mut inner.layers[0];
        l.switches.motion_blur = true;
        moving(l);
        if expr {
            l.props.prop_mut("transform/rotation").unwrap().expr = Some(Expression { text: "time * 10".into(), enabled: true });
        }
        // The outer precomp layer holds still: only the nested content moves.
        p.comp_mut(oid).unwrap().layers[0].props.prop_mut("transform/position").unwrap().keys.clear();
        let host = Outside(AtomicU64::new(5.0f64.to_bits()));
        let cache = LayerCache::default();
        for t in [0.0, 0.5, 1.0, 0.5] {
            let img = frame(&p, oid, t, &NoFootage, Some(&host), Some(&cache));
            assert_same(&img, &frame(&p, oid, t, &NoFootage, Some(&host), None), &format!("expression {expr}, t={t}"));
        }
    }
}

/// Admission: a layer whose effect animates has a new key every frame. Played through, none of
/// its buffers is admitted to the cache (they would fill the budget and never hit): only the
/// probation window holds the latest, and the frames are unchanged.
#[test]
fn per_frame_buffers_are_not_kept() {
    let mut p = project();
    let (cid, c) = comp(&mut p, "Comp");
    let mut l = solid(&mut p, &c, [0.2, 0.6, 1.0]);
    blur(&mut p, &mut l, 2.0);
    l.props.prop_mut("effects/#1/blurriness").unwrap().keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(2.0)), Keyframe::new(s(2.0), Value::Scalar(12.0))];
    p.comp_mut(cid).unwrap().layers.push(l);
    // A window smaller than two of its buffers.
    let cache = LayerCache::new(512 << 10);
    for f in 0..30 {
        let t = f as f64 / 30.0;
        let img = frame(&p, cid, t, &NoFootage, None, Some(&cache));
        assert_same(&img, &frame(&p, cid, t, &NoFootage, None, None), &format!("t={t}"));
    }
    assert_eq!(cache.inserts(), 30, "rendered every frame");
    let (admitted, probation, bytes) = cache.admission();
    assert_eq!(admitted, 0, "nothing admitted");
    assert!(probation >= 1 && bytes <= cache.window().max(cache.stats().bytes / probation), "{probation} on probation, {bytes} bytes");
    assert!(cache.stats().bytes <= 2 * cache.window(), "{:?}", cache.stats());
}

/// A static layer is admitted on its first hit; one flushed from the probation window before
/// that (a busy frame) is admitted when it is rendered the second time.
#[test]
fn static_layers_are_admitted_on_second_sight() {
    let mut p = project();
    let (cid, c) = comp(&mut p, "Comp");
    for (i, color) in [[1.0, 0.5, 0.0], [0.0, 0.5, 1.0]].into_iter().enumerate() {
        let mut l = solid(&mut p, &c, color);
        blur(&mut p, &mut l, 4.0 + i as f64);
        p.comp_mut(cid).unwrap().layers.push(l);
    }
    // Plenty of room: the second frame hits both on probation and admits them.
    let cache = LayerCache::default();
    frame(&p, cid, 0.0, &NoFootage, None, Some(&cache));
    assert_eq!(cache.admission().0, 0);
    assert_eq!(cache.admission().1, 2);
    frame(&p, cid, 0.5, &NoFootage, None, Some(&cache));
    assert_eq!((cache.inserts(), cache.admission().0), (2, 2), "hit on probation, admitted, not rendered again");
    // A window that holds one buffer: the first layer is flushed by the second, so the next
    // frame renders it again (and admits it at once), the frame after hits both.
    let cache = LayerCache::new(256 << 10);
    let want = frame(&p, cid, 0.0, &NoFootage, None, None);
    for (t, inserts) in [(0.0, 2), (0.5, 3), (1.0, 3), (1.5, 3)] {
        assert_same(&frame(&p, cid, t, &NoFootage, None, Some(&cache)), &want, &format!("t={t}"));
        assert_eq!(cache.inserts(), inserts, "t={t}");
    }
    assert_eq!(cache.admission().0, 2);
}

/// One precomp used twice in a frame: the second instance finds the nested layer the first
/// rendered (on probation), so it is rendered once, and admitted.
#[test]
fn a_precomp_used_twice_in_a_frame_renders_its_content_once() {
    let (mut p, oid, _) = precomp_scene();
    let outer = p.comp(oid).unwrap().clone();
    let LayerSource::Comp { item } = outer.layers[0].source else { panic!("precomp") };
    let second = build::layer(&mut p, &outer, "Inner 2", LayerSource::Comp { item }, (100, 80), None);
    p.comp_mut(oid).unwrap().layers.push(second);
    let cache = LayerCache::default();
    let img = frame(&p, oid, 0.0, &NoFootage, None, Some(&cache));
    assert_same(&img, &frame(&p, oid, 0.0, &NoFootage, None, None), "t=0");
    // The nested solid once, each precomp layer's buffer once.
    assert_eq!(cache.inserts(), 3);
    assert_eq!(cache.admission().0, 1, "the nested solid, hit by the second instance");
    let img = frame(&p, oid, 0.5, &NoFootage, None, Some(&cache));
    assert_same(&img, &frame(&p, oid, 0.5, &NoFootage, None, None), "t=0.5");
    assert_eq!((cache.inserts(), cache.admission().0), (3, 3));
}

/// An animated precomp's buffer has a new key every frame: played forward it isn't kept (only
/// its static nested layer is), rendered again after its frame (scrubbing back) it is admitted,
/// and the frame after that hits it. Pixels match an uncached render throughout.
#[test]
fn animated_precomp_buffers_are_kept_only_when_rendered_again() {
    let (mut p, oid, iid) = precomp_scene();
    moving(&mut p.comp_mut(iid).unwrap().layers[0]);
    let cache = LayerCache::default();
    let check = |t: f64, inserts: u64, entries: usize| {
        let img = frame(&p, oid, t, &NoFootage, None, Some(&cache));
        assert_same(&img, &frame(&p, oid, t, &NoFootage, None, None), &format!("t={t}"));
        assert_eq!((cache.inserts(), cache.stats().entries), (inserts, entries), "t={t}");
    };
    // The nested solid only (on probation).
    check(0.0, 1, 1);
    // Rendered again after its frame: the solid hits (admitted), the precomp's buffer is admitted.
    check(0.0, 2, 2);
    assert_eq!(cache.admission().0, 2);
    // A hit: nothing rendered.
    check(0.0, 2, 2);
}

/// The probation window follows the frame size (two frames' worth, at least 16 MB, at most
/// 1/16 of the budget and 128 MB).
#[test]
fn probation_window_follows_the_frame() {
    let cache = LayerCache::default();
    assert_eq!(cache.window(), 64 << 20, "before any frame: the budget's");
    drop(cache.begin_frame(1280, 720));
    assert_eq!(cache.window(), 1280 * 720 * 16 * 2, "720p");
    drop(cache.begin_frame(3840, 2160));
    assert_eq!(cache.window(), 64 << 20, "4K: capped by the budget");
    drop(cache.begin_frame(100, 80));
    assert_eq!(cache.window(), 16 << 20, "small frames: the floor");
}

fn tiny(side: u32) -> Arc<effectcraft_effects::Buf> {
    Arc::new(effectcraft_effects::Buf { img: Image::filled(side, side, [0.5; 4]), offset: [0.0; 2], scale: 1.0 })
}

/// One frame's keys against the cache: hits, and a render (insert) for each miss.
fn play(cache: &LayerCache, keys: std::ops::Range<u64>, side: u32) -> usize {
    let _frame = cache.begin_frame(64, 64);
    let mut hits = 0;
    for k in keys {
        if cache.get(k).is_some() {
            hits += 1;
        } else {
            cache.insert(k, tiny(side));
        }
    }
    hits
}

/// Under count pressure first renders on probation are evicted before admitted entries:
/// a burst of one-shot keys (an animated layer, a busy frame) leaves what was admitted.
#[test]
fn eviction_drops_probation_entries_before_admitted_ones() {
    let cache = LayerCache::default();
    let admitted = (crate::retained::MAX_RECORDS - 64) as u64;
    for k in 0..admitted {
        cache.insert(k, tiny(1));
        assert!(cache.get(k).is_some(), "admitted on its hit");
    }
    assert_eq!(cache.admission().0, admitted as usize);
    // Far more one-shot keys than free record slots.
    for k in 0..1000 {
        cache.insert(1_000_000 + k, tiny(1));
    }
    assert_eq!(cache.admission().0, admitted as usize, "no admitted entry was evicted");
    assert!((0..admitted).all(|k| cache.get(k).is_some()));
    assert!(cache.get(1_000_000).is_none(), "the oldest one-shot entry went first");
    assert!(cache.get(1_000_999).is_some(), "the newest one-shot entry is kept");
}

/// A frame touching more keys than the cache holds (records): least recently used eviction
/// would evict, during each frame, exactly what is used next, so nothing ever hit. The frame's
/// working set is kept instead, and a stable part of it hits on every frame.
#[test]
fn frames_over_the_record_cap_keep_static_hits() {
    let cache = LayerCache::default();
    let keys = 0..6000u64;
    assert_eq!(play(&cache, keys.clone(), 1), 0);
    let mut last = 0;
    for f in 1..6 {
        last = play(&cache, keys.clone(), 1);
        if f >= 3 {
            assert!(last >= crate::retained::MAX_RECORDS - 64, "frame {f}: {last} hits");
        }
    }
    let again = play(&cache, keys.clone(), 1);
    assert_eq!(again, last, "stable from frame to frame");
}

/// The same with bytes: a frame needing twice the budget keeps about half of it hitting.
#[test]
fn frames_over_the_byte_budget_keep_static_hits() {
    // 64×64 RGBA f32 = 64 KiB per buffer; 400 buffers = 25 MiB against a 12 MiB budget.
    let cache = LayerCache::new(12 << 20);
    let keys = 0..400u64;
    play(&cache, keys.clone(), 64);
    let mut hits = Vec::new();
    for _ in 0..5 {
        hits.push(play(&cache, keys.clone(), 64));
    }
    let last = *hits.last().unwrap_or(&0);
    assert!(last >= 120, "{hits:?}: a stable part of the frame hits");
    assert_eq!(hits[hits.len() - 2], last, "{hits:?}: stable");
    assert!(cache.retained_stats().live_bytes <= 12 << 20);
}

/// Outside a frame (no working set to keep) eviction stays least recently used, and shrinking
/// the budget still evicts what is in use.
#[test]
fn shrinking_the_budget_evicts_even_the_working_set() {
    let cache = LayerCache::new(12 << 20);
    let _frame = cache.begin_frame(64, 64);
    for k in 0..100 {
        cache.insert(k, tiny(64));
        assert!(cache.get(k).is_some());
    }
    let before = cache.retained_stats().live_bytes;
    assert!(before > 4 << 20);
    cache.set_budget(4 << 20);
    assert!(cache.retained_stats().live_bytes <= 4 << 20, "{before} -> {:?}", cache.retained_stats());
    assert!(cache.get(99).is_some(), "the most recent survive");
    assert!(cache.get(0).is_none(), "the least recent go");
}

/// After a cut the old shot's entries stop being kept: the new shot is admitted from its
/// second frame and hits from its third.
#[test]
fn after_a_cut_the_new_shot_takes_the_budget() {
    let cache = LayerCache::new(12 << 20);
    for _ in 0..3 {
        play(&cache, 0..400, 64);
    }
    let hits: Vec<usize> = (0..4).map(|_| play(&cache, 10_000..10_150, 64)).collect();
    assert_eq!(hits[0], 0);
    assert_eq!(hits[2..], [150, 150], "{hits:?}");
}

/// A key formats an image sequence's length and a sample of its paths, not every path (a
/// 17k-frame sequence printed about a megabyte per key), and still changes with the sequence.
#[test]
fn image_sequence_keys_follow_the_sequence_without_formatting_every_path() {
    let mut p = project();
    let (cid, c) = comp(&mut p, "C");
    let n = 20_000;
    let seq: Vec<String> = (0..n).map(|i| format!("/shots/a/frame_{i:05}.png")).collect();
    let f = Footage {
        path: seq[0].clone(),
        kind: FootageKind::Sequence,
        width: 40,
        height: 30,
        pixel_aspect: 1.0,
        has_video: true,
        loop_count: 1,
        duration: s(600.0),
        sequence: seq,
        ..Default::default()
    };
    assert!(format!("{:?}", crate::cache::FootageKey(&f)).len() < 4096);
    let fid = p.add_item("Seq", Label::Aqua, None, ItemKind::Footage(f));
    let mut l = build::layer(&mut p, &c, "Seq", LayerSource::Footage { item: fid }, (40, 30), None);
    blur(&mut p, &mut l, 4.0);
    p.comp_mut(cid).unwrap().layers.push(l);
    let key = |p: &Project| {
        let c = p.comp(cid).unwrap();
        let ctx = EvalCtx::new(p, cid, c, s(0.5));
        crate::cache::input_key(&ctx, &c.layers[0], 1.0, false, false, 1).unwrap()
    };
    let base = key(&p);
    assert_eq!(base, key(&p), "an unchanged sequence keeps its key");
    let middle = (n - 1) * 8 / 16;
    let changes: Vec<(&str, Box<dyn Fn(&mut Footage)>)> = vec![
        ("first path", Box::new(|f| f.sequence[0] = "/shots/b/frame_00000.png".into())),
        ("last path", Box::new(move |f| f.sequence[n - 1] = "/shots/b/last.png".into())),
        ("a sampled path", Box::new(move |f| f.sequence[middle] = "/shots/b/middle.png".into())),
        ("length", Box::new(|f| drop(f.sequence.pop()))),
        ("another field", Box::new(|f| f.invert_alpha = true)),
        ("the main path", Box::new(|f| f.path = "/shots/b/frame_00000.png".into())),
    ];
    for (what, change) in changes {
        let mut q = p.clone();
        if let ItemKind::Footage(f) = &mut q.item_mut(fid).unwrap().kind {
            change(f);
        }
        assert_ne!(key(&q), base, "changing the {what} changes the key");
    }
    // A proxy that is a sequence counts too.
    let mut q = p.clone();
    let proxy = Footage { kind: FootageKind::Sequence, sequence: vec!["/proxy/0.png".into(), "/proxy/1.png".into()], ..Default::default() };
    q.item_mut(fid).unwrap().proxy = Some(Box::new(effectcraft_project::Proxy { footage: proxy.clone(), enabled: true }));
    let with_proxy = key(&q);
    assert_ne!(with_proxy, base, "a proxy changes the key");
    if let Some(p) = q.item_mut(fid).unwrap().proxy.as_mut() {
        p.footage.sequence[1] = "/proxy/other.png".into();
    }
    assert_ne!(key(&q), with_proxy, "the proxy's sequence changes the key");
}

/// Counts its evaluations, returning a fixed value.
struct Counting(AtomicUsize, f64);
impl ExprHost for Counting {
    fn eval(&self, _: &EvalCtx, _: &Layer, _: &Property, _: &Value) -> Result<Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Scalar(self.1))
    }
}

/// With linear blending a layer's key is computed once per frame (it was computed for the
/// blending-space lookup and again for the render on a miss, running its expressions twice).
#[test]
fn linear_blending_keys_a_layer_once_per_frame() {
    let mut p = project();
    let (cid, c) = comp(&mut p, "C");
    let mut l = solid(&mut p, &c, [1.0, 0.0, 0.0]);
    blur(&mut p, &mut l, 2.0);
    l.props.prop_mut("effects/#1/blurriness").unwrap().expr = Some(Expression { text: "time * 4".into(), enabled: true });
    p.comp_mut(cid).unwrap().layers.push(l);
    // Evaluations of the expression in a first (missing) cached frame.
    let evaluations = |p: &Project| {
        let host = Counting(AtomicUsize::new(0), 3.0);
        let cache = LayerCache::default();
        let img = frame(p, cid, 0.5, &NoFootage, Some(&host), Some(&cache));
        assert_same(&img, &frame(p, cid, 0.5, &NoFootage, Some(&host), None), "cached render");
        host.0.load(Ordering::SeqCst)
    };
    let gamma = evaluations(&p);
    p.settings.blend_linear = true;
    assert_eq!(evaluations(&p), gamma, "linear blending keys the layer once, as gamma blending does");
}
