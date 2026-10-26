//! 8 bpc working images as codes (`crate::codes`): where they are used, and that they are not
//! used at 16/32 bpc. Bit-identity with f32 working images is checked by every scene test
//! (`tests::compare_at` against `tests::gpu_f32`).

use std::sync::atomic::Ordering::Relaxed;

use effectcraft_color::BlendMode;
use effectcraft_keyframe::Value;
use effectcraft_project::BitDepth;
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::Gpu;
use crate::codes::Pixels;
use crate::context::{Enc, GpuContext};
use crate::tests::{Pattern, Scene, assert_same_bits, set, v3};

fn context() -> Option<Gpu> {
    crate::tests::gpu()?;
    pollster::block_on(GpuContext::request()).ok().map(Gpu::from_context)
}

fn scene(depth: BitDepth) -> Scene {
    let mut s = Scene::new(depth);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut a = s.footage(40, 30);
    a.blend_mode = BlendMode::Multiply;
    set(&mut a, "transform/rotation", Value::Scalar(21.0));
    set(&mut a, "transform/scale", Value::Vec3([30.0, 35.0, 100.0]));
    s.push(a);
    let mut b = s.solid([0.3, 0.6, 0.9], 30, 20);
    set(&mut b, "transform/position", v3(20.0, 15.0));
    set(&mut b, "transform/opacity", Value::Scalar(60.0));
    s.push(b);
    s
}

#[test]
fn every_codes_variant_builds_on_the_device() {
    let Some(g) = context() else { return };
    let Some(bank) = g.ctx.codes.as_ref() else {
        assert!(!crate::codes::enabled() || std::env::var("EFFECTCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "the test GPU has no codes bank");
        return;
    };
    bank.warm().unwrap();
}

#[test]
fn eight_bpc_frames_are_codes_and_other_depths_are_not() {
    let Some(g) = context() else { return };
    if g.ctx.codes.is_none() {
        return;
    }
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let s = scene(depth);
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(&g);
        let bank = g.ctx.codes.as_ref().unwrap();
        let before = bank.dispatches.load(Relaxed);
        let mut e = Enc::new(&g.ctx);
        let img = crate::walk::render(&mut e, &r, s.cid, Tick::ZERO).unwrap();
        let codes = bank.dispatches.load(Relaxed) - before;
        let out = e.download_px(&img).unwrap();
        e.submit();
        match depth {
            BitDepth::Bpc8 => {
                assert!(matches!(img, Pixels::Codes(_)), "an 8 bpc frame ends as codes");
                assert!(codes >= 3, "the layers composite as codes ({codes} passes)");
            }
            _ => {
                assert!(matches!(img, Pixels::Float(_)), "{depth:?} stays f32");
                assert_eq!(codes, 0, "{depth:?} never touches codes");
            }
        }
        // Read back as codes or floats: the same frame as `Gpu::render`.
        let again = g.render(&r, s.cid, Tick::ZERO).unwrap();
        assert_same_bits(&format!("{depth:?} readback"), &out, &again);
    }
}

#[test]
fn display_frames_of_codes_match_f32_display_frames() {
    let Some(g) = context() else { return };
    let Some(f) = crate::tests::gpu_f32() else { return };
    if g.ctx.codes.is_none() {
        return;
    }
    let s = scene(BitDepth::Bpc8);
    let frame = |g: &Gpu| {
        let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
        r.accel = Some(g);
        let d = g.render_display(&r, s.cid, Tick::ZERO).unwrap();
        // Level 0 (the mip levels are built from it by the same pass either way).
        g.read_display(&d).unwrap()
    };
    assert_eq!(frame(&g), frame(f), "display frames of codes and of f32 differ");
}

#[test]
fn a_kept_codes_canvas_is_retained_without_packing() {
    let Some(g) = context() else { return };
    if g.ctx.codes.is_none() {
        return;
    }
    let s = scene(BitDepth::Bpc8);
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(&g);
    let mut e = Enc::new(&g.ctx);
    let img = crate::walk::render(&mut e, &r, s.cid, Tick::ZERO).unwrap();
    let Pixels::Codes(c) = &img else { panic!("8 bpc frame as codes") };
    assert_eq!(c.bytes(), 97 * 61 * 4, "a quarter of the f32 bytes");
    // Decoding gives the f32 frame's floats.
    let f = img.float(&mut e).unwrap();
    let decoded = e.download(&f).unwrap();
    let direct = e.download_px(&img).unwrap();
    assert_same_bits("decode", &decoded, &direct);
}
