title: fix(viewer): split views off the UI thread, click-free preview audio and a faster GPU box blur
branch: upstream/preview-viewer-fixes
depends: none

## What

**Viewer**
- Split 3D views (2/4 views) and the locked viewer render on the frame workers as CPU pixels instead of synchronously on the UI thread on every frame of playback; their jobs are keyed by the document revision so they don't drop the main viewer's queued prefetch.

**Preview playback**
- Preview audio: each block is resampled from its exact phase at the mix rate, so blocks join without a jump (48 kHz mix on a 44.1 kHz device crackled, #335).
- The achieved-fps readout falls while playback holds instead of freezing at the last value.
- Auto backend: AutoPick re-probes the CPU less often while the GPU is clearly faster.

**Effects / GPU**
- GPU blur: `box_h` stages its reads through workgroup memory (coalesced; bit-identical to the old kernel, which stays as the test reference and as the Metal pipeline, where fast math reassociates the sums).

**Tests**
- Profiling test for timeline scrub and zoom on a long 500-layer comp (#714).
- WebP-with-alpha sequence decodes each frame once and plays from the cache (#719, not reproduced).
- The OpenGL init test is skipped where wgpu has no GL backend (macOS with `EFFECTCRAFT_REQUIRE_GPU=1`).

## Why

Preview responsiveness and correctness fixes from the 12 Oct preview audit: extra views stalled the UI thread during playback, preview audio clicked at every block on 44.1 kHz devices, and the horizontal box pass read memory uncoalesced (~3.2 ms per pass at 4K).

Upstream landed its own Cryptomatte rank-plane fix (#473) meanwhile, so that change is dropped here. Three commits are adapted to this base: the split-view change keys frame content with `Frames::content_of` (the fork used a freeze-preview variant upstream doesn't have) and its test no longer assumes the native GPU display path; the audio resampling change is ported without the fork's streamed audio-effect state; the fps-readout change leaves out the fork's audio-hold hunks. Each commit is cherry-picked from eliasx45/effectcraft (`-x` trailer).

## How tested

All on this branch, against `upstream/main` (Linux, lavapipe for GPU tests with `EFFECTCRAFT_REQUIRE_GPU=1`):
- `rustfmt --edition 2024 --check` on the changed files; `cargo clippy -p effectcraft-gpu -p effectcraft-render -p effectcraft-media -p effectcraft-ui-egui --all-targets -- -D warnings`: clean.
- `cargo test -p effectcraft-raster -p effectcraft-effects`, `-p effectcraft-render`, `-p effectcraft-media --test footage`: pass.
- `cargo test -p effectcraft-gpu`: pass (including `tests_box::staged_box_h_matches_the_serial_kernel_bit_for_bit`) except `tests_fx_pixel2::stylize` and `tests_fx_stylize::posterize_thresholds_strobe_vignette`, which fail the same way on unmodified `upstream/main` under lavapipe.
- `cargo test -p effectcraft-ui-egui --lib` (new `panels::viewer::tests::extra_view_frames_do_not_drop_the_main_viewers_prefetch`, `extra_views_render_on_the_frame_workers`, `audio::tests::resampled_blocks_join_without_a_jump`): pass except the known upstream failure `theme::script_font_tests::arabic_words_are_joined_and_read_right_to_left`.
- `cargo test -p effectcraft-ui-egui --test ui_preview_align --test ui_timeline_zoom --test viewer_geometry --test ui_audio_scrub`: pass.
- `cargo check --target wasm32-unknown-unknown` for raster, effects, render, media, gpu: clean.
- Not run here: the Metal path of `box_h` (verified on an M1 Max in the original commit).
