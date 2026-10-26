//! Per-dispatch savings keep every bit: dispatches batched into one compute pass and the
//! minification chain kept with a retained layer image render exactly what one pass per dispatch
//! and a chain rebuilt every frame render, and a layer drawn again reuses its chain.
use crate::context::Enc;
use crate::tests::{self, Pattern, Scene, set, v3};
use effectcraft_keyframe::Value;
use effectcraft_project::BitDepth;
use effectcraft_raster::Image;
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

#[test]
fn kept_mip_chains_and_batched_passes_match_rebuilding_per_frame() {
    let Some(g) = tests::gpu() else {
        assert!(!std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"));
        return;
    };
    g.ctx.clear_uploads();
    let mut scene = Scene::new(BitDepth::Bpc32);
    // 400 × 300 drawn at 18 %: two `half` levels (0.36, then 0.72 ≥ ½).
    let mut layer = scene.footage(400, 300);
    set(&mut layer, "transform/scale", Value::Vec3([18.0, 18.0, 100.0]));
    set(&mut layer, "transform/position", v3(47.0, 29.0));
    scene.push(layer);
    let cache = LayerCache::default();
    let mut r = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(g);
    r.cache = Some(&cache);
    let frame = |reuse: bool| -> (Image, u64) {
        g.within_memory(|| {
            let mut e = Enc::with_resident(&g.ctx, cache.gpu_owner());
            e.reuse = reuse;
            let img = crate::walk::render(&mut e, &r, scene.cid, Tick::ZERO).unwrap();
            let pixels = e.download_px(&img).unwrap();
            e.publish_native();
            (pixels, e.mips.passes)
        })
        .unwrap()
    };
    let bits = |img: &Image| img.data.iter().flatten().map(|v| v.to_bits()).collect::<Vec<u32>>();
    // The old path: a pass per dispatch, levels rebuilt (also retains the layer's image).
    let (old, old_passes) = frame(false);
    assert_eq!(old_passes, 2, "the layer is minified through two levels");
    assert!(old.data.iter().any(|p| p[3] > 0.0), "the layer is visible");
    let (first, first_passes) = frame(true);
    assert_eq!(bits(&first), bits(&old), "batched passes and a newly kept chain change no bit");
    assert!(first_passes <= 2);
    let (again, again_passes) = frame(true);
    assert_eq!(again_passes, 0, "a cached layer drawn again builds its chain once");
    assert_eq!(bits(&again), bits(&old), "the kept chain changes no bit");
    let (later, later_passes) = frame(false);
    assert_eq!(later_passes, 2);
    assert_eq!(bits(&later), bits(&old));
    g.ctx.clear_uploads();
}
