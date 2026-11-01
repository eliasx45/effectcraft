//! The VideoToolbox ProRes encoder behind `effectcraft_media::prores_enc` (safe code).
//!
//! [`encode`] is the hardware encode function [`crate::register`] installs: it encodes one frame
//! on Apple silicon's ProRes engines, through a session borrowed from a process-wide pool
//! ([`MAX_SESSIONS`] at most; callers encode frames on several threads). It declines (an error;
//! the caller encodes that frame in software) what it doesn't take: interlaced frames, a frame
//! that doesn't match its configuration, sizes above [`MAX_SIDE`], colour codes CoreVideo has no
//! name for, and a system without a hardware ProRes encoder (the session can't be created; that
//! format is not tried again for [`RETRY`]).
//!
//! What the hardware writes is not bit-identical to the software encoder: Apple's own quantiser
//! choice and data rate (it ignores a custom `bits_per_mb`). `effectcraft_media::prores_enc`
//! checks every frame's header (size, chroma, alpha, colour) before using it.

use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use filmcraft_prores::{ChromaFormat, EncoderConfig, Frame};

use crate::prores_pack;
use crate::videotoolbox_enc::{S4AS, SV22, Session};

/// Compression sessions alive at once (all formats): enough to keep the encode engines busy from
/// a few threads.
pub const MAX_SESSIONS: usize = 4;
/// How long an encode waits for a busy session before the frame is encoded in software.
const WAIT: Duration = Duration::from_secs(2);
/// A format whose session could not be created is tried again after this long.
const RETRY: Duration = Duration::from_secs(30);
/// Largest picture side the hardware path takes.
pub const MAX_SIDE: u32 = 8192;

/// What a session encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Format {
    codec: u32,
    width: u32,
    height: u32,
    color: (u8, u8, u8),
    alpha: bool,
}

#[derive(Default)]
struct Pool {
    idle: Vec<(Format, Session)>,
    /// Sessions alive (idle or lent out).
    live: usize,
    /// Formats whose session failed to create, and when.
    failed: HashMap<Format, Instant>,
}

static POOL: LazyLock<(Mutex<Pool>, Condvar)> = LazyLock::new(|| (Mutex::new(Pool::default()), Condvar::new()));

fn pool() -> MutexGuard<'static, Pool> {
    POOL.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A session for `f`: an idle one, a new one while fewer than [`MAX_SESSIONS`] are alive (or in
/// place of an idle one of another format), or the next one returned within [`WAIT`].
fn acquire(f: Format) -> Result<Session, String> {
    let deadline = Instant::now() + WAIT;
    let mut p = pool();
    if p.failed.get(&f).is_some_and(|at| at.elapsed() < RETRY) {
        return Err("no hardware ProRes encoder for this format".into());
    }
    let replaced = loop {
        if let Some(i) = p.idle.iter().position(|(k, _)| *k == f) {
            return Ok(p.idle.swap_remove(i).1);
        }
        if p.live < MAX_SESSIONS {
            p.live += 1;
            break None;
        }
        if let Some(i) = p.idle.iter().position(|(k, _)| *k != f) {
            break Some(p.idle.swap_remove(i).1);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err("every hardware ProRes encoder session is busy".into());
        }
        p = POOL.1.wait_timeout(p, deadline - now).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
    };
    drop(p);
    // the replaced session is invalidated outside the lock; its slot is reused
    drop(replaced);
    match Session::new(f.codec, f.width, f.height, f.color, f.alpha) {
        Ok(s) => Ok(s),
        Err(e) => {
            let mut p = pool();
            p.live = p.live.saturating_sub(1);
            p.failed.insert(f, Instant::now());
            POOL.1.notify_one();
            Err(e)
        }
    }
}

/// Give a session back (`ok`), or drop it after a failure (it may be in a bad state).
fn release(f: Format, s: Session, ok: bool) {
    let mut p = pool();
    if ok {
        p.idle.push((f, s));
    } else {
        p.live = p.live.saturating_sub(1);
        drop(p);
        drop(s);
        POOL.1.notify_one();
        return;
    }
    POOL.1.notify_one();
}

/// Encode `frame` with `cfg` on the hardware ProRes encoder (an `effectcraft_media::prores_enc::HardwareEncode`).
pub fn encode(cfg: &EncoderConfig, frame: &Frame) -> Result<Vec<u8>, String> {
    if frame.is_interlaced() {
        return Err("interlaced frame".into());
    }
    if (frame.width, frame.height) != (cfg.width, cfg.height) || frame.chroma != cfg.profile.chroma() {
        return Err(format!("{}x{} {:?} frame for a {}x{} {:?} encoder", frame.width, frame.height, frame.chroma, cfg.width, cfg.height, cfg.profile));
    }
    if cfg.width == 0 || cfg.height == 0 || cfg.width > MAX_SIDE || cfg.height > MAX_SIDE {
        return Err(format!("{}x{} is beyond the hardware encoder", cfg.width, cfg.height));
    }
    let alpha = cfg.encode_alpha && cfg.profile.chroma() == ChromaFormat::Yuv444 && frame.alpha.is_some();
    let f = Format {
        codec: u32::from_be_bytes(cfg.profile.fourcc()),
        width: cfg.width,
        height: cfg.height,
        color: (cfg.color.primaries, cfg.color.transfer, cfg.color.matrix),
        alpha,
    };
    let mut s = acquire(f)?;
    let pf = s.pixel_format();
    let r = s.encode(&mut |planes| match pf {
        S4AS => prores_pack::pack_s4as(frame, planes),
        SV22 => prores_pack::pack_sv22(frame, planes),
        other => Err(format!("unexpected source format {other:#x}")),
    });
    release(f, s, r.is_ok());
    r
}

/// Whether this Mac encodes ProRes in hardware (creates one small session; cached).
pub fn hardware_available() -> bool {
    static AVAILABLE: LazyLock<bool> = LazyLock::new(|| Session::new(u32::from_be_bytes(*b"apcn"), 256, 256, (1, 1, 1), false).is_ok());
    *AVAILABLE
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_prores::Profile;

    /// A gradient frame with a soft alpha ramp.
    fn frame(profile: Profile, w: u32, h: u32, depth: u8, alpha: bool) -> Frame {
        let mut f = Frame::new(w, h, profile.chroma(), depth, alpha);
        let max = (1u32 << depth) - 1;
        let lo = 16u32 << (depth - 8);
        let span = 219u32 << (depth - 8);
        for (k, y) in f.y.iter_mut().enumerate() {
            let (x, r) = (k as u32 % w, k as u32 / w);
            *y = (lo + (x * 3 + r * 2) * span / (3 * w + 2 * h)) as u16;
        }
        let cw = f.chroma_width().max(1);
        for (k, c) in f.cb.iter_mut().enumerate() {
            *c = (lo + (k as u32 % cw) * span / cw) as u16;
        }
        if let Some(a) = f.alpha.as_mut() {
            for (k, v) in a.iter_mut().enumerate() {
                *v = ((k as u32 % w) * max / w.max(1)) as u16;
            }
        }
        f
    }

    /// On a Mac with ProRes encode engines: every profile's hardware frame decodes in FilmCraft's
    /// software decoder to the picture that went in (within lossy-coding error), with alpha only
    /// when asked, and its header carries the configured colour tags. Elsewhere (no hardware) the
    /// encoder declines with an error, not a panic.
    #[test]
    fn hardware_frames_decode_in_software() {
        let have = hardware_available();
        for profile in Profile::ALL {
            for alpha in [false, true] {
                if alpha && profile.chroma() != ChromaFormat::Yuv444 {
                    continue;
                }
                let depth = if profile.chroma() == ChromaFormat::Yuv444 { 12 } else { 10 };
                let (w, h) = (320, 180);
                let f = frame(profile, w, h, depth, alpha);
                let mut cfg = EncoderConfig::new(profile, w, h);
                cfg.encode_alpha = alpha;
                cfg.color = filmcraft_prores::ColorInfo { primaries: 1, transfer: 1, matrix: 1 };
                let r = encode(&cfg, &f);
                if !have {
                    assert!(r.is_err());
                    continue;
                }
                let data = r.unwrap_or_else(|e| panic!("{profile:?} alpha {alpha}: {e}"));
                effectcraft_media::prores_enc::consistent(&cfg, &f, &data).unwrap_or_else(|e| panic!("{profile:?} alpha {alpha}: {e}"));
                let opts = filmcraft_prores::DecodeOptions { bit_depth: Some(depth), ..Default::default() };
                let back = filmcraft_prores::decode_frame_with(&data, &opts).unwrap();
                assert_eq!(back.alpha.is_some(), alpha, "{profile:?}");
                let mean = |a: &[u16], b: &[u16]| a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).sum::<f64>() / a.len() as f64;
                let tol = f64::from(1u32 << (depth - 8)) * if profile == Profile::Proxy { 3.0 } else { 1.0 };
                assert!(mean(&back.y, &f.y) < tol, "{profile:?} Y off by {}", mean(&back.y, &f.y));
                assert!(mean(&back.cb, &f.cb) < tol, "{profile:?} Cb off by {}", mean(&back.cb, &f.cb));
                if let (Some(a), Some(b)) = (&back.alpha, &f.alpha) {
                    assert!(mean(a, b) < tol, "{profile:?} alpha off by {}", mean(a, b));
                }
            }
        }
        // a frame that doesn't match its configuration is declined
        let f = frame(Profile::Hq, 64, 64, 10, false);
        assert!(encode(&EncoderConfig::new(Profile::P4444, 64, 64), &f).is_err());
        assert!(encode(&EncoderConfig::new(Profile::Hq, 32, 64), &f).is_err());
        let mut odd = EncoderConfig::new(Profile::Hq, 64, 64);
        odd.color.primaries = 2;
        assert!(encode(&odd, &f).is_err(), "unspecified primaries");
    }
}
