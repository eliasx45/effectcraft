//! Backend::Auto's per-comp CPU / GPU choice (`auto.rs`), with a mock accelerator.

use std::sync::atomic::{AtomicUsize, Ordering};

use effectcraft_color::Label;
use effectcraft_project::build;
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{Accelerator, AutoKey, AutoPick, Backend, NoFootage, RenderOpts, Renderer};

/// An accelerator that takes `delay` per frame (or declines with `handle = false`) and keeps
/// Auto timing history.
struct TimedAccel {
    delay: std::time::Duration,
    handle: bool,
    calls: AtomicUsize,
    pick: AutoPick,
}

impl TimedAccel {
    fn new(ms: u64, handle: bool) -> TimedAccel {
        TimedAccel { delay: std::time::Duration::from_millis(ms), handle, calls: Default::default(), pick: Default::default() }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Accelerator for TimedAccel {
    fn name(&self) -> String {
        "timed".into()
    }
    fn comp_frame(&self, _r: &Renderer, _comp: ItemId, _t: Tick) -> Option<crate::Image> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        // The centre of the comp, as the CPU draws it (a red solid).
        self.handle.then(|| {
            let mut img = crate::Image::new(200, 100);
            img.set(100, 50, [1.0, 0.0, 0.0, 1.0]);
            img
        })
    }
    fn supports_effect(&self, _id: &str) -> bool {
        false
    }
    fn effects(&self, _chain: &[crate::FxStep], _buf: &crate::Buf, _levels: Option<f32>) -> Option<crate::Buf> {
        None
    }
    fn auto_pick(&self) -> Option<&AutoPick> {
        Some(&self.pick)
    }
}

fn project() -> (Project, ItemId) {
    let mut p = Project::default();
    p.settings.gpu_acceleration = true;
    let mut comp = Comp::new(200, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color: [1.0, 0.0, 0.0], width: 50, height: 50, pixel_aspect: 1.0 }));
    let l = build::layer(&mut p, &comp, "Solid", LayerSource::Solid { item: sid }, (50, 50), None);
    comp.layers.push(l);
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.into()));
    (p, cid)
}

/// Render `n` frames with `backend`; how many the accelerator was asked for.
fn frames(p: &Project, cid: ItemId, a: &TimedAccel, backend: Backend, n: usize) -> usize {
    let before = a.calls();
    for _ in 0..n {
        let mut r = Renderer::new(p, &NoFootage, RenderOpts { backend, ..Default::default() });
        r.accel = Some(a);
        let img = r.comp_frame(cid, Tick::ZERO);
        // Whichever compositor ran, the frame is the same.
        assert!(img.get(100, 50)[0] > 0.99 && img.get(100, 50)[3] > 0.99);
    }
    a.calls() - before
}

#[test]
fn auto_backend_moves_light_comps_to_the_cpu() {
    // A light comp on a slow accelerator: after the warm-up (three frames on
    // each side) Auto renders on the CPU, re-measuring the accelerator now and then.
    let (p, cid) = project();
    // Several times the CPU's frame time (unoptimised test builds are slow).
    let t0 = std::time::Instant::now();
    for _ in 0..3 {
        Renderer::new(&p, &NoFootage, RenderOpts::default()).comp_frame(cid, Tick::ZERO);
    }
    let cpu_ms = t0.elapsed().as_millis() as u64 / 3;
    let slow = TimedAccel::new((cpu_ms * 5).max(20), true);
    let n = 6 + 2 * AutoPick::REPROBE as usize;
    let asked = frames(&p, cid, &slow, Backend::Auto, n);
    assert!((3..=5).contains(&asked), "asked the slow accelerator {asked} times in {n} frames");
    let s = slow.pick.stats(AutoKey::new(cid, 1.0, false)).unwrap();
    assert!(s.gpu_ms.unwrap() > s.cpu_ms.unwrap(), "{s:?}");
    // Backend::Gpu ignores the history; Software Only never asks.
    assert_eq!(frames(&p, cid, &slow, Backend::Gpu, 3), 3);
    let mut sw = p.clone();
    sw.settings.gpu_acceleration = false;
    assert_eq!(frames(&sw, cid, &slow, Backend::Auto, 3), 0);
}

#[test]
fn auto_backend_keeps_fast_accelerators() {
    // As fast as the CPU (ties stay on the accelerator): every frame but the CPU warm-up and
    // re-probes.
    let (p, cid) = project();
    let fast = TimedAccel::new(0, true);
    let asked = frames(&p, cid, &fast, Backend::Auto, 40);
    assert!(asked >= 36, "asked {asked} of 40");
}

#[test]
fn auto_backend_falls_back_when_the_accelerator_declines() {
    let (p, cid) = project();
    let declining = TimedAccel::new(0, false);
    let n = 2 * AutoPick::REPROBE as usize;
    let asked = frames(&p, cid, &declining, Backend::Auto, n);
    // Asked first, then only on the periodic re-probes.
    assert!((1..=4).contains(&asked), "asked {asked} times in {n} frames");
}

#[test]
fn auto_pick_follows_changing_costs() {
    let pick = AutoPick::default();
    let key = AutoKey::new(ItemId(7), 0.5, true);
    let cpu = 5.0;
    let mut gpu = 10.0;
    let (mut early, mut late) = (0, 0);
    for frame in 0..400 {
        // The GPU gets faster halfway (say, its uploads became cached).
        if frame == 200 {
            gpu = 1.0;
        }
        let g = pick.choose(key);
        pick.record(key, g, if g { gpu } else { cpu });
        if (100..200).contains(&frame) && g {
            early += 1;
        }
        if frame >= 300 && g {
            late += 1;
        }
    }
    assert!(early <= 3, "{early} of 100 frames on the slower GPU");
    assert!(late >= 97, "{late} of 100 frames on the faster GPU");
    let s = pick.stats(key).unwrap();
    assert!(s.gpu_ms.unwrap() < 2.0 && s.cpu_ms.unwrap() > 4.0, "{s:?}");
    // Keys are separate per comp, scale and path.
    assert!(pick.stats(AutoKey::new(ItemId(7), 1.0, true)).is_none());
    assert!(pick.stats(AutoKey::new(ItemId(7), 0.5, false)).is_none());
}

/// Frames rendered on the CPU in `n` frames of `key` with these per-frame costs.
fn cpu_frames(pick: &AutoPick, key: AutoKey, n: usize, gpu_ms: f64, cpu_ms: f64) -> usize {
    (0..n)
        .filter(|_| {
            let g = pick.choose(key);
            pick.record(key, g, if g { gpu_ms } else { cpu_ms });
            !g
        })
        .count()
}

#[test]
fn auto_pick_probes_the_cpu_rarely_while_the_gpu_is_clearly_faster() {
    // Each CPU frame in a GPU-resident comp reads back the device's layers (a hitch): while
    // the GPU stays clearly faster, the CPU is re-measured less and less often.
    let pick = AutoPick::default();
    let key = AutoKey::new(ItemId(3), 1.0, true);
    let n = 20 * AutoPick::REPROBE as usize;
    let clear = cpu_frames(&pick, key, n, 1.0, 10.0);
    // Warm-up (3) and probes 48, 96, 192, 384, 384… frames apart.
    assert!(clear <= AutoPick::WARMUP as usize + 5, "{clear} CPU frames in {n}");
    assert!(pick.stats(key).unwrap().reprobe > AutoPick::REPROBE);
    // A close race keeps the usual cadence.
    let close = AutoKey::new(ItemId(4), 1.0, true);
    let probes = cpu_frames(&pick, close, n, 1.0, 1.5);
    assert!(probes >= AutoPick::WARMUP as usize + 18, "{probes} CPU frames in {n}");
    // When the CPU becomes faster, a probe finds it within the longest interval.
    let mut flipped = 0;
    for _ in 0..(2 * AutoPick::MAX_REPROBE as usize) {
        let g = pick.choose(key);
        pick.record(key, g, if g { 10.0 } else { 1.0 });
        if !g {
            flipped += 1;
        }
    }
    assert!(flipped > AutoPick::MAX_REPROBE as usize, "stayed on the slower GPU ({flipped} CPU frames)");
}

#[test]
fn auto_pick_skips_cpu_warm_up_at_another_resolution_of_a_gpu_comp() {
    let pick = AutoPick::default();
    let full = AutoKey::new(ItemId(5), 1.0, true);
    cpu_frames(&pick, full, 20, 1.0, 10.0);
    // Adaptive resolution steps to Half: no CPU warm-up frames there.
    let half = AutoKey::new(ItemId(5), 0.5, true);
    assert_eq!(cpu_frames(&pick, half, AutoPick::REPROBE as usize, 0.5, 5.0), 0);
    // The first re-probe measures the CPU there; the history is its own.
    assert_eq!(cpu_frames(&pick, half, 2 * AutoPick::REPROBE as usize, 0.5, 5.0), 1);
    let s = pick.stats(half).unwrap();
    assert_eq!(s.cpu_ms, Some(5.0));
    // Another comp, or a close race, still warms both sides up.
    let other = AutoKey::new(ItemId(6), 0.5, true);
    assert_eq!(cpu_frames(&pick, other, 6, 0.5, 5.0), AutoPick::WARMUP as usize);
}

#[test]
fn playback_keeps_frames_the_cpu_cannot_render_in_time_on_the_gpu() {
    // A long comp: a light section the CPU renders in 5 ms, then a heavy one where a CPU frame
    // takes 1.5 s and a GPU frame 25 ms. Scrubbing the light section taught Auto the CPU.
    let pick = AutoPick::default();
    let key = AutoKey::new(ItemId(8), 1.0, true);
    let budget = 1000.0 / 29.97;
    assert!(cpu_frames(&pick, key, 40, 12.0, 5.0) > 30);
    // Played, the light section stays on the CPU (it fits a frame's time), but for the usual
    // re-probe of the GPU.
    let played = (0..20).filter(|_| !pick.choose_within(key, budget)).count();
    assert!(played >= 19, "{played} of 20 frames on the CPU");
    // The heavy section: its first CPU frame counts in full, the next ones go to the GPU.
    let mut cpu = 0;
    for _ in 0..60 {
        let g = pick.choose_within(key, budget);
        pick.record(key, g, if g { 25.0 } else { 1500.0 });
        cpu += usize::from(!g);
    }
    assert_eq!(cpu, 1, "heavy frames on the CPU");
    assert_eq!(pick.stats(key).unwrap().cpu_ms, Some(1500.0));
    // No CPU warm-up or re-probe during playback, however long it plays.
    let fresh = AutoKey::new(ItemId(9), 1.0, true);
    let probes = (0..10 * AutoPick::MAX_REPROBE).filter(|_| {
        let g = pick.choose_within(fresh, budget);
        pick.record(fresh, g, 25.0);
        !g
    });
    assert_eq!(probes.count(), 0);
    // A GPU that declines the comp still leaves it to the CPU.
    pick.declined(fresh);
    assert!(!pick.choose_within(fresh, budget));
}

#[test]
fn a_gpu_stall_does_not_move_frames_to_the_cpu() {
    let pick = AutoPick::default();
    let key = AutoKey::new(ItemId(10), 1.0, true);
    assert_eq!(cpu_frames(&pick, key, 20, 10.0, 30.0), AutoPick::WARMUP as usize);
    // One GPU frame waits 2 s on a busy machine: it counts at most twice the average.
    pick.record(key, true, 2000.0);
    assert!(pick.stats(key).unwrap().gpu_ms.is_some_and(|g| g < 15.0));
    assert!(pick.choose(key));
}
