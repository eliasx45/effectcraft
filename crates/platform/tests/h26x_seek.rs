//! Frame-accurate seeks through the media pool with H.264 / HEVC footage decoded on the hardware
//! (previews) against the same footage decoded in software: every frame, read in a scrambled
//! order (backwards, across GOPs, repeated), is the software decoder's frame at that time and not
//! its neighbour; the exact view (exports) never decodes on the hardware. Its own test binary: the
//! decoder counters are process-wide. Without a hardware decoder the hardware half is skipped.

mod nal;

use std::sync::Arc;

use effectcraft_media::MediaPool;
use effectcraft_time::Tick;

fn worst(a: &effectcraft_render::Image, b: &effectcraft_render::Image) -> f32 {
    a.data.iter().zip(&b.data).flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs())).fold(0f32, f32::max)
}

#[test]
fn hardware_previews_seek_to_the_same_frames_as_software() {
    let available = matches!(effectcraft_platform::register(), effectcraft_platform::Availability::Available(_));
    let n = 36usize;
    let cases = [("H.264 with B-frames", "h264", nal::h264(192, 108, n, 2, 12)), ("HEVC Main 10", "hevc", nal::hevc(192, 108, n, true, 12))];
    for (name, ext, s) in cases {
        let bytes: Arc<[u8]> = nal::movie(&s).into();
        let path = format!("/mem/hw-seek-{ext}.mov");
        let footage = effectcraft_media::probe_bytes(&path, bytes.clone()).expect("probe");
        assert!(effectcraft_media::hw::takes_codec(&footage.codec), "{name}: codec {}", footage.codec);
        let at = |i: usize| footage.frame_rate.tick_of(i as i64) + Tick(footage.frame_rate.frame_duration().0 / 2);

        let pool = MediaPool::new();
        pool.add_bytes(&path, bytes.clone());
        pool.set_hardware_decoding(true);
        let software = MediaPool::new();
        software.add_bytes(&path, bytes.clone());
        let want: Vec<_> = (0..n).map(|i| software.frame_at(&footage, at(i)).expect("software frame")).collect();
        // the synthetic picture changes every frame: a neighbour is clearly another frame
        for i in 1..n {
            assert!(worst(&want[i], &want[i - 1]) > 0.05, "{name}: frames {} and {i} look alike", i - 1);
        }

        let before = effectcraft_platform::stats();
        for i in [0, n - 1, 12, 11, 13, 5, 24, 23, 30, 1, 2, 3, 17, 17, 0, n - 2] {
            let got = pool.exact().frame_at(&footage, at(i)).expect("exact frame");
            assert_eq!(got.data, want[i].data, "{name}: exact view, frame {i}");
        }
        assert_eq!(effectcraft_platform::stats().decoders, before.decoders, "{name}: the exact view made no hardware decoder");

        if !available {
            eprintln!("{name}: no hardware video decoder: preview half skipped");
            continue;
        }
        let order = [0, n - 1, 12, 11, 13, 5, 24, 23, 30, 1, 2, 3, 17, 17, 0, n - 2, 6, 7, 8, 9, 10];
        for i in order {
            let got = pool.frame_at(&footage, at(i)).expect("preview frame");
            let d = worst(&got, &want[i]);
            assert!(d < 0.01, "{name}: preview frame {i} differs from the software frame by {d}");
            if let Some(next) = want.get(i + 1) {
                assert!(worst(&got, next) > 0.05, "{name}: preview frame {i} is frame {}", i + 1);
            }
            if let Some(prev) = i.checked_sub(1).and_then(|p| want.get(p)) {
                assert!(worst(&got, prev) > 0.05, "{name}: preview frame {i} is frame {}", i - 1);
            }
        }
        let after = effectcraft_platform::stats();
        assert!(after.decoders > before.decoders && after.frames > before.frames, "{name}: previews used the hardware: {before:?} -> {after:?}");
        assert_eq!(after.fallbacks, before.fallbacks, "{name}: no software fallback");
    }
}
