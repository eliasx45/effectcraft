//! Lossless compact storage of quantised layer buffers (the [`crate::LayerCache`]).
//!
//! In 8 and 16 bpc projects each layer buffer is clamped and quantised to the bit depth
//! ([`crate::color::Pipe::quantize`]): every premultiplied channel is `k as f32 * (1.0 / levels)`
//! for an integer `k` in `0..=levels` (colour ≤ alpha). Such a buffer can be kept as those
//! integers — `u8` for 255 levels, `u16` for 32768 — a quarter or half of its `f32` size, and
//! unpacks to the identical floats. [`PackedBuf::pack`] checks that every value round-trips bit
//! for bit and declines otherwise (32 bpc, linear-blending copies, unquantised style passes), so
//! packing never changes a pixel.

use effectcraft_effects::Buf;
use effectcraft_raster::{Image, Px};
use rayon::prelude::*;

/// A [`Buf`] whose pixels sit on an integer grid, stored as the grid's integers.
#[derive(Clone, Debug)]
pub(crate) struct PackedBuf {
    width: u32,
    height: u32,
    offset: [f64; 2],
    scale: f64,
    data: Packed,
}

#[derive(Clone, Debug)]
enum Packed {
    /// 8 bpc: 255 levels.
    U8(Vec<[u8; 4]>),
    /// 16 bpc: After Effects' 32768 levels.
    U16(Vec<[u16; 4]>),
}

const L8: f32 = 255.0;
const L16: f32 = 32768.0;

/// Pixels per band of work: a band costs far more than handing it to a thread, and a buffer
/// that is off the grid stops after the bands already running.
const BAND: usize = 1 << 16;

/// The integer codes of an 8 bpc grid image: `k` with `k as f32 * (1.0 / 255.0)` bitwise equal
/// to each value, or `None` when a value is off that grid (out of range, negative zero, NaN) or
/// the codes can't be allocated. Shared by the layer cache's packing and the GPU upload.
pub fn codes8(data: &[Px]) -> Option<Vec<[u8; 4]>> {
    codes(data, L8, |k| k as u8)
}

/// [`codes8`] on the 16 bpc grid (After Effects' 32768 levels).
pub fn codes16(data: &[Px]) -> Option<Vec<[u16; 4]>> {
    codes(data, L16, |k| k as u16)
}

/// One band's codes, the check folded into a flag instead of a branch per value, so the loop
/// vectorises. Whether every value was on the grid (`dst` holds garbage otherwise).
#[inline(always)]
fn codes_band<T: Copy>(dst: &mut [[T; 4]], src: &[Px], levels: f32, inv: f32, conv: &impl Fn(u32) -> T) -> bool {
    let mut ok = true;
    for (d, p) in dst.iter_mut().zip(src) {
        for c in 0..4 {
            let v = p[c];
            // Same rounding as `color::quantize_px` (on the grid v is non-negative, so the cast
            // rounds half up; NaN and negatives saturate to 0 and then fail the checks).
            let k = (v * levels + 0.5) as u32;
            ok &= (0.0..=1.0).contains(&v) & ((k as f32 * inv).to_bits() == v.to_bits());
            d[c] = conv(k);
        }
    }
    ok
}

// One pixel allocation per attempt: collecting Option<Vec<_>> in parallel can
// retain per-worker chunks while allocating a second final vector. That peak would
// exceed the cache's pending packed-output reservation. Task/allocator overhead is
// not pixel storage; final actual vector capacity is checked again on publication.
fn codes<T: Copy + Default + Send>(data: &[Px], levels: f32, conv: impl Fn(u32) -> T + Sync + Send) -> Option<Vec<[T; 4]>> {
    let mut out = Vec::new();
    out.try_reserve_exact(data.len()).ok()?;
    out.resize(data.len(), [T::default(); 4]);
    let inv = 1.0 / levels;
    out.par_chunks_mut(BAND).zip(data.par_chunks(BAND)).try_for_each(|(d, s)| codes_band(d, s, levels, inv, &conv).then_some(()))?;
    Some(out)
}

impl PackedBuf {
    /// `buf` packed losslessly, or `None` when a value is not on the 8 or 16 bpc grid.
    pub(crate) fn pack(buf: &Buf) -> Option<PackedBuf> {
        let px = &buf.img.data;
        let data = match codes8(px) {
            Some(d) => Packed::U8(d),
            None => Packed::U16(codes16(px)?),
        };
        Some(PackedBuf { width: buf.img.width, height: buf.img.height, offset: buf.offset, scale: buf.scale, data })
    }

    /// The buffer again (bit-identical to the one packed).
    pub(crate) fn unpack(&self) -> Buf {
        fn floats<T: Copy + Into<u32> + Sync>(d: &[[T; 4]], levels: f32) -> Vec<Px> {
            let inv = 1.0 / levels;
            d.par_iter().map(|p| p.map(|k| k.into() as f32 * inv)).collect()
        }
        let data = match &self.data {
            Packed::U8(d) => floats(d, L8),
            Packed::U16(d) => floats(d, L16),
        };
        Buf { img: Image { width: self.width, height: self.height, data }, offset: self.offset, scale: self.scale }
    }

    /// Allocated pixel bytes, including spare vector capacity.
    pub(crate) fn capacity_bytes(&self) -> Option<usize> {
        match &self.data {
            Packed::U8(d) => d.capacity().checked_mul(4),
            Packed::U16(d) => d.capacity().checked_mul(8),
        }
    }

    /// Bytes of pixel storage.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        match &self.data {
            Packed::U8(d) => d.len() * 4,
            Packed::U16(d) => d.len() * 8,
        }
    }
}
