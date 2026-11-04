#![cfg(all(test, not(target_arch = "wasm32")))]
use crate::{
    EffectcraftApp,
    frames::{FrameImage, display_color_image},
};
use effectcraft_engine::{
    Session,
    project::{BitDepth, Footage, FootageKind, ItemId, ItemKind},
    render::{Backend, FootageSource, Image},
    time::Tick,
};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) fn request(app: &EffectcraftApp, display: bool, backend: Backend, frame: i64, scale: f64) -> Arc<egui::ColorImage> {
    let cid = app.session.active_comp_id().unwrap();
    let key = app.frame_key(cid, frame, scale);
    let time = app.session.active_comp().unwrap().frame_rate.tick_of(frame);
    let mut source = app.render_source();
    source.gpu_display = display;
    let mut opts = app.frame_opts(cid, scale);
    opts.backend = backend;
    app.frames.request_urgent(&source, key, cid, time, opts);
    let start = Instant::now();
    loop {
        if let Some(image) = app.frames.get(&key)
            && app.frames.inflight() == 0
        {
            let FrameImage::Cpu(image) = image else { panic!("native worker must return pixels for egui's separate device") };
            return image;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "native frame did not complete");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn native_display_rejects_bad_lengths_without_allocating() {
    assert!(display_color_image(u32::MAX, u32::MAX, &[]).is_none());
    assert!(display_color_image(0, 1, &[]).is_none());
    assert!(display_color_image(2, 2, &[0; 15]).is_none());
    assert!(display_color_image(2, 2, &[0; 17]).is_none());
    let image = display_color_image(1, 1, &[32, 64, 96, 128]).unwrap();
    assert_eq!(image.pixels[0].to_array(), [32, 64, 96, 128]);
}

// Headless Linux can select GL even though the compositor deliberately does not
// support it. Skip only that explicit capability rejection; initialization failures
// on supported backends must still fail this regression test.
fn supported_test_gpu<T>(result: Result<T, String>, required: bool) -> Result<Option<T>, String> {
    match result {
        Ok(gpu) => Ok(Some(gpu)),
        Err(error) if !required && error.ends_with(" (Gl): the GPU compositor needs DirectX 12, Vulkan, Metal or WebGPU") => {
            eprintln!("native display test skipped: {error}");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[test]
fn native_display_backend_skip_preserves_supported_failures() {
    let gl = "llvmpipe (LLVM 20.1.2, 256 bits) (Gl): the GPU compositor needs DirectX 12, Vulkan, Metal or WebGPU";
    assert_eq!(supported_test_gpu::<()>(Err(gl.into()), false), Ok(None));
    assert_eq!(supported_test_gpu::<()>(Err(gl.into()), true), Err(gl.into()));
    for error in ["Metal: device allocation failed", "Vulkan: shader validation failed", "no GPU adapter", "unrecognized backend failure"] {
        assert_eq!(supported_test_gpu::<()>(Err(error.into()), false), Err(error.into()));
    }
    assert_eq!(supported_test_gpu(Ok(7), false), Ok(Some(7)));
    assert_eq!(supported_test_gpu(Ok(7), true), Ok(Some(7)));
}

#[test]
fn native_display_bridge_matches_float_readback_and_survives_retirement() {
    struct Pixels(Arc<Image>);
    impl FootageSource for Pixels {
        fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<Image>> {
            Some(self.0.clone())
        }
    }
    let required = std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() == Ok("1");
    let Some(gpu) = supported_test_gpu(effectcraft_gpu::Gpu::try_headless(), required).unwrap() else { return };
    eprintln!("native display test adapter: {}", effectcraft_engine::render::Accelerator::name(&gpu));
    gpu.warm_pipelines().unwrap();
    let mut s = Session::default();
    s.execute("comp.new", json!({"width":64,"height":32,"duration":3})).unwrap();
    let mut image = Image::new(64, 32);
    for (i, p) in image.data.iter_mut().enumerate() {
        let a = (i % 17) as f32 / 16.0;
        *p = [((i % 37) as f32 / 12.0 - 0.3) * a, (i % 19) as f32 / 18.0 * a, 0.47 * a, a];
    }
    s.footage = Arc::new(Pixels(Arc::new(image)));
    let footage = Footage { kind: FootageKind::Video, width: 64, height: 32, has_video: true, duration: Tick::from_seconds_f64(3.0), ..Default::default() };
    let item = Arc::make_mut(&mut s.project).add_item("synthetic", Default::default(), None, ItemKind::Footage(footage));
    s.execute("layer.addItem", json!({"item":item.0})).unwrap();
    let mut app = EffectcraftApp::new(s);
    app.gpu = Some(gpu.clone());
    app.session.disk_cache = None;
    assert!(app.render_source().gpu_display);
    app.session.state.viewer.exposure = 1.0;
    assert!(!app.render_source().gpu_display);
    app.session.state.viewer.exposure = 0.0;
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        Arc::make_mut(&mut app.session.project).settings.bit_depth = depth;
        for scale in [1.0, 0.5, 0.25] {
            app.frames.clear();
            let before = gpu.context().transfer_stats();
            let old = request(&app, false, Backend::Gpu, 0, scale);
            let mid = gpu.context().transfer_stats();
            app.frames.clear();
            let new = request(&app, true, Backend::Gpu, 0, scale);
            let after = gpu.context().transfer_stats();
            assert_eq!(old.size, new.size);
            for (a, b) in old.pixels.iter().zip(&new.pixels) {
                assert!(a.to_array().iter().zip(b.to_array()).all(|(a, b)| a.abs_diff(b) <= 1), "{depth:?}/{scale}: {a:?} vs {b:?}");
            }
            assert_eq!(mid.readbacks - before.readbacks, 1);
            assert_eq!(after.readbacks - mid.readbacks, 1);
            // The bridge reads 8-bit pixels; the float path a quarter as many bytes more, except
            // in an 8 bpc project, whose frames already read back at 8 bits.
            let (old_bytes, new_bytes) = (mid.readback_bytes - before.readback_bytes, after.readback_bytes - mid.readback_bytes);
            if scale == 1.0 {
                let ratio = if depth == BitDepth::Bpc8 { 1 } else { 4 };
                assert_eq!(old_bytes, ratio * new_bytes, "{depth:?}");
            }
        }
    }
    app.frames.clear();
    let cpu = request(&app, false, Backend::Cpu, 0, 1.0);
    app.frames.clear();
    gpu.context().device_failure_notifier()("synthetic native display retirement".into());
    let retired = request(&app, true, Backend::Gpu, 0, 1.0);
    assert_eq!(retired.pixels, cpu.pixels);
}
