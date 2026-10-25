//! Generated H.264 / HEVC test movies (no committed footage): a bar that moves every frame over a
//! gradient, so neighbouring frames differ clearly and a wrong frame is caught.

#![allow(dead_code)]

use filmcraft_isobmff::{Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};

/// An encoded stream: its sample entry and samples in decoding order with their pts (track
/// timescale 24, one tick per frame).
pub struct Stream {
    pub entry: SampleEntry,
    pub samples: Vec<(Vec<u8>, i64, bool)>,
    /// Edit-list start (B-frame delay), as FilmCraft's export writes it.
    pub media_start: Option<i64>,
}

/// Frame `i` of `n` as 8-bit 4:2:0 planes.
pub fn picture(w: usize, h: usize, i: usize, n: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bar = (i * w) / n.max(1);
    let mut y = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            let base = 40 + (c * 120 / w.max(1)) + (r * 40 / h.max(1));
            let on = c >= bar && c < bar + w / 8;
            y[r * w + c] = if on { 230 } else { base as u8 };
        }
    }
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for r in 0..ch {
        for c in 0..cw {
            u[r * cw + c] = (90 + (c * 80 / cw.max(1)) + i % 7) as u8;
            v[r * cw + c] = (160 - (r * 60 / ch.max(1))) as u8;
        }
    }
    (y, u, v)
}

/// H.264 High, 8-bit 4:2:0, `bframes` B-frames between references, an IDR every `keyint` frames.
pub fn h264(w: u32, h: u32, n: usize, bframes: u8, keyint: u32) -> Stream {
    let mut cfg = filmcraft_h264enc::EncoderConfig::new(w, h, 24, 1);
    cfg.format = filmcraft_h264enc::PacketFormat::LengthPrefixed;
    cfg.aud = false;
    cfg.bframes = bframes;
    cfg.keyint = keyint;
    cfg.scenecut = false;
    cfg.threads = 1;
    cfg.rate = filmcraft_h264enc::RateControl::Crf(16.0);
    let mut enc = filmcraft_h264enc::Encoder::new(cfg).expect("encoder");
    let mut packets = Vec::new();
    for i in 0..n {
        let (y, u, v) = picture(w as usize, h as usize, i, n);
        let f = filmcraft_h264enc::YuvFrame { y: &y, u: &u, v: &v, y_stride: w as usize, uv_stride: (w as usize).div_ceil(2) };
        packets.extend(enc.try_encode(&f, i as i64).expect("encode"));
    }
    packets.extend(enc.flush());
    let avcc = filmcraft_isobmff::AvcConfig::parse(&enc.avcc()).expect("avcC");
    let entry = SampleEntry::avc(avcc, w as u16, h as u16);
    let samples = packets.into_iter().map(|p| (p.data, p.pts, p.keyframe)).collect::<Vec<_>>();
    Stream { entry, samples, media_start: (enc.delay() > 0).then_some(1) }
}

/// HEVC Main (8-bit) or Main 10, I and P pictures, an IDR every `keyint` frames.
pub fn hevc(w: u32, h: u32, n: usize, ten_bit: bool, keyint: u32) -> Stream {
    let mut cfg = effectcraft_hevcenc::EncoderConfig::new(w, h, 24, 1);
    cfg.profile = if ten_bit { effectcraft_hevcenc::Profile::Main10 } else { effectcraft_hevcenc::Profile::Main };
    cfg.keyint = keyint;
    cfg.rate = effectcraft_hevcenc::RateControl::ConstantQp(22);
    let mut enc = effectcraft_hevcenc::Encoder::new(cfg).expect("encoder");
    let shift = if ten_bit { 2 } else { 0 };
    let mut samples = Vec::new();
    for i in 0..n {
        let (y, u, v) = picture(w as usize, h as usize, i, n);
        let wide = |p: &[u8]| p.iter().map(|&s| u16::from(s) << shift).collect::<Vec<u16>>();
        let (y, u, v) = (wide(&y), wide(&u), wide(&v));
        let f = effectcraft_hevcenc::Frame { y: &y, u: &u, v: &v, y_stride: w as usize, uv_stride: (w as usize).div_ceil(2) };
        let p = enc.encode(&f);
        samples.push((p.data, i as i64, p.keyframe));
    }
    let hvcc = filmcraft_isobmff::HevcConfig::parse(&enc.hvcc()).expect("hvcC");
    Stream { entry: SampleEntry::hevc(hvcc, w as u16, h as u16), samples, media_start: None }
}

/// The stream as a MOV (24 fps).
pub fn movie(s: &Stream) -> Vec<u8> {
    let mut mux = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).expect("writer");
    let mut tc = TrackConfig::new(s.entry.clone(), 24);
    tc.media_start = s.media_start;
    let track = mux.add_track(tc).expect("track");
    // decode times are 0, 1, 2, … (minus the edit's start); the composition offset carries the pts
    let start = s.media_start.unwrap_or(0);
    for (k, (data, pts, key)) in s.samples.iter().enumerate() {
        let cto = (pts + start - k as i64) as i32;
        mux.write_sample(track, WriteSample { data, duration: 1, composition_offset: cto, is_sync: *key }).expect("sample");
    }
    mux.finish().expect("finish").into_inner()
}
