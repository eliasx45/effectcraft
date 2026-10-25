# Smart Freeze

Native port of the owner's Smart Freeze 0.4.6 workflow: cache expensive previews while
keeping the original composition editable. Exports and audio evaluate the originals.

## Workflow to preserve

- Freeze an entire composition, one precomp's contents, or adjacent ordinary 2D layers
  without precomposing or changing the originals' IDs, names, keyframes, audio or labels.
- Edit immediately restores live rendering. Refresh builds a new immutable cache.
- Manual is the default and performs no periodic dependency/media checks. Its frozen
  badge explicitly says MANUAL (unchecked), rather than implying freshness.
- Auto is opt-in per section: changes restore originals; eight seconds idle starts a
  background refresh. Activity yields the worker; only one freeze worker runs per session.
- A section list exposes live/checking/changed/rendering/draft/frozen/error states,
  reasons, progress, Edit, Refresh, Cancel, Auto, and navigation to the composition.
- Background work renders an immutable snapshot, optionally draft then full, retaining
  a successful draft if full rendering fails. Cancel, Edit, project switches and stale
  dependencies prevent late attachment. Native snapshots do not require saving first.
- Scoped dependency checks include nested sources, expressions/linked controls, media
  and render settings. Unsupported dependencies fail conservatively with an explanation.
  Media verification runs outside the UI thread with a three-second timeout. Manual
  sections do not start scans. A stalled filesystem can retain one checker thread,
  but retries cannot accumulate blocked checkers or authorize a stale cache.
- Undo can reuse matching verified caches; explicit Edit suppresses automatic reuse.
  Partial refresh reuses frames only where independence is proven, with a full refresh
  fallback for temporal, expression, nested or unknown dependencies.
- Measure live/cache sample costs before recommending Auto: at least 20% and 1 ms saved,
  and live rendering slower than the frame budget. Never claim a playback speedup from
  those samples alone.
- A soft cache budget reports actual storage. Superseded versions have a ten-hour grace
  period, with explicit early retirement and recovery metadata. Protect active jobs,
  current full/draft versions and unowned paths; cleanup never deletes source media.
- Export uses originals by default. An explicit Use Originals / Stop Auto action cancels
  work, restores all sections and disables Auto. Audio always comes from originals.

## Native implementation

Persist section intent separately from the authoring layers. Cache substitution belongs
to preview rendering, never destructive layer rewriting or the export default. Store
float pixels in the compositor's blending space to preserve transparency and avoid
double color conversion. A missing/corrupt cache falls back to originals with a reason.
Cache reads/rendering/dependency capture must not run on the UI thread.

## Controls and commands

Open **Window → Smart Freeze**. With no selection (or every layer selected), Freeze
targets the composition. One selected precomp targets its source; other supported
selections stay in their existing timeline. Edit restores originals; Refresh creates
a new cache version. Auto must be enabled explicitly for each section. Default freezes
follow composition-duration edits on Refresh. Explicit `start`/`end` ranges stay fixed;
`freeze.configure` can restore `followCompositionRange: true`. Changing a range restores
originals until the next Refresh.

| Command | Parameters / result |
|---|---|
| `freeze.freeze` | `comp?`, `layers?`, `mode: auto|comp|precomp|selection`, `name?`, `start?`, `end?`, `followCompositionRange?`, `quickDraft?`, `auto?` (also turn Auto on, as `freeze.auto`), `wait?` |
| `freeze.suggest` | Layers too heavy to play in real time, slowest first: `{candidates:[{comp, layer, name, msPerFrame, frameBudgetMs, ratio, samples, target, freeze}], autoHeavy}`; `freeze` is ready-made `freeze.freeze` params. `dismiss: {comp, layer}` hides one for the session |
| `freeze.list` | Persisted section intent, cache versions and measurements |
| `freeze.edit` / `freeze.refresh` | `section`; Refresh also accepts `wait` |
| `freeze.auto` | `section`, `enabled` |
| `freeze.configure` | `section`, `quickDraft?`, `name?`, `start?`, `end?`, `followCompositionRange?` |
| `freeze.cancel` | Cancel the owned freeze/check jobs; a completed draft remains usable |
| `freeze.originals` | Restore originals and stop Auto for all sections |
| `freeze.remove` | Remove one section's controls; retain its derived cache files |
| `freeze.storage` | Measure storage on a worker; `wait?` |
| `freeze.settings` | `budgetGiB?`, `retireOld?` |
| `freeze.cleanup` | Retire eligible versions/abandoned jobs; `retireNow?` bypasses only the ten-hour grace |
| `freeze.restore` | Restore a retired version by its recorded `directory`; `wait?` |

### Heavy layers (hint and `previews.freezeAutoHeavy`)

The viewer reports each frame's per-layer render times (the Render Time pane's numbers) to the
engine (`Session::note_layer_costs`). A layer is *heavy* when, over its latest 16 frames (at
least 8 measured), at least 80% took more than twice the composition's frame time and its
average does too. A layer inside a precomp that is itself heavy is left out (freezing the precomp
covers it), as are frozen and dismissed layers. `freeze.suggest` lists them.

The Timeline shows the slowest as a hint above its footer, "Stat Stack renders at 1.8 s/frame —
Freeze it?" (`timeline.freezeHint`, buttons `timeline.freezeHint.freeze` / `.dismiss`). Freeze runs
`freeze.freeze` with `auto: true`: the section is one undo step, edits restore the originals and
Auto re-freezes after eight idle seconds. Nothing freezes without the click unless
`previews.freezeAutoHeavy` (Preferences ▸ Previews ▸ Freeze Heavy Layers When Idle, off by default) is
on: then the slowest heavy layer is frozen the same way once editing and playback have been
idle for eight seconds, one section at a time and each layer once. Only the viewer's own
(local) frames are measured; frames from a cloud frame server carry another machine's times.

### Heavy footage (automatic proxies)

The same hint place offers a proxy for heavy footage first (`timeline.proxyHint`, buttons
`.yes` / `.no`): "Smoke is 6K and costs ~46 ms/frame here. Make a 1080p proxy?". A movie used
by a comp is suggested once when it is at least 1.5× the largest comp using it (either
dimension), when the hardware decoder refuses it (or it is H.264 beyond 4096×2304), or when its
layer's measured time stays above the frame budget (8+ frames). Yes (`proxy.autoCreate {item,
wait?}`) makes an H.264 proxy at that comp's size (aspect kept) in `Proxies/` next to the saved
project, in the background, and sets it as the item's proxy marked `auto`. No
(`proxy.dismissSuggestion {item}`) is saved in the project (`proxy_hint_dismissed`).
`proxy.suggest` lists the candidates.

Automatic proxies are **preview-only**: under Proxy Use ▸ Current Settings they are used only
by renders through the preview view of the footage (viewer, RAM preview). Render Queue items,
exports and Save Frame As read the exact view and use the original; only Use All Proxies renders
with an automatic proxy. Proxies set by hand behave as before.

`wait: true` follows the draft into the full render through the control channel. Jobs
remain visible through the shared Progress panel and `jobs.list`. Automatic cache
attachments are derived preview state: they do not add authoring undo steps.

Caches use checked, lossless float RGBA files in the compositor's blending space.
They preserve HDR values and transparency. Final exports always evaluate originals;
remote renderers also use originals because local cache files are not transferred.
The browser cannot create native freeze caches. Retirement adapters use macOS Trash,
the Windows Recycle Bin, and the freedesktop home Trash on Linux. A move that cannot
be completed (including a Linux cross-filesystem move) retains the cache and reports
the error; retirement never falls back to permanent deletion.

Cached frames are sampled at the composition's frame rate. Sub-frame evaluation and
alternate camera views use originals; an in-composition region aligned to the cached
pixel grid can crop the cache. This preserves rendering context instead of substituting
a frame from a different time or view.

Literal `comp("Name")` / `footage("Name")` expression references are captured by name,
including their nested dependencies. Opaque/dynamic global lookups are refused with
an explanation. Selection capture retains the original scene for camera, light and
layer-control reads. Installed/linked font files and the actual loaded font bytes
participate in freshness checks. Instance Essential Properties overrides bypass a
source cache made without those overrides.

Storage's 500 GiB default is a soft cleanup target. Current versions, a surviving
draft and its preceding full version, unowned content and linked paths are protected.
Only terminal, never-attached jobs older than 24 hours qualify as abandoned. Completed
outputs with missing ledger records are conservatively retained because attachment
may have succeeded before ledger maintenance failed. A successful full refresh adopts
previously attached drafts into the retirement ledger without resetting existing grace periods. Moving
data to Trash does not free disk space until Trash is emptied. Earlier saved projects
or undo states can need a retired version restored to its recorded location.

The native implementation currently bounds a section to 216,000 frames, 33,554,432 pixels
per cached frame, 10,000 dependency items and a bounded property/keyframe walk. Exceeding
these limits returns an error and leaves originals intact.

## Verification scope

Model round trips; command/undo/cancel/project-switch tests; stale/media/linked-control
tests; CPU/GPU live-versus-frozen pixel comparisons; selection audio and export-original
checks; draft/full/partial-reuse tests; cleanup protection and recovery tests; actual UI
inspection; measured busy-project interaction and repeated playback checks. Compile-only
evidence does not establish rendering fidelity or responsiveness.

## Validation record (2026-10-11)

Focused engine checks passed: 30 tests, with the native Trash test separately enabled
and passed on macOS. Coverage includes cache pixels, original export/audio, draft/full,
cancellation, blocked-worker project switches, Undo, scoped dependencies, partial frame
reuse, ROI and sub-frame fallback. The UI checks exercise real pointer controls and
verify that frozen pixels remain visible until Edit restores the changed originals.

A generated 320×180 solid/blur fixture matched the live Metal render exactly in both
2D and camera-based 3D (Apple M1 Max; maximum float delta zero). This is fixture evidence,
not general effect-parity certification. Native Windows Recycle Bin and Linux home
Trash probes moved and restored generated bytes; Windows/Linux engine checks passed.

In a headless full-app 640×360, 24 fps animated-noise probe, frozen playback measured
23.76–24.48 fps after warming. A subsequent timeline-ruler drag completed and sought
to the requested frame. These bounded checks do not establish foreground OS cursor
latency or performance on every project. A 121-section manual-mode engine probe ran
10,000 heartbeat/view calls in 4.876 ms with no jobs, media checks or project copies;
that measures the engine path, not full UI frame time.
