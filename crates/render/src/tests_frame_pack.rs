//! Compressed viewer frames ([`crate::frame_pack`]) and the disk cache's use of them.

use std::sync::Arc;

use crate::disk_cache::{self, DiskCache, Kind};
use crate::frame_pack::{PackedFrame, STRIPS, is_packed_payload};

/// Premultiplied RGBA8 with flat runs, sparse noise and transparent holes.
fn frame(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for i in 0..w * h {
        let (x, y) = (i % w, i / w);
        let a: u8 = if (x / 5 + y / 3 + seed) % 4 == 0 { 0 } else { 255 };
        let noise = (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 24) as u8;
        let flat = ((x / 8 + seed) * 31 % 256) as u8;
        let g = if i % 3 == 0 { noise } else { flat };
        v.extend_from_slice(&[flat.min(a), g.min(a), (y as u8).min(a), a]);
    }
    v
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("effectcraft-frame-pack-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn frames_round_trip_exactly_on_any_number_of_threads() {
    for (w, h) in [(1, 1), (5, 3), (17, 16), (31, 17), (320, 180), (1920, 2)] {
        let rgba = frame(w, h, w ^ h);
        let p = PackedFrame::encode(w, h, &rgba).unwrap();
        assert_eq!((p.width(), p.height(), p.raw_len()), (w, h, rgba.len()));
        assert_eq!(p.strips(), STRIPS.min(h as usize));
        for threads in [1, 2, 4, 16, 64] {
            assert_eq!(p.decode(threads).unwrap(), rgba, "{w}×{h} on {threads} threads");
        }
        // Its payload is all it is.
        let again = PackedFrame::from_payload(p.payload().to_vec()).unwrap();
        assert_eq!(again, p);
        assert!(is_packed_payload(p.payload()));
    }
}

#[test]
fn compressed_frames_are_smaller() {
    let rgba = frame(320, 180, 7);
    let p = PackedFrame::encode(320, 180, &rgba).unwrap();
    assert!(p.bytes() < rgba.len(), "{} of {}", p.bytes(), rgba.len());
    // A flat frame (a title card's background) is tiny.
    let flat = vec![200u8; 320 * 180 * 4];
    assert!(PackedFrame::encode(320, 180, &flat).unwrap().bytes() < flat.len() / 50);
}

#[test]
fn bad_sizes_and_damaged_payloads_are_refused_not_panics() {
    let rgba = frame(16, 16, 1);
    assert!(PackedFrame::encode(16, 15, &rgba).is_none(), "pixels don't match the size");
    assert!(PackedFrame::encode(0, 16, &[]).is_none(), "empty");
    let p = PackedFrame::encode(16, 16, &rgba).unwrap();
    let good = p.payload().to_vec();
    // Truncated, extended, a strip length or the size changed, every byte flipped in turn.
    assert!(PackedFrame::from_payload(good[..good.len() - 1].to_vec()).is_none());
    let mut longer = good.clone();
    longer.push(0);
    assert!(PackedFrame::from_payload(longer).is_none());
    let mut huge = good.clone();
    huge[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(PackedFrame::from_payload(huge).is_none());
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x5a;
        // Either refused or decoded to something; never a panic or an out-of-bounds write.
        if let Some(p) = PackedFrame::from_payload(bad) {
            let mut out = vec![0u8; p.raw_len()];
            let _ = p.decode_into(&mut out, 3);
        }
    }
    let mut wrong = vec![0u8; good.len()];
    assert!(!p.decode_into(&mut wrong, 1), "output of the wrong size");
}

/// Frames written by an older version (one LZ4 block) still read, as pixels and as compressed
/// frames; new ones are written in strips.
#[test]
fn disk_frames_read_in_both_layouts() {
    let rgba = frame(40, 30, 3);
    // The older payload: width, height, LZ4 with the size prepended.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&40u32.to_le_bytes());
    legacy.extend_from_slice(&30u32.to_le_bytes());
    legacy.extend_from_slice(&lz4_flex::compress_prepend_size(&rgba));
    assert!(!is_packed_payload(&legacy));
    let entry = disk_cache::seal(Kind::Frame, &legacy);
    assert_eq!(disk_cache::read_frame_entry(&entry).unwrap().rgba, rgba);
    let new = disk_cache::frame_entry(40, 30, &rgba);
    assert_eq!(disk_cache::read_frame_entry(&new).unwrap().rgba, rgba);
    // On disk: a legacy file comes back compressed in strips too.
    let dir = tmpdir("layouts");
    let dc = DiskCache::open(&dir, 1 << 30).unwrap();
    let file = dir.join("v1").join("frames").join("00").join(disk_cache::entry_name(0x00ab));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, &entry).unwrap();
    dc.rescan();
    let p = dc.get_packed_frame(0x00ab).unwrap();
    assert_eq!(p.decode(2).unwrap(), rgba);
    assert_eq!(dc.get_frame(0x00ab).unwrap().rgba, rgba);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A compressed frame goes to disk as it is (no second compression) and comes back the same,
/// bytes and pixels; a frame already on disk isn't queued again.
#[test]
fn packed_frames_are_written_as_they_are() {
    let dir = tmpdir("put-packed");
    let dc = DiskCache::open(&dir, 1 << 30).unwrap();
    let rgba = frame(64, 48, 5);
    let p = Arc::new(PackedFrame::encode(64, 48, &rgba).unwrap());
    assert!(dc.put_packed_frame(7, p.clone()));
    dc.flush();
    assert!(dc.contains(Kind::Frame, 7));
    assert!(!dc.put_packed_frame(7, p.clone()), "on disk already");
    assert_eq!(dc.get_packed_frame(7).unwrap().payload(), p.payload());
    assert_eq!(dc.get_frame(7).unwrap().rgba, rgba);
    // Frames queued as pixels are written in strips by the writer.
    dc.put_frame(8, 64, 48, rgba.clone());
    dc.flush();
    assert_eq!(dc.get_packed_frame(8).unwrap().payload(), p.payload(), "one encoding, the same bytes");
    let _ = std::fs::remove_dir_all(&dir);
}
