//! Layer sources: solids, precomps, footage, text and shapes rasterised in layer space,
//! and their content bounds.

use crate::footage::frame_blend_mode;
use crate::placement::Window;
use crate::{ContentClip, EvalCtx, Image, RenderOpts, Renderer, blend_frames, color, frame_position, is_vector_footage, shapes, source_size, text};
use effectcraft_effects::Buf;
use effectcraft_geom::vec2;
use effectcraft_project::{FrameBlend, ItemKind, Layer, LayerSource};

/// The Essential Properties overrides of a precomp layer, evaluated at the context time.
pub fn essential_overrides(ctx: &EvalCtx, layer: &Layer) -> Vec<effectcraft_project::essential::Override> {
    use effectcraft_project::essential;
    let over = essential::overridden(layer);
    let Some(g) = essential::group(layer).filter(|_| !over.is_empty()) else { return vec![] };
    let mut out = vec![];
    g.walk("", &mut |_, p| {
        if over.contains(&p.uid)
            && let Some(control) = essential::control_of(&p.match_id)
        {
            out.push(essential::Override { control, value: ctx.value(layer, p) });
        }
    });
    out
}

impl<'a> Renderer<'a> {
    /// Whether `layer`'s processed buffer (source → masks → effects) is cached and the same as
    /// one frame earlier: drawing it from the layer cache (an accelerator uploads it once) beats
    /// re-running its effects every frame.
    pub fn layer_is_static(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        if self.cache.is_none() {
            return false;
        }
        let (scale, draft, blur) = (self.raster_scale(ctx, layer), self.opts.draft, self.mb_on(ctx, layer));
        let prev = ctx.time - ctx.comp.frame_rate.frame_duration();
        let now = crate::cache::layer_key(ctx, layer, scale, draft, blur);
        now.is_some() && now == crate::cache::layer_key(&ctx.at(prev), layer, scale, draft, blur)
    }

    /// Whether `layer`'s styles are the same as one frame earlier ([`Self::layer_is_static`]
    /// covers its content only): a card whose drop shadow moves has static content and styles
    /// that change every frame.
    pub fn styles_are_static(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        let prev = ctx.time - ctx.comp.frame_rate.frame_duration();
        crate::cache::styles_key(ctx, layer, 0) == crate::cache::styles_key(&ctx.at(prev), layer, 0)
    }

    /// Rasterisation scale of a layer's source: the output scale, or — for text and shape layers,
    /// which are always continuously rasterised (#390, #751), and Continuously Rasterize vector
    /// footage — the output scale times the layer's on-screen scale (quarter-octave steps, rounded
    /// up, so vector content is drawn at its final size and stays sharp when scaled up). The buffer
    /// stays within about 16 million pixels, or 1.25× the output's pixels for larger outputs.
    ///
    /// In the Layer panel ([`Renderer::layer_view`]) the on-screen scale is the panel's zoom.
    pub fn raster_scale(&self, ctx: &EvalCtx, layer: &Layer) -> f64 {
        let s = self.opts.scale;
        let continuous = match layer.source {
            LayerSource::Text | LayerSource::Shape => true,
            LayerSource::Footage { item } => {
                layer.switches.collapse && matches!(self.project.item(item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if is_vector_footage(f))
            }
            _ => false,
        };
        if !continuous {
            return s;
        }
        let (s, k) = match self.layer_view {
            Some(zoom) => (1.0, zoom),
            None => {
                let (l2c, _) = ctx.layer_to_comp(layer);
                let m = self.outer.map_or(l2c, |o| o * l2c);
                let a = layer.transform().map(|tr| ctx.v3(layer, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
                let p0 = m.apply(vec2(a[0], a[1]));
                let px = m.apply(vec2(a[0] + 1.0, a[1]));
                let py = m.apply(vec2(a[0], a[1] + 1.0));
                (s, ((px.x - p0.x).hypot(px.y - p0.y)).max((py.x - p0.x).hypot(py.y - p0.y)))
            }
        };
        if !k.is_finite() || k <= 1e-6 {
            return s;
        }
        // Sharp zoom (an output above comp resolution: the viewer's magnified region): drawn at
        // its size on screen, within the sharp-zoom pixel budget.
        if self.sharp() && self.layer_view.is_none() {
            let r = s * k.clamp(1.0 / 16.0, 64.0);
            return match content_bounds(ctx, layer) {
                Some(b) => self.sharp_fit(r, b[2] - b[0], b[3] - b[1], r.min(k.max(1.0))),
                None => r,
            };
        }
        // The step is rounded up past rounding noise only: an unscaled layer whose position isn't
        // a whole number maps the unit vectors to lengths a hair over 1, which must stay 1 (#390:
        // rounded up to 2^¼, unscaled text was drawn larger and resampled, softer).
        let onscreen = k;
        let mut k = 2f64.powf((k.log2() * 4.0 - 1e-6).ceil() / 4.0).clamp(1.0 / 16.0, 64.0);
        // Keep the buffer within about 16 million pixels. The cap is never under 1: a step that
        // doesn't enlarge needs no measuring (a text layout or shape walk per call).
        #[cfg(test)]
        let measure = k > 1.0 || tests::ALWAYS_MEASURE.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let measure = k > 1.0;
        if measure && let Some(b) = content_bounds(ctx, layer) {
            let area = ((b[2] - b[0]) * (b[3] - b[1])).max(1.0) * s * s;
            let capped = k.min((16.0e6 / area).sqrt().max(1.0));
            // Outputs past 16 Mpx (8K renders) raise the cap to their own size and a quarter for
            // effect growth, so a layer filling the frame is not drawn softer than on-screen.
            // Only steps the 16 Mpx cap held below the on-screen scale move (sharper).
            let (cw, ch) = self.out_comp.unwrap_or((ctx.comp.width, ctx.comp.height));
            let cap = cw as f64 * ch as f64 * s * s * 1.25;
            #[cfg(test)]
            let cap = if tests::FIXED_CAP.with(std::cell::Cell::get) { 0.0 } else { cap };
            k = if capped < onscreen && cap > 16.0e6 { self.raised_scale(ctx, layer, s, k.min((cap / area).sqrt())).max(capped) } else { capped };
        }
        s * k
    }

    /// A raised rasterisation step (see [`Self::raster_scale`]) kept within the rasterisers'
    /// 16384-pixel buffer sides, which would cut the content off: 0 when no step fits.
    fn raised_scale(&self, ctx: &EvalCtx, layer: &Layer, s: f64, k: f64) -> f64 {
        let side = |k: f64| crate::placement::drawn_buf_at(ctx, layer, s * k).map(|(w, h, ..)| w.max(h));
        // At scale 1 a side is ⌈extent⌉ + 4 pixels: the extent is at most that minus 4.
        let Some(unit) = side(1.0 / s).filter(|w| *w < 16384) else { return 0.0 };
        let k = if unit > 4 { k.min(16379.0 / (unit - 4) as f64 / s) } else { k };
        if side(k).is_some_and(|w| w < 16384) { k } else { 0.0 }
    }

    /// The renderer drawing precomp layer `layer`'s nested comp `item`, and the scale of the
    /// buffer it draws. Preserve resolution when nested: drawn at full size even when this comp
    /// renders smaller (the buffer's scale places it).
    pub(crate) fn precomp_renderer(&self, layer: &Layer, item: effectcraft_project::ItemId) -> (Renderer<'a>, f64) {
        let s = self.opts.scale;
        let full = s < 1.0 && self.project.comp(item).is_some_and(|nc| nc.preserve_resolution);
        let bs = if full { 1.0 } else { s };
        let base = self.nested_through(layer);
        (Renderer { opts: RenderOpts { scale: bs, ..base.opts }, ..base }, bs)
    }

    /// Source pixels of a layer in layer space (before masks/effects).
    pub(crate) fn source(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Buf> {
        match &layer.source {
            LayerSource::Solid { item } => {
                let ItemKind::Solid(sol) = &self.project.item(*item)?.kind else { return None };
                let s = self.solid_scale(sol.width, sol.height);
                let w = ((sol.width as f64 * s).ceil() as u32).max(1);
                let h = ((sol.height as f64 * s).ceil() as u32).max(1);
                if layer.switches.adjustment {
                    return Some(Buf { img: Image::filled(w, h, [1.0; 4]), offset: [0.0; 2], scale: s });
                }
                let mut c = sol.color;
                if let Some(conv) = self.pipe.authored_in() {
                    c = conv.apply(c);
                }
                let px = self.pipe.quantize_px([c[0], c[1], c[2], 1.0]);
                Some(Buf { img: Image::filled(w, h, px), offset: [0.0; 2], scale: s })
            }
            LayerSource::Comp { .. } => self.source_window(ctx, layer, None),
            LayerSource::Footage { item } => {
                let it = self.project.item(*item)?;
                let ItemKind::Footage(f) = &it.kind else { return None };
                if !f.has_video {
                    return None;
                }
                // Continuously rasterised vector footage: drawn at the on-screen scale.
                if layer.switches.collapse && is_vector_footage(f) {
                    let k = self.raster_scale(ctx, layer);
                    if let Some(img) = self.footage.vector_frame(*item, f, k) {
                        let mut buf = Buf { img: (*img).clone(), offset: [0.0; 2], scale: k };
                        if let Some(c) = self.pipe.media_in(f) {
                            color::convert(&mut buf.img, &c);
                        }
                        return Some(buf);
                    }
                }
                let (item, original, pf, lt) = self.footage_selection(ctx, layer)?;
                let img = self.footage_frame(ctx, layer, item, pf, lt)?;
                Some(self.footage_buf(&img, pf, original.width, original.height))
            }
            LayerSource::Text => Some(self.authored(text::render(ctx, layer, self.raster_scale(ctx, layer)))),
            LayerSource::Shape => layer.props.sub("contents").map(|c| self.authored(shapes::render(ctx, layer, c, self.raster_scale(ctx, layer)))),
            _ => None,
        }
    }

    /// A precomp layer's source: its nested comp's frame, or only the window `win` of it (see
    /// [`Self::precomp_clip`]), whose offset places it where those pixels are in the whole frame.
    pub(crate) fn source_window(&self, ctx: &EvalCtx, layer: &Layer, win: Option<Window>) -> Option<Buf> {
        let LayerSource::Comp { item } = &layer.source else { return None };
        if self.project.comp_contains(*item, ctx.comp_id) {
            return None;
        }
        // A composition proxy (a rendered still or movie) stands in for the comp.
        // Nothing before the nested comp starts or after it ends.
        let lt = ctx.nested_time(layer);
        if !self.project.comp(*item).is_some_and(|nc| nc.covers(lt)) {
            return None;
        }
        if let Some(pf) = self.proxy_for(*item)
            && let Some(nc) = self.project.comp(*item)
        {
            let img = self.footage.frame_with(*item, pf, lt, None, self.pipe.float_encoding())?;
            return Some(self.footage_buf(&img, pf, nc.width, nc.height));
        }
        // Essential Properties overrides render the nested comp with this instance's
        // values.
        let ov = essential_overrides(ctx, layer);
        let tmp = if ov.is_empty() { None } else { effectcraft_project::essential::with_overrides(self.project, *item, &ov) };
        let (mut base, mut bs) = self.precomp_renderer(layer, *item);
        // Sharp zoom: a nested comp drawn whole (not windowed to what shows) stays within the
        // sharp-zoom pixel budget, drawn at a lower scale and magnified.
        if win.is_none()
            && base.sharp()
            && let Some(nc) = self.project.comp(*item)
        {
            let fit = base.sharp_fit(bs, f64::from(nc.width), f64::from(nc.height), 1.0);
            if fit < bs {
                (base.opts.scale, bs) = (fit, fit);
            }
        }
        let base = base.windowed(win);
        let offset = window_offset(win);
        let sub = match &tmp {
            // A source cache was made without this instance's property overrides.
            Some(p) => Renderer { project: p, freeze: None, ..base },
            None => base,
        };
        // The nested frames share their layers' transforms (parents evaluated once).
        let frame = |t| crate::eval::with_transform_memo(sub.project, || sub.comp_frame(*item, t));
        // Frame blending between the nested comp's frames (time-stretched/remapped).
        let mode = frame_blend_mode(ctx, layer);
        if mode != FrameBlend::Off
            && let Some(nc) = self.project.comp(*item)
        {
            let (i, w) = frame_position(lt, nc.frame_rate);
            if w > 1e-6 {
                let a = frame(nc.frame_rate.tick_of(i));
                let b = frame(nc.frame_rate.tick_of(i + 1));
                return Some(Buf { img: blend_frames(mode, &a, &b, w as f32), offset, scale: bs });
            }
        }
        Some(Buf { img: frame(lt), offset, scale: bs })
    }

    /// This (nested) renderer drawing only the window `clip` of its comp's frame (see
    /// [`Self::precomp_clip`]); itself when `clip` is the whole frame.
    pub fn with_clip(&self, clip: ContentClip) -> Renderer<'a> {
        self.windowed(clip.window())
    }

    pub(crate) fn windowed(&self, win: Option<Window>) -> Renderer<'a> {
        match win {
            Some(w) => Renderer { window: Some(w), ..*self },
            None => Renderer { ..*self },
        }
    }

    /// Authored colours into the working space (linear working spaces).
    pub(crate) fn authored(&self, mut buf: Buf) -> Buf {
        if let Some(c) = self.pipe.authored_in() {
            color::convert(&mut buf.img, &c);
        }
        buf
    }
}

/// The offset of a buffer that holds the window `win` of a whole buffer at offset 0 (buffer
/// pixels; see [`Buf::offset`]).
pub(crate) fn window_offset(win: Option<Window>) -> [f64; 2] {
    win.map_or([0.0; 2], |w| [-f64::from(w.x), -f64::from(w.y)])
}

/// Layer-space bounds `[x0, y0, x1, y1]` of a layer's content at the context time (source size
/// for solids/footage/precomps, glyph bounds for text, painted bounds for shapes). Used for viewer
/// handles and hit testing.
pub fn content_bounds(ctx: &EvalCtx, layer: &effectcraft_project::Layer) -> Option<[f64; 4]> {
    match &layer.source {
        LayerSource::Text => {
            let glyphs = text::glyph_paths(ctx, layer);
            let paths: Vec<_> = glyphs.into_iter().map(|g| g.0).collect();
            effectcraft_path::bounds(&paths).map(|r| [r.x0, r.y0, r.x1, r.y1])
        }
        LayerSource::Shape => layer.props.sub("contents").and_then(|c| shapes::content_bounds(ctx, layer, c)).map(|r| [r.x0, r.y0, r.x1, r.y1]),
        LayerSource::Camera | LayerSource::Light { .. } => None,
        _ => {
            let (w, h) = source_size(ctx.project, layer);
            (w > 0 && h > 0).then_some([0.0, 0.0, w as f64, h as f64])
        }
    }
}

/// A mask/shape path as a kurbo path (for drawing overlays).
pub fn kurbo_path(sp: &effectcraft_keyframe::ShapePath) -> kurbo::BezPath {
    effectcraft_path::to_kurbo(sp)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use effectcraft_color::Label;
    use effectcraft_geom::vec2;
    use effectcraft_keyframe::{TextDoc, Value};
    use effectcraft_project::Layer;
    use effectcraft_project::build::{self, Ids};
    use effectcraft_project::{Comp, ItemKind, LayerSource, Project};
    use effectcraft_time::{FrameRate, Tick};

    use crate::{EvalCtx, NoFootage, RenderOpts, Renderer};

    thread_local! {
        /// Measure the content for every continuous layer (the path before the shortcut).
        pub(super) static ALWAYS_MEASURE: Cell<bool> = const { Cell::new(false) };
        /// Cap every buffer at 16 Mpx whatever the output size (the path before output-relative
        /// caps).
        pub(super) static FIXED_CAP: Cell<bool> = const { Cell::new(false) };
    }

    /// The rasterisation scale of text and Continuously Rasterize shape layers, scaled from tiny
    /// to beyond the 16 Mpx cap, at several output scales: the same without measuring content
    /// that isn't enlarged as with measuring every layer.
    #[test]
    fn raster_scale_skips_measuring_only_where_the_cap_cannot_bind() {
        let mut p = Project::default();
        let comp = Comp::new(400, 300, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut layers = vec![];
        let scales = [3.0, 50.0, 99.9, 100.0, 100.0001, 120.0, 400.0, 3000.0, 20000.0];
        for (i, scale) in scales.into_iter().enumerate() {
            let mut t = build::layer(&mut p, &comp, "Text", LayerSource::Text, (400, 300), None);
            let doc = TextDoc { text: "Wide text line".into(), size: 60.0, ..Default::default() };
            t.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
            let mut s = build::layer(&mut p, &comp, "Shape", LayerSource::Shape, (400, 300), None);
            let mut next = p.next_id;
            let mut ids = Ids(&mut next);
            let star = build::shape_star(&mut ids, true, 5.0, [0.0, 0.0], 150.0, 60.0);
            let fill = build::shape_fill(&mut ids, [0.9, 0.2, 0.1, 1.0]);
            let g = build::shape_group(&mut ids, "Star", vec![star, fill]);
            p.next_id = next;
            s.props.sub_mut("contents").unwrap().children.push(g.into());
            s.switches.collapse = true;
            for l in [&mut t, &mut s] {
                l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([scale, scale * 0.7, 100.0]);
                l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(13.0 * i as f64);
                l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([123.4, 56.7, 0.0]);
            }
            layers.extend([t, s]);
        }
        p.comp_mut(cid).unwrap().layers = layers.into();
        let c = p.comp(cid).unwrap();
        let (mut capped, mut small) = (0, 0);
        for out in [1.0, 0.5, 0.25, 2.0] {
            let r = Renderer::new(&p, &NoFootage, RenderOpts { scale: out, ..Default::default() });
            let ctx = EvalCtx::new(&p, cid, c, Tick::ZERO);
            for (i, l) in c.layers.iter().enumerate() {
                let fast = r.raster_scale(&ctx, l);
                ALWAYS_MEASURE.with(|m| m.set(true));
                let measured = r.raster_scale(&ctx, l);
                ALWAYS_MEASURE.with(|m| m.set(false));
                assert_eq!(fast.to_bits(), measured.to_bits(), "{} at output scale {out}", l.name);
                // Unclamped, 3000 % and more would step to the 64× limit: the cap held them down.
                capped += (scales[i / 2] >= 3000.0 && fast < out * 32.0) as usize;
                small += (fast <= out) as usize;
            }
        }
        assert!(capped >= 8 && small > 8, "capped {capped}, not enlarged {small}");
    }

    /// A text and a Continuously Rasterize shape layer at `scale` % (and rotated) in `comp`.
    fn enlarged(p: &mut Project, comp: &Comp, scale: f64, rot: f64) -> [Layer; 2] {
        let mut t = build::layer(p, comp, "Text", LayerSource::Text, (comp.width, comp.height), None);
        let doc = TextDoc { text: "Wide text line".into(), size: 60.0, ..Default::default() };
        t.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
        let mut s = build::layer(p, comp, "Shape", LayerSource::Shape, (comp.width, comp.height), None);
        let mut next = p.next_id;
        let mut ids = Ids(&mut next);
        let star = build::shape_star(&mut ids, true, 5.0, [0.0, 0.0], 150.0, 60.0);
        let fill = build::shape_fill(&mut ids, [0.9, 0.2, 0.1, 1.0]);
        let g = build::shape_group(&mut ids, "Star", vec![star, fill]);
        p.next_id = next;
        s.props.sub_mut("contents").unwrap().children.push(g.into());
        s.switches.collapse = true;
        for l in [&mut t, &mut s] {
            l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([scale, scale * 0.8, 100.0]);
            l.props.prop_mut("transform/rotation").unwrap().value = Value::Scalar(rot);
            l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([960.4, 540.7, 0.0]);
        }
        [t, s]
    }

    /// The scale a layer is seen at (output pixels per layer unit over the output scale) and
    /// its content's area in layer units.
    fn onscreen(ctx: &EvalCtx, l: &effectcraft_project::Layer, outer: Option<effectcraft_geom::Mat3>) -> (f64, f64) {
        let m = outer.map_or(ctx.layer_to_comp(l).0, |o| o * ctx.layer_to_comp(l).0);
        let a = l.transform().map(|tr| ctx.v3(l, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
        let (p0, px, py) = (m.apply(vec2(a[0], a[1])), m.apply(vec2(a[0] + 1.0, a[1])), m.apply(vec2(a[0], a[1] + 1.0)));
        let k = ((px.x - p0.x).hypot(px.y - p0.y)).max((py.x - p0.x).hypot(py.y - p0.y));
        let b = crate::content_bounds(ctx, l).unwrap();
        (k, (b[2] - b[0]) * (b[3] - b[1]))
    }

    /// The rasterisation cap grows with outputs past 16 Mpx: at every output size and layer scale
    /// the step equals the fixed 16 Mpx cap's bit for bit, except where that cap held the layer
    /// below the scale it is seen at, and there it is at least that scale (a sharper frame where
    /// main was softer than on-screen) within 1.25× the output's pixels, or as far as the
    /// rasterisers' 16384-pixel sides allow. Collapsed precomp layers measure the frame they are
    /// drawn into.
    #[test]
    fn raster_scale_cap_grows_with_the_output_only_where_it_softened() {
        let mut p = Project::default();
        let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut layers = vec![];
        for (i, scale) in [50.0, 100.0, 150.0, 400.0, 900.0, 1300.0, 2000.0, 3000.0, 6000.0, 20000.0].into_iter().enumerate() {
            layers.extend(enlarged(&mut p, &comp, scale, 7.0 * i as f64));
        }
        // The same text, 2000 %, inside a collapsed precomp of a smaller comp.
        let inner = Comp::new(400, 300, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let [nt, _] = enlarged(&mut p, &inner, 2000.0, 0.0);
        let mut inner = inner;
        inner.layers = vec![nt].into();
        let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
        let mut pre = build::layer(&mut p, &comp, "Inner", LayerSource::Comp { item: iid }, (400, 300), None);
        pre.switches.collapse = true;
        layers.push(pre);
        p.comp_mut(cid).unwrap().layers = layers.into();
        let c = p.comp(cid).unwrap();
        let ctx = EvalCtx::new(&p, cid, c, Tick::ZERO);
        let (mut sharper, mut same) = (0, 0);
        for out in [0.25, 0.5, 1.0, 2.0, 2.5, 4.0] {
            let r = Renderer::new(&p, &NoFootage, RenderOpts { scale: out, ..Default::default() });
            let mut check = |r: &Renderer, ctx: &EvalCtx, l: &effectcraft_project::Layer, what: String| {
                let new = r.raster_scale(ctx, l);
                FIXED_CAP.with(|m| m.set(true));
                let old = r.raster_scale(ctx, l);
                FIXED_CAP.with(|m| m.set(false));
                if new.to_bits() == old.to_bits() {
                    same += 1;
                    return;
                }
                let (k, area) = onscreen(ctx, l, r.outer);
                let cap = 1920.0 * 1080.0 * out * out * 1.25;
                assert!(cap > 16.0e6, "{what}: changed at a {out}× output");
                assert!(old < out * k && new > old, "{what}: {old} → {new} at on-screen {}", out * k);
                let side = crate::placement::drawn_buf_at(ctx, l, 1.0).map_or(0, |(w, h, ..)| w.max(h)) as f64 - 4.0;
                let most = (out * k).min((cap / area).sqrt()).min(16379.0 / side);
                assert!(new >= most * 0.99 * (1.0 - 1e-9), "{what}: {new} softer than {most} (on-screen {})", out * k);
                assert!(area * new * new <= cap * 1.0001, "{what}: {} px over the {cap} cap", area * new * new);
                // Within the rasterisers' sides (beyond them the content would be cut off).
                let (bw, bh, ..) = crate::placement::drawn_buf_at(ctx, l, new).unwrap();
                assert!(bw < 16384 && bh < 16384, "{what}: {bw}×{bh} buffer");
                sharper += 1;
            };
            for l in c.layers.iter() {
                match r.collapse_into(&ctx, l, iid, 1.0).filter(|_| l.switches.collapse && matches!(l.source, LayerSource::Comp { .. })) {
                    Some((sub, nctx)) => {
                        let nl = nctx.comp.layers.iter().next().unwrap();
                        check(&sub, &nctx, nl, format!("collapsed text at {out}"));
                    }
                    None => check(&r, &ctx, l, format!("{} {:?} at {out}", l.name, l.props.prop("transform/scale").map(|p| &p.value))),
                }
            }
        }
        assert!(sharper >= 6 && same > 60, "sharper {sharper}, same {same}");
    }

    /// Timing and memory: a 2000 % text layer and a large Continuously Rasterize shape layer on a
    /// 1920×1080 comp, rasterised at Full and at an 8K (4×) output.
    #[test]
    #[ignore]
    fn time_enlarged_text_and_shape() {
        let mut p = Project::default();
        let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let [t, _] = enlarged(&mut p, &comp, 2000.0, 0.0);
        let [_, s] = enlarged(&mut p, &comp, 1500.0, 0.0);
        p.comp_mut(cid).unwrap().layers = vec![t, s].into();
        let c = p.comp(cid).unwrap();
        let ctx = EvalCtx::new(&p, cid, c, Tick::ZERO);
        for out in [1.0, 4.0] {
            for fixed in [true, false] {
                FIXED_CAP.with(|m| m.set(fixed));
                let r = Renderer::new(&p, &NoFootage, RenderOpts { scale: out, ..Default::default() });
                for l in c.layers.iter() {
                    let t0 = std::time::Instant::now();
                    let b = r.source(&ctx, l).unwrap();
                    let (k, _) = onscreen(&ctx, l, None);
                    eprintln!(
                        "{} at {out}× (fixed cap {fixed}): scale {:.2} (on-screen {:.2}), buffer {}x{} ({:.0} MB), {:.0} ms",
                        l.name,
                        b.scale,
                        out * k,
                        b.img.width,
                        b.img.height,
                        (b.img.width as f64 * b.img.height as f64 * 16.0) / 1e6,
                        t0.elapsed().as_secs_f64() * 1e3
                    );
                }
            }
        }
        FIXED_CAP.with(|m| m.set(false));
    }

    /// Timing: `raster_scale` of an unscaled paragraph text layer, measured or not.
    #[test]
    #[ignore]
    fn time_raster_scale() {
        let mut p = Project::default();
        let comp = Comp::new(1920, 1080, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut t = build::layer(&mut p, &comp, "Text", LayerSource::Text, (1920, 1080), None);
        let doc = TextDoc { text: "The quick brown fox jumps over the lazy dog. ".repeat(6), size: 40.0, ..Default::default() };
        t.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(doc));
        p.comp_mut(cid).unwrap().layers = vec![t].into();
        let c = p.comp(cid).unwrap();
        let r = Renderer::new(&p, &NoFootage, RenderOpts::default());
        let ctx = EvalCtx::new(&p, cid, c, Tick::ZERO);
        let l = c.layers.iter().next().unwrap();
        for always in [true, false, true, false] {
            ALWAYS_MEASURE.with(|m| m.set(always));
            let t0 = std::time::Instant::now();
            for _ in 0..100 {
                std::hint::black_box(r.raster_scale(&ctx, l));
            }
            eprintln!("measure {always}: {:.1} us per call", t0.elapsed().as_secs_f64() * 1e4);
        }
        ALWAYS_MEASURE.with(|m| m.set(false));
    }
}
