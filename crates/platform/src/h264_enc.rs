//! The VideoToolbox H.264 encoder behind `effectcraft_media::h264_hw` (safe code; E2b, 6 Nov 2026).
//!
//! [`open`] is the function [`crate::register`] installs: a hardware H.264 session
//! (`videotoolbox_h264`) for one movie. It declines (an error; the exporter then encodes in
//! software) sizes above [`MAX_SIDE`] or below 64 px and odd sizes, and a Mac without a hardware
//! H.264 encoder (the session can't be created).
//!
//! What the hardware writes is not FilmCraft's stream: Apple's rate control and mode decisions,
//! High profile without B-frames. That is why exports use it only when asked
//! (`effectcraft-cli render --hw-h264` / `--fast`).

use effectcraft_media::h264_hw::{H264Config, H264Sample, H264Session};

use crate::prores_pack;
use crate::videotoolbox_h264::{Params, Session};

/// Largest picture side the hardware path takes.
pub const MAX_SIDE: u32 = 4096;
/// Smallest (VideoToolbox's H.264 encoder refuses tiny pictures).
pub const MIN_SIDE: u32 = 64;

struct VtH264 {
    s: Session,
    w: usize,
    h: usize,
    ps: Option<(Vec<u8>, Vec<u8>)>,
}

/// Open a hardware H.264 session for `cfg` (an `effectcraft_media::h264_hw::OpenH264`).
pub fn open(cfg: &H264Config) -> Result<Box<dyn H264Session>, String> {
    let (w, h) = (cfg.width, cfg.height);
    if !(MIN_SIDE..=MAX_SIDE).contains(&w) || !(MIN_SIDE..=MAX_SIDE).contains(&h) || w % 2 == 1 || h % 2 == 1 {
        return Err(format!("{w}x{h} is beyond the hardware H.264 encoder"));
    }
    if cfg.fps_num == 0 || cfg.fps_den == 0 {
        return Err("no frame rate".into());
    }
    let p = Params {
        width: w,
        height: h,
        fps_num: cfg.fps_num,
        fps_den: cfg.fps_den,
        bits_per_second: i64::from(cfg.kbps).saturating_mul(1000),
        keyint: cfg.keyint,
    };
    let s = Session::new(p)?;
    Ok(Box::new(VtH264 { s, w: w as usize, h: h as usize, ps: None }))
}

impl VtH264 {
    fn drain(&mut self) -> Result<Vec<H264Sample>, String> {
        let mut out = Vec::new();
        for r in self.s.take() {
            let c = r?;
            let key = effectcraft_media::h264_hw::has_idr(&c.data);
            if let Some(ps) = c.ps {
                match &self.ps {
                    None => self.ps = Some(ps),
                    Some(have) if *have != ps => return Err("the hardware encoder changed its parameter sets mid-stream".into()),
                    Some(_) => {}
                }
            }
            out.push(H264Sample { data: c.data, key, index: c.index });
        }
        Ok(out)
    }
}

impl H264Session for VtH264 {
    fn encode(&mut self, y: &[u8], u: &[u8], v: &[u8], index: u64) -> Result<Vec<H264Sample>, String> {
        let (w, h) = (self.w, self.h);
        self.s.submit(index, &mut |planes| prores_pack::pack_nv12(y, u, v, w, h, planes))?;
        self.drain()
    }
    fn complete(&mut self) -> Result<Vec<H264Sample>, String> {
        self.s.complete()?;
        self.drain()
    }
    fn parameter_sets(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.ps.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a Mac with a hardware H.264 encoder: a session codes a moving picture in order, with
    /// key frames at the interval and SPS / PPS, and goes on after `complete`. Elsewhere `open`
    /// is an error, not a panic. (The decoded export is checked end to end with ffprobe / ffmpeg.)
    #[test]
    fn hardware_h264_decodes_in_software() {
        let (w, h, n) = (320usize, 240usize, 24u64);
        let cfg = H264Config { width: w as u32, height: h as u32, fps_num: 30, fps_den: 1, kbps: 4000, keyint: 12 };
        let Ok(mut s) = open(&cfg) else {
            eprintln!("no hardware H.264 encoder: skipped");
            return;
        };
        let (cw, ch) = (w / 2, h / 2);
        let mut samples = Vec::new();
        let mut pictures = Vec::new();
        for k in 0..n {
            let y: Vec<u8> = (0..w * h).map(|i| (16 + ((i % w) + (i / w) + k as usize * 4) % 200) as u8).collect();
            let (u, v) = (vec![128u8; cw * ch], vec![110u8; cw * ch]);
            samples.extend(s.encode(&y, &u, &v, k).unwrap());
            pictures.push(y);
        }
        samples.extend(s.complete().unwrap());
        assert_eq!(samples.len() as u64, n);
        assert!(samples.iter().enumerate().all(|(i, s)| s.index == i as u64), "in order");
        assert!(samples[0].key && samples[12].key, "key frames at 0 and the interval");
        let (sps, pps) = s.parameter_sets().expect("parameter sets");
        assert_eq!(sps.first().map(|b| b & 0x1f), Some(7));
        assert_eq!(pps.first().map(|b| b & 0x1f), Some(8));
        // more frames after a complete(): the session goes on
        let more = s.encode(&pictures[0], &vec![128u8; cw * ch], &vec![110u8; cw * ch], n).unwrap();
        let more = [more, s.complete().unwrap()].concat();
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].index, n);
        // a size it doesn't take
        assert!(open(&H264Config { width: 33, ..cfg }).is_err());
    }
}
