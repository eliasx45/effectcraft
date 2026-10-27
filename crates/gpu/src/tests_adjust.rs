//! Adjustment layers on the GPU vs the CPU: GPU-only stacks (resident on the device), mixed
//! stacks (CPU effects in between read back and upload), masks, opacity, colour management.

use effectcraft_color::BlendMode;
use effectcraft_keyframe::{ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, ColorSpace, LayerSource, MaskMode, MatteKind, TrackMatte};
use effectcraft_render::{Backend, FxStep, FxTarget, LayerTimes, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::tests::{Pattern, Scene, c, check, compare_at, gpu, n, opts, set, v3};

fn adjustment_scene(depth: BitDepth, stack: &[(&str, Vec<(&str, Value)>)], masked: bool) -> Scene {
    let mut s = Scene::new(depth);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.footage(60, 40);
    set(&mut l, "transform/rotation", Value::Scalar(15.0));
    s.push(l);
    let mut adj = s.solid([1.0, 1.0, 1.0], 80, 50);
    adj.switches.adjustment = true;
    set(&mut adj, "transform/opacity", Value::Scalar(75.0));
    set(&mut adj, "transform/rotation", Value::Scalar(-6.0));
    for (id, vals) in stack {
        s.effect(&mut adj, id, vals);
    }
    if masked {
        let mut next = s.p.next_id;
        let m = build::mask(&mut Ids(&mut next), "Mask 1", ShapePath::ellipse([40.0, 25.0], 60.0, 40.0), MaskMode::Add, [255, 255, 0]);
        s.p.next_id = next;
        adj.props.sub_mut("masks").unwrap().children.push(m.into());
    }
    s.push(adj);
    let top = s.solid([0.2, 0.6, 0.9], 20, 20);
    s.push(top);
    s
}

fn gpu_stack() -> Vec<(&'static str, Vec<(&'static str, Value)>)> {
    vec![
        ("ec.blur.gaussian", vec![("blurriness", n(6.0))]),
        ("ec.color.levels", vec![("inBlack", n(0.1)), ("gamma", n(1.4))]),
        ("ec.color.tint", vec![("black", c(0.1, 0.0, 0.2)), ("amount", n(60.0))]),
    ]
}

#[test]
fn gpu_adjustment_chains_match_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        for masked in [false, true] {
            let s = adjustment_scene(depth, &gpu_stack(), masked);
            check(&format!("gpu adjustment {depth:?} masked {masked}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
            check(&format!("gpu adjustment {depth:?} masked {masked} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
        }
    }
}

#[test]
fn mixed_adjustment_chains_fall_back_per_effect() {
    let mut stack = gpu_stack();
    stack.insert(1, ("ec.distort.rollingshutterrepair", vec![]));
    stack.push(("ec.stylize.glow", vec![("threshold", n(30.0))]));
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let s = adjustment_scene(depth, &stack, true);
        check(&format!("mixed adjustment {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
    }
    // Colour-managed: effects run in the working space, compositing in linear light.
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let mut s = adjustment_scene(depth, &stack, false);
        s.p.settings.working_space = Some(ColorSpace::Rec709);
        s.p.settings.blend_linear = true;
        check(&format!("colour-managed adjustment {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
    }
}

/// A track matte limits where the adjustment shows, as on the CPU (#1201).
#[test]
fn track_mattes_limit_gpu_adjustments_like_the_cpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for kind in [MatteKind::Alpha, MatteKind::AlphaInverted, MatteKind::Luma, MatteKind::LumaInverted] {
            let mut s = adjustment_scene(depth, &gpu_stack(), false);
            let mut matte = s.footage(50, 30);
            matte.switches.video = false;
            set(&mut matte, "transform/rotation", Value::Scalar(10.0));
            let mid = s.push(matte);
            let c = s.p.comp_mut(s.cid).unwrap();
            let adj = c.layers.iter_mut().find(|l| l.switches.adjustment).unwrap();
            adj.track_matte = Some(TrackMatte { layer: mid, kind });
            check(&format!("matted adjustment {kind:?} {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
        }
    }
}

/// Counts where each run of the stack goes.
#[derive(Default)]
struct Probe {
    gpu: usize,
    cpu: usize,
}

impl FxTarget for Probe {
    fn gpu(&mut self, steps: &[FxStep]) -> bool {
        self.gpu += steps.len();
        true
    }
    fn cpu(&mut self, _f: &mut dyn FnMut(effectcraft_effects::Buf) -> effectcraft_effects::Buf) {
        self.cpu += 1;
    }
    fn shift(&mut self, _d: [f64; 2]) {}
}

#[test]
fn adjustment_stacks_split_into_gpu_runs_and_cpu_steps() {
    let Some(g) = gpu() else { return };
    let mut stack = gpu_stack();
    stack.insert(1, ("ec.generate.fractal", vec![]));
    let s = adjustment_scene(BitDepth::Bpc32, &stack, false);
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
    r.accel = Some(g);
    let ctx = r.eval_ctx(s.cid, Tick::ZERO).unwrap();
    let adj = ctx.comp.layers.iter().find(|l| l.switches.adjustment).unwrap();
    let mut p = Probe::default();
    r.run_effects_on(&ctx, adj, true, &mut p);
    assert_eq!((p.gpu, p.cpu), (3, 1));
}

#[test]
fn composite_reads_the_original_adjustment_input_on_gpu() {
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        for mode in [0, 1, 2, 4, 15] {
            for rgb in [false, true] {
                let stack = vec![
                    ("ec.channel.invert", vec![]),
                    ("ec.channel.cccomposite", vec![("compositeOriginal", Value::Enum(mode)), ("opacity", n(65.0)), ("rgbOnly", Value::Bool(rgb))]),
                ];
                let s = adjustment_scene(depth, &stack, true);
                check(&format!("original adjustment {depth:?} mode {mode} rgb {rgb}"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
            }
        }
    }
}

/// Elias's "K Step Stack" glass cards (27 Oct): per card a rounded-rect shape matte and a card
/// face parented to a null, and between them a comp-sized white adjustment solid with Gaussian
/// Blur (Repeat Edge Pixels) + Tint (black and white to white, 12 %) at part opacity whose alpha
/// track matte is the rounded rect. The GPU walk handed every matted adjustment layer to the CPU
/// ("rendered on the CPU (an adjustment layer the GPU can't apply)", 136 ms a frame), a guard
/// left from before the GPU path multiplied the footprint by the track matte. Now it renders on
/// the GPU, with no CPU note, and matches the CPU. (The opacity and transforms are expression
/// driven in his project; here they are static: compare_at renders without an expression host.)
#[test]
fn matted_glass_adjustment_renders_on_the_gpu_like_the_cpu() {
    let glass_scene = |depth: BitDepth| {
        let mut s = Scene::new(depth);
        let bg = s.solid([0.05, 0.08, 0.14], 97, 61);
        s.push(bg);
        let mut ramp = s.footage(97, 61);
        ramp.blend_mode = BlendMode::Add;
        set(&mut ramp, "transform/opacity", Value::Scalar(20.0));
        s.push(ramp);
        let mut blob = s.footage(60, 40);
        set(&mut blob, "transform/rotation", Value::Scalar(15.0));
        s.push(blob);
        let mut null = build::layer(&mut s.p, &s.comp, "K Step 3", LayerSource::Null, (100, 100), None);
        set(&mut null, "transform/position", v3(50.0, 32.0));
        set(&mut null, "transform/scale", Value::Vec3([90.0, 90.0, 100.0]));
        let nid = null.id;
        let mut card = s.solid([0.1, 0.3, 0.7], 50, 14);
        card.parent = Some(nid);
        set(&mut card, "transform/position", v3(50.0, 50.0));
        s.push(card);
        let mut glass = s.solid([1.0, 1.0, 1.0], 97, 61);
        glass.switches.adjustment = true;
        set(&mut glass, "transform/opacity", Value::Scalar(80.0));
        s.effect(&mut glass, "ec.blur.gaussian", &[("blurriness", n(8.0)), ("repeatEdge", Value::Bool(true))]);
        s.effect(&mut glass, "ec.color.tint", &[("black", c(1.0, 1.0, 1.0)), ("white", c(1.0, 1.0, 1.0)), ("amount", n(12.0))]);
        let mut matte = build::layer(&mut s.p, &s.comp, "K Step 3 Matte", LayerSource::Shape, (97, 61), None);
        let mut next = s.p.next_id;
        let mut ids = Ids(&mut next);
        let rect = build::shape_rect(&mut ids, [52.0, 16.0], [0.0, 0.0], 4.0);
        let fill = build::shape_fill(&mut ids, [1.0, 1.0, 1.0, 1.0]);
        let g = build::shape_group(&mut ids, "Rectangle 1", vec![rect, fill]);
        s.p.next_id = next;
        matte.props.sub_mut("contents").unwrap().children.push(g.into());
        matte.parent = Some(nid);
        matte.switches.video = false;
        set(&mut matte, "transform/position", v3(50.0, 50.0));
        glass.track_matte = Some(TrackMatte { layer: matte.id, kind: MatteKind::Alpha });
        let gid = s.push(glass);
        s.push(matte);
        s.push(null);
        (s, gid)
    };
    for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
        let (s, gid) = glass_scene(depth);
        check(&format!("glass adjustment {depth:?}"), compare_at(&s, opts(), Tick::ZERO), 0.0);
        check(&format!("glass adjustment {depth:?} half"), compare_at(&s, RenderOpts { scale: 0.5, ..opts() }, Tick::ZERO), 0.0);
        let Some(g) = gpu() else { return };
        let times = LayerTimes::default();
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..opts() });
        r.accel = Some(g);
        r.times = Some(&times);
        g.render(&r, s.cid, Tick::ZERO).expect("the GPU renders the frame");
        assert_eq!(times.frame_cpu(), None, "{depth:?}: the frame ran on the GPU");
        assert_eq!(times.cpu_reason(s.cid, gid), None, "{depth:?}: the matted adjustment layer ran on the GPU");
    }
}
