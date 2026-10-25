//! OS media integration (layer L5, fork-only): hardware Apple ProRes, H.264 and HEVC decoding
//! for previews.
//!
//! [`register`] puts VideoToolbox decoder factories in front of FilmCraft's own decoders
//! (`filmcraft_codecs::register_video_decoder`) on macOS, where Apple silicon decodes these codecs
//! on its media engines for a fraction of the CPU time of the pure-Rust decoders: ProRes
//! ([`prores`]) and H.264 / HEVC ([`h26x`]: progressive 4:2:0, 8-bit H.264, 8- or 10-bit HEVC).
//! On other systems it does nothing and reports [`Availability::Unavailable`].
//!
//! Hardware pictures look the same as the pure-Rust decoders' but are not guaranteed
//! bit-identical, so they are for previews only:
//!
//! - the factories decline (the software decoder is used) unless the decode runs inside a preview
//!   read ([`effectcraft_media::hw::allowed`]): exports, the Render Queue and other final renders
//!   read footage through the media pool's exact view and always decode in software;
//! - they decline for streams the hardware path does not take (other codecs, layouts the software
//!   decoder outputs differently, sizes above 8192) and when the OS cannot create a hardware
//!   session;
//! - ProRes: a sample the hardware fails on (or one it does not take: interlaced, a frame header
//!   that doesn't match the sample entry) is decoded by the software decoder instead. ProRes is
//!   intra-only, so there is nothing to replay;
//! - H.264 / HEVC: a decoder that fails mid-stream (or meets in-band parameter sets it was not
//!   set up for) becomes the software decoder for the same stream, replaying the samples since
//!   the last restart point ([`hybrid::HybridDecoder`]): same pictures, pts and order.
//!
//! Frames come out as the software decoders' planar YUV (a CPU copy of the decoded buffer), so
//! the media pool, caches and GPU upload are unchanged.
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
pub mod h26x;
pub mod hybrid;
pub mod nal;
#[cfg(target_os = "macos")]
pub mod prores;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod qos;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod videotoolbox;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod videotoolbox_nal;

use std::sync::atomic::{AtomicU64, Ordering};

/// What [`register`] made available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// A hardware decoder factory was registered (its name).
    Available(&'static str),
    /// Nothing to register on this system (why).
    Unavailable(&'static str),
}

/// Register the platform's hardware decoders: ProRes, H.264 and HEVC, each when this Mac has a
/// hardware decoder for it (call once at startup; repeated calls are harmless). Only preview reads
/// use them ([`effectcraft_media::hw`]).
pub fn register() -> Availability {
    #[cfg(target_os = "macos")]
    {
        let prores = prores::hardware_available();
        let nal = h26x::hardware_available();
        if prores {
            filmcraft_codecs::register_video_decoder(prores::factory);
        }
        if nal {
            filmcraft_codecs::register_video_decoder(h26x::factory);
        }
        effectcraft_media::hw::set_available(prores || nal);
        match (prores, nal) {
            (true, true) => Availability::Available("VideoToolbox ProRes, H.264, HEVC"),
            (true, false) => Availability::Available("VideoToolbox ProRes"),
            (false, true) => Availability::Available("VideoToolbox H.264, HEVC"),
            (false, false) => Availability::Unavailable("this Mac has no hardware video decoder"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Availability::Unavailable("hardware video decoding needs macOS")
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

/// Raise the calling thread back to the priority of work the user waits for (a frame worker
/// rendering the viewer's frame or playback after prefetch): on macOS user-initiated quality of
/// service. Elsewhere it does nothing. Whether it was raised.
pub fn foreground_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        qos::user_initiated()
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
    /// Software fallbacks: ProRes frames the software decoder decoded instead (hardware failure or
    /// a frame it doesn't take); H.264 / HEVC decoders that switched to software mid-stream.
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
