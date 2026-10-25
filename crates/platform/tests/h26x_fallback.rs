//! H.264 / HEVC hardware decoding, the parts that need no hardware (every system): which streams
//! go to the hardware decoder, and the hybrid decoder's switch to software mid-stream giving
//! exactly the software decoder's pictures. The hardware is played by the software decoder with a
//! failure injected after `n` samples.

mod nal;

use effectcraft_platform::hybrid::HybridDecoder;
use effectcraft_platform::nal::{Decline, NalCodec, NalStream, select};
use filmcraft_codecs::{CodecError, DecodedFrame, Result, VideoDecoder};
use filmcraft_isobmff::{FourCc, SampleEntry};

/// A stand-in for a hardware decoder: the software decoder, failing from the `n`-th sample on.
struct Failing {
    inner: Box<dyn VideoDecoder>,
    fail_from: usize,
    fed: usize,
}

impl VideoDecoder for Failing {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        self.fed += 1;
        if self.fed > self.fail_from {
            return Err(CodecError::Decode("session lost (test)".into()));
        }
        self.inner.decode(sample, pts)
    }
    fn flush(&mut self) -> Vec<DecodedFrame> {
        if self.fed > self.fail_from { Vec::new() } else { self.inner.flush() }
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn name(&self) -> &str {
        "fake hardware"
    }
}

fn stream_of(s: &nal::Stream) -> NalStream {
    select(&s.entry, true, |_| true).expect("the hardware path takes the stream")
}

/// Same samples in the same layout (8- or 16-bit planar YUV).
fn same(a: &filmcraft_frame::PixelData, b: &filmcraft_frame::PixelData) -> bool {
    use filmcraft_frame::PixelData::{Yuv8, Yuv16};
    match (a, b) {
        (Yuv8 { planes: p, chroma: c, alpha: x }, Yuv8 { planes: q, chroma: d, alpha: y }) => p == q && c == d && x == y,
        (Yuv16 { planes: p, chroma: c, bits: m, alpha: x }, Yuv16 { planes: q, chroma: d, bits: n, alpha: y }) => p == q && c == d && m == n && x == y,
        _ => false,
    }
}

/// Decode every sample in order and flush: (pts, planes) in output order.
fn run(d: &mut dyn VideoDecoder, s: &nal::Stream) -> Vec<(i64, filmcraft_frame::PixelData)> {
    let mut out = Vec::new();
    for (data, pts, _) in &s.samples {
        out.extend(d.decode(data, *pts).expect("decode"));
    }
    out.extend(d.flush());
    out.into_iter().map(|f| (f.pts, f.frame.data)).collect()
}

#[test]
fn only_preview_reads_of_supported_h264_and_hevc_go_to_the_hardware() {
    let h264 = nal::h264(64, 48, 4, 2, 12);
    let hevc = nal::hevc(64, 48, 2, false, 12);
    let hevc10 = nal::hevc(64, 48, 2, true, 12);

    assert_eq!(select(&h264.entry, false, |_| true).err(), Some(Decline::NotPreview), "exports decode exactly");
    assert_eq!(select(&h264.entry, true, |_| false).err(), Some(Decline::NoHardware));
    assert_eq!(select(&hevc.entry, true, |c| c == NalCodec::H264).err(), Some(Decline::NoHardware), "asked per codec");
    let prores = SampleEntry::prores(FourCc(*b"apcn"), 64, 48);
    assert_eq!(select(&prores, true, |_| true).err(), Some(Decline::OtherCodec), "ProRes has its own factory");

    let s = select(&h264.entry, true, |_| true).expect("8-bit H.264");
    assert_eq!((s.codec, s.layout.bits, s.layout.crop), (NalCodec::H264, 8, (0, 0, 64, 48)));
    assert_eq!(s.parameter_sets.len(), 2, "SPS + PPS");
    assert!(s.reorder >= 1, "B-frames reorder: depth {}", s.reorder);
    let s = select(&hevc.entry, true, |_| true).expect("HEVC Main");
    assert_eq!((s.codec, s.layout.bits, s.parameter_sets.len()), (NalCodec::Hevc, 8, 3));
    let s = select(&hevc10.entry, true, |_| true).expect("HEVC Main 10");
    assert_eq!(s.layout.bits, 10);

    // A 1080p H.264 stream is coded 1088 high and cropped to 1080, like the software decoder.
    let tall = nal::h264(64, 40, 1, 0, 12);
    let s = stream_of(&tall);
    assert_eq!((s.layout.coded, s.layout.crop), ((64, 48), (0, 0, 64, 40)));

    // The colour and pixel aspect match the software decoder's pictures.
    let mut sw = s.software_decoder().expect("software");
    let (data, pts, _) = &tall.samples[0];
    let mut out = sw.decode(data, *pts).expect("decode");
    out.extend(sw.flush());
    let f = &out[0].frame;
    assert_eq!((f.width, f.height, f.color, f.par), (64, 40, s.layout.color, s.layout.par));
}

#[test]
fn a_mid_stream_failure_continues_with_exactly_the_software_pictures() {
    let cases =
        [("H.264 with B-frames", nal::h264(96, 64, 30, 2, 10)), ("HEVC", nal::hevc(96, 64, 20, false, 8)), ("HEVC Main 10", nal::hevc(96, 64, 12, true, 5))];
    for (name, s) in cases {
        let stream = stream_of(&s);
        let reference = run(stream.software_decoder().expect("software").as_mut(), &s);
        assert_eq!(reference.len(), s.samples.len(), "{name}: every frame decodes");
        let mut sorted: Vec<i64> = reference.iter().map(|r| r.0).collect();
        sorted.sort_unstable();
        assert_eq!(sorted, reference.iter().map(|r| r.0).collect::<Vec<_>>(), "{name}: presentation order");
        for fail_from in 0..=s.samples.len() {
            let hw = Failing { inner: stream.software_decoder().expect("software"), fail_from, fed: 0 };
            let mut d = HybridDecoder::new(Box::new(hw), stream.clone());
            let got = run(&mut d, &s);
            assert_eq!(d.is_hardware(), fail_from >= s.samples.len(), "{name}: fell back after {fail_from}");
            assert_eq!(got.len(), reference.len(), "{name}, failing from sample {fail_from}: picture count");
            for ((p, data), (q, want)) in got.iter().zip(&reference) {
                assert_eq!(p, q, "{name}, failing from sample {fail_from}: pts order");
                assert!(same(data, want), "{name}, failing from sample {fail_from}: picture {p} differs from the software decoder's");
            }
        }
    }
}

#[test]
fn a_seek_after_a_fallback_decodes_the_same_frames() {
    let s = nal::h264(96, 64, 30, 2, 10);
    let stream = stream_of(&s);
    let reference = run(stream.software_decoder().expect("software").as_mut(), &s);
    let hw = Failing { inner: stream.software_decoder().expect("software"), fail_from: 7, fed: 0 };
    let mut d = HybridDecoder::new(Box::new(hw), stream.clone());
    let _ = run(&mut d, &s);
    assert!(!d.is_hardware());
    // seek back to the start (the GOP logic resets and decodes forward from a sync sample)
    d.reset();
    assert_eq!(run(&mut d, &s).iter().map(|r| r.0).collect::<Vec<_>>(), reference.iter().map(|r| r.0).collect::<Vec<_>>());
    // and to the second IDR
    let idr = s.samples.iter().position(|x| x.2 && x.1 > 0).expect("a second IDR");
    d.reset();
    let mut out = Vec::new();
    for (data, pts, _) in &s.samples[idr..] {
        out.extend(d.decode(data, *pts).expect("decode"));
    }
    out.extend(d.flush());
    let first = s.samples[idr].1;
    let want: Vec<i64> = reference.iter().map(|r| r.0).filter(|&p| p >= first).collect();
    assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), want, "frames from the IDR on, in order");
}

#[test]
fn in_band_parameter_sets_switch_to_software_only_when_they_differ() {
    let s = nal::h264(64, 48, 6, 0, 3);
    let stream = stream_of(&s);
    let sps = stream.parameter_sets[0].clone();
    let with = |nal: &[u8], sample: &[u8]| {
        let mut v = (nal.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(nal);
        v.extend_from_slice(sample);
        v
    };
    let first = &s.samples[0].0;
    assert!(!stream.new_parameter_sets(&with(&sps, first)), "a repeated SPS is the same stream");
    let mut other = sps.clone();
    if let Some(b) = other.last_mut() {
        *b ^= 0x40;
    }
    assert!(stream.new_parameter_sets(&with(&other, first)), "a different SPS is not");
    assert!(!stream.new_parameter_sets(first));

    // Repeated identical parameter sets keep the hardware decoder.
    let hw = Failing { inner: stream.software_decoder().expect("software"), fail_from: usize::MAX, fed: 0 };
    let mut d = HybridDecoder::new(Box::new(hw), stream.clone());
    for (data, pts, key) in &s.samples {
        let data = if *key { with(&sps, data) } else { data.clone() };
        d.decode(&data, *pts).expect("decode");
    }
    assert!(d.is_hardware());
}

#[test]
fn junk_samples_never_panic_the_nal_helpers() {
    let s = nal::h264(64, 48, 3, 0, 3);
    let stream = stream_of(&s);
    let junk: [&[u8]; 5] = [&[], &[0, 0, 0], &[0xff; 9], &[0, 0, 0, 1, 0x65], &[0, 0, 0, 200, 0x67, 1]];
    for j in junk {
        let _ = stream.nal_types(j);
        let _ = (stream.is_irap(j), stream.is_cra(j), stream.is_rasl(j), stream.new_parameter_sets(j), stream.is_disposable(j));
    }
}
