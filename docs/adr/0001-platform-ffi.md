# ADR 0001: `unsafe` OS media FFI in `crates/platform`, and nowhere else (fork-only)

- **Status:** accepted (2026-10-12, approved by the project owner, Elias, for the eliasx45 fork).
  Upstream (`storytold/effectcraft`) forbids `unsafe`; this crate and this record are fork-only
  (`scripts/fork/upstream-exclude.txt`) and are never queued upstream.
- **Mirrors:** FilmCraft's
  [ADR 0001: platform FFI](https://github.com/storytold/filmcraft/blob/main/docs/adr/0001-platform-ffi.md),
  whose VideoToolbox session wrapper this crate adapts (MIT OR Apache-2.0, attributed in
  `crates/platform/src/videotoolbox.rs`).

## Context

EffectCraft is pure Rust and the workspace sets `unsafe_code = "forbid"`. Its ProRes footage is
decoded by FilmCraft's clean-room ProRes decoder, which stays the reference. The owner's projects
put the same ProRes 4444 + alpha clip (1920×1080, 12-bit, 86 Mb/s) under dozens of layers;
decoding it in software costs about 16 ms of CPU per frame, while the Apple silicon media
engines decode it for about 0.6 ms of CPU per frame (ffmpeg measurement on the owner's M1 Max).
Previews are where that CPU is missing.

Using the media engines needs FFI into VideoToolbox / CoreMedia / CoreVideo, which is `unsafe`
Rust. The bindings (the `objc2-*` crates, MIT / Apache-2.0 / Zlib) are generated declarations;
nothing GPL/LGPL is linked.

## Decision

Allow `unsafe` in exactly one crate, `crates/platform` (`effectcraft-platform`, layer L5), for one
job: hardware **ProRes** decoding for **previews** on macOS. Not H.264/HEVC, not encoding, not
audio, dialogs or other OS integration: anything else needs a new decision.

Containment rules (FilmCraft's, unchanged):

1. The crate does not use `lints.workspace = true`. Its own `[lints]` table copies the workspace
   lints except `unsafe_code = "deny"` (not `forbid`), and adds
   `clippy::undocumented_unsafe_blocks = "deny"`. Only the FFI module (`videotoolbox`) carries
   `#[allow(unsafe_code)]`; the session pool, fallback logic (`prores`) and pixel unpacking
   (`convert`) are safe code.
2. Every `unsafe` block has a `// SAFETY:` comment saying why it is sound.
3. The public API is safe: no `pub unsafe fn`, no raw pointers or FFI types in public signatures;
   failures are `Result`s. The crate keeps the never-crash `deny(clippy::unwrap_used, …)`
   attribute of every clean crate.
4. No panic crosses the FFI boundary: the VideoToolbox output callback runs under `catch_unwind`
   and reports a panic as a failed frame.
5. The crate compiles on every target. The `objc2-*` crates are macOS-only dependencies;
   elsewhere `register()` reports `Unavailable` and does nothing. It is not part of the wasm
   check (L5).

## Previews only: the exactness guarantee

Hardware ProRes pictures are visually identical to the software decoder's, not bit-identical
(the owner accepted this for previews on 2026-10-12). Exports must stay exact:

- FilmCraft has one process-wide decoder registry, so the choice is per decoding thread:
  `effectcraft_media::hw` holds a thread-local "hardware allowed" flag, and the media pool runs
  every movie decode inside a scope that sets it (on or off) for that read. The hardware factory
  declines unless it is on; threads outside any scope decode exactly.
- The media pool decides per handle: the session's pool decodes ProRes with hardware when
  Settings ▸ Previews ▸ Hardware ProRes Decoding is on; its exact view
  (`FootageSource::exact`), which every export job uses (`crates/export`, so the Render Queue,
  File ▸ Export, pre-renders and every other job alike) and Save Frame As, never does. (Analyses
  such as tracking read the preview pool and its layer cache: visually identical input.) Decoded frames, open
  sources (and FilmCraft's per-source GOP cache inside them) and decode-ahead streams are keyed
  by that choice, so an export never reads a frame a preview decoded in hardware.
- The hardware path declines what it doesn't take (other codecs, interlaced frames, sizes above
  8192, a frame header that doesn't match the sample entry, a system without a hardware ProRes
  decoder) and any sample it fails on is decoded by the software decoder. ProRes is intra-only,
  so there is nothing to replay.

## Consequences

- ProRes previews decode on the media engines; the CPU goes to compositing.
- The fork carries an `unsafe` crate upstream does not have: changes to `crates/platform` need
  the scrutiny of any FFI code, and it never goes into an upstream pull request.

## Amendment (2026-10-20): background thread priority

Cache Frames When Idle rendered at normal priority on every core and made the owner's whole Mac
lag (300–470 % CPU). Its frames now render on their own small thread pool, and those threads
lower themselves to macOS **utility** quality of service, so the UI, the window server and other
apps run first. That needs one libSystem call, `pthread_set_qos_class_self_np`, which has no safe
binding in the standard library or in the workspace's dependencies. It is the second job this
crate may do, under the same containment rules:

- the FFI module `qos` (`crates/platform/src/qos.rs`) carries `#[allow(unsafe_code)]` and holds
  only the declaration, in an `unsafe extern` block with a `// SAFETY:` comment; the function is
  declared `safe` because it takes plain integers, changes only the calling thread's scheduling
  and reports bad values as an error code;
- the public API is `effectcraft_platform::background_thread() -> bool` (safe; `false` and a no-op
  off macOS). The desktop app hands it to the UI crate (`frames::set_idle_thread_start`), which
  calls it at the start of each idle-cache thread; the UI crate itself stays `forbid(unsafe)`.
