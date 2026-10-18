# GPU preview profiling (RT0)

The CPU renderer remains the pixel reference. Profiling does not change shaders or precision.
The `profile` Cargo feature on `effectcraft-cli` enables bounded CPU spans and optional GPU
timestamps. Ordinary builds compile CPU spans to no-ops and request no timestamp features.

On a native machine with a GPU, run the complete suite with one command:

```sh
CARGO_BUILD_JOBS=4 nice -n 10 cargo xtask bench --gpu
```

Pass --bin with an existing profiling CLI to skip compilation. Use the lane's existing `CARGO_TARGET_DIR`. This is a development benchmark build, not an app or
release build. Record its profile, commit, adapter and system load with the results. `--quick`
uses eight frames for a smoke check; the default uses 60. `--scenario mograph-1080`,
`--scenario footage-4k` or `--scenario adv3d-1080` selects one workload. A supplied `--project`
and `--comp` use an existing project. `--frames` is bounded to 1–600.

The generated suite contains:

- `footage-4k`: separate H.264 4:2:0 and ProRes 4:2:2 10-bit clips, generated locally by external
  ffmpeg, with Levels, Curves, Gaussian Blur, Glow, Tint, Exposure, Sharpen and a feathered mask.
  ffmpeg is a fixture generator only, never linked or shipped. Generated files stay in `target`.
- `mograph-1080`: 80 original shape/text layers with animated transforms, trim paths, masks
  and drop-shadow styles. No Motion Library or third-party artwork is required.
- `adv3d-1080`: the CLI's Advanced 3D scene with a camera move, depth of field and motion blur.

`target/bench/gpu/results.json` contains completed viewer-frame p50/p95, a separately reported
cold frame, upload/readback traffic and CPU/GPU stage totals for Full and Half. Each workload
also has its own JSON and Chrome trace file. GPU initialization/pipeline compilation occurs
before these frame measurements and is reported separately as `initializationMs`. `gpuDiagnosticWindowMs` includes the gaps between the first and last recorded GPU passes, unlike the sum of kernel durations. Consecutive times use exact frame ticks; this is not a
benchmark of repeatedly fetching one finished frame.

Latency samples are collected with trace recording stopped. One additional moving frame is
traced separately to avoid folding instrumentation overhead into the sample distribution.
The CPU spans are **inclusive**, so nested spans overlap. GPU durations are adapter timestamps,
on their own trace lane with its origin normalized independently, **not correlated to the CPU clock**. Compare aggregate stage
durations; the trace does not establish CPU/GPU overlap. Headless display preparation plus a
completion wait is measured; actual window presentation and display latency are not measured.

Timestamp queries are requested only when the adapter supports them. Existing shared devices
without the feature still render, with CPU timing and an explicit unavailable indication. The
recorder retains at most 100,000 events. At most eight query batches (1024 queries each) may be
in flight. Failures/capacity losses are counted in `droppedTraceEvents`; do not treat an incomplete
trace as a complete kernel breakdown. Async results from earlier recordings are discarded.

The benchmark refuses missing GPUs and failed GPU viewer frames rather than silently measuring
CPU fallback. CPU-only layer work and unsupported individual effects inside the compositor are
still allowed and visible in CPU spans. Use the existing CPU/GPU oracle suite to check fidelity;
timings alone do not establish correctness or a performance improvement.

### Colour fusion

Colour fusion (RT2b): profiling builds accept `EFFECTCRAFT_PROFILE_FUSION=0` to
run consecutive colour effects as separate passes; the report records `colorFusion`.
Use the same binary and inputs in alternating off/on/on/off runs. Default builds
always enable fusion. Levels (RGB controls), Curves, Tint and Exposure share exact
pointwise shader arithmetic, preserving premultiplication and quantization after
each effect. Channel-specific Levels and unsupported effects remain boundaries.
Generated pipelines are cached by operation sequence (parameters stay dynamic),
with at most 32 cached sequences and 32 operations per pass. Full caches or failed
compilation use the existing separate-pass path. Opacity and Preserve Transparency
apply within compositing; the latter no longer needs its own pass.

### Pipeline warm-up

The native UI returns its first UI invocation before starting one background
compositor initialization worker. CPU frames remain available until its result
is ready; a retired viewer discards a late result. The compositor still uses a
device separate from the window's surface device. Standard pipelines are built
by initialization; `Gpu::warm_pipelines` additionally prepares Advanced 3D and
particle pipelines through the same guarded cells used by rendering.

For paired cold-frame measurements, pass `--rt-warmup` with `--rt-profile`.
`initializationMs` and `warmupMs` are separate from `coldMs`: this moves work
before the measured frame, it does not eliminate compilation. The default CLI
leaves optional pipelines cold. `staticPipelines` excludes runtime-specialized
colour sequences. The focused warm-up test covers all registered default GPU
effect configurations and Advanced 3D; it is a compilation/reuse check, not a
replacement for per-effect pixel oracles.

Explicit disk pipeline caching is deferred: the pinned wgpu API requires
`unsafe` and supports Vulkan only, while this workspace forbids unsafe and the
target Mac uses Metal. Upload staging reuse is a separate pending slice; the
existing readback pool does not imply upload pooling.

### Adaptive playback availability (RT5)

Auto resolution with Settings ▸ Previews ▸ Adaptive Resolution While Playing (on by
default) or Fast Previews set to Adaptive Resolution steps Full → Half → Third → Quarter
(the ladder in `playback_budget.rs`; the paragraph below describes the original Half step;
`examples/playbench.rs` measures playback fps and stalls headless) can reduce to Half
of the normal Auto scale when native render-service p95 exceeds the displayed
frame budget. Each scale discards eight cold renders, then needs eight real
render completions with at least three quarters missing the budget by 10%.
This avoids paying for a resolution trial on isolated slow frames. Half is tried
once and retained only if its p95 is at least 10% faster than the Full reference.
A slower or inconclusive trial restores Full and locks that choice until the
playback/content state changes; idle always restores normal Auto. The trial is
bounded to 64 actual completions even if the rendering path keeps changing. Explicit resolution overrides,
cache-before-playback, other Fast Previews modes and remote workers keep their
existing behavior. Cache hits and obsolete completions do not train the budget.

The opt-in UI library benchmark exercises actual native viewer workers against
RT0's generated mograph project, without building or opening the application:

```sh
RT5_PROJECT=/absolute/path/to/target/bench/gpu/mograph-1080.ecproj \
RT5_REPORT=/absolute/path/to/rt5-paired.json \
CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p effectcraft-ui-egui --lib \
  adaptive_playback_frame_availability -- --ignored --nocapture --test-threads=1
```

Run in an exclusive GPU timing window. It runs Off/Adaptive/Adaptive/Off,
records the first 48 adaptation frames separately and reports p50/p95 of the
next 60 requested-frame availability times. Native Auto compositor selection,
readback and the bounded polling delay are included. This is a serial worker
availability measurement, not window presentation or a playback-fps guarantee.
All raw timings, chosen scales and the first reduction frame are retained.

### Progressive scrub deadline probe (RT5)

The opt-in `progressive_scrub_deadlines` UI library test sends 60 timeline
positions at 33 ms intervals, forward then backward, through real native viewer
workers. It compares Off/Adaptive/Adaptive/Off, warms both scales equally, and
records how many current frames finish before the next input, completion latency,
and final full-resolution refinement. This is a worker-deadline probe, not an
on-screen frame-rate measurement. Set `RT5_PROJECT` to a generated project with
at least 136 valid source frames and `RT5_REPORT` to the output JSON path. Keep
other GPU work and own builds stopped while timing it. The probe requests urgent
frames directly; it does not measure sparse neighbor prefetch or UI presentation.

### Native display readback worker probe

The ignored UI library test `native_display_worker_benchmark` compares the old
float readback with GPU display conversion followed by RGBA8 readback. Set
`RT_DISPLAY_PROJECT` to a generated project with at least 68 valid frames and
`RT_DISPLAY_REPORT` to the output JSON. Run in an exclusive native timing window.
It compares Off/On/On/Off at Full and Half through real viewer workers, checks a
forced-GPU output frame outside timing, warms Auto at each scale, then records
60 completion times, readback bytes and Auto's CPU/GPU decisions. It includes
worker completion and bounded polling, not window presentation. The renderer and
export precision remain unchanged; this measures final viewer transfer only.
