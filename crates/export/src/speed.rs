//! Process-wide export speed choices (E2/E3, 6 Nov 2026).
//!
//! - **Parallel H.264** (on by default): an H.264 movie is encoded as independent keyframe groups
//!   (one group per key-frame interval, 2 s) on several encoders at once, and the groups are
//!   joined in order into one stream ([`crate::h264_groups`]). Off: one encoder for the whole
//!   movie, as before. `EFFECTCRAFT_H264_PARALLEL=0` turns it off too.
//! - **Hardware H.264** (off by default): H.264 movies encode on the platform's hardware encoder
//!   (`effectcraft_media::h264_hw`, VideoToolbox on a Mac) when there is one; otherwise in
//!   software. Its bits differ from the software encoder's and it is not on every system, so it is
//!   for draft / fast exports (`effectcraft-cli render --hw-h264` or `--fast`).
//! - **Hardware footage decoding** (off by default): exports read footage through the footage
//!   source's own view, which may decode on a hardware decoder (as previews do), instead of its
//!   exact view ([`crate::Job::source`]). The caller turns the source's hardware decoding on.
//!   Not bit-exact, so for draft / fast exports only (`--hw-decode` or `--fast`).
//!
//! They are process-wide (atomics), set once at startup by the CLI.

use std::sync::atomic::{AtomicBool, Ordering};

static PARALLEL_H264: AtomicBool = AtomicBool::new(true);
static HARDWARE_H264: AtomicBool = AtomicBool::new(false);
static HARDWARE_DECODE: AtomicBool = AtomicBool::new(false);

/// Turns parallel keyframe-group H.264 encoding off: `EFFECTCRAFT_H264_PARALLEL=0`.
pub const PARALLEL_H264_ENV: &str = "EFFECTCRAFT_H264_PARALLEL";

/// Encode H.264 in parallel keyframe groups (the default) or with one encoder.
pub fn set_parallel_h264(on: bool) {
    PARALLEL_H264.store(on, Ordering::Relaxed);
}

/// Whether H.264 encodes in parallel keyframe groups: [`set_parallel_h264`], unless
/// `EFFECTCRAFT_H264_PARALLEL` is `0`, `off` or `false`.
pub fn parallel_h264() -> bool {
    PARALLEL_H264.load(Ordering::Relaxed) && !env_off(std::env::var(PARALLEL_H264_ENV).ok().as_deref())
}

pub(crate) fn env_off(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// Encode H.264 movies on the hardware encoder when there is one (draft / fast exports).
pub fn set_hardware_h264(on: bool) {
    HARDWARE_H264.store(on, Ordering::Relaxed);
}

/// Whether H.264 movies try the hardware encoder first.
pub fn hardware_h264() -> bool {
    HARDWARE_H264.load(Ordering::Relaxed)
}

/// Read footage through the footage source's own (possibly hardware-decoding) view instead of
/// its exact view (draft / fast exports).
pub fn set_hardware_decode(on: bool) {
    HARDWARE_DECODE.store(on, Ordering::Relaxed);
}

/// Whether exports may read hardware-decoded footage.
pub fn hardware_decode() -> bool {
    HARDWARE_DECODE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_parallel_switch_reads_off_values() {
        for v in ["0", "off", " OFF ", "false", "no"] {
            assert!(super::env_off(Some(v)), "{v:?}");
        }
        for v in [None, Some("1"), Some(""), Some("on"), Some("yes")] {
            assert!(!super::env_off(v), "{v:?}");
        }
    }
}
