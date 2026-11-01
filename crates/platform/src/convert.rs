//! Decoded VideoToolbox pixel buffers (as byte rows) to the planar samples FilmCraft's ProRes
//! decoder gives: `u16` planes holding 10-bit (4:2:2) or 12-bit (4:4:4) samples, alpha at the
//! same depth. Safe code; the FFI module hands over the locked rows.
//!
//! Speed (1 Nov 2026): this copy ran on VideoToolbox's callback thread and was ~80% of a 4K
//! 4444+alpha hardware decode (about 13 ms of 16.6 ms a frame, the decode itself ~3 ms). Each
//! row is now converted by plain loops over whole rows that the compiler turns into NEON code
//! (a saturating add and a shift per 8 samples, `ld2`/`ld4` for the interleaved Cb Cr), and a big
//! picture is split into bands of rows on up to [`MAX_THREADS`] scoped threads. The output is the
//! same, sample for sample (the tests compare against the old per-sample code).

/// Planar samples of one picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Planes {
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
    pub alpha: Option<Vec<u16>>,
}

/// One plane of a locked buffer: `stride` bytes a row.
#[derive(Clone, Copy)]
pub(crate) struct Rows<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

impl<'a> Rows<'a> {
    /// `len` bytes of row `y`.
    fn row(&self, y: usize, len: usize) -> Result<&'a [u8], String> {
        let start = y.checked_mul(self.stride).ok_or("row offset overflows")?;
        let end = start.checked_add(len).ok_or("row end overflows")?;
        self.data.get(start..end).ok_or_else(|| "decoded picture row out of range".to_string())
    }
}

/// Most threads one picture is split across. Several decoders can run at once (one per ProRes
/// layer in view), so one picture doesn't take every core; past 4 the copy is bound by memory
/// bandwidth anyway (M1 Max, 4K 4444+alpha: 3.4 ms on 1 thread, 2.4 on 2, 2.2 on 4, 2.2 on 8).
const MAX_THREADS: usize = 4;

/// Samples (all planes together) per thread: a thread costs tens of microseconds to start, so
/// small pictures stay on the calling thread (1080p 4:2:2 gets 2 threads, 4K 4:4:4 gets 4).
const SAMPLES_PER_THREAD: usize = 1 << 21;

/// How many threads convert a picture of `samples` samples.
fn threads_for(samples: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    (samples / SAMPLES_PER_THREAD).clamp(1, MAX_THREADS.min(cores).max(1))
}

/// A 16-bit video-range sample at `16 - S` bits, rounded: VideoToolbox keeps more precision
/// than the coded depth, and truncating it put the picture about 0.1 code below the software
/// decoder's on average (rounded: within 0.02). The saturating add is the clamp: a sum past
/// `u16::MAX` would round up to `1 << (16 - S)`, and saturating gives the top code instead, the
/// same as rounding in 32 bits and clamping, but 8 samples per NEON instruction.
#[inline(always)]
fn round_down<const S: u32>(v: u16) -> u16 {
    v.saturating_add(1 << (S - 1)) >> S
}

/// One row of a Y' plane.
#[inline(always)]
fn luma_row<const S: u32>(out: &mut [u16], src: &[u8]) {
    for (o, b) in out.iter_mut().zip(src.as_chunks::<2>().0) {
        *o = round_down::<S>(u16::from_le_bytes(*b));
    }
}

/// One row of a full-range alpha plane at `16 - S` bits. Its low bits repeat the high ones
/// (`a << 4 | a >> 8` at 12 bits), so dropping them gives the coded value exactly; rounding
/// would add one to the upper half.
#[inline(always)]
fn alpha_row<const S: u32>(out: &mut [u16], src: &[u8]) {
    for (o, b) in out.iter_mut().zip(src.as_chunks::<2>().0) {
        *o = u16::from_le_bytes(*b) >> S;
    }
}

/// One row of an interleaved Cb Cr plane, split into its two planes.
#[inline(always)]
fn chroma_row<const S: u32>(cb: &mut [u16], cr: &mut [u16], src: &[u8]) {
    for ((u, v), q) in cb.iter_mut().zip(cr.iter_mut()).zip(src.as_chunks::<4>().0) {
        *u = round_down::<S>(u16::from_le_bytes([q[0], q[1]]));
        *v = round_down::<S>(u16::from_le_bytes([q[2], q[3]]));
    }
}

/// A band of whole output rows, from picture row `row0`.
struct Band<'o> {
    row0: usize,
    y: &'o mut [u16],
    cb: &'o mut [u16],
    cr: &'o mut [u16],
    a: Option<&'o mut [u16]>,
}

/// Split the planes (Y' and alpha `w` samples a row, Cb and Cr `cw`) into at most `threads`
/// bands of whole rows.
fn bands<'o>(y: &'o mut [u16], cb: &'o mut [u16], cr: &'o mut [u16], a: Option<&'o mut [u16]>, w: usize, cw: usize, h: usize, threads: usize) -> Vec<Band<'o>> {
    let rows = h.div_ceil(threads.max(1)).max(1);
    let (luma, chroma) = (rows.saturating_mul(w).max(1), rows.saturating_mul(cw).max(1));
    let mut a_bands = a.map(|a| a.chunks_mut(luma));
    y.chunks_mut(luma)
        .zip(cb.chunks_mut(chroma))
        .zip(cr.chunks_mut(chroma))
        .enumerate()
        .map(|(i, ((y, cb), cr))| Band { row0: i.saturating_mul(rows), y, cb, cr, a: a_bands.as_mut().and_then(Iterator::next) })
        .collect()
}

/// Run `job` on every band: the first on this thread, the others on scoped threads. The first
/// error wins; a thread that panicked is an error too.
fn run<'o>(bands: Vec<Band<'o>>, job: impl Fn(Band<'o>) -> Result<(), String> + Sync) -> Result<(), String> {
    let mut bands = bands.into_iter();
    let Some(first) = bands.next() else { return Ok(()) };
    std::thread::scope(|s| {
        let job = &job;
        let others: Vec<_> = bands.map(|b| s.spawn(move || job(b))).collect();
        let mut res = job(first);
        for t in others {
            let r = t.join().unwrap_or_else(|_| Err("picture conversion thread failed".to_string()));
            res = res.and(r);
        }
        res
    })
}

/// Tri-planar `s4as` (`kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar`: a Y' plane, an
/// interleaved Cb Cr plane at full width and an alpha plane, 16-bit little-endian, video-range
/// Y'CbCr in the high bits, full-range alpha) to 12-bit 4:4:4 planes; alpha only when `alpha`.
pub(crate) fn unpack_s4as(luma: Rows, chroma: Rows, a: Option<Rows>, w: usize, h: usize) -> Result<Planes, String> {
    let n = w.checked_mul(h).ok_or("picture size overflows")?;
    unpack_s4as_on(luma, chroma, a, w, h, threads_for(n.saturating_mul(if a.is_some() { 4 } else { 3 })))
}

/// [`unpack_s4as`] on `threads` threads.
fn unpack_s4as_on(luma: Rows, chroma: Rows, a: Option<Rows>, w: usize, h: usize, threads: usize) -> Result<Planes, String> {
    let n = w.checked_mul(h).ok_or("picture size overflows")?;
    // `vec![0; n]` is calloc: fresh zeroed pages, not a pass that writes zeros.
    let (mut y, mut cb, mut cr) = (vec![0u16; n], vec![0u16; n], vec![0u16; n]);
    let mut alpha = a.map(|_| vec![0u16; n]);
    if w == 0 {
        return Ok(Planes { y, cb, cr, alpha });
    }
    let (luma_len, chroma_len) = (w.saturating_mul(2), w.saturating_mul(4));
    let parts = bands(&mut y, &mut cb, &mut cr, alpha.as_deref_mut(), w, w, h, threads);
    run(parts, |b| {
        let rows = b.y.chunks_exact_mut(w).zip(b.cb.chunks_exact_mut(w)).zip(b.cr.chunks_exact_mut(w));
        for (i, ((yo, cbo), cro)) in rows.enumerate() {
            let row = b.row0.saturating_add(i);
            luma_row::<4>(yo, luma.row(row, luma_len)?);
            chroma_row::<4>(cbo, cro, chroma.row(row, chroma_len)?);
        }
        if let (Some(out), Some(a)) = (b.a, a) {
            for (i, ao) in out.chunks_exact_mut(w).enumerate() {
                alpha_row::<4>(ao, a.row(b.row0.saturating_add(i), luma_len)?);
            }
        }
        Ok(())
    })?;
    Ok(Planes { y, cb, cr, alpha })
}

/// Biplanar `x422` (`kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange`: a Y' plane and an
/// interleaved Cb Cr plane at half width, 10-bit samples in the high bits of 16-bit words) to
/// 10-bit 4:2:2 planes.
pub(crate) fn unpack_x422(luma: Rows, chroma: Rows, w: usize, h: usize) -> Result<Planes, String> {
    let n = w.checked_mul(h).ok_or("picture size overflows")?;
    unpack_x422_on(luma, chroma, w, h, threads_for(n.saturating_mul(2)))
}

/// [`unpack_x422`] on `threads` threads.
fn unpack_x422_on(luma: Rows, chroma: Rows, w: usize, h: usize, threads: usize) -> Result<Planes, String> {
    let cw = w.div_ceil(2);
    let n = w.checked_mul(h).ok_or("picture size overflows")?;
    let cn = cw.checked_mul(h).ok_or("picture size overflows")?;
    let (mut y, mut cb, mut cr) = (vec![0u16; n], vec![0u16; cn], vec![0u16; cn]);
    if w == 0 {
        return Ok(Planes { y, cb, cr, alpha: None });
    }
    let (luma_len, chroma_len) = (w.saturating_mul(2), cw.saturating_mul(4));
    let parts = bands(&mut y, &mut cb, &mut cr, None, w, cw, h, threads);
    run(parts, |b| {
        let rows = b.y.chunks_exact_mut(w).zip(b.cb.chunks_exact_mut(cw)).zip(b.cr.chunks_exact_mut(cw));
        for (i, ((yo, cbo), cro)) in rows.enumerate() {
            let row = b.row0.saturating_add(i);
            luma_row::<6>(yo, luma.row(row, luma_len)?);
            chroma_row::<6>(cbo, cro, chroma.row(row, chroma_len)?);
        }
        Ok(())
    })?;
    Ok(Planes { y, cb, cr, alpha: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s4as_planes_unpack_to_12_bit_planes() {
        let words = |v: &[u16]| v.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        // 2×1 picture; the luma plane has 4 bytes of row padding
        let mut luma = words(&[64 << 4, (940 << 4) + 9]);
        luma.extend([0; 4]);
        let chroma = words(&[2048 << 4, 100 << 4, (1000 << 4) + 7, 0xffff]);
        let alpha = words(&[0xffff, 0x0fff]);
        fn rows(data: &[u8], stride: usize) -> Rows<'_> {
            Rows { data, stride }
        }
        let p = unpack_s4as(rows(&luma, 8), rows(&chroma, 8), Some(rows(&alpha, 4)), 2, 1).unwrap();
        assert_eq!(p.y, vec![64, 941], "rounded to the nearest 12-bit code");
        assert_eq!(p.cb, vec![2048, 1000]);
        assert_eq!(p.cr, vec![100, 4095], "clamped to 12 bits");
        assert_eq!(p.alpha, Some(vec![4095, 255]), "alpha drops its repeated low bits");
        assert_eq!(unpack_s4as(rows(&luma, 8), rows(&chroma, 8), None, 2, 1).unwrap().alpha, None);
        // planes shorter than the picture are an error, not a panic
        assert!(unpack_s4as(rows(&luma, 8), rows(&chroma, 8), None, 2, 2).is_err());
        assert!(unpack_s4as(rows(&luma, usize::MAX), rows(&chroma, 8), None, 2, 2).is_err());
    }

    #[test]
    fn x422_planes_unpack_to_10_bit_planes() {
        let words = |v: &[u16]| v.iter().flat_map(|v| (v << 6).to_le_bytes()).collect::<Vec<u8>>();
        let luma = words(&[64, 512, 940]);
        let chroma = words(&[100, 200, 300, 400]);
        let p = unpack_x422(Rows { data: &luma, stride: 6 }, Rows { data: &chroma, stride: 8 }, 3, 1).unwrap();
        assert_eq!(p.y, vec![64, 512, 940]);
        assert_eq!((p.cb, p.cr), (vec![100, 300], vec![200, 400]));
        assert!(unpack_x422(Rows { data: &luma, stride: 6 }, Rows { data: &chroma, stride: 8 }, 3, 2).is_err());
    }

    /// The per-sample code this module had until 1 Nov 2026, kept as the reference the fast
    /// code must match exactly.
    mod reference {
        use super::super::{Planes, Rows};

        fn round_down(lo: u8, hi: u8, shift: u32) -> u16 {
            let v = u32::from(u16::from_le_bytes([lo, hi]));
            ((v + (1 << (shift - 1))) >> shift).min((1 << (16 - shift)) - 1) as u16
        }

        fn alpha_down(lo: u8, hi: u8, shift: u32) -> u16 {
            u16::from_le_bytes([lo, hi]) >> shift
        }

        pub fn unpack_s4as(luma: Rows, chroma: Rows, a: Option<Rows>, w: usize, h: usize) -> Result<Planes, String> {
            let n = w.checked_mul(h).ok_or("picture size overflows")?;
            let (mut y, mut cb, mut cr) = (vec![0u16; n], vec![0u16; n], vec![0u16; n]);
            let mut alpha = a.map(|_| vec![0u16; n]);
            if w == 0 {
                return Ok(Planes { y, cb, cr, alpha });
            }
            let rows = y.chunks_exact_mut(w).zip(cb.chunks_exact_mut(w)).zip(cr.chunks_exact_mut(w));
            for (row, ((yo, cbo), cro)) in rows.enumerate() {
                for (o, b) in yo.iter_mut().zip(luma.row(row, w * 2)?.as_chunks::<2>().0) {
                    *o = round_down(b[0], b[1], 4);
                }
                for ((u, v), q) in cbo.iter_mut().zip(cro.iter_mut()).zip(chroma.row(row, w * 4)?.as_chunks::<4>().0) {
                    *u = round_down(q[0], q[1], 4);
                    *v = round_down(q[2], q[3], 4);
                }
            }
            if let (Some(out), Some(a)) = (alpha.as_mut(), a) {
                for (row, ao) in out.chunks_exact_mut(w).enumerate() {
                    for (o, b) in ao.iter_mut().zip(a.row(row, w * 2)?.as_chunks::<2>().0) {
                        *o = alpha_down(b[0], b[1], 4);
                    }
                }
            }
            Ok(Planes { y, cb, cr, alpha })
        }

        pub fn unpack_x422(luma: Rows, chroma: Rows, w: usize, h: usize) -> Result<Planes, String> {
            let cw = w.div_ceil(2);
            let n = w.checked_mul(h).ok_or("picture size overflows")?;
            let cn = cw.checked_mul(h).ok_or("picture size overflows")?;
            let (mut y, mut cb, mut cr) = (vec![0u16; n], vec![0u16; cn], vec![0u16; cn]);
            if w == 0 {
                return Ok(Planes { y, cb, cr, alpha: None });
            }
            let rows = y.chunks_exact_mut(w).zip(cb.chunks_exact_mut(cw)).zip(cr.chunks_exact_mut(cw));
            for (row, ((yo, cbo), cro)) in rows.enumerate() {
                for (o, b) in yo.iter_mut().zip(luma.row(row, w * 2)?.as_chunks::<2>().0) {
                    *o = round_down(b[0], b[1], 6);
                }
                for ((u, v), q) in cbo.iter_mut().zip(cro.iter_mut()).zip(chroma.row(row, cw * 4)?.as_chunks::<4>().0) {
                    *u = round_down(q[0], q[1], 6);
                    *v = round_down(q[2], q[3], 6);
                }
            }
            Ok(Planes { y, cb, cr, alpha: None })
        }
    }

    /// Pseudo-random bytes, with the words near the top of the range (where rounding saturates)
    /// and near rounding ties over-represented.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(len + 1);
        while out.len() < len {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let v = match s % 8 {
                0 => 0xffff - (s >> 8) as u16 % 64,
                1 => ((s >> 8) as u16 & !0x3f) | 0x20,
                2 => ((s >> 8) as u16 & !0xf) | 0x8,
                3 => (s >> 8) as u16 % 64,
                _ => (s >> 8) as u16,
            };
            out.extend(v.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn s4as_matches_the_per_sample_code_on_any_size_stride_and_thread_count() {
        for (w, h) in [(1usize, 1usize), (2, 1), (3, 5), (7, 3), (33, 17), (257, 9), (640, 31), (1, 64)] {
            for pad in [0usize, 2, 6, 64] {
                let (ls, cs) = (w * 2 + pad, w * 4 + pad);
                // the last row carries no padding, as in a tight buffer
                let (luma, chroma, alpha) = (noise(ls * h - pad, 1), noise(cs * h - pad, 2), noise(ls * h, 3));
                let (l, c, a) = (Rows { data: &luma, stride: ls }, Rows { data: &chroma, stride: cs }, Rows { data: &alpha, stride: ls });
                for with_alpha in [false, true] {
                    let a = with_alpha.then_some(a);
                    let want = reference::unpack_s4as(l, c, a, w, h).unwrap();
                    assert_eq!(unpack_s4as(l, c, a, w, h).unwrap(), want, "{w}x{h} pad {pad} alpha {with_alpha}");
                    for t in [1, 2, 3, 4, 7, 100] {
                        assert_eq!(unpack_s4as_on(l, c, a, w, h, t).unwrap(), want, "{w}x{h} pad {pad} alpha {with_alpha} {t} threads");
                    }
                }
            }
        }
    }

    #[test]
    fn x422_matches_the_per_sample_code_on_any_size_stride_and_thread_count() {
        for (w, h) in [(1usize, 1usize), (2, 1), (3, 5), (7, 3), (33, 17), (257, 9), (640, 31), (1, 64)] {
            let cw = w.div_ceil(2);
            for pad in [0usize, 2, 6, 64] {
                let (ls, cs) = (w * 2 + pad, cw * 4 + pad);
                let (luma, chroma) = (noise(ls * h - pad, 4), noise(cs * h - pad, 5));
                let (l, c) = (Rows { data: &luma, stride: ls }, Rows { data: &chroma, stride: cs });
                let want = reference::unpack_x422(l, c, w, h).unwrap();
                assert_eq!(unpack_x422(l, c, w, h).unwrap(), want, "{w}x{h} pad {pad}");
                for t in [1, 2, 3, 4, 7, 100] {
                    assert_eq!(unpack_x422_on(l, c, w, h, t).unwrap(), want, "{w}x{h} pad {pad} {t} threads");
                }
            }
        }
    }

    #[test]
    fn every_16_bit_word_rounds_as_before() {
        let all: Vec<u8> = (0..=u16::MAX).flat_map(u16::to_le_bytes).collect();
        let chroma: Vec<u8> = (0..=u16::MAX).flat_map(|v| [v.to_le_bytes(), (!v).to_le_bytes()]).flatten().collect();
        let w = 1 << 16;
        let (l, c) = (Rows { data: &all, stride: w * 2 }, Rows { data: &chroma, stride: w * 4 });
        let a = Some(l);
        assert_eq!(unpack_s4as_on(l, c, a, w, 1, 1).unwrap(), reference::unpack_s4as(l, c, a, w, 1).unwrap());
        let c422 = Rows { data: &chroma, stride: w * 2 };
        assert_eq!(unpack_x422_on(l, c422, w, 1, 1).unwrap(), reference::unpack_x422(l, c422, w, 1).unwrap());
    }

    #[test]
    fn short_planes_fail_on_every_thread_count_without_a_panic() {
        let (w, h) = (16, 40);
        let luma = noise(w * 2 * h, 6);
        let short = noise(w * 4 * (h - 1), 7);
        let (l, c) = (Rows { data: &luma, stride: w * 2 }, Rows { data: &short, stride: w * 4 });
        for t in [1, 2, 4] {
            assert!(unpack_s4as_on(l, c, None, w, h, t).is_err(), "{t} threads");
            assert!(unpack_s4as_on(l, Rows { data: &luma, stride: w * 2 }, Some(l), w, h, t).is_err(), "short chroma, {t} threads");
            assert!(unpack_x422_on(Rows { data: &luma[..w * 2 * (h - 1)], stride: w * 2 }, c, w, h, t).is_err(), "{t} threads");
        }
        // an empty picture is fine
        assert_eq!(unpack_s4as_on(l, c, Some(l), 0, 5, 4).unwrap().y, Vec::<u16>::new());
        assert_eq!(unpack_x422_on(l, c, 3, 0, 4).unwrap().cb, Vec::<u16>::new());
    }
}
