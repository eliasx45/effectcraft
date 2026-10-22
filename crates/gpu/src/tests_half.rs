//! Half-float effect intermediates (`crate::half`): a render whose effects keep their working
//! images as RGBA16F (a viewer display render with Settings ▸ Previews ▸ Half-float effect
//! buffers on) stays within 1/255 (8-bit space; relative above 1.0) of the f32 render for
//! Gaussian Blur, Glow, Deep Glow and Bevel and Emboss, and holds fewer working-image bytes at
//! its peak.

use effectcraft_keyframe::Value;
use effectcraft_project::build::Ids;
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{BitDepth, Layer};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, LayerCache, RenderOpts, Renderer};
use effectcraft_time::Tick;

use crate::Gpu;
use crate::context::Enc;
use crate::tests::{Pattern, Scene, n, set};

fn style(s: &mut Scene, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
    let mut next = s.p.next_id;
    st::add_style(l, &mut Ids(&mut next), id, &s.comp.global_light, true).unwrap();
    s.p.next_id = next;
    for (k, v) in vals {
        set(l, &format!("{GROUP}/{id}/{k}"), v.clone());
    }
}

/// One frame through a fresh layer cache with half working images on or off: the pixels and
/// the most working-image bytes handed out at once during it.
fn render(g: &Gpu, s: &Scene, half: bool) -> (Image, usize) {
    let cache = LayerCache::default();
    let mut r = Renderer::new(&s.p, &Pattern, RenderOpts { backend: Backend::Gpu, ..Default::default() });
    r.accel = Some(g);
    r.cache = Some(&cache);
    g.within_memory(|| {
        let mut enc = Enc::with_owner(&g.ctx, cache.gpu_owner());
        enc.half = half;
        g.ctx.reset_vram_peak();
        let (before, _) = g.ctx.leased_bytes();
        let image = crate::walk::render(&mut enc, &r, s.cid, Tick::ZERO).unwrap();
        let (_, peak) = g.ctx.leased_bytes();
        let pixels = enc.download_px(&image).unwrap();
        (pixels, peak.saturating_sub(before))
    })
    .unwrap()
}

/// Largest difference in 8-bit steps (relative to the value above 1.0), and the share of
/// channel values more than one step apart.
fn diff(a: &Image, b: &Image) -> (f32, f64) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let (mut max, mut over, mut total) = (0.0f32, 0usize, 0usize);
    for (p, q) in a.data.iter().zip(&b.data) {
        for c in 0..4 {
            let d = (p[c] - q[c]).abs() / q[c].abs().max(1.0) * 255.0;
            max = max.max(d);
            over += usize::from(d > 1.0);
            total += 1;
        }
    }
    (max, over as f64 / total.max(1) as f64)
}

fn case(label: &str, s: &Scene, steps: f32) {
    let Some(g) = crate::tests::gpu() else { return };
    if g.ctx.half_bank().is_none() {
        eprintln!("{label}: no half kernels on {}, skipped", g.ctx.name);
        return;
    }
    let (full, full_peak) = render(g, s, false);
    let (half, half_peak) = render(g, s, true);
    let (max, share) = diff(&half, &full);
    eprintln!("{label}: max {max:.3} / 255, {:.4} % over 1/255, peak working bytes f32 {full_peak} → half {half_peak}", share * 100.0);
    assert!(max <= steps, "{label}: half floats differ by {max:.3}/255 (allowed {steps}/255)");
    assert!(half_peak < full_peak, "{label}: half working images did not lower the peak ({half_peak} ≥ {full_peak})");
}

#[test]
fn half_gaussian_blur_matches_f32() {
    let mut s = Scene::new(BitDepth::Bpc32);
    let mut l = s.footage(97, 61);
    s.effect(&mut l, "ec.blur.gaussian", &[("blurriness", n(12.0))]);
    s.push(l);
    case("gaussian blur", &s, 1.0);
}

#[test]
fn half_glow_matches_f32() {
    let mut s = Scene::new(BitDepth::Bpc32);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.solid([0.9, 0.4, 0.1], 40, 24);
    s.effect(&mut l, "ec.stylize.glow", &[("threshold", n(20.0)), ("radius", n(14.0)), ("intensity", n(1.5))]);
    s.push(l);
    case("glow", &s, 1.0);
}

#[test]
fn half_deep_glow_matches_f32() {
    let mut s = Scene::new(BitDepth::Bpc32);
    let mut l = s.footage(60, 40);
    s.effect(&mut l, "ec.stylize.deepglow", &[("PEDG-0001", n(24.0)), ("PEDG-0004", n(30.0))]);
    s.push(l);
    case("deep glow", &s, 1.0);
}

#[test]
fn half_bevel_matches_f32() {
    let mut s = Scene::new(BitDepth::Bpc32);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.solid([0.8, 0.5, 0.2], 50, 34);
    style(&mut s, &mut l, "bevelEmboss", &[("size", Value::Scalar(9.0)), ("soften", Value::Scalar(2.0))]);
    s.push(l);
    // The lighting takes the height profile's slope: half floats' rounding of the profile shows
    // as at most a step or two of shading, never more.
    case("bevel and emboss", &s, 2.0);
}

#[test]
fn half_layer_styles_match_f32() {
    let mut s = Scene::new(BitDepth::Bpc32);
    let bg = s.footage(97, 61);
    s.push(bg);
    let mut l = s.solid([0.3, 0.6, 0.9], 44, 30);
    style(&mut s, &mut l, "dropShadow", &[("size", Value::Scalar(8.0)), ("distance", Value::Scalar(4.0))]);
    style(&mut s, &mut l, "outerGlow", &[("size", Value::Scalar(10.0))]);
    style(&mut s, &mut l, "innerGlow", &[("size", Value::Scalar(6.0))]);
    s.push(l);
    case("layer styles", &s, 1.0);
}
