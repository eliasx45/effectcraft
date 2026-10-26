//! A layer whose content stays the same while its styles animate (a solid card whose drop
//! shadow moves): its styles change every frame, so they render on the GPU like an animated
//! layer's, not on the CPU each frame as a static layer's (drawn once and cached) do.

use std::sync::atomic::Ordering;

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build::Ids;
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{BitDepth, Comp, ItemKind, Layer};
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

use crate::tests::{self, Pattern, Scene, n, set};

fn style(s: &mut Scene, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
    let mut next = s.p.next_id;
    st::add_style(l, &mut Ids(&mut next), id, &s.comp.global_light, true).unwrap();
    s.p.next_id = next;
    for (k, v) in vals {
        set(l, &format!("{GROUP}/{id}/{k}"), v.clone());
    }
}

/// A `w`×`h` comp: footage behind a `cw`×`ch` solid card with a drop shadow, an outer glow and a
/// bevel, the shadow's distance animated over the first second (`animated`) or held.
fn card(w: u32, h: u32, cw: u32, ch: u32, animated: bool) -> Scene {
    let mut s = Scene::new(BitDepth::Bpc32);
    let comp = Comp::new(w, h, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    s.cid = s.p.add_item("Card", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    s.comp = comp;
    let bg = s.footage(w, h);
    s.push(bg);
    let mut l = s.solid([0.2, 0.4, 0.8], cw, ch);
    style(&mut s, &mut l, "dropShadow", &[("size", n(24.0)), ("distance", n(10.0))]);
    style(&mut s, &mut l, "outerGlow", &[("size", n(18.0))]);
    style(&mut s, &mut l, "bevelEmboss", &[("size", n(12.0))]);
    if animated {
        let path = format!("{GROUP}/dropShadow/distance");
        l.props.prop_mut(&path).unwrap().keys = vec![Keyframe::new(Tick::ZERO, n(10.0)), Keyframe::new(Tick::from_seconds_f64(1.0), n(40.0))];
    }
    s.push(l);
    s
}

/// Render `frames` frames from frame 1 through one layer cache, as the viewer plays: the time of
/// each (ms) and how many styled the layer on the GPU.
fn play(s: &Scene, frames: i64) -> (Vec<f64>, usize) {
    let Some(g) = tests::gpu() else { return (vec![], 0) };
    let cache = LayerCache::default();
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(g);
    r.cache = Some(&cache);
    let before = crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed);
    let times = (1..=frames)
        .map(|f| {
            let t0 = std::time::Instant::now();
            g.render(&r, s.cid, s.comp.frame_rate.tick_of(f)).expect("renders");
            t0.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    (times, crate::walk::STYLED_ON_GPU.load(Ordering::Relaxed).saturating_sub(before))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// How long a frame of a 1080p comp takes with a static card whose styles animate, and with the
/// same card held (`cargo test -p effectcraft-gpu --lib styles_static_timing -- --ignored
/// --nocapture`).
#[test]
#[ignore = "timing, run by hand"]
fn styles_static_timing() {
    for animated in [false, true] {
        let s = card(1920, 1080, 1200, 700, animated);
        let (times, on_gpu) = play(&s, 30);
        let first = times.first().copied().unwrap_or(0.0);
        eprintln!(
            "{}: first {first:.1} ms, median of the rest {:.1} ms, styled on the GPU {on_gpu} of 30",
            if animated { "animated styles" } else { "held styles" },
            median(times.into_iter().skip(1).collect())
        );
    }
}

/// Played through the layer cache, the card's moving shadow is styled on the GPU every frame,
/// and the frame matches the CPU's; held, it is styled once on the CPU and cached.
#[test]
fn animated_styles_on_static_content_render_on_the_gpu() {
    let Some(g) = tests::gpu() else { return };
    let s = card(240, 140, 120, 70, true);
    let (_, on_gpu) = play(&s, 3);
    assert!(on_gpu >= 3, "styled on the GPU {on_gpu} of 3 frames");
    let held = card(240, 140, 120, 70, false);
    assert_eq!(play(&held, 3).1, 0, "held styles are cached, not styled again");
    let cache = LayerCache::default();
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(g);
    r.cache = Some(&cache);
    let t = s.comp.frame_rate.tick_of(12);
    let _ = g.render(&r, s.cid, s.comp.frame_rate.tick_of(11));
    let img = g.render(&r, s.cid, t).expect("renders");
    let cpu = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame_cpu(s.cid, t);
    tests::check("moving shadow", Some(tests::diff(&cpu, &img, tests::tolerance(BitDepth::Bpc32))), 0.002);
}
