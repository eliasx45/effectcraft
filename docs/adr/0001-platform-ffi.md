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

## Amendment (2026-10-25): hardware H.264 / HEVC decoding for previews

**Status:** accepted (2026-10-25, approved by the project owner, Elias).

Requested by the owner (session brief HWDEC1): footage decoding was the largest single cost in
his comps (about 52 ms per frame for an H.264 video inside a precomp: 45 ms of software decoding
plus the YUV upload). The third job of this crate is hardware **H.264 and HEVC** decoding for
**previews** on macOS, under the same containment rules and the same previews-only guarantee as
ProRes (the media pool's exact view, which every export reads, never uses it):

- FFI module `videotoolbox_nal` (`#[allow(unsafe_code)]`, every block with `// SAFETY:`; the
  output callback runs under `catch_unwind`), adapted from FilmCraft's VideoToolbox decoder: a
  session per decoder from the `avcC` / `hvcC` parameter sets, hardware required, asynchronous
  decompression, decoded `420v`/`420f`/`x420`/`xf20` buffers copied into the software decoders'
  planar `Yuv8` / `Yuv16` pictures (cropped, same colour and pixel aspect).
- Safe code around it: `nal` (which streams the hardware path takes, decided from the SPS: only
  progressive 4:2:0 8-bit H.264 and 8/10-bit HEVC that FilmCraft's software decoders also take,
  so a fallback is always possible), `h26x` (decoder: reorder buffer of the stream's depth for
  presentation order, RASL pictures left out after a CRA/BLA start as in software, a fresh
  session per seek) and `hybrid` (a decoder that fails mid-stream or meets new in-band parameter
  sets switches to the software decoder and replays the samples since the last restart point, so
  the output is the software decoder's own sequence).
- Frames still go through a CPU copy into the existing YUV upload; handing the `CVPixelBuffer`'s
  IOSurface to the GPU without a copy would need Metal interop in the GPU crate and is a separate
  decision.
- The switch is the existing Settings ▸ Previews ▸ Hardware Video Decoding preference (on by
  default; formerly "Hardware ProRes Decoding", same setting id); it now covers H.264 and HEVC as well.

## Amendment (2026-10-25b): decoded surfaces shared with the GPU (HWDEC2)

**Status:** accepted (2026-10-25, approved by the project owner, Elias).

Requested by the owner (session brief HWDEC2). Hardware-decoded H.264 / HEVC pictures no longer
need an upload to reach the GPU compositor on Metal. The fourth job of this crate:

- FFI module `metal_surface` (`#[allow(unsafe_code)]`, every block with `// SAFETY:`): a kept
  `CVPixelBuffer`'s two IOSurface planes become `MTLTexture`s of wgpu's own `MTLDevice`
  (`newTextureWithDescriptor:iosurface:plane:`, no copy), wrapped with
  `wgpu::hal::metal::Device::texture_from_raw` and `wgpu::Device::create_texture_from_hal`. The
  hal texture's drop callback holds the pixel buffer, so the decoder cannot reuse the surface
  while wgpu (and the GPU) may still read it. Sessions ask for IOSurface-backed, Metal-compatible
  buffers.
- Safe registry `surfaces`: once the GPU asked on a Metal device, the decoder keeps each
  picture's pixel buffer keyed by its luma plane (weak reference, so a reused address never
  matches a dead entry), at most 16, oldest out first.
- The GPU crate stays free of `unsafe`: `effectcraft_gpu::surface` takes an importer function
  (`set_surface_import`, installed by `register`) and splits the imported Y / CbCr planes into
  the integer planes its native YUV decode already reads (`surface.wgsl`); everything after is
  unchanged. No importer, another backend, an unsupported layout or a surface no longer kept
  means the CPU planes are uploaded as before; CPU renders and exports never see surfaces.
- The decoder still copies each picture into planar samples (FilmCraft's frame type, its GOP
  cache and every CPU path need them); dropping that copy would need a surface-carrying variant
  of `filmcraft_frame::PixelData` upstream.

## Amendment (2026-11-01): ProRes surfaces shared with the GPU too (Fix 2)

**Status:** accepted (2026-11-01, approved by the project owner, Elias, as "Fix 2" of the ProRes
preview speed plan).

A 4K ProRes 4444 + alpha frame cost about 15 ms of GPU upload per layer (66 MB of planes; the
hardware decode itself is about 3 ms). HWDEC2's zero-copy path now covers hardware ProRes too,
under the same rules:

- ProRes sessions (`videotoolbox`) ask for IOSurface-backed, Metal-compatible `s4as` / `x422`
  buffers; once the GPU shares surfaces, the output callback also retains the decoded buffer and
  `prores` keeps it in `surfaces` with the picture's layout (bits, chroma subsampling, alpha).
- `metal_surface` imports `s4as` as R16 Y, RG16 CbCr and R16 alpha planes and `x422` as R16 Y and
  half-width RG16 CbCr (plane sizes from CoreVideo); the GPU's split pass rounds luma / chroma to
  the coded depth and clamps, and shifts alpha down, exactly as `convert` does for the CPU planes,
  so the decoded picture is bit-identical to the uploaded one.
- Held surfaces are bounded by bytes (768 MB by default, `surfaces::set_budget`) as well as count
  (64); gone pictures leave first, then those the GPU already read, then the oldest. A hardware
  ProRes decode that fails while surfaces are held lets them all go (in case the decoder's pool
  ran dry) and is not counted towards giving the stream up. Every other failure uploads as before.
- Decode Footage at 8 Bits (the owner's default): the media pool still rounds each frame to 8 bits
  on the CPU (now a vectorised loop) and keeps only that, and tells the platform through
  `effectcraft_media::hw::set_eight_bit_hook`; the kept surface moves to the 8-bit frame and the
  split pass rounds it to 8 bits exactly as `frame_to_8bit` does (limited range only; other
  frames upload), so the 8-bit picture is bit-identical to its upload too.
- The CPU copy stays (CPU renders, caches and the footage cache read it); keeping frames on the
  GPU with a CPU copy only on demand is a later step ("Fix 3").

## Amendment (2026-11-01): hardware ProRes encoding (Fix 4)

**Status:** accepted (2026-11-01, approved by the project owner, Elias, as "Fix 4" of his ProRes
plan).

ProRes exports (File ▸ Export, the Render Queue, pre-renders) and preview proxies encoded every
frame with FilmCraft's pure-Rust encoder on the CPU: about 3 fps for a 4K ProRes 4444 + alpha
frame stream on one encoder (6 fps on four threads) on the owner's M1 Max, which has ProRes
encode engines. The fifth job of this crate is hardware **ProRes encoding**, under the same
containment rules:

- FFI module `videotoolbox_enc` (`#[allow(unsafe_code)]`, every block with `// SAFETY:`; the
  output callback runs under `catch_unwind`): a `VTCompressionSession` per fourcc, size, colour
  description and alpha choice, hardware encoder required
  (`kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder`), fed the encoder's own
  source formats from its IOSurface buffer pool (`s4as` for 4444 / 4444 XQ, `sv22` for the 422
  family), colour tags set as session properties, alpha coded only when asked
  (`kVTCodecPropertyKey_PreserveAlphaChannel`). One frame is encoded synchronously at a time per
  session.
- Safe code around it: `prores_pack` (FilmCraft's planar `u16` frame into those buffers) and
  `prores_enc` (a pool of at most 4 sessions shared by the threads that encode; a format whose
  session can't be created is not tried again for 30 s).
- The hook is `effectcraft_media::prores_enc` (FilmCraft has no encoder registry): `register`
  installs the hardware encode function at startup (`effectcraft_platform::register`), and every
  ProRes writer encodes through `ProResEncoder`, which writes the software encoder's frame
  whenever the hardware declines, fails, or returns a frame whose header doesn't match what was
  asked (size, chroma, alpha, colour), and stops asking after 3 failures in a row. Both write
  ordinary ProRes frames with the same sample entry.
- **Not bit-identical**: unlike decoding (previews only), encoded frames reach exported files.
  The owner approved hardware encoding; using its (not bit-identical) output for exports was a
  decision taken during implementation, since encoding is lossy either way, open for him to
  revisit.
  Measured 1 Nov 2026 (bench `crates/platform/examples/prores_enc_bench.rs`): PSNR of the
  decoded hardware frames against the source within 0.5 dB of the software encoder's on every
  plane. `EFFECTCRAFT_SOFTWARE_PRORES_ENCODE=1` keeps every encode in software.
- The hardware has Apple's own data rate (no rate property for ProRes). Preview proxies use it
  too (owner, 2026-11-01: "go hw"): a proxy of a sparse keyed 1080p 4444 clip built 2.8x faster
  (88 vs 31 fps) but is ~1.9x bigger (373 vs 200 KB a frame) than the software encoder's at the
  source's density, which still applies to frames the software fallback writes.

## Amendment (2026-11-01): hardware frames stay on the GPU, CPU copy on demand (Fix 3)

**Status:** accepted (2026-11-01, approved by the project owner, Elias, as "Fix 3" of his ProRes
plan).

After Fix 2 the GPU read hardware ProRes pictures from VideoToolbox's surface, but every
picture was still copied out into CPU planes on the decoder's callback thread and, with Decode
Footage at 8 Bits, rounded to 8 bits on the CPU, whether or not anything read those planes. Now a
preview read leaves the picture in its surface and the CPU copy is made only when something asks
for CPU pixels:

- `videotoolbox` decodes with a *defer* flag (the frame refcon carries it with the alpha flag):
  the callback checks the buffer (format, size, planes) and retains it, no copy;
  `copy_surface` copies a kept buffer out later (same `convert` code, so the same samples).
- Safe module `resident`: the picture (`Resident`) is held in a registry keyed by the luma
  allocation of a *stand-in* `VideoFrame` handed to FilmCraft (FilmCraft's `PixelData` has no
  surface variant): the picture's size, layout and colour, a luma plane shorter than the picture
  (so the media pool knows a stand-in and never reads its planes), and shared zero "ballast"
  planes (one virtual allocation per length) so its byte size is the picture's and FilmCraft's
  GOP cache bounds it as before. A picture lives while its stand-in does. Bounded by bytes
  (8 GiB default, `resident::set_budget`); past it, or for 5 s after a hardware decode failed
  while pictures were held, or with `EFFECTCRAFT_RESIDENT_FRAMES=0`, decodes copy at once as
  before. Holding never blocks a decode (VideoToolbox gives each picture a new buffer).
- `effectcraft_media::hw::ResidentHooks` (resolve, sweep, usage) is how the media pool, which
  owns all FilmCraft frames, turns a stand-in into a cache frame (`convert::Lazy`): only its own
  reads of movies it keeps open may defer (`hw::resident_scope`; proxies, thumbnails of other
  files and exports, which never use the hardware, get planes). `effectcraft_render::FramePixels`
  gained `gpu_yuv()`: a view without samples and a surface key (`yuv::View::surface`), which the
  GPU's native decode reads through the same importer as Fix 2; `yuv()` / `rows()` (CPU renders,
  CPU-only layer work, Save Frame, thumbnails, a GPU that can't read the surface) make the CPU
  copy once, into recycled plane buffers (`media::recycle`), rounded to 8 bits on scoped threads
  when the frame is kept at 8 bits. Pictures are bit-identical to the copy-at-decode path on both
  the GPU (Fix 2's shader rounding) and the CPU (the same copy code). After a CPU copy of a
  movie's frame its next frames are copied at decode for 10 s, so footage the CPU reads anyway
  isn't held twice.
- Memory: a cached frame counts its surface; a CPU copy made later counts too. `memory.stats`
  reports `mediaSurfaceFrames` / `mediaSurfaceBytes` (pictures held in surfaces, cached or in the
  decoders' own caches). At 8 bits a cached frame holds its full-depth surface: twice the bytes
  of the 8-bit CPU copy it replaced, so the same budget caches about half as many frames.
- Measured 1 Nov 2026 (`crates/platform/examples/resident_bench.rs`: media pool preview read
  + GPU native decode per frame, decode-ahead off, whole-process CPU time, dev profile at opt 3,
  M1 Max at load average 65–370 from other agents, so ranges over runs): 1080p 4444+alpha full
  depth 4.7–5.2 → 1.5–1.8 ms CPU a frame (wall 8.0–9.1 → 6.0–7.0 ms); at 8 bits 6.9 → 1.8 ms
  CPU (15.5–16.0 with the rounding as it was before this change; wall 14–18 → 5.3–8 ms); 4K
  4444+alpha full depth 13.9–17.9 → 2.6–3.2 ms CPU (wall 17–29 → 13–18 ms), at 8 bits 25.5
  (50–54 before) → 2.3–3.6 ms CPU (wall 27–71 → 13–19 ms); 4K 422 HQ 7.4 → 2.0 ms full depth,
  27.8 → 1.9 ms at 8 bits. Every GPU read came from the surface, no CPU copy made. A CPU copy
  on demand into recycled buffers vs fresh pages: 1080p 4444+alpha 1.7–2.4 → 0.6–1.2 ms, 4K
  5.3–8.8 → 1.6–3.3 ms. The 8-bit rounding loop (`convert::frame_to_8bit`) was not vectorised in
  builds with overflow checks (a checked shift per sample); a wrapping shift makes it 0.75 ms on
  one thread for a 1080p 4444+alpha frame (6.6 ms before); release builds, the app's, are not
  affected by the overflow checks.
- H.264 / HEVC keep copying at decode (Fix 2's surfaces): their decoders feed FilmCraft's GOP
  cache with reordering and replay, and their 8-bit NV12 copy is cheap.
