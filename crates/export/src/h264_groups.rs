//! H.264 in parallel keyframe groups (E2a, 6 Nov 2026).
//!
//! One FilmCraft H.264 encoder per movie was about half the time of a GPU H.264 export: its slice
//! threads don't scale (one encoder on 8 threads ≈ 25 fps on a 1080p motion-graphics comp, eight
//! one-thread encoders ≈ 45 fps together). The movie already has a key frame every
//! [`crate::encode::h264_keyint`] frames (2 s), and the encoder closes every group of pictures at
//! a key frame (B-frames never reach across an IDR), so each group is an independent stream: here
//! every group of `keyint` frames goes to an encoder of its own, several run at once on threads of
//! their own, and their samples are joined in order into one stream.
//!
//! - Every group's encoder has the same configuration, so the same SPS / PPS (checked) and one
//!   sample entry. Each starts with an IDR and ends its last B-frames before the group ends.
//! - Decode times stay one sample duration apart across groups; composition offsets are worked
//!   out from each picture's presentation time and its place in the joined stream, so a group
//!   that codes fewer frames than its B-frame delay (a one-frame last group) is still timed right.
//! - Rate control starts again in each group (each aims at the movie's bitrate on its own).
//! - Frames wait for their group's encoder as 4:2:0 pictures; at most [`QUEUE_BUDGET`] of them
//!   (groups at once = budget / group size, at most one per core). A movie of one group encodes
//!   exactly as the single encoder did.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use filmcraft_h264enc::{EncoderConfig, YuvFrame};
use filmcraft_isobmff::SampleEntry;
use rayon::prelude::*;

use crate::encode::{Packet, VideoEncoder, rgba_to_yuv420};
use crate::{ExportError, Result};

/// Bytes of pictures that may wait for the group encoders.
pub(crate) const QUEUE_BUDGET: u64 = 1 << 30;
/// Overrides the number of groups encoding at once (a whole number of at least 1).
pub(crate) const GROUPS_ENV: &str = "EFFECTCRAFT_H264_GROUPS";
/// Overrides each group encoder's threads (0: all cores).
pub(crate) const THREADS_ENV: &str = "EFFECTCRAFT_H264_GROUP_THREADS";

/// How many groups encode at once, and each encoder's threads (0: all cores, as the single
/// encoder). Every group encoder gets all cores by default: the OS shares them while several
/// run, and the last group, which finishes alone after the render has ended, gets the whole
/// machine (measured on C0666 MAIN 100–110 s, GPU: 11.0–14.2 s against 12.9–14.7 s with
/// cores / groups threads each; the group encoders then also cut the same slices as the single
/// encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub lanes: usize,
    pub threads: usize,
}

/// The [`Plan`] for `total` frames of `w`×`h` in groups of `keyint` on `cores` cores, the
/// waiting pictures within `budget` bytes.
pub(crate) fn plan(w: u32, h: u32, keyint: u64, total: u64, cores: usize, budget: u64) -> Plan {
    let keyint = keyint.max(1);
    let groups = total.div_ceil(keyint).max(1);
    let group_bytes = yuv_bytes(w, h).saturating_mul(keyint).max(1);
    let by_memory = (budget / group_bytes).max(1);
    let lanes = usize::try_from(groups.min(by_memory)).unwrap_or(usize::MAX).min(cores.max(1)).max(1);
    Plan { lanes, threads: 0 }
}

/// [`plan`] with the `EFFECTCRAFT_H264_GROUPS` / `EFFECTCRAFT_H264_GROUP_THREADS` overrides.
pub(crate) fn plan_with_env(p: Plan, groups: Option<&str>, threads: Option<&str>) -> Plan {
    let lanes = groups.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n >= 1).unwrap_or(p.lanes);
    let threads = threads.and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(p.threads);
    Plan { lanes, threads }
}

fn yuv_bytes(w: u32, h: u32) -> u64 {
    let (w, h) = (u64::from(w), u64::from(h));
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

fn enc_err(e: impl std::fmt::Display) -> ExportError {
    ExportError::Encode(e.to_string())
}

/// A 4:2:0 picture waiting for its group's encoder.
struct Yuv {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

type Coded = Result<Vec<filmcraft_h264enc::Packet>>;

/// One group's encoder thread: `tx` feeds it (dropped once the group has all its frames).
struct Lane {
    tx: Option<Sender<(u64, Yuv)>>,
    handle: JoinHandle<Coded>,
}

/// The H.264 [`VideoEncoder`] of a movie in parallel keyframe groups.
pub(crate) struct GroupedH264 {
    cfg: EncoderConfig,
    avcc: Vec<u8>,
    w: u32,
    h: u32,
    /// Frame duration in the track timescale (the rate's denominator).
    den: i64,
    keyint: u64,
    lanes: usize,
    /// Groups encoding, oldest first; the last one is being fed.
    running: VecDeque<Lane>,
    /// The first frame of the group being fed.
    open: Option<u64>,
    /// Samples handed out so far (their place in the joined stream).
    emitted: i64,
    /// The B-frame delay in frames (decode times start this many frames before 0).
    delay: i64,
    /// Tells the group threads to give up (the export stopped).
    stop: Arc<AtomicBool>,
}

impl GroupedH264 {
    /// Groups of `cfg.keyint` frames, `plan.lanes` at once, each encoder on `plan.threads`.
    pub(crate) fn new(cfg: EncoderConfig, den: i64, plan: Plan) -> Result<GroupedH264> {
        let mut cfg = cfg;
        cfg.threads = plan.threads;
        // (the parameter sets don't depend on the threads; a one-thread probe makes no pool)
        let probe = filmcraft_h264enc::Encoder::new(EncoderConfig { threads: 1, ..cfg.clone() }).map_err(enc_err)?;
        let (avcc, delay) = (probe.avcc(), i64::from(probe.delay() > 0));
        drop(probe);
        Ok(GroupedH264 {
            w: cfg.width,
            h: cfg.height,
            keyint: u64::from(cfg.keyint.max(1)),
            cfg,
            avcc,
            den,
            lanes: plan.lanes.max(1),
            running: VecDeque::new(),
            open: None,
            emitted: 0,
            delay,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start the group whose first frame is `first`: the group fed so far has all its frames;
    /// with every lane busy, wait for the oldest group and hand its samples out first.
    fn start(&mut self, first: u64, out: &mut Vec<Packet>) -> Result<()> {
        if let Some(l) = self.running.back_mut() {
            l.tx = None;
        }
        while self.running.len() >= self.lanes {
            self.join_front(out)?;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (cfg, want, den, stop) = (self.cfg.clone(), self.avcc.clone(), self.den, self.stop.clone());
        let handle = std::thread::Builder::new()
            .name("h264-group".into())
            .spawn(move || encode_group(cfg, &want, den, rx, &stop))
            .map_err(|e| ExportError::Encode(format!("cannot start an H.264 encoder thread: {e}")))?;
        self.running.push_back(Lane { tx: Some(tx), handle });
        self.open = Some(first);
        Ok(())
    }

    /// Wait for the oldest group and hand out its samples.
    fn join_front(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        let Some(mut lane) = self.running.pop_front() else { return Ok(()) };
        lane.tx = None;
        let coded = lane.handle.join().map_err(|_| ExportError::Encode("an H.264 encoder thread failed".into()))??;
        self.hand_out(coded, out)
    }

    /// The samples of the next group in the joined stream: decode time = place × duration less the
    /// B-frame delay, composition offset = presentation time − decode time.
    fn hand_out(&mut self, coded: Vec<filmcraft_h264enc::Packet>, out: &mut Vec<Packet>) -> Result<()> {
        out.reserve(coded.len());
        for p in coded {
            let dts = self.emitted.saturating_sub(self.delay).saturating_mul(self.den);
            let cto = i32::try_from(p.pts.saturating_sub(dts)).map_err(|_| ExportError::Encode("H.264 composition offset out of range".into()))?;
            out.push(Packet { data: p.data, key: p.keyframe, cto });
            self.emitted = self.emitted.saturating_add(1);
        }
        Ok(())
    }

    /// Wait for every group that has all its frames and hand out their samples, in order.
    fn harvest(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        while self.running.front().is_some_and(|l| l.tx.is_none()) {
            self.join_front(out)?;
        }
        Ok(())
    }
}

/// Encode one group: the frames `rx` brings until it closes, then the rest the encoder holds.
fn encode_group(cfg: EncoderConfig, want: &[u8], den: i64, rx: Receiver<(u64, Yuv)>, stop: &AtomicBool) -> Coded {
    let (w, cw) = (cfg.width as usize, (cfg.width as usize).div_ceil(2));
    let mut enc = filmcraft_h264enc::Encoder::new(cfg).map_err(enc_err)?;
    if enc.avcc() != want {
        return Err(ExportError::Encode("H.264 keyframe groups would have different parameter sets".into()));
    }
    let mut out = Vec::new();
    for (i, p) in rx {
        if stop.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let frame = YuvFrame { y: &p.y, u: &p.u, v: &p.v, y_stride: w, uv_stride: cw };
        let pts = i64::try_from(i).unwrap_or(i64::MAX).saturating_mul(den);
        out.extend(enc.try_encode(&frame, pts).map_err(enc_err)?);
    }
    out.extend(enc.flush());
    Ok(out)
}

impl Drop for GroupedH264 {
    fn drop(&mut self) {
        // (an export that stopped early: the group threads end at their next frame)
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl VideoEncoder for GroupedH264 {
    fn sample_entry(&self) -> SampleEntry {
        crate::encode::avc_entry(&self.avcc, None, self.w, self.h)
    }
    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        self.encode_batch(std::slice::from_ref(&rgba.to_vec()), index)
    }
    fn encode_batch(&mut self, frames: &[Vec<u8>], first: u64) -> Result<Vec<Packet>> {
        let (w, h) = (self.w as usize, self.h as usize);
        let pictures: Vec<Yuv> = frames
            .par_iter()
            .map(|px| {
                let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
                rgba_to_yuv420(px, w, h, &mut y, &mut u, &mut v);
                Yuv { y, u, v }
            })
            .collect();
        let mut out = Vec::new();
        for (k, p) in pictures.into_iter().enumerate() {
            let i = first.saturating_add(k as u64);
            let group = i / self.keyint * self.keyint;
            if self.open != Some(group) {
                self.start(group, &mut out)?;
            }
            let sent = self.running.back().and_then(|l| l.tx.as_ref()).is_some_and(|tx| tx.send((i, p)).is_ok());
            if !sent {
                // the group's encoder stopped early: its error
                if let Some(l) = self.running.back_mut() {
                    l.tx = None;
                }
                self.harvest(&mut out)?;
                return Err(ExportError::Encode("an H.264 encoder stopped early".into()));
            }
        }
        // (groups are handed out only when a lane is needed or at the end, never as they happen to
        // finish: the file's layout, where the audio goes between the samples, is the same every run)
        Ok(out)
    }
    fn flush(&mut self) -> Result<Vec<Packet>> {
        if let Some(l) = self.running.back_mut() {
            l.tx = None;
        }
        let mut out = Vec::new();
        self.harvest(&mut out)?;
        Ok(out)
    }
    fn media_start(&self) -> Option<i64> {
        (self.delay > 0).then_some(self.den)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_at_once_follow_cores_and_memory() {
        let gib = 1 << 30;
        // 1080p, 2 s groups at 29.97: ~187 MB a group, five fit 1 GiB
        assert_eq!(plan(1920, 1080, 60, 300, 10, gib), Plan { lanes: 5, threads: 0 });
        // fewer groups than that
        assert_eq!(plan(1920, 1080, 60, 120, 10, gib), Plan { lanes: 2, threads: 0 });
        // one group: the single encoder (all cores)
        assert_eq!(plan(1920, 1080, 60, 60, 10, gib), Plan { lanes: 1, threads: 0 });
        assert_eq!(plan(1920, 1080, 60, 0, 10, gib), Plan { lanes: 1, threads: 0 });
        // 4K: ~746 MB a group, one fits
        assert_eq!(plan(3840, 2160, 60, 600, 10, gib).lanes, 1);
        // small frames: one per core
        assert_eq!(plan(640, 360, 60, 6000, 8, gib), Plan { lanes: 8, threads: 0 });
        // never zero
        assert_eq!(plan(16, 16, 0, 10, 0, 0).lanes, 1);
    }

    /// `n` RGBA frames of `w`×`h`: a grey ramp that brightens and a square that moves each frame.
    fn frames(n: u64, w: usize, h: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|k| {
                let mut px = vec![0u8; w * h * 4];
                for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (i % w, i / w);
                    let inside = (x + 64 - (k as usize * 3) % 64) % 64 < 12 && y > h / 3 && y < 2 * h / 3;
                    let g = ((x * 2 + y + k as usize * 5) % 200) as u8 + 30;
                    *p = if inside { [230, 40, 40, 255] } else { [g, g, g / 2, 255] };
                }
                px
            })
            .collect()
    }

    /// Encode `frames` in batches of `batch` and flush.
    fn encode_all(e: &mut dyn VideoEncoder, frames: &[Vec<u8>], batch: usize) -> Vec<Packet> {
        let mut out = Vec::new();
        for (b, chunk) in frames.chunks(batch).enumerate() {
            out.extend(e.encode_batch(chunk, (b * batch) as u64).unwrap());
        }
        out.extend(e.flush().unwrap());
        out
    }

    #[test]
    fn groups_join_into_one_timed_stream() {
        let (w, h) = (64usize, 48usize);
        let rate = effectcraft_time::FrameRate::new(10, 1);
        let mut cfg = crate::encode::h264_config(w as u32, h as u32, rate, 400);
        cfg.keyint = 5;
        // groups of 5, 5, 5 and a last group of one frame (shorter than the B-frame delay)
        let pics = frames(16, w, h);
        for (lanes, batch) in [(3, 3), (2, 7), (8, 16)] {
            let mut g = GroupedH264::new(cfg.clone(), rate.den, Plan { lanes, threads: 1 }).unwrap();
            assert_eq!(g.media_start(), Some(rate.den), "one frame of B-frame delay");
            let ps = encode_all(&mut g, &pics, batch);
            assert_eq!(ps.len(), 16, "lanes {lanes}");
            // every group starts with a sync sample
            for j in [0, 5, 10, 15] {
                assert!(ps[j].key, "lanes {lanes}: sample {j} is not a key frame");
            }
            // presentation times (decode time + offset, less the edit) are each frame once
            let mut pts: Vec<i64> = ps
                .iter()
                .enumerate()
                .map(|(j, p)| {
                    assert!(p.cto >= 0, "negative composition offset at {j}");
                    (j as i64 - 1) * rate.den + i64::from(p.cto)
                })
                .collect();
            pts.sort_unstable();
            assert_eq!(pts, (0..16).collect::<Vec<i64>>(), "lanes {lanes}");
            // a sample entry with the single encoder's parameter sets
            let one = filmcraft_h264enc::Encoder::new(EncoderConfig { threads: 1, ..cfg.clone() }).unwrap();
            assert_eq!(g.avcc, one.avcc());
        }
        // The first group is exactly what one encoder writes for those frames (same state at the
        // start); later groups restart rate control, so they may differ.
        let mut single = crate::encode::H264::with_config(EncoderConfig { threads: 1, ..cfg.clone() }, rate).unwrap();
        let serial = encode_all(&mut single, &pics, 4);
        let mut g = GroupedH264::new(cfg.clone(), rate.den, Plan { lanes: 4, threads: 1 }).unwrap();
        let grouped = encode_all(&mut g, &pics, 4);
        assert_eq!(serial.len(), grouped.len());
        for j in 0..5 {
            assert_eq!(serial[j].data, grouped[j].data, "sample {j}");
            assert_eq!(serial[j].cto, grouped[j].cto, "sample {j}");
        }
    }

    #[test]
    fn a_stopped_export_leaves_no_encoder_waiting() {
        let rate = effectcraft_time::FrameRate::new(10, 1);
        let mut cfg = crate::encode::h264_config(64, 48, rate, 400);
        cfg.keyint = 4;
        let mut g = GroupedH264::new(cfg, rate.den, Plan { lanes: 3, threads: 1 }).unwrap();
        let _ = g.encode_batch(&frames(10, 64, 48), 0).unwrap();
        // dropped mid-stream (an export that failed or was cancelled): nothing hangs
        drop(g);
    }

    #[test]
    fn plan_overrides() {
        let p = Plan { lanes: 5, threads: 2 };
        assert_eq!(plan_with_env(p, None, None), p);
        assert_eq!(plan_with_env(p, Some("3"), None), Plan { lanes: 3, threads: 2 });
        assert_eq!(plan_with_env(p, Some("1"), None), Plan { lanes: 1, threads: 2 });
        assert_eq!(plan_with_env(p, Some("0"), Some("x")), p);
        assert_eq!(plan_with_env(p, None, Some("0")), Plan { lanes: 5, threads: 0 });
    }
}
