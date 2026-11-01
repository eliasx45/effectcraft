//! Adjustment layers drawn through a window of the comp (`adjust_window`) give the very same
//! bits as on the whole comp: Gaussian / Fast Box Blur blocks, Mosaic tiles (with and without
//! Sharp Colors), colour steps; mattes in the middle and at the comp's edges; 8/16/32 bpc,
//! Full and Half. Stacks the window can't take run on the whole comp as before.

use std::sync::atomic::Ordering;

use effectcraft_color::Label;
use effectcraft_keyframe::{ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemKind, MaskMode, MatteKind, Project, TrackMatte};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::adjust_window::{WHOLE, WINDOWS};
use crate::tests::{Pattern, Scene, assert_same_bits, c, gpu, n, opts, set, v3};

const W: u32 = 960;
const H: u32 = 720;

fn scene(depth: BitDepth, stack: &[(&str, Vec<(&str, Value)>)], matte_at: [f64; 2], matte_size: (u32, u32), kind: Option<MatteKind>) -> Scene {
    let mut p = Project::default();
    p.settings.bit_depth = depth;
    let comp = Comp::new(W, H, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let mut s = Scene { p, cid, comp };
    let bg = s.footage(W, H);
    s.push(bg);
    let mut blob = s.footage(300, 200);
    set(&mut blob, "transform/rotation", Value::Scalar(12.0));
    s.push(blob);
    // A comp-sized adjustment solid, as Elias's glass and mosaic layers.
    let mut adj = s.solid([1.0, 1.0, 1.0], W, H);
    adj.switches.adjustment = true;
    set(&mut adj, "transform/opacity", Value::Scalar(85.0));
    for (id, vals) in stack {
        s.effect(&mut adj, id, vals);
    }
    match kind {
        Some(kind) => {
            let mut matte = s.footage(matte_size.0, matte_size.1);
            matte.switches.video = false;
            set(&mut matte, "transform/position", v3(matte_at[0], matte_at[1]));
            set(&mut matte, "transform/rotation", Value::Scalar(4.0));
            adj.track_matte = Some(TrackMatte { layer: matte.id, kind });
            s.push(adj);
            s.push(matte);
        }
        None => {
            // A layer mask instead: an ellipse around `matte_at`.
            let mut next = s.p.next_id;
            let (mw, mh) = (f64::from(matte_size.0), f64::from(matte_size.1));
            let m = build::mask(&mut Ids(&mut next), "Mask 1", ShapePath::ellipse(matte_at, mw, mh), MaskMode::Add, [255, 255, 0]);
            s.p.next_id = next;
            adj.props.sub_mut("masks").unwrap().children.push(m.into());
            s.push(adj);
        }
    }
    let top = s.solid([0.2, 0.6, 0.9], 40, 40);
    s.push(top);
    s
}

fn render(s: &Scene, o: RenderOpts, whole: bool) -> Option<Image> {
    let g = gpu()?;
    WHOLE.store(whole, Ordering::Relaxed);
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..o });
    r.accel = Some(g);
    let img = g.render(&r, s.cid, Tick::ZERO);
    WHOLE.store(false, Ordering::Relaxed);
    img
}

/// The windowed frame equals the whole-comp frame bit for bit, and the window was used.
fn same(label: &str, s: &Scene, o: RenderOpts) {
    let Some(whole) = render(s, o, true) else { return };
    let before = WINDOWS.load(Ordering::Relaxed);
    let Some(windowed) = render(s, o, false) else { return };
    assert!(WINDOWS.load(Ordering::Relaxed) > before, "{label}: the window wasn't used");
    assert_same_bits(label, &windowed, &whole);
}

fn stacks() -> Vec<(&'static str, Vec<(&'static str, Vec<(&'static str, Value)>)>)> {
    vec![
        // Elias's glass: Gaussian Blur (Repeat Edge Pixels) + Tint.
        (
            "glass",
            vec![
                ("ec.blur.gaussian", vec![("blurriness", n(45.0)), ("repeatEdge", Value::Bool(true))]),
                ("ec.color.tint", vec![("black", c(1.0, 1.0, 1.0)), ("white", c(1.0, 1.0, 1.0)), ("amount", n(12.0))]),
            ],
        ),
        ("mosaic", vec![("ec.stylize.mosaic", vec![("horizontal", n(160.0)), ("vertical", n(90.0))])]),
        ("mosaic uneven", vec![("ec.stylize.mosaic", vec![("horizontal", n(37.0)), ("vertical", n(23.0))])]),
        ("mosaic sharp", vec![("ec.stylize.mosaic", vec![("horizontal", n(53.0)), ("vertical", n(29.0)), ("sharpColors", Value::Bool(true))])]),
        (
            "blur rows then mosaic",
            vec![
                ("ec.blur.gaussian", vec![("blurriness", n(12.0)), ("dimensions", Value::Enum(1))]),
                ("ec.stylize.mosaic", vec![("horizontal", n(64.0)), ("vertical", n(36.0))]),
                ("ec.color.levels", vec![("inBlack", n(0.1)), ("gamma", n(1.4))]),
            ],
        ),
        ("fast box", vec![("ec.blur.fastbox", vec![("radius", n(7.0)), ("iterations", n(3.0))]), ("ec.color.exposure", vec![("master/exposure", n(0.5))])]),
    ]
}

#[test]
fn windowed_adjustments_match_the_whole_comp() {
    for (name, stack) in stacks() {
        for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
            // In the middle, at the top-left and the bottom-right corners.
            for (at, size) in [([330.0, 200.0], (200, 34)), ([30.0, 12.0], (120, 30)), ([940.0, 700.0], (90, 50))] {
                let s = scene(depth, &stack, at, size, Some(MatteKind::Alpha));
                same(&format!("{name} {depth:?} matte at {at:?}"), &s, opts());
                same(&format!("{name} {depth:?} matte at {at:?} half"), &s, RenderOpts { scale: 0.5, ..opts() });
            }
        }
    }
}

#[test]
fn masked_and_luma_matted_adjustments_use_the_window_too() {
    for (name, stack) in stacks().into_iter().take(2) {
        for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
            let s = scene(depth, &stack, [300.0, 150.0], (160, 60), None);
            same(&format!("{name} {depth:?} masked"), &s, opts());
            let s = scene(depth, &stack, [300.0, 150.0], (160, 60), Some(MatteKind::Luma));
            same(&format!("{name} {depth:?} luma matte"), &s, opts());
        }
    }
}

/// An inverted matte shows the adjustment everywhere but the matte; an effect the window can't
/// take (Glow reads past its blur, CC Composite reads the original): the whole comp, as before.
#[test]
fn whole_comp_when_the_window_cannot_help() {
    let Some(_) = gpu() else { return };
    let cases: Vec<(&str, Vec<(&str, Vec<(&str, Value)>)>, MatteKind)> = vec![
        ("inverted", stacks().swap_remove(0).1, MatteKind::AlphaInverted),
        ("glow", vec![("ec.stylize.glow", vec![("threshold", n(30.0))])], MatteKind::Alpha),
        ("cpu effect", vec![("ec.distort.rollingshutterrepair", vec![]), ("ec.color.tint", vec![])], MatteKind::Alpha),
    ];
    for (name, stack, kind) in cases {
        let s = scene(BitDepth::Bpc8, &stack, [330.0, 200.0], (200, 34), Some(kind));
        let before = WINDOWS.load(Ordering::Relaxed);
        let (Some(a), Some(b)) = (render(&s, opts(), false), render(&s, opts(), true)) else { return };
        assert_eq!(WINDOWS.load(Ordering::Relaxed), before, "{name}: drawn through a window");
        assert_same_bits(name, &a, &b);
    }
}

/// Timing (run by hand: `cargo test -p effectcraft-gpu --lib bench_k_step_like -- --ignored
/// --nocapture`): a 1920×1080 comp with five glass (1000×170 matte) and five mosaic (700×56)
/// adjustment layers, as Elias's "K Step Stack", whole comp vs window, frames rendered in turn.
#[test]
#[ignore]
fn bench_k_step_like() {
    let Some(_) = gpu() else { return };
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc8;
    let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let mut s = Scene { p, cid, comp };
    let bg = s.footage(1920, 1080);
    s.push(bg);
    let glass = stacks().swap_remove(0).1;
    let mosaic = stacks().swap_remove(1).1;
    for i in 0..5 {
        let y = 140.0 + 190.0 * f64::from(i);
        for (stack, size, x) in [(&glass, (1000, 170), 760.0), (&mosaic, (700, 56), 1300.0)] {
            let mut adj = s.solid([1.0, 1.0, 1.0], 1920, 1080);
            adj.switches.adjustment = true;
            for (id, vals) in stack.iter() {
                s.effect(&mut adj, id, vals);
            }
            let mut matte = s.footage(size.0, size.1);
            matte.switches.video = false;
            set(&mut matte, "transform/position", v3(x, y));
            adj.track_matte = Some(TrackMatte { layer: matte.id, kind: MatteKind::Alpha });
            s.push(adj);
            s.push(matte);
        }
    }
    // With a layer cache, as the app renders (the mattes' buffers are made once).
    let g = gpu().unwrap();
    let cache = effectcraft_render::cache::LayerCache::new(1 << 30);
    let frame = |whole: bool| {
        WHOLE.store(whole, Ordering::Relaxed);
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let t0 = std::time::Instant::now();
        let _ = g.render(&r, s.cid, Tick::ZERO);
        WHOLE.store(false, Ordering::Relaxed);
        t0.elapsed().as_secs_f64() * 1e3
    };
    let _ = (frame(true), frame(false));
    let (mut whole, mut window) = (Vec::new(), Vec::new());
    for _ in 0..12 {
        window.push(frame(false));
        whole.push(frame(true));
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    eprintln!("k-step-like 1920x1080, 10 adjustment layers: whole comp {:.1} ms, window {:.1} ms (median of 12)", median(&mut whole), median(&mut window));
}

/// Which adjustment layers of a real project use the window, and the frame time both ways (run
/// by hand on a project copy: `EC_ADJ_PROJECT=x.ecproj EC_ADJ_COMP="K Step Stack · Step 3"
/// EC_ADJ_TIME=8 cargo test -p effectcraft-gpu --lib adjust_window_in_a_project -- --ignored
/// --nocapture`; footage stands in as test patterns).
#[test]
#[ignore]
fn adjust_window_in_a_project() {
    let (Ok(path), Ok(name)) = (std::env::var("EC_ADJ_PROJECT"), std::env::var("EC_ADJ_COMP")) else { return };
    let t = Tick::from_seconds_f64(std::env::var("EC_ADJ_TIME").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0));
    let Some(g) = gpu() else { return };
    let p = Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cid = p.items.iter().find(|(_, i)| i.name == name && matches!(i.kind, ItemKind::Comp(_))).map(|(id, _)| *id).unwrap();
    let cache = effectcraft_render::cache::LayerCache::new(1 << 30);
    for scale in [1.0, 0.5] {
        let frame = |whole: bool| {
            WHOLE.store(whole, Ordering::Relaxed);
            let mut r = Renderer::new(&p, &Pattern, RenderOpts { backend: Backend::Gpu, scale, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            let t0 = std::time::Instant::now();
            let img = g.render(&r, cid, t);
            WHOLE.store(false, Ordering::Relaxed);
            (t0.elapsed().as_secs_f64() * 1e3, img)
        };
        let (_, a) = frame(true);
        crate::adjust_window::NOTES.lock().unwrap().clear();
        let before = WINDOWS.load(Ordering::Relaxed);
        let (_, b) = frame(false);
        eprintln!("scale {scale}: {} windowed; whole comp: {:?}", WINDOWS.load(Ordering::Relaxed) - before, crate::adjust_window::NOTES.lock().unwrap());
        if let (Some(a), Some(b)) = (&a, &b) {
            assert_same_bits(&format!("{name} at {scale}"), b, a);
        }
        let (mut whole, mut window) = (Vec::new(), Vec::new());
        for _ in 0..10 {
            window.push(frame(false).0);
            whole.push(frame(true).0);
        }
        whole.sort_by(f64::total_cmp);
        window.sort_by(f64::total_cmp);
        eprintln!("scale {scale}: whole comp {:.1} ms, window {:.1} ms (median of 10)", whole[5], window[5]);
    }
}
