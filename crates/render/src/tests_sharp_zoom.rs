//! Sharp zoom: a region rendered above comp resolution (`scale` > 1 with a region of interest)
//! draws text, shapes and nested comps at the magnification instead of magnifying the
//! comp-resolution frame, and every layer buffer stays within the sharp-zoom pixel budget.

use effectcraft_color::Label;
use effectcraft_keyframe::{TextDoc, Value};
use effectcraft_project::build;
use effectcraft_project::{Comp, ItemId, ItemKind, Layer, LayerSource, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{Image, NoFootage, RenderOpts, Renderer, SHARP_MAX_PIXELS};

fn project(w: u32, h: u32) -> (Project, ItemId, Comp) {
    let mut p = Project::default();
    p.settings.bit_depth = effectcraft_project::BitDepth::Bpc32;
    let comp = Comp::new(w, h, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    (p, cid, comp)
}

fn solid(p: &mut Project, comp: &Comp, color: [f32; 3], w: u32, h: u32, pos: [f64; 2]) -> Layer {
    let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color, width: w, height: h, pixel_aspect: 1.0 }));
    let mut l = build::layer(p, comp, "Solid", LayerSource::Solid { item: sid }, (w, h), None);
    l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([pos[0], pos[1], 0.0]);
    l
}

fn text(p: &mut Project, comp: &Comp, s: &str, size: f64, pos: [f64; 2]) -> Layer {
    let mut l = build::layer(p, comp, "Text", LayerSource::Text, (comp.width, comp.height), None);
    l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(TextDoc { text: s.into(), size, ..Default::default() }));
    l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([pos[0], pos[1], 0.0]);
    l
}

fn render(p: &Project, cid: ItemId, scale: f64, roi: Option<[f64; 4]>) -> Image {
    Renderer::new(p, &NoFootage, RenderOpts { scale, roi, sharp: true, ..Default::default() }).comp_frame(cid, Tick::ZERO)
}

/// The comp-resolution frame's region `[x, y, w, h]` magnified `k` times the way the viewer
/// shows it above 100 % (nearest neighbour: never smoothed).
fn magnified(full: &Image, r: [u32; 4], k: u32) -> Image {
    let mut out = Image::new(r[2] * k, r[3] * k);
    for y in 0..out.height {
        for x in 0..out.width {
            out.data[(y * out.width + x) as usize] = full.get((r[0] + x / k) as i64, (r[1] + y / k) as i64);
        }
    }
    out
}

/// Share of the drawn pixels that are partly covered: the edges' width. A sharp edge at the
/// output's resolution is one pixel wide; a magnified one is a block of the magnification.
fn soft_share(img: &Image) -> f64 {
    let drawn = img.data.iter().filter(|p| p[3] > 0.05).count();
    let soft = img.data.iter().filter(|p| p[3] > 0.05 && p[3] < 0.95).count();
    soft as f64 / drawn.max(1) as f64
}

fn coverage(img: &Image) -> f64 {
    img.data.iter().map(|p| f64::from(p[3])).sum::<f64>() / img.data.len().max(1) as f64
}

#[test]
fn text_at_four_times_is_sharper_than_the_magnified_frame() {
    let (mut p, cid, comp) = project(320, 180);
    let l = text(&mut p, &comp, "VIRAL", 48.0, [100.0, 100.0]);
    p.comp_mut(cid).unwrap().layers.push(l);
    let region = [80u32, 50, 120, 70];
    let roi = region.map(f64::from);
    let sharp = render(&p, cid, 4.0, Some(roi));
    assert_eq!((sharp.width, sharp.height), (480, 280));
    let blocky = magnified(&render(&p, cid, 1.0, None), region, 4);
    // The same glyphs in the same place…
    let (a, b) = (coverage(&sharp), coverage(&blocky));
    assert!(a > 0.02 && (a - b).abs() < 0.1 * b, "coverage {a} vs {b}");
    // …with edges a quarter as wide.
    let (s, m) = (soft_share(&sharp), soft_share(&blocky));
    assert!(s < 0.5 * m, "soft share: sharp {s:.3}, magnified {m:.3}");
}

/// A detailed nested comp shown smaller: at the magnification its pixels come from the nested
/// comp's own resolution, not from the comp-resolution frame (where the detail averaged out).
#[test]
fn a_scaled_down_precomp_resolves_its_detail() {
    let (mut p, cid, comp) = project(200, 100);
    let inner = Comp::new(80, 80, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let iid = p.add_item("Stripes", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    // Vertical stripes 2 nested pixels wide: 1 comp pixel at 50 %.
    let stripes: Vec<Layer> = (0..20).map(|i| solid(&mut p, &inner, [1.0, 1.0, 1.0], 2, 80, [i as f64 * 4.0 + 1.0, 40.0])).collect();
    p.comp_mut(iid).unwrap().layers = stripes.into();
    let mut pre = build::layer(&mut p, &comp, "Stripes", LayerSource::Comp { item: iid }, (80, 80), None);
    pre.props.prop_mut("transform/position").unwrap().value = Value::Vec3([100.5, 50.0, 0.0]);
    pre.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([50.0, 50.0, 100.0]);
    p.comp_mut(cid).unwrap().layers.push(pre);
    let sharp = render(&p, cid, 4.0, Some([90.0, 40.0, 20.0, 20.0]));
    let blocky = magnified(&render(&p, cid, 1.0, None), [90, 40, 20, 20], 4);
    let contrast = |img: &Image| {
        let row: Vec<f32> = (0..img.width).map(|x| img.get(x as i64, (img.height / 2) as i64)[3]).collect();
        row.iter().cloned().fold(0.0f32, f32::max) - row.iter().cloned().fold(1.0f32, f32::min)
    };
    let (s, m) = (contrast(&sharp), contrast(&blocky));
    assert!(s > 0.9, "the stripes resolve at 400 %: contrast {s}");
    assert!(m < 0.3, "the comp-resolution frame averaged them out: contrast {m}");
}

/// A full-frame solid in an HD comp at 800 %: drawn within the budget (not 15360×8640), the
/// region still the solid's colour.
#[test]
fn layer_buffers_stay_within_the_budget() {
    let (mut p, cid, comp) = project(1920, 1080);
    let bg = solid(&mut p, &comp, [0.2, 0.4, 0.6], 1920, 1080, [960.0, 540.0]);
    let label = text(&mut p, &comp, "WIDE HEADLINE ACROSS THE FRAME", 160.0, [40.0, 600.0]);
    p.comp_mut(cid).unwrap().layers = vec![label, bg].into();
    let r = Renderer::new(&p, &NoFootage, RenderOpts { scale: 8.0, roi: Some([900.0, 500.0, 100.0, 60.0]), sharp: true, ..Default::default() });
    let c = p.comp(cid).unwrap();
    let ctx = r.ctx(cid, c, Tick::ZERO);
    for l in c.layers.iter() {
        let b = r.layer_buf(&ctx, l).unwrap();
        let px = b.img.width as f64 * b.img.height as f64;
        assert!(px <= SHARP_MAX_PIXELS * 1.01, "{}: {}×{}", l.name, b.img.width, b.img.height);
        assert!(b.scale >= 1.0, "{}: never below comp resolution ({})", l.name, b.scale);
    }
    let img = r.comp_frame(cid, Tick::ZERO);
    assert_eq!((img.width, img.height), (800, 480));
    let px = img.get(400, 240);
    assert!((px[2] - 0.6).abs() < 1e-3 && px[3] > 0.999, "{px:?}");
}

/// At and below 100 %, and in renders that aren't sharp zoom (exports to a larger output), the
/// budget changes nothing: the same buffers as before.
#[test]
fn budget_only_applies_to_sharp_zoom_above_comp_resolution() {
    let (p, _, _) = project(1920, 1080);
    let at = |scale: f64, sharp: bool| Renderer::new(&p, &NoFootage, RenderOpts { scale, sharp, ..Default::default() }).solid_scale(3000, 2000);
    for (scale, sharp) in [(1.0, true), (0.5, true), (0.25, false), (2.0, false), (4.0, false)] {
        assert_eq!(at(scale, sharp), scale, "{scale}× (sharp {sharp})");
    }
    // Sharp zoom: 3000×2000 at 4× would be 96 Mpx; held to the budget…
    let k = at(4.0, true);
    assert!(k > 1.0 && k < 4.0 && 3000.0 * 2000.0 * k * k <= SHARP_MAX_PIXELS * 1.0001, "{k}");
    // …but never below 100 % (a bigger solid is as sharp as at 100 %, not softer).
    let r = Renderer::new(&p, &NoFootage, RenderOpts { scale: 4.0, sharp: true, ..Default::default() });
    assert_eq!(r.solid_scale(6000, 4000), 1.0);
}

/// The options' `Debug` text keys the caches: without sharp zoom it is the text it always was.
#[test]
fn debug_text_shows_sharp_only_when_on() {
    let off = format!("{:?}", RenderOpts::default());
    assert!(!off.contains("sharp") && off.ends_with("nested_switches: true, draft_shadows: true }"), "{off}");
    assert!(format!("{:?}", RenderOpts { sharp: true, ..Default::default() }).contains("sharp: true"));
}
