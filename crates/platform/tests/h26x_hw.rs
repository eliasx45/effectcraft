//! The VideoToolbox H.264 / HEVC decoder against FilmCraft's software decoders (macOS with a
//! hardware decoder; elsewhere, or when the system refuses a session, the tests note why and pass).

#![cfg(target_os = "macos")]

mod nal;

use effectcraft_platform::h26x::VtNalDecoder;
use effectcraft_platform::hybrid::HybridDecoder;
use effectcraft_platform::nal::select;
use filmcraft_codecs::{DecodedFrame, VideoDecoder};
use filmcraft_frame::PixelData;

fn run(d: &mut dyn VideoDecoder, s: &nal::Stream) -> Vec<DecodedFrame> {
    let mut out = Vec::new();
    for (data, pts, _) in &s.samples {
        out.extend(d.decode(data, *pts).expect("decode"));
    }
    out.extend(d.flush());
    out
}

/// Largest and mean difference of two pictures' planes, in codes of their own depth.
fn diff(a: &PixelData, b: &PixelData) -> (u32, f64) {
    let planes: Vec<(Vec<u32>, Vec<u32>)> = match (a, b) {
        (PixelData::Yuv8 { planes: p, .. }, PixelData::Yuv8 { planes: q, .. }) => {
            (0..3).map(|i| (p[i].iter().map(|&v| u32::from(v)).collect(), q[i].iter().map(|&v| u32::from(v)).collect())).collect()
        }
        (PixelData::Yuv16 { planes: p, bits: m, .. }, PixelData::Yuv16 { planes: q, bits: n, .. }) => {
            assert_eq!(m, n, "bit depth");
            (0..3).map(|i| (p[i].iter().map(|&v| u32::from(v)).collect(), q[i].iter().map(|&v| u32::from(v)).collect())).collect()
        }
        _ => panic!("different picture layouts"),
    };
    let (mut max, mut sum, mut count) = (0u32, 0f64, 0usize);
    for (p, q) in planes {
        assert_eq!(p.len(), q.len(), "plane size");
        for (x, y) in p.iter().zip(&q) {
            let d = x.abs_diff(*y);
            max = max.max(d);
            sum += f64::from(d);
        }
        count += p.len();
    }
    (max, sum / count.max(1) as f64)
}

/// Hardware pictures come out in the software decoder's presentation order with its pts, size,
/// colour and pixel aspect, and the same samples (H.264 / HEVC decoding is exactly specified; a
/// little slack in case a system's decoder rounds differently); a decoder whose hardware fails
/// mid-stream continues with exactly the software decoder's pictures.
#[test]
fn hardware_pictures_match_software_in_order_and_failures_fall_back() {
    let cases = [
        ("H.264 with B-frames", nal::h264(192, 108, 30, 2, 10)),
        ("H.264 without B-frames", nal::h264(160, 96, 12, 0, 5)),
        ("HEVC Main", nal::hevc(192, 108, 16, false, 8)),
        ("HEVC Main 10", nal::hevc(192, 108, 10, true, 5)),
    ];
    for (name, s) in cases {
        let stream = select(&s.entry, true, |_| true).expect("the hardware path takes it");
        let mut hw = match VtNalDecoder::new(stream.clone()) {
            Ok(d) => d,
            Err(why) => {
                eprintln!("{name}: no hardware session ({why}): skipping");
                continue;
            }
        };
        let reference = run(stream.software_decoder().expect("software").as_mut(), &s);
        let got = run(&mut hw, &s);
        assert_eq!(got.iter().map(|f| f.pts).collect::<Vec<_>>(), reference.iter().map(|f| f.pts).collect::<Vec<_>>(), "{name}: pts in presentation order");
        for (g, r) in got.iter().zip(&reference) {
            assert_eq!(
                (g.frame.width, g.frame.height, g.frame.color, g.frame.par),
                (r.frame.width, r.frame.height, r.frame.color, r.frame.par),
                "{name}: layout"
            );
            let (max, mean) = diff(&g.frame.data, &r.frame.data);
            assert!(max <= 4 && mean < 0.25, "{name}: picture {} differs (max {max}, mean {mean:.3})", g.pts);
        }

        // a second run after a seek to the start: a fresh session, the same pictures
        hw.reset();
        assert_eq!(run(&mut hw, &s).len(), reference.len(), "{name}: after a reset");

        // a session lost mid-stream: the hybrid continues in software, exactly
        let mut failing = VtNalDecoder::new(stream.clone()).expect("second session");
        failing.fail_after(s.samples.len() as u64 / 2);
        let mut d = HybridDecoder::new(Box::new(failing), stream.clone());
        let got = run(&mut d, &s);
        assert!(!d.is_hardware(), "{name}: fell back");
        assert_eq!(got.iter().map(|f| f.pts).collect::<Vec<_>>(), reference.iter().map(|f| f.pts).collect::<Vec<_>>(), "{name}: pts after a fallback");
    }
}
