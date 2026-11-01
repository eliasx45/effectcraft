//! Planar ProRes encoder input (FilmCraft's `filmcraft_prores::Frame`: `u16` planes at any depth
//! 8..=16) into the source buffers of VideoToolbox's hardware ProRes encoders, as byte rows the
//! FFI module (`videotoolbox_enc`) hands over. Safe code.
//!
//! - `s4as` (`kCVPixelFormatType_444YpCbCr16VideoRange_16A_TriPlanar`, 4444 / 4444 XQ): a Y'
//!   plane, an interleaved Cb Cr plane at full width and an alpha plane, 16-bit little-endian;
//!   video-range Y'CbCr codes in the high bits (`v << (16 - depth)`), full-range alpha scaled to
//!   16 bits (`a · 65535 / max`, which repeats the high bits in the low ones, as the decoder's
//!   `s4as` output does). Without alpha the plane is opaque (the session then doesn't code it).
//! - `sv22` (`kCVPixelFormatType_422YpCbCr16BiPlanarVideoRange`, the 422 family): a Y' plane and
//!   an interleaved Cb Cr plane at half width, 16-bit little-endian, codes in the high bits.

use filmcraft_prores::{ChromaFormat, Frame};

/// One plane of a locked buffer: `stride` bytes a row.
pub(crate) struct RowsMut<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

impl RowsMut<'_> {
    /// `len` bytes of row `y`.
    fn row(&mut self, y: usize, len: usize) -> Result<&mut [u8], String> {
        let start = y.checked_mul(self.stride).ok_or("row offset overflows")?;
        let end = start.checked_add(len).ok_or("row end overflows")?;
        self.data.get_mut(start..end).ok_or_else(|| "source picture row out of range".to_string())
    }
}

/// A `depth`-bit video-range code as a 16-bit one (codes above the depth's range clamp).
#[inline]
fn up(v: u16, shift: u32, max: u16) -> [u8; 2] {
    (v.min(max) << shift).to_le_bytes()
}

/// A `depth`-bit full-range alpha sample as a 16-bit one.
#[inline]
fn alpha_up(a: u16, max: u16) -> [u8; 2] {
    let m = u32::from(max.max(1));
    (((u32::from(a.min(max)) * 65535 + m / 2) / m).min(65535) as u16).to_le_bytes()
}

fn geometry(f: &Frame, chroma: ChromaFormat) -> Result<(usize, usize, usize, u32, u16), String> {
    if f.chroma != chroma {
        return Err(format!("{:?} frame for a {chroma:?} buffer", f.chroma));
    }
    if !(8..=16).contains(&f.bit_depth) {
        return Err(format!("{}-bit samples", f.bit_depth));
    }
    let (w, h) = (f.width as usize, f.height as usize);
    let cw = chroma.chroma_width(f.width) as usize;
    let n = w.checked_mul(h).ok_or("picture size overflows")?;
    let cn = cw.checked_mul(h).ok_or("picture size overflows")?;
    if f.y.len() < n || f.cb.len() < cn || f.cr.len() < cn || f.alpha.as_ref().is_some_and(|a| a.len() < n) {
        return Err("frame planes are smaller than the picture".into());
    }
    let shift = 16 - u32::from(f.bit_depth);
    let max = ((1u32 << f.bit_depth) - 1) as u16;
    Ok((w, h, cw, shift, max))
}

/// Fill an `s4as` buffer's three planes (Y', Cb Cr, A) from a 4:4:4 frame.
pub(crate) fn pack_s4as(f: &Frame, planes: &mut [RowsMut<'_>]) -> Result<(), String> {
    let (w, h, _, shift, max) = geometry(f, ChromaFormat::Yuv444)?;
    let [luma, chroma, alpha] = planes else {
        return Err(format!("s4as buffer with {} planes", planes.len()));
    };
    for row in 0..h {
        let at = row * w;
        let (ys, cbs, crs) = (f.y.get(at..at + w), f.cb.get(at..at + w), f.cr.get(at..at + w));
        let (Some(ys), Some(cbs), Some(crs)) = (ys, cbs, crs) else { return Err("frame row out of range".into()) };
        for (o, &v) in luma.row(row, w * 2)?.as_chunks_mut::<2>().0.iter_mut().zip(ys) {
            *o = up(v, shift, max);
        }
        for (o, (&u, &v)) in chroma.row(row, w * 4)?.as_chunks_mut::<4>().0.iter_mut().zip(cbs.iter().zip(crs)) {
            let (a, b) = (up(u, shift, max), up(v, shift, max));
            *o = [a[0], a[1], b[0], b[1]];
        }
        let out = alpha.row(row, w * 2)?;
        match f.alpha.as_ref().and_then(|a| a.get(at..at + w)) {
            Some(src) => {
                for (o, &a) in out.as_chunks_mut::<2>().0.iter_mut().zip(src) {
                    *o = alpha_up(a, max);
                }
            }
            None => out.fill(0xff),
        }
    }
    Ok(())
}

/// Fill an `sv22` buffer's two planes (Y', Cb Cr) from a 4:2:2 frame.
pub(crate) fn pack_sv22(f: &Frame, planes: &mut [RowsMut<'_>]) -> Result<(), String> {
    let (w, h, cw, shift, max) = geometry(f, ChromaFormat::Yuv422)?;
    let [luma, chroma] = planes else {
        return Err(format!("sv22 buffer with {} planes", planes.len()));
    };
    for row in 0..h {
        let (at, cat) = (row * w, row * cw);
        let (ys, cbs, crs) = (f.y.get(at..at + w), f.cb.get(cat..cat + cw), f.cr.get(cat..cat + cw));
        let (Some(ys), Some(cbs), Some(crs)) = (ys, cbs, crs) else { return Err("frame row out of range".into()) };
        for (o, &v) in luma.row(row, w * 2)?.as_chunks_mut::<2>().0.iter_mut().zip(ys) {
            *o = up(v, shift, max);
        }
        for (o, (&u, &v)) in chroma.row(row, cw * 4)?.as_chunks_mut::<4>().0.iter_mut().zip(cbs.iter().zip(crs)) {
            let (a, b) = (up(u, shift, max), up(v, shift, max));
            *o = [a[0], a[1], b[0], b[1]];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(b: &[u8]) -> Vec<u16> {
        b.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).collect()
    }

    #[test]
    fn s4as_packs_codes_high_and_alpha_full_range() {
        let mut f = Frame::new(2, 1, ChromaFormat::Yuv444, 12, true);
        f.y = vec![256, 4095];
        f.cb = vec![2048, 5000];
        f.cr = vec![100, 0];
        f.alpha = Some(vec![4095, 0x800]);
        // the luma plane has 4 bytes of row padding
        let (mut l, mut c, mut a) = (vec![0u8; 8], vec![0u8; 8], vec![0u8; 4]);
        let mut planes = [RowsMut { data: &mut l, stride: 8 }, RowsMut { data: &mut c, stride: 8 }, RowsMut { data: &mut a, stride: 4 }];
        pack_s4as(&f, &mut planes).unwrap();
        assert_eq!(words(&l), vec![256 << 4, 4095 << 4, 0, 0]);
        assert_eq!(words(&c), vec![2048 << 4, 100 << 4, 4095 << 4, 0], "Cb Cr interleaved, out-of-range codes clamp");
        assert_eq!(words(&a), vec![0xffff, 0x8008], "alpha repeats its high bits");
        // without alpha the plane is opaque
        f.alpha = None;
        let mut planes = [RowsMut { data: &mut l, stride: 8 }, RowsMut { data: &mut c, stride: 8 }, RowsMut { data: &mut a, stride: 4 }];
        pack_s4as(&f, &mut planes).unwrap();
        assert_eq!(words(&a), vec![0xffff, 0xffff]);
        // a plane shorter than the picture, the wrong plane count or chroma: errors, not panics
        let mut short = vec![0u8; 2];
        let mut planes = [RowsMut { data: &mut short, stride: 2 }, RowsMut { data: &mut c, stride: 8 }, RowsMut { data: &mut a, stride: 4 }];
        assert!(pack_s4as(&f, &mut planes).is_err());
        assert!(pack_s4as(&f, &mut []).is_err());
        assert!(pack_sv22(&f, &mut []).is_err());
    }

    #[test]
    fn sv22_packs_half_width_chroma() {
        let mut f = Frame::new(3, 1, ChromaFormat::Yuv422, 10, false);
        f.y = vec![64, 512, 940];
        f.cb = vec![100, 300];
        f.cr = vec![200, 400];
        let (mut l, mut c) = (vec![0u8; 6], vec![0u8; 8]);
        let mut planes = [RowsMut { data: &mut l, stride: 6 }, RowsMut { data: &mut c, stride: 8 }];
        pack_sv22(&f, &mut planes).unwrap();
        assert_eq!(words(&l), vec![64 << 6, 512 << 6, 940 << 6]);
        assert_eq!(words(&c), vec![100 << 6, 200 << 6, 300 << 6, 400 << 6]);
        // 8-bit input
        let mut f8 = Frame::new(3, 1, ChromaFormat::Yuv422, 8, false);
        f8.y = vec![16, 128, 235];
        let mut planes = [RowsMut { data: &mut l, stride: 6 }, RowsMut { data: &mut c, stride: 8 }];
        pack_sv22(&f8, &mut planes).unwrap();
        assert_eq!(words(&l), vec![16 << 8, 128 << 8, 235 << 8]);
    }
}
