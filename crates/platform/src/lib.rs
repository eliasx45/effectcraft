//! OS media integration (layer L5, fork-only): hardware Apple ProRes decoding for previews.
//!
//! [`register`] puts a VideoToolbox ProRes decoder factory in front of FilmCraft's own decoders
//! (`filmcraft_codecs::register_video_decoder`) on macOS, where Apple silicon decodes ProRes on
//! its media engines for a fraction of the CPU time of the pure-Rust decoder. On other systems it
//! does nothing and reports [`Availability::Unavailable`].
//!
//! Hardware pictures look the same as the pure-Rust decoder's but are not bit-identical, so they
//! are for previews only:
//!
//! - the factory declines (the software decoder is used) unless the decode runs inside a preview
//!   read ([`effectcraft_media::hw::allowed`]): exports, the Render Queue and other final renders
//!   read footage through the media pool's exact view and always decode in software;
//! - it declines for streams the hardware path does not take (other codecs, sizes above 8192)
//!   and when the OS cannot create a hardware session;
//! - a sample the hardware fails on (or one it does not take: interlaced, a frame header that
//!   doesn't match the sample entry) is decoded by the software decoder instead. ProRes is
//!   intra-only, so there is nothing to replay.
//!
//! [`background_thread`] lowers a thread to background quality of service (macOS), for the
//! threads Cache Frames When Idle renders on.
//!
//! This is the one crate allowed to use `unsafe` (OS FFI), and only in its FFI modules
//! (docs/adr/0001-platform-ffi.md).

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod convert;
/// The cursor's screen position while the Finder drags files over the window (no `unsafe`).
pub mod cursor;
#[cfg(target_os = "macos")]
pub mod prores;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod qos;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod videotoolbox;

use std::sync::atomic::{AtomicU64, Ordering};

/// What [`register`] made available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// A hardware decoder factory was registered (its name).
    Available(&'static str),
    /// Nothing to register on this system (why).
    Unavailable(&'static str),
}

/// Register the platform's hardware ProRes decoder (call once at startup; repeated calls are
/// harmless). Only preview reads use it ([`effectcraft_media::hw`]).
pub fn register() -> Availability {
    #[cfg(target_os = "macos")]
    {
        if !prores::hardware_available() {
            return Availability::Unavailable("this Mac has no hardware ProRes decoder");
        }
        filmcraft_codecs::register_video_decoder(prores::factory);
        effectcraft_media::hw::set_available(true);
        Availability::Available("VideoToolbox ProRes")
    }
    #[cfg(not(target_os = "macos"))]
    {
        Availability::Unavailable("hardware ProRes decoding needs macOS")
    }
}

/// Lower the calling thread's priority for background work that nobody waits for (Cache Frames
/// When Idle): on macOS utility quality of service, so the UI, playback and other apps run first
/// and it prefers efficiency cores. Elsewhere it does nothing. Whether it was lowered.
pub fn background_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        qos::utility()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Hardware decoding counters (process-wide).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Hardware decoders the factory created.
    pub decoders: u64,
    /// Frames the hardware decoded.
    pub frames: u64,
    /// Frames the software decoder decoded instead (hardware failure or a frame it doesn't take).
    pub fallbacks: u64,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) static DECODERS: AtomicU64 = AtomicU64::new(0);
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) static FRAMES: AtomicU64 = AtomicU64::new(0);
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) static FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// The counters so far.
pub fn stats() -> Stats {
    Stats { decoders: DECODERS.load(Ordering::Relaxed), frames: FRAMES.load(Ordering::Relaxed), fallbacks: FALLBACKS.load(Ordering::Relaxed) }
}
