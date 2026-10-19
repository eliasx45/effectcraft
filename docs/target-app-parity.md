# Parity with After Effects

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (portable collection with dependency warnings; runtime acceptance pending; issue, catalog and export-format references re-audited against `upstream/main` 470ea7a3; the 10 October re-measure is otherwise unchanged) · **Target:** Adobe After Effects 2026 (26.5.0.89)

Cells hold the current state only; append history to [parity-log.md](parity-log.md).

The authoritative assessment of how close EffectCraft is to After Effects, and how much work is
left. [gaps.md](gaps.md) lists every known shortfall one at a time; the
[ROADMAP](../ROADMAP.md) summarises both. Deep dives: [effects-parity.md](effects-parity.md),
[scripting-parity.md](scripting-parity.md), [file-format-parity.md](file-format-parity.md),
[ui-parity.md](ui-parity.md), [hardware-parity.md](hardware-parity.md),
[localization-parity.md](localization-parity.md), and the live measurements in
[fidelity/](fidelity/README.md).

## Headline

| Number | Value | Kind |
|---|---|---|
| **Feature breadth** (does each After Effects 26.5 feature exist?) | **≈ 91%** | Estimated, weighted by area (table below). The self-graded 92-item checklist says 89 done / 3 partial (≈ 99%), but it was written before After Effects 26.2 and 26.5 and does not count animation presets, project formats or hardware; see the appendix |
| **Ready for real work** (can an After Effects professional do client work here?) | **≈ 45%** (range 40–50%) | Estimated: 60% feature depth (≈ 50%, table below), 15% opening and delivering work files (≈ 35%), 10% performance (≈ 35%), 10% stability (≈ 45%), 5% platforms (≈ 65%) |
| **Stage** | **alpha** | All six core workflows pass the [alpha gate](roadmap.md#alpha-gate) on macOS and ready (≈ 45%) is above the ≈ 40% bar, but `.aep` projects open only through the py-aep converter (this fork, 10 October), fidelity is measured for one narrow case, and 216 issues are open on 12 October ([ROADMAP](../ROADMAP.md#stage)) |
| **Mainstream practitioner** (the typical motion designer's weekly work) | **≈ 47%** | Estimated: everyday depth 72% × interaction 0.90 × stability 0.90 × file exchange 0.80 ([below](#mainstream-practitioner)) |
| **Essentials user** (core features only) | **≈ 52%** | Estimated: core depth 77% × launch/stability 0.88 × clarity 0.90 × opening files 0.85 ([below](#essentials-user)) |
| Remaining to **beta** (≈ 75% ready, `.aep` opens) | **≈ 350–620 Opus 5.5 agent-hours** | Estimated |
| Remaining to **full parity** | **≈ 850–1,500 Opus 5.5 agent-hours** | Estimated |

### Readiness by audience

| Audience | Ready % | Opus 5.5 agent wall-clock hours to ~95% | Work that dominates |
|---|---:|---:|---|
| Full target (ready for real work) | ≈ 45% | ≈ 850–1,500 h | `.aep` import, a fidelity corpus for every feature, the bug backlog, performance, hardware video, formats, localization, ecosystem |
| Mainstream practitioner | ≈ 47% | ≈ 280–530 h | `.aep` exchange (80–160 h), stability and crash fixes (40–80 h), fidelity of the everyday features (40–70 h), everyday-area bugs and depth (60–110 h), interaction feel (30–60 h), preview speed (25–50 h) |
| Essentials user | ≈ 52% | ≈ 150–300 h | Opening the `.aep` files tutorials ship, basic subset (60–120 h), start-up and stability (20–40 h), core-feature bugs and clarity (30–60 h), preview with sound (20–40 h), fidelity of the core features (20–40 h) |

Each audience's hours are a subset of the next (essentials ≤ mainstream ≤ full). They are
calibrated like the rest of this file ([calibration](#calibration-of-the-hours)): a behaviour bug
with its regression test ≈ 0.1–0.3 h, a mid-size feature 1–4 h. About 60–70% parallelises across
5–8 agents. The serial parts: reference capture for the broader `.aep` corpus, and the fidelity
harness before per-feature scores. Every audience needs After Effects running on the owner's Mac
for reference captures.

The previous headline (5 October: breadth ≈ 99%, real use ≈ 30–50%) moved for these reasons:
breadth went **down** because this pass measured against 26.5 rather than our own catalogue and
found missing features the catalogue never listed (Object Matte, Substance materials, ACES 2.0
configs, proportional scrubbing, copy frame to clipboard, paste SVG/AI as shapes, percentage
guides, After Effects' 621 animation presets, FBX/USD/STL models, Media Encoder-style hardware
encoding); real use moved **up** to the upper half of the old range because the 5–10 October
work closed 179 issues, most of them user-reported bugs fixed with regression tests, added six partial UI languages
and a first live After Effects fidelity measurement. It is still an estimate: nothing renders a
corpus in both apps yet.

## How this was measured

- **Target.** Adobe After Effects 2026, the copy installed on the owner's Mac:
  `CFBundleShortVersionString` 26.5.0, build 26.5.0.89. The one live fidelity comparison
  ([fidelity/README.md](fidelity/README.md)) used 26.3x87.
- **From the installed bundle (file names and listings only, per [AGENTS.md](../AGENTS.md) §2):**
  `Info.plist` `CFBundleDocumentTypes` (26 document types: `.aep`, `.aepx`, `.aet`, `.aetx`,
  `.mogrt`, `.mgjson`, `.aecap`, `.aegraphic`, `.ffx`, `.ars`, `.aom`, `.jsx`, `.psd`, `.cin`,
  `.mov`, `.pct`, `.pic`…); the `Plug-ins/Format` importers (AIFF, Animate, JPEG, OpenEXR, PNG,
  Premiere Pro import, Photoshop layers in and out, Radiance, SGI, SVG, Targa, the standard
  multi-format plug-in); `Plug-ins/Effects` (306 `.plugin` bundles in 231 entries);
  `Presets` (**621** `.ffx` animation presets); `Scripts` (11 sample scripts, Startup / Shutdown /
  ScriptUI Panels folders); `Resources/usd_plugins` (USD, FBX, glTF, OBJ, STL); `Resources/MLModels`
  (FastMask, ShotCutDetection) and tokenizer folders (Whisper, BERT, Marian); `.lproj`
  localisations (11 languages, [localization-parity.md](localization-parity.md)).
  After Effects was not launched for this pass, so there is no menu dump.
- **From Adobe's public release notes** for 26.2 (April 2026: Object Matte, displacement on
  parametric meshes, Quick Apply, Proportional Scrubbing), 26.3 (June 2026: Advanced 3D depth of
  field, paste SVG/AI as shapes, faster mask tracker, copy frame to clipboard, Curl Noise,
  variable-font filter) and 26.5 (September 2026: percentage guides, Object Matte disk cache,
  effect colour labels, a rebuilt Effect Controls panel, ACES 2.0 through OpenColorIO 2.5.1).
- **From this repository at `origin/main` 1446e27 (10 October 2026):** 587 menu entries in the
  After Effects-shaped menu tree (`crates/engine/src/menus.rs`), 411 registered engine commands,
  306 registered effects (280 on the GPU, [effects.md](effects.md)), 13 export formats
  (`OutputFormat::ALL`, with uncompressed QuickTime since #778), the import extension lists (`crates/media/src/lib.rs`), 2,704 tests, ≈ 329,000
  lines of Rust, 6 UI language catalogs, and the 134 open / 179 closed GitHub issues. On 12 October (`upstream/main` 470ea7a3) the counts were 216 open / 328 closed issues, 401 merged PRs, ≈ 361,000 lines of Rust and 10 menu catalogs (6 panel catalogs).
- **Presence is not parity.** A feature counts toward *breadth* when it exists and works in the
  common case; it counts toward *ready* in proportion to how much of the area a professional
  could rely on, discounted for open bugs in it, for being unmeasured against After Effects, and
  for being checked on one platform only.

### Calibration of the hours

Hours are **Opus 5.5 agent wall-clock hours**: one agent session working sequentially, tests and
verification included. Calibrated against this repository's own history:

- The whole repository (≈ 329,000 lines including tests, 1,142 commits, 233 merged PRs) was built
  between 1 and 10 October 2026, typically by 4–8 agents at once: roughly 700–1,000 agent-hours,
  or ≈ 350–450 lines per agent-hour. Getting to today's ≈ 45% took that; the estimate to full
  parity is about 1–1.5× it again, because what is left is measurement, compatibility and
  long-tail behaviour rather than new code.
- Named arcs (from the audit notes kept below): Classic 3D ≈ 2.7 h, timeline depth (graph editor,
  keyframe dialogs, time remapping, pen and masks) ≈ 2.3 h, 85 effects ≈ 1.2 h, the After Effects
  menu bar ≈ 1.3 h, layer styles ≈ 0.6 h; a wave the audit priced at 21 h took ≈ 6 agent-hours
  but landed at 80–90%.
- Recent PRs (commit-to-merge): seven Timeline/viewer issues (#436) ≈ 0.6 h; eight tweaks from
  #290 (#383) ≈ 1 h; image-sequence import (#384) ≈ 4 h; panel i18n infrastructure plus 384
  Chinese strings (#398) ≈ 2–3 h; one 618-row menu language (#401 Ukrainian) ≈ 1.5–2 h; EXR
  beauty layer + OCIO v2 configs (#449) ≈ 1.3 h. So a user-reported behaviour bug costs ≈ 0.1–0.3 h,
  a mid-size feature 1–4 h.
- What history can't calibrate: fidelity work (each corpus case needs After Effects running,
  which only the owner's Mac has), broader `.aep` import beyond the existing py-aep corpus, and anything
  needing hardware we don't have. Those carry the widest ranges.
- **Parallelism:** about 70% of the hours parallelise across 5–8 agents (effects fidelity,
  languages, bugs, formats are independent). Serial chains: the G1 harness before per-feature
  fidelity scores and broader `.aep` reference captures before importer fidelity work. **Needs a human:**
  running After Effects for oracle captures, native-speaker review of every
  language, Windows/Linux GPU hardware for #613 (#591 and #596 are closed), and an optional decision on hosting
  After Effects SDK plug-ins.

## By feature area

Weights are the share of a typical motion designer's working time in After Effects (animation,
compositing and effects dominate; audio is light). Hours are to full parity for the area.

| Area | Weight | Breadth | Ready | Hours | Biggest gaps (details in [gaps.md](gaps.md)) |
|---|---:|---:|---:|---:|---|
| Project, footage and import | 8% | 92% | 55% | 25–45 | Relative-path media/proxy packages and relinking implemented; unbundled dependency warnings and runtime acceptance remain; Media Browser lists AVI, which it can't import (TGA/HDR/DPX/MXF import since #745, #756, #767); #431, #324, #552 and #351 are closed |
| Compositions, layers and compositing | 13% | 98% | 60% | 30–55 | 32-bpc blend modes differed from After Effects (#494, closed); expand precomps in place (#601); #527, #510, #513, #539 and #540 are closed |
| Animation: keyframes, graph editor, motion paths, puppet | 15% | 97% | 55% | 35–60 | Only scalar ease is measured against After Effects (168 samples); puppet lag (#586); #456, #457, #355, #553 and #318 are closed; Proportional Scrubbing (26.2) |
| Expressions and scripting | 7% | 93% | 65% | 20–40 | `.jsxbin`, Startup/Shutdown folders, script shortcuts (`#include` landed, #759); external editor (#364); see [scripting-parity.md](scripting-parity.md) |
| Shapes, masks and roto | 9% | 93% | 45% | 30–50 | Open mask and shape issues (#1004, #999, #1042, #932); #510, #513, #527, #509 and #303 are closed; paste SVG/AI as shape layers (26.3; SVG markup pastes since #738) |
| Text and typography | 7% | 94% | 50% | 20–35 | CJK text in comps renders wrongly (#453); paragraph alignment inverted (#316); soft text at Fit (#417, mostly fixed by #444); toolbar font options (#550); #483 and #583 are closed; variable-font filter landed (#741) |
| Effects and animation presets | 14% | 85% | 40% | 70–130 | All 306 effects exist, but rendered output is unmeasured against After Effects; 76 built-in presets (text, Transitions, Backgrounds, Behaviors) against After Effects' 621 ([effects-parity.md](effects-parity.md)); open effect bugs (#580 crash, #463, #369, #473, #474; #514 and #526 are closed); effect colour labels (26.5); see [effects-parity.md](effects-parity.md) |
| 3D (Classic and Advanced) | 6% | 82% | 35% | 35–65 | No FBX/USD/STL models, no Substance materials, mesh displacement unverified; no 3D transform gizmos (#392, #347); reflections (#261); camera aperture (#260); #489 and #315 are closed |
| Tracking and AI-assisted tools | 5% | 80% | 35% | 35–70 | Object Matte (26.2) has the effect, the viewer tool (click / box), background tracking, Freeze and the 26.5 disk cache, not yet compared with After Effects; Roto Brush (MobileSAM) and face tracking (MediaPipe) unmeasured against After Effects; camera-tracker view (#505) |
| Preview and cache | 7% | 92% | 40% | 25–50 | Playback stutters on cached frames (#595, #304); late audio (#504, #335); #329 and #554 are closed; no benchmark against After Effects |
| Render queue and export | 7% | 85% | 55% | 25–45 | No hardware encoding; ProRes 4444 colour tag (#340); rendering over an existing file (#325); #446 and #372 (8-bit uncompressed QuickTime, #778) are closed; HEVC/AV1 without B-frames |
| Audio | 2% | 85% | 45% | 6–12 | Audio sync and crackle (#335); audio to keyframes depth |
| **Weighted** | 100% | **≈ 91%** | **≈ 50%** | **≈ 360–660** | |

Feature depth (≈ 50%) feeds the overall *ready* number with the other dimensions below.

## By dimension

| Dimension | % | Hours | Evidence | Doc |
|---|---:|---:|---|---|
| Features, breadth | 91% | (in features) | Table above | this file |
| Features, ready for real work | 50% | 360–660 | Table above | this file, [gaps.md](gaps.md) |
| UI/UX fidelity | 65% | 60–120 | After Effects-shaped menus (587 entries), panels, workspaces, shortcuts; many interaction issues still open; multi-monitor only as separate panel windows (#486 open); Effect Controls not yet like 26.5's | [ui-parity.md](ui-parity.md) |
| File formats | 50% | 130–230 | `.aep` only through the py-aep converter (this fork); `.aepx` / `.mogrt` / `.ffx` can't be read; strong modern codecs (H.264, HEVC, AV1, VP9, ProRes) | [file-format-parity.md](file-format-parity.md) |
| Hardware | 40% | 60–120 | wgpu GPU on Metal/Vulkan/Direct3D 12/WebGPU with CPU fallback; no hardware decode/encode; Direct3D 12 compositor fails (#613); no video-out hardware | [hardware-parity.md](hardware-parity.md) |
| Localization | 18% | 60–110 + review | Average coverage of After Effects' 10 non-English languages (measured catalog rows over ≈ 3,900 strings): menus in 10 languages, panels in 6, effects in none; After Effects ships 11 languages fully | [localization-parity.md](localization-parity.md) |
| Performance | 35% | 50–100 | Internal numbers only; users report stutter and slow scrubbing | [gaps.md](gaps.md#g5-performance-at-real-world-scale) |
| Stability | 45% | 40–80 | Open crash reports #580, #501, #426, #341, #234, #879 (#591, #306 and #511 are closed); no CI runs the tests on pull requests | [gaps.md](gaps.md#g3-stability-and-a-green-main) |
| Platforms | 65% | 30–60 | Releases for macOS, Windows x64/x86/ARM64, Linux, FreeBSD and the web (more than After Effects), but Linux and Windows users hit basic bugs | [gaps.md](gaps.md#g2-real-user-reliability-on-every-platform) |
| Ecosystem and plug-ins | 20% | 40–80 | After Effects scripts and expressions mostly carry over; no After Effects SDK plug-ins, `.ffx` presets or `.mogrt`; our WebAssembly plug-in API has no third-party plug-ins yet | [plugins.md](plugins.md) |
| AI features | 35% | 40–80 (overlaps Tracking) | Roto Brush (MobileSAM) and face tracking (MediaPipe) as optional downloads; Object Matte (point / box prompt, same model; viewer tool, background tracking, Freeze, disk cache) unmeasured against After Effects; the After Effects bundle ships Whisper, BERT and Marian tokenizers and a shot-cut model we have no equivalent of | [gaps.md](gaps.md) |
| Text Gradient Fill (beyond After Effects) | ahead | — | A text animator property: a smooth per-pixel gradient across the selected word, line or run of characters that moves with their animation, with viewer handles and a stops bar; Lottie export drops it with a note, `.aep` never has it | [parity-log.md](parity-log.md) |
| Agent control (beyond After Effects) | ahead | — | Every command is drivable over MCP, the control channel and the CLI | [agents.md](agents.md) |

**Totals.** Summing the rows double-counts: AI overlaps Tracking, performance overlaps Preview,
UI overlaps every area. With the overlaps removed: **≈ 850–1,500 h to full parity**, of which
**≈ 350–620 h to beta**: reliable `.aep` / `.aepx` import beyond the converter (80–160 h), the G1 fidelity
harness and P0/P1 corpus (60–100 h), the open bug and crash backlog (40–80 h), performance to
real-time preview at 1080p (40–70 h), Linux/Windows reliability (30–50 h), and feature depth in
animation, masks, effects and text (100–160 h).

## Ready for real work: how the number is built

The full-target number is a written weighted sum, not a single judgement:

| Component | Weight | Value | Source |
|---|---:|---:|---|
| Feature depth | 60% | 50% | Weighted feature-area table above |
| Opening and delivering work files | 15% | 35% | [file-format-parity.md](file-format-parity.md): `.aep` / `.mogrt` / `.ffx` 0%, delivery formats strong |
| Performance | 10% | 35% | By dimension, below |
| Stability | 10% | 45% | By dimension, below |
| Platforms | 5% | 65% | By dimension, below |
| **Result** | | **46.2%**, reported as **≈ 45%** (40–50%) | Rounded down to the nearest 5: no live render comparison exists yet |

Checked on 10 October 2026 when the mainstream and essentials numbers were added: it is unchanged.

## Mainstream practitioner and essentials user

Two narrower questions, computed the same way in every craft app (craftrules
`standards/progress-docs.md`, "Numbers"). The stage still follows the full-target number and the
[alpha gate](roadmap.md#alpha-gate).

### Mainstream practitioner

**≈ 47%** (estimated).

*Can the typical professional motion designer do their weekly work here?* Areas are the alpha-gate
workflows and their everyday tools. Left out: third-party plug-ins and `.ffx` preset libraries,
generative/cloud AI (Object Matte, Firefly), Adobe services and team features, specialist hardware
(video I/O, tablets), languages other than English, and areas a typical designer uses rarely
(Advanced 3D models, 3D camera tracking, Roto Brush). Depth is for the everyday subset of each area
(the common effects, the common formats), which is why it sits above the full feature-area table.

| Area (everyday subset) | Weight | Depth | Evidence |
|---|---:|---:|---|
| Project and import: MOV/MP4, PNG/JPEG sequences, PSD/AI, WAV/MP3 | 10% | 75% | Import tests; #431 (image imported as a sequence) and #324 (MP3 silence) were counted here and are now closed |
| Comps, layers, blend modes, mattes, parenting, precomps | 16% | 80% | Compositor tests; #494 32-bpc blend modes and #539/#540 separated Position were counted here and are now closed |
| Keyframes, Graph Editor, easing, motion paths | 20% | 75% | 168 live samples match After Effects; #456/#457, #553 and #569 were counted here and are now closed |
| Expressions: `wiggle`, `loopOut`, linking, sliders | 6% | 80% | `crates/expr` tests; no live expression oracle yet |
| Shape layers and masks | 12% | 65% | #510, #513, #527 mask bugs and #303 Mesh Warp were counted here and are now closed; open: #1004, #999, #1042 |
| Text and text animators | 12% | 70% | #316 alignment inverted, #453 CJK (open); #583 font search (closed) |
| Everyday effects: blurs, glow, fill/tint, curves/levels, keying, drop shadow, displacement | 10% | 70% | All present with GPU paths; unmeasured against After Effects; #514 Apply Color LUT (closed) |
| Preview (RAM preview with audio) | 8% | 55% | #595, #304 stutter; #504 late audio (#554 closed) |
| Render to H.264 / ProRes / PNG sequences | 6% | 75% | Encoder tests; #340 ProRes 4444 tag (open); #446 default macOS output folder (closed) |
| **Average depth** | 100% | **72.2%** | |

Discounts for what still stops real work:

| Discount | Factor | Evidence |
|---|---:|---|
| Interaction fidelity | ×0.90 | UI parity ≈ 65% ([ui-parity.md](ui-parity.md)): viewer zoom keys (#570), multi-monitor only as separate panel windows (#486 open); Graph Editor jitter (#456) and snapping (#352) are closed |
| Stability on real machines | ×0.90 | Six open crash reports (#580, #501, #426, #341, #234, #879; #591, #306 and #511 are closed); Direct3D 12 compositor fails (#613); no CI on pull requests |
| File exchange with After Effects users | ×0.80 | Can't open `.aep`, `.aepx`, `.mogrt` or `.ffx` sent by colleagues or bought as templates; PSD, AI, footage and rendered deliverables exchange fine |

72.2% × 0.90 × 0.90 × 0.80 = **46.8%, ≈ 47%**. It sits only just above the full number because
the gap is not in After Effects' long tail: fidelity, stability and `.aep` exchange hit
mainstream users as hard as specialists.

### Essentials user

**≈ 52%** (estimated).

*Can someone who only touches the core features (a social-media animator, a student, a
first-time user) get their work done?* No advanced options, pro workflows or exchange edge cases.

| Core feature | Weight | Depth | Evidence |
|---|---:|---:|---|
| Launch, new project, undo, save, reopen | 10% | 85% | Versioned project reopen tests, auto-save and crash recovery |
| Import a video, picture or song | 10% | 75% | #431 and #324 (closed) |
| New comp; move, scale, rotate, fade and trim layers | 15% | 80% | `ui_viewer`, Timeline tests |
| Add and style text | 10% | 70% | #316 (open), #583 (closed) |
| Draw shapes and simple masks | 10% | 70% | Shape tool tests; #513 mask drawing feedback (closed) |
| Keyframes and Easy Ease | 15% | 85% | Ease measured against After Effects |
| Apply a few common effects | 10% | 75% | Effects & Presets search, Effect Controls tests |
| Preview with sound | 10% | 65% | #595 stutter (open), #554 late audio (closed) |
| Export an MP4 | 10% | 80% | H.264 encoder tests; #446 (closed) |
| **Average depth** | 100% | **76.75%** | |

| Discount | Factor | Evidence |
|---|---:|---|
| Launch and stability | ×0.88 | Start-up failures (#501, #234 macOS 27, #613 Direct3D 12; #596 is closed); #426 timeline crash while playing |
| Discoverability and UI clarity | ×0.90 | After Effects' dense layout reproduced faithfully; Home screen and interactive Learn tutorials help; renders fail silently on macOS until an output folder is chosen (#446); layout nitpicks (#581) |
| Opening files people send | ×0.85 | Tutorial projects and templates arrive as `.aep` / `.mogrt`, which don't open |

76.75% × 0.88 × 0.90 × 0.85 = **51.7%, ≈ 52%**.

### User evidence (GitHub, 10 October 2026)

Counted from all 315 issues and their comments, excluding the maintainer account and
agent-generated maintainer replies:

- **Praise:** 5 people in 5 issues say so outright: "felt almost exactly like After Effects.
  Really impressive" (#611), "performance has been excellent" (#326), "Awesome project" (#322),
  "Great work" (#293), "great tool and AE alternative" (#290). About 6 more open with "would love…"
  in a feature request.
- **"Switched from After Effects" reports:** none found. #468 is a working motion designer's
  list of Mac issues, so people are trying it on real work.
- **Open issues (136):** ≈ 95 are core-path bugs (masks, keyframes, timeline, preview, audio,
  text, render, crashes; this includes 15 community `fix(...)` proposals), ≈ 40 are feature
  requests or niche (plug-in hosting, OpenFX, ffmpeg export, Discord presence, Store listing,
  rigging tools). Mostly core-path bugs, which matches the discounts above.

## Out of scope by design

Adobe services (Team Projects, Libraries, Stock, Media Encoder, Dynamic Link, Frame.io,
Exchange, Firefly), third-party bundles (Cineware / Cinema 4D, Mocha), and formats without a
public specification or permissive data (`.prproj`, `.jsxbin`, Vanishing Point `.vpe`, the
predefined CJK CMaps in PDF import, Kodak film emulations) are not counted against breadth.
`.aep` / `.aepx` *are* counted: they are the main file format and decide beta.

---

## Appendix: feature-checklist audit history

Moved here from `docs/parity.md` on 10 October 2026 and kept as evidence. It scores the 92-item
catalogue in `plan/aftereffects/feature-catalog.md`, graded by the agents that built the
features; read it as breadth only. **Its per-area table ("By area", below) was stale**
(Interface 75%, Project 68%, Animation 70%, Paint 0% next to a 99% headline) and is superseded by
the feature-area table above; it is kept for its "biggest gaps" notes, which record what
landed when.

### Checklist status (audit at commit `d39c0e8`, 4 October 2026; updated 5 October for M3.9–M4.12)

| Measure | Value |
|---|---|
| **Feature parity, weighted by tier** (P0 ×3, P1 ×2, P2 ×1) | **≈ 99%** counting every partial feature as half done; ≈ 99.8% using per-feature fractions |
| Unweighted | ≈ 98.4% (half-credit) / 99.6% (fractions) |
| P0 / P1 / P2 | 100% / 98% / 90% (half-credit); 100% / 99.7% / 97.5% (fractions) |
| Features done / partial / missing | 89 / 3 / 0 of 92 |
| **Effects** | **306** effects, every one implemented in full ([effects.md](effects.md)); 280 run on the GPU |
| Disabled menu entries left | 1, on purpose: Import ▸ Vanishing Point (.vpe), whose format has no public specification |
| Remaining work | ≈ 8.5 agent-hours at the pace measured so far (≈ 26 on the conservative audit scale); ≈ 3.5 without the learned-model items |
| **Wall-clock estimate** | **≈ 2–3 hours** with five agents in parallel; ≈ 1 hour without the learned-model items |

Partial features:

| id | Tier | Done | What is missing |
|---|---|---|---|
| EFF-5 GPU effects | P1 | 0.98 | Fractal (its escape iteration needs f64) and the analysis / tool effects (Warp Stabilizer, Roto Brush, Camera Tracker, Paint…) render on the CPU; the 13 audio filters stay on the CPU by design |
| MSK-4 Roto Brush | P2 | 0.85 | Roto Brush 2.0 / 3.0 can use a trained model since M13.35 (MobileSAM, optional download); its quality has not been compared with After Effects (G1), so the score stands |
| TRK-3 Face tracking | P2 | 0.9 | Can use a trained model since M13.36 (MediaPipe Face Landmarker, optional download); the classical fitter is weak on profile and occluded faces; neither is compared with After Effects (G1), so the score stands; Rolling Shutter Ripple is approximated |

Since this audit: the project operations (M3.9–M3.14) and preview (M4.5–M4.12) passes fixed
behaviour inside features already counted as done, so the counts above stand: unsaved-changes
prompts and a modified mark that follows undo, Solid / Layer Settings with "Affect all layers that
use this solid", lock-aware layer commands, New Comp from Selection's options, menu-bar fixes,
project files that always reopen; RAM preview keys and eviction, Cache Before Playback that fits,
Cache Frames When Idle, Preserve frame rate / resolution when nested, nested comps past their end
and one motion blur gate (see [parity-log.md](parity-log.md)). Their code was checked against
[AGENTS.md](../AGENTS.md)'s "Never crash" rules (M4.12).

Before this audit: M13.22 and M13.23 put 70 more effects on the GPU (236) and composite every
Advanced 3D mode on the GPU; M13.24 runs GPU effects in the browser's render workers with a disk
cache in browser storage and a storage manager; M13.25 added the Home template gallery, Save as
Template and View ▸ Simulate Output ▸ My Custom RGB; M13.26 Premiere Pro interop through FCP7 XML,
FCPXML, OTIO, EDL, AAF and OMF (import and export, with pre-renders for layers a timeline can't
hold); WebM VP9 alpha now imports (FilmCraft); M13.27 pins the CPU simulation effects with golden
hashes.

Out of clean-room scope (no public specification or no permissively licensed data): native
`.prproj`, `.jsxbin`, Vanishing Point `.vpe`, the predefined CJK CMaps in PDF import, Kodak film
emulations. Adobe service integrations (Team Projects, Libraries, Media Encoder, Dynamic Link,
Frame.io, Exchange) and third-party plug-ins (Cinema 4D, Mocha) are intentionally absent.

Earlier audits and the per-milestone update sections are in [parity-log.md](parity-log.md).

### By area (stale; superseded on 2026-10-10 by the feature-area table at the top)

| Area | Weighted parity | Remaining (agent-hours) | Biggest gaps |
|---|---|---|---|
| Layers | 88% | 4.8 | motion blur of collapsed precomps (their own motion and the parent camera's) and of animated layer content |
| Output | 97% | 0.8 | HEVC/AV1 export lacks B-frames, multi-reference, SAO/CDEF/restoration and alpha; Opus lacks FEC/DTX (20 ms frames only); no Photoshop sequence output; movie overflow checked only at file creation |
| Audio | 85% | 0.5 | audio to keyframes |
| Import | 97% | 0.5 | JPX / JBIG2 images and predefined non-Identity CJK CMaps inside PDF/AI files, Illustrator EPS relying on Adobe procsets, PSD 3D layers |
| Automation | ≈ 99% | 0.1 | `.jsxbin`; the object model, ScriptUI, Essential Graphics hooks, Watch Folder and the WebAssembly effect plug-in API are in |
| Shapes | ≈ 80% | 1.5 | Lottie can't carry stroke taper/wave; taper, wave, multi-segment dashes, the pen tool and vertex editing are in |
| Compositions | ≈ 81% | 3.3 | Mocha-style planar tracks for templates; nested comp markers on the precomp layer bar |
| Animation | 70% | 9.0 | puppet depth beyond pins, recording, rigging and follow-through; Wiggler/Smoother/Motion Sketch |
| Text | ≈ 96% | 0.3 | no extruded strokes; variable font axes, OpenType features, vertical text, per-character styles and on-canvas editing are in |
| Web | 98% | 0.1 | No shared-memory threads inside one engine instance (one engine instance per worker by design; a threaded build needs nightly `build-std`) |
| 3D | 88% | 5.5 | reflections of other layers (Advanced 3D reflects only the Environment light, #261), multi-view layouts, the Extended Viewer for Advanced 3D comps |
| Effects | ≈ 85% | 3.0 | GPU versions of the remaining effects (280 on the GPU; Fractal and the analysis / tool effects on the CPU) and the missing controls listed as partial in [effects.md](effects.md) |
| Interface | 75% | 5.0 | more Learn tutorials and pixel-level fidelity of dialogs |
| Project | ≈ 68% | 5.5 | OCIO displays beyond the built-in tone map; complete dependency packaging beyond footage/proxies and cross-platform handoff acceptance |
| Masks & roto | 74% | 5.0 | (audit estimates) Roto Brush 2.0 / 3.0 with a trained model, Refine Edge, mask tracking, Mask Interpolation and variable-width feather are in |
| Preview | 77% | 2.8 | GPU effects for the audio-driven effects and the stepped particle systems; content-keyed RAM preview frames (an edit drops every comp's), audio scrubbing, more snapshot slots |
| Tracking | ≈ 96% | 0.3 | face tracking's trained model is an optional download; without it the classical fitter is weak on profile and occluded faces; Rolling Shutter Ripple is approximated |
| Paint | 0% | 4.0 | Brush, Clone Stamp, Eraser |

### Effects still missing

The full catalogue with GPU / 32-bpc badges and per-effect status is [effects.md](effects.md)
(generated from the registry).

None: the 3D Camera Tracker landed in M12.6. Boris FX
Mocha and Cineware are third-party and not counted.

#### Added in M9.11 (39 effects)

| Category | Effects | Notes |
|---|---|---|
| 3D Channel | 3D Channel Extract, Cryptomatte, Depth Matte, Depth of Field, EXtractoR, Fog 3D, ID Matte, IDentifier | Read auxiliary channels: multi-layer OpenEXR (any `layer.channel`, Cryptomatte manifests from the header; EXtractoR picks a layer and channels from popups of the footage's layers and channels, `layer.channels`, #295; the footage itself shows the beauty pass, never Cryptomatte or depth, #412) or the compositor's own depth / layer-ID / normal / UV / Cryptomatte pass for precomps of 3D comps. |
| Immersive Video | VR Blur, VR Chromatic Aberrations, VR Color Gradients, VR Converter, VR De-Noise, VR Digital Glitch, VR Fractal Noise, VR Glow, VR Plane to Sphere, VR Rotate Sphere, VR Sharpen, VR Sphere to Plane | Seam-aware equirectangular processing (wrap-around, pole continuation, latitude-scaled filters), mono / over-under / side-by-side layouts; VR Converter between equirectangular, cube maps (4:3, 3:2, 6:1), sphere map and 2D. |
| Color Correction | OCIO CDL / Color Space / Display / File / Look Transform, Color Stabilizer | Built-in minimal OCIO-style config (ACES2065-1, ACEScg, ACEScct, sRGB, Rec.709, Rec.2020, Display P3, linear variants, XYZ) from published matrices and transfer functions; `.cube` / `.3dl` / `.csp` / `.spi1d` / `.spi3d` / `.spimtx` LUTs and ASC `.cc` / `.ccc` / `.cdl`. Custom `.ocio` config files are read from the OCIO v2 documentation (Configuration ▸ Custom: colorspaces and display colorspaces with the scene / display reference spaces joined by the default view transform, roles, displays with v1 and v2 views, view transforms, looks; Matrix, File, Exponent, ExponentWithLinear, Log, LogAffine, CDL, Range, Allocation, ColorSpace, Look, DisplayView, Builtin (matrices and display curves) and Group transforms), so Blender 4.x / 5.x configs give the Standard, AgX, Filmic and False Color views (#410). Grading Primary / Tone transforms, the ACES output and gamut-compression builtins and HLG pass colours through, and the effect shows a warning naming them (`effect.warning`). |
| Distort | CC Flo Motion, Liquify, Rolling Shutter Repair | Liquify strokes are effect data replayed into a displacement mesh (`liquify.stroke` / `liquify.clear` commands; no interactive viewer brush yet). Rolling Shutter Repair uses the KLT tracker. |
| Blur & Sharpen | Camera-Shake Deblur, CC Radial Blur | Deblur substitutes aligned patches from sharper neighbouring frames. |
| Audio | Compressor, Distortion, Gate | Applied in the mixdown like the other audio effects. |
| Simulation | CC Hair, Particle Playground | Particle Playground: cannon, grid, layer exploder, layer map, gravity, repel, wall, persistent property mapper (no Particle Exploder, text particles or ephemeral mapper yet). |
| Keying | Key Light | The full Keylight 1.2 control set under a generic name ("Keylight" is a vendor trademark); `lookup("Keylight (1.2)")` and the Effects & Presets search ("keylight") find it. Keyers' colour eyedroppers sample the effect's input (`effect.pickColor`), so a screen that is already keyed can still be picked. |
| Utility | Color Profile Converter | Our colour spaces and ACES; rendering intents (perceptual gamut compression, relative / absolute colorimetric, saturation). |
| Matte | Mocha shape | Mocha's export format is not public: reads a documented JSON shape format instead. |

## Highest-value gaps, in order

1. ~~On-canvas text editing and per-character styles~~ (landed: M9.9–M9.10).
2. ~~Effect Controls widgets: angle dial, point crosshair, eyedropper, curves and levels editors.~~ (second wave, 2 October)
3. ~~Viewer basics: snapping, rulers, channel view, snapshots, exposure, a drawable region of interest.~~ (M0.13)
4. ~~A GPU (wgpu) compositor, then GPU effects~~ (M12.2: 2D compositing and 16 GPU effects; M12.7:
   Classic 3D runs, adjustment layers, 58 GPU effects and GPU particles; M13.9: 152 GPU effects).
5. ~~Pen tool for shape paths, shape vertex editing, free transform~~ (M6.5); the Layer viewer.
6. ~~Motion-path handles in the viewer; graph editor transform box and snapping.~~ (M5.8)
7. ~~Point tracking and stabilization~~ (M6.x; Warp Stabilizer M12.5, 3D Camera Tracker M12.6).
8. ~~Frame blending, collapse transformations, slip edit.~~ (M4.3–M4.4)
9. ~~A real 8/16/32-bit pipeline with linear blending and colour management.~~ (third wave; OCIO / ACES in M7.7)
10. ~~Drag-to-dock and floating panels, saved workspaces, native macOS menus.~~ (Undocked panel groups are OS windows of their own on the desktop.)
11. ~~Auto-save, crash recovery, recent projects~~ (M3.7, M14); unsaved-changes prompts (M3.9).
12. ~~Expression gaps: `sampleImage`, `footage()`~~ (landed with data footage, the error bar and the Expression Language menu; the `sourceText` style API landed in M9.9).
13. ~~Lottie, WebM, SVG and PSD import~~ (landed; a disk cache too).
14. ~~Puppet and paint tools~~ (second wave; puppet rigging and follow-through in M13.14–M13.16).
15. ~~Preferences and a shortcut editor that can rebind; real Wiggler, Smoother and Motion Sketch; the marker dialog.~~ (second and third waves)
16. ~~Motion blur of collapsed precomps and of animated content inside a layer (shape paths, text
    animators).~~ (M4.13)
17. ~~Content-keyed RAM preview frames, so an edit keeps the frames of comps it doesn't touch.~~
    (M4.14)
18. ~~View ▸ New Viewer (several Composition viewers)~~ (M13.32); ~~nested comp markers on the
    precomp layer bar~~ (M13.33), ~~audio scrubbing~~ (M13.34).

At the original audit the engine underneath (keyframes, expressions, shape operators, text
animators, Classic 3D, all 38 blend modes, the render queue) was already deep, and most of what was
missing was interactive tooling, settings stored but not rendered, about 69 disabled menu entries
and the large systems (tracking, puppet, paint, roto). All of those have landed since; one menu
entry stays disabled on purpose (Import ▸ Vanishing Point), and what is left is listed at the
top of this page and in items 16–18 above.


## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-15 | minor | Portable project collection, explicit unbundled dependencies and failure cleanup; regression coverage added, runtime execution deferred; parity estimates unchanged |
| 2026-10-12 | minor | Audited against `upstream/main` 470ea7a3: open crash list, closed-issue references in the area tables, export formats 12 to 13, localization ≈ 12% to ≈ 18%, `#include`, built-in preset count, 12 October repository counts |
| 2026-10-10 | minor | Readiness-by-audience table: hours to ≈ 95% for each of the three numbers; the full number stays an additive weighted sum (unchanged) |
| 2026-10-10 | minor | Added mainstream practitioner (≈ 47%) and essentials user (≈ 52%) numbers with weights, discounts and GitHub user evidence; wrote down how the full ready number is built (unchanged at ≈ 45%) |
| 2026-10-10 | minor | Stage checked against the core-workflow alpha gate: passes, still alpha |
| 2026-10-10 | major | Full re-measure against After Effects 2026 26.5 (installed bundle, 26.2–26.5 release notes, repository counts); two numbers (breadth ≈ 91%, ready ≈ 45%); feature-area and dimension tables with Opus 5.5 hours; `docs/parity.md` merged in as the appendix and its stale per-area table superseded |
| 2026-10-05 | minor | Checklist updated for M3.9–M4.12 (as `docs/parity.md`) |
| 2026-10-04 | major | Checklist audit at `d39c0e8`: ≈ 99% weighted breadth, 89 / 3 / 0 of 92 |
| 2026-10-02 | major | First audit: ≈ 64% weighted breadth |
