# Control protocol

The desktop app exposes a control channel for agents, tests and the MCP bridge:

```sh
effectcraft --control 9877        # or EFFECTCRAFT_CONTROL_PORT=9877
```

The port is `--control`, else `EFFECTCRAFT_CONTROL_PORT`, else Settings ▸ Scripting & Expressions ▸
Control Channel Port (9877) when Let Agents Control EffectCraft is on. The app prints the port it
listens on. A port that is already taken (usually by a second EffectCraft) is reported in the
window's status bar and on stderr: give each app its own port, and point its MCP bridge at it
(`effectcraft-cli mcp --bridge` without a port uses `EFFECTCRAFT_CONTROL_PORT`, else 9877).

The port must be a number from 1 to 65535, and `--control` overrides the variable. A bad port or an
unknown option prints an error and the app exits with status 1 before it opens a window.

- **Transport:** TCP on `127.0.0.1` only, one JSON object per line in each direction. Connections are
  persistent; requests on one connection are answered in order.
- **Request:** `{"id": 1, "method": "engine.execute", "params": {...}}` (`id` is echoed back, `params`
  defaults to `{}`). An optional top-level `"requestId": "<unique string>"` identifies a retry.
  String ids must contain 1–256 UTF-8 bytes (numeric ids are also accepted). While its id remains
  in the server's in-memory cache (128 entries shared across connections), a repeated request
  gets its first reply without running again. Running entries are never evicted; a cache full of
  running requests rejects a new id with `notRun: true`. Completed entries, including the receipts
  below, can age out as new requests arrive. This is finite, process-local replay protection;
  it does not guarantee exactly-once execution after cache eviction or an app restart.
  Replies larger than 256 KiB leave a bounded receipt instead of their payload. Retrying returns
  `ok: false, replyUnavailable: true`, the `requestId`, `originalOk`, `resultOk`, `outcome` and
  bounded `originalError` details. `completed: true` means the original control reply reported
  completion, `completed: false` records a script still waiting in a modal dialog, and
  `completed: null` means an error reply did not prove execution had finished. The receipt tells the caller to inspect the project or saved output; it never reruns the request.
- **Bridge reconnects:** each TCP connection reads `app.capabilities` first to learn the host's
  `processInstance` and `sessionId`. A bridge that lost a reply resends the same `requestId` at
  most once, and only to the same known process/session. It refuses to replay an uncertain write
  if either identity changed or the host does not expose identity. Older hosts can still receive
  fresh calls; harmless inspection reads may be retried when identity is unavailable. Timeouts,
  malformed replies and mismatched reply ids are not automatically retried. Inspect the project
  before making a new mutation after an uncertain failure.
- **Reply:** `{"id": 1, "ok": true, "result": ...}` or `{"id": 1, "ok": false, "error": "message"}`.
- Requests run on the UI thread between frames. Input methods (`ui.click`, `ui.key` and similar)
  reply after the synthetic input has been processed. Methods that need an element that isn't drawn
  yet are retried for a few frames. A command that waits for a background job (`wait: true` on
  `renderQueue.render`, the analyses, `footage.check`, Content-Aware Fill, Scene Edit Detection;
  `jobs.wait`, `render.frame` / `render.contactSheet`, which render on a copy of the project, and
  `file.importAep` / `file.importProject`, which read the file in the background)
  starts the job and replies when it ends, with the same result as the blocking
  command: the UI keeps drawing and other connections' requests run meanwhile. A request times
  out after 60 s. Clients sharing the app take turns: each `client`'s first request runs before
  anyone's second, and one frame runs at most 50 ms of requests (the rest run on the next frame),
  so the window keeps drawing. If the app hadn't started it
  by then, it is cancelled and never runs: the reply has `"notRun": true` and says it is safe to
  retry. If it had started, the reply says it may still complete.
- **Only requests are read:** every line must be a JSON object with a string `method` (blank lines are
  skipped). Anything else (text that isn't JSON, a JSON array or number, an object without `method`,
  invalid UTF-8, a line longer than 4 MiB) gets one error reply ending in "closing the connection", and
  the server closes the connection, so nothing sent after it runs. An HTTP request (for example a web
  page's cross-origin `fetch` to `127.0.0.1:<port>`) therefore can't smuggle a command in its body: its
  request line is rejected first. At most 32 connections are served at once; further ones get an error
  line and are closed. The port has no authentication, so only enable it while you use it.

Quick test:

```sh
printf '{"id":1,"method":"engine.execute","params":{"command":"comp.new","params":{"name":"A"}}}\n' | nc 127.0.0.1 9877
```

## Engine

| Method | Params | Result |
|---|---|---|
| `engine.execute` | `{command, params?}` | The command's result. Every engine command (`effectcraft-cli commands`) runs exactly as it does headless, so it never opens a dialog: `comp.new {}` creates a default comp. UI-only ids (`tool.*`, `view.*`, `window.*`, `playback.*`, `timeline.*`, `app.*`) fall through to the menu dispatcher. Unknown top-level params are rejected, and the error lists the accepted keys. |
| `ui.menu.invoke` | `{id, params?}` | Like a menu click: commands with empty params may open their dialog (New Composition, Solid Settings, file pickers). |
| Copy a Media Browser source path | `ui.menu.invoke {id: "mediaBrowser.copySourcePath", params: {path}}` | Copies the exact non-empty path string to the clipboard and returns `{path}`. Supports file/folder paths and virtual source identifiers. Missing, empty or non-string paths return an error. |
| UI commands (via either) | | `timeline.column {column, visible?, width?, before?}` (av, keys, label, num, name, comment, switches, modes, parent, in, out, duration, stretch: shows/hides (toggles when nothing else is given), resizes the name, comment, parent, in, out, duration and stretch columns, or moves the column before another, `before: null` to the right end; returns `{columns, order, widths, outlineWidth, sourceName}`), `timeline.columnLayout {reset?, outlineWidth?}` (the layout; `reset` restores After Effects' defaults; `outlineWidth` places the outline / time graph splitter, `null` fits the columns; the layout is kept in `timeline_columns.json` in the settings store), `timeline.sourceName {value?}`, `timeline.search {query}`, `timeline.reveal.<kind>` / `timeline.revealAdd.<kind>` (the property shortcuts and Shift+shortcut: position, scale, rotation, opacity, anchor, effects, masks, feather, levels, animated, and the double-press sets maskPath, maskOpacity, timeRemap, expressions, material, paint, selected, missingEffects, waveform), `timeline.keyAt.<anchor\|position\|scale\|rotation\|opacity>` (Alt+Shift+A/P/S/R/T), `timeline.twirlSelected` (Ctrl/Cmd+`), `viewer.freeTransform {layer?, path?, end?, angle?, anchor?: [u, v]}` (Free Transform Points in the viewer, as a double-click on a path or Cmd+T with its points selected: no params starts it on the selected points' path; returns `{freeTransform: {layer, path, angle, anchor}}`; widgets `viewer.freeTransform.box`, `.anchor`, `.handle.0`–`.handle.7`), `timeline.collapseAll`, `flowchart.options {layers?, effects?, solids?, direction?: lr\|tb, comp?}`, `flowchart.graph {comp?}` → `{nodes, edges}`, `window.workspace {name}` (all After Effects workspaces), `app.fxConsole {query?}` (Edit ▸ Effect Console, Ctrl+Space: a search bar at the pointer, `fxConsole.search`, results `fxConsole.result.<i>`; Enter applies the effect or preset to the selected layers). |
| `engine.commands` | `{filter?, enabledOnly?}` | `[{id, label, menu, shortcut, params, enabled, why}]` |
| `ui.menu.list` | `{}` | The menu tree with enablement. |
| `render.frame` | `{comp? \| item?, layer?, time? \| frame?, max_side?, transparent?, region?, overlays?, path?, base64?}` | Renders a frame through the session, independent of the viewer zoom or resolution; the same code as the engine command `render.frame`. `comp` is an id or name (default: the active comp), `time` is in comp seconds (default: the CTI), `max_side` caps the longest side (0 = full size). `item` renders a footage or solid item on its own (source time), `layer` one layer solo, `region: [x, y, w, h]` one comp rectangle at full resolution (clipped to the frame; `max_side` applies to it), and `overlays` (`true` = bounds + anchor, or a list of `bounds`, `anchor`, `motionPath`, `safeMargins`, `grid`, `all`) draws the viewer's overlays and adds `layers: [{id, index, name, bounds: [x, y, w, h], anchor, keyPositions?}]` (comp space, as `layer.bounds` measures them). Writes a PNG to `path` (default: a temp file) and returns `{comp \| item, time, frame, width, height, scale, region?, path}`. With `base64: true` it returns `{…, png: "<base64>"}` (and still writes `path` when one is given). Several frames on one labelled sheet: `engine.execute` `render.contactSheet {times? \| count?, start?, end?, cols?, max_side?, labels?, gif?, …}`. |

Useful query commands for `engine.execute` (they aren't journaled or undoable): `project.summary`,
`comp.info {comp?}`, `layer.tree {layer, comp?, depth?, time?}` (every node has a `path`),
`prop.get {layer, path, comp?, time?}`, `editor.state`, `command.list {filter?, enabledOnly?, schemas?}`,
`command.describe {command}` (params doc and JSON Schema), `app.capabilities` (version, counts,
export formats, parity summary), `jobs.list` (Progress panel jobs, e.g. the background
`footage.check` after File ▸ Open), `render.snapshot {name, comp?, time?, max_side?, transparent?}`
(keep a rendered frame in the session, 16 at most), `render.snapshots {clear?}` and
`render.compare {a, b, threshold?, min_psnr?, max_side?, region?, amplify?, diff_path?, diff_inline?}`
(`a`/`b`: a snapshot name, `{snapshot}`, `{path}` of a PNG/JPEG, or `{comp?, time?, transparent?}`
rendered now) → `{width, height, max_abs, mean_abs, alpha_max_abs, psnr_db, ssim, changed_pixels,
changed_pct, pass, diffBounds, region?, diff_path?, diff_png?}` (`region: [x, y, w, h]` compares
one rectangle; `diffBounds` boxes the changed pixels or is null; metrics and size rules in
[agents.md](agents.md#verify-your-change-snapshot--edit--compare)). Analysis queries (see
[mcp.md](mcp.md#analysis-tools)): `layer.bounds {comp?, time?, layers?, pixels?, overlaps?}`
(layer geometry in comp space) and `audio.analyze {comp?, start?, end?, layer?, waveform?, path?}`
(peak, RMS, LUFS, clipping and silences of the mix). Design lint: `render.lint {comp?, time? | start?, end?, step?, rules?, minTextPx?, minContrast?}`
→ `{comp, compName, times, truncated, clean, counts {error, warning, info}, problems: [{rule, severity,
layer, layerName, time, until, message, other?}], notes}` (rules and how to read it: [agents.md](agents.md#design-lint)).
Motion summaries: `comp.motion {comp?, layers?, start?, end?, step?}` /
`layer.motion {comp?, layer, start?, end?, step?}` → per layer `segments` (enter, exit, move,
hold, scale, rotate, fade-in, fade-out), `offscreen` intervals and a `summary` sentence, plus
`overlaps` ([agents.md](agents.md#motion-summary)).
Recipes: `comp.recipe {comp?, layers?, path?, includeDefaults?}` → a compact JSON recipe of what
was authored (layers, structure, changed values and keyframes); `comp.fromRecipe {recipe | path,
name?, open?}` builds a new comp from one in one undo step ([agents.md](agents.md#reuse-a-library-comp-recipes)).
Agent exam: `exam.check {task? | checks?, base?}` scores the
project against an exam task's checks (each runs one read-only query and tests its result) →
`{passed, score, total, passedCount, results: [{id, desc, pass, reason, actual}]}`
([evals/agent/README.md](../evals/agent/README.md)).

`project.summary` and `editor.state` include the document `revision` (bumped on every change, undo and
redo included). Every command's `params` also accepts `expectRevision` (the command doesn't run and
errors with `document changed (revision X, expected Y)` when the document moved on) and `client` (a
driver name: its undo steps are tagged, and `edit.undo {"client": …}` refuses unless that client made
the last step). Both are handled by `Session::execute` and stripped before the command sees its params.
A number always names an id (layer, effect, mask…), never a position; use `"#n"` for the n-th.

Every engine command is reachable through `engine.execute`, and every interactive widget of every
panel registers an element id; both are enforced by `crates/ui-egui/tests/ui_agent_audit.rs`.

### Clients: cursors and claims

A request whose params carry `client` runs on that client's own cursor: active comp, current times,
selected layers / properties / keys / mask vertices / Project items, the current track and on-canvas
text editing (`Session::execute` swaps it in for the command and the user's back afterwards; batch and
script steps inherit it). A new client starts from a copy of the user's cursor; at most 64 are kept
(least recently used dropped). Open comp tabs, clipboards and tool and viewer options stay shared; a
client opening a comp adds its tab but sends no `OpenComp` to the UI. `editor.state` returns the
caller's view and adds `client` (when given), `clients` (`[{client, active_comp, time, selected_layers}]`,
most recent first) and `claims`.

| Command | Params | Result |
|---|---|---|
| `comp.claim` | `{comp?, ttl?}` + `client` (required) | `{comp, name, client, ttl, renewed, warnings?}`; `ttl` seconds, default 300, capped at 86400; the owner re-claiming renews; another client's live claim is an error naming it |
| `comp.release` | `{comp?, force?}` | `{released: [{comp, name, client}]}`; with `client`: that comp (only if you own it) or all your claims; without `client` only with `force: true` (anyone's, one comp or all) |
| `comp.claims` | `{}` | `{claims: [{comp, name, client, expiresIn, ttl}]}` |

While a claim is live, a command from another client that changes the claimed comp (layers, properties,
name) or deletes it fails with `composition "<name>" is claimed by <owner> (expires in 4m 12s); …` and
changes nothing; edits are checked in `Session::edit`, and anything else (undo, redo, history jumps)
by comparing the comp before and after the command. Requests without `client` are never refused.
Expired claims are dropped when next checked; opening another project drops all claims and cursors.
The TCP control server counts the open connections per `client` and runs `comp.release {client}` when
the last one closes.

## Change events

`{"method": "events.subscribe"}` → `{subscribed: true, revision, events}`. From then on the
connection also receives event lines (no `id`), at most ten bursts a second, coalesced:

```json
{"event": "project.changed", "revision": 42}
{"event": "selection.changed", "revision": 42, "layers": [3, 5]}
```

`revision` is the session's project revision (`app.revision` → `{revision}` reads it cheaply).
Replies and events share the connection: tell them apart by `event`. Closing the connection
unsubscribes.

## UI state

| Method | Params | Result |
|---|---|---|
| `ui.inspect` | `{}` | Window size, pixels per point, UI fps, serialized `UiState`, playback (`achievedFps`, `targetFps`, `realTime`, `shown`, `caching`, `waiting`), time, active comp, selection, open dialog, last render ms, `keyboardFocus`, `windowFocused`, `workspaceStorageError`. Read-only `preview` diagnostics are described below. |
| `ui.elements` | `{prefix?}` | `[{id, label, rect: [x, y, w, h], window?}]`: the widgets from the most recently completed paint pass, in points. ID-based input uses that same snapshot, without falling back to widgets removed since an older frame. On the desktop an undocked panel group is an OS window of its own: its widgets carry `window` (the group's id) and their rect is in that window's coordinates. |
| `ui.set` | `{tool?, workspace?, theme?, focused?, viewer?, timeline?, menuBar?, home?}` | `viewer: {zoom (null = fit), res: Full\|Half\|Third\|Quarter\|Auto, pan: [x,y], grid, rulers, safeMargins, transparencyGrid}`; `timeline: {pps (null = fit), start, graphEditor, graphMode: auto|value|speed, graphShowSet, graphAutoZoom, showModes, openLayers: [ids], openGroups: [uids]}`. |
| `ui.panel.show` / `ui.panel.close` | `{panel}` | Panels: `Project`, `Composition`, `Timeline`, `EffectControls`, `EffectsPresets`, `Info`, `Preview`, `Character`, `Paragraph`, `Align` and others. |
| `engine.execute` `window.*` (panel menu) | `{panel, comp?, rect?, stacked?, solo?, small?, anchor?, zone?}` | The panel menu's items (≡ beside a tab name, or right-click a tab; automation ids `panelMenu.<key>`): `window.closePanel` (on a Timeline comp tab, `comp` closes that comp's Timeline), `window.floatPanel` (Undock Panel), `window.dockPanel` (with `anchor`/`zone`, or back to its usual place), `window.closeOtherPanels`, `window.closePanelGroup`, `window.floatPanelGroup`, `window.maximizePanel` (Maximize Panel Group, as `` ` ``), `window.stackPanelGroup`, `window.soloPanels` (Solo Panels in Stack: one expanded panel at a time; `solo`, or toggle), `window.smallTabs` (Small Tabs: a shorter tab strip, for a docked or undocked group; `small`, or toggle), and `window.panelMenu {panel, x?, y?}` to open the menu. |
| `ui.playback` | `{action: play\|stop\|toggle\|status}` | `{playing, time, audio, levelsDb, peaksDb}` (audio: an audio preview is running; L/R meter levels and held peaks in dBFS) |
| `ui.resize` | `{width, height}` | Resizes the window (points). |
| `ui.focus` | `{}` | Activates the window and takes keyboard focus. Agents normally avoid this, because the app is started without stealing focus. |
| `ui.figmaPair` | `{client}` | The one-time Figma pairing prompt the app's Figma bridge shows (**Connect Figma?**, automation ids `figmaPair.allow` / `figmaPair.deny`). The reply waits for the answer: `{allowed}` (a 120 s timeout counts as Deny). |
| `app.quit` | `{force?}` | Closes the app. A modified project shows the Save / Don't Save / Cancel prompt first (automation ids `dialog.unsaved.save`, `dialog.unsaved.dontSave`, `dialog.unsaved.cancel`) unless `force: true`. |


### Preview diagnostics

`ui.inspect.preview` exposes playback observations without changing playback:

| Field | Meaning |
|---|---|
| `time`, `frame` | Current timeline time in seconds and frame number at the active composition's frame rate; `frame` is null without an active composition. These are timeline positions, not proof of physical presentation. |
| `playing`, `caching`, `waiting` | Current playback state, cache-before-playback state, and whether playback is waiting for a frame. |
| `shown` | Existing playback frame-advance counter. It does not count every fallback advance or physical display refresh. |
| `achievedFpsDiagnostic`, `realTimeDiagnostic` | Existing playback-rate estimate and target-rate comparison. They are distinct from top-level UI redraw `fps` and do not measure display scanout. They can be null when stopped, caching, or before enough observations. |
| `targetFps`, `frameStep` | Requested playback rate and preview plan frame step (1 without a plan). The real-time comparison accounts for the planned frame step. |
| `resolutionStep` | Playback resolution override in pixel steps; null means use the viewer's selection, available in serialized `ui.viewer`. |
| `observationTimeSeconds` | Current egui clock time in seconds; use differences within this app session, not as a wall-clock timestamp. |
| `lastShownTimeSeconds`, `lastShownAgeSeconds` | Last recorded playback frame-advance time on the same clock and its nonnegative age at inspection; both are null before an observation. |
| `intervals` | Since the preview started: frames `shown` and `skipped`, `p50Ms`/`p95Ms`/`p99Ms`/`maxMs` between frames shown, `seconds`. |
| `stream` | CR6b, a cloud preview playing from the server's playback stream (the running stream, else the last one; null before any): `state` (`starting`/`playing`/`ended`/`stopped`/`failed`, `error`), `warmupMs` (start request → first chunk decoded), `chunks`, `upgrades` (full-size copies of frames it had smaller), `late` (chunks decoded after playback reached them), `stallMs`/`stalls` (playback waiting after the warm-up), `bytes`, `kbps`, `decodeMs` (per frame, smoothed), first copies by resolution `full`/`half`/`quarter`, `stale` (edits while playing), `broken`, `size`; for the preview (CR6c): `held` (frames ahead found on this Mac, not streamed), `fromDisk` (read back from the disk cache), `streams` (one per run of missing frames), `toDisk` (written to the disk cache). |

The rate window is pruned only when another frame advance is recorded, so the diagnostic rate can
remain stale during a stall. Inspect its age and `waiting`, and sample `time`/`frame` over known
playback loop ranges to observe timeline progress. Neither these samples nor `shown` establish
physical presentation FPS. The existing `playback` object remains available for compatibility.

`ui.inspect.renderQueue` shows what the preview's frame renderer is doing: `queued` (and the first
`queuedFrames`), the frames `running`, `landing` (GPU frames whose readback is in flight),
`parked` (jobs waiting for memory), `urgentRunning`, `inflight` (queued or rendering) and
`layerCacheWaitTimeouts` (process-wide count of frame threads that gave up waiting for another
thread's layer render). A frame stuck in `running` while playback waits, with the CPU idle, is a
stall; run with `RUST_LOG=warn,effectcraft=info,effectcraft_stall=debug` to log every slow frame,
decode, wait and GPU pipeline creation (over 150 ms) with its thread.

`keyboardFocus` is `null` when no egui widget has keyboard focus. Otherwise it contains the
actual widget `id` (hexadecimal), its `rect: [x, y, w, h]` when available, an `automationId`
when the widget registered its response identity, and `registryCandidates` containing exact
rectangle matches. Candidates alone do not establish widget identity. Header, toolbar and
dock-tab controls register their response identity. `ui.focused` continues to identify the
focused panel; it is separate from keyboard widget focus. `windowFocused` reports the native
window focus received by egui, which can differ from the retained widget focus.

When a configuration store is available, current dock and floating geometry, focused and
maximized panels, and named workspace snapshots persist in `workspaces.json`. The current
layout and named snapshots are independent: changing a divider does not overwrite the named
snapshot until `window.saveWorkspace` runs. Changes save after pointer release, at most once
per second, with a final save on normal exit. Version 1 files are limited to 1 MiB and bounded
workspace trees; extra viewer IDs must be between 1 and 65,535. Native reads enforce the size
limit while reading and report I/O and UTF-8
errors. Unreadable, invalid or newer files leave the default workspace available and are
preserved for the entire session, including after later layout changes. Load and save errors
appear in the status message and the nullable
`workspaceStorageError` field; a failed write remains eligible for retry.
`window.saveWorkspace`, `window.saveWorkspaceAs` and `window.editWorkspaces` (rename, delete)
write the file immediately. Through `ui.menu.invoke` they return an error when it isn't written
(a failed write, a file preserved after a failed load, or no configuration store);
`engine.execute` hands them to the UI and returns `{frontend}` first, so check
`workspaceStorageError` there. The changed workspaces still apply for the session, and a later
save retries them.

## Input

Points are logical window coordinates. A target is `{id}` (the element's centre; add `fx`/`fy` between
0 and 1 to pick a spot inside it) or `{x, y}`. `modifiers` is `{shift, alt, command, ctrl}`, where
`command` is ⌘ on macOS and Ctrl elsewhere.

| Method | Params |
|---|---|
| `ui.click` | `{id \| x,y, button?: left\|right\|middle, count?, modifiers?}` |
| `ui.move` | `{id \| x,y, down?, up?, button?, modifiers?}`: `down` presses and holds the button there, later moves drag, `up` releases |
| `ui.drag` | `{from: target, to: target, steps? (12), modifiers?}`: press, move, release |
| `ui.scroll` | `{id \| x,y, dx?, dy?, unit?: point\|line, pinch?, modifiers?}`: `line` is a mouse wheel (zooms the viewers); `point` (default) is a pixel-precise scroll, which the viewers take as a wheel until one moves on both axes (a trackpad: from then on it pans); `pinch` is a trackpad pinch's zoom factor |
| `ui.key` | `{key}`, for example `Space`, `F9`, `Cmd+Z`, `Cmd+Shift+Z`, `PageDown`, `Escape`; `down: true` presses and holds it, a later `up: true` releases it (key holds: G / A wheels, `\` Claude's version) |
| `ui.type` | `{text}`: text input into the focused field |

Input on an element in an undocked panel group's OS window (one with `window` in `ui.elements`)
goes to that window; with `x`,`y` pass `window` (the group id) for the same. `ui.key` and `ui.type`
go to the window the last pointer input went to.

Shortcut presets also accept `Physical+<shortcut>` for a keyboard position, using egui's
US-position names (for example `Physical+6` and `Physical+Minus` distinguish AZERTY's
`-` and `)` keys). Physical bindings take precedence over logical bindings for that key
press. `ui.key` accepts the same notation to test them. Ordinary shortcuts remain layout-aware.
`time.workAreaStart` and `time.workAreaEnd` navigate to the first and last included work-area
frames and can be assigned through `shortcuts.set`.

## Locating things

| Method | Params | Result |
|---|---|---|
| `ui.viewer.locate` | `{x, y}` (comp pixels) | `{x, y}` window point |
| `ui.viewer.hit` | `{id \| x,y}` (window point) | `{x, y}` comp pixel |
| `ui.timeline.locate` | `{layer (id), time?}` | `{x, y}` window point on the layer bar |

## Screenshots

| Method | Params | Result |
|---|---|---|
| `ui.screenshot` | `{path?, panel?, id?, max_side?}` | `{path, width, height}`: a PNG of the window, or cropped to one panel or element, downscaled so the longest side is at most `max_side` pixels (default 1600, 0 = full size). It captures the main window only: a panel in an undocked OS window is an error (dock it with `window.dockPanel` first). |

Element ids are stable, for example `tools.Selection`, `panel.Timeline`, `panel.tab.EffectControls`,
`panel.tab.Timeline.<comp>` (one Timeline tab per open comp; `panel.tab.Timeline` is the shown
one), `panel.tab.Viewer(<n>)` and `viewers.<n>` (View ▸ New Viewer's other Composition viewers;
`viewers.0` is the Composition panel while another viewer is active), `header.workspace.Animation`, `project.item.<id>` (a double-click opens the item, Enter
renames it), `viewer.comp`, `viewer.handle.<layer>.<i>`,
`timeline.layer.<id>.bar`, `timeline.layer.<id>.nestedMarker.<i>` (a precomp's comp markers), `timeline.layer.<id>.twirl`, `timeline.layer.<id>.row` (click selects,
Enter or a double-click renames, drag reorders), `timeline.prop.<uid>.stopwatch`,
`timeline.key.<uid>.<frame>` (and `timeline.keyAt.<uid>.<ticks>`, exact in layer-time ticks, for
keys on subframes), `timeline.cti`, `effectControls.prop.<uid>.value` (an angle's
revolutions are `timeline.prop.<uid>.revolutions`, `effectControls.prop.<uid>.revolutions` and
`properties.prop.<uid>.revolutions`), the Properties panel's shape rows
`properties.shape.<key>[.value[.0|.1]|.stopwatch|.prevKey|.keyKey|.nextKey]` for the first shape group and
`properties.shape.<n>.<key>` for the n-th (`size`, `roundness`, `ellipseSize`, `points`,
`outerRadius`, `innerRadius`, `outerRoundness`, `innerRoundness`, `fillColor`, `fillOpacity`,
`strokeColor`, `strokeWidth`), a solid's `properties.solid.roundness`, and `effects.item.<name>`. The Composition viewer adds `viewer.magnification`, `viewer.resolution`,
`viewer.roi`, `viewer.grid`, `viewer.channel`, `viewer.exposure`, `viewer.snapshot`,
`viewer.showSnapshot`, `viewer.fastPreviews` (their popup entries are `viewer.<menu>Item.<n>`),
`viewer.ruler.top|left|origin`, `viewer.mask.<uid>.vertex.<i>`, `viewer.shapePath.<uid>.vertex.<i>`,
`viewer.motionPath.<layer>.<key>[.in|.out]`, `viewer.puppetPin.<uid>[.rotate|.scale]`,
`viewer.freeTransform.handle.<i>` and
`viewer.regionOfInterest`; the Graph Editor adds `timeline.graph.transformBox[.<i>]`,
`timeline.graph.snap`, `timeline.graph.typeOptions` and `timeline.graph.propertyOptions`.
Open the type menu to access `timeline.graph.valueGraph`, `timeline.graph.speedGraph`,
`timeline.graph.autoSelectGraphType` and `timeline.graph.reference`; the properties menu exposes
`timeline.graph.showSelected`, `timeline.graph.showAnimated` and `timeline.graph.showSet`.
`timeline.prop.<uid>.graphPin` toggles a property's saved Graph Editor Set membership, also
available via `engine.execute` with `prop.graphPin {layer, path|prop, value?}`. Pins are project
metadata and support undo; Show Graph Editor Set is a view toggle. Handle split/join is exposed
through `keys.setEase {layer, path|prop, time, side, continuous?, speed?, influence?}`.
Use `ui.elements` to see what is on screen.

Viewer state that agents drive headless too: `view.snapping`, `view.snappingOptions
{edgesExtended?, collapsedFeatures?, toggle?}` (the Tools bar's Snapping options,
`header.snappingOptions`: After Effects' Snap Edges Extended and Snap to Features in Collapsed
Compositions and Text Layers), `view.channel {channel, colorized?}`,
`view.exposure {stops | delta}`, `view.resetExposure`, `view.takeSnapshot`, `view.showSnapshot`,
`view.adaptivePlayback {on?}` (Settings ▸ Previews ▸ Adaptive Resolution While Playing; no `on`
toggles), `view.fastPreviewMode {mode}` (one mode each, for shortcuts:
`view.fastPreviews.off|adaptive|draft|fastDraft|wireframe`, and for the channels
`view.channel.red|green|blue|alpha|redColorized|greenColorized|blueColorized|rgbStraight`), `view.res.full|half|third|quarter|auto {comp?}` and
`view.res.custom {factor, comp?}` (the comp's Resolution, saved with the project but no undo
step; `ui.set {viewer: {res}}` sets the active comp's, and `ui.inspect` reports it as
`ui.viewer.res`), `view.setRegionOfInterest {rect}`, `view.addGuide {percent?}`,
`view.addPercentGuide`, `view.convertGuides {to: percent|pixels, index?}`,
`view.moveGuide` (comp pixels, whatever the guide's unit), `view.removeGuide`, `effect.setLabel {layer?, effect, label}`; editing: `shape.newPath` (Pen on shape layers),
`shape.newShape` (the shape tools: a new group in the selected shape layer, else a new shape layer;
`roundness` px rounds a rect's corners), `layer.roundCorners {layer?|layers?, roundness?}` (Layer ›
Create › Round Corners and the Properties panel's Roundness: sets the Roundness of a shape layer's
Rectangle Paths; a solid becomes, in one undo step, a shape layer drawing a rectangle of its size
and colour that keeps its id, name, timing, transform, switches, masks, styles and markers, and
renders the same at roundness 0; solids with effects, adjustment layers and other solids that
could not render the same are refused with the reason),
`shape.toolOptions` (the Tools bar's Tool Creates Shape / Mask and Fill and Stroke Options),
`mask.insertVertex`, `mask.convertVertex`, `mask.deleteVertices`, `path.freeTransform` (masks and
shape paths by uid), `keys.setSpatialTangents` (motion-path handles), `keys.bendSpatialSegment`
(bend the motion path between two keys by their tangents, no key added) and `keys.transform` (Graph
Editor transform box, timeline Alt-drag scaling).

The MCP server's bridge mode (`effectcraft-cli mcp --bridge 9877`) is a thin client of this protocol;
see [agents.md](agents.md).

Keyframe command times (`prop.set`, `prop.addKey`, `keys.set`, `keys.select`) default to layer
seconds. Set `timeBase: "comp"` for comp seconds; `keys.set.newTime` and every
`keys.select.keys[].time` use the same input base. Omitted `prop.set` or `prop.addKey` times use
the target comp's CTI. Supplied write times snap to its nearest comp frame before conversion
to stored layer time, including on stretched or reversed layers. Existing `keys.set` neighbor
constraints and roving adjustment can change the final stored time.

Explicit bases add `layerTime` and `compTime` to unsplit `prop.set` and `prop.addKey` replies,
alongside `value` or `count`. Separated Position `prop.set` keeps an array of per-axis replies.
Explicit `keys.select` replies contain `count` and `keys[]`, with timing on each key in the
resulting selection. Key metadata's `time` remains layer seconds regardless of the input base.
Without `timeBase`, these three commands retain their existing JSON value/count replies,
including outside the layer's `[in, out)` range. Timing metadata carries outside-range warnings;
`keys.set` and `prop.get.keys[]` report both bases and warnings without an explicit base too.
`prop.get.time` is comp seconds. For these commands, `path` or `prop` identifies a property, not a group.
Set a layer's offset with `layer.timing {layer, start}` (comp seconds).

Panel moves, tab ordering, edge splits and floating geometry use shared docking transactions.
`window.floatPanel` requires exactly four finite numbers when `rect` is supplied. Invalid
geometry or missing source/anchor panels leaves the layout unchanged. The persisted workspace
shape is unchanged: accordion open states and explicit heights, floating groups, and dynamic
viewer/ScriptUI identities remain app data. Pointer drops use the same shared placement rules
as commands, with floating groups resolved in their actual window stacking order.

`window.dockAction` accepts `{action}` using the shared docking action's JSON form. It
covers tab moves and ordering, split and floating geometry, activation, and closing.
For example, `{"action":{"ResizeSplit":{"path":[],"size":{"FixedFirst":360}}}}`
resizes the root split; path entries use `false` for the first child and `true` for
the second. `{"action":{"MoveFloating":{"panel":"Info","rect":[45,65,420,320]}}}`
repositions and resizes the floating group containing Info. Invalid actions leave the
layout unchanged. Pointer gestures dispatch through this same catalog command.
EffectCraft also supports `SetStackOpen` and `ResizeStack`. Dynamic panel identities use
`{"Viewer":42}` or `{"ScriptPanel":77}` in the action. Closing runs the existing viewer
and ScriptUI lifecycle callbacks; opening uses the existing panel creation commands.

## Frame channel

`effectcraft-cli serve --frames PORT [--gpu] [--token T]` also serves viewer
frames to a desktop app that renders in the cloud (a rented GPU). It listens on 127.0.0.1 only:
reach it through an SSH tunnel (`ssh -L 9878:127.0.0.1:9878 …`). Frames need a binary channel
next to the JSON lines, so each message is one length-prefixed frame
(`effectcraft_engine::remote_wire`):

```text
"ECF1" | header length (u32 LE) | payload length (u32 LE) | header (JSON) | payload (bytes)
```

- The first message is `{"type":"hello","token":…}`. A wrong token, or anything else, gets
  `{"type":"refused"}` and the connection closes; five wrong tokens in a row refuse every token
  for a minute. The token comes from `--token`, `EFFECTCRAFT_FRAME_TOKEN` or
  `<config dir>/frame-server-token` (created with mode 0600 on first use).
- `{"type":"msg","msg":…,"want":…,"urgent":…}` carries a `FrameMsg` (`project`, `patch`,
  `render`, `segs`, `purge`). The app is the source of truth: each connection holds a replica of
  its project and only renders it. Replies are `{"type":"reply","reply":…,"pixels":…}` with a
  `FrameReply`; a frame's pixels are the payload.
- Render requests queue for a pool of workers (8, capped by `--frame-threads` /
  `EFFECTCRAFT_FRAME_THREADS`; one frame on the GPU at a time, `EFFECTCRAFT_FRAME_GPU_SLOTS`): urgent first (the newest first), then prefetch (the oldest first), then lossless
  refines. `{"type":"cancel","ids":[…]}` drops requests. A request for another revision than the
  replica's, also one that turned old while it rendered, answers `failed` with `stale: true`.
- `want`: `codec` `jpeg` (scrubbing; quality 85 by default; strips of 64 rows, each its own JPEG,
  plus a lossless alpha plane when the frame isn't opaque), `qoi` (lossless) or `raw`;
  `maxSize` scales the frame down to the viewer's size; `refine` sends the lossless version of a
  JPEG frame once the server is idle (`pixels.refine`). Pixels are premultiplied RGBA8, as the
  viewer shows them.
- `{"type":"control","id":…,"method":…,"params":…}` runs a control-channel request on the host
  (`effectcraft_automation::frame_link::RemoteControl` is a `ControlTransport` over it);
  `{"type":"echo","id":…}` sends the payload back.
- The server logs render ms, encode ms, bytes and queue depth per frame on standard error.
  `effectcraft-cli frames-bench --frames HOST:PORT [F.ecproj]` measures a server: round trips,
  scrub latency and, with `--duration`, a soak of random requests.

`effectcraft_engine::remote_wire::FrameClient` is the client: it reconnects with backoff (0.1 s
doubling to 5 s) and reports each connection, after which the app sends its project again. A lost
connection is an event, never an error for the document.

### Playback stream

For playback the app asks for a comp to be *played* instead of one request per frame
(`effectcraft_engine::remote_stream`, `plan/cloud-render.md` CR6). The server renders ahead of the
app's playhead in play order and sends chunks of about half a second, each one closed H.264 GOP
(SPS/PPS + IDR first, P frames only, Annex B; FilmCraft's pure-Rust `h264enc`), so any chunk
decodes alone (`effectcraft_media::preview_stream::decode_chunk`). Frames are opaque: the comp's
background colour is composited on the server.

App → server, as `{"type":"msg","msg":{"type":"stream","op":…}}`:

| op | fields | |
|---|---|---|
| `start` | `stream`, `revision`, `comp`, `from`, `to?`, `loopFrom?`, `looping`, `opts`, `codec` (`h264`), `crf` (23), `chunkFrames` (15), `aheadSecs` (4), `adaptive` (true), `maxSize?` | play from `from`; a running stream with the same id restarts |
| `seek` | `stream`, `time` | the playhead jumped; a new chunk grid from there |
| `playhead` | `stream`, `time`, `playing`, `received?` (newest chunk `seq` the app has) | a few times a second while playing: flow control and adaptive resolution |
| `invalidate` | `stream`, `from` | frames from `from` changed (the server also works this out from each sync) |
| `stop` | `stream` | |

Server → app, as `{"type":"reply","reply":{"type":"stream","op":…}}`:

| op | fields | payload |
|---|---|---|
| `started` | `stream`, `width`, `height` (full size), `frameRate`, `chunkFrames`, `codec` | |
| `chunk` | `stream`, `seq`, `revision`, `index`, `first` (comp frame), `start`, `frames`, `frameRate`, `width`, `height`, `scale` (1, 0.5, 0.25 of full size), `codec`, `renderMs`, `encodeMs` | the H.264 chunk |
| `stale` | `stream`, `revision`, `from` | chunks of older revisions from `from` on are out of date; new ones follow |
| `end` | `stream`, `at` | every chunk up to the end of the range is sent |
| `failed` | `stream`, `error` | the stream ended (for example a frame that can't render while footage is missing) |

- Chunks sit on a grid of `chunkFrames` frames anchored at `from` (or the latest seek); frame `i`
  of a chunk is at `frameRate.tick_of(first + i)`. A chunk can come again for the same frames at a
  higher `scale` or a newer revision: the newest wins.
- The server keeps `aheadSecs` plus its current chunk latency ahead of the playhead. The first
  chunks (warm-up) go at quarter, then half size; while playing, a lead under 1 s lowers the
  resolution one level and a lead over 2.5 s raises it (at most once per 1.2 s); chunks sent below
  full size go again at full size when there is time.
- Test it: `effectcraft-cli stream-test --frames 19880 F.ecproj --comp NAME --start S --duration 30
  [--ahead 8] [--edit-at S]` plays through the tunnel and reports warm-up, each chunk against its
  deadline, stalls and bitrate; `stream-bench F.ecproj --gpu --start S [--render-only]` measures
  render and encode on the server.

**The desktop app (CR6b, `crates/ui-egui/src/cloud_stream.rs`).** With Preview On ▸ Cloud and the
cloud ready, a playing preview opens a stream at the playhead (the Preview panel's range and loop,
the viewer's render options, `aheadSecs` 8 or what the RAM preview holds) on the CR5 connection.
Chunks decode on two threads into the RAM preview cache under the viewer's frame keys (half and
quarter chunks too, scaled up in the viewer; the newest copy of a frame wins), and playback runs
as for local frames: it waits at a missing frame and the sound follows its clock. Nothing renders
or is asked for on the Mac meanwhile. Edits go as patches; after `stale`, the old frames keep
playing until the new revision's arrive. Stop or pause sends `stop`. CR6c: the stream's frames stay on
this Mac when it stops: below-full-size ones as *drafts* (lighter green on the cache bar; the
paused viewer renders the exact frame on screen over one), only stale ones are taken out; current
frames also go to this Mac's disk cache (keys at the size they came, never found as rendered
frames). A stream plays one run of frames this Mac doesn't hold (RAM, disk), so a replay, also
after a restart, downloads nothing it has; a seek plans again from the new frame. A `failed` stream or a lost connection ends it: the preview
goes on rendering on the Mac with a status note. `EFFECTCRAFT_CLOUD_STREAM=0` turns the stream off
(CR5's frame requests then). `ui.inspect.preview.stream` has its numbers.
## Render box

`effectcraft-cli serve --gpu --follow APP_PORT --control PORT` is a render box: a windowless
EffectCraft on the same machine as the desktop app that mirrors the app's project, so agents
render and lint there instead of on the GPU and UI thread the user plays back on. The app stays
the one source of truth: the box is read-only (a command that changes its project is undone and
answers an error), keeps no checkpoints, is not in the host list, and `host.release` just stops it.

- **`app.sync {follower, have?}`** (the app, and any `serve` host): what the follower needs to hold
  the session's revision. `have` is the app revision it holds; when it matches what was last sent
  to `follower`, the reply is a CR3 patch of the comps and layers that changed (a pointer diff,
  cheap on the UI thread), else the whole project. Reply `{revision, kind: same|patch|project,
  msg?, bytes?, cursors, path}`: `msg` is base64 of `remote_sync::encode_msg` (a whole project
  zstd-compressed); `cursors` is `{user: {comp, time}, agents: {name: {comp, time}}}` (time in
  ticks), so a render without `comp` or `time` shows what it would in the app. The app makes the
  bytes on another thread. At most four followers per session.
- The box pulls before every `engine.execute` / `render.frame` (a render shows the edit made just
  before it), every 200 ms while the project keeps changing and every 500 ms when it is calm.
  An app without `app.sync` (older builds) is followed by `file.saveCopy` when its revision
  changed, only before requests. A render the box can't bring up to date fails with
  `render box out of sync with the app: …`.
- At most two renders run at once on the box; later ones queue. `host.info` adds `follow:
  {app, mode: sync|copy, appRevision, connected, pulls, patches, projects, errors, bytes, lastMs,
  lastError}` and `queued`.
- `EFFECTCRAFT_GPU_VRAM_CAP_MB` caps the box's video memory: creating a texture or buffer past
  that share of the driver's budget fails, and the frame renders on the CPU instead of taking
  memory from the app (on Vulkan / Direct3D 12 the cap shrinks while other programs use the card).
- **MCP:** `effectcraft-cli mcp --bridge APP --render-box BOX` sends `render.frame`,
  `render.contactSheet`, `render.lint`, `render.snapshot` and `render.compare` to the box and
  everything else to the app. Replies carry `renderedOn: render-box | app`. While the box is
  unreachable or out of sync, renders run in the app (retried on the box after 10 s).

## FigCraft link

FigCraft (the sibling design app) shows and plays EffectCraft comps on its canvas, like Dynamic
Link: a card on FigCraft's canvas is a live view of a comp range. FigCraft calls these methods on
the desktop app's control channel; they never open dialogs. `comp` is a composition id or name
everywhere. Junk parameters are an error (`{"ok": false, "error": "figcraft.<method>: …"}`).

- **`figcraft.frames {comp, frames: [int], maxSide: int, quality?: int}`** → `{comp: id, revision,
  width, height (the comp's size), frameRate, frames: [{frame, w, h, jpeg: "<base64>"} | {frame,
  pending: true} | {frame, failed: true}]}`.
  - `frames` are comp frame numbers at the comp's frame rate, at most 64 per request; duplicates,
    non-integers and frames outside the comp are dropped. `maxSide` (clamped to 32–1920) is the
    longest side FigCraft wants; `quality` is the JPEG quality (30–95, default 80).
  - The reply comes at once with what EffectCraft has **now** (no waiting for renders): a frame is
    taken from the RAM preview of the comp's viewer at ¼, ⅓, ½ or full resolution (and the scale
    the viewer shows now) — the smallest cached one whose longest side reaches `maxSide`, else the
    largest — scaled down to fit `maxSide` and encoded as a JPEG over black (GPU frames are read
    back first). Reading back, scaling and encoding run on a thread, not the UI thread.
  - A frame that isn't cached is queued as a **prefetch** render (never urgent, so the viewer's
    own frames keep their priority) at the smallest of ¼, ⅓, ½, full whose longest side reaches
    `maxSide`, with the viewer's render options, so the viewer reuses it (and a frame in the disk
    cache is just read back); it is reported `pending: true`. Ask again a little later: frames
    appear as soon as EffectCraft has them. A frame already queued isn't queued twice.
  - `failed: true`: the frame's render failed at this project revision (or couldn't be encoded).
    `revision` changes with every edit: re-ask for frames after it changes.
  - While four encoders are busy, cached frames are reported pending too.
- **`figcraft.audio {comp, start, end, rate?}`** → `{rate, channels: 2, start, end, wav:
  "<base64>"}` or `{silent: true}`. `start`/`end` are comp seconds (`end - start` ≤ 300; `start`
  below 0 counts from 0); `rate` is 8000–96000 Hz (default 48000). The WAV is 16-bit PCM stereo
  of the comp's mix (what previews play), mixed on a thread. `silent: true` when the comp has no
  audible layers, or the range is all silence.
- **`figcraft.tree {comp, start?, end?, depth?}`** → `{comp: {id, name, width, height, frameRate,
  duration}, children: [{layer: {id, name}, comp: {…}, start, stretch, in, out, timeRemap,
  children: […]}]}`: the precomp layers that show between `start` and `end` (comp seconds,
  default the whole comp), top layer first, `depth` levels deep (1–4, default 3). A child's
  `start` (where its time 0 sits), `in` and `out` are seconds in its parent comp and `stretch` is
  in percent; its own `children` are those that show in the part of it its layer shows (the
  whole comp when `timeRemap` is true).
- **`figcraft.reveal {comp, time}`** → `{comp, time}`: opens the comp (its viewer and Timeline tab,
  as double-clicking it in the Project panel does), moves the current time to `time` (comp
  seconds, kept inside the comp) and brings the window to the front. FigCraft calls it when a card
  is double-clicked.

**Composition ▸ Send to FigCraft** (`comp.sendToFigCraft {comp?, start?, end?}`, also in the
Timeline layer context menu and the Project panel's context menu of a comp) places a comp range on
FigCraft's canvas. The comp is the active one (the right-clicked one in the Project panel); the
range is the selected layers' span (earliest In point to latest Out point, clipped to the comp)
when that comp's layers are selected, else its work area, else (a work area that is empty or the
whole comp says nothing) the span of its graphics layers: text, shapes, solids, precomps and nulls,
leaving out footage, cameras, lights and layers that run nearly the whole comp
(`start`/`end` override it). The engine
returns the place params; the desktop app connects to FigCraft (127.0.0.1, port
`FIGCRAFT_CONTROL_PORT`, default 7979) on a thread and sends one line:

```json
{"id":1,"method":"engine.execute","params":{"command":"effectcraft.place","params":{"comp":"Promo","compId":12,"project":"/path/Promo.ecproj","start":1,"end":3,"fps":25,"width":1920,"height":1080,"name":"Promo · 0:01–0:03"}}}
```

`project` is the saved project's path (empty for an unsaved one). The toast says "Sent to
FigCraft" when FigCraft answers `ok`, "FigCraft isn't open" when nothing listens on the port, else
FigCraft's error. A headless session (CLI, MCP without the app) only returns the params.
