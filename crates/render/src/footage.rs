//! Footage: the decoded-frame source, proxies, Interpret Footage (fields, alpha, linear
//! light) and frame blending.

use crate::{EvalCtx, Image, Renderer, color};
use effectcraft_effects::Buf;
use effectcraft_project::{Footage, FootageKind, FrameBlend, ItemId, ItemKind, Layer};
use effectcraft_raster::Px;
use effectcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A decoded footage frame as its source keeps it, turned into compositor pixels a band of rows
/// at a time: float frames are copied, compact ones (a decoder's 8-bit Y'CbCr samples) are
/// converted from their samples straight into the caller's buffer.
pub trait FramePixels: Send + Sync {
    /// Width and height in pixels.
    fn size(&self) -> (u32, u32);
    /// Rows `y0..` (as many whole rows as `out` holds): exactly the pixels
    /// [`FootageSource::frame`] returns for the frame.
    fn rows(&self, y0: usize, out: &mut [Px]);
    /// Native planar video samples, when their interpretation is supported.
    /// The source keeps ownership; CPU consumers continue using `rows`.
    fn yuv(&self) -> Option<crate::yuv::View<'_>> {
        None
    }
    /// The pixels themselves when they are an [`Image`] already.
    fn image(&self) -> Option<&Image> {
        None
    }
}

impl FramePixels for Image {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn rows(&self, y0: usize, out: &mut [Px]) {
        let start = y0.saturating_mul(self.width as usize);
        if let Some(src) = self.data.get(start..start.saturating_add(out.len()).min(self.data.len())) {
            out.get_mut(..src.len()).unwrap_or_default().copy_from_slice(src);
        }
    }
    fn image(&self) -> Option<&Image> {
        Some(self)
    }
}

/// A footage frame shared with its source's cache.
pub type SharedFrame = Arc<dyn FramePixels>;

/// The size a footage frame `frame_width` pixels wide is resampled to for a layer of nominal
/// size `w`×`h` drawn at render scale `s`: `None` at (nearly) full resolution, or when the frame
/// is smaller than the layer (an upscaled proxy keeps its pixels).
pub fn preview_size(frame_width: u32, w: u32, h: u32, s: f64) -> Option<(u32, u32)> {
    let k = if frame_width > 0 { w.max(1) as f64 / frame_width as f64 } else { 1.0 };
    (s < 0.75 && k <= 1.0 + 1e-9).then(|| (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1)))
}

/// A footage frame for a layer: `preview` when its source has already resampled it to
/// [`preview_size`] for the layer's nominal size at this render's scale.
#[derive(Clone)]
pub struct FootageFrame {
    pub px: SharedFrame,
    pub preview: bool,
}

/// A footage frame as [`Renderer::footage_buf`] takes it.
pub(crate) trait AsFootageFrame {
    fn pixels(&self) -> &dyn FramePixels;
    fn is_preview(&self) -> bool {
        false
    }
}

impl AsFootageFrame for Arc<Image> {
    fn pixels(&self) -> &dyn FramePixels {
        &**self
    }
}

impl AsFootageFrame for FootageFrame {
    fn pixels(&self) -> &dyn FramePixels {
        &*self.px
    }
    fn is_preview(&self) -> bool {
        self.preview
    }
}

/// Rows converted per parallel task.
const ROWS: usize = 8;

/// `px` as an [`Image`], converted with `conv` (as [`color::convert`] would after it) in the same
/// pass: one write of the frame instead of a copy and a second sweep.
fn expand(px: &dyn FramePixels, conv: Option<&effectcraft_color::Conversion>) -> Image {
    let (w, h) = px.size();
    let mut img = Image::new(w, h);
    if w == 0 || h == 0 {
        return img;
    }
    img.data.par_chunks_mut(w as usize * ROWS).enumerate().for_each(|(i, band)| {
        px.rows(i * ROWS, band);
        if let Some(c) = conv {
            for p in band.iter_mut() {
                convert_px(p, c);
            }
        }
    });
    img
}

/// [`color::convert`] of one premultiplied pixel.
#[inline(always)]
fn convert_px(p: &mut Px, c: &effectcraft_color::Conversion) {
    let a = p[3];
    if a <= 0.0 {
        return;
    }
    let o = c.apply([p[0] / a, p[1] / a, p[2] / a]);
    *p = [o[0] * a, o[1] * a, o[2] * a, a];
}

/// The exact view of `footage` ([`FootageSource::exact`]): for final renders (exports through
/// `effectcraft_export::Job::source`, Save Frame As), which must not depend on whether previews
/// decode on a hardware decoder. (Such renders must not share a layer cache with previews.)
pub fn exact_footage(footage: &dyn FootageSource) -> &dyn FootageSource {
    footage.exact().unwrap_or(footage)
}

/// How a [`FootageSource`] hands over float footage (OpenEXR, float TIFF), whose values are
/// linear light: encoded with a curve, as the compositor takes footage, or as stored. Other
/// footage is decoded the same way whatever it says. Interpret Footage ▸ Preserve RGB always
/// reads the values as stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FloatCurve {
    /// The sRGB curve (managed projects, and Working Gamma 2.2).
    #[default]
    Srgb,
    /// A pure 2.4 power curve (no working space, Working Gamma 2.4).
    Gamma24,
    /// The linear values as stored (Interpret As Linear Light encodes them afterwards).
    Linear,
}

/// Supplies decoded footage frames (implemented by the media layer).
pub trait FootageSource: Send + Sync {
    /// The frame of `item` at source time `t`, straight from the file (any size).
    fn frame(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<Arc<Image>>;
    /// [`Self::frame`] as the source keeps it: a compact frame is converted by the caller,
    /// straight into its own buffer.
    fn frame_pixels(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<SharedFrame> {
        self.frame(item, footage, t).map(|img| img as SharedFrame)
    }
    /// The frame for a layer of nominal size `w`×`h` drawn at render scale `scale`: sources
    /// that keep frames at preview size return it resampled to [`preview_size`] (exactly as the
    /// renderer resamples it; `preview` set), others the frame as [`Self::frame_pixels`].
    fn preview_frame(&self, item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64) -> Option<FootageFrame> {
        let _ = (w, h, scale);
        self.frame_pixels(item, footage, t).map(|px| FootageFrame { px, preview: false })
    }
    /// [`Self::frame`] with float footage handed over as `float` says ([`Self::frame`] uses
    /// [`FloatCurve::Srgb`]). `size` is the size a reduced-resolution render shows the frame at,
    /// a hint sources may ignore (the renderer resamples). Sources without float footage can
    /// leave the default, which ignores `float`.
    fn frame_with(&self, item: ItemId, footage: &Footage, t: Tick, size: Option<(u32, u32)>, float: FloatCurve) -> Option<Arc<Image>> {
        let _ = (size, float);
        self.frame(item, footage, t)
    }
    /// [`Self::frame_pixels`] with float footage handed over as `float` says.
    fn frame_pixels_with(&self, item: ItemId, footage: &Footage, t: Tick, float: FloatCurve) -> Option<SharedFrame> {
        match float {
            FloatCurve::Srgb => self.frame_pixels(item, footage, t),
            _ => self.frame_with(item, footage, t, None, float).map(|img| img as SharedFrame),
        }
    }
    /// [`Self::preview_frame`] with float footage handed over as `float` says.
    #[allow(clippy::too_many_arguments)]
    fn preview_frame_with(&self, item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64, float: FloatCurve) -> Option<FootageFrame> {
        match float {
            FloatCurve::Srgb => self.preview_frame(item, footage, t, w, h, scale),
            _ => self.frame_pixels_with(item, footage, t, float).map(|px| FootageFrame { px, preview: false }),
        }
    }
    /// `frames` interleaved stereo sample frames of `item`'s audio from source time `t` at
    /// `rate` Hz (`None` when unavailable). The first sample is `t.to_units_floor(rate)`;
    /// a sample boundary uses the earliest representable tick that selects that sample.
    fn audio(&self, _item: ItemId, _footage: &Footage, _t: Tick, _frames: usize, _rate: u32) -> Option<Vec<f32>> {
        None
    }
    /// Auxiliary 3D channels of `item` at source time `t` (multi-layer OpenEXR: depth, IDs,
    /// Cryptomatte, any named channel), with `scale` relative to the file's pixels. `None` when
    /// the footage has none.
    fn aux(&self, _item: ItemId, _footage: &Footage, _t: Tick) -> Option<Arc<effectcraft_raster::AuxChannels>> {
        None
    }
    /// The parsed 3D model of a [`effectcraft_project::FootageKind::Model`] item (Advanced 3D).
    fn model(&self, _item: ItemId, _footage: &Footage) -> Option<Arc<effectcraft_model::Model>> {
        None
    }
    /// Set the decoded-frame cache budget in bytes (Settings ▸ Memory & CPU); sources without
    /// a cache ignore it.
    fn set_cache_budget(&self, _bytes: usize) {}
    /// Drop the decoded frames it keeps (Edit ▸ Purge ▸ All Memory / Image Cache Memory) and close
    /// the files nobody is reading: the next reads decode again. Sources without a cache ignore it.
    fn purge(&self) {}
    /// The file at `path` changed on disk (Reload Footage): drop what it keeps of it (decoded
    /// frames, open decoders, parsed models), so the next reads see the new contents. Sources
    /// without a cache ignore it.
    fn forget(&self, _path: &str) {}
    /// A thumbnail of `footage` (its first frame) fitting in `max_side` × `max_side`, aspect
    /// kept, for browsing files that aren't in the project: sources don't cache the frame or keep
    /// the file open for it.
    fn thumbnail(&self, footage: &Footage, max_side: u32) -> Option<Image> {
        let img = self.frame(ItemId(u64::MAX), footage, Tick::ZERO)?;
        let (w, h) = (img.width.max(1), img.height.max(1));
        let s = (max_side.max(1) as f64 / w.max(h) as f64).min(1.0);
        if s >= 1.0 {
            return Some((*img).clone());
        }
        Some(effectcraft_raster::resample(&img, ((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1)))
    }
    /// Close the files (decoders, parsed models, cached frames) of footage `keep` doesn't name:
    /// called when footage leaves the project or another project is opened. Sources that keep
    /// nothing open ignore it.
    fn retain_files(&self, _keep: &dyn Fn(&str) -> bool) {}
    /// Settings ▸ Disk ▸ Conformed Audio Folder: where decoded audio is kept between reads
    /// (`None` = off); sources that don't decode audio ignore it.
    fn set_conform_folder(&self, _folder: Option<std::path::PathBuf>) {}
    /// Settings ▸ Previews ▸ Hardware ProRes Decoding: let previews decode footage on a hardware
    /// decoder (visually identical, not bit-exact). Sources without one ignore it.
    fn set_hardware_decoding(&self, _on: bool) {}
    /// Settings ▸ Media & Disk Cache ▸ Disk Cache: where decoded stills that are slow to decode
    /// persist between sessions (`None` = off). Sources without a still decoder ignore it.
    fn set_still_cache(&self, _disk: Option<Arc<crate::disk_cache::DiskCache>>) {}
    /// The view of this source that decodes exactly (never on a hardware decoder): what exports
    /// and other final renders read. `None`: this source is exact already. A source returns the
    /// same view every time (jobs compare footage sources by address).
    fn exact(&self) -> Option<&dyn FootageSource> {
        None
    }
    /// The decoded-frame cache budget in bytes, if the source has a cache.
    fn cache_budget(&self) -> Option<usize> {
        None
    }
    /// Vector footage (SVG) rasterised at `scale` × its pixel size, for Continuously Rasterize.
    /// `None` when the footage is not vector or cannot be read.
    fn vector_frame(&self, _item: ItemId, _footage: &Footage, _scale: f64) -> Option<Arc<Image>> {
        None
    }
    /// Run `f`, which reads frames from this source, as work that can be called off: once
    /// `cancel` is set, long decodes it starts (a movie seeking back to a keyframe and decoding
    /// forward) stop early and their frames come back `None`. Sources that can't stop a decode
    /// just run `f`. See [`CancellableFootage`].
    fn cancellable(&self, _cancel: &Arc<AtomicBool>, f: &mut dyn FnMut()) {
        f()
    }
    /// Reads through this source have been called off: frames may have come back `None` that
    /// exist, so what was made from them must not be kept.
    fn called_off(&self) -> bool {
        false
    }
    /// Why `footage` shows no picture although its file is there: a read of it failed because
    /// no decoder handles its codec, profile, chroma format or bit depth, or the stream is damaged
    /// (e.g. "H.264 (High 4:2:2) isn't supported by the built-in decoder: ..."). `None` while it
    /// decodes, and for sources that don't track failures. Stays set until the file is forgotten.
    fn problem(&self, _footage: &Footage) -> Option<String> {
        None
    }
}

/// A render's view of a [`FootageSource`] whose frame reads can be called off with `cancel`
/// (a viewer frame nobody waits for any more, #329). Every read goes through
/// [`FootageSource::cancellable`] on its own, so reads made for this render on another thread
/// (a parallel loop) answer to this render's flag, and reads for other renders never do.
pub struct CancellableFootage<'a> {
    pub inner: &'a dyn FootageSource,
    pub cancel: Arc<AtomicBool>,
}

impl CancellableFootage<'_> {
    fn read<R>(&self, f: impl FnOnce() -> Option<R>) -> Option<R> {
        if self.cancel.load(Ordering::Relaxed) {
            return None;
        }
        let (mut f, mut out) = (Some(f), None);
        self.inner.cancellable(&self.cancel, &mut || out = f.take().and_then(|f| f()));
        out
    }
}

impl FootageSource for CancellableFootage<'_> {
    fn frame(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<Arc<Image>> {
        self.read(|| self.inner.frame(item, footage, t))
    }
    fn frame_pixels(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<SharedFrame> {
        self.read(|| self.inner.frame_pixels(item, footage, t))
    }
    fn preview_frame(&self, item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64) -> Option<FootageFrame> {
        self.read(|| self.inner.preview_frame(item, footage, t, w, h, scale))
    }
    fn frame_with(&self, item: ItemId, footage: &Footage, t: Tick, size: Option<(u32, u32)>, float: FloatCurve) -> Option<Arc<Image>> {
        self.read(|| self.inner.frame_with(item, footage, t, size, float))
    }
    fn frame_pixels_with(&self, item: ItemId, footage: &Footage, t: Tick, float: FloatCurve) -> Option<SharedFrame> {
        self.read(|| self.inner.frame_pixels_with(item, footage, t, float))
    }
    fn preview_frame_with(&self, item: ItemId, footage: &Footage, t: Tick, w: u32, h: u32, scale: f64, float: FloatCurve) -> Option<FootageFrame> {
        self.read(|| self.inner.preview_frame_with(item, footage, t, w, h, scale, float))
    }
    fn audio(&self, item: ItemId, footage: &Footage, t: Tick, frames: usize, rate: u32) -> Option<Vec<f32>> {
        self.inner.audio(item, footage, t, frames, rate)
    }
    fn aux(&self, item: ItemId, footage: &Footage, t: Tick) -> Option<Arc<effectcraft_raster::AuxChannels>> {
        self.inner.aux(item, footage, t)
    }
    fn model(&self, item: ItemId, footage: &Footage) -> Option<Arc<effectcraft_model::Model>> {
        self.inner.model(item, footage)
    }
    fn set_cache_budget(&self, bytes: usize) {
        self.inner.set_cache_budget(bytes)
    }
    fn purge(&self) {
        self.inner.purge()
    }
    fn forget(&self, path: &str) {
        self.inner.forget(path)
    }
    fn thumbnail(&self, footage: &Footage, max_side: u32) -> Option<Image> {
        self.inner.thumbnail(footage, max_side)
    }
    fn retain_files(&self, keep: &dyn Fn(&str) -> bool) {
        self.inner.retain_files(keep)
    }
    fn set_conform_folder(&self, folder: Option<std::path::PathBuf>) {
        self.inner.set_conform_folder(folder)
    }
    fn set_hardware_decoding(&self, on: bool) {
        self.inner.set_hardware_decoding(on)
    }
    fn set_still_cache(&self, disk: Option<Arc<crate::disk_cache::DiskCache>>) {
        self.inner.set_still_cache(disk)
    }
    fn exact(&self) -> Option<&dyn FootageSource> {
        // (the exact view reads without this wrapper's cancellation: final renders finish)
        Some(exact_footage(self.inner))
    }
    fn cache_budget(&self) -> Option<usize> {
        self.inner.cache_budget()
    }
    fn vector_frame(&self, item: ItemId, footage: &Footage, scale: f64) -> Option<Arc<Image>> {
        self.inner.vector_frame(item, footage, scale)
    }
    fn cancellable(&self, cancel: &Arc<AtomicBool>, f: &mut dyn FnMut()) {
        self.inner.cancellable(cancel, f)
    }
    fn called_off(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.inner.called_off()
    }
    fn problem(&self, footage: &Footage) -> Option<String> {
        self.inner.problem(footage)
    }
}

/// Footage that can be rasterised at any scale (SVG, PDF, AI, EPS). Footage without a codec name
/// (converted from an After Effects project, never probed) is judged by its file extension: the
/// Motion Library's Figma PDFs came in that way and were drawn at their native size, blurry at
/// any Continuously Rasterize scale.
pub fn is_vector_footage(f: &Footage) -> bool {
    match f.codec.as_str() {
        "SVG" | "PDF" | "AI" | "EPS" => true,
        "" => std::path::Path::new(&f.path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["svg", "pdf", "ai", "eps"].iter().any(|v| e.eq_ignore_ascii_case(v))),
        _ => false,
    }
}

/// No footage available (renders footage layers as transparent).
pub struct NoFootage;
impl FootageSource for NoFootage {
    fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<Image>> {
        None
    }
}

/// Compact native footage and its placement/colour interpretation. RGB resampling
/// must precede `conversion`, just as on the CPU preview path.
pub struct NativeFootage {
    pub pixels: SharedFrame,
    pub preview: Option<(u32, u32)>,
    pub scale: f64,
    pub conversion: Option<effectcraft_color::Conversion>,
}

impl<'a> Renderer<'a> {
    /// Resolve original dimensions, proxy selection and source time together.
    pub(crate) fn footage_selection(&self, ctx: &EvalCtx, layer: &Layer) -> Option<(ItemId, &'a Footage, &'a Footage, Tick)> {
        let effectcraft_project::LayerSource::Footage { item } = layer.source else { return None };
        let ItemKind::Footage(original) = &self.project.item(item)?.kind else { return None };
        if !original.has_video {
            return None;
        }
        Some((item, original, self.proxy_for(item).unwrap_or(original), ctx.source_time(layer)))
    }

    /// Native planar footage for an accelerator. Interpretations requiring CPU
    /// temporal sampling deliberately retain the existing footage path.
    pub fn native_footage(&self, ctx: &EvalCtx, layer: &Layer) -> Option<NativeFootage> {
        let (item, original, footage, time) = self.footage_selection(ctx, layer)?;
        // Non-power preview ratios can amplify CPU/GPU bilinear-coordinate
        // rounding beyond the integer-depth oracle. Keep those on the reference path.
        if ![1.0, 0.5, 0.25].contains(&self.opts.scale) {
            return None;
        }
        if self.content_times(ctx, layer).is_some()
            || footage.fields != effectcraft_project::FieldOrder::Off
            || footage.invert_alpha
            || footage.linear_light_applies()
            || frame_blend_mode(ctx, layer) != FrameBlend::Off
            || is_vector_footage(original)
            || is_vector_footage(footage)
        {
            return None;
        }
        let conversion = self.pipe.media_in(footage);
        // Integer working-space buffers amplify tiny resampling/conversion
        // differences at quantisation thresholds (especially dark linear RGB).
        // Keep the CPU reference conversion until that boundary is exact.
        if conversion.is_some() && self.pipe.levels.is_some() {
            return None;
        }
        let pixels = self.footage.frame_pixels_with(item, footage, time, self.pipe.float_curve(footage))?;
        let view = pixels.yuv()?;
        view.validate().ok()?;
        if pixels.size() != (view.width, view.height) {
            return None;
        }
        let preview = preview_size(view.width, original.width, original.height, self.opts.scale);
        let scale = if preview.is_some() { self.opts.scale } else { view.width as f64 / original.width.max(1) as f64 };
        Some(NativeFootage { pixels, preview, scale, conversion })
    }

    /// The proxy footage used for `item` under this render's Proxy Use, if any.
    pub fn proxy_for(&self, item: ItemId) -> Option<&'a Footage> {
        let it = self.project.item(item)?;
        let px = it.proxy.as_deref()?;
        (self.opts.proxy.uses(matches!(it.kind, ItemKind::Comp(_)), px.enabled) && px.footage.has_video).then_some(&px.footage)
    }

    /// A decoded frame of `f` as a layer buffer covering `w`×`h` layer pixels (the footage's
    /// nominal size; proxies are scaled up to it), converted from its colour profile.
    ///
    /// The frame is written into the buffer once: a compact frame is converted from its samples
    /// and the colour conversion applied in the same pass (the cached f32 frame used to be
    /// copied, then swept again).
    pub(crate) fn footage_buf(&self, img: &impl AsFootageFrame, f: &Footage, w: u32, h: u32) -> Buf {
        let s = self.opts.scale;
        let px = img.pixels();
        let conv = self.pipe.media_in(f);
        // Resampled to the preview size by the source already.
        if img.is_preview() {
            let mut out = expand(px, conv.as_ref());
            if let Some((rw, rh)) = preview_size(w, w, h, s)
                && (out.width, out.height) != (rw, rh)
            {
                out = effectcraft_raster::resample(&out, rw, rh);
            }
            return Buf { img: out, offset: [0.0; 2], scale: s };
        }
        let (iw, _) = px.size();
        let k = if iw > 0 { w.max(1) as f64 / iw as f64 } else { 1.0 };
        // Keep native pixels when downsampling is small; resample otherwise.
        match preview_size(iw, w, h, s) {
            Some((rw, rh)) => {
                let mut out = match px.image() {
                    Some(full) => effectcraft_raster::resample(full, rw, rh),
                    None => effectcraft_raster::resample(&expand(px, None), rw, rh),
                };
                if let Some(c) = &conv {
                    color::convert(&mut out, c);
                }
                Buf { img: out, offset: [0.0; 2], scale: s }
            }
            None => Buf { img: expand(px, conv.as_ref()), offset: [0.0; 2], scale: 1.0 / k },
        }
    }

    /// A footage frame at source time `t`, frame-blended between the two nearest source frames
    /// when the layer's Frame Blending switch (and the comp's Enable Frame Blending) is on and
    /// `t` falls between frames (footage rate ≠ comp rate, time stretch, time remapping).
    pub(crate) fn footage_frame(&self, ctx: &EvalCtx, layer: &Layer, item: ItemId, f: &Footage, t: Tick) -> Option<FootageFrame> {
        let full = |img: Arc<Image>| FootageFrame { px: img, preview: false };
        // Interpret Footage ▸ Separate Fields: each field is a frame at twice the rate.
        if f.fields != effectcraft_project::FieldOrder::Off && matches!(f.kind, FootageKind::Video | FootageKind::Sequence) {
            let field_rate = FrameRate::new(f.frame_rate.num * 2, f.frame_rate.den);
            let i = field_rate.frame_at(t).max(0);
            let img = self.footage.frame_with(item, f, f.frame_rate.tick_of(i / 2), None, self.pipe.float_curve(f))?;
            let dominant_upper = f.fields == effectcraft_project::FieldOrder::UpperFirst;
            // Upper field = even lines (0, 2, …).
            let parity = if (i % 2 == 0) == dominant_upper { 0 } else { 1 };
            return Some(full(Arc::new(interpret_pixels(&field_frame(&img, parity), f, self.pipe.linear_footage_space(f)))));
        }
        if f.invert_alpha || f.linear_light_applies() {
            let img = self.footage_frame_raw(ctx, layer, item, f, t)?;
            return Some(full(Arc::new(interpret_pixels(&img, f, self.pipe.linear_footage_space(f)))));
        }
        if let Some(img) = self.blended_frame(ctx, layer, item, f, t) {
            return Some(full(img));
        }
        let float = self.pipe.float_curve(f);
        // The layer's nominal size: the proxy fills the footage's frame.
        let nominal = match self.project.item(item).map(|it| &it.kind) {
            Some(ItemKind::Footage(orig)) => Some((orig.width, orig.height)),
            _ => None,
        };
        match nominal {
            Some((w, h)) if self.opts.scale < 0.75 => self.footage.preview_frame_with(item, f, t, w, h, self.opts.scale, float),
            _ => self.footage.frame_pixels_with(item, f, t, float).map(|px| FootageFrame { px, preview: false }),
        }
    }

    fn footage_frame_raw(&self, ctx: &EvalCtx, layer: &Layer, item: ItemId, f: &Footage, t: Tick) -> Option<Arc<Image>> {
        if let Some(img) = self.blended_frame(ctx, layer, item, f, t) {
            return Some(img);
        }
        self.footage.frame_with(item, f, t, None, self.pipe.float_curve(f))
    }

    /// The frame-blended footage frame at `t` (`None` when frame blending doesn't apply there).
    fn blended_frame(&self, ctx: &EvalCtx, layer: &Layer, item: ItemId, f: &Footage, t: Tick) -> Option<Arc<Image>> {
        let mode = frame_blend_mode(ctx, layer);
        let float = self.pipe.float_curve(f);
        if mode != FrameBlend::Off && matches!(f.kind, FootageKind::Video | FootageKind::Sequence) {
            let (i, w) = frame_position(t, f.frame_rate);
            if w > 1e-6 {
                // Blended frames are cached (Pixel Motion is expensive).
                let key = self.cache.map(|_| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (0x00f8_a3e5_u64, item, i, (w as f32).to_bits(), mode as u8, f.frame_rate.num, f.frame_rate.den, &f.path, float).hash(&mut h);
                    h.finish()
                });
                if let (Some(c), Some(k)) = (self.cache, key)
                    && let Some(b) = c.get(k)
                {
                    return Some(Arc::new(b.img.clone()));
                }
                let a = self.footage.frame_with(item, f, f.frame_rate.tick_of(i), None, float);
                let b = self.footage.frame_with(item, f, f.frame_rate.tick_of(i + 1), None, float);
                if let (Some(a), Some(b)) = (a, b) {
                    let img = blend_frames(mode, &a, &b, w as f32);
                    if let (Some(c), Some(k)) = (self.cache_to_keep(), key) {
                        c.insert(k, Arc::new(Buf { img: img.clone(), offset: [0.0; 2], scale: 1.0 }));
                    }
                    return Some(Arc::new(img));
                }
            }
        }
        None
    }
}

/// The layer's frame blending mode, if the comp's Enable Frame Blending switch is on.
/// One field of an interlaced frame as a full frame: the field's lines (`parity` 0 = even /
/// upper, 1 = odd / lower) kept, the other lines interpolated from their neighbours.
pub fn field_frame(img: &Image, parity: usize) -> Image {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut out = img.clone();
    if h < 2 {
        return out;
    }
    for y in 0..h {
        if y % 2 == parity {
            continue;
        }
        let above = y.checked_sub(1);
        let below = (y + 1 < h).then_some(y + 1);
        for x in 0..w {
            let px = match (above, below) {
                (Some(a), Some(b)) => {
                    let (p, q) = (img.data[a * w + x], img.data[b * w + x]);
                    [(p[0] + q[0]) * 0.5, (p[1] + q[1]) * 0.5, (p[2] + q[2]) * 0.5, (p[3] + q[3]) * 0.5]
                }
                (Some(a), None) => img.data[a * w + x],
                (None, Some(b)) => img.data[b * w + x],
                (None, None) => img.data[y * w + x],
            };
            out.data[y * w + x] = px;
        }
    }
    out
}

/// Interpret Footage ▸ Invert Alpha and Interpret As Linear Light on decoded (premultiplied)
/// pixels; linear light is encoded in `space` ([`color::Pipe::linear_footage_space`]).
pub fn interpret_pixels(img: &Image, f: &Footage, space: effectcraft_color::ColorSpace) -> Image {
    let linear = f.linear_light_applies();
    if !f.invert_alpha && !linear {
        return img.clone();
    }
    let mut out = img.clone();
    for p in out.data.iter_mut() {
        let a = p[3];
        let mut c = if a > 1e-6 { [p[0] / a, p[1] / a, p[2] / a] } else { [0.0; 3] };
        if linear {
            // The file's values are linear light: encode them like the rest of the footage.
            c = c.map(|v| space.encode(v));
        }
        let a = if f.invert_alpha { 1.0 - a } else { a };
        *p = [c[0] * a, c[1] * a, c[2] * a, a];
    }
    out
}

pub(crate) fn frame_blend_mode(ctx: &EvalCtx, layer: &Layer) -> FrameBlend {
    if ctx.comp.enable_frame_blending { layer.switches.frame_blend } else { FrameBlend::Off }
}

/// Position of source time `t` among frames at `rate`: the frame at or before `t` and the
/// fraction (0..1) of the way to the next frame. Exact (integer tick arithmetic), so frames that
/// line up exactly report a fraction of 0.
pub fn frame_position(t: Tick, rate: FrameRate) -> (i64, f64) {
    let num = t.0 as i128 * rate.num as i128;
    let den = TICKS_PER_SECOND as i128 * rate.den as i128;
    (num.div_euclid(den) as i64, num.rem_euclid(den) as f64 / den as f64)
}

/// Blend two neighbouring frames `w` of the way from `a` to `b`: a cross-fade (Frame Mix) or a
/// motion-compensated interpolation (Pixel Motion).
pub fn blend_frames(mode: FrameBlend, a: &Image, b: &Image, w: f32) -> Image {
    match mode {
        FrameBlend::PixelMotion => effectcraft_raster::flow::interpolate(a, b, w),
        _ => effectcraft_raster::flow::mix(a, b, w),
    }
}
