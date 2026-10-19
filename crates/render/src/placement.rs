//! Where a processed layer buffer lands in the output: the layer → output matrices, the
//! region of interest, motion-blur sub-samples, sampling, and the changed region.

use crate::color::Region;
use crate::motion_blur::MotionMemo;
use crate::{EvalCtx, Image, Renderer, color, source_size, styles};
use effectcraft_color::BlendMode;
use effectcraft_effects::{Buf, EffectEnv};
use effectcraft_geom::{Mat3, Rect, vec2};
use effectcraft_project::{GroupKind, ItemKind, Layer, LayerSource, Quality, Sampling};
use effectcraft_raster::{MipChain, WarpOpts, accumulate_warp_at, composite_warp_at, pool, warp_bounds};

impl<'a> Renderer<'a> {
    pub(crate) fn sampling(&self, layer: &Layer) -> effectcraft_raster::Sampling {
        if self.quality(layer) == Quality::Draft {
            // Draft quality: no interpolation (nearest neighbour), as After Effects' Draft.
            effectcraft_raster::Sampling::Nearest
        } else if self.opts.draft {
            effectcraft_raster::Sampling::Bilinear
        } else if layer.switches.sampling == Sampling::Bicubic {
            effectcraft_raster::Sampling::Bicubic
        } else {
            effectcraft_raster::Sampling::Bilinear
        }
    }

    /// The region of interest of a top-level 2D frame: (x, y) offset of the output in output
    /// pixels and its size. `None` renders the whole comp (no ROI, or a nested comp).
    pub(crate) fn roi_offset(&self) -> Option<(f64, f64, u32, u32)> {
        if let Some(w) = self.window {
            return Some((f64::from(w.x), f64::from(w.y), w.w.max(1), w.h.max(1)));
        }
        let r = self.opts.roi.filter(|_| self.top)?;
        let s = self.opts.scale;
        if r[2] <= 0.0 || r[3] <= 0.0 {
            return None;
        }
        Some(((r[0] * s).round(), (r[1] * s).round(), ((r[2] * s).round() as u32).max(1), ((r[3] * s).round() as u32).max(1)))
    }

    /// The region of interest's offset in output pixels for 3D projection (0 without one).
    pub(crate) fn out_offset(&self) -> (f64, f64) {
        self.roi_offset().map_or((0.0, 0.0), |(x, y, _, _)| (x, y))
    }

    /// Scaled comp pixel → output pixel: the region of interest's offset.
    fn out_matrix(&self) -> Mat3 {
        match self.roi_offset() {
            Some((x, y, _, _)) => Mat3::translate(vec2(-x, -y)),
            None => Mat3::IDENTITY,
        }
    }

    /// Layer → (top-level) comp matrix at the context time: through the collapsed precomps.
    pub(crate) fn comp_matrix(&self, ctx: &EvalCtx, layer: &Layer) -> Mat3 {
        let (l2c, _) = ctx.layer_to_comp(layer);
        self.outer.map_or(l2c, |o| o * l2c)
    }

    /// Buffer pixel → output pixel matrix for the layer at the context time.
    pub(crate) fn buf_matrix(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, mo: &MotionMemo) -> Mat3 {
        self.place_matrix(mo.still(|| self.comp_matrix(ctx, layer)), buf)
    }

    /// Buffer pixel → output pixel matrix for a layer → (top-level) comp matrix.
    pub(crate) fn place_matrix(&self, l2c: Mat3, buf: &Buf) -> Mat3 {
        let s = self.opts.scale;
        self.out_matrix()
            * Mat3::scale(vec2(s, s))
            * l2c
            * Mat3::scale(vec2(1.0 / buf.scale, 1.0 / buf.scale))
            * Mat3::translate(vec2(-buf.offset[0], -buf.offset[1]))
    }

    /// Canvas pixels that compositing `buf` for `layer` can change: the transformed buffer's
    /// bounds plus the resampling footprint. Stencil/silhouette modes, motion blur and
    /// perspective fall back to the whole canvas.
    ///
    /// A quantised buffer placed pixel-exactly (integer translation, Normal, 100%) over a blank
    /// canvas leaves exactly its own (quantised) pixels: nothing to re-quantise.
    pub(crate) fn footprint(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, opacity: f32, blank: bool, mo: &MotionMemo) -> Region {
        if layer.blend_mode.is_stencil() || self.mb_samples(ctx, layer, buf, mo) > 1 {
            return Region::Full;
        }
        let m = self.buf_matrix(ctx, layer, buf, mo);
        if !m.is_affine() {
            return Region::Full;
        }
        let matte = layer.track_matte.is_some_and(|tm| tm.layer != layer.id && ctx.comp.layer(tm.layer).is_some());
        let k = &m.0;
        let integral = |v: f64| v == v.round();
        if blank
            && layer.blend_mode == BlendMode::Normal
            && opacity == 1.0
            && !matte
            && !layer.preserve_transparency
            && self.pipe.to_blend().is_none()
            && k[0][0] == 1.0
            && k[1][1] == 1.0
            && k[0][1] == 0.0
            && k[1][0] == 0.0
            && integral(k[0][2])
            && integral(k[1][2])
        {
            return Region::Empty;
        }
        let (w, h) = (buf.img.width as f64, buf.img.height as f64);
        let c = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(x, y)| m.apply(vec2(x, y)));
        let (x0, x1) = (c.iter().map(|p| p.x).fold(f64::INFINITY, f64::min), c.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max));
        let (y0, y1) = (c.iter().map(|p| p.y).fold(f64::INFINITY, f64::min), c.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max));
        let [ox, oy] = self.canvas_origin;
        color::PxRect::covering(x0, y0, x1, y1, 3.0).map_or(Region::Full, |r| {
            Region::Rect(color::PxRect { x0: r.x0.saturating_sub(ox), y0: r.y0.saturating_sub(oy), x1: r.x1.saturating_sub(ox), y1: r.y1.saturating_sub(oy) })
        })
    }

    /// How a processed layer buffer lands in the output: one buffer → output matrix, or one per
    /// motion-blur sub-sample (averaged), with the layer's sampling filter and dissolve seed.
    pub fn placement(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf) -> Placement {
        self.placement_memo(ctx, layer, buf, &MotionMemo::default())
    }

    /// [`Self::placement`] sharing the layer's matrices with the draw's other buffers and steps.
    pub(crate) fn placement_memo(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, mo: &MotionMemo) -> Placement {
        let samples = self.mb_samples(ctx, layer, buf, mo);
        let (sampling, seed) = (self.sampling(layer), layer.id.0 as u32);
        if samples <= 1 {
            return Placement { matrices: vec![self.buf_matrix(ctx, layer, buf, mo)], sampling, seed };
        }
        let sh = self.shutter(ctx);
        let matrices = (0..samples).map(|i| self.buf_matrix_at(ctx, layer, buf, sh.offset(i, samples), mo)).collect();
        Placement { matrices, sampling, seed }
    }

    /// Draw a processed layer into `target` (transform, motion blur) with `mode`/`opacity`.
    pub(crate) fn place(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, target: &mut Image, mode: BlendMode, opacity: f32) {
        self.place_memo(ctx, layer, buf, target, mode, opacity, &MotionMemo::default());
    }

    /// [`Self::place`] sharing the layer's matrices through `mo` (see [`MotionMemo`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn place_memo(&self, ctx: &EvalCtx, layer: &Layer, buf: &Buf, target: &mut Image, mode: BlendMode, opacity: f32, mo: &MotionMemo) {
        self.place_at(ctx, layer, buf, target, [0, 0], mode, opacity, mo);
    }

    /// [`Self::place_memo`] into `target`, the window of the canvas at `origin` (see
    /// [`Self::reach`]): exactly the canvas's pixels there (the canvas itself being the window of
    /// the output at [`Renderer::canvas_origin`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn place_at(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        buf: &Buf,
        target: &mut Image,
        origin: [u32; 2],
        mode: BlendMode,
        opacity: f32,
        mo: &MotionMemo,
    ) {
        let pl = self.placement_memo(ctx, layer, buf, mo);
        let origin = [origin[0].saturating_add(self.canvas_origin[0]), origin[1].saturating_add(self.canvas_origin[1])];
        let opts = WarpOpts { sampling: pl.sampling, opacity, mode, seed: pl.seed, clip: None };
        // One minification chain for every sub-sample (built only when the layer is shrunk).
        let mips = MipChain::new(&buf.img);
        if let [m] = pl.matrices.as_slice() {
            composite_warp_at(target, origin, &mips, m, &opts);
            return;
        }
        // Sub-samples sum straight into one buffer (each warp is row-parallel), as large as they
        // reach (stencil modes also change what they don't cover: the whole target).
        let whole = Window { x: origin[0], y: origin[1], w: target.width, h: target.height };
        let win = if mode.is_stencil() { whole } else { whole.clip(reach_of(&[(&pl, buf)])) };
        if win.is_empty() {
            return;
        }
        let mut acc = pool::zeroed(win.w, win.h);
        let k = 1.0 / pl.matrices.len() as f32;
        for m in &pl.matrices {
            accumulate_warp_at(&mut acc, win.origin(), &mips, m, opts.sampling, k);
        }
        target.blend_from_at(origin, &acc, win.origin(), mode, opacity, pl.seed);
        pool::recycle(acc);
    }

    /// Whether a 2D layer certainly draws nothing on a `w`×`h` canvas, before its source is
    /// rendered: its source rectangle (with a margin for the buffer's rounding and the filters),
    /// grown by what its effects and layer styles can add, lands outside the canvas. Only layers
    /// whose pixels stay inside a rectangle known up front qualify: solids, precomps and raster
    /// footage (their source size), text and shapes (the buffer their rasteriser makes); with
    /// effects whose growth is known (see [`Self::fx_growth`]) and styles that don't stencil;
    /// not in a stencil mode (which changes the comp outside the layer) and not seen through a
    /// perspective. A motion-blurred layer qualifies when its buffer is known exactly (no
    /// effects that grow it, no styles; solids, precomps and text or shapes whose content holds
    /// still): then every sub-sample it would be drawn at must miss the canvas.
    pub(crate) fn off_canvas(&self, ctx: &EvalCtx, layer: &Layer, (w, h): (u32, u32), mo: &MotionMemo) -> bool {
        #[cfg(test)]
        if tests::NO_CULL.with(std::cell::Cell::get) {
            return false;
        }
        let [ox, oy] = self.canvas_origin.map(f64::from);
        let canvas = Rect::new(ox, oy, ox + w as f64, oy + h as f64);
        let Some(rects) = self.drawn_rects(ctx, layer, mo, Some(canvas)) else { return false };
        let outside = |r: &Rect| {
            let r = r.inflate(4.0);
            // NaN bounds (a degenerate transform) never count as outside.
            let out = r.x1 < canvas.x0 || r.y1 < canvas.y0 || r.x0 > canvas.x1 || r.y0 > canvas.y1;
            out && [r.x0, r.y0, r.x1, r.y1].iter().all(|v| !v.is_nan())
        };
        rects.iter().all(outside)
    }

    /// Output pixels a 2D layer can draw, before its source is rendered (see
    /// [`Self::off_canvas`]): every pixel it changes lies within 4 pixels of one of these
    /// rectangles. `None` when that is not known up front. With `canvas` (the output pixels being
    /// drawn), text and shape layers whose anchor point lies on it are not measured (`None`).
    pub(crate) fn drawn_rects(&self, ctx: &EvalCtx, layer: &Layer, mo: &MotionMemo, canvas: Option<Rect>) -> Option<Vec<Rect>> {
        let source = match &layer.source {
            LayerSource::Solid { .. } | LayerSource::Comp { .. } => Bounded::Rect,
            LayerSource::Footage { item } if matches!(self.project.item(*item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if !crate::is_vector_footage(f)) => {
                Bounded::Rect
            }
            LayerSource::Text | LayerSource::Shape => Bounded::Drawn,
            _ => return None,
        };
        if layer.is_3d() || layer.blend_mode.is_stencil() {
            return None;
        }
        let s = self.opts.scale;
        let m = self.out_matrix() * Mat3::scale(vec2(s, s)) * mo.still(|| self.comp_matrix(ctx, layer));
        if !m.is_affine() {
            return None;
        }
        let fx = layer.effects().is_some_and(|g| !g.children.is_empty());
        let styled = styles::active(ctx, layer);
        let blurred = self.blurred(ctx, layer);
        // Layer-space rectangle of the source buffer, the smallest scale (buffer pixels per
        // layer unit) it can be drawn at, and the buffer exactly when it is known.
        let (r, scale, exact) = match source {
            Bounded::Rect => {
                let (sw, sh) = source_size(self.project, layer);
                if sw == 0 || sh == 0 {
                    return None;
                }
                // Footage and comp proxies keep their own pixels (any scale).
                let (scale, exact) = match &layer.source {
                    LayerSource::Solid { .. } => {
                        let k = self.solid_scale(sw, sh);
                        let px = |v: u32| ((v as f64 * k).ceil() as u32).max(1);
                        (Some(k), Some(([px(sw), px(sh)], [0.0; 2], k)))
                    }
                    // Sharp zoom: a nested comp drawn whole may be drawn below the output
                    // scale (see `source_window`), not known exactly here.
                    LayerSource::Comp { item } if self.proxy_for(*item).is_none() && self.sharp() => (Some(1.0), None),
                    LayerSource::Comp { item } if self.proxy_for(*item).is_none() => {
                        // `comp_frame_cpu`'s frame at the nested renderer's scale (≥ `s`).
                        let (_, bs) = self.precomp_renderer(layer, *item);
                        let px = |v: u32| ((v as f64 * bs).round() as u32).max(1);
                        (Some(s), Some(([px(sw), px(sh)], [0.0; 2], bs)))
                    }
                    _ => (None, None),
                };
                let pad = (0.1 * sw.max(sh) as f64).max(4.0);
                (Rect::new(-pad, -pad, sw as f64 + pad, sh as f64 + pad), scale, exact)
            }
            Bounded::Drawn => {
                // Measuring text or shapes costs a layout: only for layers whose anchor point is
                // off the canvas (most visible layers draw around it).
                if let Some(c) = canvas {
                    let a = layer.transform().map(|tr| ctx.v3(layer, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
                    let p = m.apply(vec2(a[0], a[1]));
                    if !(p.x < c.x0 || p.y < c.y0 || p.x > c.x1 || p.y > c.y1) {
                        return None;
                    }
                }
                let (bw, bh, off, k) = self.drawn_buf(ctx, layer)?;
                let r = buf_rect(bw, bh, off, k)?;
                (r, Some(k), Some(([bw, bh], off, k)))
            }
        };
        if blurred {
            // The sub-samples depend on the buffer: only for one known exactly (shape and text
            // content that moves within the shutter is drawn at several times, a larger buffer).
            let still = !fx || self.fx_growth(ctx, layer, s) == Some(0.0);
            let moves = matches!(source, Bounded::Drawn) && self.content_times(ctx, layer).is_some();
            let ([bw, bh], off, k) = exact.filter(|_| still && !styled && !moves)?;
            // The buffer as the draw places it: in and out of effect space (see
            // `apply_effects_timed`), the same float steps.
            let origin = crate::fx::fx_origin(ctx, layer, false);
            let mut buf = Buf { img: Image { width: bw, height: bh, data: vec![] }, offset: off, scale: k };
            buf.rebase(origin);
            buf.rebase([-origin[0], -origin[1]]);
            let pl = self.placement_memo(ctx, layer, &buf, mo);
            let whole = Rect::new(0.0, 0.0, bw as f64, bh as f64);
            return pl.matrices.iter().map(|m| m.is_affine().then(|| m.map_rect(&whole))).collect();
        }
        let mut grow = 0.0;
        if fx || styled {
            let scale = scale.filter(|k| k.is_finite() && *k > 0.0)?;
            if fx {
                grow += self.fx_growth(ctx, layer, scale)?;
            }
            if styled {
                // `styles::render` pads by its reach plus two pixels, rounded up.
                grow += styles::growth(ctx, layer)? + 3.0 / scale;
            }
            if !(grow.is_finite() && grow >= 0.0) {
                return None;
            }
        }
        Some(vec![m.map_rect(&r.inflate(grow))])
    }

    /// Layer units the layer's effect stack can grow its buffer by on every side (each effect's
    /// padding at buffer scale `scale` or more, pixels per layer unit), `None` when an enabled
    /// effect's growth is not known (it may resize the buffer, move it, or read other layers'
    /// pixels into it). The effects the stack skips (disabled, unknown, audio) add nothing.
    pub(crate) fn fx_growth(&self, ctx: &EvalCtx, layer: &Layer, scale: f64) -> Option<f64> {
        self.fx_pad(ctx, layer, scale, false)
    }

    /// Layer units around a pixel that the layer's effect stack reads to make it (what
    /// [`Self::fx_growth`] grows by, also where Repeat Edge Pixels keeps the buffer's size), `None`
    /// when not known.
    pub(crate) fn fx_reach(&self, ctx: &EvalCtx, layer: &Layer, scale: f64) -> Option<f64> {
        self.fx_pad(ctx, layer, scale, true)
    }

    /// [`Self::fx_growth`] (`reach` false) or [`Self::fx_reach`] (`reach` true).
    fn fx_pad(&self, ctx: &EvalCtx, layer: &Layer, scale: f64, reach: bool) -> Option<f64> {
        let Some(fx) = layer.effects().filter(|_| layer.switches.effects) else { return Some(0.0) };
        // Blur sizes of a layer whose effects run in comp space are in comp pixels.
        let c = crate::fx::fx_comp_space(ctx, layer).map_or([1.0; 2], |m| EffectEnv { comp_space: Some([m.0[0], m.0[1]]), ..Default::default() }.comp_px());
        let (c, px) = (c[0].max(c[1]), 1.0 / scale);
        let mut grow = 0.0;
        for g in fx.groups().filter(|g| g.enabled) {
            let GroupKind::Effect { effect } = &g.kind else { continue };
            let Some(spec) = effectcraft_effects::find(effect) else { continue };
            if effectcraft_effects::audio_fx::is_audio_effect(spec.id) {
                continue;
            }
            let p = self.effect_params(ctx, layer, g);
            // Each bound below follows the effect's own padding (crates/effects misc.rs, and the
            // same steps on the GPU): a pad of ⌈x⌉ + n buffer pixels is at most x + n + 1.
            grow += match spec.id {
                "ec.blur.gaussian" => {
                    let sigma = effectcraft_effects::gaussian_sigma(p.f("blurriness"));
                    if sigma <= 0.0 || (p.b("repeatEdge") && !reach) { 0.0 } else { 3.0 * sigma * c + px }
                }
                "ec.blur.fastbox" => {
                    let r = p.f("radius").max(0.0);
                    let it = p.f("iterations").round().clamp(1.0, 50.0);
                    if r <= 0.0 || (p.b("repeatEdge") && !reach) { 0.0 } else { (r * c + 0.5 * px) * it + px }
                }
                "ec.blur.directional" => {
                    let len = p.f("length");
                    if len <= 0.0 { 0.0 } else { len + 2.0 * px }
                }
                "ec.stylize.glow" => 3.0 * (p.f("radius") * effectcraft_effects::GLOW_SIGMA_PER_RADIUS).max(0.5 * px) + 3.0 * px,
                "ec.perspective.dropshadow" => (p.f("distance") + p.f("softness") * 1.5).max(0.0) + 3.0 * px,
                // Per-pixel colour changes and Expression Controls: the buffer as it is.
                "ec.color.levels"
                | "ec.color.curves"
                | "ec.color.huesaturation"
                | "ec.color.tint"
                | "ec.color.brightnesscontrast"
                | "ec.color.exposure"
                | "ec.channel.invert" => {
                    // Text and shape effects that can create alpha fill the expanded effect
                    // canvas (#659).
                    if matches!(layer.source, LayerSource::Text | LayerSource::Shape) && crate::fx::text_needs_canvas(spec) {
                        return None;
                    }
                    0.0
                }
                _ if spec.category == "Expression Controls" => 0.0,
                _ => return None,
            };
        }
        grow.is_finite().then_some(grow)
    }

    /// The buffer a text or shape layer's source draws into (see [`crate::text::render`] and
    /// [`crate::shapes::buffer_rect`]): its size, offset and scale; `None` when it draws nothing
    /// or is not known.
    fn drawn_buf(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(u32, u32, [f64; 2], f64)> {
        drawn_buf_at(ctx, layer, self.raster_scale(ctx, layer))
    }

    /// Layer-space rectangle of the buffer a text or shape layer's source draws into (see
    /// [`Self::drawn_buf`]).
    #[cfg(test)]
    fn drawn_rect(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Rect> {
        let (bw, bh, off, k) = self.drawn_buf(ctx, layer)?;
        buf_rect(bw, bh, off, k)
    }

    /// The window of a `w`×`h` canvas that drawing `bufs` for `layer` can change: their
    /// transformed bounds over every motion-blur sub-sample. Callers that clear or tint outside
    /// the layer (stencil modes) need the whole canvas instead.
    pub(crate) fn reach(&self, ctx: &EvalCtx, layer: &Layer, bufs: &[&Buf], mo: &MotionMemo, (w, h): (u32, u32)) -> Window {
        #[cfg(test)]
        if crate::tests_footprint::full_windows() {
            return Window { x: 0, y: 0, w, h };
        }
        let pls: Vec<Placement> = bufs.iter().map(|b| self.placement_memo(ctx, layer, b, mo)).collect();
        let items: Vec<(&Placement, &Buf)> = pls.iter().zip(bufs.iter().copied()).collect();
        // Clipped in output pixels, then moved into the canvas (whole pixels, no rounding).
        let [ox, oy] = self.canvas_origin;
        let out = Window { x: ox, y: oy, w, h }.clip(reach_of(&items));
        Window { x: out.x.saturating_sub(ox), y: out.y.saturating_sub(oy), ..out }
    }
}

/// The buffer a text or shape layer's source draws into at rasterisation scale `k` (see
/// [`Renderer::drawn_buf`]).
pub(crate) fn drawn_buf_at(ctx: &EvalCtx, layer: &Layer, k: f64) -> Option<(u32, u32, [f64; 2], f64)> {
    if !(k.is_finite() && k > 0.0) {
        return None;
    }
    let (bw, bh, off) = match &layer.source {
        LayerSource::Shape => crate::shapes::buffer_rect(ctx, layer, layer.props.sub("contents")?, k)?,
        LayerSource::Text => {
            // `text::render`'s buffer: every glyph's bounds grown by its stroke and blur.
            let geom = crate::text::text_geom(ctx, layer)?;
            let b = geom
                .glyphs
                .iter()
                .filter_map(|g| {
                    let pad = g.stroke_width + 2.0 + g.xf.blur[0].max(g.xf.blur[1]) * 1.5;
                    effectcraft_path::bounds(&[g.path()]).map(|b| b.inflate(pad, pad))
                })
                .reduce(|a, b| a.union(b))?;
            let b = b.intersect(kurbo::Rect::new(-20000.0, -20000.0, 20000.0, 20000.0));
            // On the layer pixel grid, as `text::render` places it.
            let (x0, y0) = ((b.x0 * k).floor(), (b.y0 * k).floor());
            let bw = ((b.x1 * k).ceil() - x0 + 4.0).clamp(1.0, 16384.0) as u32;
            let bh = ((b.y1 * k).ceil() - y0 + 4.0).clamp(1.0, 16384.0) as u32;
            (bw, bh, [-x0 + 2.0, -y0 + 2.0])
        }
        _ => return None,
    };
    Some((bw, bh, off, k))
}

/// What bounds a culled layer's pixels (see [`Renderer::off_canvas`]).
#[derive(Clone, Copy)]
enum Bounded {
    /// Its source rectangle.
    Rect,
    /// The buffer its text or shapes are rasterised into.
    Drawn,
}

/// Layer-space rectangle of a `bw`×`bh` buffer at `off` and scale `k` (finite bounds only).
fn buf_rect(bw: u32, bh: u32, off: [f64; 2], k: f64) -> Option<Rect> {
    let r = Rect::new(-off[0] / k, -off[1] / k, (bw as f64 - off[0]) / k, (bh as f64 - off[1]) / k);
    [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()).then_some(r)
}

/// The union of the warps' bounds of these placed buffers (output pixels).
fn reach_of(items: &[(&Placement, &Buf)]) -> Rect {
    let mut r = Rect::EMPTY;
    for (pl, buf) in items {
        for m in &pl.matrices {
            r = r.union(&warp_bounds(m, buf.img.width, buf.img.height));
        }
    }
    r
}

/// A window of the canvas: `w`×`h` pixels from (`x`, `y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Window {
    pub(crate) fn origin(&self) -> [u32; 2] {
        [self.x, self.y]
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Whether this is the whole `w`×`h` canvas.
    pub(crate) fn is_whole(&self, (w, h): (u32, u32)) -> bool {
        self.x == 0 && self.y == 0 && self.w == w && self.h == h
    }

    /// The canvas region this window covers (the whole canvas as [`Region::Full`]).
    pub(crate) fn region(&self, size: (u32, u32)) -> Region {
        if self.is_empty() {
            Region::Empty
        } else if self.is_whole(size) {
            Region::Full
        } else {
            Region::Rect(color::PxRect { x0: self.x, y0: self.y, x1: self.x + self.w, y1: self.y + self.h })
        }
    }

    /// The part of this window inside `r` (whole pixels, `r` rounded out).
    pub(crate) fn clip(&self, r: Rect) -> Window {
        let (x1, y1) = (self.x as f64 + self.w as f64, self.y as f64 + self.h as f64);
        let c = |v: f64, lo: f64, hi: f64| if v.is_nan() { lo } else { v.clamp(lo, hi) };
        let (a0, a1) = (c(r.x0.floor(), self.x as f64, x1), c(r.x1.ceil(), self.x as f64, x1));
        let (b0, b1) = (c(r.y0.floor(), self.y as f64, y1), c(r.y1.ceil(), self.y as f64, y1));
        if a1 <= a0 || b1 <= b0 {
            return Window { x: self.x, y: self.y, w: 0, h: 0 };
        }
        Window { x: a0 as u32, y: b0 as u32, w: (a1 - a0) as u32, h: (b1 - b0) as u32 }
    }
}

/// Where a layer buffer lands (see [`Renderer::placement`]).
#[derive(Clone, Debug)]
pub struct Placement {
    /// Buffer pixel → output pixel; several = motion-blur sub-samples (equal weights).
    pub matrices: Vec<Mat3>,
    pub sampling: effectcraft_raster::Sampling,
    /// Dissolve noise seed.
    pub seed: u32,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use effectcraft_color::Label;
    use effectcraft_keyframe::Keyframe;
    use effectcraft_keyframe::{TextDoc, Value};
    use effectcraft_project::build::{self, Ids};
    use effectcraft_project::styles::{self as st, GROUP};
    use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, Layer, LayerSource, Project, Solid};
    use effectcraft_time::{FrameRate, Tick};

    use crate::{EvalCtx, Image, NoFootage, RenderOpts, Renderer};

    thread_local! {
        /// Draw every layer (the path without culling).
        pub(super) static NO_CULL: Cell<bool> = const { Cell::new(false) };
    }

    fn setup() -> (Project, ItemId, Comp) {
        let mut p = Project::default();
        p.settings.bit_depth = BitDepth::Bpc8;
        let comp = Comp::new(200, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        (p, cid, comp)
    }

    fn text(p: &mut Project, comp: &Comp, stroke: f64) -> Layer {
        let mut l = build::layer(p, comp, "Text", LayerSource::Text, (comp.width, comp.height), None);
        let doc = TextDoc { text: "Hgj".into(), size: 40.0, stroke_width: stroke, apply_stroke: stroke > 0.0, ..Default::default() };
        l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
        l
    }

    /// A text layer whose characters are blurred by an animator.
    fn blurred_text(p: &mut Project, comp: &Comp) -> Layer {
        let mut l = text(p, comp, 0.0);
        let mut next = p.next_id;
        let mut ids = Ids(&mut next);
        let props = build::text_anim_props(&mut ids, "blur", false);
        let g = build::text_animator(&mut ids, "Animator", props);
        p.next_id = next;
        l.props.group_mut("text/animators").unwrap().children.push(g.into());
        l.props.group_mut("text/animators/#1/properties").unwrap().get_mut("blur").unwrap().value = Value::Vec2([6.0, 3.0]);
        l
    }

    /// A shape layer: a stroked, filled star whose group sits away from the layer's anchor.
    fn shape(p: &mut Project, comp: &Comp, offset: [f64; 2]) -> Layer {
        let mut l = build::layer(p, comp, "Shape", LayerSource::Shape, (comp.width, comp.height), None);
        let mut next = p.next_id;
        let mut ids = Ids(&mut next);
        let star = build::shape_star(&mut ids, true, 5.0, offset, 30.0, 12.0);
        let stroke = build::shape_stroke(&mut ids, [1.0, 1.0, 1.0, 1.0], 6.0);
        let fill = build::shape_fill(&mut ids, [0.9, 0.2, 0.1, 1.0]);
        let g = build::shape_group(&mut ids, "Star", vec![star, stroke, fill]);
        p.next_id = next;
        l.props.sub_mut("contents").unwrap().children.push(g.into());
        l
    }

    fn place(l: &mut Layer, pos: [f64; 2], rot: f64, scale: f64) {
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([pos[0], pos[1], 0.0]);
        l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(rot);
        l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([scale, scale, 100.0]);
    }

    /// The buffer measured before rendering is the source's (text: stroke, blur, continuous
    /// rasterisation; shapes: Continuously Rasterize), at full and half resolution.
    #[test]
    fn drawn_rect_is_the_source_buffers() {
        let (mut p, cid, comp) = setup();
        let mut layers = vec![text(&mut p, &comp, 0.0), text(&mut p, &comp, 5.0), blurred_text(&mut p, &comp), shape(&mut p, &comp, [40.0, -20.0])];
        let mut big = shape(&mut p, &comp, [0.0, 0.0]);
        big.switches.collapse = true;
        place(&mut big, [10.0, 10.0], 30.0, 340.0);
        let mut small = text(&mut p, &comp, 2.0);
        place(&mut small, [10.0, 10.0], 0.0, 13.0);
        layers.extend([big, small]);
        p.comp_mut(cid).unwrap().layers = layers.into();
        let c = p.comp(cid).unwrap();
        for scale in [1.0, 0.5] {
            let r = Renderer::new(&p, &NoFootage, RenderOpts { scale, ..Default::default() });
            let ctx = EvalCtx::new(&p, cid, c, Tick::ZERO);
            for (i, l) in c.layers.iter().enumerate() {
                let b = r.source(&ctx, l).unwrap();
                let k = b.scale;
                let want = [-b.offset[0] / k, -b.offset[1] / k, (b.img.width as f64 - b.offset[0]) / k, (b.img.height as f64 - b.offset[1]) / k];
                let got = r.drawn_rect(&ctx, l).unwrap();
                assert_eq!([got.x0, got.y0, got.x1, got.y1], want, "layer {i} at scale {scale}");
                // Exactly the buffer (motion blur's sub-samples depend on it).
                assert_eq!(r.drawn_buf(&ctx, l), Some((b.img.width, b.img.height, b.offset, b.scale)), "layer {i} at scale {scale}");
            }
        }
    }

    fn frame(p: &Project, cid: ItemId, cull: bool) -> (Image, Vec<String>) {
        frame_at(p, cid, cull, Tick::ZERO, 1.0)
    }

    fn frame_at(p: &Project, cid: ItemId, cull: bool, t: Tick, scale: f64) -> (Image, Vec<String>) {
        NO_CULL.with(|c| c.set(!cull));
        let prof = std::sync::Mutex::new(vec![]);
        let mut r = Renderer::new(p, &NoFootage, RenderOpts { scale, ..Default::default() });
        r.profile = Some(&prof);
        let img = r.comp_frame(cid, t);
        NO_CULL.with(|c| c.set(false));
        (img, prof.into_inner().unwrap().into_iter().map(|t| t.layer).collect())
    }

    /// Text and shape layers swept around and across the comp's edges (rotated, scaled, stroked,
    /// blurred, their content away from the anchor): culling skips the ones off the comp and
    /// the frames are identical to drawing every layer.
    #[test]
    fn culled_text_and_shapes_leave_frames_identical() {
        let (mut p, cid, comp) = setup();
        let sid = p.add_item("BG", Label::Red, None, ItemKind::Solid(Solid { color: [0.3, 0.4, 0.5], width: 200, height: 100, pixel_aspect: 1.0 }));
        let bg = build::layer(&mut p, &comp, "BG", LayerSource::Solid { item: sid }, (200, 100), None);
        let mut culled = 0;
        let mut drawn = 0;
        let spots = [-140.0, -70.0, -35.0, -12.0, 0.0, 100.0, 212.0, 235.0, 270.0, 340.0];
        for (n, &x) in spots.iter().enumerate() {
            for &y in &[-90.0, -30.0, -8.0, 50.0, 108.0, 140.0, 200.0] {
                let mut layers: Vec<Layer> = vec![];
                for (j, mut l) in [text(&mut p, &comp, 4.0), blurred_text(&mut p, &comp), shape(&mut p, &comp, [55.0, 0.0]), shape(&mut p, &comp, [0.0, -45.0])]
                    .into_iter()
                    .enumerate()
                {
                    place(&mut l, [x, y], 23.0 * (n + j) as f64, 60.0 + 35.0 * j as f64);
                    l.switches.collapse = j % 2 == 0;
                    l.name = format!("{j}");
                    layers.push(l);
                }
                layers.push(bg.clone());
                p.comp_mut(cid).unwrap().layers = layers.into();
                let (a, names) = frame(&p, cid, true);
                let (b, all) = frame(&p, cid, false);
                assert_eq!(all.len(), 5, "every layer drawn without culling");
                assert!(a == b, "at ({x}, {y}): culling changed the frame (drawn {names:?})");
                culled += all.len() - names.len();
                drawn += names.len() - 1;
            }
        }
        assert!(culled > 60 && drawn > 30, "culled {culled}, drawn {drawn}");
    }

    fn solid(p: &mut Project, comp: &Comp, w: u32, h: u32) -> Layer {
        let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [0.2, 0.9, 0.4], width: w, height: h, pixel_aspect: 1.0 }));
        build::layer(p, comp, "S", LayerSource::Solid { item: sid }, (w, h), None)
    }

    fn effect(p: &mut Project, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
        let spec = effectcraft_effects::find(id).unwrap();
        let mut next = p.next_id;
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [40.0, 30.0]);
        p.next_id = next;
        for (k, v) in vals {
            g.prop_mut(k).unwrap().value = v.clone();
        }
        l.props.sub_mut("effects").unwrap().children.push(g.into());
    }

    fn style(p: &mut Project, comp: &Comp, l: &mut Layer, name: &str, vals: &[(&str, Value)]) {
        let mut next = p.next_id;
        st::add_style(l, &mut Ids(&mut next), name, &comp.global_light, true).unwrap();
        p.next_id = next;
        for (k, v) in vals {
            l.props.prop_mut(&format!("{GROUP}/{name}/{k}")).unwrap().value = v.clone();
        }
    }

    /// Position keyframes from `a` (0 s) to `b` (1 s).
    fn moving(l: &mut Layer, a: [f64; 2], b: [f64; 2]) {
        let pos = l.props.prop_mut("transform/position").unwrap();
        pos.keys = vec![Keyframe::new(Tick::ZERO, Value::Vec3([a[0], a[1], 0.0])), Keyframe::new(Tick::from_seconds_f64(1.0), Value::Vec3([b[0], b[1], 0.0]))];
    }

    /// Layers with effects, layer styles and motion blur swept around and across the comp's
    /// edges (scaled, rotated, at full and half resolution): culling skips the ones that miss
    /// the comp, never one whose effect, style or motion reaches it, and every frame is identical
    /// to drawing every layer. Effects of unknown growth are never culled.
    #[test]
    fn culled_effects_styles_and_motion_blur_leave_frames_identical() {
        type Scene = Box<dyn Fn(&mut Project, &Comp, [f64; 2]) -> Layer>;
        let n = |v: f64| Value::Scalar(v);
        let fx = |id: &'static str, vals: Vec<(&'static str, Value)>| -> Scene {
            Box::new(move |p, c, at| {
                let mut l = solid(p, c, 40, 30);
                place(&mut l, at, 20.0, 80.0);
                effect(p, &mut l, id, &vals);
                l
            })
        };
        let styled = |name: &'static str, vals: Vec<(&'static str, Value)>| -> Scene {
            Box::new(move |p, c, at| {
                let mut l = solid(p, c, 40, 30);
                place(&mut l, at, -15.0, 110.0);
                style(p, c, &mut l, name, &vals);
                l
            })
        };
        let blurred = |src: u8| -> Scene {
            Box::new(move |p, c, at| {
                let mut l = match src {
                    0 => solid(p, c, 40, 30),
                    1 => text(p, c, 3.0),
                    2 => shape(p, c, [20.0, 0.0]),
                    _ => {
                        let mut nc = Comp::new(45, 35, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
                        nc.layers = vec![solid(p, &nc, 30, 20)].into();
                        let item = p.add_item("Nested", Label::Sandstone, None, ItemKind::Comp(nc.into()));
                        build::layer(p, c, "Nested", LayerSource::Comp { item }, (45, 35), None)
                    }
                };
                place(&mut l, at, 10.0, 90.0);
                moving(&mut l, [at[0] - 1500.0, at[1] - 600.0], [at[0] + 1500.0, at[1] + 600.0]);
                l.switches.motion_blur = true;
                l
            })
        };
        let scenes: Vec<(&str, Scene, bool)> = vec![
            ("gaussian 0", fx("ec.blur.gaussian", vec![]), true),
            ("gaussian 30", fx("ec.blur.gaussian", vec![("blurriness", n(30.0))]), true),
            ("gaussian 90 horizontal", fx("ec.blur.gaussian", vec![("blurriness", n(90.0)), ("dimensions", Value::Enum(1))]), true),
            ("gaussian repeat", fx("ec.blur.gaussian", vec![("blurriness", n(30.0)), ("repeatEdge", Value::Bool(true))]), true),
            ("fast box", fx("ec.blur.fastbox", vec![("radius", n(9.0)), ("iterations", n(3.0))]), true),
            ("directional", fx("ec.blur.directional", vec![("direction", n(70.0)), ("length", n(40.0))]), true),
            ("glow", fx("ec.stylize.glow", vec![("radius", n(60.0)), ("threshold", n(10.0))]), true),
            ("drop shadow", fx("ec.perspective.dropshadow", vec![("distance", n(45.0)), ("softness", n(20.0)), ("opacity", n(100.0))]), true),
            ("levels", fx("ec.color.levels", vec![]), true),
            ("unknown growth", fx("ec.distort.transform", vec![]), false),
            ("style drop shadow", styled("dropShadow", vec![("distance", n(40.0)), ("size", n(15.0))]), true),
            ("style outer glow", styled("outerGlow", vec![("size", n(35.0))]), true),
            ("style stroke", styled("stroke", vec![("size", n(25.0))]), true),
            ("motion blur solid", blurred(0), true),
            ("motion blur text", blurred(1), true),
            ("motion blur shape", blurred(2), true),
            ("motion blur precomp", blurred(3), true),
        ];
        let xs = [-160.0, -70.0, -40.0, -15.0, 100.0, 215.0, 245.0, 275.0, 360.0];
        let ys = [-90.0, -30.0, -10.0, 50.0, 110.0, 135.0, 190.0];
        for (what, scene, cullable) in &scenes {
            let (mut p, cid, comp) = setup();
            p.comp_mut(cid).unwrap().enable_motion_blur = true;
            let bg = solid(&mut p, &comp, 200, 100);
            let (mut culled, mut drawn) = (0, 0);
            for &x in &xs {
                for &y in &ys {
                    let mut l = scene(&mut p, &comp, [x, y]);
                    l.name = "L".into();
                    p.comp_mut(cid).unwrap().layers = vec![l, bg.clone()].into();
                    for scale in [1.0, 0.5] {
                        let t = Tick::from_seconds_f64(0.5);
                        let (a, names) = frame_at(&p, cid, true, t, scale);
                        let (b, all) = frame_at(&p, cid, false, t, scale);
                        assert!(all.iter().any(|n| n == "L"), "{what}: every layer drawn without culling");
                        assert!(a == b, "{what} at ({x}, {y}) scale {scale}: culling changed the frame (drawn {names:?})");
                        if !names.iter().any(|n| n == "L") {
                            culled += 1;
                        } else {
                            drawn += 1;
                        }
                    }
                }
            }
            if *cullable {
                assert!(culled > 16 && drawn > 10, "{what}: culled {culled}, drawn {drawn}");
            } else {
                assert_eq!(culled, 0, "{what}: never culled");
            }
        }
        // At rest (0.5 s) just right of the comp, its shutter sweeps it in: drawn (and seen), while the
        // same layer without motion blur is culled.
        let (mut p, cid, comp) = setup();
        p.comp_mut(cid).unwrap().enable_motion_blur = true;
        let mut l = solid(&mut p, &comp, 40, 30);
        moving(&mut l, [6245.0, 50.0], [-5755.0, 50.0]);
        l.name = "L".into();
        for mb in [true, false] {
            l.switches.motion_blur = mb;
            p.comp_mut(cid).unwrap().layers = vec![l.clone()].into();
            let (img, names) = frame_at(&p, cid, true, Tick::from_seconds_f64(0.5), 1.0);
            assert_eq!(names.len(), mb as usize, "motion blur {mb}: {names:?}");
            assert_eq!(img.get(199, 50)[3] > 0.0, mb, "motion blur {mb}: {:?}", img.get(199, 50));
        }
    }

    /// Timing: 1920×1080, 60 blurred, styled or motion-blurred 400×300 layers off the comp.
    #[test]
    #[ignore]
    fn time_off_comp_effects_styles_motion_blur() {
        let (mut p, cid, _) = setup();
        let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        *p.comp_mut(cid).unwrap() = comp.clone();
        p.comp_mut(cid).unwrap().enable_motion_blur = true;
        let mut layers = vec![];
        for i in 0..60 {
            let mut l = solid(&mut p, &comp, 400, 300);
            let at = [-600.0 - 10.0 * i as f64, 200.0 + 10.0 * i as f64];
            place(&mut l, at, 0.0, 100.0);
            match i % 3 {
                0 => effect(&mut p, &mut l, "ec.blur.gaussian", &[("blurriness", Value::Scalar(40.0))]),
                1 => style(&mut p, &comp, &mut l, "dropShadow", &[("distance", Value::Scalar(20.0)), ("size", Value::Scalar(10.0))]),
                _ => {
                    moving(&mut l, [at[0] - 30.0, at[1]], [at[0] + 30.0, at[1]]);
                    l.switches.motion_blur = true;
                }
            }
            layers.push(l);
        }
        layers.push(solid(&mut p, &comp, 1920, 1080));
        p.comp_mut(cid).unwrap().layers = layers.into();
        for cull in [false, true, false, true] {
            let t0 = std::time::Instant::now();
            let (_, names) = frame_at(&p, cid, cull, Tick::from_seconds_f64(0.5), 1.0);
            eprintln!("cull {cull}: {:.1} ms, {} layers drawn", t0.elapsed().as_secs_f64() * 1e3, names.len());
        }
    }

    /// Text effects that can create alpha (Levels, Invert alpha) fill the expanded effect canvas
    /// beyond the glyphs (#659): a text layer whose glyphs miss the comp but whose canvas reaches
    /// it is drawn, never culled. Colour effects that keep zero alpha still cull.
    #[test]
    fn text_alpha_creating_effects_are_not_culled() {
        let n = |v: f64| Value::Scalar(v);
        let cases: [(&str, Vec<(&str, Value)>, bool); 3] = [
            ("ec.color.levels", vec![("alphaOutBlack", n(1.0))], false),
            ("ec.channel.invert", vec![("channel", Value::Enum(effectcraft_effects::INVERT_ALPHA))], false),
            ("ec.color.tint", vec![("amount", n(70.0))], true),
        ];
        for (id, vals, cullable) in cases {
            let (mut p, cid, comp) = setup();
            let mut l = text(&mut p, &comp, 0.0);
            // Glyphs right of the comp, the comp-sized canvas centred on the anchor reaching into it.
            place(&mut l, [260.0, 50.0], 0.0, 100.0);
            effect(&mut p, &mut l, id, &vals);
            l.name = "L".into();
            p.comp_mut(cid).unwrap().layers = vec![l].into();
            let t = Tick::ZERO;
            let (a, names) = frame_at(&p, cid, true, t, 1.0);
            let (b, _) = frame_at(&p, cid, false, t, 1.0);
            assert!(a == b, "{id}: culling changed the frame (drawn {names:?})");
            assert_eq!(names.is_empty(), cullable, "{id}: drawn {names:?}");
            if !cullable {
                assert!(b.get(190, 50)[3] > 0.5, "{id}: the canvas reaches the comp");
            }
        }
    }
}
