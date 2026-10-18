//! Processed layer buffers: source → masks → effects → layer styles, served from and
//! stored in the layer cache.

use crate::placement::Window;
use crate::{EvalCtx, LayerTiming, Renderer, cache, clip, color, masks, source_size, styles};
use effectcraft_color::BlendMode;
use effectcraft_effects::Buf;
use effectcraft_keyframe::Value;
use effectcraft_project::{Comp, Layer, LayerSource, Node, PropGroup};
use std::sync::Arc;

/// A layer's content key, its precomp bookkeeping id and its shape clip window (see
/// `Renderer::content_cache_key_noted`).
type ContentKey = (Option<u64>, Option<u64>, Option<Window>);

impl<'a> Renderer<'a> {
    /// Source → masks, clamped/quantised to the project depth. `clip`: only that window of a
    /// shape layer's buffer (see [`Self::shape_clip`]).
    fn masked_source(&self, ctx: &EvalCtx, layer: &Layer, clip: Option<Window>) -> Option<Buf> {
        let _profile = crate::profile::span("layer_raster");
        let mut buf = match (self.content_times(ctx, layer), clip, &layer.source) {
            (Some(times), ..) => self.blurred_source(ctx, layer, &times)?,
            (None, Some(w), LayerSource::Shape) => {
                let contents = layer.props.sub("contents")?;
                let k = self.raster_scale(ctx, layer);
                self.authored(crate::shapes::render_window(ctx, layer, contents, k, Some([w.x, w.y, w.w, w.h])))
            }
            (None, ..) => self.source(ctx, layer)?,
        };
        // A solid is filled with a quantised colour: only masks can take it off the grid.
        let solid = matches!(layer.source, LayerSource::Solid { .. });
        if masks::apply(ctx, layer, &mut buf, self.mask_blur(ctx, layer)) || !solid {
            self.pipe.quantize(&mut buf.img);
        }
        Some(buf)
    }

    /// Fully processed layer buffer (source → masks → effects → layer styles, flattened).
    /// Served from the layer cache when the layer's content key matches.
    pub fn layer_buf(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Arc<Buf>> {
        self.layer_buf_keyed(ctx, layer).map(|(b, _)| b)
    }

    /// The pixels a layer lends as a track matte: its content (source, masks, effects) before
    /// Layer Styles and their blending options. After Effects mattes through a shape whose Fill
    /// Opacity is 0 as through the shape (SceneYard "Apple Maps Route").
    pub fn matte_buf(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Arc<Buf>> {
        self.styled(ctx, layer, None).map(|st| st.content)
    }

    /// [`Self::layer_buf`] in the blending space (linear with Blend Colors Using 1.0 Gamma).
    pub fn blend_layer_buf(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Arc<Buf>> {
        let (b, k) = self.layer_buf_keyed(ctx, layer)?;
        Some(self.to_blend(b, k))
    }

    /// A buffer converted to the blending space (cached under a key derived from `key`).
    pub(crate) fn to_blend(&self, b: Arc<Buf>, key: Option<u64>) -> Arc<Buf> {
        let Some(c) = self.pipe.to_blend() else { return b };
        let key = key.map(|k| cache::derive(k, 0x1ea7));
        if let (Some(cache), Some(k)) = (self.cache, key)
            && let Some(x) = cache.get(k)
        {
            return x;
        }
        let mut nb = (*b).clone();
        color::convert(&mut nb.img, &c);
        let nb = Arc::new(nb);
        if let (Some(cache), Some(k)) = (self.cache_to_keep(), key) {
            cache.insert(k, nb.clone());
        }
        nb
    }

    fn layer_buf_keyed(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(Arc<Buf>, Option<u64>)> {
        let st = self.styled(ctx, layer, None)?;
        if st.passes.is_empty() {
            return Some((st.body, st.key));
        }
        let key = st.key.map(|k| cache::derive(k, 0xf1a7));
        if let (Some(c), Some(k)) = (self.cache, key)
            && let Some(b) = c.get(k)
        {
            return Some((b, key));
        }
        let flat = Arc::new(styles::Styled { passes: st.passes.iter().map(|(b, m)| ((**b).clone(), *m)).collect(), body: (*st.body).clone() }.flatten());
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            c.insert(k, flat.clone());
        }
        Some((flat, key))
    }

    /// A layer's source pixels at the context time, before masks, effects and the transform
    /// (what the motion tracker analyses, like After Effects' Layer panel). `None` for layers
    /// without pixels.
    pub fn layer_source(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Buf> {
        self.source(ctx, layer)
    }

    /// Layer pixels after source → masks → effects (no layer styles).
    pub fn content_buf(&self, ctx: &EvalCtx, layer: &Layer) -> Option<Arc<Buf>> {
        self.content_buf_timed(ctx, layer, None, None).map(|(b, _)| b)
    }

    /// The layer cache key of a layer's content (`None` without a cache or for uncached layers).
    fn content_cache_key(&self, ctx: &EvalCtx, layer: &Layer) -> Option<u64> {
        self.content_cache_key_noted(ctx, layer).0
    }

    /// Processed native footage uses the same dependency key as cached CPU content.
    /// Exact Processed key used by ordinary CPU content consumers.
    pub fn resident_content_key(&self, ctx: &EvalCtx, layer: &Layer) -> Option<u64> {
        self.content_cache_key(ctx, layer)
    }
    /// Time-varying source pixels are transient; transform-only animation still
    /// reuses layer-space pixels. Uses the CPU cache's dependency classification.
    pub fn resident_content_animated(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        cache::content_animated(ctx.comp, layer)
    }

    /// A precomp's Processed key (by the nested comp's values, so an animated precomp holding
    /// its values keeps its key) and the existing key-attempt bookkeeping id. Accelerators must
    /// still apply `precomp_source` eligibility before using this key, and admit animated
    /// content ([`Self::resident_precomp_animated`]) only once its key repeats.
    pub fn resident_precomp_key(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(u64, u64)> {
        if !matches!(layer.source, LayerSource::Comp { .. }) || self.freeze.is_some() {
            return None;
        }
        let (key, noted, _) = self.content_cache_key_noted(ctx, layer);
        Some((key?, noted?))
    }

    /// Whether a precomp's pixels may change from frame to frame (its key then changes with
    /// its values): retained only when the same key comes back.
    pub fn resident_precomp_animated(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        self.precomp_varies(ctx, layer)
    }

    /// Report a static precomp lookup through the same miss backoff used by CPU content.
    pub fn resident_precomp_note(&self, id: u64, hit: bool) {
        if let Some(cache) = self.cache {
            cache.precomp_note(id, hit);
        }
    }

    /// [`Self::resident_content_key`] and the window of the layer's buffer that key stands for
    /// (for [`Self::resident_source`]): decided together, once.
    pub fn resident_content(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(u64, ContentClip)> {
        let (key, _, clip) = self.content_cache_key_noted(ctx, layer);
        Some((key?, ContentClip(clip)))
    }

    /// Source and masks rebased into effect space, exactly as the CPU content producer, in the
    /// window `clip` that came with the layer's key ([`Self::resident_content`]).
    pub fn resident_source(&self, ctx: &EvalCtx, layer: &Layer, clip: ContentClip) -> Option<Buf> {
        let mut buf = self.masked_source(ctx, layer, clip.0)?;
        buf.rebase(crate::fx::fx_origin(ctx, layer, false));
        Some(buf)
    }

    /// Bare footage also gets its source-time key, without enabling unsafe effect caching.
    pub fn resident_footage_key(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(u64, cache::ResidentToken)> {
        let c = self.cache?;
        let token = c.resident_token()?;
        let key = self.content_cache_key(ctx, layer).or_else(|| {
            if layer.switches.effects && layer.effects().is_some_and(|f| !f.children.is_empty()) {
                return None;
            }
            let LayerSource::Footage { item } = layer.source else { return None };
            let key = cache::input_key_src(
                ctx,
                layer,
                self.raster_scale(ctx, layer),
                self.opts.draft,
                self.mb_on(ctx, layer),
                0,
                Some(self.proxy_for(item).is_some() as u64),
            );
            self.content_key(ctx, layer, key)
        })?;
        Some((key, token))
    }

    /// [`Self::content_cache_key`], and for a precomp layer that was keyed, its id for
    /// [`cache::LayerCache::precomp_note`] (an animated precomp is keyed only now and then, see
    /// [`cache::LayerCache::precomp_try`]).
    ///
    /// A shape layer drawn clipped (see [`Self::shape_clip`]) has the window in its key, and
    /// the window is returned too.
    fn content_cache_key_noted(&self, ctx: &EvalCtx, layer: &Layer) -> ContentKey {
        let Some(c) = self.cache else { return (None, None, self.shape_clip(ctx, layer, None)) };
        let (src, noted) = match layer.source {
            LayerSource::Comp { .. } => {
                let id = cache::derive(cache::derive(ctx.comp_id.0, layer.id.0), self.depth as u64);
                if !c.precomp_try(id) {
                    return (None, None, None);
                }
                (self.precomp_key(ctx, layer), Some(id))
            }
            // Which proxy the footage shows.
            LayerSource::Footage { item } => (Some(self.proxy_for(item).is_some() as u64), None),
            _ => (None, None),
        };
        let key = cache::layer_key_src(ctx, layer, self.raster_scale(ctx, layer), self.opts.draft, self.mb_on(ctx, layer), src);
        let key = self.content_key(ctx, layer, key);
        let clip = self.shape_clip(ctx, layer, key);
        let key = match clip {
            Some(w) => key.map(|k| clip::clipped_key(k, w)),
            None => key,
        };
        (key, noted, clip)
    }

    /// `keyed`: [`Self::content_cache_key_noted`] when the caller has it already (a key is
    /// computed once per layer and frame: keying a precomp counts towards its miss streak).
    fn content_buf_timed(
        &self,
        ctx: &EvalCtx,
        layer: &Layer,
        mut timing: Option<&mut LayerTiming>,
        keyed: Option<ContentKey>,
    ) -> Option<(Arc<Buf>, Option<u64>)> {
        let (key, noted, clip) = keyed.unwrap_or_else(|| self.content_cache_key_noted(ctx, layer));
        // An animated precomp's buffer isn't kept for other uses in its frame (see
        // [`cache::LayerCache::insert_once`]): no use waiting for another thread's render of it.
        let once = key.is_some() && noted.is_some() && self.precomp_varies(ctx, layer);
        let mut _flight = None;
        if let Some(c) = self.cache {
            let hit = match key {
                Some(k) if once => c.get(k),
                Some(k) => c.get(k).or_else(|| match c.claim(k) {
                    cache::Claim::Ready(b) => Some(b),
                    cache::Claim::Owner(g) => {
                        _flight = Some(g);
                        None
                    }
                    cache::Claim::Render => None,
                }),
                None => None,
            };
            if let Some(id) = noted {
                c.precomp_note(id, hit.is_some());
            }
            if let Some(b) = hit {
                if let Some(t) = timing.as_deref_mut() {
                    t.cached = true;
                }
                return Some((b, key));
            }
        }
        let t0 = web_time::Instant::now();
        let buf = self.masked_source(ctx, layer, clip)?;
        let t1 = web_time::Instant::now();
        let (buf, kept) = self.apply_effects_kept(ctx, layer, buf, false, usize::MAX, timing.map(|t| &mut t.effects));
        if t0.elapsed() >= std::time::Duration::from_millis(400) {
            log::debug!(
                target: "effectcraft_stall",
                "layer {:?} at {:.3}: source {:.0} ms, effects {:.0} ms (key {}, {})",
                layer.name,
                ctx.time.seconds(),
                (t1 - t0).as_secs_f64() * 1000.0,
                t1.elapsed().as_secs_f64() * 1000.0,
                key.is_some(),
                if _flight.is_some() { "owner" } else { "unclaimed" }
            );
        }
        let buf = Arc::new(buf);
        // The accelerator composites this buffer from the effects' result on the device (no
        // second upload), unless it blends a converted copy (linear blending).
        if let Some(k) = kept
            && self.pipe.to_blend().is_none()
        {
            k.adopt(&buf);
        }
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            if once {
                c.insert_once(k, buf.clone());
            } else {
                c.insert(k, buf.clone());
            }
        }
        // (`_flight`, dropped here, wakes the threads waiting for this buffer.)
        Some((buf, key))
    }

    /// A layer's input at the context time: source → masks → its first `effects` effects
    /// (what Time effects read at neighbouring times), in effect space (see [`effect_bounds`]):
    /// what effect `effects` sees. Served from / stored in the layer cache under its own key,
    /// so scrubbing reuses frames rendered for earlier output frames.
    pub fn layer_input(&self, ctx: &EvalCtx, layer: &Layer, effects: usize) -> Option<Arc<Buf>> {
        let src = match layer.source {
            LayerSource::Comp { .. } if self.cache.is_some() => self.precomp_key(ctx, layer),
            LayerSource::Footage { item } => Some(self.proxy_for(item).is_some() as u64),
            _ => None,
        };
        let key =
            self.cache.and_then(|_| cache::input_key_src(ctx, layer, self.raster_scale(ctx, layer), self.opts.draft, self.mb_on(ctx, layer), effects, src));
        let key = self.content_key(ctx, layer, key);
        if let (Some(c), Some(k)) = (self.cache, key)
            && let Some(b) = c.get(k)
        {
            return Some(b);
        }
        let mut buf = self.masked_source(ctx, layer, None)?;
        let origin = ctx.effect_bounds(layer).1;
        buf.rebase(origin);
        if effects > 0 {
            buf = self.effect_space_stack(ctx, layer, buf, false, effects, None, origin);
        }
        let buf = Arc::new(buf);
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            c.insert(k, buf.clone());
        }
        Some(buf)
    }

    /// A styled layer for an accelerator that renders its styles itself: the content cropped
    /// and padded for them, and the styles ([`crate::styles::gpu_prepare`]). `None` leaves the
    /// layer to [`Self::styled`]: styles the accelerator can't render, a static layer (its
    /// styles are cached and uploaded once), or a frame seen before. The first visit of a frame
    /// leaves a mark in the layer cache; the next one styles it on the CPU and caches the
    /// result, so scrubbing back over it costs no styling at all.
    pub fn gpu_styles(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(Arc<Buf>, crate::styles::GpuStyles)> {
        if self.layer_is_static(ctx, layer) {
            return None;
        }
        let (content, ckey) = self.content_buf_timed(ctx, layer, None, None)?;
        if let (Some(c), Some(k)) = (self.cache, ckey.map(|k| cache::styles_key(ctx, layer, k))) {
            let mark = cache::derive(k, 0x6e57);
            if c.peek(k).is_some() || c.peek(mark).is_some() {
                return None;
            }
            if let Some(keep) = self.cache_to_keep() {
                keep.insert(mark, Arc::new(Buf { img: effectcraft_raster::Image::new(1, 1), offset: [0.0; 2], scale: 1.0 }));
            }
        }
        let size = source_size(self.project, layer);
        let layer_size = if size.0 == 0 { [ctx.comp.width as f64, ctx.comp.height as f64] } else { [size.0 as f64, size.1 as f64] };
        let (buf, plan) = crate::styles::gpu_prepare(ctx, layer, &content, layer_size)?;
        Some((Arc::new(buf), plan))
    }

    /// The layer with its styles: exterior passes and body (cached per pass).
    pub(crate) fn styled(&self, ctx: &EvalCtx, layer: &Layer, timing: Option<&mut LayerTiming>) -> Option<StyledLayer> {
        self.styled_keyed(ctx, layer, timing, None)
    }

    /// [`Self::styled`] with the content key already computed (see [`Self::content_buf_timed`]).
    fn styled_keyed(&self, ctx: &EvalCtx, layer: &Layer, mut timing: Option<&mut LayerTiming>, keyed: Option<ContentKey>) -> Option<StyledLayer> {
        let (content, ckey) = self.content_buf_timed(ctx, layer, timing.as_deref_mut(), keyed)?;
        if !styles::active(ctx, layer) {
            return Some(StyledLayer { passes: vec![], body: content.clone(), content, key: ckey, ckey, plain: true });
        }
        let modes = styles::pass_modes(ctx, layer);
        let key = ckey.map(|k| cache::styles_key(ctx, layer, k));
        if let (Some(c), Some(k)) = (self.cache, key)
            && let Some(body) = c.get(k)
        {
            let passes: Option<Vec<_>> = modes.iter().enumerate().map(|(i, m)| c.get(cache::derive(k, i as u64 + 1)).map(|b| (b, *m))).collect();
            if let Some(passes) = passes {
                return Some(StyledLayer { passes, body, content, key, ckey, plain: false });
            }
        }
        let t0 = web_time::Instant::now();
        let size = source_size(self.project, layer);
        let layer_size = if size.0 == 0 { [ctx.comp.width as f64, ctx.comp.height as f64] } else { [size.0 as f64, size.1 as f64] };
        let st = styles::render(ctx, layer, &content, layer_size);
        if let Some(t) = timing {
            t.cached = false;
            t.effects.push(("layerStyles".into(), t0.elapsed().as_secs_f64() * 1e3));
        }
        let body = Arc::new(st.body);
        let passes: Vec<_> = st.passes.into_iter().map(|(b, m)| (Arc::new(b), m)).collect();
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            c.insert(k, body.clone());
            for (i, (b, _)) in passes.iter().enumerate() {
                c.insert(cache::derive(k, i as u64 + 1), b.clone());
            }
        }
        Some(StyledLayer { passes, body, content, key, ckey, plain: false })
    }

    /// [`Self::styled`] in the blending space ([`Self::styled_to_blend`]). With linear blending,
    /// the blending-space copies are looked up first: on a hit the working-space buffers they
    /// were converted from aren't touched, so they go cold in the layer cache (packed or evicted
    /// under pressure) instead of being kept hot next to their converted copies.
    pub(crate) fn styled_blend(&self, ctx: &EvalCtx, layer: &Layer, mut timing: Option<&mut LayerTiming>) -> Option<StyledLayer> {
        // The content key, computed once for the lookups below (only linear blending has them).
        let keyed = (self.cache.is_some() && self.pipe.to_blend().is_some()).then(|| self.content_cache_key_noted(ctx, layer));
        if let Some(st) = keyed.and_then(|(ckey, _, _)| self.styled_blend_cached(ctx, layer, ckey?)) {
            if let Some(t) = timing.as_deref_mut() {
                t.cached = true;
            }
            return Some(st);
        }
        if self.blend_only(ctx, layer) {
            return self.plain_blend(ctx, layer, timing, keyed);
        }
        self.styled_keyed(ctx, layer, timing, keyed).map(|st| self.styled_to_blend(st))
    }

    /// With linear blending, is `layer` a plain layer (no Layer Styles) whose working-space
    /// pixels nothing else reads? Then only its blending-space copy is worth caching.
    fn blend_only(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        self.cache.is_some() && self.pipe.to_blend().is_some() && !styles::active(ctx, layer) && !working_read(ctx.comp, layer)
    }

    /// [`Self::styled_blend`] of a [`Self::blend_only`] layer: its content is converted in place
    /// and cached in the blending space alone (no working-space copy cloned and kept next to
    /// it). The same pixels as [`Self::content_buf_timed`] then [`Self::to_blend`].
    fn plain_blend(&self, ctx: &EvalCtx, layer: &Layer, mut timing: Option<&mut LayerTiming>, keyed: Option<ContentKey>) -> Option<StyledLayer> {
        let (ckey, noted, clip) = keyed.unwrap_or_else(|| self.content_cache_key_noted(ctx, layer));
        let key = ckey.map(|k| cache::derive(k, 0x1ea7));
        // A resident Processed result is an actual CPU demand here. Reuse it in
        // working space, then convert forward; a Blend texture is never inverted.
        if let (Some(c), Some(ck), Some(bk)) = (self.cache, ckey, key) {
            let token = c.resident_token();
            let intent = c.disk_intent(bk);
            let cpu = c.peek(ck);
            if cpu.is_some() || c.gpu_owner().and_then(|owner| owner.resident(ck, cache::GpuStage::Processed)).is_some() {
                if let Some(body) = c.get(bk) {
                    if let Some(t) = timing.as_deref_mut() {
                        t.cached = true;
                    }
                    return Some(StyledLayer { passes: vec![], content: body.clone(), body, key: ckey, ckey, plain: true });
                }
                if let Some(content) = cpu.or_else(|| c.get(ck)) {
                    let mut converted = (*content).clone();
                    if let Some(conversion) = self.pipe.to_blend() {
                        color::convert(&mut converted.img, &conversion);
                    }
                    let body = Arc::new(converted);
                    if let Some(token) = token {
                        c.insert_captured(bk, &body, &token, intent);
                    }
                    if let Some(t) = timing.as_deref_mut() {
                        t.cached = true;
                    }
                    return Some(StyledLayer { passes: vec![], content: body.clone(), body, key: ckey, ckey, plain: true });
                }
            }
        }
        let mut _flight = None;
        if let Some(c) = self.cache {
            let hit = match key {
                Some(k) => c.get(k).or_else(|| match c.claim(k) {
                    cache::Claim::Ready(b) => Some(b),
                    cache::Claim::Owner(g) => {
                        _flight = Some(g);
                        None
                    }
                    cache::Claim::Render => None,
                }),
                None => None,
            };
            if let Some(id) = noted {
                c.precomp_note(id, hit.is_some());
            }
            if let Some(b) = hit {
                if let Some(t) = timing.as_deref_mut() {
                    t.cached = true;
                }
                return Some(StyledLayer { passes: vec![], content: b.clone(), body: b, key: ckey, ckey, plain: true });
            }
        }
        let buf = self.masked_source(ctx, layer, clip)?;
        let mut buf = self.apply_effects_timed(ctx, layer, buf, false, usize::MAX, timing.map(|t| &mut t.effects));
        if let Some(c) = self.pipe.to_blend() {
            color::convert(&mut buf.img, &c);
        }
        let buf = Arc::new(buf);
        if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
            c.insert(k, buf.clone());
        }
        // (`_flight`, dropped here, wakes the threads waiting for this buffer.)
        Some(StyledLayer { passes: vec![], content: buf.clone(), body: buf, key: ckey, ckey, plain: true })
    }

    fn styled_blend_cached(&self, ctx: &EvalCtx, layer: &Layer, ckey: u64) -> Option<StyledLayer> {
        let c = self.cache?;
        self.pipe.to_blend()?;
        let blend = |k: u64| c.peek(cache::derive(k, 0x1ea7));
        if !styles::active(ctx, layer) {
            let body = blend(ckey)?;
            return Some(StyledLayer { passes: vec![], content: body.clone(), body, key: Some(ckey), ckey: Some(ckey), plain: true });
        }
        let key = cache::styles_key(ctx, layer, ckey);
        let body = blend(key)?;
        let content = blend(ckey)?;
        let passes = styles::pass_modes(ctx, layer)
            .into_iter()
            .enumerate()
            .map(|(i, m)| blend(cache::derive(key, i as u64 + 1)).map(|b| (b, m)))
            .collect::<Option<Vec<_>>>()?;
        Some(StyledLayer { passes, body, content, key: Some(key), ckey: Some(ckey), plain: false })
    }

    /// Would [`Self::styled_blend`] find all of `layer`'s buffers in the layer cache? Counts no
    /// use: the pre-pass leaves such layers to the walk, which then hits once per layer.
    pub(crate) fn styled_blend_held(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        let Some(c) = self.cache else { return false };
        let Some(ckey) = self.content_cache_key(ctx, layer) else { return false };
        let linear = self.pipe.to_blend().is_some();
        let held = |k: u64| c.holds(if linear { cache::derive(k, 0x1ea7) } else { k });
        if !styles::active(ctx, layer) {
            return held(ckey);
        }
        let key = cache::styles_key(ctx, layer, ckey);
        held(key) && held(ckey) && (0..styles::pass_modes(ctx, layer).len()).all(|i| held(cache::derive(key, i as u64 + 1)))
    }

    /// The styled layer's buffers in the blending space.
    pub(crate) fn styled_to_blend(&self, st: StyledLayer) -> StyledLayer {
        if self.pipe.to_blend().is_none() {
            return st;
        }
        let body = self.to_blend(st.body, st.key);
        let content = if st.plain { body.clone() } else { self.to_blend(st.content, st.ckey) };
        let passes = st.passes.into_iter().enumerate().map(|(i, (b, m))| (self.to_blend(b, st.key.map(|k| cache::derive(k, i as u64 + 1))), m)).collect();
        StyledLayer { passes, body, content, ..st }
    }
}

/// Could anything in `layer`'s comp read its working-space pixels (its content buffer) other
/// than compositing it: a track matte naming it, an effect's layer parameter (Displacement Map,
/// Set Matte…) or an expression's `sampleImage`? Errs towards yes: a wrong yes only keeps a
/// copy in the layer cache, a wrong no only re-renders the layer for that reader.
pub(crate) fn working_read(comp: &Comp, layer: &Layer) -> bool {
    comp.layers.iter().any(|l| {
        l.track_matte.as_ref().is_some_and(|m| m.layer == layer.id)
            || l.effects().is_some_and(names_a_layer)
            || (cache::layer_info(comp, l).expressions && samples_pixels(&l.props))
    })
}

/// Does a property of `g` take a layer (whatever its value)?
fn names_a_layer(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => matches!(p.value, Value::Layer(_)),
        Node::Group(sub) => names_a_layer(sub),
    })
}

/// Does an expression in `g` read rendered pixels (`sampleImage`)?
fn samples_pixels(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => p.expr.as_ref().is_some_and(|e| e.text.contains("sampleImage")),
        Node::Group(sub) => samples_pixels(sub),
    })
}

/// The window of a layer's buffer its content key stands for (see [`Renderer::resident_content`]):
/// the whole buffer, or the part of a shape layer's that can show (see `clip.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContentClip(Option<Window>);

/// A layer ready to composite (see [`Renderer::styled_layer`]).
pub struct StyledLayer {
    /// Exterior style passes (Drop Shadow, Outer Glow) with their blend modes.
    pub passes: Vec<(Arc<Buf>, BlendMode)>,
    pub body: Arc<Buf>,
    /// Pre-style pixels (knockout shape).
    pub content: Arc<Buf>,
    /// Styled cache key (the content key when plain).
    key: Option<u64>,
    /// Content cache key.
    ckey: Option<u64>,
    /// No layer styles: composite `body` the usual way.
    pub plain: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayerCache, NoFootage, RenderOpts};
    use effectcraft_color::Label;
    use effectcraft_project::build::{self, Ids};
    use effectcraft_project::{Expression, ItemId, ItemKind, MatteKind, Project, Property, Solid, TrackMatte};
    use effectcraft_time::{FrameRate, Tick};

    fn solid(p: &mut Project, c: &Comp, color: [f32; 3], w: u32) -> Layer {
        let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color, width: w, height: 20, pixel_aspect: 1.0 }));
        build::layer(p, c, "S", LayerSource::Solid { item: sid }, (w, 20), None)
    }

    fn blur(p: &mut Project, l: &mut Layer, amount: f64) {
        let Some(spec) = effectcraft_effects::find("ec.blur.gaussian") else { panic!("gaussian") };
        let mut next = p.next_id;
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [40.0, 20.0]);
        p.next_id = next;
        let Some(b) = g.prop_mut("blurriness") else { panic!("blurriness") };
        b.value = Value::Scalar(amount);
        let Some(fx) = l.props.sub_mut("effects") else { panic!("effects") };
        fx.children.push(g.into());
    }

    /// A linear-blending comp: a blurred solid screened over a solid, the top one optionally
    /// the bottom one's track matte.
    fn scene(matte: bool) -> (Project, ItemId) {
        let mut p = Project::default();
        p.settings.blend_linear = true;
        let comp = Comp::new(40, 20, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let bottom = solid(&mut p, &comp, [0.1, 0.5, 0.9], 40);
        let mut top = solid(&mut p, &comp, [0.8, 0.2, 0.1], 24);
        blur(&mut p, &mut top, 3.0);
        top.blend_mode = BlendMode::Screen;
        let mut bottom = bottom;
        if matte {
            bottom.track_matte = Some(TrackMatte { layer: top.id, kind: MatteKind::Alpha });
        }
        let Some(c) = p.comp_mut(cid) else { panic!("comp") };
        c.layers.insert(0, bottom);
        c.layers.insert(0, top);
        (p, cid)
    }

    /// With linear blending, layers nothing else reads are cached in the blending space alone
    /// (one entry per layer, not two) and the frame is bit-identical to an uncached render,
    /// which converts a working-space copy.
    #[test]
    fn linear_blend_caches_unread_layers_once() {
        let (p, cid) = scene(false);
        let cache = LayerCache::default();
        let frame = || {
            let mut r = Renderer::new(&p, &NoFootage, RenderOpts::default());
            r.cache = Some(&cache);
            r.comp_frame(cid, Tick::ZERO)
        };
        let a = frame();
        assert_eq!(cache.stats().entries, 2, "one blending-space copy per layer");
        let hits = cache.stats().hits;
        let b = frame();
        assert_eq!(cache.stats().hits, hits + 2, "one hit per layer");
        assert_eq!(cache.stats().entries, 2);
        let uncached = Renderer::new(&p, &NoFootage, RenderOpts::default()).comp_frame(cid, Tick::ZERO);
        let bits = |i: &effectcraft_raster::Image| i.data.iter().flat_map(|px| px.map(f32::to_bits)).collect::<Vec<_>>();
        assert_eq!(bits(&a), bits(&uncached));
        assert_eq!(bits(&b), bits(&uncached));
    }

    /// A layer another layer reads as its track matte keeps its working-space copy (the matte
    /// reads it); pixels are unchanged either way.
    #[test]
    fn linear_blend_keeps_working_copies_that_are_read() {
        let (p, cid) = scene(true);
        let cache = LayerCache::default();
        let mut r = Renderer::new(&p, &NoFootage, RenderOpts::default());
        r.cache = Some(&cache);
        let a = r.comp_frame(cid, Tick::ZERO);
        // The matte (top) layer: working and blending copies; the matted layer: blending only.
        assert_eq!(cache.stats().entries, 3);
        let uncached = Renderer::new(&p, &NoFootage, RenderOpts::default()).comp_frame(cid, Tick::ZERO);
        assert_eq!(a.data, uncached.data);
    }

    /// Layer parameters and `sampleImage` expressions count as readers.
    #[test]
    fn working_readers() {
        let (mut p, cid) = scene(false);
        let read = |p: &Project| {
            let Some(c) = p.comp(cid) else { panic!("comp") };
            c.layers.iter().map(|l| working_read(c, l)).collect::<Vec<_>>()
        };
        assert_eq!(read(&p), [false, false]);
        let Some(c) = p.comp_mut(cid) else { panic!("comp") };
        let Some(pos) = c.layers.get_mut(1).and_then(|l| l.props.prop_mut("transform/position")) else { panic!("position") };
        pos.expr = Some(Expression { text: "thisComp.layer(1).sampleImage([10, 10])".into(), enabled: true });
        assert_eq!(read(&p), [true, true]);
        let mut fx = PropGroup::new(1, "fx", "Fx");
        assert!(!names_a_layer(&fx));
        let mut sub = PropGroup::new(2, "sub", "Sub");
        sub.children.push(Node::Prop(Property::new(3, "layer", "Layer", Value::Layer(None))));
        fx.children.push(Node::Group(sub));
        assert!(names_a_layer(&fx));
    }
}
