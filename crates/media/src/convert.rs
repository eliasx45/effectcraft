//! Decoded pictures → compositor [`Image`]s (premultiplied f32 RGBA, sRGB/Rec.709-encoded values).
//!
//! Y'CbCr is converted with the matrix and range the stream signals (BT.601 / BT.709 / BT.2020,
//! limited or full), upsampling chroma bilinearly with centred siting (like ffmpeg's swscale,
//! which is also how most tools subsample when encoding).
//! Values stay gamma-encoded: like After Effects, footage pixels enter the working space as they
//! are encoded in the file. Alpha follows the footage's [`AlphaMode`] interpretation.

use effectcraft_project::{AlphaMode, Footage};
use effectcraft_raster::{Image, Px};
use effectcraft_render::FloatCurve;
use filmcraft_color::{Matrix, Range};
use filmcraft_frame::{Chroma, PixelData, VideoFrame};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// How to turn a (colour, alpha) sample into a premultiplied pixel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AlphaOp {
    mode: AlphaMode,
    matte: [f32; 3],
    /// How float values (linear light) are encoded: sRGB by default, a pure 2.4 power curve
    /// (Working Gamma 2.4), or not at all (Preserve RGB, Interpret As Linear Light).
    float: FloatCurve,
}

impl AlphaOp {
    pub(crate) fn new(mode: AlphaMode, matte: [f32; 3]) -> Self {
        Self { mode, matte, float: FloatCurve::Srgb }
    }
    /// The interpretation of footage `f`, float values handed over as `float` says (as stored
    /// under Preserve RGB, whatever it says).
    pub(crate) fn of(f: &Footage, float: FloatCurve) -> Self {
        let float = if f.preserve_rgb { FloatCurve::Linear } else { float };
        Self { float, ..Self::new(f.alpha, f.premul_color) }
    }
    /// How float values are encoded.
    pub(crate) fn float_curve(self) -> FloatCurve {
        self.float
    }
    /// A linear float value as the compositor takes it.
    #[inline(always)]
    fn float(self, v: f32) -> f32 {
        match self.float {
            FloatCurve::Srgb => encode(v),
            FloatCurve::Gamma24 => v.abs().powf(1.0 / 2.4).copysign(v),
            FloatCurve::Linear => v,
        }
    }
    /// `c` is the file's colour (straight or premultiplied as the mode says), `a` its alpha.
    #[inline(always)]
    fn apply(self, c: [f32; 3], a: f32) -> Px {
        match self.mode {
            AlphaMode::Ignore => [c[0], c[1], c[2], 1.0],
            AlphaMode::Straight => [c[0] * a, c[1] * a, c[2] * a, a],
            AlphaMode::Premultiplied => {
                // premultiplied against a matte colour m: c = s·a + m·(1−a) → s·a = c − m·(1−a)
                let k = 1.0 - a;
                let m = self.matte;
                [(c[0] - m[0] * k).max(0.0), (c[1] - m[1] * k).max(0.0), (c[2] - m[2] * k).max(0.0), a]
            }
        }
    }
}

/// Convert a FilmCraft frame to an [`Image`].
pub(crate) fn frame_to_image(f: &VideoFrame, op: AlphaOp) -> Image {
    frame_to_image_in(f, op, Vec::new())
}

/// Rows converted per parallel task.
const ROWS: usize = 8;

/// Convert a FilmCraft frame to an [`Image`], reusing `buf`'s allocation when it is large enough
/// (every pixel is overwritten, so a recycled frame's contents do not matter).
pub(crate) fn frame_to_image_in(f: &VideoFrame, op: AlphaOp, mut buf: Vec<Px>) -> Image {
    let _profile = effectcraft_render::profile::span("yuv_to_rgb");
    let (w, h) = (f.width as usize, f.height as usize);
    if buf.len() != w * h {
        buf = vec![[0.0; 4]; w * h];
    }
    let mut img = Image { width: f.width, height: f.height, data: buf };
    if w == 0 || h == 0 {
        return img;
    }
    img.data.par_chunks_mut(w * ROWS).enumerate().for_each(|(i, rows)| frame_rows(f, op, i * ROWS, rows));
    img
}

/// Rows `y0..` of `f` (as many whole rows as `out` holds) as compositor pixels: the pixels of
/// [`frame_to_image`], a band at a time.
pub(crate) fn frame_rows(f: &VideoFrame, op: AlphaOp, y0: usize, out: &mut [Px]) {
    let _profile = effectcraft_render::profile::span("pixel_conversion_rows");
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 {
        return;
    }
    match &f.data {
        PixelData::Rgba8(d) => {
            for (r, row) in out.chunks_mut(w).enumerate() {
                let y = y0 + r;
                let Some(src) = d.get(y * w * 4..(y + 1) * w * 4) else { continue };
                for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                    *o = op.apply([U8[s[0] as usize], U8[s[1] as usize], U8[s[2] as usize]], U8[s[3] as usize]);
                }
            }
        }
        PixelData::RgbaF32(d) => {
            // linear premultiplied → encoded premultiplied
            for (r, row) in out.chunks_mut(w).enumerate() {
                let y = y0 + r;
                let Some(src) = d.get(y * w * 4..(y + 1) * w * 4) else { continue };
                for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                    let a = s[3];
                    let inv = if a > 0.0 { 1.0 / a } else { 0.0 };
                    let c = [op.float(s[0] * inv) * a, op.float(s[1] * inv) * a, op.float(s[2] * inv) * a];
                    *o = if op.mode == AlphaMode::Ignore { [c[0], c[1], c[2], 1.0] } else { [c[0], c[1], c[2], a] };
                }
            }
        }
        PixelData::Yuv8 { planes, chroma, alpha } => {
            let yuv = Yuv::new(f.color.matrix, f.color.range, 8);
            let amax = 255.0;
            yuv_rows(w, h, *chroma, [&planes[0][..], &planes[1][..], &planes[2][..]], alpha.as_deref().map(|a| &a[..]), amax, &yuv, op, y0, out);
        }
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            let bits = (*bits).clamp(8, 16);
            let yuv = Yuv::new(f.color.matrix, f.color.range, bits);
            let amax = ((1u32 << bits) - 1) as f32;
            yuv_rows(w, h, *chroma, [&planes[0][..], &planes[1][..], &planes[2][..]], alpha.as_deref().map(|a| &a[..]), amax, &yuv, op, y0, out);
        }
    }
}

/// A 9–16-bit Y'CbCr frame with its samples rounded to 8 bits, for previews that keep footage
/// at 8 bits per channel (Settings ▸ Previews ▸ Decode Footage at 8 Bits for Previews): half the
/// memory (12-bit 4:4:4 + alpha, 8 bytes a pixel → 4), converted by the same matrix and range,
/// so the picture differs from the full-depth one by at most about one 8-bit step. `None` for
/// frames that are 8-bit (or RGBA, or float) already: they are kept as they are.
pub(crate) fn frame_to_8bit(f: &VideoFrame) -> Option<VideoFrame> {
    frame_to_8bit_on(f, Bands::Rayon)
}

/// How [`to_u8`] spreads a plane over threads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bands {
    /// Rayon's pool, into a new plane.
    Rayon,
    /// This thread, into a recycled plane ([`crate::recycle`]): for a CPU copy made on demand
    /// (Fix 3), which runs inside a lazy cell that rayon workers may be waiting on (a rayon join
    /// there could run another band of the same frame on this thread: a re-entrant
    /// initialisation).
    Here,
}

/// [`frame_to_8bit`], its planes made as `bands` says.
fn frame_to_8bit_on(f: &VideoFrame, bands: Bands) -> Option<VideoFrame> {
    let PixelData::Yuv16 { planes, chroma, bits, alpha } = &f.data else { return None };
    let bits = (*bits).clamp(8, 16);
    let max = (1u32 << bits) - 1;
    let shift = bits - 8;
    let half = (1u32 << shift) >> 1;
    // Limited range scales by 2^(bits-8) (16..235 at 8 bits is 64..940 at 10); full range by
    // max / 255 around the chroma midpoint. Alpha is always full range.
    let full = move |v: u16| ((u32::from(v).min(max) * 255 + max / 2) / max).min(255) as u8;
    // (in 16 bits, as `(min(v, max) + half) >> shift` in 32 cannot pass 16 bits: 8 samples a
    // NEON instruction, the same value; `shift` is at most 8, and a wrapping shift has no
    // per-sample overflow check in builds with overflow checks, which kept the loop scalar)
    let (max16, half16) = (u16::try_from(max).unwrap_or(u16::MAX), u16::try_from(half).unwrap_or(0));
    let limited = move |v: u16| v.min(max16).saturating_add(half16).wrapping_shr(shift).min(255) as u8;
    let mid = 1i64 << (bits - 1);
    let full_chroma =
        move |v: u16| (128 + ((i64::from(v).min(i64::from(max)) - mid) * 255 + i64::from(max / 2)).div_euclid(i64::from(max))).clamp(0, 255) as u8;
    let planes = match f.color.range {
        Range::Limited => [to_u8(&planes[0], limited, bands), to_u8(&planes[1], limited, bands), to_u8(&planes[2], limited, bands)],
        Range::Full => [to_u8(&planes[0], full, bands), to_u8(&planes[1], full_chroma, bands), to_u8(&planes[2], full_chroma, bands)],
    };
    // alpha at the decoders' own depths divides by a constant (a multiply the compiler
    // vectorises) instead of by `max`
    let alpha = alpha.as_ref().map(|a| match bits {
        10 => to_u8(a, full_at::<1023>, bands),
        12 => to_u8(a, full_at::<4095>, bands),
        _ => to_u8(a, full, bands),
    });
    Some(VideoFrame { width: f.width, height: f.height, data: PixelData::Yuv8 { planes, chroma: *chroma, alpha }, color: f.color, par: f.par, pts: f.pts })
}

/// A full-range sample of `MAX` (2^bits − 1) at 8 bits, rounded: `frame_to_8bit`'s `full`.
#[inline(always)]
fn full_at<const MAX: u32>(v: u16) -> u8 {
    ((u32::from(v).min(MAX) * 255 + MAX / 2) / MAX).min(255) as u8
}

/// `f` of every sample of a plane, in parallel bands. Each band is a plain loop over the plane
/// with `f` inlined, which the compiler vectorises (the limited-range rounding is a min, an add
/// and a shift); it was a parallel iterator over single samples through a `dyn Fn`, 43–57 % of
/// all CPU time while previews decoded 1080p ProRes 4444 at 8 bits (1 Nov 2026 profile).
fn to_u8(p: &[u16], f: impl Fn(u16) -> u8 + Sync, bands: Bands) -> Arc<Vec<u8>> {
    const BAND: usize = 1 << 18;
    let band = |o: &mut [u8], s: &[u16]| {
        for (o, v) in o.iter_mut().zip(s) {
            *o = f(*v);
        }
    };
    if bands == Bands::Rayon {
        let mut out = vec![0u8; p.len()];
        out.par_chunks_mut(BAND).zip(p.par_chunks(BAND)).for_each(|(o, s)| band(o, s));
        return Arc::new(out);
    }
    // (on this thread: the loop is vectorised, ~0.1-0.3 ms a 1080p plane; helper threads
    // started per plane cost more than that on a loaded machine)
    let mut out = crate::recycle::take8(p.len());
    band(&mut out, p);
    Arc::new(out)
}

/// A decoded picture kept compactly and turned into compositor pixels on use, exactly as
/// [`frame_to_image`] / [`dynamic_to_image`] convert it: the decoder's own samples (8-bit 4:2:0
/// Y'CbCr is 1.5 bytes a pixel, 10-bit 3) or a still's 8 / 16-bit RGBA, instead of 16 bytes a
/// pixel of f32.
pub(crate) enum Packed {
    Video {
        frame: Arc<VideoFrame>,
        op: AlphaOp,
    },
    Rgba8 {
        width: u32,
        height: u32,
        data: Vec<u8>,
        op: AlphaOp,
    },
    Rgba16 {
        width: u32,
        height: u32,
        data: Vec<u16>,
        op: AlphaOp,
    },
    /// Fix 3: a hardware-decoded picture kept in its decoder's surface; its CPU samples are made
    /// when a CPU reader first asks ([`Lazy`]).
    Resident(Box<Lazy>),
}

/// CPU copies made on demand of a media pool's frames kept in decoder surfaces ([`Lazy`]): the
/// bytes they hold now (counted with the pool's cache) and how many were made.
#[derive(Debug, Default)]
pub(crate) struct LazyMeter {
    pub(crate) bytes: AtomicUsize,
    pub(crate) copies: AtomicU64,
}

/// Milliseconds since the first call (never 0), for [`Lazy::hint`].
pub(crate) fn now_ms() -> u64 {
    static EPOCH: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);
    u64::try_from(EPOCH.elapsed().as_millis()).unwrap_or(u64::MAX).saturating_add(1)
}

/// Fix 3 (1 Nov 2026): a hardware-decoded picture left in its decoder's surface
/// ([`crate::hw::ResidentFrame`]), with no CPU samples until a CPU reader asks for them.
///
/// The GPU compositor reads it from the surface ([`effectcraft_render::FramePixels::gpu_yuv`]:
/// a view without samples, keyed by the stand-in's luma allocation), at full depth or, for
/// Decode Footage at 8 Bits (`eight`), rounded to 8 bits in its shader exactly as
/// [`frame_to_8bit`] rounds. Everything else (CPU renders, CPU-only layer work, Save Frame,
/// thumbnails, `yuv()` for a GPU that can't read the surface) asks for the CPU frame
/// ([`Lazy::cpu`]): copied out of the surface (and rounded to 8 bits) once, into recycled
/// buffers ([`crate::recycle`]), kept with this frame, and the same samples a decode that
/// copied at once gave, so the pixels are identical either way.
pub(crate) struct Lazy {
    /// The decoder's stand-in for the picture: size, layout, colour (its planes hold no
    /// samples); its luma allocation keys the surface and stays alive with this frame.
    stand_in: Arc<VideoFrame>,
    picture: Arc<dyn crate::hw::ResidentFrame>,
    op: AlphaOp,
    /// Kept at 8 bits (Decode Footage at 8 Bits): the CPU copy is rounded, the GPU rounds.
    eight: bool,
    cpu: OnceLock<Option<VideoFrame>>,
    meter: Arc<LazyMeter>,
    /// When a CPU copy of a frame of this movie was last made ([`now_ms`]; the media pool
    /// copies the movie's next frames at decode for a while then).
    hint: Arc<AtomicU64>,
}

impl Lazy {
    pub(crate) fn new(stand_in: Arc<VideoFrame>, picture: Arc<dyn crate::hw::ResidentFrame>, op: AlphaOp, meter: Arc<LazyMeter>, hint: Arc<AtomicU64>) -> Self {
        Self { stand_in, picture, op, eight: false, cpu: OnceLock::new(), meter, hint }
    }

    /// The stand-in's layout: chroma, depth of the decoder's samples, alpha.
    fn layout(&self) -> Option<(Chroma, u32, bool)> {
        match &self.stand_in.data {
            PixelData::Yuv16 { chroma, bits, alpha, .. } => Some((*chroma, *bits, alpha.is_some())),
            _ => None,
        }
    }

    /// The frame at 8 bits (the GPU rounds; a CPU copy is rounded when made). `None`: it is
    /// already, or the GPU can't round it (only limited-range 10 / 12-bit pictures, as Fix 2).
    fn to_8bit(&self) -> Option<Lazy> {
        let (_, bits, _) = self.layout()?;
        if self.eight || !matches!(bits, 10 | 12) || self.stand_in.color.range != Range::Limited {
            return None;
        }
        Some(Lazy {
            stand_in: self.stand_in.clone(),
            picture: self.picture.clone(),
            op: self.op,
            eight: true,
            cpu: OnceLock::new(),
            meter: self.meter.clone(),
            hint: self.hint.clone(),
        })
    }

    /// The CPU frame, made now the first time (`None`: the copy failed; logged).
    pub(crate) fn cpu(&self) -> Option<&VideoFrame> {
        self.cpu.get_or_init(|| self.make()).as_ref()
    }

    /// Whether the CPU frame was made.
    #[cfg(test)]
    pub(crate) fn made(&self) -> bool {
        self.cpu.get().is_some_and(Option::is_some)
    }

    /// Whether it is kept at 8 bits.
    #[cfg(test)]
    pub(crate) fn is_eight(&self) -> bool {
        self.eight
    }

    fn make(&self) -> Option<VideoFrame> {
        let _profile = effectcraft_render::profile::span("resident_copy");
        let (chroma, bits, alpha) = self.layout()?;
        let f = &self.stand_in;
        let (sx, sy) = chroma.shifts();
        let n = (f.width as usize).saturating_mul(f.height as usize);
        let cn = (f.width as usize).div_ceil(1 << sx).saturating_mul((f.height as usize).div_ceil(1 << sy));
        let reuse = crate::hw::Planes16 {
            y: crate::recycle::take16(n),
            cb: crate::recycle::take16(cn),
            cr: crate::recycle::take16(cn),
            alpha: alpha.then(|| crate::recycle::take16(n)),
        };
        let p = match self.picture.planes(reuse) {
            Ok(p) if p.y.len() == n && p.cb.len() == cn && p.cr.len() == cn && p.alpha.as_ref().map(Vec::len) == alpha.then_some(n) => p,
            Ok(_) => {
                log::warn!("media: a frame kept on the GPU came back with planes of the wrong size");
                return None;
            }
            Err(e) => {
                log::warn!("media: a frame kept on the GPU could not be copied out: {e}");
                return None;
            }
        };
        let full = VideoFrame {
            width: f.width,
            height: f.height,
            data: PixelData::Yuv16 { planes: [Arc::new(p.y), Arc::new(p.cb), Arc::new(p.cr)], chroma, bits, alpha: p.alpha.map(Arc::new) },
            color: f.color,
            par: f.par,
            pts: f.pts,
        };
        let out = if self.eight {
            let e = frame_to_8bit_on(&full, Bands::Here);
            // (the full-depth copy goes back for the next one)
            crate::recycle::frame(full);
            e
        } else {
            Some(full)
        };
        if let Some(o) = &out {
            self.meter.bytes.fetch_add(o.byte_size(), Ordering::Relaxed);
            self.meter.copies.fetch_add(1, Ordering::Relaxed);
            self.hint.store(now_ms(), Ordering::Relaxed);
        }
        out
    }

    /// The view the GPU reads from the surface (no samples: [`View::surface`]).
    fn gpu_view(&self) -> Option<effectcraft_render::yuv::View<'_>> {
        use effectcraft_render::yuv::Samples;
        let (chroma, bits, alpha) = self.layout()?;
        let PixelData::Yuv16 { planes, .. } = &self.stand_in.data else { return None };
        let (empty, bits) = if self.eight { (Samples::U8(&[]), 8) } else { (Samples::U16(&[]), bits) };
        let f = &self.stand_in;
        let mut v = native_view(f.width, f.height, chroma, bits, f.color, self.op, [empty; 3], alpha.then_some(empty))?;
        v.surface = Some(planes[0].as_ptr() as usize);
        Some(v)
    }
}

impl Drop for Lazy {
    fn drop(&mut self) {
        if let Some(Some(f)) = self.cpu.take() {
            self.meter.bytes.fetch_sub(f.byte_size().min(self.meter.bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
            crate::recycle::frame(f);
        }
    }
}

/// The native YUV view of a `width`×`height` picture (`chroma`, `bits`, `color`, alpha as `op`
/// interprets it) over `samples` (rows packed); `None` for a layout the GPU decode doesn't take.
#[allow(clippy::too_many_arguments)]
fn native_view<'a>(
    width: u32,
    height: u32,
    chroma: Chroma,
    bits: u32,
    color: filmcraft_color::ColorInfo,
    op: AlphaOp,
    samples: [effectcraft_render::yuv::Samples<'a>; 3],
    alpha: Option<effectcraft_render::yuv::Samples<'a>>,
) -> Option<effectcraft_render::yuv::View<'a>> {
    use effectcraft_render::yuv::{ChromaSiting, Coefficients, Plane, View};
    let (sx, sy) = chroma.shifts();
    if sx > 1 || sy > 1 {
        return None;
    }
    let cw = width.div_ceil(1 << sx);
    let ch = height.div_ceil(1 << sy);
    let plane = |samples, w, h| Plane { samples, width: w, height: h, stride: w as usize };
    let planes = [plane(samples[0], width, height), plane(samples[1], cw, ch), plane(samples[2], cw, ch)];
    let k = Yuv::new(color.matrix, color.range, bits);
    Some(View {
        width,
        height,
        planes,
        alpha: alpha.map(|a| plane(a, width, height)),
        bits,
        subsampling: [sx, sy],
        siting: ChromaSiting::Centered,
        coefficients: Coefficients { luma: [k.y_off, k.y_scale], chroma: [k.c_off, k.c_scale], rgb: [k.cr_r, k.cr_g, k.cb_g, k.cb_b] },
        alpha_mode: op.mode,
        matte: op.matte,
        surface: None,
    })
}

impl Packed {
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            Packed::Video { frame, .. } => (frame.width, frame.height),
            Packed::Rgba8 { width, height, .. } | Packed::Rgba16 { width, height, .. } => (*width, *height),
            Packed::Resident(l) => (l.stand_in.width, l.stand_in.height),
        }
    }

    /// The frame with its samples rounded to 8 bits ([`frame_to_8bit`]; 16-bit stills to 8-bit
    /// RGBA), for previews that keep footage at 8 bits. `None` when it is 8-bit already.
    pub(crate) fn to_8bit(&self) -> Option<Packed> {
        match self {
            Packed::Video { frame, op } => frame_to_8bit(frame).map(|f| {
                // (the decoder surface the GPU may read goes with the 8-bit frame)
                crate::hw::rounded_to_8bit(frame, &f);
                Packed::Video { frame: Arc::new(f), op: *op }
            }),
            Packed::Rgba16 { width, height, data, op } => Some(Packed::Rgba8 {
                width: *width,
                height: *height,
                data: data.par_iter().map(|v| ((u32::from(*v) * 255 + 32_767) / 65_535) as u8).collect(),
                op: *op,
            }),
            Packed::Rgba8 { .. } => None,
            // Fix 3: the GPU rounds; a CPU copy, when one is made, is rounded then
            Packed::Resident(l) => match l.to_8bit() {
                Some(e) => Some(Packed::Resident(Box::new(e))),
                // (a layout the GPU can't round: rounded on the CPU now, as before)
                None if !l.eight => l.cpu().and_then(frame_to_8bit).map(|f| Packed::Video { frame: Arc::new(f), op: l.op }),
                None => None,
            },
        }
    }

    /// Bytes of samples held.
    pub(crate) fn bytes(&self) -> usize {
        match self {
            Packed::Video { frame, .. } => match &frame.data {
                PixelData::Rgba8(d) => d.len(),
                PixelData::RgbaF32(d) => d.len() * 4,
                PixelData::Yuv8 { planes, alpha, .. } => planes.iter().map(|p| p.len()).sum::<usize>() + alpha.as_ref().map_or(0, |a| a.len()),
                PixelData::Yuv16 { planes, alpha, .. } => (planes.iter().map(|p| p.len()).sum::<usize>() + alpha.as_ref().map_or(0, |a| a.len())) * 2,
            },
            Packed::Rgba8 { data, .. } => data.len(),
            Packed::Rgba16 { data, .. } => data.len() * 2,
            // the surface (a CPU copy made later is counted by its pool's [`LazyMeter`])
            Packed::Resident(l) => l.picture.bytes(),
        }
    }

    /// Rows `y0..` (as many whole rows as `out` holds) as compositor pixels.
    pub(crate) fn rows(&self, y0: usize, out: &mut [Px]) {
        match self {
            Packed::Video { frame, op } => frame_rows(frame, *op, y0, out),
            Packed::Resident(l) => {
                if let Some(f) = l.cpu() {
                    frame_rows(f, l.op, y0, out);
                }
            }
            Packed::Rgba8 { width, data, op, .. } => {
                let w = *width as usize;
                for (r, row) in out.chunks_mut(w.max(1)).enumerate() {
                    let y = y0 + r;
                    let Some(src) = data.get(y * w * 4..(y + 1) * w * 4) else { continue };
                    for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                        *o = op.apply([U8[s[0] as usize], U8[s[1] as usize], U8[s[2] as usize]], U8[s[3] as usize]);
                    }
                }
            }
            Packed::Rgba16 { width, data, op, .. } => {
                let w = *width as usize;
                let k = 1.0 / 65535.0;
                for (r, row) in out.chunks_mut(w.max(1)).enumerate() {
                    let y = y0 + r;
                    let Some(src) = data.get(y * w * 4..(y + 1) * w * 4) else { continue };
                    for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                        *o = op.apply([s[0] as f32 * k, s[1] as f32 * k, s[2] as f32 * k], s[3] as f32 * k);
                    }
                }
            }
        }
    }

    /// The whole picture as an [`Image`].
    pub(crate) fn to_image(&self) -> Image {
        if let Packed::Resident(l) = self {
            // (the CPU copy made once, before the bands that read it)
            l.cpu();
        }
        let (w, h) = self.size();
        let mut img = Image::new(w, h);
        if w == 0 || h == 0 {
            return img;
        }
        img.data.par_chunks_mut(w as usize * ROWS).enumerate().for_each(|(i, rows)| self.rows(i * ROWS, rows));
        img
    }
}

impl effectcraft_render::FramePixels for Packed {
    fn yuv(&self) -> Option<effectcraft_render::yuv::View<'_>> {
        use effectcraft_render::yuv::Samples;
        let (frame, op) = match self {
            Self::Video { frame, op } => (&**frame, *op),
            // Fix 3: the CPU samples, made now the first time
            Self::Resident(l) => (l.cpu()?, l.op),
            _ => return None,
        };
        let (samples, alpha, chroma, bits) = match &frame.data {
            PixelData::Yuv8 { planes, chroma, alpha } => {
                (planes.each_ref().map(|p| Samples::U8(&p[..])), alpha.as_ref().map(|p| Samples::U8(&p[..])), *chroma, 8)
            }
            PixelData::Yuv16 { planes, chroma, bits, alpha } => {
                (planes.each_ref().map(|p| Samples::U16(&p[..])), alpha.as_ref().map(|p| Samples::U16(&p[..])), *chroma, (*bits).clamp(8, 16))
            }
            _ => return None,
        };
        native_view(frame.width, frame.height, chroma, bits, frame.color, op, samples, alpha)
    }
    fn gpu_yuv(&self) -> Option<effectcraft_render::yuv::View<'_>> {
        match self {
            // Fix 3: read from the surface, no CPU copy
            Self::Resident(l) => l.gpu_view(),
            _ => self.yuv(),
        }
    }
    fn size(&self) -> (u32, u32) {
        Packed::size(self)
    }
    fn rows(&self, y0: usize, out: &mut [Px]) {
        Packed::rows(self, y0, out);
    }
}

/// A still decoded by the `image` crate, kept packed (8 / 16-bit) or as float pixels.
pub(crate) fn pack_dynamic(img: image::DynamicImage, op: AlphaOp) -> std::result::Result<Image, Packed> {
    use image::DynamicImage as D;
    let (width, height) = (img.width(), img.height());
    match img {
        D::ImageRgb32F(_) | D::ImageRgba32F(_) => Ok(dynamic_to_image(&img, op)),
        D::ImageLuma16(_) | D::ImageLumaA16(_) | D::ImageRgb16(_) | D::ImageRgba16(_) => {
            Err(Packed::Rgba16 { width, height, data: img.into_rgba16().into_raw(), op })
        }
        _ => Err(Packed::Rgba8 { width, height, data: img.into_rgba8().into_raw(), op }),
    }
}

/// A movie frame turned the way its file says it is shown ([`crate::probe::video_turn`]:
/// clockwise quarter turns; anything else leaves it as decoded).
pub(crate) fn turned(img: Image, turn: image::metadata::Orientation) -> Image {
    use image::metadata::Orientation;
    let (w, h) = (img.width as usize, img.height as usize);
    if img.data.len() != w * h || w == 0 || h == 0 {
        return img;
    }
    // The source pixel shown at (x, y) of the turned picture.
    let (tw, th, from): (usize, usize, fn(usize, usize, usize, usize) -> usize) = match turn {
        Orientation::Rotate90 => (h, w, |x, y, w, h| (h - 1 - x) * w + y),
        Orientation::Rotate180 => (w, h, |x, y, w, h| (h - 1 - y) * w + (w - 1 - x)),
        Orientation::Rotate270 => (h, w, |x, y, w, _| x * w + (w - 1 - y)),
        _ => return img,
    };
    let mut data = vec![[0.0; 4]; tw * th];
    data.par_chunks_mut(tw).enumerate().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            if let Some(s) = img.data.get(from(x, y, w, h)) {
                *px = *s;
            }
        }
    });
    Image { width: tw as u32, height: th as u32, data }
}

/// A FilmCraft frame for a render that shows it at `w` × `h`, at most half its size each way:
/// what `resample(&frame_to_image(f, op), w, h)` makes, converting a quarter of the pixels.
/// Opaque 4:2:0 Y'CbCr is box-filtered to half size before conversion (each 2×2 block of luma,
/// and the [1 6 1]/8 neighbourhood of its chroma sample: the mean of the upsampled chroma its
/// pixels get), then resampled the rest of the way. Conversion is linear until it clips, so
/// this is the RGB box filter `resample` starts with, except in blocks where some colours clip.
/// `None` for other frames.
#[cfg(test)]
pub(crate) fn frame_to_image_scaled(f: &VideoFrame, w: u32, h: u32) -> Option<Image> {
    let (fw, fh) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 || fw % 2 != 0 || fh % 2 != 0 || (w as usize).saturating_mul(2) > fw || (h as usize).saturating_mul(2) > fh {
        return None;
    }
    let half = match &f.data {
        PixelData::Yuv8 { planes, chroma: Chroma::C420, alpha: None } => {
            half_420(fw, fh, [&planes[0][..], &planes[1][..], &planes[2][..]], &Yuv::new(f.color.matrix, f.color.range, 8))?
        }
        PixelData::Yuv16 { planes, chroma: Chroma::C420, bits, alpha: None } => {
            half_420(fw, fh, [&planes[0][..], &planes[1][..], &planes[2][..]], &Yuv::new(f.color.matrix, f.color.range, *bits))?
        }
        _ => return None,
    };
    Some(if (half.width, half.height) == (w, h) { half } else { effectcraft_raster::resample(&half, w, h) })
}

/// An opaque 4:2:0 frame of even size `w` × `h` converted at half that size (see
/// [`frame_to_image_scaled`]); `None` when the planes are too short.
#[cfg(test)]
fn half_420<T: Sample>(w: usize, h: usize, [luma, cb, cr]: [&[T]; 3], k: &Yuv) -> Option<Image> {
    /// Row `r` of a plane `n` samples wide (empty past its end).
    fn row_of<T>(p: &[T], r: usize, n: usize) -> &[T] {
        p.get(r.saturating_mul(n)..(r + 1).saturating_mul(n)).unwrap_or_default()
    }
    let (cw, ch) = (w / 2, h / 2);
    if luma.len() < w.checked_mul(h)? || cb.len() < cw * ch || cr.len() < cw * ch {
        return None;
    }
    let mut img = Image::new(cw as u32, ch as u32);
    img.data.par_chunks_mut(cw).enumerate().for_each_init(
        // Chroma rows with their edge samples repeated on both ends, for the horizontal [1 6 1].
        || (vec![0f32; cw + 2], vec![0f32; cw + 2]),
        |(uc, vc), (cy, row)| {
            // The chroma rows above and below weigh 1/8, as `centred` upsampling averages out.
            let rows = [cy.saturating_sub(1), cy, (cy + 1).min(ch - 1)];
            for (dst, plane) in [(&mut *uc, cb), (&mut *vc, cr)] {
                let [above, at, below] = rows.map(|r| row_of(plane, r, cw));
                for (d, ((a, b), c)) in dst[1..=cw].iter_mut().zip(above.iter().zip(at).zip(below)) {
                    *d = ((a.f() + c.f()) * 0.125 + b.f() * 0.75 - k.c_off) * k.c_scale;
                }
                (dst[0], dst[cw + 1]) = (dst[1], dst[cw]);
            }
            let (top, bottom) = (row_of(luma, 2 * cy, w), row_of(luma, 2 * cy + 1, w));
            let pixels = top.as_chunks::<2>().0.iter().zip(bottom.as_chunks::<2>().0).zip(uc.windows(3).zip(vc.windows(3)));
            for (o, ((a, b), (u, v))) in row.iter_mut().zip(pixels) {
                let (u, v) = ((u[0] + u[2]) * 0.125 + u[1] * 0.75, (v[0] + v[2]) * 0.125 + v[1] * 0.75);
                let yy = ((a[0].f() + a[1].f() + b[0].f() + b[1].f()) * 0.25 - k.y_off) * k.y_scale;
                *o = [(yy + k.cr_r * v).clamp(0.0, 1.0), (yy - k.cr_g * v - k.cb_g * u).clamp(0.0, 1.0), (yy + k.cb_b * u).clamp(0.0, 1.0), 1.0];
            }
        },
    );
    Some(img)
}

/// 0..255 → 0..1.
static U8: [f32; 256] = {
    let mut t = [0f32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as f32 / 255.0;
        i += 1;
    }
    t
};

/// Linear → sRGB-encoded (sign-preserving, unclamped above 1 for float footage).
#[inline]
fn encode(v: f32) -> f32 {
    if v < 0.0 {
        -encode(-v)
    } else if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Integer code values → normalised Y'CbCr and the matrix coefficients.
struct Yuv {
    y_scale: f32,
    y_off: f32,
    c_scale: f32,
    c_off: f32,
    cr_r: f32,
    cr_g: f32,
    cb_g: f32,
    cb_b: f32,
}

impl Yuv {
    fn new(m: Matrix, range: Range, bits: u32) -> Self {
        let s = (1u32 << (bits - 8)) as f32;
        let (y_scale, y_off, c_scale, c_off) = match range {
            Range::Limited => (1.0 / (219.0 * s), 16.0 * s, 1.0 / (224.0 * s), 128.0 * s),
            Range::Full => {
                let max = ((1u32 << bits) - 1) as f32;
                (1.0 / max, 0.0, 1.0 / max, (1u32 << (bits - 1)) as f32)
            }
        };
        let (kr, kb) = m.kr_kb();
        let kg = 1.0 - kr - kb;
        let cr_r = 2.0 * (1.0 - kr);
        let cb_b = 2.0 * (1.0 - kb);
        Self { y_scale, y_off, c_scale, c_off, cr_r, cr_g: cr_r * kr / kg, cb_g: cb_b * kb / kg, cb_b }
    }
}

trait Sample: Copy + Send + Sync {
    fn f(self) -> f32;
}
impl Sample for u8 {
    #[inline(always)]
    fn f(self) -> f32 {
        self as f32
    }
}
impl Sample for u16 {
    #[inline(always)]
    fn f(self) -> f32 {
        self as f32
    }
}

/// Centred chroma siting (as ffmpeg's swscale and JPEG): for a subsampled axis, full-resolution
/// sample `i` lies ¼ of a chroma sample from chroma sample `i/2` towards its neighbour on that
/// side. Returns (nearest chroma index, neighbour index); the weights are ¾ and ¼.
#[inline(always)]
fn centred(i: usize, n: usize) -> (usize, usize) {
    let c = (i >> 1).min(n - 1);
    let o = if i & 1 == 0 { c.saturating_sub(1) } else { (c + 1).min(n - 1) };
    (c, o)
}

/// Rows `y0..` (as many whole rows as `out` holds) of a planar Y'CbCr picture `w`×`h`.
#[allow(clippy::too_many_arguments)]
fn yuv_rows<T: Sample>(w: usize, h: usize, chroma: Chroma, planes: [&[T]; 3], alpha: Option<&[T]>, amax: f32, k: &Yuv, op: AlphaOp, y0: usize, out: &mut [Px]) {
    let (sx, sy) = chroma.shifts();
    let cw = w.div_ceil(1 << sx);
    let ch = h.div_ceil(1 << sy);
    if cw == 0 || ch == 0 {
        return;
    }
    let inv_amax = 1.0 / amax;
    let (mut uc, mut vc, mut uf, mut vf) = (vec![0f32; cw], vec![0f32; cw], vec![0f32; w], vec![0f32; w]);
    for (r, row) in out.chunks_mut(w).enumerate() {
        let y = y0 + r;
        if y >= h {
            break;
        }
        // vertical chroma interpolation into normalised rows (Cb, Cr in −0.5..0.5)
        let (r0, r1, t) = if sy == 1 {
            let (c, o) = centred(y, ch);
            (c, o, 0.25)
        } else {
            (y, y, 0.0)
        };
        for (dst, plane) in [(&mut uc, planes[1]), (&mut vc, planes[2])] {
            let (Some(a), Some(b)) = (plane.get(r0 * cw..r0 * cw + cw), plane.get(r1 * cw..r1 * cw + cw)) else { continue };
            for ((d, a), b) in dst.iter_mut().zip(a).zip(b) {
                let v = a.f() * (1.0 - t) + b.f() * t;
                *d = (v - k.c_off) * k.c_scale;
            }
        }
        // horizontal
        if sx == 1 {
            for x in 0..w {
                let (c, o) = centred(x, cw);
                uf[x] = uc[c] * 0.75 + uc[o] * 0.25;
                vf[x] = vc[c] * 0.75 + vc[o] * 0.25;
            }
        } else {
            uf.copy_from_slice(&uc[..w]);
            vf.copy_from_slice(&vc[..w]);
        }
        let Some(yrow) = planes[0].get(y * w..y * w + w) else { continue };
        let rgb = |x: usize| {
            let (u, v) = (uf[x], vf[x]);
            let yy = (yrow[x].f() - k.y_off) * k.y_scale;
            [(yy + k.cr_r * v).clamp(0.0, 1.0), (yy - k.cr_g * v - k.cb_g * u).clamp(0.0, 1.0), (yy + k.cb_b * u).clamp(0.0, 1.0)]
        };
        match alpha.and_then(|al| al.get(y * w..y * w + w)) {
            // opaque: every alpha interpretation gives the colour itself
            None => {
                for (x, o) in row.iter_mut().enumerate() {
                    let c = rgb(x);
                    *o = [c[0], c[1], c[2], 1.0];
                }
            }
            Some(arow) => {
                for (x, o) in row.iter_mut().enumerate() {
                    *o = op.apply(rgb(x), (arow[x].f() * inv_amax).clamp(0.0, 1.0));
                }
            }
        }
    }
}

/// Convert a still decoded by the `image` crate. Float images (OpenEXR, float TIFF) hold linear
/// light and are sRGB-encoded here (values above 1 are kept), unless the footage preserves RGB.
pub(crate) fn dynamic_to_image(img: &image::DynamicImage, op: AlphaOp) -> Image {
    use image::DynamicImage as D;
    let (w, h) = (img.width(), img.height());
    let mut out = Image::new(w, h);
    let wu = w as usize;
    if wu == 0 || h == 0 {
        return out;
    }
    match img {
        D::ImageRgb32F(_) | D::ImageRgba32F(_) => {
            let px = img.to_rgba32f();
            let raw = px.as_raw();
            let premul = op.mode == AlphaMode::Premultiplied;
            out.data.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
                for (x, o) in row.iter_mut().enumerate() {
                    let s = &raw[(y * wu + x) * 4..(y * wu + x) * 4 + 4];
                    let a = s[3];
                    let c = if premul {
                        let inv = if a > 0.0 { 1.0 / a } else { 0.0 };
                        [op.float(s[0] * inv) * a, op.float(s[1] * inv) * a, op.float(s[2] * inv) * a]
                    } else {
                        [op.float(s[0]), op.float(s[1]), op.float(s[2])]
                    };
                    *o = op.apply(c, a.clamp(0.0, 1.0));
                }
            });
        }
        D::ImageLuma16(_) | D::ImageLumaA16(_) | D::ImageRgb16(_) | D::ImageRgba16(_) => {
            let px = img.to_rgba16();
            let raw = px.as_raw();
            let k = 1.0 / 65535.0;
            out.data.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
                for (x, o) in row.iter_mut().enumerate() {
                    let s = &raw[(y * wu + x) * 4..(y * wu + x) * 4 + 4];
                    *o = op.apply([s[0] as f32 * k, s[1] as f32 * k, s[2] as f32 * k], s[3] as f32 * k);
                }
            });
        }
        _ => {
            let px = img.to_rgba8();
            let raw = px.as_raw();
            out.data.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
                for (x, o) in row.iter_mut().enumerate() {
                    let s = &raw[(y * wu + x) * 4..(y * wu + x) * 4 + 4];
                    *o = op.apply([U8[s[0] as usize], U8[s[1] as usize], U8[s[2] as usize]], U8[s[3] as usize]);
                }
            });
        }
    }
    out
}

/// Straight RGBA `f32` pixels (layered stills: Photoshop, SVG) to an [`Image`].
pub(crate) fn straight_to_image(w: u32, h: u32, px: &[[f32; 4]], op: AlphaOp) -> Image {
    let mut out = Image::new(w, h);
    let wu = w as usize;
    if wu == 0 || h == 0 {
        return out;
    }
    out.data.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let s = px[y * wu + x];
            *o = op.apply([s[0], s[1], s[2]], s[3].clamp(0.0, 1.0));
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_render::FramePixels;
    use std::sync::Arc;

    fn yuv_frame(y: u8, u: u8, v: u8, color: filmcraft_color::ColorInfo) -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 2,
            data: PixelData::Yuv8 { planes: [Arc::new(vec![y; 8]), Arc::new(vec![u; 2]), Arc::new(vec![v; 2])], chroma: Chroma::C420, alpha: None },
            color,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        }
    }

    #[test]
    fn limited_range_grey_and_white() {
        let op = AlphaOp::new(AlphaMode::Straight, [0.0; 3]);
        let img = frame_to_image(&yuv_frame(235, 128, 128, filmcraft_color::ColorInfo::REC709), op);
        assert!(img.data.iter().all(|p| (p[0] - 1.0).abs() < 1e-5 && (p[1] - 1.0).abs() < 1e-5 && p[3] == 1.0));
        let img = frame_to_image(&yuv_frame(126, 128, 128, filmcraft_color::ColorInfo::REC709), op);
        assert!((img.data[0][0] - 110.0 / 219.0).abs() < 1e-5);
    }

    /// A smooth opaque 4:2:0 frame whose colours never clip.
    fn smooth_420(w: usize, h: usize) -> VideoFrame {
        let plane = |w: usize, h: usize, base: f32, amp: f32, fx: f32, fy: f32| -> Arc<Vec<u8>> {
            Arc::new((0..w * h).map(|i| (base + amp * ((i % w) as f32 * fx + (i / w) as f32 * fy).sin()) as u8).collect())
        };
        VideoFrame {
            width: w as u32,
            height: h as u32,
            data: PixelData::Yuv8 {
                planes: [plane(w, h, 125.0, 45.0, 0.31, 0.17), plane(w / 2, h / 2, 128.0, 10.0, 0.4, -0.3), plane(w / 2, h / 2, 128.0, 10.0, -0.2, 0.5)],
                chroma: Chroma::C420,
                alpha: None,
            },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        }
    }

    #[test]
    fn scaled_conversion_matches_resampling_the_converted_frame() {
        let f = smooth_420(96, 64);
        let full = frame_to_image(&f, AlphaOp::new(AlphaMode::Straight, [0.0; 3]));
        for (w, h) in [(48, 32), (32, 21), (24, 16), (13, 9)] {
            let scaled = frame_to_image_scaled(&f, w, h).expect("4:2:0, at most half size");
            let want = effectcraft_raster::resample(&full, w, h);
            assert_eq!((scaled.width, scaled.height), (w, h));
            let worst = scaled.data.iter().zip(&want.data).flat_map(|(a, b)| (0..4).map(move |c| (a[c] - b[c]).abs())).fold(0.0f32, f32::max);
            assert!(worst < 1e-5, "{w}×{h}: {worst}");
        }
        // More than half the size, odd sizes, other chroma layouts: convert the full frame.
        assert!(frame_to_image_scaled(&f, 49, 32).is_none());
        assert!(frame_to_image_scaled(&smooth_420(95, 64), 40, 30).is_none());
        assert!(frame_to_image_scaled(&yuv_frame(100, 128, 128, filmcraft_color::ColorInfo::REC709), 2, 1).is_some());
        let mut f444 = smooth_420(96, 64);
        if let PixelData::Yuv8 { chroma, .. } = &mut f444.data {
            *chroma = Chroma::C444;
        }
        assert!(frame_to_image_scaled(&f444, 48, 32).is_none());
    }

    #[test]
    fn bt709_red() {
        // 8-bit limited BT.709 red: Y 63, Cb 102, Cr 240
        let op = AlphaOp::new(AlphaMode::Straight, [0.0; 3]);
        let p = frame_to_image(&yuv_frame(63, 102, 240, filmcraft_color::ColorInfo::REC709), op).data[0];
        assert!(p[0] > 0.98 && p[1] < 0.02 && p[2] < 0.02, "{p:?}");
    }

    #[test]
    fn alpha_modes() {
        let c = [0.8, 0.4, 0.2];
        let s = AlphaOp::new(AlphaMode::Straight, [0.0; 3]).apply(c, 0.5);
        assert_eq!(s, [0.4, 0.2, 0.1, 0.5]);
        let p = AlphaOp::new(AlphaMode::Premultiplied, [0.0; 3]).apply([0.4, 0.2, 0.1], 0.5);
        assert_eq!(p, [0.4, 0.2, 0.1, 0.5]);
        // premultiplied against white: c = s·a + 1·(1−a)
        let p = AlphaOp::new(AlphaMode::Premultiplied, [1.0; 3]).apply([0.9, 0.7, 0.6], 0.5);
        assert!((p[0] - 0.4).abs() < 1e-6 && (p[2] - 0.1).abs() < 1e-6);
        let i = AlphaOp::new(AlphaMode::Ignore, [0.0; 3]).apply(c, 0.0);
        assert_eq!(i, [0.8, 0.4, 0.2, 1.0]);
    }

    #[test]
    fn float_stills_are_encoded() {
        let img = image::DynamicImage::ImageRgba32F(image::ImageBuffer::from_raw(1, 1, vec![0.2140, 1.0, 4.0, 1.0]).expect("buf"));
        let p = dynamic_to_image(&img, AlphaOp::new(AlphaMode::Premultiplied, [0.0; 3])).data[0];
        assert!((p[0] - 0.5).abs() < 1e-3 && (p[1] - 1.0).abs() < 1e-5 && p[2] > 1.5, "{p:?}");
    }

    /// The conversion as it was before frames were kept packed and converted a band at a time
    /// (verbatim): the packed paths must give exactly its pixels.
    mod reference {
        use super::super::*;

        pub(super) fn frame_to_image_in(f: &VideoFrame, op: AlphaOp, mut buf: Vec<Px>) -> Image {
            let (w, h) = (f.width as usize, f.height as usize);
            if buf.len() != w * h {
                buf = vec![[0.0; 4]; w * h];
            }
            let mut img = Image { width: f.width, height: f.height, data: buf };
            if w == 0 || h == 0 {
                return img;
            }
            match &f.data {
                PixelData::Rgba8(d) => {
                    img.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                        let src = &d[y * w * 4..(y + 1) * w * 4];
                        for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                            *o = op.apply([U8[s[0] as usize], U8[s[1] as usize], U8[s[2] as usize]], U8[s[3] as usize]);
                        }
                    });
                }
                PixelData::RgbaF32(d) => {
                    // linear premultiplied → encoded premultiplied
                    img.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                        let src = &d[y * w * 4..(y + 1) * w * 4];
                        for (o, s) in row.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                            let a = s[3];
                            let inv = if a > 0.0 { 1.0 / a } else { 0.0 };
                            let c = [op.float(s[0] * inv) * a, op.float(s[1] * inv) * a, op.float(s[2] * inv) * a];
                            *o = if op.mode == AlphaMode::Ignore { [c[0], c[1], c[2], 1.0] } else { [c[0], c[1], c[2], a] };
                        }
                    });
                }
                PixelData::Yuv8 { planes, chroma, alpha } => {
                    let yuv = Yuv::new(f.color.matrix, f.color.range, 8);
                    let amax = 255.0;
                    convert_yuv(&mut img, w, h, *chroma, [&planes[0][..], &planes[1][..], &planes[2][..]], alpha.as_deref().map(|a| &a[..]), amax, &yuv, op);
                }
                PixelData::Yuv16 { planes, chroma, bits, alpha } => {
                    let yuv = Yuv::new(f.color.matrix, f.color.range, *bits);
                    let amax = ((1u32 << *bits) - 1) as f32;
                    convert_yuv(&mut img, w, h, *chroma, [&planes[0][..], &planes[1][..], &planes[2][..]], alpha.as_deref().map(|a| &a[..]), amax, &yuv, op);
                }
            }
            img
        }

        #[allow(clippy::too_many_arguments)]
        fn convert_yuv<T: Sample>(
            img: &mut Image,
            w: usize,
            h: usize,
            chroma: Chroma,
            planes: [&[T]; 3],
            alpha: Option<&[T]>,
            amax: f32,
            k: &Yuv,
            op: AlphaOp,
        ) {
            let (sx, sy) = chroma.shifts();
            let cw = w.div_ceil(1 << sx);
            let ch = h.div_ceil(1 << sy);
            let inv_amax = 1.0 / amax;
            img.data.par_chunks_mut(w).enumerate().for_each_init(
                || (vec![0f32; cw], vec![0f32; cw], vec![0f32; w], vec![0f32; w]),
                |(uc, vc, uf, vf), (y, row)| {
                    // vertical chroma interpolation into normalised rows (Cb, Cr in −0.5..0.5)
                    let (r0, r1, t) = if sy == 1 {
                        let (c, o) = centred(y, ch);
                        (c, o, 0.25)
                    } else {
                        (y, y, 0.0)
                    };
                    for (dst, plane) in [(&mut *uc, planes[1]), (&mut *vc, planes[2])] {
                        let a = &plane[r0 * cw..r0 * cw + cw];
                        let b = &plane[r1 * cw..r1 * cw + cw];
                        for ((d, a), b) in dst.iter_mut().zip(a).zip(b) {
                            let v = a.f() * (1.0 - t) + b.f() * t;
                            *d = (v - k.c_off) * k.c_scale;
                        }
                    }
                    // horizontal
                    if sx == 1 {
                        for x in 0..w {
                            let (c, o) = centred(x, cw);
                            uf[x] = uc[c] * 0.75 + uc[o] * 0.25;
                            vf[x] = vc[c] * 0.75 + vc[o] * 0.25;
                        }
                    } else {
                        uf.copy_from_slice(&uc[..w]);
                        vf.copy_from_slice(&vc[..w]);
                    }
                    let yrow = &planes[0][y * w..y * w + w];
                    let rgb = |x: usize| {
                        let (u, v) = (uf[x], vf[x]);
                        let yy = (yrow[x].f() - k.y_off) * k.y_scale;
                        [(yy + k.cr_r * v).clamp(0.0, 1.0), (yy - k.cr_g * v - k.cb_g * u).clamp(0.0, 1.0), (yy + k.cb_b * u).clamp(0.0, 1.0)]
                    };
                    match alpha {
                        // opaque: every alpha interpretation gives the colour itself
                        None => {
                            for (x, o) in row.iter_mut().enumerate() {
                                let c = rgb(x);
                                *o = [c[0], c[1], c[2], 1.0];
                            }
                        }
                        Some(al) => {
                            let arow = &al[y * w..y * w + w];
                            for (x, o) in row.iter_mut().enumerate() {
                                *o = op.apply(rgb(x), (arow[x].f() * inv_amax).clamp(0.0, 1.0));
                            }
                        }
                    }
                },
            );
        }
    }

    fn frame(w: u32, h: u32, data: PixelData, color: filmcraft_color::ColorInfo) -> VideoFrame {
        VideoFrame { width: w, height: h, data, color, par: (1, 1), pts: filmcraft_time::Tick::ZERO }
    }

    /// FC1: 12-bit Y'CbCr 4:4:4 with alpha rounded to 8 bits keeps the video range (16..235 at
    /// 8 bits is 256..3760 at 12), maps full-range alpha 0..4095 onto 0..255, halves the bytes,
    /// and converts within an 8-bit step of the 12-bit frame. 8-bit and float frames are kept.
    #[test]
    fn twelve_bit_yuv_rounds_to_eight_bits() {
        let rec709 = filmcraft_color::ColorInfo::REC709;
        let y: Vec<u16> = vec![256, 3760, 2000, 2009, 1000, 3000];
        let cb: Vec<u16> = vec![2048, 256, 3840, 2056, 1500, 2600];
        let cr: Vec<u16> = vec![2048, 3840, 256, 2040, 2600, 1500];
        let a: Vec<u16> = vec![0, 4095, 2048, 1, 4094, 3000];
        let f = frame(
            3,
            2,
            PixelData::Yuv16 { planes: [Arc::new(y), Arc::new(cb), Arc::new(cr)], chroma: Chroma::C444, bits: 12, alpha: Some(Arc::new(a)) },
            rec709,
        );
        let e = frame_to_8bit(&f).expect("12-bit converts");
        let PixelData::Yuv8 { planes, chroma, alpha } = &e.data else { panic!("8-bit samples") };
        assert_eq!(*chroma, Chroma::C444);
        assert_eq!(&planes[0][..], &[16, 235, 125, 126, 63, 188]);
        assert_eq!(&planes[1][..3], &[128, 16, 240]);
        assert_eq!(&alpha.as_ref().unwrap()[..], &[0, 255, 128, 0, 255, 187]);
        let (p16, p8) = (
            Packed::Video { frame: Arc::new(f.clone()), op: AlphaOp::new(AlphaMode::Straight, [0.0; 3]) },
            Packed::Video { frame: Arc::new(e.clone()), op: AlphaOp::new(AlphaMode::Straight, [0.0; 3]) },
        );
        assert_eq!(p8.bytes() * 2, p16.bytes(), "half the memory");
        let (a, b) = (frame_to_image(&e, AlphaOp::new(AlphaMode::Straight, [0.0; 3])), frame_to_image(&f, AlphaOp::new(AlphaMode::Straight, [0.0; 3])));
        for (p, q) in a.data.iter().zip(&b.data) {
            for (x, w) in p.iter().zip(q) {
                assert!((x - w).abs() <= 2.5 / 255.0, "{p:?} vs {q:?}");
            }
        }
        // Already 8-bit: nothing to do.
        assert!(frame_to_8bit(&e).is_none());
        assert!(p8.to_8bit().is_none());
        // 16-bit stills: 8-bit RGBA.
        let s16 = Packed::Rgba16 { width: 1, height: 1, data: vec![0, 65535, 32768, 257], op: AlphaOp::new(AlphaMode::Straight, [0.0; 3]) };
        let Some(Packed::Rgba8 { data, .. }) = s16.to_8bit() else { panic!("8-bit RGBA") };
        assert_eq!(data, vec![0, 255, 128, 1]);
    }

    fn noise8(n: usize, seed: u32) -> Arc<Vec<u8>> {
        Arc::new((0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect())
    }

    fn noise16(n: usize, seed: u32, bits: u32) -> Arc<Vec<u16>> {
        Arc::new((0..n as u32).map(|i| ((i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 7) & ((1 << bits) - 1)) as u16).collect())
    }

    fn assert_bit_exact(a: &Image, b: &Image, what: &str) {
        assert_eq!((a.width, a.height), (b.width, b.height), "{what}");
        for (i, (p, q)) in a.data.iter().zip(&b.data).enumerate() {
            assert!(p.iter().zip(q).all(|(x, y)| x.to_bits() == y.to_bits()), "{what}: pixel {i}: {p:?} vs {q:?}");
        }
    }

    /// Frames kept as the decoder's samples convert to exactly the f32 pixels the cache held
    /// before (every layout, odd sizes, alpha interpretations, limited and full range).
    #[test]
    fn packed_frames_expand_bit_exact() {
        let ops = [
            AlphaOp::new(AlphaMode::Straight, [0.0; 3]),
            AlphaOp::new(AlphaMode::Premultiplied, [1.0, 0.5, 0.0]),
            AlphaOp::new(AlphaMode::Ignore, [0.0; 3]),
            AlphaOp { float: FloatCurve::Linear, ..AlphaOp::new(AlphaMode::Premultiplied, [0.0; 3]) },
        ];
        let full = filmcraft_color::ColorInfo { range: Range::Full, ..filmcraft_color::ColorInfo::REC709 };
        let mut frames = Vec::new();
        for (w, h) in [(37u32, 21u32), (64, 18), (1, 1), (2, 9)] {
            let n = (w * h) as usize;
            for (chroma, (cw, ch)) in [(Chroma::C420, (w.div_ceil(2), h.div_ceil(2))), (Chroma::C422, (w.div_ceil(2), h)), (Chroma::C444, (w, h))] {
                let cn = (cw * ch) as usize;
                for (alpha, color) in [(None, filmcraft_color::ColorInfo::REC709), (Some(noise8(n, 9)), full)] {
                    frames.push(frame(w, h, PixelData::Yuv8 { planes: [noise8(n, 1), noise8(cn, 2), noise8(cn, 3)], chroma, alpha }, color));
                }
                for bits in [10u32, 12] {
                    let alpha = (bits == 10).then(|| noise16(n, 9, bits));
                    frames.push(frame(
                        w,
                        h,
                        PixelData::Yuv16 { planes: [noise16(n, 1, bits), noise16(cn, 2, bits), noise16(cn, 3, bits)], chroma, bits, alpha },
                        full,
                    ));
                }
            }
            frames.push(frame(w, h, PixelData::Rgba8(noise8(n * 4, 5)), full));
            let f: Vec<f32> = noise8(n * 4, 6).iter().map(|v| *v as f32 / 97.0).collect();
            frames.push(frame(w, h, PixelData::RgbaF32(Arc::new(f)), full));
        }
        for f in frames {
            let f = Arc::new(f);
            for op in ops {
                let old = reference::frame_to_image_in(&f, op, Vec::new());
                assert_bit_exact(&frame_to_image(&f, op), &old, "frame_to_image");
                let packed = Packed::Video { frame: f.clone(), op };
                assert_bit_exact(&packed.to_image(), &old, "packed");
                if let Some(view) = packed.yuv() {
                    assert_eq!((view.width, view.height), packed.size());
                    assert!(view.validate().is_ok());
                    assert_eq!(view.alpha_mode, op.mode);
                    assert_eq!(view.matte, op.matte);
                } else {
                    assert!(matches!(f.data, PixelData::Rgba8(_) | PixelData::RgbaF32(_)));
                }
                // a band at a time, as a renderer expands it
                let mut band = vec![[0.0; 4]; f.width as usize * 3];
                for y0 in (0..f.height as usize).step_by(3) {
                    let rows = (f.height as usize - y0).min(3);
                    packed.rows(y0, &mut band[..rows * f.width as usize]);
                    let want = &old.data[y0 * f.width as usize..(y0 + rows) * f.width as usize];
                    assert!(band[..want.len()].iter().zip(want).all(|(p, q)| p.map(f32::to_bits) == q.map(f32::to_bits)), "band at {y0}");
                }
            }
        }
    }

    /// A hardware picture kept in its surface, as a test double: the planes it copies out, and
    /// how many times it was asked.
    struct Held {
        planes: crate::hw::Planes16,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl crate::hw::ResidentFrame for Held {
        fn planes(&self, reuse: crate::hw::Planes16) -> std::result::Result<crate::hw::Planes16, String> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            // (writes into the buffers it is given when they fit, as the platform's copy does)
            let fill = |mut v: Vec<u16>, src: &[u16]| {
                if v.len() < src.len() {
                    v = vec![0; src.len()];
                }
                v.truncate(src.len());
                v.copy_from_slice(src);
                v
            };
            Ok(crate::hw::Planes16 {
                y: fill(reuse.y, &self.planes.y),
                cb: fill(reuse.cb, &self.planes.cb),
                cr: fill(reuse.cr, &self.planes.cr),
                alpha: self.planes.alpha.as_ref().map(|a| fill(reuse.alpha.unwrap_or_default(), a)),
            })
        }
        fn bytes(&self) -> usize {
            1234
        }
    }

    /// Fix 3: a frame kept in its decoder's surface gives the GPU a view without samples (keyed
    /// by its stand-in's luma allocation), at full depth or 8 bits; a CPU reader gets exactly the
    /// pixels of the frame copied at decode (and its 8-bit rounding), copied out once; the copy is
    /// counted while it lives and its buffers come back when it goes.
    #[test]
    fn frames_kept_on_the_gpu_are_copied_once_and_match_the_copied_frame() {
        let (w, h) = (37u32, 21u32);
        let n = (w * h) as usize;
        for (chroma, cn, alpha) in [(Chroma::C444, n, true), (Chroma::C422, (w.div_ceil(2) * h) as usize, false)] {
            let bits = if chroma == Chroma::C444 { 12 } else { 10 };
            let planes = crate::hw::Planes16 {
                y: noise16(n, 1, bits).to_vec(),
                cb: noise16(cn, 2, bits).to_vec(),
                cr: noise16(cn, 3, bits).to_vec(),
                alpha: alpha.then(|| noise16(n, 4, bits).to_vec()),
            };
            let copied = Arc::new(frame(
                w,
                h,
                PixelData::Yuv16 {
                    planes: [Arc::new(planes.y.clone()), Arc::new(planes.cb.clone()), Arc::new(planes.cr.clone())],
                    chroma,
                    bits,
                    alpha: planes.alpha.clone().map(Arc::new),
                },
                filmcraft_color::ColorInfo::REC709,
            ));
            // the decoder's stand-in: a short luma plane, zero planes for the rest
            let stand_in = Arc::new(frame(
                w,
                h,
                PixelData::Yuv16 {
                    planes: [Arc::new(Vec::with_capacity(1)), Arc::new(vec![0; n + cn]), Arc::new(vec![0; cn])],
                    chroma,
                    bits,
                    alpha: alpha.then(|| Arc::new(vec![0; n])),
                },
                filmcraft_color::ColorInfo::REC709,
            ));
            assert!(crate::hw::is_stand_in(&stand_in) && !crate::hw::is_stand_in(&copied));
            let held = Arc::new(Held { planes, asked: Default::default() });
            let meter = Arc::new(LazyMeter::default());
            let op = AlphaOp::new(AlphaMode::Straight, [0.0; 3]);
            let lazy = Packed::Resident(Box::new(Lazy::new(stand_in.clone(), held.clone(), op, meter.clone(), Arc::default())));
            let video = Packed::Video { frame: copied.clone(), op };
            assert_eq!(lazy.bytes(), 1234, "the cache counts the surface");
            assert_eq!(lazy.size(), (w, h));
            // the GPU: no samples, the stand-in's key, the same layout and coefficients
            let g = effectcraft_render::FramePixels::gpu_yuv(&lazy).expect("a GPU view");
            let v = effectcraft_render::FramePixels::yuv(&video).expect("the copied frame's view");
            let PixelData::Yuv16 { planes: sp, .. } = &stand_in.data else { panic!("a planar stand-in") };
            assert_eq!(g.surface, Some(sp[0].as_ptr() as usize));
            assert!(g.planes.iter().all(|p| p.samples.is_empty()) && g.validate() == Ok(0));
            assert_eq!(
                (g.bits, g.subsampling, g.alpha.is_some(), g.coefficients.luma, g.coefficients.chroma),
                (v.bits, v.subsampling, v.alpha.is_some(), v.coefficients.luma, v.coefficients.chroma)
            );
            assert_eq!(held.asked.load(Ordering::Relaxed), 0, "nothing copied for the GPU");
            // a CPU reader: the same pixels, copied once
            assert_bit_exact(&lazy.to_image(), &video.to_image(), "CPU pixels");
            let mut band = vec![[0.0; 4]; w as usize * 2];
            lazy.rows(4, &mut band);
            assert_eq!(held.asked.load(Ordering::Relaxed), 1, "copied once");
            let Packed::Resident(l) = &lazy else { panic!("kept on the GPU") };
            assert!(l.made());
            assert_eq!(meter.bytes.load(Ordering::Relaxed), copied.byte_size(), "the copy is counted");
            assert_eq!(meter.copies.load(Ordering::Relaxed), 1);
            let cpu_view = effectcraft_render::FramePixels::yuv(&lazy).expect("CPU view");
            assert!(cpu_view.surface.is_none() && cpu_view.validate().is_ok());
            // Decode Footage at 8 Bits: the GPU rounds (an 8-bit view of the same surface), a
            // CPU reader gets exactly frame_to_8bit's samples
            let eight = lazy.to_8bit().expect("8-bit");
            let eight_video = video.to_8bit().expect("8-bit copied frame");
            let g8 = effectcraft_render::FramePixels::gpu_yuv(&eight).expect("8-bit GPU view");
            let v8 = effectcraft_render::FramePixels::yuv(&eight_video).expect("8-bit view");
            assert_eq!(g8.surface, g.surface, "the same surface");
            assert!(matches!(g8.planes[0].samples, effectcraft_render::yuv::Samples::U8(_)));
            assert_eq!((g8.bits, g8.coefficients.luma, g8.coefficients.chroma), (8, v8.coefficients.luma, v8.coefficients.chroma));
            assert!(eight.to_8bit().is_none(), "8-bit already");
            assert_eq!(held.asked.load(Ordering::Relaxed), 1, "rounding for the GPU copies nothing");
            assert_bit_exact(&eight.to_image(), &eight_video.to_image(), "8-bit CPU pixels");
            assert_eq!(held.asked.load(Ordering::Relaxed), 2);
            assert_eq!(meter.bytes.load(Ordering::Relaxed), copied.byte_size() + copied.byte_size() / 2, "both copies counted");
            drop((lazy, eight));
            assert_eq!(meter.bytes.load(Ordering::Relaxed), 0, "copies gone with their frames");
        }
    }

    /// The 16-bit limited-range rounding and the constant-depth alpha scaling give the 32-bit
    /// formulas' values for every sample at every depth.
    #[test]
    fn eight_bit_rounding_formulas_agree_for_every_sample() {
        for bits in 9u32..=16 {
            let max = (1u32 << bits) - 1;
            let shift = bits - 8;
            let half = (1u32 << shift) >> 1;
            let (max16, half16) = (max as u16, half as u16);
            for v in 0..=u16::MAX {
                let old = ((u32::from(v).min(max) + half) >> shift).min(255) as u8;
                assert_eq!(v.min(max16).saturating_add(half16).wrapping_shr(shift).min(255) as u8, old, "{bits} bits, {v}");
                let full = ((u32::from(v).min(max) * 255 + max / 2) / max).min(255) as u8;
                match bits {
                    10 => assert_eq!(full_at::<1023>(v), full),
                    12 => assert_eq!(full_at::<4095>(v), full),
                    _ => {}
                }
            }
        }
    }

    /// The 8-bit rounding of CPU copies made on demand (on the calling thread) gives what the rayon one gives.
    #[test]
    fn eight_bit_rounding_on_this_thread_matches() {
        for (n, range) in [(5usize, Range::Limited), ((1 << 20) + 77, Range::Limited), ((1 << 20) + 3, Range::Full)] {
            let f = frame(
                n as u32,
                1,
                PixelData::Yuv16 {
                    planes: [noise16(n, 1, 12), noise16(n, 2, 12), noise16(n, 3, 12)],
                    chroma: Chroma::C444,
                    bits: 12,
                    alpha: Some(noise16(n, 4, 12)),
                },
                filmcraft_color::ColorInfo { range, ..filmcraft_color::ColorInfo::REC709 },
            );
            let (a, b) = (frame_to_8bit_on(&f, Bands::Rayon).expect("8-bit"), frame_to_8bit_on(&f, Bands::Here).expect("8-bit"));
            let (PixelData::Yuv8 { planes: pa, alpha: aa, .. }, PixelData::Yuv8 { planes: pb, alpha: ab, .. }) = (&a.data, &b.data) else { panic!("8-bit") };
            assert_eq!(pa, pb, "{n} samples, {range:?}");
            assert_eq!(aa, ab);
        }
    }

    /// `cargo test -p effectcraft-media --lib bench_eight_bit -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_eight_bit_rounding() {
        let n = 1920 * 1080;
        let f = frame(
            1920,
            1080,
            PixelData::Yuv16 {
                planes: [noise16(n, 1, 12), noise16(n, 2, 12), noise16(n, 3, 12)],
                chroma: Chroma::C444,
                bits: 12,
                alpha: Some(noise16(n, 4, 12)),
            },
            filmcraft_color::ColorInfo::REC709,
        );
        for _ in 0..3 {
            for bands in [Bands::Rayon, Bands::Here] {
                let t = std::time::Instant::now();
                for _ in 0..20 {
                    let e = frame_to_8bit_on(&f, bands).expect("8-bit");
                    if bands == Bands::Here {
                        crate::recycle::frame(e);
                    }
                }
                eprintln!("{} {:?}", if bands == Bands::Rayon { "rayon" } else { "here " }, t.elapsed() / 20);
            }
        }
    }

    /// Stills kept as 8 / 16-bit RGBA convert to exactly the f32 pixels decoded before.
    #[test]
    fn packed_stills_expand_bit_exact() {
        use image::DynamicImage as D;
        let (w, h) = (13u32, 7u32);
        let px = |c: usize| noise8((w * h) as usize * c, c as u32).to_vec();
        let px16 = |c: usize| noise16((w * h) as usize * c, c as u32, 16).to_vec();
        let stills = [
            D::ImageRgba8(image::ImageBuffer::from_raw(w, h, px(4)).expect("rgba8")),
            D::ImageRgb8(image::ImageBuffer::from_raw(w, h, px(3)).expect("rgb8")),
            D::ImageLuma8(image::ImageBuffer::from_raw(w, h, px(1)).expect("l8")),
            D::ImageLumaA8(image::ImageBuffer::from_raw(w, h, px(2)).expect("la8")),
            D::ImageRgba16(image::ImageBuffer::from_raw(w, h, px16(4)).expect("rgba16")),
            D::ImageRgb16(image::ImageBuffer::from_raw(w, h, px16(3)).expect("rgb16")),
            D::ImageLuma16(image::ImageBuffer::from_raw(w, h, px16(1)).expect("l16")),
        ];
        for img in stills {
            for op in [
                AlphaOp::new(AlphaMode::Straight, [0.0; 3]),
                AlphaOp::new(AlphaMode::Premultiplied, [0.2, 0.4, 0.6]),
                AlphaOp::new(AlphaMode::Ignore, [0.0; 3]),
            ] {
                let old = dynamic_to_image(&img, op);
                match pack_dynamic(img.clone(), op) {
                    Err(packed) => {
                        assert!(packed.bytes() <= (w * h) as usize * 8);
                        assert_bit_exact(&packed.to_image(), &old, "still");
                    }
                    Ok(_) => panic!("8 / 16-bit stills are kept packed"),
                }
            }
        }
        let float = D::ImageRgba32F(image::ImageBuffer::from_raw(1, 1, vec![0.2, 1.0, 4.0, 1.0]).expect("f32"));
        assert!(pack_dynamic(float, AlphaOp::new(AlphaMode::Straight, [0.0; 3])).is_ok(), "float stills stay float");
    }

    /// `cargo test -p effectcraft-media --lib bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_convert_1080p() {
        let (w, h) = (1920usize, 1080usize);
        let f = VideoFrame {
            width: w as u32,
            height: h as u32,
            data: PixelData::Yuv8 {
                planes: [Arc::new((0..w * h).map(|i| (i % 251) as u8).collect()), Arc::new(vec![100; w * h / 4]), Arc::new(vec![150; w * h / 4])],
                chroma: Chroma::C420,
                alpha: None,
            },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        };
        let op = AlphaOp::new(AlphaMode::Straight, [0.0; 3]);
        for _ in 0..3 {
            let t = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(Image::new(1920, 1080));
            }
            eprintln!("alloc {:?}", t.elapsed() / 20);
            let t = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(frame_to_image(&f, op));
            }
            eprintln!("convert {:?}", t.elapsed() / 20);
            let mut keep = Vec::new();
            let t = std::time::Instant::now();
            for _ in 0..20 {
                keep.push(frame_to_image(&f, op));
            }
            eprintln!("convert into fresh memory {:?}", t.elapsed() / 20);
            let t = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(frame_to_image_scaled(&f, 640, 360));
            }
            eprintln!("convert at a third {:?}", t.elapsed() / 20);
        }
    }
}
