//! Static nested compositions reuse Processed pixels; CPU demand remains lazy.
use effectcraft_color::Label;
use effectcraft_keyframe::Keyframe;
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, LayerId, LayerSource, build};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer, cache::GpuStage};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{Pattern, Scene, check, diff, n, set, tolerance, v3};

fn scene(depth: BitDepth, linear: bool) -> (Scene, ItemId, LayerId) {
    let mut scene = Scene::new(depth);
    scene.p.settings.blend_linear = linear;
    let inner = Comp::new(60, 40, FrameRate::FPS_30, Tick::from_seconds_f64(2.));
    let nested = scene.p.add_item("Static inner", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    let mut child = Scene { p: std::mem::take(&mut scene.p), cid: nested, comp: inner };
    let mut solid = child.solid([0.72, 0.31, 0.13], 31, 23);
    set(&mut solid, "transform/position", v3(25., 18.));
    child.push(solid);
    scene.p = child.p;
    let mut layer = build::layer(&mut scene.p, &scene.comp, "Nested", LayerSource::Comp { item: nested }, (60, 40), None);
    // Expansion exercises retained offset and scale, not only the texture dimensions.
    scene.effect(&mut layer, "ec.blur.gaussian", &[("blurriness", n(3.))]);
    let id = scene.push(layer);
    (scene, nested, id)
}

fn key(r: &Renderer, comp: ItemId, layer: LayerId, time: Tick) -> Option<u64> {
    let ctx = r.eval_ctx(comp, time)?;
    let layer = ctx.comp.layers.iter().find(|l| l.id == layer)?;
    r.resident_precomp_key(&ctx, layer).map(|(key, ..)| key)
}

fn assert_pixels(actual: &effectcraft_raster::Image, expected: &effectcraft_raster::Image, depth: BitDepth) {
    assert_eq!((actual.width, actual.height), (expected.width, expected.height));
    let mut d = diff(expected, actual, tolerance(depth));
    d.quantized = depth != BitDepth::Bpc32;
    check("precomp Processed pixels", Some(d), 0.0);
}

#[test]
fn static_precomp_resident_stage_geometry_and_lazy_cpu() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        for linear in [false, true] {
            for scale in [1., 0.5] {
                g.ctx.clear_uploads();
                let (mut scene, _, id) = scene(depth, linear);
                let cache = LayerCache::default();
                let opts = RenderOpts { backend: Backend::Gpu, scale, ..Default::default() };
                let original_key;
                {
                    let mut r = Renderer::new(&scene.p, &Pattern, opts);
                    r.accel = Some(g);
                    r.cache = Some(&cache);
                    original_key = key(&r, scene.cid, id, Tick::ZERO).unwrap();
                    let before = g.ctx.transfer_stats();
                    g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
                    assert_eq!(g.ctx.transfer_stats().readbacks, before.readbacks, "cold precomp stays resident");
                    assert!(cache.gpu_owner().unwrap().resident(original_key, GpuStage::Processed).is_some(), "nested Processed slot was published");
                }
                let layer = scene.p.comp_mut(scene.cid).unwrap().layers.iter_mut().find(|l| l.id == id).unwrap();
                set(layer, "transform/position", v3(43., 26.));
                set(layer, "transform/opacity", n(65.));
                let later = Tick::from_seconds_f64(0.5);
                let mut r = Renderer::new(&scene.p, &Pattern, opts);
                r.accel = Some(g);
                r.cache = Some(&cache);
                assert_eq!(key(&r, scene.cid, id, later), Some(original_key), "placement and time leave static content reusable");
                let before = g.ctx.transfer_stats();
                let display = g.within_memory(|| g.render_display(&r, scene.cid, later)).unwrap().unwrap();
                let after = g.ctx.transfer_stats();
                assert_eq!(after.uploads, before.uploads, "warm precomp has no uploads");
                assert_eq!(after.readbacks, before.readbacks, "warm precomp has no readbacks");
                let actual = g.read_display(&display).unwrap();
                // The ordinary uncached GPU walk remains a display oracle.
                let mut uncached = Renderer::new(&scene.p, &Pattern, opts);
                uncached.accel = Some(g);
                let expected = g.within_memory(|| g.render_display(&uncached, scene.cid, later)).unwrap().unwrap();
                assert_eq!(actual, g.read_display(&expected).unwrap(), "cache reuse does not change GPU display pixels");
                let ctx = r.eval_ctx(scene.cid, later).unwrap();
                let layer = ctx.comp.layers.iter().find(|l| l.id == id).unwrap();
                let before = g.ctx.transfer_stats();
                let retained_before = cache.retained_stats();
                let materialized = r.content_buf(&ctx, layer).unwrap();
                let after = g.ctx.transfer_stats();
                assert_eq!(after.readbacks - before.readbacks, 1, "first CPU demand reads once");
                let cpu_bytes = materialized.img.data.capacity() * std::mem::size_of::<[f32; 4]>();
                assert_eq!(
                    cache.retained_stats().live_bytes,
                    retained_before.live_bytes + cpu_bytes,
                    "CPU demand charges its separate allocation alongside Processed GPU pixels"
                );
                assert!(std::sync::Arc::ptr_eq(&materialized, &r.content_buf(&ctx, layer).unwrap()));
                assert_eq!(g.ctx.transfer_stats().readbacks, after.readbacks);
                let cpu = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..opts });
                let cpu_ctx = cpu.eval_ctx(scene.cid, later).unwrap();
                let expected = cpu.content_buf(&cpu_ctx, layer).unwrap();
                assert_eq!(materialized.offset, expected.offset);
                assert_eq!(materialized.scale, expected.scale);
                assert_pixels(&materialized.img, &expected.img, depth);
            }
        }
    }
}

#[test]
fn static_precomp_key_changes_and_animated_fallback() {
    let (mut scene, nested, id) = scene(BitDepth::Bpc32, false);
    let cache = LayerCache::default();
    let get = |scene: &Scene, scale| {
        let mut r = Renderer::new(&scene.p, &Pattern, RenderOpts { scale, ..Default::default() });
        r.cache = Some(&cache);
        key(&r, scene.cid, id, Tick::ZERO)
    };
    let original = get(&scene, 1.).unwrap();
    assert_ne!(get(&scene, 0.5).unwrap(), original);
    let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
    set(child, "transform/opacity", n(50.));
    assert_ne!(get(&scene, 1.).unwrap(), original, "nested edit changes content identity");
    let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
    let opacity = child.props.prop_mut("transform/opacity").unwrap();
    opacity.keys = vec![Keyframe::new(Tick::ZERO, n(50.)), Keyframe::new(Tick::from_seconds_f64(1.), n(60.))];
    let at = |scene: &Scene, time| {
        let mut r = Renderer::new(&scene.p, &Pattern, RenderOpts::default());
        r.cache = Some(&cache);
        let k = key(&r, scene.cid, id, time);
        let ctx = r.eval_ctx(scene.cid, time).unwrap();
        (k, r.resident_precomp_animated(&ctx, ctx.comp.layers.iter().find(|l| l.id == id).unwrap()))
    };
    let (k0, animated) = at(&scene, Tick::ZERO);
    assert!(animated, "keyframed nested content is admitted only on repeat");
    assert_ne!(k0.unwrap(), at(&scene, Tick::from_seconds_f64(0.5)).0.unwrap(), "animated values change the key");
    let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
    let opacity = child.props.prop_mut("transform/opacity").unwrap();
    opacity.keys.clear();
    opacity.expr = Some(effectcraft_project::Expression { text: "50".into(), enabled: true });
    let (k, animated) = at(&scene, Tick::ZERO);
    assert!(!animated, "a literal expression is static");
    assert_eq!(k, at(&scene, Tick::from_seconds_f64(0.5)).0);
    let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
    child.props.prop_mut("transform/opacity").unwrap().expr = Some(effectcraft_project::Expression { text: "50 + time".into(), enabled: true });
    assert!(at(&scene, Tick::ZERO).1, "a live expression animates");
}

/// Held values (Hold keys, a literal expression) keep a nested comp's key: the GPU walk reuses
/// its Processed pixels once the key repeats, identical to rendering every frame.
#[test]
fn held_precomp_values_reuse_resident_pixels() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for expression in [false, true] {
        g.ctx.clear_uploads();
        let (mut scene, nested, id) = scene(BitDepth::Bpc16, false);
        let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
        let opacity = child.props.prop_mut("transform/opacity").unwrap();
        if expression {
            opacity.expr = Some(effectcraft_project::Expression { text: "[60]".into(), enabled: true });
        } else {
            opacity.keys = vec![Keyframe::new(Tick::ZERO, n(60.)).hold(), Keyframe::new(Tick::from_seconds_f64(1.), n(90.)).hold()];
        }
        let cache = LayerCache::default();
        let opts = RenderOpts { backend: Backend::Gpu, ..Default::default() };
        let mut r = Renderer::new(&scene.p, &Pattern, opts);
        r.accel = Some(g);
        r.cache = Some(&cache);
        let mut uncached = Renderer::new(&scene.p, &Pattern, opts);
        uncached.accel = Some(g);
        let k = key(&r, scene.cid, id, Tick::ZERO).unwrap();
        let resident = || cache.gpu_owner().unwrap().resident(k, GpuStage::Processed);
        let mut kept = None;
        for frame in 0..6 {
            let at = FrameRate::FPS_30.tick_of(frame);
            assert_eq!(key(&r, scene.cid, id, at), Some(k), "held values keep the key");
            let actual = g.within_memory(|| g.render_display(&r, scene.cid, at)).unwrap().unwrap();
            let expected = g.within_memory(|| g.render_display(&uncached, scene.cid, at)).unwrap().unwrap();
            assert_eq!(g.read_display(&actual).unwrap(), g.read_display(&expected).unwrap(), "retention keeps pixels identical");
            // Hold keys animate: the doorkeeper admits the second production. A literal is static.
            match (frame, resident()) {
                (0, r) if !expression => assert!(r.is_none(), "first sight of an animated key stays transient"),
                (_, None) => panic!("held precomp is retained (frame {frame})"),
                (_, Some(t)) => match &kept {
                    None => kept = Some(t),
                    Some(first) => assert!(std::sync::Arc::ptr_eq(first, &t), "later held frames reuse the retained pixels, not a new composite"),
                },
            }
        }
        let later = Tick::from_seconds_f64(1.);
        if !expression {
            assert_ne!(key(&r, scene.cid, id, later), Some(k), "the next hold changes the key");
        }
    }
}

#[test]
fn static_precomp_budget_denial_and_purge_recompute() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for budget in [0, 16 << 20] {
        g.ctx.clear_uploads();
        let (scene, _, id) = scene(BitDepth::Bpc32, true);
        let cache = LayerCache::new(budget);
        let mut r = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let k = key(&r, scene.cid, id, Tick::ZERO).unwrap();
        let first = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
        let first = g.read_display(&first).unwrap();
        assert_eq!(cache.gpu_owner().unwrap().resident(k, GpuStage::Processed).is_some(), budget > 0);
        cache.clear();
        assert!(cache.gpu_owner().unwrap().resident(k, GpuStage::Processed).is_none());
        let again = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
        assert_eq!(first, g.read_display(&again).unwrap());
        if budget == 0 {
            assert_eq!(cache.retained_stats().live_bytes, 0);
        }
    }
}

#[test]
fn animated_admission_preserves_static_parent_under_pressure() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"));
    let Some(g) = g else { return };
    let (mut scene, _, parent) = scene(BitDepth::Bpc8, false);
    let other = Comp::new(60, 40, FrameRate::FPS_30, Tick::from_seconds_f64(2.));
    let animated_comp = scene.p.add_item("Animated", Label::Sandstone, None, ItemKind::Comp(other.clone().into()));
    let mut moving = Scene { p: std::mem::take(&mut scene.p), cid: animated_comp, comp: other };
    let mut layer = moving.solid([0.2, 0.4, 0.7], 60, 40);
    moving.effect(&mut layer, "ec.blur.gaussian", &[("blurriness", n(1.))]);
    let amount = layer.props.prop_mut("effects/#1/blurriness").unwrap();
    amount.keys = vec![Keyframe::new(Tick::ZERO, n(1.)), Keyframe::new(Tick::from_seconds_f64(1.), n(8.))];
    let animated = moving.push(layer);
    scene.p = moving.p;
    let cache = LayerCache::new(160 << 10);
    let opts = RenderOpts { backend: Backend::Gpu, ..Default::default() };
    let mut r = Renderer::new(&scene.p, &Pattern, opts);
    r.accel = Some(g);
    r.cache = Some(&cache);
    let static_key = key(&r, scene.cid, parent, Tick::ZERO).unwrap();
    let first = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
    let first = g.read_display(&first).unwrap();
    assert!(cache.gpu_owner().unwrap().resident(static_key, GpuStage::Processed).is_some());
    let before = g.ctx.transfer_stats();
    for frame in 1..=12 {
        let at = FrameRate::FPS_30.tick_of(frame);
        let ctx = r.eval_ctx(animated_comp, at).unwrap();
        let layer = ctx.comp.layers.iter().find(|l| l.id == animated).unwrap();
        let k = r.resident_content_key(&ctx, layer).unwrap();
        g.within_memory(|| g.render_display(&r, animated_comp, at)).unwrap().unwrap();
        assert!(cache.gpu_owner().unwrap().resident(k, GpuStage::Processed).is_none(), "animated results remain frame-local");
    }
    assert_eq!(g.ctx.transfer_stats().readbacks, before.readbacks, "admission policy does not fall back to CPU pixels");
    assert!(cache.gpu_owner().unwrap().resident(static_key, GpuStage::Processed).is_some(), "inactive static parent survives animated frames");
    let before = g.ctx.transfer_stats();
    let again = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
    assert_eq!(g.ctx.transfer_stats().uploads, before.uploads, "returning to static parent reuses native pixels");
    assert_eq!(first, g.read_display(&again).unwrap());
    let at = FrameRate::FPS_30.tick_of(12);
    let actual = g.within_memory(|| g.render_display(&r, animated_comp, at)).unwrap().unwrap();
    let actual = g.read_display(&actual).unwrap();
    let mut uncached = Renderer::new(&scene.p, &Pattern, opts);
    uncached.accel = Some(g);
    let expected = g.within_memory(|| g.render_display(&uncached, animated_comp, at)).unwrap().unwrap();
    assert_eq!(actual, g.read_display(&expected).unwrap(), "admission does not change animated pixels");
}

fn requests(g: &crate::Gpu) -> u64 {
    let (hits, misses) = g.ctx.pool_stats();
    hits + misses
}

/// A static precomp under a moving layer (the parent push of a real edit): the second frame
/// places the retained pixels without drawing the nested comp or running the precomp's
/// effects again (fewer working textures than an uncached frame), and the display matches
/// the uncached GPU walk exactly.
#[test]
fn static_precomp_under_a_moving_layer_is_composited_once() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    g.ctx.clear_uploads();
    let (mut scene, _, id) = scene(BitDepth::Bpc16, false);
    let layer = scene.p.comp_mut(scene.cid).unwrap().layers.iter_mut().find(|l| l.id == id).unwrap();
    let position = layer.props.prop_mut("transform/position").unwrap();
    position.keys = vec![Keyframe::new(Tick::ZERO, v3(20., 15.)), Keyframe::new(Tick::from_seconds_f64(1.), v3(50., 30.))];
    let cache = LayerCache::default();
    let opts = RenderOpts { backend: Backend::Gpu, ..Default::default() };
    let mut r = Renderer::new(&scene.p, &Pattern, opts);
    r.accel = Some(g);
    r.cache = Some(&cache);
    let mut uncached = Renderer::new(&scene.p, &Pattern, opts);
    uncached.accel = Some(g);
    g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
    for frame in 1..4 {
        let at = FrameRate::FPS_30.tick_of(frame);
        let key0 = key(&r, scene.cid, id, Tick::ZERO);
        assert_eq!(key(&r, scene.cid, id, at), key0, "the move leaves the content key alone");
        let before = (requests(g), g.ctx.transfer_stats());
        let cached = g.within_memory(|| g.render_display(&r, scene.cid, at)).unwrap().unwrap();
        let cached_requests = requests(g) - before.0;
        let after = g.ctx.transfer_stats();
        assert_eq!((after.uploads, after.readbacks), (before.1.uploads, before.1.readbacks), "frame {frame}: nothing uploaded or read back");
        let before = requests(g);
        let expected = g.within_memory(|| g.render_display(&uncached, scene.cid, at)).unwrap().unwrap();
        let uncached_requests = requests(g) - before;
        assert!(cached_requests < uncached_requests, "frame {frame}: {cached_requests} working textures, uncached {uncached_requests}");
        assert_eq!(g.read_display(&cached).unwrap(), g.read_display(&expected).unwrap(), "frame {frame}: same pixels");
    }
}

/// Editing a layer inside the nested comp (a new project snapshot, the cache kept) changes the
/// precomp's key: the next frame shows the edit, never the retained pixels.
#[test]
fn an_edit_inside_a_retained_precomp_shows_on_the_next_frame() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    g.ctx.clear_uploads();
    let (mut scene, nested, id) = scene(BitDepth::Bpc8, true);
    let cache = LayerCache::default();
    let opts = RenderOpts { backend: Backend::Gpu, ..Default::default() };
    let first = {
        let mut r = Renderer::new(&scene.p, &Pattern, opts);
        r.accel = Some(g);
        r.cache = Some(&cache);
        let k = key(&r, scene.cid, id, Tick::ZERO).unwrap();
        let f = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
        assert!(cache.gpu_owner().unwrap().resident(k, GpuStage::Processed).is_some());
        g.read_display(&f).unwrap()
    };
    for (path, value) in [("transform/opacity", n(40.)), ("transform/position", v3(12., 9.))] {
        let child = scene.p.comp_mut(nested).unwrap().layers.get_mut(0).unwrap();
        set(child, path, value);
        let mut r = Renderer::new(&scene.p, &Pattern, opts);
        r.accel = Some(g);
        r.cache = Some(&cache);
        let shown = g.within_memory(|| g.render_display(&r, scene.cid, Tick::ZERO)).unwrap().unwrap();
        let shown = g.read_display(&shown).unwrap();
        let mut uncached = Renderer::new(&scene.p, &Pattern, opts);
        uncached.accel = Some(g);
        let expected = g.within_memory(|| g.render_display(&uncached, scene.cid, Tick::ZERO)).unwrap().unwrap();
        assert_eq!(shown, g.read_display(&expected).unwrap(), "{path}: the edit shows");
        assert_ne!(shown, first, "{path}: not the retained pixels");
    }
}
