//! Skipped work (`cull`): an off-canvas precomp, a layer behind an empty track matte, layers
//! under an opaque full-canvas solid and a second identical precomp instance render exactly as
//! the walk that draws everything, and the skipped work does not happen.
use effectcraft_color::Label;
use effectcraft_project::{BitDepth, Comp, ItemKind, LayerSource, MatteKind, TrackMatte, build};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::cull::{COUNTS, Counts, OFF};
use crate::tests::{Pattern, Scene, n, set, v3};

fn scene(depth: BitDepth, linear: bool) -> Scene {
    let mut s = Scene::new(depth);
    s.p.settings.blend_linear = linear;
    let inner = Comp::new(60, 40, FrameRate::FPS_30, Tick::from_seconds_f64(2.));
    let tile = s.p.add_item("Tile", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    let mut child = Scene { p: std::mem::take(&mut s.p), cid: tile, comp: inner };
    let content = child.footage(60, 40);
    child.push(content);
    s.p = child.p;
    let precomp = |s: &mut Scene, name: &str, x: f64, y: f64| {
        let mut l = build::layer(&mut s.p, &s.comp, name, LayerSource::Comp { item: tile }, (60, 40), None);
        set(&mut l, "transform/position", v3(x, y));
        l
    };
    // Hidden under the opaque solid.
    let under = s.footage(97, 61);
    s.push(under);
    let hidden = precomp(&mut s, "Hidden tile", 30., 20.);
    s.push(hidden);
    // Covers the 97×61 canvas with a margin, at a fractional position (bilinear interior).
    let mut cover = s.solid([0.2, 0.5, 0.7], 120, 80);
    set(&mut cover, "transform/position", v3(48.3, 30.7));
    s.push(cover);
    let off = precomp(&mut s, "Off-canvas tile", 400., -300.);
    s.push(off);
    // Alpha matte through a layer at 0 % opacity: nothing shows.
    let mut matte = s.solid([1., 1., 1.], 40, 30);
    set(&mut matte, "transform/opacity", n(0.));
    let mut matted = s.footage(50, 40);
    matted.track_matte = Some(TrackMatte { layer: matte.id, kind: MatteKind::Alpha });
    s.push(matted);
    s.push(matte);
    // Two instances of the same nested frame, each with its own placement and effects.
    let a = precomp(&mut s, "Tile A", 25., 20.);
    s.push(a);
    let mut b = precomp(&mut s, "Tile B", 70., 40.);
    s.effect(&mut b, "ec.blur.gaussian", &[("blurriness", n(2.))]);
    s.push(b);
    s
}

fn frame(g: &crate::Gpu, s: &Scene, opts: RenderOpts, off: bool) -> (Image, Counts, u64) {
    OFF.with(|c| c.set(off));
    COUNTS.with(|c| c.set(Counts::default()));
    g.ctx.clear_uploads();
    let mut r = Renderer::new(&s.p, &Pattern, opts);
    r.accel = Some(g);
    let before = g.ctx.transfer_stats().uploads;
    let img = g.within_memory(|| g.render(&r, s.cid, Tick::ZERO)).unwrap().expect("the GPU renders the frame");
    let uploads = g.ctx.transfer_stats().uploads - before;
    OFF.with(|c| c.set(false));
    (img, COUNTS.with(std::cell::Cell::get), uploads)
}

#[test]
fn culled_work_renders_identically_and_is_skipped() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for linear in [false, true] {
            for scale in [1., 0.5] {
                let label = format!("{depth:?} linear {linear} scale {scale}");
                let s = scene(depth, linear);
                let opts = RenderOpts { backend: Backend::Gpu, scale, ..Default::default() };
                let (all, none, all_uploads) = frame(g, &s, opts, true);
                let (culled, counts, culled_uploads) = frame(g, &s, opts, false);
                assert_eq!(none, Counts { nested_renders: 4, ..Counts::default() }, "{label}: reference walk skips nothing");
                assert_eq!((all.width, all.height), (culled.width, culled.height), "{label}");
                let same = all.data.iter().zip(&culled.data).all(|(p, q)| p.iter().zip(q).all(|(x, y)| x == y || (x.is_nan() && y.is_nan())));
                assert!(same, "{label}: culled frame differs from the full walk");
                assert_eq!(counts.occluded, 2, "{label}: footage and tile under the solid");
                assert_eq!(counts.off_canvas, 1, "{label}: off-canvas tile");
                assert_eq!(counts.matte, 1, "{label}: empty matte");
                assert_eq!((counts.nested_renders, counts.nested_shared), (1, 1), "{label}: one nested render, shared");
                assert!(culled_uploads < all_uploads, "{label}: skipped layers upload nothing ({culled_uploads} vs {all_uploads})");
            }
        }
    }
}

/// One tile precomp inside two different screens (precomps of the frame's comp): its nested
/// frame renders once and the other screen's instance shares it, and the frame equals the walk
/// that renders every instance.
#[test]
fn a_precomp_in_two_different_comps_renders_once_per_frame() {
    let g = crate::tests::gpu();
    assert!(g.is_some() || !std::env::var("EFFECTCRAFT_REQUIRE_GPU").is_ok_and(|v| v == "1"), "required GPU unavailable");
    let Some(g) = g else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut s = Scene::new(depth);
        let inner = Comp::new(30, 20, FrameRate::FPS_30, Tick::from_seconds_f64(2.));
        let tile = s.p.add_item("Tile", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
        let mut child = Scene { p: std::mem::take(&mut s.p), cid: tile, comp: inner };
        let content = child.footage(30, 20);
        child.push(content);
        s.p = child.p;
        let screen = |s: &mut Scene, name: &str, at: f64, blur: bool| {
            let comp = Comp::new(50, 40, FrameRate::FPS_30, Tick::from_seconds_f64(2.));
            let id = s.p.add_item(name, Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
            let mut sc = Scene { p: std::mem::take(&mut s.p), cid: id, comp };
            let mut l = build::layer(&mut sc.p, &sc.comp, "Tile", LayerSource::Comp { item: tile }, (30, 20), None);
            set(&mut l, "transform/position", v3(at, 18.));
            if blur {
                sc.effect(&mut l, "ec.blur.gaussian", &[("blurriness", n(2.))]);
            }
            sc.push(l);
            s.p = sc.p;
            let mut outer = build::layer(&mut s.p, &s.comp, name, LayerSource::Comp { item: id }, (50, 40), None);
            set(&mut outer, "transform/position", v3(at * 2., 30.));
            outer
        };
        let a = screen(&mut s, "Screen A", 20., false);
        s.push(a);
        let b = screen(&mut s, "Screen B", 30., true);
        s.push(b);
        let opts = RenderOpts { backend: Backend::Gpu, ..Default::default() };
        let (all, _, _) = frame(g, &s, opts, true);
        let (shared, counts, _) = frame(g, &s, opts, false);
        let same = all.data.iter().zip(&shared.data).all(|(p, q)| p.iter().zip(q).all(|(x, y)| x == y || (x.is_nan() && y.is_nan())));
        assert!(same, "{depth:?}: shared frame differs from the full walk");
        assert_eq!((counts.nested_renders, counts.nested_shared), (3, 1), "{depth:?}: two screens and the tile once, shared once");
    }
}
