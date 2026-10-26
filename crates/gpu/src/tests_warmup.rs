use crate::tests::{Pattern, Scene};
use effectcraft_project::BitDepth;
use effectcraft_render::{Accelerator, Backend, NoFootage, RenderOpts, Renderer};
use effectcraft_time::Tick;

#[test]
fn warmup_all_static_effects_adds_no_pipelines() {
    crate::tests::hold_gpu_lock();
    let gpu = crate::Gpu::try_headless();
    assert!(gpu.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(gpu) = gpu else { return };
    let cold = gpu.static_pipeline_count();
    // Warmup must not change the existing Advanced 3D output. Compare the same
    // GPU before/after warmup, avoiding unrelated CPU/GPU silhouette differences.
    let (project, cid) = crate::tests_adv3d::scene_project();
    let render_advanced = || {
        let mut renderer = Renderer::new(&project, &NoFootage, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        renderer.accel = Some(&gpu);
        gpu.render(&renderer, cid, Tick::ZERO).expect("Advanced 3D frame")
    };
    let advanced_before = render_advanced();
    assert!(advanced_before.data.iter().any(|p| p[3] > 0.0));
    std::thread::scope(|s| {
        let a = s.spawn(|| gpu.warm_pipelines());
        let b = s.spawn(|| gpu.warm_pipelines());
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
    });
    let warmed = gpu.static_pipeline_count();
    assert!(warmed > cold);
    let advanced_after = render_advanced();
    assert_eq!(advanced_after.data, advanced_before.data, "warmup changed Advanced 3D pixels");
    assert_eq!(gpu.static_pipeline_count(), warmed, "Advanced 3D created a pipeline after warmup");
    for id in effectcraft_effects::GPU_EFFECTS {
        let mut scene = Scene::new(BitDepth::Bpc8);
        let mut layer = scene.footage(16, 16);
        scene.effect(&mut layer, id, &[]);
        scene.push(layer);
        let mut renderer = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        renderer.accel = Some(&gpu);
        assert!(gpu.render(&renderer, scene.cid, Tick::ZERO).is_some(), "{id}");
        assert_eq!(gpu.static_pipeline_count(), warmed, "{id} created a static pipeline after warmup");
    }
    gpu.warm_pipelines().unwrap();
    assert_eq!(gpu.static_pipeline_count(), warmed);
}

#[test]
fn warmup_failure_is_cached_and_cpu_path_remains_available() {
    crate::tests::hold_gpu_lock();
    let gpu = crate::Gpu::try_headless();
    assert!(gpu.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(gpu) = gpu else { return };
    assert!(gpu.ctx.adv3d.set(Err("synthetic pipeline failure".into())).is_ok());
    let count = gpu.static_pipeline_count();
    for _ in 0..2 {
        assert_eq!(gpu.warm_pipelines(), Err("synthetic pipeline failure".into()));
    }
    assert_eq!(gpu.static_pipeline_count(), count);
    let (mut project, cid) = crate::tests_adv3d::scene_project();
    project.settings.bit_depth = BitDepth::Bpc32;
    let advanced = crate::tests_adv3d::scene(&project, cid);
    assert!(!advanced.indices.is_empty(), "fixture exercises the failed raster pipeline");
    assert!(gpu.raster_3d(&advanced).is_none(), "failed optional pipeline must decline GPU rasterization");
    let cpu = Renderer::new(&project, &NoFootage, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame(cid, Tick::ZERO);
    let mut renderer = Renderer::new(&project, &NoFootage, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    renderer.accel = Some(&gpu);
    let fallback = renderer.comp_frame(cid, Tick::ZERO);
    assert!(cpu.data.iter().any(|p| p[3] > 0.0));
    assert_eq!((fallback.width, fallback.height), (cpu.width, cpu.height));
    assert!(fallback.data.iter().flatten().all(|v| v.is_finite()), "fallback pixels must be finite");
    let diff = crate::tests::diff(&cpu, &fallback, 1e-3);
    assert_eq!(diff.over, 0, "failed Advanced 3D pipeline fallback differs: max {}", diff.max);
    assert_eq!(gpu.static_pipeline_count(), count, "failed pipeline must not retry compilation");
}

#[test]
fn warmup_particle_failure_falls_back_with_accelerator_attached() {
    crate::tests::hold_gpu_lock();
    let gpu = crate::Gpu::try_headless();
    assert!(gpu.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(gpu) = gpu else { return };
    assert!(gpu.ctx.particles.set(Err("synthetic particle pipeline failure".into())).is_ok());
    for _ in 0..2 {
        assert_eq!(gpu.warm_pipelines(), Err("synthetic particle pipeline failure".into()));
    }
    let count = gpu.static_pipeline_count();
    let mut scene = Scene::new(BitDepth::Bpc32);
    let mut layer = scene.solid([0.0, 0.0, 0.0], 70, 44);
    scene.effect(
        &mut layer,
        "ec.sim.ccparticleworld",
        &[("birthRate", crate::tests::n(2.0)), ("particle/particleType", effectcraft_keyframe::Value::Enum(1)), ("particle/birthSize", crate::tests::n(0.3))],
    );
    scene.push(layer);
    let at = Tick::from_seconds_f64(0.5);
    let cpu = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame(scene.cid, at);
    assert!(cpu.data.iter().any(|p| p[0] > 0.0 || p[1] > 0.0 || p[2] > 0.0), "fixture draws particles over black");
    let mut renderer = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    renderer.accel = Some(&gpu);
    let fallback = renderer.comp_frame(scene.cid, at);
    assert_eq!((fallback.width, fallback.height), (cpu.width, cpu.height));
    assert!(fallback.data.iter().flatten().all(|v| v.is_finite()), "fallback pixels must be finite");
    let diff = crate::tests::diff(&cpu, &fallback, 1e-3);
    assert_eq!(diff.over, 0, "failed particle pipeline fallback differs: max {}", diff.max);
    assert_eq!(gpu.static_pipeline_count(), count, "failed particle pipeline must not retry compilation");
}

#[test]
fn static_warmup_exposes_dynamic_sequence_compilation() {
    crate::tests::hold_gpu_lock();
    let gpu = crate::Gpu::try_headless();
    assert!(gpu.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(gpu) = gpu else { return };
    if !crate::fusion::enabled() {
        return;
    }
    gpu.warm_pipelines().unwrap();
    let static_count = gpu.static_pipeline_count();
    let warmed = gpu.pipeline_count();
    let render = |reverse: bool, amount: f64| {
        let mut scene = Scene::new(BitDepth::Bpc8);
        let mut layer = scene.footage(16, 16);
        let ids = if reverse { ["ec.color.exposure", "ec.color.tint"] } else { ["ec.color.tint", "ec.color.exposure"] };
        for id in ids {
            let values = if id == "ec.color.tint" { vec![("amount", crate::tests::n(amount))] } else { vec![("master/exposure", crate::tests::n(0.2))] };
            scene.effect(&mut layer, id, &values);
        }
        scene.push(layer);
        let mut renderer = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        renderer.accel = Some(&gpu);
        gpu.render(&renderer, scene.cid, Tick::ZERO).expect("two-effect frame")
    };
    let _ = render(false, 30.0);
    assert_eq!(gpu.pipeline_count(), warmed + 1, "first sequence compiles after static warmup");
    let _ = render(false, 70.0);
    assert_eq!(gpu.pipeline_count(), warmed + 1, "parameters reuse the same sequence pipeline");
    let _ = render(true, 30.0);
    assert_eq!(gpu.pipeline_count(), warmed + 2, "different sequence needs another pipeline");
    assert_eq!(gpu.static_pipeline_count(), static_count, "static-only counter hides these creations");
}

#[test]
fn source_free_planner_matches_runtime_runs_and_bounds_work() {
    use effectcraft_effects::Buf;
    use effectcraft_render::{FxStep, FxTarget, Image};
    struct Fake;
    impl Accelerator for Fake {
        fn name(&self) -> String {
            "planner".into()
        }
        fn comp_frame(&self, _: &Renderer, _: effectcraft_project::ItemId, _: Tick) -> Option<Image> {
            panic!("preflight rendered pixels")
        }
        fn supports_effect(&self, id: &str) -> bool {
            matches!(id, "ec.color.tint" | "ec.color.exposure")
        }
        fn effects(&self, _: &[FxStep], _: &Buf, _: Option<f32>) -> Option<Buf> {
            panic!("preflight executed effects")
        }
    }
    #[derive(Default)]
    struct Record {
        runs: Vec<Vec<&'static str>>,
        cpu: usize,
    }
    impl FxTarget for Record {
        fn gpu(&mut self, steps: &[FxStep]) -> bool {
            self.runs.push(steps.iter().map(|s| s.spec.id).collect());
            true
        }
        fn cpu(&mut self, _: &mut dyn FnMut(Buf) -> Buf) {
            self.cpu += 1;
        }
        fn shift(&mut self, _: [f64; 2]) {}
    }
    let mut scene = Scene::new(BitDepth::Bpc8);
    let mut layer = scene.footage(16, 16);
    for id in ["ec.color.tint", "ec.audio.backwards", "ec.blur.gaussian", "ec.color.exposure", "ec.color.tint"] {
        // A blur that blurs: a Gaussian Blur at 0 is skipped as an identity.
        let blur = [("blurriness", effectcraft_keyframe::Value::Scalar(2.0))];
        scene.effect(&mut layer, id, if id == "ec.blur.gaussian" { &blur } else { &[] });
    }
    if let Some(effectcraft_project::Node::Group(g)) = layer.props.sub_mut("effects").unwrap().children.last_mut() {
        g.enabled = false;
    }
    scene.push(layer);
    let mut renderer = Renderer::new(&scene.p, &NoFootage, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    renderer.accel = Some(&Fake);
    let ctx = renderer.eval_ctx(scene.cid, Tick::ZERO).unwrap();
    let layer = renderer.visible_layers(&ctx)[0];
    let mut runtime = Record::default();
    renderer.run_layer_effects_on(&ctx, layer, &mut runtime);
    assert!(runtime.cpu > 0, "fixture includes a CPU effect boundary");
    let mut planned = Vec::new();
    assert!(renderer.visit_gpu_effect_runs(&ctx, layer, 5, &mut |steps| planned.push(steps.iter().map(|s| s.spec.id).collect::<Vec<_>>())));
    assert_eq!(planned, runtime.runs);
    assert_eq!(planned, vec![vec!["ec.color.tint"], vec!["ec.color.exposure"]]);
    assert!(!renderer.visit_gpu_effect_runs(&ctx, layer, 4, &mut |_| panic!("over-budget visitor")));
    assert!(!Renderer::effect_plan_safe(layer, 0));
    let mut expression_layer = layer.clone();
    expression_layer.props.prop_mut("transform/opacity").unwrap().expr =
        Some(effectcraft_project::Expression { text: "sampleImage([0,0])".into(), enabled: true });
    assert!(!renderer.visit_gpu_effect_runs(&ctx, &expression_layer, 5, &mut |_| panic!("expression preflight")));
}

#[test]
fn project_prewarm_prevents_matched_sequence_compile_attempts() {
    crate::tests::hold_gpu_lock();
    let gpu = crate::Gpu::try_headless();
    assert!(gpu.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(gpu) = gpu else { return };
    if !crate::fusion::enabled() {
        return;
    }
    gpu.warm_pipelines().unwrap();
    let mut scene = Scene::new(BitDepth::Bpc8);
    let mut layer = scene.footage(16, 16);
    scene.effect(&mut layer, "ec.color.tint", &[("amount", crate::tests::n(30.0))]);
    scene.effect(&mut layer, "ec.color.exposure", &[("master/exposure", crate::tests::n(0.2))]);
    scene.push(layer);
    let report = gpu.warm_project(&scene.p, scene.cid, Tick::ZERO);
    assert_eq!(report.ready_sequences, 1);
    assert_eq!(report.compile_attempts, 1);
    assert_eq!(report.created, 1);
    let attempts = gpu.fusion_compile_attempts();
    let count = gpu.pipeline_count();
    let mut renderer = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    renderer.accel = Some(&gpu);
    assert!(gpu.render(&renderer, scene.cid, Tick::ZERO).is_some());
    assert_eq!(gpu.fusion_compile_attempts(), attempts);
    assert_eq!(gpu.pipeline_count(), count);
    assert_eq!(gpu.warm_project(&scene.p, scene.cid, Tick::ZERO).compile_attempts, 0);
}

#[test]
fn project_prewarm_preserves_pixels_and_cpu_oracle() {
    crate::tests::hold_gpu_lock();
    let cold = crate::Gpu::try_headless();
    assert!(cold.is_ok() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"));
    let Ok(cold) = cold else { return };
    let warm = crate::Gpu::try_headless().expect("second device on the available adapter");
    if !crate::fusion::enabled() {
        return;
    }
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let mut scene = Scene::new(depth);
        let mut layer = scene.footage(16, 16);
        scene.effect(&mut layer, "ec.color.tint", &[("amount", crate::tests::n(30.0))]);
        scene.effect(&mut layer, "ec.color.exposure", &[("master/exposure", crate::tests::n(0.2))]);
        scene.push(layer);
        let report = warm.warm_project(&scene.p, scene.cid, Tick::ZERO);
        assert_eq!(report.ready_sequences, 1);
        assert_eq!(report.declined_scopes, 0);
        let attempts = warm.fusion_compile_attempts();
        let render = |gpu: &crate::Gpu| {
            let mut renderer = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            renderer.accel = Some(gpu);
            gpu.render(&renderer, scene.cid, Tick::ZERO).expect("GPU frame")
        };
        let before = render(&cold);
        let after = render(&warm);
        assert_eq!(before.data, after.data, "prewarming changed pixels at {depth:?}");
        assert_eq!(warm.fusion_compile_attempts(), attempts, "render compiled after warmup");
        let cpu = Renderer::new(&scene.p, &Pattern, RenderOpts { backend: Backend::Cpu, ..Default::default() }).comp_frame(scene.cid, Tick::ZERO);
        assert_eq!((cpu.width, cpu.height), (after.width, after.height));
        let tolerance = if depth == BitDepth::Bpc8 { 1.0_f64 / 255.0 } else { 1e-3 };
        let mut max_error = 0.0_f64;
        for (expected, actual) in cpu.data.iter().flatten().zip(after.data.iter().flatten()) {
            assert!(expected.is_finite() && actual.is_finite());
            max_error = max_error.max((f64::from(*expected) - f64::from(*actual)).abs());
        }
        assert!(max_error <= tolerance, "{depth:?}: absolute error {max_error} exceeds {tolerance}");
    }
}
