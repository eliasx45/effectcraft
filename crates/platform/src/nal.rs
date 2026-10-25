//! H.264 / HEVC stream facts for the hardware decoder, read from the sample entry's `avcC` /
//! `hvcC` (safe code; compiled and tested on every system).
//!
//! [`select`] decides whether a sample entry goes to the hardware decoder: only in a preview read
//! ([`effectcraft_media::hw::allowed`]), only for streams FilmCraft's software decoder also takes
//! (so a hardware failure can always fall back to it, [`crate::hybrid`]) and only for the layouts
//! the hardware path outputs exactly like the software decoder: progressive 4:2:0, 8-bit H.264,
//! 8- or 10-bit HEVC, at most 8192 pixels a side. Pictures carry the software decoder's colour,
//! pixel aspect and cropping, computed here from the SPS the same way.

use filmcraft_codecs::video::{H264Decoder, HevcDecoder, h264_disposable, hevc_disposable};
use filmcraft_codecs::{Result, VideoDecoder};
use filmcraft_color::{ColorInfo, Matrix, Range, Transfer};
use filmcraft_isobmff::{CodecConfig, SampleEntry};

/// Largest picture side the hardware path takes.
const MAX_SIDE: u32 = 8192;

/// The two NAL-unit codecs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

/// How decoded pictures look (what the software decoder would output).
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Significant bits per sample: 8 (`Yuv8`) or 10 (`Yuv16`).
    pub bits: u32,
    /// Full-range samples (the decoder's buffer format follows it).
    pub full_range: bool,
    /// Coded (uncropped) luma size.
    pub coded: (u32, u32),
    /// Output rectangle in luma samples: x, y, width, height.
    pub crop: (u32, u32, u32, u32),
    pub color: ColorInfo,
    pub par: (u32, u32),
}

/// One `avcC` / `hvcC` stream the hardware path takes.
#[derive(Clone, Debug)]
pub struct NalStream {
    pub codec: NalCodec,
    /// NAL length field size of the samples (1, 2 or 4).
    pub length_size: usize,
    /// The parameter sets of the record, NAL header included: SPS then PPS (H.264), VPS, SPS,
    /// PPS (HEVC).
    pub parameter_sets: Vec<Vec<u8>>,
    /// Pictures that may precede a picture in decoding order and follow it in output order.
    pub reorder: usize,
    /// HEVC: the highest temporal sub-layer (`numTemporalLayers - 1`), for disposable samples.
    pub highest_tid: Option<u8>,
    pub layout: Layout,
    /// The `avcC` / `hvcC` bytes (the software decoder is made from them).
    record: Vec<u8>,
}

/// Why a sample entry does not go to the hardware decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decline {
    /// Not a preview read: exports and final renders decode exactly.
    NotPreview,
    /// Not an `avcC` / `hvcC` stream (another decoder's business).
    OtherCodec,
    /// No hardware decoder for this codec on this system.
    NoHardware,
    /// A stream the hardware path does not take (why).
    Unsupported(String),
}

/// Whether `e` goes to the hardware decoder: in a preview read (`preview`), for an H.264 / HEVC
/// stream the hardware path takes, when `hardware(codec)` says the system can decode it.
pub fn select(e: &SampleEntry, preview: bool, hardware: impl Fn(NalCodec) -> bool) -> std::result::Result<NalStream, Decline> {
    if !preview {
        return Err(Decline::NotPreview);
    }
    let stream = NalStream::from_entry(e).ok_or(Decline::OtherCodec)?.map_err(Decline::Unsupported)?;
    if !hardware(stream.codec) {
        return Err(Decline::NoHardware);
    }
    Ok(stream)
}

/// The NAL units of a length-prefixed sample (stops at the first malformed length).
pub fn nal_units(sample: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> + '_ {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if !(1..=4).contains(&length_size) {
            return None;
        }
        let len_bytes = sample.get(pos..pos.checked_add(length_size)?)?;
        let len = len_bytes.iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        let start = pos + length_size;
        let end = start.checked_add(len)?;
        let nal = sample.get(start..end)?;
        pos = end;
        (!nal.is_empty()).then_some(nal)
    })
}

/// NAL payload without emulation-prevention bytes (`00 00 03` → `00 00`).
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The software decoder's colour for a stream (`filmcraft_codecs::video::{H264,Hevc}Decoder`):
/// Rec. 709 with the size's default matrix, overridden by the VUI's matrix and transfer codes, and
/// full range when the VUI says so.
fn color(width: u32, height: u32, matrix: u8, transfer: u8, full_range: bool) -> ColorInfo {
    let mut c = ColorInfo { matrix: filmcraft_frame::default_matrix(width, height), ..ColorInfo::REC709 };
    if let Some(m) = Matrix::from_code(matrix) {
        c.matrix = m;
    }
    if let Some(t) = Transfer::from_code(transfer) {
        c.transfer = t;
    }
    if full_range {
        c.range = Range::Full;
    }
    c
}

fn par(sar: (u16, u16)) -> (u32, u32) {
    if sar.0 > 0 && sar.1 > 0 { (u32::from(sar.0), u32::from(sar.1)) } else { (1, 1) }
}

/// The output rectangle for coded size `coded` and a crop of (left, right, top, bottom) units of
/// `unit` samples, or why it is out of range.
fn crop_rect(coded: (u32, u32), crop: (u32, u32, u32, u32), unit: (u32, u32)) -> std::result::Result<(u32, u32, u32, u32), String> {
    let (l, r, t, b) = crop;
    let bad = || format!("cropping {crop:?} of a {}x{} picture", coded.0, coded.1);
    let x = l.checked_mul(unit.0).ok_or_else(bad)?;
    let y = t.checked_mul(unit.1).ok_or_else(bad)?;
    let w = coded.0.checked_sub(l.checked_add(r).and_then(|s| s.checked_mul(unit.0)).ok_or_else(bad)?).ok_or_else(bad)?;
    let h = coded.1.checked_sub(t.checked_add(b).and_then(|s| s.checked_mul(unit.1)).ok_or_else(bad)?).ok_or_else(bad)?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((x, y, w, h))
}

fn check_size(coded: (u32, u32)) -> std::result::Result<(), String> {
    if coded.0 == 0 || coded.1 == 0 || coded.0 > MAX_SIDE || coded.1 > MAX_SIDE {
        return Err(format!("picture size {}x{}", coded.0, coded.1));
    }
    Ok(())
}

impl NalStream {
    /// The stream of an `avcC` / `hvcC` sample entry (`None`: another codec), or why the hardware
    /// path does not take it.
    pub fn from_entry(e: &SampleEntry) -> Option<std::result::Result<NalStream, String>> {
        match &e.codec {
            CodecConfig::Avc(a) => Some(Self::h264(a)),
            CodecConfig::Hevc(h) => Some(Self::hevc(h)),
            _ => None,
        }
    }

    fn h264(a: &filmcraft_isobmff::AvcConfig) -> std::result::Result<NalStream, String> {
        let [sps_nal] = a.sps.as_slice() else {
            return Err(format!("{} sequence parameter sets", a.sps.len()));
        };
        if a.pps.is_empty() {
            return Err("no picture parameter set".into());
        }
        let sps = filmcraft_h264::params::Sps::parse(&unescape(sps_nal.get(1..).ok_or("empty SPS")?)).map_err(|e| e.to_string())?;
        // what the software decoder takes, and the layout it outputs (8-bit 4:2:0 progressive)
        sps.check_supported().map_err(|e| e.to_string())?;
        if sps.separate_colour_plane || sps.chroma_format_idc != 1 || sps.bit_depth_luma != 8 || !sps.frame_mbs_only {
            return Err("not progressive 8-bit 4:2:0".into());
        }
        let coded = (sps.pic_width_in_mbs.checked_mul(16).ok_or("width")?, sps.pic_height_in_map_units.checked_mul(16).ok_or("height")?);
        check_size(coded)?;
        let crop = match sps.frame_crop {
            None => (0, 0, coded.0, coded.1),
            Some(c) => crop_rect(coded, c, (2, 2))?,
        };
        let vui = sps.vui.clone().unwrap_or_default();
        let layout = Layout {
            bits: 8,
            full_range: vui.full_range,
            coded,
            crop,
            color: color(crop.2, crop.3, vui.matrix_coefficients, vui.transfer_characteristics, vui.full_range),
            par: par(vui.sar),
        };
        let mut parameter_sets = vec![sps_nal.clone()];
        parameter_sets.extend(a.pps.iter().cloned());
        if parameter_sets.iter().any(Vec::is_empty) {
            return Err("empty parameter set".into());
        }
        Ok(NalStream {
            codec: NalCodec::H264,
            length_size: usize::from(a.length_size),
            parameter_sets,
            // (sizes are checked above: the DPB arithmetic cannot overflow)
            reorder: sps.max_num_reorder_frames().min(16),
            highest_tid: None,
            layout,
            record: a.to_bytes(),
        })
        .and_then(NalStream::check_length_size)
    }

    fn hevc(c: &filmcraft_isobmff::HevcConfig) -> std::result::Result<NalStream, String> {
        let of = |t: u8| c.arrays.iter().filter(move |a| a.nal_type == t).flat_map(|a| a.nalus.iter());
        let (vps, sps, pps): (Vec<_>, Vec<_>, Vec<_>) = (of(32).collect(), of(33).collect(), of(34).collect());
        let [sps_nal] = sps.as_slice() else {
            return Err(format!("{} sequence parameter sets", sps.len()));
        };
        if vps.is_empty() || pps.is_empty() {
            return Err("missing a video or picture parameter set".into());
        }
        let s = filmcraft_hevc::params::Sps::parse(&unescape(sps_nal.get(2..).ok_or("empty SPS")?)).map_err(|e| e.to_string())?;
        s.check_supported().map_err(|e| e.to_string())?;
        if s.separate_colour_plane || s.chroma_format_idc != 1 || s.bit_depth_luma != s.bit_depth_chroma || !matches!(s.bit_depth_luma, 8 | 10) {
            return Err(format!("{}-bit chroma format {}", s.bit_depth_luma, s.chroma_format_idc));
        }
        let vui = s.vui.clone().unwrap_or_default();
        if vui.field_seq {
            return Err("field-coded HEVC".into());
        }
        let coded = (s.width, s.height);
        check_size(coded)?;
        let crop = crop_rect(coded, s.conf_win, (2, 2))?;
        let layout = Layout {
            bits: s.bit_depth_luma,
            full_range: vui.full_range,
            coded,
            crop,
            color: color(crop.2, crop.3, vui.matrix_coefficients, vui.transfer_characteristics, vui.full_range),
            par: par(vui.sar),
        };
        let parameter_sets: Vec<Vec<u8>> = vps.into_iter().chain(sps.iter().copied()).chain(pps).cloned().collect();
        if parameter_sets.iter().any(Vec::is_empty) {
            return Err("empty parameter set".into());
        }
        Ok(NalStream {
            codec: NalCodec::Hevc,
            length_size: usize::from(c.length_size),
            parameter_sets,
            reorder: (s.max_num_reorder as usize).min(16),
            highest_tid: Some(c.num_temporal_layers).filter(|&n| n > 0).map(|n| n - 1),
            layout,
            record: c.to_bytes(),
        })
        .and_then(NalStream::check_length_size)
    }

    fn check_length_size(self) -> std::result::Result<NalStream, String> {
        if matches!(self.length_size, 1 | 2 | 4) { Ok(self) } else { Err(format!("NAL length size {}", self.length_size)) }
    }

    /// FilmCraft's software decoder for this stream (made directly, not through the decoder
    /// registry, so the hardware factory is never asked again).
    pub fn software_decoder(&self) -> Result<Box<dyn VideoDecoder>> {
        Ok(match self.codec {
            NalCodec::H264 => Box::new(H264Decoder::new(self.record.clone())?),
            NalCodec::Hevc => Box::new(HevcDecoder::new(self.record.clone())?),
        })
    }

    /// The NAL unit types of a sample.
    pub fn nal_types(&self, sample: &[u8]) -> Vec<u8> {
        nal_units(sample, self.length_size)
            .filter_map(|n| n.first())
            .map(|&h| match self.codec {
                NalCodec::H264 => h & 0x1f,
                NalCodec::Hevc => (h >> 1) & 0x3f,
            })
            .collect()
    }

    /// The sample is a point decoding can restart from: an H.264 IDR, an HEVC IRAP picture.
    pub fn is_irap(&self, sample: &[u8]) -> bool {
        self.nal_types(sample).iter().any(|&t| match self.codec {
            NalCodec::H264 => t == 5,
            NalCodec::Hevc => (16..=23).contains(&t),
        })
    }

    /// An HEVC CRA picture: its leading (RASL) pictures reference pictures before it, so a replay
    /// must start at the restart point before it.
    pub fn is_cra(&self, sample: &[u8]) -> bool {
        self.codec == NalCodec::Hevc && self.nal_types(sample).contains(&21)
    }

    /// An HEVC CRA or BLA picture (its RASL pictures are skipped when decoding starts there).
    pub fn starts_with_rasl(&self, sample: &[u8]) -> bool {
        self.codec == NalCodec::Hevc && self.nal_types(sample).iter().any(|&t| matches!(t, 16..=18 | 21))
    }

    /// An HEVC RASL picture.
    pub fn is_rasl(&self, sample: &[u8]) -> bool {
        self.codec == NalCodec::Hevc && self.nal_types(sample).iter().any(|&t| matches!(t, 8 | 9))
    }

    /// The sample carries a parameter set that is not one of the record's (the hardware session
    /// was set up from those): a stream concatenated from two recordings, say.
    pub fn new_parameter_sets(&self, sample: &[u8]) -> bool {
        nal_units(sample, self.length_size).any(|n| {
            let t = match self.codec {
                NalCodec::H264 => n.first().map(|h| h & 0x1f),
                NalCodec::Hevc => n.first().map(|h| (h >> 1) & 0x3f),
            };
            let param = match self.codec {
                NalCodec::H264 => matches!(t, Some(7 | 8)),
                NalCodec::Hevc => matches!(t, Some(32..=34)),
            };
            param && !self.parameter_sets.iter().any(|p| p.as_slice() == n)
        })
    }

    /// The software decoder's answer: the sample can be left out without changing any other
    /// picture.
    pub fn is_disposable(&self, sample: &[u8]) -> bool {
        match self.codec {
            NalCodec::H264 => h264_disposable(sample, self.length_size),
            NalCodec::Hevc => hevc_disposable(sample, self.length_size, self.highest_tid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_units_split_and_stop_at_junk() {
        let s = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41, 0, 0, 0, 9, 1];
        let v: Vec<&[u8]> = nal_units(&s, 4).collect();
        assert_eq!(v, vec![&[0x65, 1][..], &[0x41][..]], "the truncated third unit is dropped");
        assert_eq!(nal_units(&s, 3).count(), 0, "a 3-byte length field is not a length size");
        assert_eq!(nal_units(&[], 4).count(), 0);
        assert_eq!(nal_units(&[0xff; 3], 4).count(), 0);
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape(&[1, 0, 0, 3, 1, 0, 0, 3, 0, 3]), vec![1, 0, 0, 1, 0, 0, 0, 3]);
    }

    #[test]
    fn crop_is_checked() {
        assert_eq!(crop_rect((1920, 1088), (0, 0, 0, 4), (2, 2)), Ok((0, 0, 1920, 1080)));
        assert!(crop_rect((16, 16), (0, 9, 0, 0), (2, 2)).is_err(), "crop wider than the picture");
        assert!(crop_rect((16, 16), (u32::MAX, 0, 0, 0), (2, 2)).is_err(), "overflow");
    }

    #[test]
    fn junk_records_are_declined_not_crashed() {
        let mut a = filmcraft_isobmff::AvcConfig::new(vec![vec![0x67, 0xff, 0xff, 0xff, 0xff]], vec![vec![0x68, 0xce]], 4);
        assert!(NalStream::h264(&a).is_err());
        a.sps = vec![vec![0x67]];
        assert!(NalStream::h264(&a).is_err());
        a.sps = vec![];
        assert!(NalStream::h264(&a).is_err());
        let h = filmcraft_isobmff::HevcConfig::default();
        assert!(NalStream::hevc(&h).is_err());
    }
}
