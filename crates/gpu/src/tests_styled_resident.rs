//! Layer-styled layers whose content stays on the GPU (`walk::draw_styled_resident`): Elias's
//! Tapa card bodies (a rounded-rectangle shape with Noise + Bevel Alpha and an Inner Shadow).
//! Their Noise changes every frame, so every frame used to make the content on the CPU, read the
//! GPU effect run back, crop it on the CPU and upload it again before the GPU styled it. Now the
//! content is made, cropped and styled on the device. The frames must match the old way's (the
//! same content, plan and kernels) and the CPU's, at 8, 16 and 32 bpc, Full and Half, with
//! linear blending, an exterior pass with Knockout, and a card cut by the comp's edge; the grain
//! still moves from frame to frame; and the content is no longer read back.

use std::sync::atomic::Ordering;

use effectcraft_keyframe::Value;
use effectcraft_project::build::{self, Ids};
use effectcraft_project::styles::{self as st, BLENDING, GROUP};
use effectcraft_project::{BitDepth, Layer, LayerSource};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::tests::{Pattern, Scene, check, diff, gpu, n, set, tolerance, v3};
use crate::walk::{NO_STYLED_RESIDENT, STYLED_RESIDENT};

const MULTIPLY: u32 = 3;

fn style(s: &mut Scene, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
    let mut next = s.p.next_id;
    st::add_style(l, &mut Ids(&mut next), id, &s.comp.global_light, true).unwrap();
    s.p.next_id = next;
    for (k, v) in vals {
        set(l, &format!("{GROUP}/{id}/{k}"), v.clone());
    }
}

#[derive(Clone, Copy, Debug)]
enum Variant {
    /// The card as Elias builds it.
    Card,
    /// Scaled up and moved so the comp's edge cuts it (shape clip window, crop, bounds).
    Cut,
    /// A Drop Shadow too (an exterior pass) and Knockout (reads the content).
    Exterior,
    /// Blend Colors Using 1.0 Gamma.
    Linear,
}

/// Over footage, a rotated rounded-rectangle card with Noise (colour, clipped) and Bevel Alpha,
/// and an Inner Shadow (Multiply, 60 %), as the 26-10 card bodies.
fn card(depth: BitDepth, v: Variant) -> Scene {
    let mut s = Scene::new(depth);
    s.p.settings.blend_linear = matches!(v, Variant::Linear);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = build::layer(&mut s.p, &s.comp, "S5 04 X01 Card", LayerSource::Shape, (97, 61), None);
    let mut next = s.p.next_id;
    let mut ids = Ids(&mut next);
    let items = vec![build::shape_rect(&mut ids, [58.0, 34.0], [0.0, 0.0], 7.0), build::shape_fill(&mut ids, [0.93, 0.9, 0.84, 1.0])];
    l.props.sub_mut("contents").unwrap().children.push(build::shape_group(&mut ids, "Card", items).into());
    s.p.next_id = next;
    set(&mut l, "transform/position", v3(48.0, 31.0));
    set(&mut l, "transform/rotation", n(8.0));
    if matches!(v, Variant::Cut) {
        set(&mut l, "transform/position", v3(14.0, 52.0));
        set(&mut l, "transform/scale", Value::Vec3([150.0, 140.0, 100.0]));
    }
    s.effect(&mut l, "ec.noise.noise", &[("amount", n(30.0)), ("color", Value::Bool(true)), ("clip", Value::Bool(true))]);
    s.effect(&mut l, "ec.perspective.bevelalpha", &[("edgeThickness", n(2.0)), ("lightAngle", n(-60.0)), ("lightIntensity", n(0.4))]);
    style(
        &mut s,
        &mut l,
        "innerShadow",
        &[("blendMode", Value::Enum(MULTIPLY)), ("color", Value::Color([0.02, 0.07, 0.15, 1.0])), ("opacity", n(60.0)), ("distance", n(0.0)), ("size", n(9.0))],
    );
    if matches!(v, Variant::Exterior) {
        style(&mut s, &mut l, "dropShadow", &[("size", n(5.0)), ("distance", n(3.0))]);
        set(&mut l, &format!("{GROUP}/{BLENDING}/advancedBlending/knockout"), Value::Enum(1));
    }
    s.push(l);
    s
}

/// Frames 0, 1 and 2 on the GPU through one layer cache (as the viewer plays), the content made
/// on the device or, `old`, the old way; with how many layers styled from device-made content
/// and the bytes read back.
fn play(s: &Scene, o: RenderOpts, old: bool) -> Option<(Vec<Image>, usize, u64)> {
    let g = gpu()?;
    let cache = LayerCache::default();
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..o });
    r.accel = Some(g);
    r.cache = Some(&cache);
    NO_STYLED_RESIDENT.store(old, Ordering::Relaxed);
    let (resident, read) = (STYLED_RESIDENT.load(Ordering::Relaxed), g.ctx.transfer_stats().readback_bytes);
    let frames = (0..3).map(|f| g.render(&r, s.cid, s.comp.frame_rate.tick_of(f)).expect("the GPU renders the frame")).collect();
    NO_STYLED_RESIDENT.store(false, Ordering::Relaxed);
    let resident = STYLED_RESIDENT.load(Ordering::Relaxed).saturating_sub(resident);
    Some((frames, resident, g.ctx.transfer_stats().readback_bytes.saturating_sub(read)))
}

#[test]
fn card_bodies_style_on_the_gpu_from_content_kept_there() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        for v in [Variant::Card, Variant::Cut, Variant::Exterior, Variant::Linear] {
            for scale in [1.0, 0.5] {
                let label = format!("{v:?} {depth:?} at {scale}");
                let s = card(depth, v);
                let o = RenderOpts { scale, ..Default::default() };
                let Some((new, resident, new_read)) = play(&s, o, false) else { return };
                let (old, old_resident, old_read) = play(&s, o, true).unwrap();
                assert_eq!(resident, 3, "{label}: the card's content was made on the device every frame");
                assert_eq!(old_resident, 0, "{label}: the old way still runs when asked for");
                assert!(new_read < old_read, "{label}: the content is no longer read back ({new_read} vs {old_read} bytes)");
                let tol = tolerance(depth);
                for (f, (a, b)) in new.iter().zip(&old).enumerate() {
                    let mut d = diff(b, a, tol);
                    d.quantized = depth != BitDepth::Bpc32;
                    check(&format!("{label}, frame {f}: kept on the GPU vs the old way"), Some(d), 0.0);
                    let t = s.comp.frame_rate.tick_of(f as i64);
                    let cpu = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..o }).comp_frame_cpu(s.cid, t);
                    let mut d = diff(&cpu, a, tol);
                    d.quantized = depth != BitDepth::Bpc32;
                    check(&format!("{label}, frame {f}: kept on the GPU vs the CPU"), Some(d), 0.002);
                }
                // The grain moves: frame 1's card differs from frame 0's.
                let moved = new[0].data.iter().zip(&new[1].data).filter(|(p, q)| p != q).count();
                assert!(moved > 100, "{label}: the Noise grain still changes every frame ({moved} pixels changed)");
            }
        }
    }
}

/// A frame seen again is styled on the CPU and cached, as before (its first visit left a mark);
/// it shows the same picture.
#[test]
fn a_revisited_card_frame_matches_its_first_visit() {
    let Some(g) = gpu() else { return };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let s = card(depth, Variant::Card);
        let cache = LayerCache::default();
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let t = Tick::ZERO;
        let before = STYLED_RESIDENT.load(Ordering::Relaxed);
        let first = g.render(&r, s.cid, t).expect("renders");
        assert_eq!(STYLED_RESIDENT.load(Ordering::Relaxed), before + 1, "{depth:?}: first visit styled from device-made content");
        let again = g.render(&r, s.cid, t).expect("renders");
        let mut d = diff(&first, &again, tolerance(depth));
        d.quantized = depth != BitDepth::Bpc32;
        check(&format!("{depth:?}: revisited card"), Some(d), 0.0);
    }
}

/// Footage as one still per item and size (made once: the timings measure the cards, not the
/// test pattern).
struct Stills(std::sync::Mutex<std::collections::HashMap<(u64, u32, u32), std::sync::Arc<Image>>>);

impl effectcraft_render::FootageSource for Stills {
    fn frame(&self, item: effectcraft_project::ItemId, f: &effectcraft_project::Footage, _t: Tick) -> Option<std::sync::Arc<Image>> {
        let mut m = self.0.lock().ok()?;
        Some(m.entry((item.0, f.width, f.height)).or_insert_with(|| std::sync::Arc::new(crate::tests::pattern(item.0 as u32, f.width, f.height))).clone())
    }
}

/// Play `frames` frames from `from` through one layer cache: the median frame (ms) and the
/// slowest layers by median (name, ms).
fn time_play(p: &effectcraft_project::Project, cid: effectcraft_project::ItemId, from: i64, frames: i64, old: bool) -> (f64, Vec<(String, f64)>) {
    let Some(g) = gpu() else { return (0.0, vec![]) };
    let stills = Stills(Default::default());
    let rate = p.comp(cid).map(|c| c.frame_rate).unwrap_or(effectcraft_time::FrameRate::FPS_30);
    let cache = LayerCache::default();
    NO_STYLED_RESIDENT.store(old, Ordering::Relaxed);
    // The frame before warms the cache (static layers, the first visit's misses).
    {
        let mut r = Renderer::new(p, &stills, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        r.cache = Some(&cache);
        let _ = g.render(&r, cid, rate.tick_of(from - 1));
    }
    let mut ms = vec![];
    let mut layers: std::collections::HashMap<(effectcraft_project::ItemId, effectcraft_project::LayerId), Vec<f64>> = Default::default();
    for f in from..from + frames {
        let times = effectcraft_render::LayerTimes::default();
        {
            let mut r = Renderer::new(p, &stills, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(g);
            r.cache = Some(&cache);
            r.times = Some(&times);
            let t0 = std::time::Instant::now();
            let _ = g.render(&r, cid, rate.tick_of(f));
            ms.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        for (k, v) in times.into_map() {
            layers.entry(k).or_default().push(v);
        }
    }
    NO_STYLED_RESIDENT.store(false, Ordering::Relaxed);
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let name = |(c, l): (effectcraft_project::ItemId, effectcraft_project::LayerId)| {
        p.comp(c).and_then(|c| c.layers.iter().find(|x| x.id == l)).map_or_else(|| format!("{l:?}"), |x| x.name.clone())
    };
    let mut top: Vec<(String, f64)> = layers.into_iter().map(|(k, v)| (name(k), median(v))).collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    top.truncate(6);
    (median(ms), top)
}

/// How long a frame takes with the content kept on the GPU and the old way, interleaved
/// (`cargo test -p effectcraft-gpu --lib styled_resident_timing -- --ignored --nocapture`): a
/// synthetic 1080p comp with a 1000 × 700 card (Inner Shadow 162 px, as HV1 01's month panels),
/// and with `EFFECTCRAFT_STYLED_BENCH=<a copy of 26-10.ecproj>` Elias's S5 04 Four questions
/// (t = 1 s) and HV1 01 Calendar (t = 6 s).
#[test]
#[ignore = "timing, run by hand"]
fn styled_resident_timing() {
    let mut runs: Vec<(String, effectcraft_project::Project, effectcraft_project::ItemId, i64)> = vec![];
    {
        let mut s = card(BitDepth::Bpc8, Variant::Card);
        let comp = effectcraft_project::Comp::new(1920, 1080, effectcraft_time::FrameRate::FPS_30, Tick::from_seconds_f64(4.0));
        let c = s.p.comp_mut(s.cid).unwrap();
        let layers = std::mem::take(&mut c.layers);
        *c = comp;
        c.layers = layers;
        for l in &mut c.layers {
            if matches!(l.source, LayerSource::Shape) {
                set(l, "transform/position", v3(960.0, 540.0));
                set(l, "transform/scale", Value::Vec3([1700.0, 2000.0, 100.0]));
                set(l, &format!("{GROUP}/innerShadow/size"), n(162.0));
            } else {
                set(l, "transform/scale", Value::Vec3([2000.0, 1800.0, 100.0]));
                set(l, "transform/position", v3(960.0, 540.0));
            }
        }
        runs.push(("synthetic 1080p card".into(), s.p, s.cid, 10));
    }
    if let Ok(path) = std::env::var("EFFECTCRAFT_STYLED_BENCH") {
        let json = std::fs::read_to_string(&path).expect("reads the project");
        let p = effectcraft_project::Project::from_json(&json).expect("parses the project");
        for (name, t) in [("S5 04 Four questions", 1.0), ("HV1 01 Calendar", 6.0)] {
            let Some((&cid, _)) = p.items.iter().find(|(id, it)| it.name == name && p.comp(**id).is_some()) else { continue };
            let rate = p.comp(cid).map(|c| c.frame_rate).unwrap();
            runs.push((name.into(), p.clone(), cid, rate.frame_at(Tick::from_seconds_f64(t))));
        }
    }
    for (name, p, cid, from) in &runs {
        for round in 0..3 {
            for old in [true, false] {
                let (ms, top) = time_play(p, *cid, *from + round * 8, 8, old);
                let top: Vec<String> = top.iter().map(|(n, ms)| format!("{n} {ms:.1}")).collect();
                eprintln!("{name} round {round} {}: {ms:.1} ms/frame; layers: {}", if old { "old way " } else { "kept on GPU" }, top.join(", "));
            }
        }
    }
}
