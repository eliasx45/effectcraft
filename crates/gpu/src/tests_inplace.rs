//! Layers drawn onto the canvas in place over their bounds (`ops::warp_q_in`) give bit for bit
//! the pixels of one full-canvas pass per layer (`ops::warp_q`), inside the bounds and out;
//! stencil modes, layers covering most of the canvas and a canvas not yet quantised take the
//! full pass. So do track mattes, blends of isolated layers, motion blur and channel mixes.
use effectcraft_color::BlendMode::{self, Add, Difference, Dissolve, Multiply, Normal, Overlay, Screen, StencilAlpha};
use effectcraft_geom::{Mat3, vec2};
use effectcraft_project::MatteKind;
use effectcraft_raster::Sampling::{self, Bicubic, Bilinear, Nearest};

use crate::context::{Enc, GpuImage};
use crate::ops;
use crate::tests;

struct Step {
    m: Mat3,
    sampling: Sampling,
    mode: BlendMode,
    opacity: f32,
    accumulate: Option<f32>,
    levels: Option<f32>,
    /// Drawn on the canvas's own texture (or nothing drawn at all).
    in_place: bool,
}

fn step(m: Mat3, sampling: Sampling, mode: BlendMode, opacity: f32, levels: Option<f32>, in_place: bool) -> Step {
    Step { m, sampling, mode, opacity, accumulate: None, levels, in_place }
}

/// A 40 × 30 layer centred at (`x`, `y`), rotated and scaled.
fn at(x: f64, y: f64, deg: f64, s: f64) -> Mat3 {
    Mat3::translate(vec2(x, y)) * Mat3::rotate_deg(deg) * Mat3::scale(vec2(s, s)) * Mat3::translate(vec2(-20.0, -15.0))
}

#[test]
fn in_place_layers_match_full_canvas_passes() {
    let Some(g) = tests::gpu() else {
        assert!(std::env::var_os("EFFECTCRAFT_REQUIRE_GPU").is_none());
        return;
    };
    let (w, h) = (160, 120);
    let bg = g.ctx.upload_image(&tests::pattern(3, w, h)).unwrap();
    let layer = g.ctx.upload_image(&tests::pattern(7, 40, 30)).unwrap();
    let q = Some(255.0);
    let steps = [
        // Partial bounds, then hanging off the top-left and the bottom-right edges.
        step(at(50.0, 40.0, 10.0, 1.0), Bilinear, Normal, 0.8, None, true),
        step(at(5.0, 3.0, 30.0, 1.2), Bicubic, Multiply, 1.0, None, true),
        step(at(155.0, 118.0, -20.0, 1.0), Nearest, Screen, 1.0, None, true),
        // Minified (pre-filtered levels), dissolve noise (absolute pixel coordinates).
        step(at(80.0, 60.0, 45.0, 0.2), Bilinear, Normal, 1.0, None, true),
        step(at(70.0, 50.0, 0.0, 0.9), Bilinear, Dissolve, 0.6, None, true),
        // Entirely off the canvas: nothing drawn, the canvas kept.
        step(at(500.0, 500.0, 0.0, 1.0), Bilinear, Normal, 1.0, None, true),
        // Stencil clears outside its bounds; a layer covering the canvas: full passes.
        step(at(60.0, 60.0, 0.0, 1.0), Bilinear, StencilAlpha, 1.0, None, false),
        step(at(80.0, 60.0, 0.0, 5.0), Bilinear, Normal, 0.7, None, false),
        // 8 bpc: the first pass quantises the whole canvas, later ones are in place.
        step(at(40.0, 40.0, 15.0, 1.0), Bilinear, Add, 0.9, q, false),
        step(at(90.0, 70.0, -15.0, 1.0), Bicubic, Normal, 0.9, q, true),
        step(at(30.0, 80.0, 5.0, 0.7), Bilinear, Overlay, 1.0, q, true),
        step(at(100.0, 30.0, 0.0, 0.3), Bicubic, Difference, 1.0, q, true),
        step(at(120.0, 90.0, 0.0, 1.0), Bilinear, Normal, 0.5, None, true),
        // A motion-blur accumulate sample.
        Step { accumulate: Some(0.25), ..step(at(60.0, 70.0, 20.0, 1.0), Bilinear, Normal, 1.0, None, true) },
    ];
    let mut e = Enc::new(&g.ctx);
    let mut full = bg.clone();
    let mut canvas = e.scratch(w, h);
    e.copy_into(&bg, &canvas, 0, 0);
    let mut shots = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        assert!(e.owns(&canvas), "step {i}: the canvas is the encoder's own");
        full = ops::warp_q(&mut e, &full, &layer, &s.m, s.sampling, s.mode, s.opacity, 5, s.accumulate, s.levels);
        let before = canvas.texture.clone();
        ops::warp_q_in(&mut e, &mut canvas, &layer.clone().into(), &s.m, s.sampling, s.mode, s.opacity, 5, s.accumulate, s.levels);
        assert_eq!(canvas.texture == before, s.in_place, "step {i}: in place");
        assert_eq!(canvas.levels, full.levels, "step {i}");
        if let Some(l) = s.levels {
            full = ops::quantize(&mut e, &full, l);
            canvas = ops::quantize(&mut e, &canvas, l);
        }
        // Snapshot by copy: a clone would share the canvas (no longer the encoder's own).
        let shot = e.scratch(w, h);
        e.copy_into(&canvas, &shot, 0, 0);
        shots.push((full.clone(), shot));
    }
    let bits = |img: &effectcraft_raster::Image| img.data.iter().flatten().map(|v| v.to_bits()).collect::<Vec<u32>>();
    for (i, (full, shot)) in shots.iter().enumerate() {
        let (full, shot) = (e.download(full).unwrap(), e.download(shot).unwrap());
        assert!(bits(&full) == bits(&shot), "step {i}: {:?}", tests::diff(&full, &shot, 0.0));
    }
    let start = e.download(&bg).unwrap();
    assert!(tests::diff(&start, &e.download(&full).unwrap(), 1e-3).over > 1000, "the layers draw");
}

/// A copy of `img` the encoder owns (a clone would share it).
fn own(e: &mut Enc, img: &GpuImage) -> GpuImage {
    let out = e.scratch(img.width, img.height);
    e.copy_into(img, &out, 0, 0);
    out
}

fn same_bits(e: &mut Enc, label: &str, full: &GpuImage, in_place: &GpuImage) {
    let bits = |img: &effectcraft_raster::Image| img.data.iter().flatten().map(|v| v.to_bits()).collect::<Vec<u32>>();
    let (full, in_place) = (e.download(full).unwrap(), e.download(in_place).unwrap());
    assert!(bits(&full) == bits(&in_place), "{label}: {:?}", tests::diff(&full, &in_place, 0.0));
}

/// Track mattes, Preserve Transparency, blend modes through `blend_full`, motion blur and
/// layer-style channel mixes run in place over the layer's bounds (`ops::matte_in`,
/// `blend_full_in`, `warp_accumulate_in`, `channel_mix_in`) and give bit for bit the pixels of
/// their full-canvas passes; stencils and a canvas not yet quantised take the full pass.
#[test]
fn in_place_mattes_blends_and_motion_blur_match_full_passes() {
    let Some(g) = tests::gpu() else {
        assert!(std::env::var_os("EFFECTCRAFT_REQUIRE_GPU").is_none());
        return;
    };
    let (w, h) = (160, 120);
    let bg = g.ctx.upload_image(&tests::pattern(3, w, h)).unwrap();
    let layer = g.ctx.upload_image(&tests::pattern(7, 40, 30)).unwrap();
    let mimg = g.ctx.upload_image(&tests::pattern(11, w, h)).unwrap();
    let mut e = Enc::new(&g.ctx);
    let zeros = e.zeros(w, h);
    let m = at(60.0, 50.0, 25.0, 1.1);
    let reach = ops::warp_bounds((zeros.width, zeros.height), (layer.width, layer.height), &m);
    let iso = ops::warp(&mut e, &zeros, &layer, &m, Bilinear, Normal, 1.0, 0, None);
    let q = Some(255.0);
    let bg_q = ops::quantize(&mut e, &bg, 255.0);

    // Track mattes on the isolated layer, then its blend (Preserve Transparency too).
    for kind in [MatteKind::Alpha, MatteKind::AlphaInverted, MatteKind::Luma, MatteKind::LumaInverted] {
        for (mode, preserve) in [(Normal, false), (Multiply, false), (Normal, true)] {
            let label = format!("{kind:?} {mode:?} preserve {preserve}");
            let matted = ops::matte(&mut e, &iso, &mimg, kind);
            let full = ops::blend_full_preserve(&mut e, &bg, &matted, mode, 0.8, 5, None, preserve);
            let mut iso_in = own(&mut e, &iso);
            let before = iso_in.texture.clone();
            ops::matte_in(&mut e, &mut iso_in, &mimg, kind, Some(reach));
            assert!(iso_in.texture == before, "{label}: matte in place");
            let mut canvas = own(&mut e, &bg);
            let before = canvas.texture.clone();
            ops::blend_full_in(&mut e, &mut canvas, &iso_in, mode, 0.8, 5, None, preserve, Some(reach));
            assert!(canvas.texture == before, "{label}: blend in place");
            same_bits(&mut e, &label, &full, &canvas);
        }
    }

    // Blend modes (dissolve noise at absolute pixels), 8 bpc on a quantised canvas; stencils
    // and an unquantised canvas at 8 bpc take the full pass.
    let cases = [
        (Screen, 1.0, None, &bg, true),
        (Overlay, 0.7, None, &bg, true),
        (Dissolve, 0.6, None, &bg, true),
        (Difference, 1.0, q, &bg_q, true),
        (Add, 0.9, q, &bg_q, true),
        (Normal, 0.5, q, &bg, false),
        (StencilAlpha, 1.0, None, &bg, false),
    ];
    for (mode, opacity, levels, base, in_place) in cases {
        let label = format!("{mode:?} {levels:?}");
        let full = ops::blend_full(&mut e, base, &iso, mode, opacity, 9, levels);
        let mut canvas = own(&mut e, base);
        canvas.levels = base.levels;
        let before = canvas.texture.clone();
        ops::blend_full_in(&mut e, &mut canvas, &iso, mode, opacity, 9, levels, false, Some(reach));
        assert_eq!(canvas.texture == before, in_place, "{label}: in place");
        assert_eq!(canvas.levels, full.levels, "{label}");
        same_bits(&mut e, &label, &full, &canvas);
    }

    // Motion blur: sub-samples at minification levels 0, 1, 0 (three runs), summed in place on
    // an owned canvas, and on zeros (the first run a full pass) then blended in place; against
    // one full pass per run.
    let samples = [at(50.0, 50.0, 0.0, 1.0), at(54.0, 52.0, 4.0, 1.0), at(58.0, 54.0, 8.0, 0.3), at(60.0, 55.0, 10.0, 0.3), at(64.0, 56.0, 12.0, 1.0)];
    let runs: [&[Mat3]; 3] = [&samples[..2], &samples[2..4], &samples[4..]];
    let k = 1.0 / samples.len() as f32;
    for (base, sum_in_place) in [(&bg, true), (&zeros, false)] {
        let label = if sum_in_place { "motion blur on the canvas" } else { "motion blur on zeros" };
        // Each call sees a canvas it doesn't own (`acc` is still held): one full pass per run.
        let mut acc = base.clone();
        for run in runs {
            acc = ops::warp_accumulate(&mut e, &acc, &layer, run, Bilinear, k);
        }
        let mut sum = if sum_in_place { own(&mut e, base) } else { zeros.clone() };
        let before = sum.texture.clone();
        let b = ops::warp_accumulate_in(&mut e, &mut sum, &layer.clone().into(), &samples, Bilinear, k);
        assert_eq!(sum.texture == before, sum_in_place, "{label}: in place");
        same_bits(&mut e, label, &acc, &sum);
        if sum_in_place {
            continue;
        }
        // The sum on zeros is transparent outside `b`: blended in place.
        let full = ops::blend_full(&mut e, &bg, &acc, Screen, 0.9, 3, None);
        let mut canvas = own(&mut e, &bg);
        let before = canvas.texture.clone();
        ops::blend_full_in(&mut e, &mut canvas, &sum, Screen, 0.9, 3, None, false, Some(b));
        assert!(canvas.texture == before, "{label}: blend in place");
        same_bits(&mut e, label, &full, &canvas);
    }

    // Layer styles: the canvas moved toward the styled result on the enabled channels.
    let mut tmp = own(&mut e, &bg);
    ops::warp_q_in(&mut e, &mut tmp, &layer.clone().into(), &m, Bilinear, Multiply, 1.0, 0, None, None);
    let full = ops::channel_mix(&mut e, &bg, &tmp, [true, false, true], 0.6);
    let mut canvas = own(&mut e, &bg);
    let before = canvas.texture.clone();
    ops::channel_mix_in(&mut e, &mut canvas, &tmp, [true, false, true], 0.6, Some(reach));
    assert!(canvas.texture == before, "channel mix in place");
    same_bits(&mut e, "channel mix", &full, &canvas);

    let start = e.download(&bg).unwrap();
    assert!(tests::diff(&start, &e.download(&full).unwrap(), 1e-3).over > 200, "the layers draw");
}

/// A frame long enough to be submitted in several command buffers (RT10c submits every few
/// dozen dispatches so the GPU starts while the walk records the rest): the canvas stays the
/// encoder's own across those submits, so layers keep drawing in place, bit for bit as full
/// passes; an image another `Enc` or a clone holds is never owned.
#[test]
fn in_place_layers_survive_early_submits() {
    let Some(g) = tests::gpu() else {
        assert!(std::env::var_os("EFFECTCRAFT_REQUIRE_GPU").is_none());
        return;
    };
    let (w, h) = (160, 120);
    let bg = g.ctx.upload_image(&tests::pattern(3, w, h)).unwrap();
    let layer = g.ctx.upload_image(&tests::pattern(7, 40, 30)).unwrap();
    let mut e = Enc::new(&g.ctx);
    let mut full = bg.clone();
    let mut canvas = own(&mut e, &bg);
    for i in 0..150 {
        let (x, y) = (20.0 + f64::from(i % 13) * 9.0, 15.0 + f64::from(i % 7) * 13.0);
        let m = at(x, y, f64::from(i) * 7.0, 0.6 + f64::from(i % 5) * 0.1);
        let mode = [Normal, Multiply, Screen, Add][(i % 4) as usize];
        assert!(e.owns(&canvas), "step {i}: the canvas is the encoder's own across submits");
        full = ops::warp_q(&mut e, &full, &layer, &m, Bilinear, mode, 0.7, 5, None, None);
        let before = canvas.texture.clone();
        ops::warp_q_in(&mut e, &mut canvas, &layer.clone().into(), &m, Bilinear, mode, 0.7, 5, None, None);
        assert!(canvas.texture == before, "step {i}: drawn in place");
    }
    same_bits(&mut e, "150 layers over several submits", &full, &canvas);
    // Never owned: an image of another encoder, or one with a second handle.
    let mut other = Enc::new(&g.ctx);
    assert!(!other.owns(&canvas));
    let theirs = other.scratch(8, 8);
    other.submit();
    assert!(!e.owns(&theirs));
    let shared = canvas.clone();
    assert!(!e.owns(&canvas));
    drop(shared);
    assert!(e.owns(&canvas));
}
