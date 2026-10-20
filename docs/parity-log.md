# Parity log

History behind [parity.md](parity.md): what landed when, and the audits before the current one.
parity.md's cells hold the current state only. This file is append-only and union-merged
(`.gitattributes`), so parallel branches don't conflict here: add a bullet under the row's
heading, or a new `## Update:` section at the end. Don't rewrite or reorder existing lines.

## Partial features

### EFF-5 GPU effects

- Until 10 October 2026: Fractal (its escape iteration needs f64: WGSL has none and Metal's fast math defeats double-f32 emulation) and the analysis / tool effects (Camera-Shake Deblur, Detail-preserving Upscale, Puppet, Rolling Shutter Repair, Warp Stabilizer, Mocha Shape, Roto Brush, Match Grain, Paint, Camera Tracker, Face Tracker) render on the CPU. M13.28 moved the remaining pixel effects (CC blur / glass family, Color Link, Cineon, HDR, Inner/Outer Key, Basic 3D…) and the geometry and audio generators (Lightning, Advanced Lightning, Beam, Lens Flare, Radio Waves, Vegas, Stroke, Scribble, Write-on, Paint Bucket, Eyedropper Fill, CC Glue Gun, CC Threads, Audio Spectrum / Waveform, Basic / Path Text) onto the GPU; M13.29 the particle effects (CC Particle World / Systems II, Particle Playground, CC Ball Action, CC Pixel Polly, CC Scatterize) and Curl Noise, and removed the Shatter wireframe, Foam extras, Timewarp matte, strong Warp Fisheye / Twist and custom `.ocio` fallbacks. The 13 audio filters stay on the CPU by design

## By area

### Layers

- Until 10 October 2026: motion blur of collapsed precomps (their own motion and the parent camera's) and of animated layer content (lock-aware layer commands, delete keeping children in place, step-by-step Arrange, Transform keeping depth and separated dimensions and links kept on duplicate / paste landed in M3.11; frame blending, collapse transformations and slip edit in M4.3–M4.4)

### Output

- Until 10 October 2026: Render Settings complete (field render + 3:2 pulldown, effects/solo/guide/depth/blending/blur overrides, time sampling, storage overflow), Output Module crop/ROI/resize, alpha modes, post-render actions, PCM formats, templates with defaults, render logs, Notify (M10.2); WebM VP9 key + inter frames with motion search, loop filter and rate control; M13.4: HEVC export (MP4 `hvc1`, `effectcraft-hevcenc`: Main / Main 10, IDR + P slices, quarter-pel motion, deblocking, bit-exact with ffmpeg) and AV1 export (MP4 `av01` and WebM, `effectcraft-av1enc`: 8/10-bit key + inter frames, quarter-pel motion, loop filter, bit-exact with libdav1d) with profile, level, bitrate / constant-quality and key-frame interval options; Opus SILK (NB/MB/WB, mid/side stereo) and hybrid (SWB/FB) modes chosen by bitrate and Audio/Voice tuning, with the Opus bitrate in the Output Module. HEVC/AV1 import decodes through FilmCraft (MP4/MOV/MKV/WebM); WebM VP9 alpha (BlockAdditions) imports as straight alpha. Left: HEVC/AV1 have no B-frames, multi-reference, SAO/CDEF/restoration or alpha (compression below mature encoders); Opus has no FEC/DTX and only 20 ms frames; Photoshop sequence output, overflow for movies only checks at file creation

### Import

- Until 10 October 2026: JPX / JBIG2 images and predefined non-Identity CJK CMaps inside PDF/AI files, Illustrator EPS relying on Adobe procsets, PSD 3D layers (M13.12: mesh shadings — free-form / lattice Gouraud triangles, Coons and tensor patches — and function-based shadings, CCITT G3 / G4 images, knockout / isolated transparency groups with group alpha and soft masks on the group's result, `/W2` vertical metrics, EPS text with embedded Type 1 / CFF or bundled fonts, and Create Shapes from Vector Layer keeping images as parented footage layers in paint order; M13.6: PDF/AI text with embedded TrueType / CFF / Type 1 / Type 3 fonts and standard-14 fallback to the bundled fonts, Flate / DCT / inline / stencil images with soft masks, Indexed and ICC alternates, luminosity / alpha soft masks, the 16 blend modes, tiling patterns, calculator functions, any page via `file.import page` and the Import dialog, clip groups kept by Create Shapes from Vector Layer as layer masks / Merge Paths; PSD smart-object perspective quads and placed-layer warps — named styles and custom quilt meshes — baked as placed; PDF / PDF-compatible AI / EPS vector footage with Continuously Rasterize, layered composition import and Create Shapes from Vector Layer, and PSD smart objects with embedded files landed in M13.2; PSD as footage/composition/retain layer sizes, SVG footage earlier)

### Automation

- Until 10 October 2026: `.jsxbin` (AUT-2: concave / self-intersecting `onDraw` fills, real images in `drawImage`, image controls and icon buttons landed in M13.11; ScriptUI resource strings, `onDraw`/ScriptUIGraphics, live `onChanging`, `Socket`, Essential Graphics hooks — `addToMotionGraphicsTemplate(As)`, `canAddToMotionGraphicsTemplate`, `exportAsMotionGraphicsTemplate`, `motionGraphicsTemplateName`, controller count/names — and Watch Folder landed in M13.7; the core object model landed in M14.4; ScriptUI windows/dialogs/dockable panels with `scriptui.*` agent commands, File ▸ Scripts install + sample scripts, and the effect plug-in API (EFF-6, WebAssembly) landed in M13.1)

### Shapes

- Until 10 October 2026: Lottie can't carry stroke taper/wave (stroke Taper and Wave, Dash 2/Gap 2/Dash 3/Gap 3 and radial-gradient Highlight Length/Angle landed in M13.5; pen tool for shape paths and vertex editing in M6.5)

### Compositions

- Until 10 October 2026: Mocha-style planar tracks for templates; nested comp markers on the precomp layer bar (Preserve frame rate / resolution when nested and nested comps showing nothing past their end landed in M4.9–M4.10; Pre-compose keeping the comp's settings and New Comp from Selection's dialog in M3.12; CMP-7: Essential Graphics mirrored and linked properties landed in M13.11; Font and uniform Scale controls, Composition ▸ Open in Essential Graphics, Save Frame As ▸ Photoshop Layers / ProEXR and the VR Comp Editor landed in M13.7; the marker dialog, Composition Flowchart, Essential Graphics with master properties, `.ectemplate` templates and Responsive Design — Time landed: CMP-6, CMP-7)

### Animation

- Until 10 October 2026: puppet depth beyond pins, recording, rigging and follow-through (G2: an angle's revolutions scrub and take typing in Effect Controls, the Timeline and Properties, #93; puppet pin recording with Record Options landed in M13.1; pin selection, rotate/scale handles, nulls for pins and Follow-Through in M13.14–M13.16), Wiggler/Smoother/Motion Sketch (motion-path handles and the graph editor transform box landed in M5.8; keyframe colour labels and Select Keyframe Label Group, Graph Editor snapping to markers / layer ends in M13.5)

### Text

- Until 10 October 2026: no extruded strokes (G2: the font menus list every installed family with its own styles — before, only a project asking for a font read the system fonts — and `text.fonts` / MCP `list_fonts` list them for agents; M13.12: the Variable Font Axes animator re-spaces the text — advances follow the animated axes; M13.6: variable font axes in the character style — `layer.setText variations`, the Character panel's Variable Font Axes fields — shape with HVAR / gvar advances and draw at that design-space position; OpenType features — stylistic sets, discretionary ligatures, contextual / stylistic alternates, swash, titling, ordinals, fractions, figure styles, true small caps / all small caps and superior / inferior glyphs with faux fallback — per character with the Character panel's OpenType popup and `text.fontFeatures` landed in M13.2; vertical Roman / Tate-Chu-Yoko, forced LTR paragraphs, caret on animated and path text, Variable Font Axes and Lottie style runs landed in M13.5; extruded, bevelled text in M7.6; per-character styles, paragraph settings, on-canvas editing and the `sourceText` style API in M9.9–M9.10)

### Web

- Until 10 October 2026: No shared-memory threads inside one engine instance (the decided design is one engine instance per worker; a threaded build needs nightly `build-std`) (M13.30: Render Queue, analyses and Content-Aware Fill render on each job worker's own WebGPU device in passes, particles and Advanced 3D read back under keys, layer buffers in the browser's disk cache; browser storage, Web Audio, Web Worker renders/analyses, WebGPU viewer and offline install landed in M15.2; viewer frames in frame workers fed by project diffs, Roto Brush propagation in a worker, non-blocking `wait: true` jobs and a browser Media Browser (File System Access folders, browser storage) in M13.10; WEB-1 in M13.24: GPU effects and the GPU compositor in the frame workers on their own WebGPU devices with deferred readbacks (frames render in passes; Backend Auto per comp), the disk cache in the Origin Private File System (written by the workers with sync access handles, LRU under the settings' limit, served after a reload), the storage manager (Settings ▸ Disk ▸ Browser Storage, `storage.info` / `storage.persist` / `storage.clear`), and a WGSL constant Chrome rejected (it disabled every GPU kernel in Chrome) fixed)

### 3D

- Until 10 October 2026: reflections of other layers (Advanced 3D surfaces reflect only the Environment light; Material Options ▸ Appears in Reflections is stored but has no visible effect yet, #261), multi-view layouts, the Extended Viewer for Advanced 3D comps (Classic 3D Extended Viewer landed in M13.5 UI completion); collapsed precomps of another size seen through the parent's camera render (fixed in M13.2); stereo rigs, orbit nulls, lights controlled by the camera, cameras/lights from glTF models, environment backgrounds, Advanced 3D motion blur, blend modes and track mattes landed in M7.7; Classic 3D iris-shaped bokeh with highlights, progressive depth of field on tilted layers and the focus-link commands landed in M13.5; Advanced 3D depth of field with the iris and highlight options, collapsed precomps as real Advanced 3D geometry and extruded text/shape strokes landed in M13.8; Advanced 3D (glTF/OBJ models, primitives, extruded text and shapes, PBR, image-based light, shadow maps, GPU rasteriser) in M7.4–M7.6; Advanced 3D end to end on the GPU (motion blur, iris depth of field, compositing) in M13.11

### Effects

- Until 10 October 2026: GPU versions of the remaining effects (280 run on the GPU since M13.29, EFF-5: M13.28 added the remaining pixel effects and the geometry and audio generators (Lightning, Advanced Lightning, Beam, Lens Flare, Radio Waves, Vegas, Stroke, Scribble, Write-on, Paint Bucket, Eyedropper Fill, CC Glue Gun, CC Threads, Audio Spectrum / Waveform, Basic / Path Text); M13.29 added the particle effects (CC Particle World / Systems II, Particle Playground, CC Ball Action, CC Pixel Polly, CC Scatterize, Curl Noise) and removed the fallbacks of Shatter's wireframe views, Foam's extras, Timewarp's Matte Layer, strong Warp Fisheye / Twist bends and custom `.ocio` configs; M13.23 added the 3D Channel and Immersive Video families, Apply Color LUT, the OCIO effects, Color Profile Converter and the simulations' render passes (Card Wipe included); M13.22 added the CC light family, the CC transitions, Block Dissolve, Radial Shadow, CC Bender / Blobbylize / Cylinder / Sphere / Spotlight / Environment, 3D Glasses, Numbers, Timecode and the time effects joined the 166 of M13.13; Fractal and the analysis / tool effects still render on the CPU) and the missing controls listed as partial in [effects.md](effects.md) (every After Effects effect exists since M9.11, M12.5 and M12.6; parameter names, order, twirl-downs, popups, units and defaults were aligned in M9.12)

### Interface

- Until 10 October 2026: more Learn tutorials and pixel-level fidelity of dialogs (G2 on 9 October: the in-window menu bar works from the keyboard (Alt, arrows, Enter, Escape) and the app's shortcuts wait while a menu is open, #279; the Timeline's composition marker bin and numpad * add comp markers, #275; Settings ▸ Appearance ▸ UI Scale (75–200 %, beyond After Effects, which follows the system's scaling), #284; G2 on 7 October: Project items, files and effects dropped on the Composition viewer, Project items dropped in the Timeline between layers and at a time, #85, #88, #89; popup menus moved to fit the window take clicks, so Render Queue templates apply, #117; View ▸ New Viewer landed in M13.32; an automation id on every control of Composition and Solid Settings, Group / Ungroup Shapes in the Layer menu, Window ▸ Learn, the flowchart shortcuts and Reveal in Explorer landed in M3.10 and M3.13; the Home ▸ Templates gallery (eight original built-in templates, user templates from File ▸ Save as Template…) and View ▸ Simulate Output ▸ My Custom RGB… landed in M13.25; Timeline layer reordering by drag, a non-snapping viewer pan, a working rename field and twirl arrows, and the full set of property reveal shortcuts — double presses, Alt+Shift keyframes, Ctrl+` — landed in M13.17–M13.20; the Home ▸ Learn tab with interactive tutorials and a UI fidelity pass landed in M13.5 UI completion; visual editors for Lumetri RGB / hue-saturation curves, Colorama's output cycle wheel, Glow's colour map and Reshape's correspondence points (viewer handles), a scrolling Preview panel and AE-style Composition / Timeline tabs (close, label swatch, viewer lock) landed in M13.10; Timeline outline and Project panel columns scroll horizontally, the Layer Style dialog, ROI resize handles, Pan Behind snapping and 3D Reference Axes landed in M13.5; native macOS menu bar, Timeline columns/search/reveal-add, Home screen with recent projects and all AE workspaces landed; viewer rulers/snapping/channels/snapshots landed in M0.13)

### Project

- Until 10 October 2026: OCIO displays beyond the built-in tone map; relative footage paths and relinking when a project moves (unsaved-changes prompts, a modified mark that follows undo and project files that name their version and always reopen landed in M3.9 and M3.14; auto-save and crash recovery in M3.7; Color Engine with OCIO/ACES working spaces, HDR compand/tone mapping, Rec. 2100 PQ/HLG output, Feet + Frames, display colour management, Simulate Output and the locked viewer landed in M7.7; proxies and Interpret Footage fields / pixel aspect / alpha guess landed: PRJ-8, PRJ-3; image sequence import options (Sequence, Force Alphabetical Order, frame rate, picked ranges, folders, the browser), missing-frame placeholders and Interpret Footage Start Timecode landed: #297)

### Masks & roto

- Until 10 October 2026: (audit estimates; Roto Brush 2.0 / 3.0's trained model, MobileSAM in the swappable `segment` module, has since landed in M13.35; variable-width mask feather points with the Mask Feather tool landed in M13.5; mask tracking and Mask Interpolation landed in M6.6; Roto Brush & Refine Edge with graph-cut segmentation, flow propagation, edge matting, decontamination and Freeze in M6.7)

### Preview

- Until 10 October 2026: GPU effects for the audio-driven effects and the stepped particle systems; content-keyed RAM preview frames (an edit drops every comp's), audio scrubbing, more snapshot slots (G2 on 7 October: Audio, Lock and Shy switches keep the cached frames and playback with audio waits for frames instead of skipping them, #103; running out of video memory renders the frame on the CPU and keeps fewer GPU frames instead of crashing, #106; RAM preview keys with the render options, least-recently-shown eviction, Auto resolution while playing, Cache Before Playback that fits, an achieved-fps readout, Cache Frames When Idle and purges that clear what they name landed in M4.5–M4.8; 3D channel, VR, OCIO, Card Wipe and the simulations' render passes run on the GPU since M13.23; Advanced 3D layers with blend modes / track mattes / Preserve Transparency and environment backgrounds composite on the GPU since M13.22; pooled GPU textures and fused quantisation since M13.13: Lower Third GPU warm 21 → 4 ms/frame, and Auto now picks the CPU or the GPU per comp from measured frame times; Advanced 3D runs — raster, motion blur, iris depth of field, compositing — and wireframes run on the GPU since M13.11: CPU warm 3394 → GPU warm 290 ms/frame at 1920×1080 (11.7×; Half 968 → 85 ms), GPU ≠ CPU on 0.005 % of pixels, on an M4 Pro under load; Classic 3D bokeh depth of field — iris shapes, highlights, fringe, progressive blur on tilted planes — runs in WGSL since M13.6: 3D Showcase GPU warm 205 → 54 ms/frame at full size on an M4 Pro; the Preview panel's five shortcuts with their own Include / Loop / Cache Before Playback / Range / Play From / Frame Rate / Skip / Resolution / Full Screen / stop options and `playback.settings.get/set` landed in M13.8; Classic 3D runs and adjustment layers composite on the GPU since M12.7; persistent disk cache with the blue cache bar landed; region of interest, snapshots, exposure and Fast Previews landed in M0.13)

### Tracking

- Until 10 October 2026: face tracking's trained model (MediaPipe Face Landmarker, M13.36) is an optional download; without it the classical (skin model + feature components + shape model) fitter is weak on profile views and occluded faces; Rolling Shutter Ripple is approximated by Subspace Warp's mesh density (face tracking (Outline Only / Detailed Features with Face Track Points and Extract & Copy Face Measurements), Subspace Warp's content-preserving mesh warp on subspace-smoothed trajectories, and radial lens distortion (k1, k2) in the camera bundle adjustment with Undistort Footage landed in M13.3; Rolling Shutter Repair in M9.11; point tracker, mask tracking, Warp Stabilizer and the 3D Camera Tracker in M6.x / M12.5 / M12.6)

## Audits and updates

Moved verbatim from parity.md on 10 October 2026. Append new `## Update:` sections at the end.

### Previous audit (commit `58163a2`, 3 October 2026, evening): ≈ 98%

87 done / 5 partial / 0 missing.

### Previous audit (commit `978e8d7`, 3 October 2026): ≈ 94%

80 done / 12 partial / 0 missing.

### Previous audit (commit `fa26ad9`, 2 October 2026, late): ≈ 93%

By area: Layers 98%, Output 95%, Compositions 95%, Automation 96%, Paint 95%, Text 95%, Import 96%,
Animation 94%, Masks 94%, Preview 94%, Interface 93%, Shapes 93%, 3D 91%, Audio 90%, Project 90%,
Tracking 96%, Effects 85%, Web 65%.

What is left, in priority order: the web app's depth (threads, storage, audio, non-blocking
renders); stroke taper/wave and multi-segment dashes; variable mask feather points; camera iris/bokeh and focus-link commands; 
approximated effects (Key Cleaner) and Liquify's viewer brush; JPX / JBIG2 images in
imported PDF/AI files (mesh shadings and CCITT landed in M13.12) and Illustrator procset EPS; and codec depth (B-frames, SAO/CDEF; HEVC and AV1 export and Opus SILK/hybrid landed in M13.4). Face tracking and a true Subspace Warp
landed in M13.3. ScriptUI (dialogs that block `show()`, palettes, dockable ScriptUI panels, File ▸ Scripts install
and sample scripts), puppet pin recording and the remaining "better than After Effects" items — a
versioned effect plug-in API with sandboxed WebAssembly plug-ins ([plugins.md](plugins.md)),
branching undo history in the History panel, and a bit-identical rendering test across runs and
thread counts — landed in M13.1 (GPU particles landed in M12.7). Render Queue field render, crop/resize, templates and logs, and VP9 inter frames landed in M10.2.

The sections below are the original audit (morning of 2 October, ≈ 64%) and its updates, kept for
history.

## Summary

| Measure | Value |
|---|---|
| **Feature parity, weighted by tier** (P0 ×3, P1 ×2, P2 ×1) | **≈ 64%** |
| Feature parity, unweighted | ≈ 58% |
| P0 (it isn't After Effects without it) | ≈ 75% |
| P1 (professional daily use) | ≈ 38% |
| P2 (long tail) | ≈ 17% |
| Features done / partial / missing | 18 / 60 / 14 of 92 |
| **Effects** | **298 of 298** After Effects 2026 effects by name (100%, after M9.11, Warp Stabilizer and the 3D Camera Tracker; was 257) |
| Remaining work | ≈ **167 agent-hours** (139 h of features + ≈ 20% for 1:1 polish against After Effects) |
| Wall-clock estimate | ≈ **42–50 hours** with five agents working in parallel and one integrating; ≈ 31–35 h for P0 + P1 only |

"Agent-hours" are hours of one autonomous coding agent (Claude Opus 5.5) working in its own git
worktree with tests. The estimate is calibrated on this repository's own history: Classic 3D
(cameras, lights, shadows, depth of field, views, tools) took one agent about 2.7 h; timeline
depth (graph editor, keyframe dialogs, time remapping, pen and masks) about 2.3 h; 85 effects
about 1.2 h; the After Effects menu bar about 1.3 h; layer styles about 0.6 h; the web build
about 0.8 h. What is left is harder on average (tracking, roto, puppet, GPU, Advanced 3D), and
sequential chains (tracker → mask tracking → Warp Stabilizer → 3D camera tracker; GPU
compositor → GPU effects) limit how much can run in parallel.

Work in progress at the time of the audit, not yet counted: motion tracking, preferences and the
keyboard shortcut editor, auto-save and crash recovery, paint and puppet tools, Lottie import and
export.

## Update after the second wave (same day, commit `e1f7241`)

Motion tracking (1-, 2- and 4-point, stabilize, corner pin), paint (Brush, Clone Stamp, Eraser
with write-on) and the puppet tools, preferences, a keyboard shortcut editor that rebinds,
auto-save and crash recovery, Lottie import and export, Effect Controls widgets (angle dial,
point crosshair, eyedropper, curves and levels editors, effect copy/paste) and 20 more menu
commands landed. Re-scoring only the features they touch:

| Measure | Before | After |
|---|---|---|
| Feature parity, weighted | 63.7% | **≈ 70%** |
| P0 / P1 / P2 | 75% / 38% / 17% | 78% / 56% / 17% |
| Remaining, audit estimate | 167 agent-hours | ≈ 141 agent-hours |

**Calibration.** The audit priced this wave at about 21 agent-hours; five agents delivered it in
about 1.5 hours of wall-clock time (roughly 6 agent-hours of actual work), though some features
landed at 80–90% rather than fully done. So the per-feature estimates above are conservative.
Remaining wall-clock time with five parallel agents is therefore somewhere between **≈ 12–15
hours** at the pace observed so far and **≈ 35–40 hours** if the audit's figures hold. The
hardest remaining systems (Roto Brush, Warp Stabilizer, the 3D camera tracker, Advanced 3D, GPU
rendering, on-canvas text editing) are where the conservative figure is most likely to be right.

## Update at the end of 2 October 2026

A third wave landed: viewer interactions (snapping, rulers, channels, exposure, snapshots, region
of interest, shape pen and pen-tool family, motion-path handles, graph editor transform box),
on-canvas text editing with per-character styles and paragraph settings, render fidelity (8/16/32
bpc, Rec.709/Rec.2020/P3 working spaces and linear blending, frame blending with optical flow,
collapse transformations, slip edits), a GPU compositor with 16 GPU effects, Wiggler/Smoother/
Motion Sketch, the marker dialog, precompose options and the comp navigator, Project panel folder
moves/rename/columns/thumbnails, drag-and-drop docking with floating panels, and the tracking
library groundwork for mask tracking and Warp Stabilizer.

| Measure | Morning | Now |
|---|---|---|
| Feature parity, weighted | 63.7% | **≈ 79%** |
| Unweighted | 57.8% | ≈ 74% |
| P0 / P1 / P2 | 75% / 38% / 17% | 87% / 67% / 21% |
| Remaining (audit-scale agent-hours) | ≈ 167 | ≈ 100 |
| Wall clock, five parallel agents | 42–50 h | **≈ 8–10 h at today's measured pace; ≈ 25 h by the audit's conservative figures** |

What is left is concentrated in large systems: the Warp Stabilizer and mask-tracking UI on top of
the new tracking library, and wasm threads. (The missing effect categories, 3D Channel, Immersive
Video and OCIO, landed in M9.11; the scripting object model in M14.4; Roto Brush and Refine Edge in
M6.7; native macOS menus, the Composition Flowchart, Timeline column/search depth, the Home screen
and every After Effects workspace in the UI-polish wave; PSD/SVG import, WebM/WAV/AIFF output and
the disk cache in the formats wave; the 3D Camera Tracker in M12.6.)

## Update: M13.5 long-tail polish

Stroke Taper / Wave / multi-segment dashes and the radial gradient highlight (SHP-2);
variable-width mask feather points (MSK-1; mask motion blur verified by tests); the Adaptive
Sample Limit driving per-layer motion-blur samples (LYR-10); Classic 3D depth of field with
the camera's iris shape, rotation, roundness, aspect ratio, diffraction fringe and highlights,
progressive blur on tilted layers, and Link Focus Distance to Point of Interest / to Layer and
Set Focus Distance to Layer (3D-1); Timeline outline and Project panel column scrolling;
keyframe colour labels and Select Keyframe Label Group (ANM-1); vertical Roman text,
Tate-Chu-Yoko, forced LTR paragraphs, the editing caret on animated / path text and Lottie
style runs; the Layer Style dialog (LYR-9); Graph Editor snapping to markers and layer ends
(ANM-4; the reference graph existed); viewer ROI resize handles, Pan Behind snapping and 3D
Reference Axes; Animate Text ▸ Variable Font Axes. Remaining in these rows: Lottie can't carry
taper/wave (the Variable Font Axes animator changes advances since M13.12, the Character panel's
axes since M13.6; Advanced 3D's DOF has iris shapes since M13.8).

## Update: M13.8 Preview panel, Align panel, Advanced 3D completion

- **Preview panel (PRV-1)**: Shortcut popup (Spacebar, Shift+Spacebar, Numpad 0, Shift+Numpad
  0, Alt+Numpad 0), each shortcut with its own saved options (settings `preview`): Include
  Video / Audio / Overlays / Layer Controls, Loop, Cache Before Playback, Range (Work Area,
  Work Area Extended By Current Time, Entire Duration, Play Around Current Time with pre/post
  roll), Play From (Range Start / Current Time), Frame Rate (Auto or a rate), Skip, Resolution
  (Auto, Full … Quarter, Custom), Full Screen, "If caching, play cached frames" and "Move time to
  preview time". `playback.settings.get/set`; playback follows the plan (range, start, skip grid,
  rate, loop, cache-first, audio-only, resolution override, hidden overlays / layer controls,
  maximized viewer); the keys trigger their shortcut (`playback.toggle {shortcut?}`,
  `playback.ramPreview`, `playback.preview.shiftSpacebar|shiftNumpad0|altNumpad0`). Full Screen
  maximizes the Composition panel rather than taking over the display.
- **Align panel (UI-1)**: `layer.align {edge, to: composition|selection}` and `layer.distribute
  {mode}` (edges or centres) on content bounds through transform and parents, one undo step;
  Align Layers to dropdown and the Distribute Layers row.
- **Advanced 3D (3D-3)**: depth of field with the camera's iris and highlight options (the
  Classic 3D bokeh kernel, by each pixel's depth); collapsed precomps add their nested layers
  as real geometry lit by the parent (2D collapsed precomps draw nested 3D layers with the
  parent's renderer); text and shape strokes extrude as bevelled meshes in paint order.

## Update: M13.22 GPU effects, part A

- **GPU ports (EFF-5)**: 30 more effects run on the GPU (196 GPU effects), each matching the
  CPU oracle within 1/255 (8 bpc) / 1e-3 (32 bpc) on every tested pixel, directly on a buffer
  (full and half resolution, as adjustment) and composited at 8 and 32 bpc:
  - the CC light family: CC Light Rays, CC Light Burst 2.5, CC Light Sweep, CC Light Wipe
    (`gpu::fx_light`);
  - transitions and perspective (`gpu::fx_transition`): Block Dissolve (block indices computed
    on the CPU per column / row sub-sample, so the hash sees the CPU's integers), CC Glass Wipe,
    CC Grid Wipe, CC Image Wipe, CC Jaws, CC Line Sweep, CC Radial ScaleWipe, CC Scale Wipe, CC
    Twister, CC WarpoMatic, Radial Shadow, CC Bender, CC Blobbylize, CC Cylinder, CC Sphere, CC
    Spotlight, CC Environment, 3D Glasses; other layers they read (gradient, reveal, backside,
    environment, stereo views) are fitted on the CPU and uploaded;
  - Numbers and Timecode (`gpu::fx_text`): the glyph coverage (fill and stroke ring) is
    rasterised on the CPU with the effect's own stroke font and composited on the GPU with the
    box, fill and stroke;
  - time (`gpu::fx_time`): Time Difference, Time Displacement, CC Force Motion Blur, CC Wide
    Time, Pixel Motion Blur and Timewarp fetch their frames through the host as Echo does and
    combine them on the GPU (weighted sums one frame per pass, in the CPU's order); Time
    Displacement's per-pixel times and Timewarp's motion vectors (block matching and smoothing)
    are computed on the CPU, Timewarp's Whole Frames / Frame Mix / Pixel Motion frame building
    and shutter average on the GPU.
- **Fallbacks and deviations**: Timewarp with a Matte Layer renders on the CPU
  (`gpu_supported`); Card Wipe (the 3D card renderer shared with Card Dance and Shatter) and the
  3D Camera Tracker stay on the CPU. CC Environment now fades nearly transparent environment
  texels to black continuously (colour / max(alpha, 1e-3), on both paths) instead of a hard
  1e-6 un-premultiply cut-off, which turned the filtered map's float noise into colour. CC
  Sphere / CC Environment keep their longitude on the side of the ±180° seam the ray's sign
  puts it (a GPU's approximate `atan2` can land across it).
- **Advanced 3D compositing on the GPU (3D-3, PRV-3)**: runs with blend modes, track mattes
  (2D, or 3D mattes drawn through the camera) or Preserve Transparency no longer read back:
  `Renderer::split_adv_run` hands `draw_run`'s split path to the GPU (main scene, then each
  special layer far to near, hidden behind the nearer main scene and composited through the 2D
  kernels), and Environment Light Background skies draw in a kernel in Classic and Advanced 3D
  comps. The GPU walk matches the CPU compositor on the same rasters exactly at 8 and 32 bpc,
  with only the frame's own readback.

## Update: M13.13 GPU performance and the remaining GPU ports

- **Small comps on the GPU (PRV-3)**: the GPU frame allocated (and zero-initialised) several
  full-frame RGBA f32 textures per layer; working textures now come from a pool (reused once
  every encoder that could read them is submitted), transparent inputs share one zero texture,
  readback staging buffers are reused and the 8/16 bpc quantisation after each layer is fused
  into the layer's composite. `bench --gpu` on an M4 Pro under load (both binaries run back to
  back, median of 15): Lower Third 1920×1080 GPU warm 21.0 → 4.1 ms/frame (viewer 11.1 → 2.6;
  Half 10.1 → 3.1), EffectCraft Intro 27.5 → 7.8 (viewer 21.7 → 3.6), 3D Showcase ≈ 95 → ≈ 60
  (viewer ≈ 80 → ≈ 40), Adjustment Layers 93 → 36. In steady state the only transfer left is
  the frame's own readback (0.0 MB uploads per frame on every demo comp; the adjustment
  footprint is now cached and uploaded once).
- **Auto picks per comp**: `Backend::Auto` (Mercury GPU Acceleration) times each comp on both
  compositors (`render::AutoPick`: per comp, scale and path; warm-up, capped stalls, re-probe
  every 48 frames) and renders on the faster one, in renders and in the viewer. A light comp
  stays on the CPU, which composites only the layers' bounds: Lower Third Auto 0.8 ms/frame
  (CPU warm 1.7, GPU warm 4.1, which is mostly the 32 MB readback).
- **GPU ports (EFF-5)**: Warp, Bezier Warp, Reshape, Smear, CC Bend It, CC Page Turn, Cartoon,
  Color Emboss, Circle, Ellipse, Iris Wipe, Bevel Alpha, Bevel Edges and Gaussian Blur (Legacy)
  run on the GPU (166 GPU effects); Median, Median (Legacy) and Dust & Scratches take any radius
  (a sliding 512-bin histogram per row segment above radius 4); CC Power Pin's Perspective
  below 100 % runs on the GPU. All match the CPU oracle within 1/255 (8 bpc) / 1e-3 (32 bpc) on
  every tested pixel except: Smear and Reshape allow 0.1 % (f32 point-in-outline tests on
  boundary pixels; measured 0); Warp's Fisheye and Twist bent past 50 % or with a Horizontal /
  Vertical Distortion render on the CPU (`gpu_supported`), since their Newton inverse wanders
  chaotically near the crease and f32 settles on other pixels.

## Update: M13.11 Advanced 3D on the GPU, Essential Graphics mirrors, ScriptUI paint

- **Advanced 3D on the GPU (PRV-3, 3D-3)**: a whole Advanced 3D run renders on the device
  (`gpu::adv3d`, `Renderer::prepare_adv_run`, `Accelerator::render_3d`): every motion-blur
  sub-sample's scene is rasterised (depth buffer, PBR, image-based light, shadow maps), then
  compute kernels resolve the 2×2 supersampling, average the sub-samples (nearest depth), apply
  the depth-based iris depth of field (the Classic 3D bokeh row spans and prefix-sum gather,
  highlight boost, progressive levels) and composite over the GPU canvas without a readback.
  Meshes from extruded text and shapes, cards, glTF models and collapsed precomps all go through
  it. Layers with blend modes, track mattes or Preserve Transparency (and environment
  backgrounds) keep the CPU's 2D compositing path, their scenes still rendered by
  `render_3d`. Wireframe-quality outlines draw on the GPU (exactly the CPU's pixels). The CPU
  stays the oracle: GPU-vs-CPU tests on lit scenes with shadows, environment light, DOF (hexagon
  with fringe and highlights, Fast Rectangle, radii at the 48 px cap), motion blur, extrusions
  and collapsed precomps agree within 1/255 on ≥ 99 % of pixels (measured: ≤ 0.01 %; the rest
  are silhouette pixels the two rasterisers cover differently), and the GPU post kernels match
  the CPU post-processing of the same GPU raster on ≥ 99.9 % (measured: all pixels). Fixed on
  the way: NaN environment lookups for straight up/down normals on Metal (`atan2(0, 0)`).
  `bench --gpu --adv3d` (1920×1080 Advanced 3D comp: PBR primitives, bevelled extruded text, a
  shadow-casting spot, point and environment lights, hexagonal-iris DOF with highlights,
  8-sample motion blur): CPU warm 3394 → GPU warm 290 ms/frame at 1920×1080 (11.7×; Half 968 → 85 ms), GPU ≠ CPU on 0.005 % of pixels, on an M4 Pro under load.
- **Essential Graphics mirrors (CMP-7)**: adding a property that is already in the panel adds a
  *mirror* (its own name and place; one value in the comp and in every instance — editing,
  reverting or pushing either acts on both; Font / Scale `as` controls mirror per kind);
  `essential.addMirror`. A property control can also drive further properties of the comp
  (`essential.linkProperty` / `unlinkProperty`): links follow the main property both ways and
  take each instance's value. Panel badges and a ⋯ menu (Add Mirror, Link Selected Property,
  Unlink); `addToMotionGraphicsTemplate` on a property already present adds a mirror, and
  mirrors count as controllers.
- **ScriptUI (AUT-2)**: `fillPath` fills any path (non-zero winding trapezoid tessellation:
  concave, self-intersecting, holes) instead of egui's convex-only polygons;
  `ScriptUI.newImage` / ScriptUIImage hold an image file or embedded PNG/JPEG bytes, drawn by
  `drawImage`, image controls and icon buttons; script-host threads are compiled out of the
  web build (no dead code warnings).

## Update: M13.5 UI completion & fidelity

**Extended Viewer** (Settings ▸ 3D, the viewer's Extended Viewer button, `view.extendedViewer`):
custom 3D views (and the Active Camera view with Draft 3D on) render the visible pasteboard
around the comp frame (the frame is outlined) so 3D layers reaching past the frame stay
visible; Classic 3D planes project through the offset canvas natively, Advanced 3D comps show
the frame only. **Home ▸ Learn**: three original interactive tutorials (Animate a title, Track
and attach, Make a 3D scene) whose steps highlight their UI target by automation id, advance when
the user runs the step's command, and can be performed with Show me (`learn.list`,
`learn.start`, `learn.step`, `learn.stop`, `learn.state`). **UI fidelity pass** against After
Effects 2026 captures: compact 19 pt Timeline / Project rows, flat rows with dark hairlines,
dark switch wells, the selected layer / item name in a light cell, label-coloured bars muted
toward grey, the time navigator with blue end handles and the work area bar (with the cache
bar under it) below the ruler, open twirl chevrons, a 16 pt current-time display, the
workspace bar order (Default, Review, Learn, Small Screen, Standard) and a single breadcrumb row
above the viewer. Not yet: Composition Settings / Render Queue / Settings dialogs compared
pixel by pixel (no reference captures of them yet).

## Update: M13.6 panels and content tools

The last placeholder panels became real panels with automation ids: **Lumetri Scopes**
(waveform RGB / Luma / YC, vectorscope YUV / HLS, histogram, parade RGB / YUV; Rec. 601 / 709 /
2020, 8-bit or float scale, clamp; `scopes.analyze`), **Footage** (double-click footage: its own
time ruler, play, In/Out, Overlay Edit and Ripple Insert Edit; `footage.*`), **Media Browser**
(desktop file system, favourites, thumbnails, import and drag to the Project panel or timeline;
`mediaBrowser.*`), **Metadata** (codec, size, rate, duration, colour profile, file dates; item
and project comments; `item.metadata`) and **Progress** (every render and analysis with
progress and cancel; `jobs.*`). **Content-Aware Fill** (panel and Layer ▸ New ▸ Content-Aware
Fill Layer): Object / Surface / Edge Blend, Work Area / Entire Duration, Alpha Expansion,
Lighting Correction and a reference frame from a layer, rendered to a PNG sequence in a Fill
layer above the source as a background job. **Scene Edit Detection** (markers, split, split and
precompose), **Auto-trace** (alpha / RGB / luminance, current frame or work area, to the layer
or a new layer) and **Align Video to Data** (JSON / CSV / TSV time keys) replace their disabled
menu entries. Not yet: creating a reference frame by editing a still (After Effects hands it to
Photoshop), Content-Aware Fill's learned model (ours is PatchMatch + flow propagation), and Media
Browser in the web app (landed in M13.10: browser storage and File System Access folders).

## Update: M13.12 import and text leftovers, test hardening

- **PDF / AI**: mesh shadings — free-form and lattice-form Gouraud triangle meshes (types 4, 5),
  Coons and tensor-product patch meshes (6, 7, tessellated to triangles) — and function-based
  shadings (type 1, with multi-input sampled functions), for `sh` and shading-pattern fills,
  rasterised into image nodes; CCITT Group 3 (1-D and 2-D) and Group 4 images from ITU-T T.4 /
  T.6 (checked against an independent encoder's output); knockout and isolated transparency
  groups, the current alpha / blend mode / soft mask applied to a transparency group XObject's
  combined result, and blend modes inside clipping groups reaching the backdrop; `/W2` / `/DW2`
  vertical metrics and CMap `WMode`. Not done: JBIG2 images (deferred) and predefined CJK CMaps (no permissively licensed
  pure-Rust source of the CMap data; Adobe's CMap resources are Adobe assets).
- **EPS**: text through the PostScript interpreter — `findfont` / `scalefont` / `makefont` /
  `selectfont` / `setfont`, `show`, `ashow`, `widthshow`, `awidthshow`, `xshow` / `yshow` /
  `xyshow`, `kshow`, `glyphshow`, `charpath`, `stringwidth`, re-encoded fonts (`definefont`
  copies with a new `/Encoding`) — with embedded Type 1 and CFF programs or the bundled fonts.
- **Create Shapes from Vector Layer**: images become PNG footage layers parented to the shape
  layer, and the shapes between images are split into layers of their own so the stack keeps
  the document's paint order; soft masks and clipped images are reported.
- **Text**: the Variable Font Axes animator re-spaces the text (advances at the animated
  design-space position), not only the outlines.
- **Hardening**: PDF / EPS parsers survive truncated and corrupted files (fixed an
  out-of-range stream slice and unbounded PostScript recursion); WebAssembly plug-ins get a
  memory cap, a manifest length limit and slider schema checks, with tests for missing exports,
  wrong signatures and versions, fuel exhaustion, traps and bit-identical output; script
  `Socket` validates ports, and ScriptUI resource strings accept trailing array commas.

## Update: plug-ins and extensions as in After Effects; ease presets become a ScriptUI panel

EffectCraft's core keeps After Effects parity; what After Effects users get from third parties is
an extension, on the same extension points After Effects has ([plugins.md](plugins.md) compares
them: scripts, ScriptUI panels, effect plug-ins, with a guide to writing a panel).

- **ScriptUI panels** in `Scripts/ScriptUI Panels/` of the settings folder are listed at the
  bottom of the Window menu and dock like any panel (as before); the panels open when the app quits
  now open again at the next launch (`window.restoreScriptPanels`), as After Effects reopens its
  workspace's panels.
- **Scripting API** (parity fixes the Ease Presets panel needed): `app.settings` lasts between
  runs (`script_settings.json`, `script.settings.get` / `save`); per-key methods
  (`setTemporalEaseAtKey`, `setInterpolationTypeAtKey`, `removeKey`, the continuous / auto-Bezier /
  roving setters) no longer change the user's key selection; `keyInTemporalEase` /
  `keyOutTemporalEase` report linear and auto-Bezier sides as they play, and
  `keyIn/OutSpatialTangent` the tangents the motion path plays; `selectedProperties` includes the
  properties of selected keyframes; `isInterpolationTypeValid` is false for Linear and Bezier on
  values that only hold.
- **Ease presets** (#254), modelled on a third-party panel, moved out of the core: they are the
  bundled ScriptUI panel `extensions/scriptui-panels/Ease Presets.jsx` (Window ▸ Ease
  Presets.jsx), written only against the public scripting API. A curve is the out side of the
  first key and the in side of the second, in the Keyframe Velocity dialog's terms (influence in
  percent, speed relative to the segment's average speed); clicking a preset eases every pair of
  neighbouring selected keyframes in one undo step, per dimension and along the motion path for
  spatial properties, leaving the keys' other sides as they play. The twelve built-in curves are
  unchanged; user presets (Save, Rename, Delete) are kept with `app.settings`, and those saved by
  the core panel of v0.6.0 (`ease_presets.json`) move there the first time settings load. The
  panel shows the working curve as a drawing and its four numbers as fields (the value graph's
  handles don't drag: ScriptUI panels get no mouse-drag events here). The `keys.easePreset.*`
  commands and `EaseCurve` are gone. Not yet: curves with more than one bend (bounce and elastic
  shapes), preset import / export files.
- **Snapping options** (#265): the Tools bar's menu has After Effects' two options, Snap Edges
  Extended and Snap to Features in Collapsed Compositions and Text Layers (the layers inside
  collapsed precomp layers snap); the per-feature toggles are gone and every feature snaps.

## Update: M5.9–M5.14 keyframes, M3.15 clipboard, M12.8 responsiveness

- **The app froze every 10 s on Windows** (M12.8): the cache budgets' memory reading started
  PowerShell on the UI thread and waited about a second for it. It runs on a background thread
  now, every 30 s.
- **Ctrl+C / Ctrl+X / Ctrl+V** (M3.15) reach Edit ▸ Copy, Cut and Paste in the app (the window
  layer sends them as clipboard events, which the shortcut dispatcher ignored), and copying puts
  a line on the system clipboard so Ctrl+V always arrives.
- **Timeline keyframes** (M5.9–M5.12): a drag follows the pointer to the end (it let go after a
  frame or two), moves every selected key as one undo step, never deletes keys it passes over,
  moves keys of time-stretched layers with the pointer, and Shift snaps to the current time,
  keys, layer ends and markers with a time / offset tooltip; Shift+click toggles selection,
  Ctrl+click switches Linear ↔ Auto Bezier, Ctrl+Alt+click toggles Hold; keys of locked layers
  can't be selected; Toggle Hold off restores Bezier; double-click edits a key's value; Select
  Equal / Previous / Following Keyframes; roving keys draw as dots.
- **Graph Editor** (M5.11): a drag moves every selected key in time and value from where it was
  grabbed, Shift keeps one axis; the transform box and Alt-drag scaling never merge keys on the
  way and move them however slowly they are dragged; the same Shift / Ctrl / Ctrl+Alt clicks.
- **Paste** (M5.13): keys copied from several layers paste onto as many layers in order; Paste
  Reversed Keyframes follows Paste's rules (stretch, target property, Hold-only properties).
- **J / K and Info** (M5.14): J / K also stop at the work area; the Info panel shows a selected
  key's property, time and value.

Since (M5.15): J / K and Select All Keyframes use what the Timeline shows — the revealed
properties' keys, layer and comp markers and the work area (`visible: [{layer, prop}]` on
`time.nextKey` / `previousKey` / `keys.selectAll`; without it, agents get every property of the
layers); the Graph Editor has Auto-Select Graph Type (the default: the speed graph when only
spatial properties are shown, else the value graph; choosing Value or Speed turns it off). Graph
Editor keys are small squares whatever their interpolation, as in After Effects.

Since (M13.34): audio scrubbing. Ctrl-dragging the current-time indicator (Cmd on macOS) plays
one frame (30–100 ms) of the comp's mix at each new frame, as in After Effects. Snippets fade in
and out and replace what is still queued, so the sound follows the pointer without lagging.
Holding still repeats nothing, the Audio panel meters follow, and the output closes after an idle
second. Agents call `playback.scrubAudio {time}`.

Since (M13.33): a precomp layer's bar shows its comp's markers, as outlined, read-only markers.
They are mapped through the layer's timing (start, stretch, time remapping, Responsive Design
stretch) and shown within its In–Out range. Hovering names them ("beat (marker in Pre)"), and a
double-click opens the nested comp at the marker. Agents read the same list with
`markers.nested {layer}`.

Since (M13.35): Roto Brush can use a trained segmentation model. MSK-4 keeps its score until G1
measures it against After Effects ([gaps.md](gaps.md)).
- The model sits behind a swappable interface: `effectcraft_segment::MaskModel` plus a registry
  where every entry must have an open-source licence compatible with ours, a source URL, a size
  and a SHA-256.
- The first model is **MobileSAM** (Apache-2.0: a TinyViT-5M encoder with Segment Anything's
  decoder), running in plain Rust (rayon and ndarray's safe GEMM, no ML framework). It reads the
  official `mobile_sam.pt` checkpoint directly and checks it against a pinned SHA-256.
- Roto Brush **Version 2.0 / 3.0** uses the model chosen in **Settings ▸ Roto Brush**; Version
  1.0 and an uninstalled model use the classic graph cut.
- On the base frame, points along the strokes prompt it. When propagating, the flow-warped matte
  prompts it (its box, points deep inside it and the matte itself). The model's probability map
  becomes a strong prior for the same graph cut, so strokes stay hard constraints and edges still
  snap to colour. A model whose mask disagrees with the flow (IoU under 0.5) is ignored for that
  frame.
- Weights are never bundled. Download (verified; uses the system `curl`) or Install from File
  puts them in the settings' `models` folder, and they load in the background.
- On the moving-disc test sequence it scores IoU 0.989 on the base frame and at worst 0.980 over
  20 propagated frames (classic: 0.973). The encoder takes about 0.7 s a frame on a 32-thread
  CPU, 1.3 s on 4 threads; its embedding is cached, so new strokes on the same frame only rerun
  the decoder (about 0.1 s).
- Agents use `roto.models`, `roto.model.select`, `roto.model.download`, `roto.model.install` and
  `roto.model.remove`. The browser build uses the classic engine for now.

Since (M13.36): face tracking can use a trained model too. TRK-3 keeps its score until G1 measures
it against After Effects ([gaps.md](gaps.md)).
- The model sits behind `effectcraft_segment::face::FaceModel` (find a face in a region, follow
  it frame to frame), in the same registry as Roto Brush's. A model reports its own points and
  says which are the tracker's landmarks and which trace the face outline, so models can be
  swapped without touching the tracker.
- The first model is **MediaPipe Face Landmarker** (Google, Apache-2.0): the BlazeFace
  short-range detector and Face Mesh V2 (478 points). Its official `face_landmarker.task` bundle
  (3.8 MB, pinned SHA-256) runs on our own TensorFlow Lite interpreter in plain Rust, which matches
  TensorFlow Lite's to about one part in a million.
- **Settings ▸ Face Tracking** chooses it. Outline Only keys the mesh's face oval into the mask;
  Detailed Features keys the mesh's eye, pupil, brow, nose and lip points; chin and jaw come from
  the outline as with the classical fitter, so Extract & Copy Face Measurements means the same with
  either. When the model finds no face in the mask, the classical fitter runs.
- On a test portrait its points are 0.85 px from Google's own pipeline on average (2.2 px at
  worst). Finding a face takes about 45 ms; following it, about 20 ms a frame (32-thread CPU).
- Agents use `face.models`, `face.model.select`, `face.model.download`, `face.model.install` and
  `face.model.remove`; `track.mask` reports the engine it tried first as `faceModel`.

Since (M13.32): View ▸ New Viewer (Alt+Shift+N, also in the Composition panel menu) opens
another Composition viewer as its own panel, as in After Effects. The viewer in use is locked,
so it keeps its comp. One viewer is active: it is the interactive one and shows the active comp.
The others show their own comp's frame at that comp's current time, and a click (or their tab)
makes them active. Opening a comp while the active viewer is locked shows it in an unlocked
viewer, or in a new one if every viewer is locked. Closing a viewer hands over to another one,
with that viewer's comp. Each viewer's tab is named after its comp, with its own lock.

Since (M4.14): RAM preview frames are keyed by their comp's content, not the project revision. An
edit keeps the cached frames (the green bar) of every comp it doesn't touch, and undo finds the
frames of the state it returns to, as in After Effects. The identity costs about 0.1 ms on the
1500-layer test project: it hashes the addresses of the comps a comp draws, and the RAM cache
keeps a project snapshot for every identity with frames, so those addresses can't be reused
meanwhile. Footage, solids, proxies and the project settings are hashed by value. At most 64
states are kept, so a long drag doesn't hold on to every intermediate state. The disk cache's
content key now also covers proxies and Use Proxy.

Since (M13.30–M13.31): the Tools bar and About dialog carry the ArtCraft mark. The
Home screen is laid out like After Effects' and covers the whole workspace. A left rail holds New
Project / Open Project, the Home, Templates and Learn pages and, at its foot, the community
links. The Home page has a "Welcome to EffectCraft" heading, quick-start tiles (New Composition,
Open Demo Project, Import Footage, New from Template) and the recent projects as a filterable
table with thumbnails and Name / Opened ("3 hours ago") / Size / Kind columns. New automation ids:
`home.filter`, `home.templates`, `home.file.import`.

Since (M4.13): motion blur follows a collapsed precomp layer's own motion. Its nested layers are
drawn with the precomp layer's transform at every sub-sample, and with the containing comp's
shutter, as they are drawn into its frames. Shape and text layers whose content animates within
the shutter (paths, Trim Paths, shape transforms, text animators) are drawn at Samples Per Frame
times and averaged, as After Effects does for shape layers; content that holds still is drawn
once.

Since (M3.15): the Timeline shows one tab per open comp, as in After Effects. Double-clicking a
comp in the Project panel opens it as a new tab next to the comps already open; it no longer
renames (Enter or the context menu still do). A tab's × closes that comp's Timeline, not the
panel. A tab dragged over a tab strip shows an insertion mark between the tabs it will land
between, and lands there; this reorders a group's own tabs too.

Since (M5.16): Key Light (After Effects' Keylight 1.2) is checked end to end on a green-screen
plate with an uneven screen, spill and soft edges (`effect.apply "Keylight (1.2)"`, pick, Clip
Black / White, render). The colour eyedroppers of Keying effects sample the effect's input
rather than the keyed frame (Ctrl/Cmd+click averages 5 × 5 pixels); agents do the same with
`effect.pickColor {effect, param, x, y, average?}` (effect space). Searching "keylight" in
Effects & Presets, or `list_effects`, finds it.

## Update: M4.9–M4.11 nested comps and motion blur

- **Composition Settings ▸ Advanced ▸ Preserve frame rate when nested or in render queue** and
  **Preserve resolution when nested** (model, renderer, Render Queue, dialog, `comp.settings`,
  `comp.info`, `CompItem.preserveNestedFrameRate` / `preserveNestedResolution`; Pre-compose
  copies them).
- **Nested comps past their end** show nothing (flattened, through a proxy, or collapsed); they
  kept drawing layers whose bars ran on.
- **Motion blur gate**: mask motion blur, effects that read the shutter and Advanced 3D follow
  the render options and the precomp layers' switches like layer motion does; mask motion blur
  uses the comp's Samples Per Frame.

Not yet: collapsed precomps don't blur with the precomp layer's own motion or the parent
camera's; animated content inside a layer (shape paths, text animators, nested frames) isn't
re-rendered per sub-sample; nested comp markers on the precomp layer bar.

## Update: M4.5–M4.8 cached playback

- **RAM preview keys** carry the render options (Fast Previews Draft / Fast Draft, Realtime
  Shadows), so a mode switch never shows frames rendered the other way; eviction drops the least
  recently shown frame; frames of an earlier revision are freed as soon as the project changes;
  the green bars count exactly what the viewer shows (scale, 3D view, region of interest).
- **Playback**: viewer Resolution Auto keeps its scale while playing (it halved it, so cached
  frames weren't reused); Cache Before Playback cuts a range larger than the RAM preview budget
  to the frames that fit and plays them (it waited forever); the Info panel shows the achieved
  frame rate and whether it is real time.
- **Cache Frames When Idle** renders the work area in the background after a second of quiet.
- **Edit ▸ Purge**: disk and snapshot purges keep the RAM preview; memory purges also drop
  decoded footage frames.

Not yet: content-keyed RAM frames (an edit drops every comp's RAM frames; the disk cache keeps
content keys), audio scrubbing, four snapshot slots.

## Update: M3.14 project files

Project files name the version that wrote them (`savedBy`); opening one that a newer EffectCraft
saved warns that what this version doesn't know is lost on saving (`file.open` reports
`savedBy`). Saving checks that the file reads back first, so a value JSON can't hold (NaN)
fails the save with a message instead of writing a project that can never be opened (the file
on disk is left as it was). Increment and Save and Save a Copy keep an XML project (`.ecprojx`)
XML; a file that isn't a project says so by name.

## Update: M3.13 menu bar

Group Shapes (Ctrl+G) and Ungroup Shapes (Ctrl+Shift+G) moved to the Layer menu itself, where
After Effects has them; Window ▸ Learn opens the Home screen's Learn tab; Composition ▸
Composition Flowchart is Ctrl+Shift+F11 and Window ▸ Flowchart Ctrl+F11 (the flowchart had
Window's shortcut); Layer ▸ Transform ▸ Center In View shows Ctrl+Home; "Reveal in Finder" reads
"Reveal in Explorer" on Windows (and "Reveal in File Manager" on Linux). Not yet: View ▸ New
Viewer (several unlocked Composition viewers at once; View ▸ Split with New Locked Viewer adds
the one locked viewer the interface has).

## Update: M3.12 Pre-compose and New Comp from Selection

- **Pre-compose** makes the new composition with the original's settings (pixel aspect,
  background, motion blur shutter / samples / switch, frame blending switch, 3D renderer); it
  only kept the size, frame rate and background.
- **File ▸ New Comp from Selection** is one undo step (it was two per item) and takes each
  item's pixel aspect. With several items selected it opens After Effects' dialog: Single or
  Multiple Compositions, Use Dimensions From, Still Duration (default: Settings ▸ Import ▸ Still
  Footage), Add to Render Queue and Sequence Layers with Overlap, Duration and Transition
  (`file.newCompFromSelection {single, dimensionsFrom, duration, addToRenderQueue, sequence,
  overlap, overlapDuration, transition}`). `layer.addItem {duration}` sets a still's length.

## Update: M3.11 layer commands

- **Lock**: locked layers can't be selected (a Timeline click, Select All, Ctrl+Up / Down step
  over them, as do shy layers while hidden) and are left alone by Clear / Delete, Arrange, the
  Layer ▸ Transform commands and Center Anchor Point (an error when every target is locked).
- **Deleting a parent** unparents its children where they are (their Position, Rotation and
  Scale are re-expressed in the composition, like the pick-whip), in the same undo step; they
  used to jump.
- **Bring Forward / Send Backward** move each selected layer one step past the next unselected
  layer, so a non-contiguous selection keeps its gaps (it was squeezed into one block).
- **Layer ▸ Transform**: Center In View and Fit to Comp keep a 3D layer's depth (Z was reset to
  0) and Fit keeps Scale / Anchor Z; Reset, Center and Fit set X / Y Position when the dimensions
  are separated (they changed the hidden combined Position, so nothing moved); Center Anchor
  Point keeps layers with separated dimensions in place.
- **Duplicate and paste** keep parents and track mattes: a copy follows the copy of its parent /
  matte when both were copied, else the original in the same composition (paste dropped them
  always).

## Update: M3.10 Layer Settings and settings dialogs

Layer ▸ Layer Settings on a solid or adjustment layer opens Solid Settings on the layer's solid
(it did nothing before): name, size, Pixel Aspect Ratio, colour and **Affect all layers that use
this solid**, which starts off as in After Effects, so a change to a solid other layers share
gives this layer a new solid of its own; nulls ask for their name, and text and shape layers
have no settings (the entry is disabled). New solids take a Pixel Aspect Ratio too. The
Composition Settings / New Composition and Solid Settings dialogs register an automation id
for every control (`dialog.comp.*`, `dialog.solid.*`), Composition Settings takes any typed
frame rate besides the list, New Composition starts from the settings of the last composition
made with it, and their colour buttons show the stored sRGB colour (they showed it lighter).

## Update: M3.9 unsaved changes

Closing a modified project asks first, as After Effects does: Quit and the window's close
button, File ▸ New Project, Open Project, Open Recent, Close Project, the demo project and a Home
template show Save / Don't Save / Cancel (Save asks for a path when the project is untitled, and
cancelling that keeps the project open); File ▸ Revert asks to discard. The window title shows
the project's path and `*` while it has unsaved changes. The modified mark now follows undo
(undoing, jumping in the History panel or rolling back a batch to the saved state clears it),
and edits that change nothing record no undo step. Agents' `engine.execute` / MCP calls are never
asked; the control channel's `app.quit {force: true}` skips the prompt.

## Update: M13.25 Home template gallery, Simulate Output ▸ My Custom RGB

**Home ▸ Templates** (UI-7; File ▸ New ▸ New Project from Template…): a gallery of eight
original built-in project templates authored in code from engine commands — Lower Third, Title
Card, Logo Reveal, Kinetic Type, Social Square 1080×1080, Vertical 9:16 Story, Slideshow and 3D
Text Orbit — each with Essential Graphics controls on its main comp and a thumbnail rendered by
our own renderer the first time the gallery shows it. **File ▸ Save as Template…** writes the
open project as an `.ectemplate` (the open template container: `manifest.json` with
`kind: "project"`, `project.ecproj`, `poster.png`, `thumb.txt`) into the config Templates
folder (in the browser: the settings store); the gallery lists built-in and user templates,
user cards can be deleted, and opening any template makes an untitled copy. Commands:
`templates.list`, `templates.thumbnail`, `templates.create`, `templates.saveAs`,
`templates.delete`, `file.newFromTemplate`; automation ids `home.tab.templates`,
`home.templates.<id>` / `.open` / `.delete`, `home.templates.saveCurrent`. M13.30: saving embeds
the footage files (default on, capped at 256 MB with a warning; the rest stays linked) and
creating from the template extracts them next to the new project (or into browser storage).

**View ▸ Simulate Output ▸ My Custom RGB…**: a dialog defines a custom output device by
primaries and white point (CIE xy) and a gamma or the sRGB curve, or reads them from an RGB
matrix/TRC ICC profile (v2/v4: colorants, `wtpt`, `chad`, `curv`/`para` tone curves; table and
parametric curves are fitted to a gamma unless they are the sRGB curve); the definition is kept
in Settings (`customRgb`) and simulated like the built-in profiles, with Preserve RGB
(`view.customRgb`, `view.simulateOutput {profile: "myCustom"}`). Generic dialog forms now register
`form.field.<key>`, `form.ok` and `form.cancel` automation ids. M13.30: LUT-based (`A2B0`)
ICC profiles too (`lut8Type`, `lut16Type`, `lutAToBType` / `lutBToAType`, evaluated from the
public ICC specification and baked into a 3D LUT for the viewer).

### M13.26: Premiere Pro interop via timeline interchange

| Feature | After Effects | EffectCraft | Status |
|---|---|---|---|
| File ▸ Import ▸ Adobe Premiere Pro Project… | reads `.prproj` | reads Final Cut Pro XML (Premiere's File ▸ Export ▸ Final Cut Pro XML), FCPXML, OTIO, EDL, AAF and OMF (`file.importTimeline`): comps, layers by track, timing/speed/reverse/holds, Motion and Opacity keyframes, dissolves as opacity keys, nested sequences as precomps, audio layers with levels, bins as folders, missing media as placeholders | ≈ 85% |
| File ▸ Export ▸ Adobe Premiere Pro Project… | writes `.prproj` | writes Final Cut Pro XML `.xml` (also FCPXML, OTIO, EDL, AAF, OMF; `file.exportTimeline`): clips per layer with timing, Motion, Opacity and levels; precomps nested; rendered-only layers pre-rendered to ProRes 4444 with alpha | ≈ 85% |

Not yet: native `.prproj` (no public specification; awaits a decision), media embedded in AAF/OMF
(not extracted), Premiere effects other than Motion/Opacity/Volume, speed
ramps, titles/graphics, and Dynamic Link.

## Update: M13.23 GPU effects, part B (EFF-5)

40 more effects run on the GPU (236 in all, with M13.22's 30), each checked against the CPU oracle (≤ 1/255 at
8 bpc, ≤ 1e-3 at 32 bpc) directly on a buffer (full and half resolution, adjustment) and
composited at 8 and 32 bpc:

- **3D Channel** (`gpu::fx_depth`): 3D Channel Extract (every channel, Anti-alias), Cryptomatte,
  Depth Matte, Depth of Field, EXtractoR, Fog 3D (with a Gradient Layer), ID Matte and
  IDentifier. The layer's auxiliary channels upload as an extra texture at the aux resolution and
  are resampled onto the buffer with the CPU's index arithmetic (tested at 1× and 2× aux
  resolution and on padded buffers). Cryptomatte's ranks are reduced per aux pixel on the CPU
  (selection coverage and ID colours), then looked up on the GPU.
- **Immersive Video** (`gpu::fx_vr`): VR Blur, Chromatic Aberrations, Color Gradients, Converter
  (all nine projections and cube layouts), De-Noise (guided filter and median), Digital Glitch,
  Fractal Noise, Glow, Plane to Sphere, Rotate Sphere, Sharpen and Sphere to Plane, in mono and
  both stereo layouts: the equirectangular maths (seam wrap, pole continuation, latitude-widened
  box blurs) ported operation for operation. Exception: VR Converter re-projections that cross a
  cube-face edge or a fisheye rim may pick the neighbouring face for directions within f32
  rounding of the edge (the CPU decides in f64): up to 0.5 % of pixels allowed (measured 0).
- **Colour management** (`gpu::fx_lut`): Apply Color LUT, OCIO CDL / Color Space / Display /
  File / Look Transform and Color Profile Converter compile to colour programs
  (`effects::color_program`, next to the CPU effects) interpreted per pixel; 1D / 3D LUTs and
  cineSpace shapers are read from a storage buffer with the CPU's nearest / trilinear /
  tetrahedral interpolation and inverses (not a hardware 3D texture: its filtering rounds the
  weights and cannot do tetrahedral). Lumetri's Input LUT and Look no longer force the CPU.
  Custom `.ocio` configurations render on the CPU. Exception: nearest-neighbour lattice lookups
  may pick the other lattice point for 8 bpc inputs exactly on a rounding boundary (up to 1 %
  of pixels allowed).
- **Simulation render passes** (`gpu::fx_sim`): CC Rainfall, CC Snowfall, CC Star Burst, CC
  Bubbles, CC Drizzle, CC Hair, CC Mr. Mercury, Caustics, Wave World, Foam, Shatter, Card Dance
  and Card Wipe. The per-frame simulation / layout stays on the CPU and is shared with the CPU
  effect as a plan (sprites, textured pieces, blobs, drops, the wave grid, Caustics' prepared
  layers); a tiled rasteriser composites each pixel's items in the CPU's order and shading, with
  layer-dependent colours (refracting rain, star-burst and hair roots, bubble refraction)
  sampled per pixel. Piece plans read the frame back once (their gradient maps and textures
  default to the layer). Shatter's wireframe views and Foam's User Defined texture,
  Environment Map and flow-map preview render on the CPU.

## Update: M13.28 GPU effects, part C (EFF-5)

37 more effects run on the GPU, each checked against the CPU oracle (≤ 1/255 at 8 bpc, ≤ 1e-3 at
32 bpc) directly on a buffer with masks, another layer and an audio track (full and half
resolution, adjustment) and composited at 8 and 32 bpc:

- **Pixel ports** (`gpu::fx_pixel2`): CC Cross Blur, CC Radial Blur, CC Vector Blur, Reduce
  Interlace Flicker, CC Composite, Color Link, Cineon Converter, HDR Compander, HDR Highlight
  Compression, Grow Bounds, CC Overbrights, CC Block Load, CC Burn Film, CC Glass, CC HexTile,
  CC Mr. Smoothie, CC Plastic, Inner/Outer Key, CC Simple Wire Removal and Basic 3D.
  Inner/Outer Key's trimap and Cleanup strokes and CC Glass / Plastic's light are built next to
  the CPU effect and shared; Color Link's sample statistics run on the CPU over the read-back
  layer (none when a Source Layer is chosen). Plane blurs (`util::gauss_plane`) sum each box
  window directly, as the CPU's f64 running sums keep the small values that normalised blurs
  divide. CC Plastic now reads its Bump Layer (it read a nonexistent parameter id).
- **Generators** (`gpu::fx_gen2`): Lightning, Advanced Lightning, Beam, Lens Flare, Radio Waves,
  Vegas, Stroke, Scribble, Write-on, Paint Bucket, Eyedropper Fill, CC Glue Gun, CC Threads,
  Audio Spectrum, Audio Waveform, Basic Text and Path Text. The geometry stays on the CPU and is
  shared with the CPU effect as a per-frame plan (`effects::bolt_plan`, `lightning_segments`,
  `paint_plan`, `marks_plan` with the FFT / peak analysis, `text_passes`, `radio_plan`, Paint
  Bucket's flood-filled region, Eyedropper Fill's sample); segments and convex polygons are
  binned into 16 × 16 tiles and rasterised on the GPU with the CPU's coverage functions, glows
  and compositing included. The CPU effects were refactored to draw from the same plans with
  bit-identical output (`crates/effects/tests/gen_golden.rs`: 288 frame hashes pinned before
  the refactor). Exception: CC Threads decides band membership and fibre shade per pixel, and at
  round parameter values (the defaults) pixel centres sit exactly on band edges, where f32 and
  the CPU's f64 can fall on either side: up to 5 % of pixels allowed (measured ≤ 3.6 % at half
  resolution with the defaults, 0 at generic values).
- **Fractal** stays on the CPU: in f32, orbits near the set escape at other iterations on 0.3–1.5 %
  of the pixels even at low magnification, WGSL has no f64 and Metal's fast math defeats
  double-f32 emulation.

## Update: M13.29 GPU effects, part D (EFF-5)

7 more effects run on the GPU (280 in all, with M13.28's 37), and five CPU fallbacks are gone. Each case is
checked against the CPU oracle (≤ 1/255 at 8 bpc, ≤ 1e-3 at 32 bpc) directly on a buffer (full
and half resolution) and composited at 8 and 32 bpc; the CPU refactors behind the shared plans
are pinned bit for bit by golden hashes (`crates/effects/tests/particle_golden.rs`,
`timewarp_golden.rs`, `sim_golden.rs`).

- **Particle render passes** (`gpu::fx_particles`): CC Particle World and CC Particle Systems II
  (every particle type and transfer mode; the stepped simulation stays on the CPU or the GPU
  `ParticleSim`, sorted and shaded as the CPU does), Particle Playground (cannon, grid, text
  strokes, layer exploder, property mappers, repel, wall; the Layer Map particles' frames are
  packed into one atlas), CC Ball Action, CC Pixel Polly and CC Scatterize (their plans read the
  frame back once: the particles take the layer's colours and, for Ball Action's twist, its
  luminance). Plans too large for one item table draw in several passes, each over the previous
  one, with the same per-pixel arithmetic.
- **Curl Noise**: the fBm potential, its curl and the streamline trace as three kernels.
- **Former fallbacks**: Shatter's wireframe views (the default view) draw their lines as
  sprites; Foam's User Defined texture, Environment Map and flow-map preview draw over the
  bubble sprites; Timewarp with a Matte Layer carries the masked frames and the foreground's and
  background's own vectors in its plan and builds both layers on the GPU; custom `.ocio` configs
  compile to the colour program (matrix and offset, exponent, log / log-affine, range, CDL, file
  LUTs, groups, inverses). Warp's Fisheye / Twist past 50 % or with a distortion keeps the CPU's
  f64 Newton solve (f32, even with more iterations, lands on other roots near the crease): the
  CPU solves the inverse map from the geometry alone and the GPU samples along it, so the chain
  stays on the GPU without a readback.

## Update: M13.30 web depth and polish

- **GPU in the browser's job workers** (WEB-1): each job worker opens its own WebGPU device
  (deferred readbacks) like the frame workers. The Render Queue's export and the analyses'
  frame loops are futures (`effectcraft_render::passes`, `offload::run_request_async`): every
  frame renders in passes on the device and the job awaits the GPU between them, so Render
  Queue renders and the input frames of Warp Stabilizer, 3D Camera Tracker, Track Motion, mask
  tracking and Roto Brush (prefetched before each step) use the GPU. GPU particles and
  Advanced 3D (`raster_3d`, `render_3d`) read back under keys, so they run on the device in
  frame and job workers too (a depth-of-field run still resolves on the CPU, its scenes on
  the GPU). Content-Aware Fill became a job-worker job too (its files go back to browser
  storage; before, it ran on the page and could not write its folder). `effectcraft.info().workers`
  reports the job workers' adapter and each job's passes and readbacks; the headless-Chrome
  smoke test measures them.
- **Layer buffers in the browser's disk cache**: frame workers back their layer caches with a
  `PrefetchStore`; the page plans which layer entries to read before a request from the
  previous frames' misses (`LayerPrefetch` over the shared `DiskIndex`, one LRU order for
  frames and layers), and slow buffers are written to `effectcraft-cache/v1/layers`.
- **Templates embed footage**: `templates.saveAs {embedFootage (default true), embedLimitMB
  (default 256)}`; `templates.create {projectPath?, footageDir?}` extracts it.
- **ICC A2B0 profiles** in View ▸ Simulate Output ▸ My Custom RGB (see above).

## Update: File ▸ Import ▸ EffectCraft Project

- **Import a project into a project**, as After Effects imports an `.aep`:
  `file.importProject {path}` (File ▸ Import ▸ EffectCraft Project..., and `.ecproj` /
  `.ecprojx` files given to File ▸ Import) merges the project into a folder named after the
  file. Every item, layer and property id moves past the open project's, so references
  (precomps, parents, track mattes, Layer Control values, Essential Graphics, tracker
  targets) stay intact. It's one undo step. Footage paths relative to the imported project
  resolve next to it. The result lists the comps, the root comps, `primaryComp` (the only
  root comp) and missing media.
- Id shifting saturates instead of overflowing on damaged files, for both templates and
  projects.

## Update: Libraries

- **Libraries** ([libraries.md](libraries.md)): folders indexed by `library.json` hold reusable
  comps (`.ecproj`), animation presets, footage and sound effects.
  - `library.search` ranks entries by name, tags, category and description.
  - `library.insert` imports a comp project and nests the entry's comp, adds media as a layer, or
    applies a preset.
  - `library.addFolder`, `library.removeFolder`, `library.folders` and `library.get` manage and
    read them.
  - Being engine commands, they reach MCP and the control channel with nothing extra.

## Update: text and viewer fidelity (G1)

Fixes from measured reports:

- Glyphs that touch or overlap ("ff") are drawn as one outline, so no light seam shows where
  their anti-aliased edges meet (#415). Characters of other colours, with Blur, or under an
  Inter-Character Blending mode are still composited one by one.
- Interior layer styles (Gradient / Color Overlay, Satin, Inner Glow, Inner Shadow, the inside of
  Stroke) blend onto the layer's colour inside its alpha, so an anti-aliased edge no longer shows
  a rim of the layer's own fill (#416). Not yet: Inner Bevel still shades an anti-aliased edge
  over the layer's own colour.
- Text layers are always rasterised at their on-screen scale, sharp at any Scale as in After
  Effects, and an unscaled layer at a fractional position is no longer rasterised a quarter octave
  too large (#390).
- The viewer's Auto resolution renders the pixels the magnification needs (Full above 50 %), and
  frames shown below 100 % are averaged (GPU frames through mip levels, CPU frames by a whole
  factor) instead of minified bilinearly (#417). Not yet: the viewer resolution and magnification
  are not remembered between launches.


## Update: interface, import and agent additions (7–10 October)

Recorded in one place on 10 October; each landed with tests.

- **Effect Console** (Edit ▸ Effect Console..., Ctrl+Space, `app.fxConsole {query?}`): a search
  bar at the pointer ranks effects and presets by name, word starts, initials (`gb` = Gaussian
  Blur) and letters in order, recent and favourite effects first. Enter applies to the selected
  layers and closes, Shift+Enter applies and stays open; with no text it lists recent effects.
- **Shortcuts:** `Physical+<key>` bindings for layouts such as AZERTY that report one logical key
  for two physical keys; Go to Work Area Start / End (`time.workAreaStart` / `time.workAreaEnd`,
  Shift+Home / Shift+End).
- **Shape layers:** Reverse Path Direction (⇄) on Path, Rectangle, Ellipse and Polystar rows
  (`shape.reversePathDirection`; Trim Paths then runs the other way). The toolbar's Fill / Stroke
  kind, blend mode and opacity apply to the selected shape layers (`shape.fillStroke`), not only
  to the next shape drawn.
- **Timeline and Graph Editor:** trim zones reach just past a layer bar's ends and win over slip;
  the view stops at the comp end; the zoom buttons and a click on the zoom slider work, and the
  Graph Editor shares the layer bars' time zoom and fits the selection when it opens; key
  marquees show the selection live while dragging.
- **Panels:** right-clicking a tab (or the ≡ menu) opens the panel menu: Close Panel, Undock
  Panel, Close Other Panels in Group, Panel Group Settings ▸ (`window.*` commands,
  `panelMenu.<key>` ids). Solids, adjustment layers and nulls show a swatch of their colour as
  their source icon; long Project panel names end in an ellipsis.
- **Settings ▸ Scripting & Expressions:** Let Agents Control EffectCraft starts the control
  channel on every launch (`scripting.agentControl`, port 9877); Let Figma Send Layers runs the
  Figma plug-in's bridge on localhost:7789 (`scripting.figmaBridge`).
- **Import:** After Effects projects (`file.importAep` through py-aep,
  [packaging/aep/README.md](../packaging/aep/README.md)), Figma frames as native layers
  (`file.importFigma`, [integrations/figma/README.md](../integrations/figma/README.md)), and AVIF
  stills.
- **Agents:** the offscreen app (`mcp --ui`), frame, contact-sheet, comparison, layout and audio
  analysis tools, progress and cancellation, resource templates, subscriptions, logging and
  prompts, stable ids, per-client undo and `expectRevision`, repro scripts, `cargo xtask agents`
  ([agents.md](agents.md), [mcp.md](mcp.md), [control-protocol.md](control-protocol.md)).

## Update: 10 October 2026, undocked panels as OS windows

- Interface: an undocked (floating) panel group is now an OS window of its own on the desktop, as in After Effects (egui multi-viewport): it can go to a second monitor, keeps its screen position and size in the workspace and across relaunches, closes its panels from its close button, and docks back with the panel menu's Dock Panel or by dragging a tab into the main window. The web and headless contexts keep the in-window floating groups.
## Update: projects reopen their open comps

Saving a project remembers the comps open in the Timeline/Composition tabs (in tab order), the
one in front and each one's current time, and opening it (Open, Open Recent, Revert, a
recovered auto-save) brings them back, as After Effects does with an `.aep`. Changing tabs or
the time doesn't mark the project modified; comps deleted since are skipped; older files open as
before.
## Update: Panel Group Settings ▸ Solo Panels in Stack and Small Tabs (10 October)

- The panel menu's last two Panel Group Settings items work: Solo Panels in Stack
  (`window.soloPanels {panel, solo?}`, stacked groups only) keeps one panel of the stack expanded,
  and Small Tabs (`window.smallTabs {panel, small?}`) gives a docked group or stack a shorter tab
  strip with smaller labels. Both are saved with the layout and its workspaces. Floating groups
  don't have Small Tabs yet.

## Update: undocked panel windows, finishing touches (10 October)

- Panel Group Settings ▸ Small Tabs works for undocked groups too (in their OS window and where
  they float inside the main window), from the panel menu or `window.smallTabs`; Undock Panel
  Group keeps a docked group's Small Tabs, and the setting is saved with the layout.
- Dragging a tab over an undocked window shows where it will land (the tab strip highlight and
  insertion mark) and the dragged tab in that window, whichever window the drag started in.
- Undocked windows stay above the main window while EffectCraft is the active app, as After
  Effects' floating panels do, and become ordinary windows when another app is active.
- An undocked window saved on a monitor that has since been unplugged opens over the main
  window instead of off-screen (macOS; Windows catches only absurd saved positions).
- Agents sharing the desktop app over MCP get their own name by default: a bridged MCP server
  without `--client` runs as `<MCP client name>-<pid>`, so it never moves the user's selection,
  comp or time and can claim comps; `"client": ""` runs one call as the user.
- MCP `--autosave` recovery stays bounded on disk: checkpoints of 8 MB or more are throttled to one
  every 30 s (flushed when the session ends), a normal save removes the session's recovery files,
  and new sessions prune finished saved sessions and folders untouched for 30 days.
- Desktop control channel: a `wait: true` job command (render queue, analyses, footage check,
  Content-Aware Fill, Scene Edit Detection) and `jobs.wait` start the job and reply when it ends,
  instead of blocking the UI thread, so one agent's long job no longer stalls the other agents or
  the window.
- Renders requested over the desktop control channel (`render.frame`, `render.contactSheet`, and
  the MCP bridge's renders) run as background tasks on a copy of the project, so the window and
  other agents carry on; `wait: false` starts one from any client (`jobs.list`, quiet, no toast).
- Host list and save guard: every EffectCraft holding a project (the app, MCP servers) is listed
  in `<config>/Hosts/` (`hosts.list`: kind, control port, open project); opening a project another
  one has open warns with the port to join; saving over a project file that changed on disk since
  it was opened or saved here is refused unless `force: true`, so copies no longer overwrite each
  other's work silently.
- MCP `hosts` tool: lists where projects are open and `join`s a host (by port or project path) as
  a named agent, so an agent hops between projects without restarting; refused while its own
  headless project has unsaved changes.
- `effectcraft-cli serve`: a windowless host holding one project that several agents join over the
  control channel (own cursors, comp claims, waiting jobs that don't block the others, throttled
  checkpoints, in the host list); `host.release` saves and stops it.
- Handover: opening a project that a windowless host (`effectcraft-cli serve`) has open asks the
  host to save and stop, then opens the saved file, instead of making a second copy; agents whose
  host went away are pointed to the MCP `hosts` tool to rejoin.
- `file.importAep` and `file.importProject` take `wait: false`: the After Effects export, or reading
  a big project file, runs in the background (Window ▸ Progress) and the import applies as one undo
  step when done; the desktop control channel and windowless hosts use it, so a 1 GB import no
  longer stalls other agents.
- Undo with agents in the project: `edit.undo` (⌘Z) undoes the caller's own latest step; behind
  someone else's steps it reverts only what that step changed (per comp or item), refused when
  they changed the same thing since; `edit.undo {any: true}` undoes the last step whoever made it.
- Control channel fairness: agents sharing the app take turns per `client`, a frame spends at most
  50 ms on requests (the window keeps drawing), and up to 32 connections are served; the Edit
  menu's Undo names the step ⌘Z undoes (your own latest, behind agents' steps).
- Undo with agents in the project works layer by layer inside a comp: the user's ⌘Z undoes their
  change to one layer while an agent adds or edits other layers of the same comp (refused only for
  the same layer, or the comp settings / layer order the step changed), and pressing ⌘Z again goes
  on to the step before instead of bringing the undone one back.
- The desktop app's control server and the windowless host (`effectcraft-cli serve`) share one
  control-channel implementation: the host now also rejects junk lines, bounds line length and
  answers repeated `requestId`s from the reply cache, and both use the same wait-for-job replies.

## Update: relinking moved footage, golden repros, fuzzing (10 October)

- Project: relinking moved footage. `footage.relink {item, path}` relinks one item and the other
  missing items that moved with it (one undo step), as Replace Footage on a missing item now does;
  `footage.missing` lists them; the
  footage check after open finds files that moved along with the project's folder. Windows and
  macOS paths relink in either direction.
- Repro scripts take `golden` frames (`cargo xtask repros [--bless]`), so a user's file can become a
  render regression check; `cargo xtask fuzz` runs ten fuzz targets (project files, AEP import,
  Lottie, SVG, PSD, PDF, timelines, expressions, media, engine commands); `cargo xtask bench
  --fail-over PCT` fails on resource regressions.

## Update: 2026-10-14 — mask/shape interactions and panel windows

- Full-frame vertex marquee, arrow nudging, edge auto-pan, per-vertex screen-space movement,
  mask locks, parametric-to-Bezier conversion and shape visibility controls implemented.
- Menu-created and ordinary floating panels use one native implementation; old window placement
  records migrate to floating-group workspace state. Returning to the main window docks the group.
- Regression tests added; validation status and remaining native checks in
  [interaction-fixes.md](interaction-fixes.md). No readiness percentage increase.

## Update: one-time native FigCraft clipboard (14 October 2026)

- `edit.pasteFigCraft` and normal Edit → Paste / Cmd+V accept the versioned FigCraft snapshot,
  create native editable layers in one undo step, and report unsupported visuals without a
  raster fallback. Invalid marked payloads do not paste older internal clipboard content;
  input size, node count and nesting are bounded and asset/file fetch inputs are rejected.
- Existing destination compositions supply their timing to nested compositions. The shared
  design mapper retains the deepest supported node, includes stroke/shadow render bounds when
  making a new selection canvas, and converts three-handle affine linear gradients into native
  gradient axes. Redo reopens a newly restored composition only when no valid composition is
  active, preserving existing focus otherwise.
- Scoped engine/UI compilation and clippy (all targets, warnings denied) and a native build
  passed. Actual FigCraft UI Copy → OS clipboard → EffectCraft menu Paste and Cmd+V produced
  nine native layers (three text, six shape), no images. Editing text, one-step undo/redo,
  independent repeated pastes, malformed-clipboard rejection, save/reopen, nested compositions,
  destination timing and standalone stroke/shadow bounds passed native runtime checks.
- The synthetic linear-gradient region differs by at most 1/255 per channel; save/reopen pixels
  are identical. Text metrics and other documented limitations remain warned. Regression tests
  were compiled but not executed under the fork's daytime rule; full CI was not run. No general
  pixel-parity grade is assigned, and this extension does not change After Effects parity scores.


## Update: ordinary FigCraft Copy carries native clipboard data (14 October 2026)

- Normal FigCraft Copy/Cmd+C can carry the existing native snapshot in an exact SVG metadata
  marker; EffectCraft Paste recognizes it while preserving generic SVG and explicit JSON paste.
  Carrier and decoded sizes are bounded, duplicate/damaged metadata is rejected without
  fallback, and native SVG geometry `path` strings are accepted only in geometry entries.
- Scoped engine/UI check and clippy (all targets, warnings denied) and a native build passed.
  Actual right-click Copy and Cmd+C → EffectCraft Paste/Cmd+V produced ten native layers
  (three text, seven shapes, including a vector), with no images. Cut/Undo, plain-text copying,
  the optional explicit action, one-step undo/redo, independent copies, malformed-carrier
  rejection, identical save/reopen pixels and standalone stroke/shadow bounds passed native QA.
  Text metrics remain warned. Regression tests were compiled but not executed; no full CI was
  run under the fork daytime rules. Parity scores are unchanged.
## Update: 2026-10-14 — inline precomps and graph controls

- Inline precomp rows edit real source layers/properties/keys with validated instance paths,
  renderer-aligned timing, source-targeted menus and atomic undo. Nonlinear inverse-time drags,
  source graphs and clipboard operations require opening the source composition.
- Graph speed samples expressions and separated Position followers; Fit All frames shown
  animation. Graph keys expose editing dialogs/context actions; Graph Editor Set Only isolates
  pinned curves. Existing graph tooltip/Reference-toggle regressions corrected.
- Implementation notes and honest verification limits: [inline-precomp-editing.md](inline-precomp-editing.md),
  [graph-editor-interactions.md](graph-editor-interactions.md). Runtime/native acceptance pending.

## Update: 2026-10-14 — shape layer drag routing

- A selected shape layer's visible path overlays no longer turn ordinary interior drags into
  vertex marquees. Selection-tool drags use the existing Position movement and Undo path.
- Direct vertex/tangent grabs, full-frame mask marquees and explicitly selected shape-vertex
  marquees retain their priority. Synthetic coverage checks layer Position, geometry and Undo.
- An isolated real-project copy reproduced the stalled drag in the prior build; the updated
  build moved Position by the requested 80 × 60 pixels with unchanged Anchor Point and one-step
  Undo. A direct vertex drag still changed only the grabbed vertex, leaving layer Position and
  other vertices unchanged. Screenshots and saved geometry were checked; private inputs remain
  local. Scoped check/clippy passed;
  regression execution is deferred under the fork's daytime policy. [Details](interaction-fixes.md).
  Parity estimates unchanged.


## Update: 2026-10-14 — nested graph/clipboard completion and measured CPU work

- Expanded precomps now expose the source Graph Editor, key dialogs and existing internal/rich/SVG
  clipboard routes, preserving parent viewer/time/zoom and atomic source undo. J/K maps source
  navigation to the parent when unambiguous; nonlinear scrubbing uses the parent ruler. Modal
  commits revalidate the instance path and locks. Spatial handles/waveforms still need the source viewer.
- An opt-in before/after-expression comparison shows dim base and bright evaluated curves,
  separately from the existing Reference Graph. Both phases participate in fitting; key/handle
  edits still address underlying animation.
- Synthetic CPU measurements justified two cache changes: 100 ordinary curves after an unrelated
  edit improved from 8.539 to 0.117 ms p95, and 14,020 expanded property rows from 4.743 to 1.480 ms.
  Whole-probe peak RSS stayed about 73 MiB. Expression-heavy resampling remains slow.
- Native all-targets check, strict Clippy and focused WASM check passed. Performance probes ran;
  behavioral regressions compile but await execution under the daytime policy. Native visual QA
  and input-to-visible-frame acceptance remain pending. No full CI; readiness estimates unchanged.
  Methods and limits: [interaction-performance.md](interaction-performance.md).


## Update: 2026-10-14 — expression graph cache capacity

- Measured the 8,192-entry result-memo cliff: a repeated 20-curve speed graph requested 28,800
  expression evaluations and reran every script. Complete memory accounting now allows that
  working set within the estimated 32 MiB per-thread budget, reuses table storage on pressure,
  and skips metadata setup only after recorded dependencies validate.
- Paired native CPU probes: actual 20-expression graph rebuild p95 319.031→23.970 ms; numeric
  diagnostic checksums match. Peak probe RSS increases about 30 MiB. Oversized 50-curve sweeps
  still recompute, with stable table storage. Cold/complex/browser behavior remains unmeasured.
- Performance probes executed; four new behavioral regressions compile but are deferred under
  the daytime policy. No full CI; overall readiness estimates unchanged.
  [Protocol, measurements and limits](expression-performance.md).


## M7: FigCraft bitmap and paint fidelity (15 October 2026)

- Version 2 clipboard snapshots carry bounded original PNG/JPEG assets with native geometry
  mattes, exact placement and editable children. Images are prepared before a single project edit;
  malformed assets and importer failure cannot partially change the project. Legacy v1 remains accepted.
- Root render bounds preserve overflow and clipping. Multiple native shadows, gradient kinds and
  affine width handles, exported text metrics and device-pixel-aligned text buffers preserve
  appearance. Eligible pass-through frames use existing collapse transformations safely.
- Native private-corpus verification: 529/529 transfers and Undo; previous nine size failures and
  51 blank targets eliminated. Original bitmap bytes verified for 953 stored files; four saved
  projects reopened with identical rendered pixels. Synthetic gradient/text and bounds checks
  are detailed in docs/figcraft-clipboard.md. Source fixtures/assets remain local and uncommitted.
- Scoped all-target check, strict Clippy, WASM and native builds passed. Rust regressions compiled
  only under daytime policy; full CI was not run. No change to After Effects parity scores.
## Update: G4 portable project handoff (15 October 2026)

- Collect Files now copies ordinary footage, sequences and item/composition proxies once per
  source path, saves package-relative paths using the checked serializer and retains editor
  metadata. New-folder ownership prevents overwrite; a failed copy/write does not publish a
  completed project and incomplete output is cleaned up. Native copies stream to disk.
- The package report lists fonts, external effect files, model sidecars, placeholders and Render
  Queue destination setup. Embedded data stays embedded; local Freeze cache references are
  dropped from the copy. Source session, history and dirty state are unchanged.
- Browser/virtual hosts and confined automation explicitly reject folder collection. Confined
  collection needs atomic checking of implicit source reads before it can be supported.
- Regression cases cover relocation, proxy/sequence completeness, duplicate filenames, copy and
  write failures, metadata, invalid serialization, embedded data, warnings and existing output.
  Tests were added but runtime execution is deferred under the fork's daytime policy; no claim
  of visual fidelity or cross-platform acceptance is made. Feature percentages are unchanged.

## Update: motion-path segment bend (17 October 2026)

- Pen (or Convert Vertex) drag on the motion path between two Position keys bends that segment
  through the pointer: only the first key's out and the second key's in spatial tangent change
  (`keys.bendSpatialSegment`, one undo step); key count, times, values and temporal ease stay.
- After Effects parity checked against its documented behaviour (not driven live): a Pen click on
  a motion path is Add Vertex and adds a keyframe (kept); Convert Vertex click toggles a key
  between Linear and Auto Bezier, and dragging from a key pulls out its handles (kept). After
  Effects has no segment drag on motion paths; the bend is an EffectCraft addition (as in
  Illustrator and Figma). Feature percentages are unchanged.

## Update: viewer Sharp Zoom (19 October 2026)

- View ▸ Sharp Zoom (on by default; beyond After Effects, which magnifies comp pixels): above
  100 % at Full or Auto resolution, the Composition viewer also renders the visible region at the
  magnification (up to 16 output pixels per comp pixel) and draws it over the magnified frame.
  Text, shapes and solids are drawn at their on-screen size, footage and nested comps are
  resampled from their own pixels, and pixel-sized effect parameters follow the buffer scale.
- The tile renders on its own thread on the CPU, one at a time; until it lands (an edit, a pan
  past the 256-pixel grid) the magnified comp-resolution frame shows. Not while playing,
  scrubbing or dragging, in 3D comps, with Show Channel / exposure / display colour management,
  snapshots or the cloud preview.
- Renders flag it (`RenderOpts::sharp`, viewer only): each layer buffer stays within 16 Mpx
  (bigger sources are drawn at a lower scale, never below 100 %), nested comps drawn whole are
  held to it, and static nested comps are windowed to what shows. Without the flag (exports,
  RAM preview, every viewer frame up to 100 %, upscaled outputs) nothing changes, and the
  render options' cache keys keep their text. Feature percentages are unchanged.

## Update: text Gradient Fill animator (19 October 2026)

- GRADFILL, an EffectCraft extension (After Effects has no gradient text animator): Animate
  Text ▸ Gradient Fill adds Gradient Type (Linear / Radial), Gradient Start, Gradient End and
  Gradient Fill Colors to a text animator. The selected characters are filled with a smooth
  gradient per pixel, blended over the fill by the selection and stacked in order with Fill
  Color animators. Start and End are in percent of the box of the group the first selector
  picks (Based On Words: each word; Lines: each line; Characters: each run of consecutive
  selected characters; no selector: the whole text), 0,0 top left to 100,100 bottom right,
  default 0,50 → 100,50; each character carries its part of the gradient when animators move,
  rotate or scale it. Composition viewer: Gradient Start / End dots on the first group, where
  the animation draws it (drag = one undo step, snaps to the box's edges and centre, Alt places
  freely). Properties panel: a colour stops bar per Gradient Fill animator. Animation presets,
  scripts (`EC Text Gradient Fill`) and expressions carry it; Lottie export drops it with a
  note; `.aep` import never produces it. Glyphs without a selected gradient draw on the solid
  path as before (bit-identical). Extruded (Advanced 3D) characters and text → shapes use the colour
  at the gradient's middle.

## Update: deleting one text animator property (19 October 2026)

- PROP-REMOVE: a single property of a text animator's Properties (Fill Color, Blur, Rotation,
  Tracking…) can be deleted, as in After Effects: Edit ▸ Clear / the Delete key with it
  selected in the Timeline, Delete in its row's context menu, scripting's `Property.remove()`,
  and `prop.removeGroup` (which now also takes these properties and returns `{removed}`).
  Properties one Animate entry adds together go together: Tracking Type with Tracking Amount,
  Skew Axis with Skew, Character Alignment / Range with Character Offset (Character Range stays
  while Character Value uses it), and the four Gradient Fill properties. Fixed properties
  (a selector's Start / End / Offset, Transform, Source Text…) are refused with an error. One
  undo step; the removed properties and their keyframes leave the selection.

## Update: Cache Frames When Idle without slowing the machine (20 October 2026)

- Cache Frames When Idle rendered up to half the frame threads' worth of frames at normal
  priority, and each frame's parallel pixel loops (blurs, shadows, quantisation) ran on every
  core: 300–470 % CPU on the owner's Mac while the app sat idle, and the whole machine lagged.
  Now one idle frame renders at a time inside its own small Rayon pool (a quarter of the cores,
  1 to 4, or Memory & CPU ▸ CPU Cores for Cache Frames When Idle), whose threads run at macOS
  utility QoS (`effectcraft_platform::background_thread`, ADR 0001 amendment); the viewer's
  frame and playback keep every core and go first. It waits while the system is short of memory
  (macOS memory pressure, low memory, or less available than RAM Reserved for Other
  Applications, read every 5 s while it is on) and goes on when memory frees up, the disk-cache
  phase included.
