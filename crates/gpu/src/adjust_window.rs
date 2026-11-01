//! An adjustment layer whose footprint (its masks, its track matte) covers a small part of the
//! comp runs its effects only on the window of the comp it can change, grown by what each
//! effect reads to make those pixels, and the result goes back into that window. Before, every
//! such layer blurred or tiled the whole frame (Elias's "K Step Stack", 1 Nov 2026: ten
//! adjustment layers a frame whose glass and mosaic mattes cover 2–8 % of it).
//!
//! The pixels are the same bits as running on the whole comp, not just close:
//! * box-blur passes (Gaussian Blur, Fast Box Blur) restart their running sums every block of
//!   pixels; the window keeps the comp's block grid (`block_phase`, the kernels' phase), and
//!   each pass reads back to the start of the block a needed pixel sits in (minus its radius);
//! * Mosaic lays its tiles over the whole comp, not over the window (`frame_of`), and the window
//!   holds every tile a needed pixel sits in;
//! * colour effects are per pixel.
//!
//! Any other effect (unknown reach, reads other layers, points or positions in the layer),
//! an effect the GPU declines at run time, a non-affine footprint or a window over most of the
//! comp: the whole comp, as before. `EFFECTCRAFT_GPU_ADJUST_WINDOW=0` turns it off.

use std::sync::{Arc, Mutex, Weak};

use effectcraft_effects::Buf;
use effectcraft_geom::Rect;
use effectcraft_project::{Layer, MatteKind};
use effectcraft_render::{EvalCtx, FxStep, FxTarget, Renderer};

use crate::codes::Pixels;
use crate::context::{Enc, GpuImage};
use crate::effects::{GpuFx, box_radii, dims_xy};
use crate::ops;
use crate::walk::{self, Footprint, MatteSource};

/// The window of the comp the effects running now see ([`Enc::fx_frame`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The window's top-left corner in the comp canvas.
    pub at: [u32; 2],
    /// The window's size: the buffer the effects run on.
    pub size: [u32; 2],
    /// The whole comp canvas.
    pub full: [u32; 2],
}

/// `e`'s window of the comp when `size` is its size (an image of any other size isn't it).
pub(crate) fn frame_of(e: &Enc, size: (u32, u32)) -> Option<Frame> {
    e.fx_frame.filter(|f| f.size == [size.0, size.1])
}

/// Where the comp's grid of box-blur blocks of `block` pixels starts in a `size` image: 0, or
/// in a window of the comp ([`frame_of`]) how far before it the window's first block starts.
pub(crate) fn block_phase(e: &Enc, size: (u32, u32), vertical: bool, block: u32) -> u32 {
    match frame_of(e, size) {
        Some(f) if block > 0 => f.at[usize::from(vertical)] % block,
        _ => 0,
    }
}

fn enabled() -> bool {
    #[cfg(test)]
    if WHOLE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("EFFECTCRAFT_GPU_ADJUST_WINDOW").map_or(true, |v| v != "0"))
}

/// Tests: run every adjustment layer on the whole comp.
#[cfg(test)]
pub(crate) static WHOLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Tests: adjustment layers drawn through a window.
#[cfg(test)]
pub(crate) static WINDOWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What an effect reads around a pixel to make it, as its GPU kernels run on an adjustment layer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reach {
    /// The pixel only.
    Pixel,
    /// Box passes along rows (`h`, radii in pixels) then along columns (`v`), edges repeated.
    Boxes { h: Vec<usize>, v: Vec<usize> },
    /// Mosaic: `blocks` tiles across and down the comp; Sharp Colors takes each tile's centre.
    Tiles { blocks: [usize; 2], sharp: bool },
}

/// Per-pixel colour effects (no position, no neighbours).
const PER_PIXEL: &[&str] =
    &["ec.color.tint", "ec.color.levels", "ec.color.curves", "ec.color.huesaturation", "ec.color.brightnesscontrast", "ec.color.exposure", "ec.channel.invert"];

/// `step`'s reach at buffer scale `scale` (the steps of `effects::gaussian_px`, `box_blur_px`,
/// `fx_distort::mosaic`), `None` when not known.
fn reach(step: &FxStep, scale: f64) -> Option<Reach> {
    let p = step.ctx.params;
    let id = step.spec.id;
    // Only on the comp below (edges repeat, the buffer keeps its size).
    if !step.ctx.adjustment {
        return None;
    }
    let [cx, cy] = step.ctx.env.comp_px();
    let (kx, ky) = dims_xy(p.e("dimensions"));
    match id {
        "ec.blur.gaussian" => {
            let s = effectcraft_effects::gaussian_sigma(p.f("blurriness")) * scale;
            if !s.is_finite() {
                return None;
            }
            if s <= 0.0 {
                return Some(Reach::Pixel);
            }
            let (sx, sy) = (s * cx * kx, s * cy * ky);
            if !(sx.is_finite() && sy.is_finite()) {
                return None;
            }
            let radii = |s: f64| if s > 0.05 { box_radii(s, 3) } else { Vec::new() };
            Some(Reach::Boxes { h: radii(sx), v: radii(sy) })
        }
        "ec.blur.fastbox" => {
            let r = p.f("radius").max(0.0) * scale;
            let (rx, ry) = ((r * cx * kx).round().min(1.0e5), (r * cy * ky).round().min(1.0e5));
            if !(rx.is_finite() && ry.is_finite()) {
                return None;
            }
            let (rx, ry) = (rx as usize, ry as usize);
            if rx == 0 && ry == 0 {
                return Some(Reach::Pixel);
            }
            let it = p.f("iterations").round().clamp(1.0, 50.0) as usize;
            Some(Reach::Boxes { h: vec![rx; it], v: vec![ry; it] })
        }
        "ec.stylize.mosaic" => {
            let blocks = |k: &str| p.f(k).round().max(1.0) as usize;
            Some(Reach::Tiles { blocks: [blocks("horizontal"), blocks("vertical")], sharp: p.b("sharpColors") })
        }
        _ if PER_PIXEL.contains(&id) || id.starts_with("ec.control.") => Some(Reach::Pixel),
        _ => None,
    }
}

/// The stack as the GPU would run it, without running it: every step's reach, or `None` when
/// a step's isn't known or a step would run on the CPU.
struct Plan {
    scale: f64,
    steps: Vec<Reach>,
    known: bool,
}

impl FxTarget for Plan {
    fn gpu(&mut self, steps: &[FxStep]) -> bool {
        for s in steps {
            match reach(s, self.scale) {
                Some(r) => self.steps.push(r),
                None => self.known = false,
            }
        }
        true
    }
    fn cpu(&mut self, _f: &mut dyn FnMut(Buf) -> Buf) {
        self.known = false;
    }
    fn shift(&mut self, _d: [f64; 2]) {}
}

/// `[x0, y0, x1, y1]` in comp canvas pixels.
pub(crate) type Span = [i64; 4];

/// Box-blur blocks along rows (`effects::box_passes_owned`).
const ROW_BLOCK: i64 = 256;

/// Box-blur blocks along columns for radius `r` (`effects::box_passes_owned`).
fn column_block(r: i64) -> i64 {
    r.saturating_mul(4).clamp(32, 256)
}

/// The window of the comp that `steps` (in stack order) must run on so that their result is
/// exact over `out`, in a `full` comp: what each step reads to make what the next one needs,
/// from the last step back (all of it held at once: the union).
pub(crate) fn window_for(steps: &[Reach], out: Span, full: [u32; 2]) -> Span {
    let (fw, fh) = (i64::from(full[0]), i64::from(full[1]));
    let fit = |n: Span| [n[0].max(0), n[1].max(0), n[2].min(fw), n[3].min(fh)];
    let union = |a: Span, b: Span| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])];
    let mut need = fit(out);
    let mut all = need;
    for step in steps.iter().rev() {
        if need[0] >= need[2] || need[1] >= need[3] {
            break;
        }
        match step {
            Reach::Pixel => {}
            Reach::Boxes { h, v } => {
                // Rows first, then columns: back from the last column pass. A pixel's sum
                // starts at its block's first pixel, `r` before it (edges repeat: the comp's
                // edge is the window's there).
                let passes = h.iter().map(|&r| (false, r)).chain(v.iter().map(|&r| (true, r)));
                for (vertical, r) in passes.rev().filter(|p| p.1 > 0) {
                    let r = i64::try_from(r).unwrap_or(i64::MAX);
                    let (lo, hi, block) = if vertical { (1, 3, column_block(r)) } else { (0, 2, ROW_BLOCK) };
                    need[lo] = need[lo].div_euclid(block).saturating_mul(block).saturating_sub(r);
                    need[hi] = need[hi].saturating_add(r);
                    need = fit(need);
                }
            }
            Reach::Tiles { blocks, sharp } => {
                let (x, y) = (tiles(need[0], need[2], fw, blocks[0], *sharp), tiles(need[1], need[3], fh, blocks[1], *sharp));
                need = fit([x.0, y.0, x.1, y.1]);
            }
        }
        all = union(all, need);
    }
    all
}

/// The pixels along one axis of a `full`-pixel comp that Mosaic (`fx_distort::mosaic`, `blocks`
/// tiles) reads to make `[lo, hi)`: the whole tiles they sit in, or with Sharp Colors their
/// centres.
fn tiles(lo: i64, hi: i64, full: i64, blocks: usize, sharp: bool) -> (i64, i64) {
    if lo >= hi || full <= 0 {
        return (lo, hi);
    }
    let c = (full as f64 / blocks.max(1) as f64).max(1.0);
    if sharp {
        let centre = |i: i64| ((((i as f64 / c).floor() + 0.5) * c) as i64).clamp(0, full - 1);
        return (centre(lo), centre(hi - 1) + 1);
    }
    let n = (full as f64 / c).ceil() as usize;
    let tile = |x: i64| ((x as f64 / c) as usize).min(n.saturating_sub(1));
    let all: Vec<usize> = (0..full).map(tile).collect();
    let (first, last) = (tile(lo), tile(hi - 1));
    (all.partition_point(|&t| t < first) as i64, all.partition_point(|&t| t <= last) as i64)
}

/// Where the part `src` (buffer pixels) of `buf` lands on the canvas when `layer` places it
/// (every motion-blur sub-sample), grown by what resampling reads; `None` when not known (a
/// perspective transform).
fn placed_part(r: &Renderer, ctx: &EvalCtx, layer: &Layer, buf: &Buf, src: Rect) -> Option<Rect> {
    if buf.img.is_empty() || src.is_empty() || src.width() <= 0.0 || src.height() <= 0.0 {
        return Some(Rect::new(0.0, 0.0, 0.0, 0.0));
    }
    let mut out: Option<Rect> = None;
    for m in &r.placement(ctx, layer, buf).matrices {
        if !m.is_affine() {
            return None;
        }
        let [[a, b, _], [c, d, _], _] = m.0;
        // Bicubic sampling reads two source pixels around a point, in canvas pixels.
        let k = a.hypot(c).max(b.hypot(d));
        let rect = m.map_rect(&src).inflate(2.0 + 3.0 * k);
        out = Some(out.map_or(rect, |o| o.union(&rect)));
    }
    out.filter(|o| [o.x0, o.y0, o.x1, o.y1].iter().all(|v| v.is_finite()))
}

/// The pixels of `buf` with alpha above 0 (`None`: none), remembered for the last few
/// buffers (a static footprint is the same cached buffer every frame).
fn alpha_bounds(buf: &Arc<Buf>) -> Option<Rect> {
    type Seen = Vec<(Weak<Buf>, Option<Rect>)>;
    static SEEN: Mutex<Seen> = Mutex::new(Vec::new());
    if let Ok(seen) = SEEN.lock()
        && let Some((_, b)) = seen.iter().find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(buf)) && w.strong_count() > 0)
    {
        return *b;
    }
    let img = &buf.img;
    let width = img.width as usize;
    let mut b: Option<[usize; 4]> = None;
    if width > 0 {
        for (y, row) in img.data.chunks(width).enumerate() {
            let (Some(x0), Some(x1)) = (row.iter().position(|p| p[3] > 0.0), row.iter().rposition(|p| p[3] > 0.0)) else { continue };
            b = Some(match b {
                Some([a0, b0, a1, _]) => [a0.min(x0), b0, a1.max(x1 + 1), y + 1],
                None => [x0, y, x1 + 1, y + 1],
            });
        }
    }
    let b = b.map(|[x0, y0, x1, y1]| Rect::new(x0 as f64, y0 as f64, x1 as f64, y1 as f64));
    if let Ok(mut seen) = SEEN.lock() {
        seen.retain(|(w, _)| w.strong_count() > 0);
        if seen.len() >= 32 {
            seen.remove(0);
        }
        seen.push((Arc::downgrade(buf), b));
    }
    b
}

/// The part of a `w`×`h` canvas where the adjustment layer's footprint × track matte can be
/// non-zero: where the walk would draw them (`None`: not known).
fn shown(
    r: &Renderer,
    ctx: &EvalCtx,
    layer: &Layer,
    foot: &Footprint,
    foot_size: (u32, u32),
    matte: Option<&Option<MatteSource>>,
    (w, h): (u32, u32),
) -> Option<Span> {
    let rect = |b: ops::Bounds| Rect::new(f64::from(b[0]), f64::from(b[1]), f64::from(b[2]), f64::from(b[3]));
    let mut win = rect(walk::placed_bounds(r, ctx, layer, foot.shape(), foot_size, (w, h)));
    // Masks (the footprint is the whole solid with its masks applied): where it isn't clear.
    if let Footprint::Cpu(buf) = foot
        && layer.masks().is_some_and(|m| !m.children.is_empty())
        && win.width() * win.height() * 5.0 > f64::from(w) * f64::from(h) * 3.0
    {
        win = match alpha_bounds(buf) {
            Some(part) => win.intersect(&placed_part(r, ctx, layer, buf, part)?),
            None => return Some([0; 4]),
        };
    }
    if let (Some((m, kind)), Some(src)) = (r.track_matte(ctx, layer), matte)
        && matches!(kind, MatteKind::Alpha | MatteKind::Luma)
    {
        // Clear outside the matte layer (an inverted matte is opaque there).
        let matte = src.as_ref().map_or([0; 4], |src| walk::matte_reach(r, ctx, m, src, (w, h)));
        win = win.intersect(&rect(matte));
    }
    let win = win.intersect(&Rect::from_size(f64::from(w), f64::from(h)));
    if win.is_empty() || win.width() <= 0.0 || win.height() <= 0.0 {
        return Some([0; 4]);
    }
    let win = win.round_out();
    Some([win.x0 as i64, win.y0 as i64, win.x1 as i64, win.y1 as i64])
}

/// An adjustment solid's footprint when the GPU makes it (all ones: no CPU buffer to build and
/// upload), see `Renderer::adjustment_footprint_shape`.
pub(crate) fn plain_footprint(r: &Renderer, layer: &Layer) -> Option<Buf> {
    if !enabled() {
        return None;
    }
    r.adjustment_footprint_shape(layer)
}

/// A `w`×`h` image of ones, filled by this encoder (once per frame and size).
pub(crate) fn ones(e: &mut Enc, w: u32, h: u32) -> GpuImage {
    if let Some(o) = e.ones.as_ref().filter(|o| (o.width, o.height) == (w, h)) {
        return o.clone();
    }
    let img = e.scratch(w, h);
    e.fill(&img, 1.0);
    e.ones = Some(img.clone());
    img
}

/// `src`'s `w`×`h` region at (`x`, `y`) as a new image (codes stay codes).
fn crop_px(e: &mut Enc, src: &Pixels, x: u32, y: u32, w: u32, h: u32) -> Option<Pixels> {
    match src {
        Pixels::Float(g) => {
            let mut out = e.crop(g, x, y, w, h);
            out.levels = g.levels;
            Some(Pixels::Float(out))
        }
        Pixels::Codes(c) => {
            let out = e.scratch_codes(w, h)?;
            e.copy_texture_region(c.texture(), out.texture(), x, y, w, h);
            Some(Pixels::Codes(out))
        }
    }
}

/// `canvas` with `part` at (`x`, `y`): in place when this render owns it, else on a copy.
fn put(e: &mut Enc, canvas: &mut Pixels, part: &GpuImage, x: u32, y: u32) -> Option<()> {
    let (w, h) = canvas.size();
    let mut out = match &*canvas {
        Pixels::Float(g) if e.owns(g) => g.clone(),
        Pixels::Float(g) => {
            let out = e.scratch(w, h);
            e.copy_into(g, &out, 0, 0);
            out
        }
        codes => codes.float(e)?,
    };
    e.copy_region_to(part, &out, 0, 0, part.width, part.height, x, y);
    // As `ops::adjust_mix` leaves the whole canvas: not quantised.
    out.levels = None;
    *canvas = out.into();
    Some(())
}

/// [`crate::walk`]'s adjustment layer through a window of the comp (see the module docs):
/// `foot` is its footprint (`foot_px` its pixels on the GPU), `matte` its track matte's content
/// (taken once it is drawn: the walk then makes it again). `None` (the canvas unchanged) when the
/// whole comp must run instead.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    e: &mut Enc,
    r: &Renderer,
    ctx: &EvalCtx,
    layer: &Layer,
    foot: &Footprint,
    foot_px: &Pixels,
    matte: &mut Option<Option<MatteSource>>,
    canvas: &mut Pixels,
    opacity: f32,
) -> Option<()> {
    if !enabled() {
        return None;
    }
    let (w, h) = canvas.size();
    // One placement each (motion-blur sub-samples: the whole comp).
    if r.placement(ctx, layer, foot.shape()).matrices.len() != 1 {
        return declined(layer, "a motion-blurred footprint");
    }
    if let (Some((m, _)), Some(Some(src))) = (r.track_matte(ctx, layer), matte.as_ref())
        && r.placement(ctx, m, &src.shape).matrices.len() != 1
    {
        return declined(layer, "a motion-blurred track matte");
    }
    let Some(out) = shown(r, ctx, layer, foot, foot_px.size(), matte.as_ref(), (w, h)) else {
        return declined(layer, "footprint or matte bounds not known");
    };
    if out[0] >= out[2] || out[1] >= out[3] {
        // The matte is clear everywhere: the comp below stays as it is.
        if let (Some((m, _)), Some(Some(src))) = (r.track_matte(ctx, layer), matte.take()) {
            walk::matte_keep(e, r, ctx, m, src);
        }
        #[cfg(test)]
        WINDOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Some(());
    }
    let mut plan = Plan { scale: r.opts.scale, steps: Vec::new(), known: true };
    r.run_effects_on(ctx, layer, true, &mut plan);
    if !plan.known {
        return declined(layer, "an effect's reach is not known");
    }
    let [x0, y0, x1, y1] = window_for(&plan.steps, out, [w, h]);
    let (Ok(x), Ok(y), Ok(ww), Ok(wh)) = (u32::try_from(x0), u32::try_from(y0), u32::try_from(x1 - x0), u32::try_from(y1 - y0)) else {
        return None;
    };
    // A window over most of the comp saves little: the whole comp in one go.
    if ww == 0 || wh == 0 || u64::from(ww) * u64::from(wh) * 5 > u64::from(w) * u64::from(h) * 3 {
        return declined(layer, "the window covers most of the comp");
    }
    let win = [x, y, x + ww, y + wh];
    // The footprint × track matte over the window only (the pixels the walk makes there).
    let pl = r.placement(ctx, layer, foot.shape());
    let [fm] = pl.matrices.as_slice() else { return None };
    let Some(mut fp) = ops::warp_window(e, foot_px, fm, pl.sampling, 1.0, pl.seed, (w, h), win) else {
        return declined(layer, "no windowed warp kernel");
    };
    if let Some((m, kind)) = r.track_matte(ctx, layer) {
        let mimg = match matte.take() {
            Some(Some(src)) => match walk::matte_window(e, r, ctx, m, src, (w, h), win) {
                Some(img) => img,
                None => return declined(layer, "the track matte can't be drawn in a window"),
            },
            Some(None) => e.zeros(ww, wh),
            None => return None,
        };
        ops::matte_in(e, &mut fp, &mimg, kind, None);
    }
    let pipe = r.pipe();
    // 8 bpc codes stay codes: the first effects and the mix read them.
    let comp = crop_px(e, canvas, x, y, ww, wh)?;
    // Effects run in the working space, not the linear blending space.
    let below: Pixels = match pipe.from_blend() {
        Some(c) => {
            let f = comp.float(e)?;
            ops::convert(e, &f, &c).into()
        }
        None => comp.clone(),
    };
    // The window's pixels sit where they are in the comp (the layer's space: the comp's).
    let offset = [-f64::from(x), -f64::from(y)];
    e.fx_frame = Some(Frame { at: [x, y], size: [ww, wh], full: [w, h] });
    let done = {
        let mut fx = GpuFx::gpu_only(e, below, offset, r.opts.scale, pipe.levels);
        r.run_effects_on(ctx, layer, true, &mut fx);
        fx.finish()
    };
    e.fx_frame = None;
    let Some((mut adjusted, at, _)) = done else {
        return declined(layer, "an effect declined the GPU");
    };
    if (adjusted.width, adjusted.height) != (ww, wh) || (at[0] - offset[0]).abs() > 1e-6 || (at[1] - offset[1]).abs() > 1e-6 {
        return declined(layer, "an effect moved or resized the buffer");
    }
    if let Some(c) = pipe.to_blend() {
        adjusted = ops::convert(e, &adjusted, &c);
    }
    let mixed = ops::adjust_mix(e, &comp, &adjusted, &fp, opacity);
    put(e, canvas, &mixed, x, y)?;
    log::trace!("adjustment {:?}: effects on a {ww}x{wh} window of the {w}x{h} comp at ({x}, {y})", layer.name);
    #[cfg(test)]
    WINDOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(())
}

/// The whole comp for `layer`, and why (log target `effectcraft_gpu::adjust_window`).
fn declined(layer: &Layer, why: &str) -> Option<()> {
    log::trace!("adjustment {:?} runs on the whole comp: {why}", layer.name);
    #[cfg(test)]
    if let Ok(mut n) = NOTES.lock() {
        n.push(format!("{}: {why}", layer.name));
    }
    None
}

/// Tests: why adjustment layers ran on the whole comp.
#[cfg(test)]
pub(crate) static NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_steps_need_only_the_window() {
        assert_eq!(window_for(&[Reach::Pixel, Reach::Pixel], [100, 50, 300, 90], [640, 360]), [100, 50, 300, 90]);
    }

    #[test]
    fn box_passes_read_back_to_their_block_start() {
        // One row pass of radius 5: blocks of 256 along rows, the pixel at 300 sums from 256 - 5.
        let s = window_for(&[Reach::Boxes { h: vec![5], v: vec![] }], [300, 40, 400, 60], [1920, 1080]);
        assert_eq!(s, [251, 40, 405, 60]);
        // A column pass of radius 10: blocks of 40 rows.
        let s = window_for(&[Reach::Boxes { h: vec![], v: vec![10] }], [300, 130, 400, 200], [1920, 1080]);
        assert_eq!(s, [300, 110, 400, 210]);
        // Three of each (a Gaussian): rows after columns, back to front; clipped to the comp.
        let s = window_for(&[Reach::Boxes { h: vec![3, 3, 4], v: vec![3, 3, 4] }], [10, 500, 1910, 520], [1920, 1080]);
        // Columns: 500 → 480 - 4, → 448 - 3, → 416 - 3 (blocks of 32 rows); rows reach both edges.
        assert_eq!(s, [0, 413, 1920, 530]);
    }

    #[test]
    fn mosaic_reads_whole_tiles_of_the_comp() {
        // 160 tiles across 1920: 12 pixels each.
        let s = window_for(&[Reach::Tiles { blocks: [160, 90], sharp: false }], [100, 50, 130, 61], [1920, 1080]);
        assert_eq!(s, [96, 48, 132, 72]);
        // Sharp Colors: only the centres (the output window is held too).
        let s = window_for(&[Reach::Tiles { blocks: [160, 90], sharp: true }], [100, 50, 130, 61], [1920, 1080]);
        assert_eq!(s, [100, 50, 130, 67]);
    }
}
