//! Large shape layers in small nested comps are rasterised only around the nested frame
//! (see `clip.rs`), with the frames identical to rasterising them whole.

use std::sync::Mutex;

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, Layer, LayerSource, MaskMode, Project};
use effectcraft_time::{FrameRate, Tick};

use crate::{Image, LayerCache, NoFootage, RenderOpts, Renderer};

/// The tests flip the process-wide switch: one at a time.
pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

fn s(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

/// What a scene's shape layer carries.
#[derive(Clone, Copy, Default)]
struct Opts {
    blur: bool,
    mask: bool,
    animated: bool,
    /// The outer precomp layer moves with motion blur on.
    outer_blur: bool,
    /// The shape layer itself moves with motion blur on.
    layer_blur: bool,
    /// The shapes animate (the frame's size): the content changes every frame.
    content: bool,
}

/// A 2600×2000 shape (a filled backdrop, a grid of small cut-out-like tiles and a stroked frame)
/// in a 120×90 comp, nested scaled and rotated in a 200×150 comp. Returns the project, the outer
/// and the inner comp.
fn scene(depth: BitDepth, o: Opts) -> (Project, ItemId, ItemId) {
    let mut p = Project::default();
    p.settings.bit_depth = depth;
    let mut inner = Comp::new(120, 90, FrameRate::FPS_30, s(2.0));
    inner.enable_motion_blur = o.layer_blur;
    let mut outer = Comp::new(200, 150, FrameRate::FPS_30, s(2.0));
    outer.enable_motion_blur = o.outer_blur;
    let mut l = build::layer(&mut p, &inner, "Shape", LayerSource::Shape, (inner.width, inner.height), None);
    let mut next = p.next_id;
    let mut ids = Ids(&mut next);
    let mut items = vec![build::shape_rect(&mut ids, [2600.0, 2000.0], [0.0, 0.0], 0.0)];
    let mut tiles = vec![];
    for i in -6..=6 {
        for j in -5..=5 {
            tiles.push(build::shape_rect(&mut ids, [23.0 + (i * j % 7) as f64, 17.0], [i as f64 * 61.0 + 0.3, j as f64 * 47.0 - 0.6], 4.0));
        }
    }
    // Dissolve: per-pixel grain that must follow the whole buffer's pixels.
    let mut fill = build::shape_fill(&mut ids, [0.1, 0.5, 0.9, 0.8]);
    let dissolve = effectcraft_color::BlendMode::ALL.iter().position(|m| *m == effectcraft_color::BlendMode::Dissolve).unwrap();
    fill.prop_mut("blend").unwrap().value = Value::Enum(dissolve as u32);
    tiles.push(fill);
    items.push(build::shape_fill(&mut ids, [0.9, 0.6, 0.2, 1.0]));
    let backdrop = build::shape_group(&mut ids, "Backdrop", items);
    let grid = build::shape_group(&mut ids, "Tiles", tiles);
    let mut rect = build::shape_rect(&mut ids, [150.0, 110.0], [10.0, -5.0], 12.0);
    if o.content {
        rect.prop_mut("size").unwrap().keys = vec![Keyframe::new(Tick::ZERO, Value::Vec2([150.0, 110.0])), Keyframe::new(s(1.0), Value::Vec2([90.0, 300.0]))];
    }
    let frame_items = vec![rect, build::shape_stroke(&mut ids, [1.0, 1.0, 1.0, 1.0], 7.0)];
    let frame = build::shape_group(&mut ids, "Frame", frame_items);
    // Top-most first.
    for g in [frame, grid, backdrop] {
        l.props.sub_mut("contents").unwrap().children.push(g.into());
    }
    l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([60.0, 45.0, 0.0]);
    l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(9.0);
    if o.animated || o.layer_blur {
        let pos = l.props.prop_mut("transform/position").unwrap();
        pos.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3([-90.0, 20.0, 0.0])), Keyframe::new(s(1.0), Value::Vec3([210.0, 70.0, 0.0]))];
    }
    if o.animated {
        let rot = l.props.prop_mut("transform/rotation").unwrap();
        rot.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(0.0)), Keyframe::new(s(1.0), Value::Scalar(30.0))];
        let sc = l.props.prop_mut("transform/scale").unwrap();
        sc.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3([100.0, 100.0, 100.0])), Keyframe::new(s(1.0), Value::Vec3([70.0, 85.0, 100.0]))];
    }
    l.switches.motion_blur = o.layer_blur;
    if o.mask {
        let mut m = build::mask(&mut ids, "Mask 1", ShapePath::rect([100.0, 50.0], 900.0, 700.0), MaskMode::Add, [255, 255, 0]);
        m.prop_mut("feather").unwrap().value = Value::Vec2([30.0, 30.0]);
        m.prop_mut("expansion").unwrap().value = Value::Scalar(10.0);
        l.props.sub_mut("masks").unwrap().children.push(m.into());
    }
    if o.blur {
        let spec = effectcraft_effects::find("ec.blur.gaussian").unwrap();
        let mut g = effectcraft_effects::instantiate(spec, &mut ids, spec.name, [120.0, 90.0]);
        g.prop_mut("blurriness").unwrap().value = Value::Scalar(25.0);
        l.props.sub_mut("effects").unwrap().children.push(g.into());
    }
    p.next_id = next;
    inner.layers.push(l);
    let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    let mut pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (120, 90), None);
    pre.props.prop_mut("transform/position").unwrap().value = Value::Vec3([100.0, 75.0, 0.0]);
    pre.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(5.0);
    pre.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([120.0, 120.0, 100.0]);
    if o.outer_blur {
        pre.switches.motion_blur = true;
        let pos = pre.props.prop_mut("transform/position").unwrap();
        pos.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3([60.0, 60.0, 0.0])), Keyframe::new(s(1.0), Value::Vec3([140.0, 90.0, 0.0]))];
    }
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.into()));
    p.comp_mut(cid).unwrap().layers.push(pre);
    (p, cid, iid)
}

/// The outer frame and the inner comp's frame (as nested) at `t`, clipped or whole.
fn frames(p: &Project, cid: ItemId, iid: ItemId, scale: f64, t: Tick, clip: bool, cache: Option<&LayerCache>) -> (Image, Image) {
    crate::set_shape_clip(clip);
    let mut r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
    r.cache = cache;
    let outer = r.comp_frame(cid, t);
    let inner = r.nested().comp_frame(iid, t);
    crate::set_shape_clip(true);
    (outer, inner)
}

/// Pixels of the inner comp's shape layer buffer (after masks and effects) as drawn nested.
fn buf_pixels(p: &Project, iid: ItemId, scale: f64, t: Tick, clip: bool) -> u64 {
    crate::set_shape_clip(clip);
    let r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
    let n = r.nested();
    let c = p.comp(iid).unwrap();
    let ctx = n.ctx(iid, c, t);
    let l: &Layer = c.layers.iter().next().unwrap();
    let b = n.content_buf(&ctx, l).unwrap();
    crate::set_shape_clip(true);
    b.img.width as u64 * b.img.height as u64
}

fn max_diff(a: &Image, b: &Image) -> f32 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.data.iter().zip(&b.data).flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs())).fold(0.0, f32::max)
}

/// Largest difference allowed: rasterising at other (whole-pixel) coordinates changes values by
/// float rounding only (under 1e-6 at 32 bpc), which can tip a value quantised after masks and
/// again after effects by a code value each at 8 and 16 bpc.
fn tolerance(depth: BitDepth) -> f32 {
    match depth {
        BitDepth::Bpc8 => 2.01 / 255.0,
        BitDepth::Bpc16 => 2.01 / 32768.0,
        BitDepth::Bpc32 => 1e-5,
    }
}

fn nonblank(img: &Image) -> bool {
    img.data.iter().any(|p| p[3] > 0.5)
}

/// The visible pixels are the same at 8/16/32 bpc, Full and Half, with a blur, a feathered
/// expanded mask and an animated transform (the layer sweeping across the frame), and the
/// rasterised buffer is a fraction of the whole one.
#[test]
fn clipped_shape_frames_match_the_whole_raster() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        for (blur, mask) in [(false, false), (true, true)] {
            let (p, cid, iid) = scene(depth, Opts { blur, mask, animated: true, ..Default::default() });
            for scale in [1.0, 0.5] {
                for t in [0.0, 0.4, 0.8] {
                    let (a, ai) = frames(&p, cid, iid, scale, s(t), true, None);
                    let (b, bi) = frames(&p, cid, iid, scale, s(t), false, None);
                    assert!(nonblank(&bi), "the shape shows ({depth:?}, {scale}, {t})");
                    let (d, di) = (max_diff(&a, &b), max_diff(&ai, &bi));
                    assert!(d <= tolerance(depth) && di <= tolerance(depth), "{depth:?} blur {blur} mask {mask} scale {scale} t {t}: outer {d}, inner {di}");
                }
                let (clipped, whole) = (buf_pixels(&p, iid, scale, s(0.4), true), buf_pixels(&p, iid, scale, s(0.4), false));
                assert!(clipped * 3 < whole, "{depth:?} blur {blur} mask {mask} scale {scale}: {clipped} of {whole} pixels");
            }
        }
    }
}

/// Motion blur: a shape inside a motion-blurred moving precomp is clipped (the nested frame is
/// still its own frame) and matches; a motion-blurred shape layer is drawn whole (its sample count
/// follows its buffer) and matches.
#[test]
fn clipped_shapes_under_motion_blur_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (o, clips) in [(Opts { outer_blur: true, blur: true, ..Default::default() }, true), (Opts { layer_blur: true, ..Default::default() }, false)] {
        let (p, cid, iid) = scene(BitDepth::Bpc8, o);
        for t in [0.3, 0.6] {
            let (a, ai) = frames(&p, cid, iid, 1.0, s(t), true, None);
            let (b, bi) = frames(&p, cid, iid, 1.0, s(t), false, None);
            assert!(nonblank(&b));
            assert!(max_diff(&a, &b) <= tolerance(BitDepth::Bpc8) && max_diff(&ai, &bi) <= tolerance(BitDepth::Bpc8), "outer blur {} at {t}", o.outer_blur);
        }
        let (clipped, whole) = (buf_pixels(&p, iid, 1.0, s(0.3), true), buf_pixels(&p, iid, 1.0, s(0.3), false));
        assert_eq!(clipped < whole, clips, "outer blur {}: {clipped} of {whole} pixels", o.outer_blur);
    }
}

/// With the layer cache, an animated, moving clipped layer keys its window: every frame matches
/// the uncached whole raster (no stale window served). Static content moving about is kept whole
/// after it held for two frames (drawn once, as before clipping).
#[test]
fn clipped_windows_are_part_of_the_cache_key() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for content in [true, false] {
        let (mut p, cid, iid) = scene(BitDepth::Bpc32, Opts { content, ..Default::default() });
        // Slow drift: the window holds for several frames, then moves on.
        let c = p.comp_mut(iid).unwrap();
        let l = c.layers.iter_mut().next().unwrap();
        l.props.prop_mut("transform/position").unwrap().keys =
            vec![Keyframe::new(Tick::ZERO, Value::Vec3([40.0, 45.0, 0.0])), Keyframe::new(s(1.0), Value::Vec3([340.0, 45.0, 0.0]))];
        let cache = LayerCache::new(1 << 30);
        let mut sizes = vec![];
        for f in 0..20 {
            let t = s(f as f64 / 30.0);
            let (a, ai) = frames(&p, cid, iid, 1.0, t, true, Some(&cache));
            let (b, bi) = frames(&p, cid, iid, 1.0, t, false, None);
            assert!(max_diff(&a, &b) <= tolerance(BitDepth::Bpc32) && max_diff(&ai, &bi) <= tolerance(BitDepth::Bpc32), "content {content} frame {f}");
            // The buffer the cached render drew.
            let r = Renderer { cache: Some(&cache), ..Renderer::new(&p, &NoFootage, RenderOpts::default()) };
            let n = r.nested();
            let ic = p.comp(iid).unwrap();
            let b = n.content_buf(&n.ctx(iid, ic, t), ic.layers.iter().next().unwrap()).unwrap();
            sizes.push(b.img.width as u64 * b.img.height as u64);
        }
        // The whole buffer: the 2600×2000 backdrop and its padding.
        let whole = 2600u64 * 2000;
        if content {
            assert!(sizes.iter().all(|&n| n * 3 < whole), "animated content stays clipped: {sizes:?}");
        } else {
            assert!(sizes[..2].iter().all(|&n| n * 3 < whole) && sizes[2..].iter().all(|&n| n > whole), "static content goes whole: {sizes:?}");
            assert!(cache.stats().hits > 0);
        }
    }
}

/// The dissolve noise of a shape's draw follows the whole buffer's pixels, so a window draws
/// the same grain.
#[test]
fn windowed_shape_render_equals_the_whole_buffer_there() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (p, _, iid) = scene(BitDepth::Bpc32, Opts::default());
    let c = p.comp(iid).unwrap();
    let ctx = crate::EvalCtx::new(&p, iid, c, Tick::ZERO);
    let l = c.layers.iter().next().unwrap();
    let contents = l.props.sub("contents").unwrap();
    let whole = crate::shapes::render(&ctx, l, contents, 1.0);
    let win = [513, 260, 300, 410];
    let part = crate::shapes::render_window(&ctx, l, contents, 1.0, Some(win));
    assert_eq!((part.img.width, part.img.height), (300, 410));
    assert_eq!(part.offset, [whole.offset[0] - 513.0, whole.offset[1] - 260.0]);
    let mut d = 0.0f32;
    for y in 0..410i64 {
        for x in 0..300i64 {
            let (p, q) = (part.img.get(x, y), whole.img.get(x + 513, y + 260));
            d = (0..4).map(|c| (p[c] - q[c]).abs()).fold(d, f32::max);
        }
    }
    assert!(d <= 1e-4, "{d}");
}
