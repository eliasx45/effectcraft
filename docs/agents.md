# Driving EffectCraft from agents

Everything in EffectCraft is an engine command with a stable id and JSON params. Menus, shortcuts,
panel gestures, the CLI, the control channel and MCP all go through the same commands, so anything a
user can do, an agent can do too. There are three ways in:

| Interface | Best for | Needs a window |
|---|---|---|
| **MCP** (`effectcraft-cli mcp`) | Claude Code and other MCP clients | no (headless, or `--ui` with the app offscreen), or yes with `--bridge` |
| **CLI** (`effectcraft-cli exec/get/set/...`) | one-shot scripting and CI; JSON output with `--json` | no |
| **Control channel** (`effectcraft --control 9877`) | driving and seeing the live UI; see [control-protocol.md](control-protocol.md) | yes |

## MCP setup

Build once with `cargo build --release -p effectcraft-cli`, then register the server. For Claude
Code, add a `.mcp.json` at the project root:

```json
{
  "mcpServers": {
    "effectcraft": {
      "command": "/path/to/effectcraft/target/release/effectcraft-cli",
      "args": ["mcp"]
    }
  }
}
```

This repository ships a ready-made [`.mcp.json`](../.mcp.json): `effectcraft` (headless, demo
project loaded), `effectcraft-ui` (the full app offscreen with the demo project: screenshots and UI
input without a window) and `effectcraft-app` (bridged to a desktop app started with
`--control 9877`). All start through [`scripts/mcp.sh`](../scripts/mcp.sh), which keeps a release binary in `target/mcp`
(its own target dir, so it never waits on another build's lock). Only the first start compiles in the
foreground; after that the server starts at once, and when the sources are newer than the binary it
rebuilds in the background (log: `target/mcp/build.log`), so reconnect (`/mcp`) to pick up the new
build. On Windows, run `cargo build --release -p effectcraft-cli` and point `command` at the binary.

You can also register it from the command line:
`claude mcp add effectcraft -- /path/to/target/release/effectcraft-cli mcp`. Other clients (Claude
Desktop, Cursor and the like) take the same `command` and `args`.

- **Headless** (`["mcp"]`): an in-process session with no window. Add `"--demo"` or
  `"--project", "file.ecproj"` to start with content. Startup is instant. It renders on the CPU
  unless you add `"--gpu"` (`["mcp", "--gpu"]`), which attaches the GPU compositor on a device of
  its own; if no adapter is usable the server says why and exits. `doc_inspect`'s `project.renderer`
  (`render.backend`) reports `active` (`gpu` / `cpu`) and, on the CPU, `why`.
- **Offscreen UI** (`["mcp", "--ui"]`, add `"--demo"` or a project as above): runs the full app in
  the server process with no window (egui_kittest + wgpu, like the `snapshot` example), so
  `screenshot` (with `panel` / `id` crops), the `ui_*` tools and `control` work exactly as in bridge
  mode: every tool goes through the same control-channel requests, and clicks, drags, keys and
  typing are real input events. Nothing appears on screen, focus is never taken, and several servers
  can run side by side. `--ui-size 1680x1020` (points) and `--ui-scale 2` (screenshot pixels per
  point) set the window. It needs a GPU adapter (startup says which; without one it exits with the
  reason). `initialize` answers at once; the first tool call waits for the app's first frames
  (about a second, longer while Metal/Vulkan compiles the GPU compositor's shaders on a cold cache). Work is in memory until you
  `save_project` it; add `"--autosave"` (`["mcp", "--ui", "--autosave"]`) for the checkpoints below.
  Built with the CLI's default `ui` feature (`--no-default-features` drops it).
- **Bridge** (`["mcp", "--bridge", "9877"]`): drives a running `effectcraft --control 9877`, so you
  see every change live. Bridge mode adds `screenshot` and the `ui_*` tools.

After a broken connection, the bridge retries explicitly read-only queries once. This fork can
also resend the same request ID to the same identified process and session: the host's receipt
cache prevents executing an edit twice. Changed host identity prevents replay; unknown identity
allows only explicitly read-only queries. Uncertain writes report an unknown outcome because the
app may already have applied them: inspect the app before explicitly retrying. A later call
reconnects; timeouts and malformed replies are never automatically resent.

### From an installed release

The release packages ship `effectcraft-cli` alongside the desktop app, so no build is needed:

| Install | CLI |
|---|---|
| Windows (MSI) | `C:\Program Files\EffectCraft\effectcraft-cli.exe` by default (wherever you installed it otherwise), not on `PATH` |
| Linux (deb, rpm) | `/usr/bin/effectcraft-cli` |
| macOS | the separate `effectcraft-cli-<version>-macos-<arch>.zip` release asset (the `.app` holds only the desktop app) |

```sh
# Windows, default install folder
claude mcp add effectcraft -- "C:\Program Files\EffectCraft\effectcraft-cli.exe" mcp
# Linux, or macOS with the CLI unzipped onto PATH
claude mcp add effectcraft -- effectcraft-cli mcp
```

The server speaks JSON-RPC 2.0 over stdio, one message per line, and supports MCP protocol versions
2025-06-18, 2025-03-26 and 2024-11-05: tools (with structured results on 2025-06-18), resources and
resource templates with subscriptions, workflow prompts and logging; see [MCP conventions](mcp.md).

### Keeping headless work across restarts

Start `effectcraft-cli mcp --autosave` (MCP args `["mcp", "--autosave"]`, or
`["mcp", "--ui", "--autosave"]` for the offscreen app) to preserve unsaved headless work. With
`--ui` the app's project is checkpointed on its UI thread, between frames, with the same folder,
rules and metadata as below. Each tool call that changes a dirty project writes a checkpoint before
its reply is sent, so even killing the server after that reply leaves the work on disk. EOF
and transport errors also flush outstanding changes. Queries do not rotate the saved versions.
A failed checkpoint adds a warning to the tool reply; the edit remains in memory and the next
call retries. Use `save_project {path}` to a writable location before disconnecting if warned.

Recovery stays small on disk: a checkpoint of 8 MB or more is written at most every 30 seconds
(the change waits, `autosave.pending` says so, and the session's end always writes it), each
session keeps the Auto-Save setting's maximum number of versions, saving the project normally (or
undoing back to the saved state) removes the session's recovery files, and a new session removes
the folders of finished, saved sessions and of any session untouched for 30 days.

Each server has its own folder under `<config directory>/EffectCraft Auto-Save/MCP/` (or
`<custom Auto-Save folder>/MCP/`). The maximum-version setting applies within that folder.
`--autosave` enables these checkpoints regardless of the desktop's Auto-Save toggle and interval;
it reads settings without writing desktop preferences, shortcuts or crash-recovery state.
`EFFECTCRAFT_CONFIG_DIR` overrides the platform config directory. Unsaved sessions' folders are
kept for recovery (up to 30 days).

`initialize` instructions and `_meta.effectcraftAutoSave` list the current folder and up to five
previous dirty sessions; `doc_inspect`'s `project.autosave` also reports the latest checkpoint and errors.
Recover explicitly with `open_project {"path":"<checkpoint .ecproj>"}`, then
`save_project {"path":"Recovered.ecproj"}`. Prior sessions may still be running and are never
opened automatically. `open_project` (`path`, `demo` or `new`) refuses to replace a dirty project —
save it first, or pass `discard: true` to drop it intentionally. The metadata describes the
current project in each session.

Without `--autosave`, headless work is in memory until explicitly saved. The flag is for a
project in the server's process (headless or `--ui`); a bridged desktop app owns its auto-saves. An edit interrupted before its reply is received
may be absent from the last checkpoint.

See [MCP conventions](mcp.md) for core tools, resources, strict arguments, and progress/cancellation of long calls (renders, analyses with `wait: true`, `jobs.wait`, scripts).

Common output options on every tool: `full` (unbounded compatibility), `offset`, `limit`, and
`max_chars`. Engine-backed reads and edits also accept `expectRevision`; live page continuation
requires it. Retained `get_result` snapshots do not rerun edits or need a live revision.

### Tools

| Tool | What it does |
|---|---|
| `command_list {filter?, enabled_only?, schemas?, brief?}` | Discover command ids, their param docs (and, with `schemas`, a JSON Schema each), and whether each can run now. The full list is ~130 KB: `brief: true` returns only `{id, label}` (~40 KB), then `describe_command` for one command. |
| `describe_command {command \| id}` | One command in full: label, menu, shortcut, params doc, JSON `schema`, enabled / `why`. |
| `command_run {id, params?}` | Run any command (undoable). Unknown parameter keys are rejected with the accepted list. |
| `command_batch {steps, stop_on_error?}` | Run commands in order with per-step results; each edit has its own undo step. |
| `get_state` | Editor state (active comp, time, selections, tool) plus `app`: version, command and effect counts, export formats, parity summary. |
| `run_script {code, name?, timeout?}` | Run JavaScript with the After Effects-style scripting object model (`app.project`, `comp.layers.addText(…)`, `layer.property("ADBE Transform Group").property("ADBE Position").setValueAtTime(…)`…). Returns `{ok, result, output, error: {message, line, column}}`; edits are undoable. Stops after `timeout` seconds (default 300, `0` = none) or on `notifications/cancelled`. |
| `doc_inspect` / `get_comp {comp?, full?, offset?, limit?, max_chars?}` | Project overview (items, active comp, time, selection, undo stack) with the active composition; one comp's settings, `layerCount` and layers. Layers are compact rows (index, id, name, type, in, out, plus only non-defaults: start, stretch, parent, trackMatte, blendMode, switch tags such as `hidden`, `3d`, `solo`), paged (default 200 a call; `next_offset` null when done, `truncated` when cut). `full: true` gives every field (`comp.info`). |
| `get_layer {layer, comp?, time?, depth?, flat?, full?, offset?, limit?, max_chars?}` | A layer's property tree (`depth` limits how many group levels expand). Every node has a `path`. `flat: true` lists the leaves, paged (default 150; `total`, `returned`, `next_offset`); a big tree carries a `hint`. |
| `get_result {result_id, pointer?, mode?, offset?, limit?, max_chars?}` | Recover an immutable retained result by array page or exact JSON chunks, without rerunning its operation. |
| `get_field {source, pointer, comp?, layer?, path?, time?, depth?, flat?, offset?, limit?, max_chars?}` | Exact JSON field chunks for an omitted inspection field. Pin comp/time from the original reply; concatenate `text` via byte `next_offset`, then parse JSON. |
| `get_property {layer, path, comp?, time?, full?, offset?, limit?, max_chars?}` | A leaf property's value at a time, keyframes and expression. Keyframes are compact (`in`/`out` omitted when Linear, exact values) and paged (default 100; `keyCount`, `next_offset`); `full: true` for every key field. Group paths (for example `geometryOptions`) contain children: inspect them with `get_layer`, then query a leaf such as `geometryOptions/extrusionDepth`. |
| `set_property {layer, path, value?, time?, timeBase?, expression?, comp?}` | Sets a static value. With `time` it sets a keyframe (`timeBase: "comp"` for comp seconds); with `expression` it sets an expression. |
| `add_keyframe {layer, path, time+value \| keys:[...], timeBase?, interpolation?, comp?}` | Adds keys (layer seconds, or comp seconds with `timeBase: "comp"`), then optionally applies linear/bezier/hold/easyEase. |
| `list_effects {filter?, brief?}` | Effect ids, names, categories, GPU / 32-bpc support and parameters (id, name, default, a slider's valid `range`, a popup's `options`). `brief: true` returns only `{id, name}` (~19 KB instead of ~360 KB). |
| `list_fonts {query?, variable?, rescan?}` | Font families text layers can use, bundled and installed, with their styles, origin, whether each is a variable font and own-language name; `variable` lists only variable fonts; `rescan` picks up fonts installed since launch. |
| `add_effect {layer, effect, values?, comp?}` | Apply an effect and set its parameters in one call and one undo step (a failing value leaves nothing applied); returns the instance path (`effects/#n`) and its parameter paths. |
| `render_preview {comp? \| item?, layer?, time?, max_side?, region?, overlays?, path?, inline?, transparent?}` | Returns a PNG image of a frame; `transparent: true` keeps the alpha (as an RGB + Alpha render writes it) instead of compositing over the comp background. `region: [x, y, w, h]` (comp pixels) renders just that rectangle at full resolution (then `max_side` applies to it); `layer` renders one layer solo in its place; `item` renders a footage or solid item on its own (source time). `overlays: true` (bounds + anchor) or a list of `bounds`, `anchor`, `motionPath`, `safeMargins`, `grid`, `all` draws the viewer's overlays (boxes tagged `#n name` in label colours) and returns each drawn layer's comp-space `bounds` [x, y, w, h] (the `get_layout` geometry), `anchor` and `keyPositions`. |
| `render_frames {comp? \| item?, layer?, times? \| count?, start?, end?, cols?, max_side?, labels?, region?, overlays?, transparent?, path?, gif?, inline?}` | One contact-sheet PNG of several frames, each cell labelled with its time and frame number: `times` (s), or `count` frames (default 6, capped at 64) spread evenly over `start`..`end` (default the work area). `max_side` is the sheet's longest side (default 1600); `gif` also writes an animated GIF. Returns `{times, frames, cols, rows, cell, width, height}`. |
| `get_layout {comp?, time?, layers?, pixels?, overlaps?}` | Where layers are on screen at a time, without rendering (`layer.bounds`): `visible` (and `why` not), comp-space `bounds` [x,y,w,h], `corners`, `center`, `anchorInComp`, `onScreen` (0-1) and the layer-space `source` rect, after parenting and (3D) the active camera. `pixels` adds the drawn pixels' bounds (effects included); `overlaps` lists overlapping visible layers. |
| `get_motion {comp?, layer? \| layers?, start?, end?, step?}` | How layers move over a time range, without rendering (`comp.motion` / `layer.motion`): per layer `segments` (enter, exit, move, hold, scale, rotate, fade-in, fade-out with times, from / to, direction, edge), `offscreen` intervals and a one-sentence `summary`, plus `overlaps` between drawn layers. See [Motion summary](#motion-summary). |
| `analyze_audio {comp?, start?, end?, layer?, silence_db?, min_silence?, true_peak?, waveform?, width?, height?, path?}` | Measure the audio mix (`audio.analyze`): peak / true peak dBFS, RMS, integrated LUFS (BS.1770), clipped samples, silent ranges, per-channel stats; `waveform` returns a waveform image. No audio gives `hasAudio: false`. |
| `lint_frame {comp?, time?, start?, end?, step?, rules?, min_text_px?, min_contrast?}` | Design lint (`render.lint`): plain-language design problems at one time or over a range, each with a fix hint: text too small or low-contrast, outside title / action safe, cut off, overlapping text, layers switched on but not seen, keyframes just off the music's beat. Returns `{clean, counts, problems: [{rule, severity, layer, layerName, time, until, message, other?}]}`. See [Design lint](#design-lint). |
| `render_snapshot {name, comp?, time?, max_side?, transparent?}` | Render a frame and keep it in the session under `name` (16 kept, oldest dropped; the same name replaces). |
| `render_compare {a, b, threshold?, min_psnr?, max_side?, region?, amplify?, diff_path?, inline?}` | Compare two frames. `a` and `b` are each a snapshot name (or `{snapshot}`), `{path}` (a PNG/JPEG golden or After Effects reference) or `{comp?, time?, transparent?}` (render now; `{}` = active comp at the CTI). Returns `{width, height, max_abs, mean_abs, alpha_max_abs, psnr_db, ssim, changed_pixels, changed_pct, pass, diffBounds}` and, when something changed, the diff heatmap as an image. `region: [x, y, w, h]` compares one rectangle. See [Verify your change](#verify-your-change-snapshot--edit--compare). |
| `open_project {path \| demo \| new, discard?}` / `save_project {path?}` | Open and save files. |
| `hosts {join?}` | Where projects are open (each running EffectCraft, its port and project); `join` (a port or a project path) switches this server onto that host as your own `client`, so several agents work in one project. Refused while your own project has unsaved changes. |
| `undo {steps?}` / `redo {steps?}` | History. |
| `history {goto?}` | The branching undo history (History panel): every state with its index, id, label, branch depth and whether it's current; `goto` (an index or id) jumps to any state, on any branch, without losing the others. |
| `script_ui {action?, window?, widget?, value?, result?}` | Drive ScriptUI windows that scripts opened: `list` (default) the open windows, `get` a window's control tree, `click` a button / checkbox / radio button, `set` text, a slider or a list item, `close` a window (`result`: what a dialog's `show()` returns, default 2 = Cancel). |
| `batch {steps: [{command, params}], label?, atomic?}` | Several commands in one call and one undo step (`engine.batch`); `"$N.key"` in a param is step N's result (`"$1.layer"`). A failing step rolls the batch back and names the step. |
| *(UI: `--ui` or `--bridge`)* `screenshot {panel?, id?, max_side?, path?}`, `ui_inspect`, `ui_elements {prefix?}`, `ui_click`, `ui_drag`, `ui_key`, `ui_type`, `ui_set`, `control {method, params}` | Look at and operate the app window (offscreen with `--ui`, the live one with `--bridge`). |
| *(hidden aliases)* `list_commands {filter?, enabled_only?, schemas?, brief?}`, `execute_command {command \| id, params?}`, `get_project`, `render_frame {comp? \| item?, layer?, time?, max_side?, region?, overlays?, path?, inline?, transparent?}` | Older names of `command_list`, `command_run`, `doc_inspect` (its `project`) and `render_preview`. Callable, but not in `tools/list` unless the server runs with `mcp --all-tools`. |

Layers are referenced by id (from `get_comp`), `"#n"` (1-based index from the top) or name. Comps are
referenced by id or name, and default to the active comp. An id may also be sent as a string (`"98"`):
an all-digit string that matches no name is taken as the id. Property paths come from `get_layer`, for
example `transform/position`, `transform/opacity`, `effects/#1/blurriness` or `@57` (by uid). Times
are in seconds. Keyframe times are layer time, which equals comp time unless the layer is offset or
stretched.

A number is always an id, never a position: a layer, effect, mask, tracker or text animator id that
doesn't exist is an error (`no layer 7 (… use "#n" …)`), not whatever sits at that place. Use `"#n"` for
"the n-th from the top" (`"layer": "#2"`, `"effect": "#1"`, `"mask": "#1"`). Ids are never reused, also
after an undo, so an id you saw keeps meaning the same object or nothing.

When you share the document with the user or other agents, two execute parameters (accepted by every
command, also inside `engine.batch`) keep you from stepping on each other:

- `expectRevision`: the `revision` from `project.summary` / `doc_inspect` / `editor.state`. If the
  document changed since, the command doesn't run and returns `document changed (revision X, expected Y)`.
- `client`: a name for you (`"client": "my-agent"`). Your undo steps are tagged with it, and
  `edit.undo {"client": "my-agent"}` only undoes your own last step: if the last change was the user's
  or another client's, it refuses and changes nothing. `edit.history.list` shows each state's `client`.

To notice edits made by someone else without polling, the control channel's `events.subscribe`
sends `project.changed` and `selection.changed` lines with the new `revision`
([control-protocol.md](control-protocol.md#change-events)); MCP clients subscribe to resources
([mcp.md](mcp.md#notifications)).

Use MCP `execute_command` for engine commands `prop.set`, `prop.addKey`, `keys.set` and
`keys.select` with `timeBase: "layer" | "comp"`; omitted means `layer`. The convenience tools
`set_property` and `add_keyframe` take the same `timeBase` (layer time when omitted). The input base applies to
both `keys.set.time` and `newTime`, and to every `keys.select.keys[].time`.
Omitted `prop.set` or `prop.addKey` times use the target comp's CTI. Supplied write times snap
to its nearest comp frame before conversion to stored layer time, including stretched or
reversed layers; existing `keys.set` neighbor constraints and roving adjustment still apply.

Explicit bases add timing metadata to mutation replies; `keys.select` reports timing in
`keys[]` for the resulting selection. Returned key `time` and `layerTime` are layer seconds;
`compTime` is comp seconds. Omitted-base `prop.set`, `prop.addKey` and `keys.select` retain
their existing JSON value/count replies even outside the active range. Timing metadata
carries outside-range warnings; the existing objects from `keys.set` and `prop.get.keys[]`
also report both bases and warnings. `prop.get.time` is comp seconds. Separated Position
`prop.set` retains per-axis replies; for these commands, `path` or `prop` identifies a property, not a group.

Set offsets with `layer.timing {layer, start: 10.5}`; `in`, `out` and `delta` also use comp seconds; `inDelta` / `outDelta` trim each layer from its own in/out point.
For an unstretched layer starting at 10.5 s in a 30 fps comp,
`prop.addKey {layer: "A", path: "transform/opacity", time: 15.3,
timeBase: "comp", value: 0}` writes a key at comp 15.3 s (layer 4.8 s), matching
`prop.get {layer: "A", path: "transform/opacity", time: 15.3}`.

Command params are checked against each command's `params` doc. An unknown key, such as
`layer.select {"index": 2}`, returns an error that lists the accepted keys (here `layers, add, toggle`)
instead of being ignored.

Errors say how to fix the call: an unknown command, effect, tool argument or property path names the
closest match (`did you mean `inBlack`?`) and, for a path, the valid ids at the level that failed; a
popup value that isn't an option lists the choices. Popups take an option index (0-based) or name
(`"value": "Red"`).

**Warnings.** Values are stored as before, but when what was stored differs from what was sent the
result carries `warnings`: a slider value outside its valid range is clamped (`inBlack: 20 is outside
the valid range -1 to 2 (slider 0 to 1); clamped to 2`), a single number fills a vector (`position:
number 5 expanded to [5, 5, 5]`), missing components become 0. `prop.set` then returns `{value,
warnings}` (`prop.addKey`: `{keys, warnings}`), `set_property`, `add_keyframe` and `add_effect` add a
`warnings` array, and `batch` gathers them as `step N (id): …`. Read them: a clamped Levels value
renders very differently from what you meant.

### Several agents at once

Pass `client` on every request (`"client": "agentA"`) when you share the app with the user or other
agents. Over MCP, every tool takes it as an argument (`set_property {"layer": 2, …, "client":
"agentA"}`; the engine commands the tool runs carry it, and renders default to your time), or start
the server as `effectcraft-cli mcp --client agentA` (or set `EFFECTCRAFT_CLIENT`) so every call runs
as you; a call's own `client` wins. A bridged MCP server (`mcp --bridge`) given no name names itself
at `initialize`: the MCP client's name and the server's process id (`claude-code-4821`, also in the
`initialize` instructions and `get_state`), so an agent sharing the app never moves the user's view
by accident. Pass `"client": ""` on a call that should act as the user, for example to open a comp
in their viewer. Headless and `--ui` servers keep running as the user (the agent is alone there).
Two things follow:

- **Your own cursor.** Selection (layers, properties, keys, mask vertices, Project items), active comp
  and current time are yours: `layer.select`, `time.set`, `comp.new` / `comp.open` and every command
  that acts on "the selected layers" or "the active comp" use your copy, never the user's or another
  agent's. Your first request starts from a copy of the user's cursor. A comp you create or open gets a
  tab, but the user's viewer stays on what they are looking at. `editor.state` with your `client` is
  your view; without one it is the user's, plus `clients` (each agent's active comp, time and selected
  layers) and `claims`. At most 64 cursors are kept (the least recently used goes); requests without
  `client` behave exactly as before. A project you save remembers the user's open comps, front comp
  and current times (`openComps`), never yours.
- **Claims.** `comp.claim {comp, ttl?}` reserves a comp for you (`ttl` seconds, default 300, at most
  24 h; claim again to renew). While it holds, another client's command that would change the comp (its
  layers, properties or name) or delete it is refused, changing nothing: `composition "Main" is claimed
  by agentA (expires in 4m 12s); claim it after they release it, or work on another comp`. That includes
  an `edit.undo` that would reach into it. The owner and the user (no `client`) edit as always; the
  user never needs a claim. `comp.release {comp?}` releases one claim or, without `comp`, all of yours;
  `comp.claims` lists them (`{comp, name, client, expiresIn, ttl}`). Claims expire on their own, are
  dropped when another project is opened, and the control channel releases a client's claims when the
  last connection that used that `client` closes (keep one connection open, as the MCP bridge does).
  Without `client`, `comp.release` needs `force: true` (the user overriding a claim).

### Undo with several people in one project

`edit.undo` undoes the caller's own latest step: the user's (no `client`) or the agent's. When
someone else made the last step, only what the caller's step changed is reverted, as a new step
(`{undone, selective: true}`), so the user's ⌘Z never removes an agent's work and an agent's undo
never removes the user's. Inside a comp it works layer by layer: an agent adding or changing other
layers of the same comp doesn't get in the way. It's refused, changing nothing, when someone changed
the same thing since (the same layer, a comp's settings or layer order the step changed, a footage
item, a folder, the project settings); the error names who and what. Pressing it again goes on to
the caller's step before (an own-step undo isn't itself undone next). The user's
`edit.undo {any: true}` undoes the last step whoever made it, and the History panel still jumps to
any state.

### Which project is open where (hosts) and the save guard

One process holds one project. Every EffectCraft that holds one (the desktop app, a headless or
`--ui` MCP server) is listed in `<config directory>/Hosts/` with its kind, control port and open
project; `command_run hosts.list` returns the live ones (`{hosts: [{pid, kind, port, project, me}]}`).
To work on a project that is already open, join that host (the MCP `hosts {join: <port or
project>}` tool, or start with `--bridge <port>`; you keep your own `client`) instead of opening a
copy: `file.open` of a project another host has open succeeds but
warns, and its reply lists them in `openElsewhere`.

With the desktop app closed, a **windowless host** keeps one project in memory for several agents:
`effectcraft-cli serve Project.ecproj [--control PORT]` prints its port; agents join it like the app
(`effectcraft-cli mcp --bridge PORT`, or the MCP `hosts {join}` tool) and work on their own comps with
their own `client` and `comp.claim`. Commands that wait for a job (renders, analyses, `jobs.wait`)
reply when it ends without holding up the other agents; unsaved work is checkpointed (big projects
at most every 30 s); `host.release {path?}` saves the project and stops the host. Opening that project in the
desktop app does this by itself (a handover: the app asks the host to save and stop, waits, then
opens the saved file; the reply's `handover` says so), and the host's agents find the project in
the app with the MCP `hosts` tool. `ui.*` methods need the app.

Two copies of one file overwrite each other when saved, so saving is guarded: `file.save` /
`file.saveAs` over a file that changed on disk since this process opened or saved it (another
app, an agent's CLI, a script) is refused, naming the ways out: save under a new name
(`file.saveAs`), reload theirs (`file.revert`), or `force: true` to overwrite.

### A typical session

1. `command_run {"id":"comp.new","params":{"name":"Intro","width":1920,"height":1080,"frameRate":30,"duration":4}}`
2. `command_run {"id":"layer.newText","params":{"text":"Hello","size":160,"fill":"#ffffff"}}` returns `{"layer": 2}`.
3. `add_keyframe {"layer":2,"path":"transform/position","keys":[{"time":0,"value":[-300,540]},{"time":1.5,"value":[960,540]}],"interpolation":"easyEase"}`
4. `command_run {"id":"effect.apply","params":{"layer":2,"effect":"Gaussian Blur"}}`, then `set_property {"layer":2,"path":"effects/#1/blurriness","value":8}`
5. `render_preview {"time":1.0}` lets you look at the result, and `save_project {"path":"intro.ecproj"}` saves it.

### Verify your change: snapshot → edit → compare

Don't just assume an edit did what you meant; look. Snapshot the frame, make the change, and compare:

1. `render_snapshot {"name":"before","time":1.0}` → `{name, comp, time, width, height, snapshots}`
2. `set_property {"layer":"EFFECTCRAFT","path":"transform/opacity","value":50}`
3. `render_compare {"a":"before","b":{"time":1.0}}` (demo project) →
   `{"width":1920,"height":1080,"max_abs":118.0,"mean_abs":1.8705,"alpha_max_abs":0.0,"psnr_db":27.091,"ssim":0.99214,"changed_pixels":135386,"changed_pct":6.529,"pass":false,"diffBounds":[…],…}`
   plus the heatmap image (768 px wide inline): unchanged pixels are frame `a` dimmed to grey, differences within the
   threshold dark blue, changed pixels red → yellow → white by size (`amplify`, default 8: a
   32-level difference is white).

So a change that should touch only the title shows a heatmap lit only around the title, and a
refactor that should change nothing gives `pass: true` with `psnr_db: "inf"`.

- **Metrics.** Colour is compared premultiplied (each image over black, so colour under fully
  transparent pixels never counts) in 8-bit levels: `max_abs` and `mean_abs` over R, G and B,
  `alpha_max_abs` for alpha, `psnr_db` from the colour MSE (the string `"inf"` when identical),
  `ssim` the mean luma SSIM over 8×8 windows (1 = identical). A pixel is *changed* when any channel
  differs by more than `threshold` levels (default 1).
- **Pass.** `pass` is true when no pixel changed. With `min_psnr` (dB) the PSNR decides instead,
  for references that are close but never bit-exact, such as an After Effects render
  (`render_compare {"a":{"path":"plan/aftereffects/ref/title.png"},"b":{},"min_psnr":35}`).
- **Sizes.** `max_side` scales both images down (area average) first. Images with the same aspect
  ratio but different sizes (a half-resolution reference) are compared at the smaller size and the
  reply says which side was `resampled`; different aspect ratios are an error naming both sizes.
- **Where.** `diffBounds` is the `[x, y, w, h]` box of the changed pixels (null when none), so an
  edit that should touch only the title can be checked against the title's `get_layout` bounds.
  `region: [x, y, w, h]` restricts the metrics, `diffBounds` and the heatmap to one rectangle
  (clipped to the frame; an empty or outside region is an error). Both are in pixels of the
  compared frames: comp pixels unless `max_side` or the size rule scaled them.
- `diff_path` also writes the full-size heatmap PNG. Snapshots live in the session (the app's in
  bridge mode), not in the project; `execute_command render.snapshots {"clear":true}` lists or drops
  them. Over the control channel the same commands are `engine.execute render.snapshot` /
  `render.compare` (`diff_inline: true` returns the heatmap as base64 `diff_png`).
- For goldens and fidelity checks from the shell: `effectcraft-cli compare a.png b.png` (see [CLI](#cli)).

### Design lint

`lint_frame` (engine command `render.lint`) checks a comp the way a designer would glance at it
and lists what is wrong, in words an agent can act on. Run it before calling a design done:
`lint_frame {}` checks the current frame, `lint_frame {"start":0,"end":5}` samples every 0.5 s
(`step`) plus each layer's in point and keyframe time (at most 120 samples, else `truncated: true`).

```json
{"comp": 1, "compName": "Main", "times": [0.0, 0.5, 1.0], "truncated": false, "clean": false,
 "counts": {"error": 1, "warning": 1, "info": 0}, "rules": ["text-too-small", "…"],
 "problems": [{"rule": "low-contrast", "severity": "error", "layer": 3, "layerName": "Title",
               "time": 0.0, "until": 1.0, "message": "“Title”'s text colour #8a8a8a on its background (about #808080 under the text) has a contrast ratio of 1.14:1, below 3.0:1: make the text lighter or darker, change what is behind it, or add a stroke or shadow."}],
 "notes": ["off-beat skipped: the comp has no audible layers"]}
```

- **Reading it.** `clean` is true when there are no errors or warnings (info doesn't count).
  The same problem at several sample times is one entry from `time` to `until` (comp seconds; the
  worst occurrence's severity and message). `layer` is the stable layer id; pair rules add
  `other` / `otherName`. Problems come errors first, then by time. `rules: [...]` runs only some.
- `text-too-small`: the smallest drawn character's font size on screen (its em size through the
  layer's scale, parents, text animators and the camera; cap height is about 70 % of it) is under
  `min_text_px` (default comp height / 30: 36 px at 1080p) → warning; under half → error.
- `low-contrast`: the WCAG contrast ratio of the text colour (blended by the layer's opacity)
  against the mean colour under the text's box, measured on a small render of the comp without
  its text layers, is under `min_contrast` (default 3:1) → warning; under two thirds of it (2:1)
  → error. The message gives both colours and the ratio.
- `outside-title-safe` (text, warning) / `outside-action-safe` (other layers, info): part of the
  layer is outside the title-safe / action-safe box (Settings ▸ Grids & Guides: 10 % / 5 % per
  side by default). The message says by how many pixels on which side.
- `cut-off`: part (not all) of the layer is outside the frame: warning for text, info otherwise.
  Layers covering the whole frame (backgrounds) are never cut off or outside a safe area.
- `text-overlap`: two visible text layers' boxes overlap by more than 2 % of the smaller one
  (warning; `layer` is the upper one).
- `hidden-visible` (info): switched on and inside its in/out range, yet nothing shows: opacity 0,
  entirely outside the frame, or entirely covered by an opaque layer above (`other`; a solid, or
  footage whose alpha is ignored, at 100 % in Normal mode without masks or effects).
- `off-beat` (warning, only when the comp has audio): a key moment (a Transform or Opacity
  keyframe, or an in point after 0) within half a beat of a beat that misses it by more than one
  frame; the message names the beat to move it to. Beats come from the comp's audio mix (onset
  envelope, tempo by autocorrelation, beats by dynamic programming); with a single `time` only the
  key moments on that frame are checked. Without audio, or without a clear beat, a `notes` entry
  says it was skipped.
- `off-accent` (warning, only when the comp has a speech map): a keyframed property's speed-graph
  peak (the fastest moment between two eased keyframes; linear segments have none; keyframes only,
  no expressions) within `accentWindow` frames (default 4) of a stressed syllable's accent that
  misses it by more than one frame. The accent is `accentLead` frames (default 1) before the
  stressed vowel, never before the syllable's onset: Elias's hand-timed peak on "CONtent" sits at
  the k burst + 2 frames = the vowel − 1 frame. The message names the word, the onset, the vowel
  and the time to move the peak to.

### Speech maps (timing peaks to stressed syllables)

1. `scripts/fork/speech-map MEDIA --start S --dur S --fps 29.97 --out map.json`: ElevenLabs Scribe
   text (`scribe-words`), Montreal Forced Aligner phones with stress digits, then a forward
   acoustic snap of each word-initial and stressed onset (0.15 frames median against hand labels;
   `~/Developer/lab/S1-speech-timing/REPORT.md`). Times are media seconds + `--offset`.
2. `speech.import {path, layer?}` stores it on the comp (undoable, saved with the project). With
   `layer` (the voice footage) media time is mapped through the layer's start and stretch, and
   words outside its in/out are dropped; without it the times are taken as comp seconds.
3. `speech.get {stressed: true, start?, end?}` lists accents (`onset`, `vowel`, `accent`,
   `accentFrame`): where to put a speed-graph peak. `speech.accents {layer?, start?, end?}` lists
   every speed peak with its nearest accent and `offsetFrames` (negative = early); `hit` is
   within one frame. `render.lint {rules: ["off-accent"]}` flags the near misses.
4. `speech.clear` removes the map.
5. `keys.alignPeak {layer, path | paths, speech: "content" | frame | time, mode?, graph?}` moves a
   keyframed move so its speed-graph peak (the same finder as `off-accent`) lands on the target,
   ±0.5 frame, in one undo step. Modes: `shift` (default, both keys, duration kept), `stretch-in` /
   `stretch-out` (keep the second / first key), `ease` (keys stay, influences change). `graph` is one
   of Devin's five (`fast-in`, `quick-launch`, `smooth-emphasis`, `slow-launch`, `fast-out`;
   `keys.peakGraphs` lists them with their peak positions), `keep`, or a custom
   `{out: {speed, influence}, in: {…}}`; without it, the graph peaking where the target falls is
   picked. Keys in a precomp: `comp` is the layer's comp, `targetComp` the comp of the frame or
   speech map (found automatically for speech), `via` the precomp layers when nested twice. Keys
   land on whole frames unless that misses by more than half a target frame (`onFrames: false`).

Geometry is the `get_layout` geometry (track-matte sources and adjustment layers are skipped);
only `low-contrast` renders, at most 480 px on the long side, once per sample time that shows text.

### Cookbook

Short recipes; every step is one tool call.

| Goal | Calls |
|---|---|
| Orient yourself | `get_state` → `doc_inspect` → `get_comp {}` |
| Find the right command | `command_list {"filter":"mask"}` → `describe_command {"command":"mask.new"}` (params + JSON Schema) |
| Title card | `command_run comp.new {...}` → `command_run layer.newText {"text":"Hi","size":120}` → `render_preview {"time":0}` |
| Animate | `add_keyframe {"layer":2,"path":"transform/scale","keys":[{"time":0,"value":[0,0]},{"time":0.5,"value":[100,100]}],"interpolation":"easyEase"}` |
| Effect with settings | `list_effects {"filter":"glow"}` → `add_effect {"layer":2,"effect":"Glow","values":{"threshold":40,"radius":30}}` → `set_property {"layer":2,"path":"effects/#1/intensity","value":2}` |
| Key a green screen | `add_effect {"layer":2,"effect":"Keylight (1.2)"}` → `command_run effect.pickColor {"layer":2,"effect":"#1","param":"screenColour","x":20,"y":20,"average":true}` (a screen pixel, layer space; samples the effect's input) → `set_property {"layer":2,"path":"effects/#1/screenMatte/clipBlack","value":10}` → `render_preview {"transparent":true}` |
| Drive with an expression | `set_property {"layer":2,"path":"transform/rotation","expression":"time*90"}` |
| Inspect a layer | `get_layer {"layer":2,"flat":true}` (every property with its `path`, value, key count) |
| Is the title centred and on screen? | `get_layout {"layers":["Title"],"time":2}` → compare `center` with half the comp size, `onScreen` should be 1; `get_layout {"overlaps":true}` for collisions |
| Does the animation read right? | `get_motion {}` → each layer's `summary` ("Logo flies in from the left 0–1s, holds 1–4s, fades out 4–5s."), `segments` for exact times, `overlaps` for collisions over time |
| Check the soundtrack | `analyze_audio {}` → `integratedLufs` (streaming targets about -14), `truePeakDbtp` under -1, `clippedSamples` 0, `silences`; `analyze_audio {"waveform":true}` to see it |
| Check the result | `render_preview {"time":1.5,"max_side":640}`; with a UI (`--ui` / `--bridge`) `screenshot {"panel":"Timeline"}` |
| Prove what an edit changed | `render_snapshot {"name":"before"}` → edit → `render_compare {"a":"before","b":{}}` (metrics + diff heatmap) |
| Check an animation | `render_frames {"count":8}` (one labelled sheet), `render_frames {"times":[0,0.5,1],"layer":2,"overlays":["motionPath"],"gif":"anim.gif"}` |
| Where are the layers? | `render_preview {"overlays":true}` (boxes tagged `#n name`, anchors; reply `layers[].bounds`) |
| Check a detail | `render_preview {"region":[800,400,320,180],"max_side":0}` (full resolution crop of a big comp) |
| Undo a mistake | `undo {}`, or `history {}` then `history {"goto": n}` |
| Missing media after opening | `command_run footage.check {"wait":true}` → `{checked, missing, probed}`; then `file.findMissing {"what":"footage"}` |
| Bring in a Figma design | No token: serialize with the Figma MCP's `use_figma` + `integrations/figma/serialize.js`, then `command_run file.importFigma {"payloadPaths": [<parts>]}` (see "Figma designs"). With a token: `{"url": "<figma link>"}` |
| Open an After Effects project | `command_run file.importAep {"path":"x.aep"}` (needs Python with py-aep while converting; [packaging/aep/README.md](../packaging/aep/README.md)); the result is a native project |
| Merge another EffectCraft project | `command_run file.importProject {"path":"other.ecproj"}` → `{folder, items, comps, rootComps, primaryComp, missingMedia, warnings}` (into a folder named after the file) |
| Reuse motion from a library | `command_run library.search {"query":"lower third"}` → `command_run library.insert {"id":"<id>","time":2}` ([libraries.md](libraries.md)) |
| Reuse a library comp, adapted | `command_run comp.recipe {"comp":"Lower Third"}` → read and edit the recipe → `command_run comp.fromRecipe {"recipe":{…}}` ([Reuse a library comp](#reuse-a-library-comp-recipes)) |
| Save | `save_project {"path":"out.ecproj"}` |

The desktop app checks footage in the background after every open (a "Checking footage" job in
the Progress panel, `jobs.list`); headless sessions run `footage.check` when they want it.

### Motion summary

Agents can't watch a video. `get_motion` (engine `comp.motion {comp?, layers?, start?, end?,
step?}`, or `layer.motion {comp?, layer, start?, end?, step?}` for one layer) describes how layers
move in text, from the same comp-space content boxes as `get_layout` plus each layer's opacity,
scale and rotation; nothing is rendered, so it is cheap.

```json
{"comp": 1, "compName": "Intro", "start": 0, "end": 5, "step": 0.0417, "truncated": false,
 "layers": [{"layer": 2, "name": "Logo", "inPoint": 0, "outPoint": 5,
   "segments": [{"kind": "enter", "start": 0, "end": 1, "from": [-60, 90], "to": [160, 90], "direction": "right", "edge": "left"},
                {"kind": "hold", "start": 1, "end": 4, "from": [160, 90], "to": [160, 90]},
                {"kind": "fade-out", "start": 4, "end": 5, "from": 100, "to": 0}],
   "offscreen": [{"start": 0, "end": 0.125}],
   "summary": "Logo flies in from the left 0–1s, holds 1–4s, fades out 4–5s."}],
 "overlaps": [{"a": 3, "b": 4, "aName": "A", "bName": "B", "start": 0.75, "end": 1.29, "maxFraction": 1}]}
```

- **Range and sampling.** Default the whole comp (0 to its duration, not the work area), one
  sample per frame, or coarser for long comps so a layer gets at most 600 samples. Each layer is
  also sampled at its in and out points and at every Transform keyframe of it and its parents
  inside the range, so segment boundaries land on keys. Layers × samples is capped at 20 000:
  past that the range is cut short, `end` says where it stopped and `truncated` is true (with
  `notes`). Times are comp seconds.
- **Segments.** `move`: the content centre travels at least 0.5% of the comp diagonal; `from` /
  `to` are centres in comp pixels and `direction` the 8-way direction (`left`, `up-right`…); a
  turn of more than 50° starts a new segment. `enter` / `exit`: a move during which the share of
  the layer inside the frame rises / falls by at least 25%, with the frame `edge` it comes from /
  leaves by; a move that does both (flies across) is split into enter, move and exit where it is
  most on screen. A layer that just appears on screen at its in point is not an enter.
  `scale` (average X/Y scale %), `rotate` (degrees, positive is clockwise), `fade-in` /
  `fade-out` (opacity 0–100) are the layer's own Transform values changing by at least 1. `hold`:
  nothing of the above for at least 0.25 s (and three samples); smaller jitter counts as holding.
  Kinds overlap in time when a layer moves and fades at once.
- **`offscreen`**: while the layer is on (in to out point), intervals where none of its box is
  inside the frame. **`overlaps`**: pairs of drawn layers whose on-screen boxes overlap by more
  than 5% of the smaller one, sampled on the grid, merged into intervals with the largest
  fraction; layers covering the whole frame (backgrounds) are left out.
- Cameras, lights, nulls, hidden (Video off) and content-less layers are listed with no segments
  and a `summary` saying why.

### Reuse a library comp (recipes)

A finished comp from a Motion Library can be inserted whole (`library.insert`), but an agent asked
to *reuse* it (new colours, another text, a different size) needs to know how it is made. The
`.ecproj` holds every default (43 KB for three shape layers); a **recipe** holds only what was
authored, in a form that reads well and rebuilds the comp exactly:

1. `command_run comp.recipe {"comp": "Main"}` (or `library.get` → `file.importProject` → the
   comp; `path` writes the recipe to a file, `layers` picks some layers, `includeDefaults`
   lists every property).
2. Read it, change what the brief asks for (values, text, colours, timing, layers).
3. `command_run comp.fromRecipe {"recipe": {…}}` (or `{"path": "r.json"}`, `name?`) builds a new
   comp, its precomps first, as one undo step, and opens it. Check it with `render_preview` or
   `render_compare` against the original.

```json
{"format": "effectcraft-recipe", "version": 1,
 "comp": {"name": "Main", "width": 1920, "height": 1080, "frameRate": 29.97, "duration": 2.002},
 "layers": [
  {"name": "Circle", "type": "shape",
   "contents": [{"kind": "group", "name": "Ellipse 1", "contents": [{"kind": "ellipse"}, {"kind": "stroke"}, {"kind": "gfill"}]}],
   "styles": ["innerShadow"],
   "props": {
    "contents/group/contents/ellipse/size": [600, 600],
    "contents/group/contents/stroke/width": 0.7,
    "contents/group/contents/gfill/type": "Radial",
    "contents/group/contents/gfill/colors": {"colors": [[0, [0.6235, 0.9098, 1, 1]], [1, [0.102, 0.1216, 0.4196, 1]]], "opacities": [[0, 1], [1, 1]]},
    "layerStyles/innerShadow/size": 70,
    "transform/position": [610, 540],
    "transform/scale": {"keys": [{"time": 0, "value": [0, 0], "in": "easy", "out": "easy"},
                                 {"time": 0.2669, "value": [115, 115], "in": "easy", "out": "easy"}]}}}]}
```

- **Layers** are listed top first. `type` is `shape`, `solid` (`color`, `size`), `text`
  (`text`: what differs from a new text layer's Source Text, or just a string), `null`, `camera`,
  `light` (`light`: Spot…), `primitive`, `footage` / `model` (`file`, `footage`: the item's
  interpretation) or `precomp` (`comp`: a name from `precomps`, the nested comps' own recipes).
  Timing (`start`, `in`, `out`, seconds), `parent` and `trackMatte` (`{layer, kind}`, layers by
  name or `"#n"`, the n-th layer of the recipe), `blendMode`, `switches` and other layer
  settings appear only when they differ from a new layer's.
- **Structure** comes before values, in order: `contents` (shape items by kind: `group` with
  its own `contents`, `rect`, `ellipse`, `star`, `polygon`, `path`, `fill`, `stroke`, `gfill`,
  `gstroke`, `trim`, `repeater`, `round`, `offset`, `pucker`, `twist`, `zigzag`, `wiggle`, `merge`),
  `effects` (`{effect: "ec.blur.gaussian"}`, then `effects/#1/…` paths), `masks` (`{mode?,
  inverted?…}`), `styles` (layer style ids) and text `animators` (`{props: [position, opacity…],
  selectors?}`). `name` / `enabled` appear when they aren't the default.
- **`props`** holds only the properties whose value differs from a freshly made item of the same
  kind, by path (the paths of `effectcraft-cli props` / `get_layer {"flat":true}`). A value is a
  number, an array (2D layers leave out a default z; colours leave out alpha 1), a popup's option
  name (`"Radial"`), a gradient, a path or a layer reference. An animated property is `{keys:
  [{time, value, in?, out?}]}` with layer-time seconds and `in` / `out` easing `easy`, `hold`,
  `bezier` or `[[speed, influence]…]` (linear when absent); an expression adds `expr`. `groups`
  holds renamed or switched-off property groups by path.
- Numbers are rounded to 4 decimals; times are exact (a time that lands on a frame is written to
  4 decimals and read back as that frame). A rebuilt comp renders like the original within one
  8-bit level, and `comp.recipe` of it is the same recipe.
- What the menus' factories can't build (a stroke with extra dashes, a plug-in effect that isn't
  installed, a tracker…) is a `raw` entry: the property group as stored. What can't come back at
  all is listed in `unsupported` (`{comp, layer?, what, why}`): a footage file that isn't on this
  machine (the layer comes back offline), a footage proxy, Essential Graphics and Essential
  Properties overrides (they point at property uids), parents, mattes and layer parameters that
  point at layers left out with `layers`.
- Mistakes in a hand-written recipe are errors that name the layer and path and change nothing.

### Pitfalls

Things that tripped up a long agent-driven session (a multi-scene 3D piece built entirely over MCP).

- **Layer time vs comp time.** `add_keyframe` and `set_property` with `time` take layer time;
  `get_property` and `render_preview` take comp time. Those tools and the engine keyframe commands
  default to layer time; pass `timeBase: "comp"` to supply comp seconds on offset or
  stretched layers. `run_script`'s `setValueAtTime` and `keyTime` also use comp time. Without an
  explicit engine time base, subtract the layer's start time before keying an unstretched layer
  ([#257](https://github.com/storytold/effectcraft/issues/257)).
- **Moving a layer in time.** `command_run layer.timing {"layers":["Scene 2"],"start":5.1}` sets
  the start time in comp seconds and moves the in and out points with it.
- **Loops in scripts.** `run_script` can call any engine command with `app.run(id, params)`, for
  example `app.run("layer.new3dPrimitive", {kind: "cube", position: [x, y, z]})` inside a loop. A
  script that throws keeps the edits it already made: wrap it in `app.beginUndoGroup()` /
  `app.endUndoGroup()` so one `undo {}` removes them all. (`batch` rolls back on its own.) A script
  stopped by its `timeout` or a cancel rolls back an undo group it left open.
- **Save early in headless mode.** Without `--autosave` the headless server keeps the project in
  memory only, so a server restart loses unsaved work: start it with `mcp --autosave` (see above)
  or call `save_project {"path": …}` after each milestone.
- **Environment light.** `layer.newLight` also accepts `"kind":"Environment"`: an equirectangular
  image that metallic surfaces reflect. Set its `lightOptions/source` to a footage, comp or solid
  layer by layer id; with no source it uses the comp's Environment Layer (`layer.environment`)
  ([#261](https://github.com/storytold/effectcraft/issues/261)).

### Cookbook (from end-to-end QA)

The scenarios in `crates/automation/tests/qa/` and `apps/effectcraft-cli/tests/qa_template.rs`
build real projects through these interfaces; they are worked examples of everything below.

* **Name things when you create them**: `layer.newText`, `layer.newShape`, `layer.newSolid`,
  `layer.newNull`, `layer.newCamera` and `layer.newLight` take `name`, so later calls can say
  `"layer": "Title"`. A reference that matches no layer is an error, never a silent no-op.
* **Nested comps**: `comp.settings {preserveFrameRate, preserveResolution}` (Composition Settings ▸
  Advanced); `comp.info` / `get_comp` report both. A precomp layer shows nothing outside its
  nested comp's span.
* **Viewer Resolution** belongs to each comp and is saved with the project, as in After Effects:
  `view.res.full|half|third|quarter|auto {comp?}` or `view.res.custom {factor: 1..40, comp?}`
  (headless too; no undo step, the project becomes modified). `comp.info` reports `resolution`
  and `resolutionFactor` (`null` for Auto).
* **Several footage items at once**: `file.newCompFromSelection {single, dimensionsFrom, duration,
  sequence, overlap, overlapDuration, transition, addToRenderQueue}` is one undo step (select the
  items first: `project.select`).
* **Where an item is used**: `project.usedIn {item?}` → `{item, count, comps: [{id, name, layers}]}`
  counts the layers, in every comp, whose source the item is (direct uses only, as the Project
  panel's "Name ▾, used 3 times" header; its ▾ lists the comps and opens the one picked).
* **Locked layers** are left alone: `edit.clear`, `layer.arrange`, `layer.transform` and
  `layer.centerAnchor` skip them and fail when every target is locked ("… is locked"); Select
  All and `layer.selectNext` / `selectPrevious` step over them. Unlock with
  `layer.setSwitch {"layers": [...], "switch": "lock", "value": false}` or `layer.unlockAll`.
* **Solid Settings**: `layer.settings {layer, name?, color?, width?, height?, pixelAspect?,
  affectAll?}` edits a solid or adjustment layer's solid (and renames the layer and the solid).
  A solid shared by several layers (duplicates share theirs) changes for all of them unless
  `affectAll: false`, which gives this layer its own copy; the reply names it (`{layer, solid}`).
  Cameras and lights take their own settings through the same command.
* **Use the paths replies give you**: `effect.apply` returns `{effects: [uid], paths:
  ["effects/#2"]}`; `layer.addShapeItem {kind, group?: uid|path}` returns `{uid, path}` (e.g.
  `contents/group/contents/gfill`), ready for `set_property`. `@uid` works as a whole path.
* **One call, one undo step**: `batch {"steps":[{"command":"layer.newShape","params":{"kind":"ellipse","name":"Ring"}},{"command":"layer.addShapeItem","params":{"layer":"$1.layer","kind":"trim"}},{"command":"prop.addKey","params":{"layer":"$1.layer","path":"transform/opacity","time":0,"value":0}}]}`
  (a string param that is exactly `"$N"` or `"$N.key"` is replaced; `$2.path` is the trim's path).
* **Expressions**: `set_property`/`get_property` replies carry `evaluated` (what renders) next to
  `value` (the keyframed value) and `expressionError` when the expression fails. An expression with
  a syntax error is kept but disabled, so it renders `value` and reports the syntax error.
* **Keyframes like a person**: `keys.select {keys, toggle: true}` (Shift+click), `keys.selectEqual`
  / `selectPrevious` / `selectFollowing`, `keys.move {delta, merge}` (steps sharing a merge key are
  one drag: each applies to the keys as they were before it, so a key passed over survives) and
  `keys.transform {…, merge, fromStart: true}` (values are the whole transform since the drag
  started). **Quick Offset** (Ctrl/Cmd+Alt drag in the Timeline): `layer.quickOffset {delta,
  layers?}` and `keys.quickOffset {delta}` stagger the selected layers, or each layer's selected
  keys together, in selection order: the first stays, the last moves by `delta` (comp seconds) and
  the ones between by equal steps, onto subframes; `merge` + `fromStart: true` make `delta` the whole
  drag. Keys of locked layers can't be selected. Copy / paste: `keys.copy`, then `keys.paste`
  at the current time (keys copied from several layers go to as many selected layers in order).
  Whole effects: `effect.copy`, then `effect.paste {layers}` places the earliest copied effect
  key at the destination comp's current time, preserving the offsets between all copied
  effects and parameters, including on offset or time-stretched layers.
  `time.nextKey` / `time.previousKey` / `keys.selectAll` take `visible: [{layer, prop}]` to act
  only on those properties (the Timeline passes its revealed ones, as J / K and Ctrl+Alt+A do).
* **Easing with Keyframe Velocity**: select keys by path, `keys.select {"keys":[{"layer":"Ring","path":"contents/trim/end","time":0}]}`,
  then `keys.velocity {"outSpeed":0,"outInfluence":33.33}`.
* **Gradients**: `set_property {"path":"contents/gfill/colors","value":["#0080ff","#ffff00"]}` or
  `{"colors":[[0,"#0080ff"],[1,[1,1,0]]],"opacities":[[0,1],[1,0.5]]}`.
* **Image sequences**: `file.import {"paths":["/r/shot_0001.png"]}` imports the file's whole
  numbered run as one item (several picked frames: that range; a folder: each run in it;
  `"sequence": false`: one still; dropped files, `"drag": true`, import as stills unless
  `"sequence": true`, as in After Effects; `"alphabetical": true`: Force Alphabetical Order, the files
  play one after another) at `frameRate` or 30 fps (Settings ▸ Import). Missing frames show
  colour bars, as in After Effects. `file.interpretFootage {"items":[id], "frameRate":12,
  "startTimecode":1001}` conforms them and sets Override Start (the item and layers that ran to its end get the new
  length). See [footage.md](footage.md).
* **Colour of footage**: `file.interpretFootage {"items":[id], "preserveRgb":true}` is Interpret
  Footage ▸ Preserve RGB: the file's values unconverted (float EXRs stay linear), for a
  per-layer OCIO conversion or data passes.
* **OCIO effects**: with Configuration ▸ Custom, `configFile` is a `.ocio` path or the config's
  text; `effect.warning {layer, effect}` says what Effect Controls warns about (a config it
  can't read, a colour space it lacks, transforms that pass colours through) or `null`.
* **Multi-layer OpenEXR**: the footage shows the beauty pass (an unnamed RGBA layer, else
  Combined / Beauty / Image / RGBA…, never Cryptomatte or depth). `layer.channels {layer}`
  lists a footage layer's EXR layers and
  channels (`{"layers":[{"name":"diffuse","rgba":["diffuse.R","diffuse.G","diffuse.B",""]}]}`);
  EXtractoR shows them when its `red` / `green` / `blue` / `alpha` are set to those names
  (`prop.set {"path":"effects/#1/red","value":"depth.Z"}`). Effect Controls offers them as popups.
* **Masks**: `layer.addMask {layer, shape: rect|ellipse, rect}` first, then `layer.mask.set
  {field: feather|expansion|opacity, value}`.
* **Renders**: `renderQueue.add {comp, format: h264|prores|webm|png…, channels: rgba,
  proresProfile: 4444, output}` then `renderQueue.render {"wait": true}`. Check a render by
  importing it (`file.import`, `file.newCompFromSelection`) and `render_preview {transparent: true}`
  for its alpha (ProRes 4444 and WebM VP9 alpha both read back as straight alpha).
* **Essential Graphics**: controls can be addressed by name (`essential.set {"layer":"#1",
  "control":"Title","value":"John Smith"}`); a command that targets an explicit `comp` runs even
  when the active comp would disable it (`essential.exportTemplate {"comp":"Lower Third", …}`).
  The CLI also renders an exported template directly:
  `effectcraft-cli render --template lt.ectemplate --values values.json --out 'title_[#####].png'`.
  `values.json` is an object such as `{"Title":"John Smith","Bar Color":"#00c080","Bar Opacity":50}`;
  names follow `essential.set`, including its case-insensitive matching. Values override one
  imported instance; the template file and its source controls keep their defaults. Omit
  `--values` to render those defaults, or use the same options with `render-frame` for a PNG.
  Values must fit in 1 MiB. Invalid JSON, unknown controls and invalid values fail before rendering.
  Template mode cannot be combined with `--project`, `--demo`, `--bridge`, `--queue` or `--comp`.
* **History**: Levels of Undo defaults to 32 (`prefs.set {"key":"general.undoLevels","value":99}`);
  `history` lists branches and `history {"goto": id}` jumps between them. A command that changes
  nothing (a value set to what it already is) records no undo step. `get_state` reports `dirty`
  (unsaved changes); it follows undo, so undoing back to the saved state clears it.
* **Unsaved changes**: commands run through MCP / `engine.execute` never ask to save. In the
  desktop app, `ui.menu.invoke` of a command that closes a modified project (`file.open`,
  `file.newProject`, `file.closeProject`, `file.revert`…) and the window's close button show the
  Save / Don't Save / Cancel prompt (`ui_click` on `dialog.unsaved.save` / `dontSave` / `cancel`;
  `dialog.unsaved.revert` for Revert); the control channel's `app.quit {"force": true}` skips it.

### Effect positions

Effect points, Puppet pins (`puppet.addPin`, `puppet.recordPin`), Paint, Liquify and Roto Brush
strokes and `effect.pickColor` are in **effect space**: layer pixels from the top-left of the
layer's effect bounds. For footage, solids and precomps that is the layer's own pixel grid. Shape
and text layers have comp-sized bounds centred on their content's origin, so in a `w` × `h` comp
the point `[w / 2, h / 2]` is that origin (a new effect's default point). `prop.get` / `prop.set`
read and write these values as they are.

### Figma designs

`file.importFigma` brings Figma frames and layers in as native, editable layers. Frames become
comps or precomps, groups become nulls with their children parented, shapes become shape layers
with editable paths, fills, gradients and strokes. Text becomes text layers with style runs, masks
become track mattes, and shadows become layer styles. Image fills and node types without a native
counterpart come in as PDF footage (sharp at any scale, transparent; PNG when a PDF can't be
used). It is one undo step.
[`integrations/figma/README.md`](../integrations/figma/README.md) has the full mapping.

#### Figma without a token: the Figma MCP route

Agents with the Figma MCP (signed in once, in Claude) need no token: its `use_figma` tool runs
Figma Plugin API code inside the file, and [`integrations/figma/serialize.js`](../integrations/figma/serialize.js),
the same serializer the plug-in uses, turns nodes into the payload `file.importFigma` reads.
The Claude Code skill [`integrations/figma/skill/SKILL.md`](../integrations/figma/skill/SKILL.md)
(installed at `~/.claude/skills/figma-to-effectcraft/`) does all of it when you say "import
<figma link>":

1. Parse the link: `figma.com/design/<fileKey>/…?node-id=12-345` → `fileKey`, node `12:345`.
2. `use_figma {fileKey, code: <serialize.js> + "return await effectcraftSerialize({ids: ['12:345'], fileKey: '<fileKey>', images: 'none', budget: 18000});"}`
   and write the reply as returned to `part-1.json`. `use_figma` replies are capped at 20 KB, so
   with `budget` the serializer cuts subtrees that don't fit into stubs `{"$ref": "<id>"}` and
   lists them in `pending`.
3. While `pending` isn't empty: the same call with `{subtree: [<pending ids>], …}` returns those
   nodes as `fragments` (and what is still `pending`); write each reply to the next part file.
4. Images: every part's `rasterize` lists the nodes that come in as images (image fills, node types
   without a counterpart, subtrees too large to transfer). Fetch each with the MCP's
   `download_assets {fileKey, nodeId, defaultFormat: "pdf"}`, save the URL with `curl`, and write
   `{"images": {"<id>": {"pdfPath": "<pdf>", "bounds": "render"}}}` as one more part. PDF is the
   default: EffectCraft imports it as vector footage with Continuously Rasterize on, so it stays
   sharp when scaled, and it keeps transparency. Add a PNG (`defaultFormat: "png", defaultScale: 2`,
   as `"path": "<png>"`) only when the PDF can't be fetched or the import notes it failed:
   EffectCraft tries the PDF first, then the PNG, and reports the `format` it used.
5. `effectcraft-cli exec file.importFigma --bridge 9877 --params '{"payloadPaths": ["part-1.json", …, "images.json"]}'`
   (the first file is the payload; the others add `fragments` and `images`).
6. Report `native` vs `images`, `rasterized` (with `format`) and `notes` from the reply.

`bounds: "render"` places the image over the node's render bounds (what `download_assets` and
`get_screenshot` export, shadows included); the default is the node's bounding box (what
`exportAsync` with `useAbsoluteBounds` and the REST API render). Instance sub-layer ids
(`I1:2;3:4`) are refused by `download_assets`; render those with `{imagesOnly: [ids], images:
'png'}` through `use_figma` instead. Requests on the control channel time out after 60 s, so split
very large frames by child.

#### Other sources

* **From a link with a token** (optional): `{"url": "https://www.figma.com/design/KEY/Name?node-id=12-345"}`, or
  `{"fileKey": "KEY", "nodeIds": ["12:345"]}`. It fetches `GET /v1/files/:key/nodes?geometry=paths`
  and `GET /v1/images/:key` with a personal access token. The token comes from `token`,
  `FIGMA_TOKEN`, or one stored with `figma.setToken {token}` (`figma.status` says which is in use).
  The token is never journaled or saved in projects, and it is sent only to `api.figma.com`. The
  requests go through the system `curl`. Requests on the control channel time out after 60 s, so
  split very large selections.
* **From JSON you already have** (the Figma REST reply, the EffectCraft plug-in's payload, a node
  or an array of nodes): `{"payload": …}` or `{"payloadPath": "nodes.json"}`. Images go in
  `"images": {"<node id>": {"pdf" | "png": "<base64>"} | {"pdfPath" | "path": "<file>", "bounds"?: "render" | {x, y, width, height}, "fallback"?: …}}`.
  Large selections come in parts: `{"payloadPaths": ["part-1.json", …]}`.
* `target`: `newComp` (frame size → comp size), `activeComp` (centred in the open comp), or `auto`
  (frames → new comps, anything else → the active comp). Use `frameRate` and `duration` for new comps.
* `dryRun: true` returns the plan and changes nothing.
* The reply has `{comp, comps, precomps, layers, native, images, rasterized: [{node, name, reason}],
  notes}`. Each layer's comment holds its Figma node id (`Figma 12:345`).
* People use the EffectCraft Transfer plug-in instead: the desktop app runs its bridge on
  `127.0.0.1:7789` (Settings ▸ Let Figma Send Layers, on by default) and asks once to allow the
  plug-in (`ui.figmaPair`, automation ids `figmaPair.allow` / `figmaPair.deny`). Its imports run
  the same command. With the app's bridge off, `effectcraft-cli figma-bridge` (`--port`, `--control`,
  `--code`) serves the plug-in standalone and forwards to the app's control channel.

### Motion tracking

Trackers live on the tracked layer (`Motion Trackers ▸ Tracker n ▸ Track Point n`) and are driven
with `track.*` commands. `track.analyze` runs in the background in the app (poll `track.status`);
pass `"wait": true` to block until it finishes, which is what the CLI and headless MCP want.

1. `command_run {"id":"track.motion","params":{"layer":"clip.mov"}}` creates a Transform
   tracker (Motion Target: the layer above). Use `track.stabilize` for Stabilize, or `track.new` with
   `kind` `transform|stabilize|affine|perspective|raw` and `rotation`/`scale`.
2. `command_run {"id":"track.setPoint","params":{"point":1,"center":[812,440],"featureSize":[40,40],"searchSize":[96,96]}}`
3. `command_run {"id":"track.analyze","params":{"direction":"forward","wait":true}}` keys Feature
   Center, Confidence and Attach Point on every frame from the current time to the layer's end.
4. `command_run {"id":"track.apply","params":{"dimensions":"xy"}}` keys the target's Position
   (and Rotation/Scale), the layer's own Anchor Point and Position for Stabilize, or a Corner Pin
   effect for `affine`/`perspective` tracks. `track.options` sets the channel, blur/enhance,
   adapt-feature and "If Confidence is Below" behaviour; `track.status` reports the track points.

```sh
effectcraft-cli run clip.ecproj track.motion '{"layer":"#2"}' \
  track.setPoint '{"point":1,"center":[812,440]}' \
  track.analyze '{"wait":true}' track.apply '{}' --save
```

### Mask tracking and Mask Interpolation

`track.mask` follows the pixels inside a mask and keys its Mask Path on every frame (vertices and
tangents move with the fitted motion). `method` is `position`, `positionScale`,
`positionScaleRotation` (the default, also kept as the Tracker panel's Method), `positionScaleRotationSkew`,
`perspective`, `faceOutline` (Face Tracking (Outline Only): the mask, drawn around a face, becomes
the face outline on every frame) or `faceDetailed` (Face Tracking (Detailed Features): also keys a
Face Track Points effect with one point per landmark — eyebrows, eyes, pupils, nose tip,
nostrils, mouth corners and lips, chin, jaw); `direction` is `forward|backward|frameForward|frameBackward` from the current
time (or `start` / `end` in seconds). Like `track.analyze` it runs in the background in the app
(progress in `track.status` under `mask`, `track.stop` cancels and keeps what was tracked) and is
one undo step; `"wait": true` blocks. `track.extractFaceMeasurements {layer?}` (Extract & Copy Face
Measurements) keys a Face Measurements effect from the Face Track Points (head position, scale and
orientation X/Y/Z, eye openness, eyebrow distance from eye, mouth openness, width and offset), puts
those keys on the keyframe clipboard and returns them per frame plus a tab-separated `clipboard`
text.

Face tracking's trained model works like Roto Brush's (below): `face.models` lists the built-in
tracker and MediaPipe Face Landmarker with its authors and licence; `face.model.download {id}` /
`face.model.install {path, id?}` install it (verified); `face.model.select {id:
classical|mediapipe-face}` chooses it; `face.model.remove {id}` deletes it. Downloads and loading
run in the background; `"wait": true` on download, install or select returns once the model is in
use (or with its error), which scripts need before tracking. `track.mask` with a
face method returns `faceModel`, the engine it tries first (the classical one when the model finds
no face in the mask).

```sh
effectcraft-cli run clip.ecproj face.model.download '{"id":"mediapipe-face","wait":true}' face.model.select '{"id":"mediapipe-face","wait":true}' \
  track.mask '{"layer":"#1","mask":"#1","method":"faceDetailed","direction":"forward","wait":true}' --save
```

`mask.interpolate` (Window ▸ Mask Interpolation ▸ Apply) adds in-between Mask Path keys between
each pair of selected Mask Path keys (or the existing keys at `times`, in seconds), giving both
ends the same vertex count with a matched correspondence. Options: `keyframeRate` (number or
`"auto"` = the comp rate), `keyframeFields`, `linearVertexPaths`, `bendingResistance`, `quality`,
`addVertices` (number or `false`) with `addVerticesUnit` `pixels|total|percent`, `matchingMethod`
`auto|curve|polyline`, `oneToOne`, `firstVerticesMatch`. `mask.interpolationOptions` sets the
panel's defaults.

```sh
effectcraft-cli exec track.mask '{"layer":"#2","mask":"#1","method":"perspective","wait":true}' clip.ecproj --save
effectcraft-cli exec mask.interpolate '{"layer":"#2","mask":"Mask 1","times":[0,2],"addVertices":10}' clip.ecproj --save --json
```

### Warp Stabilizer

Effect ▸ Distort ▸ Warp Stabilizer (`effect.apply {"effect":"Warp Stabilizer"}`), Animation ▸ Warp
Stabilizer VFX and the Tracker panel's button (`track.warpStabilizer`, which applies the effect and
starts analysing) stabilize a layer. `warp.analyze {layer?, effect?, wait?}` analyses every frame
between the layer's In and Out points in the background ("Analyzing in background (step 1 of 2)",
then "Stabilizing..."; `warp.status` reports `progress`, `banner`, whether the effect is
`analyzed`, and the current frame's `warp` matrix, `autoScale` and `crop`); `warp.cancel` stops it
without writing anything. The analysis is stored in the effect (saved with the project) and is
one undo step; trimming, slipping, time-remapping or replacing the layer's source clears it and
the app re-analyses automatically (headless: run `warp.analyze` again). Settings are ordinary
properties under the instance's groups, e.g. `effects/#1/stabilization/result` (0 Smooth Motion,
1 No Motion), `effects/#1/stabilization/smoothness`, `effects/#1/stabilization/method`,
`effects/#1/borders/framing` (0 Stabilize Only … 3 Stabilize, Synthesize Edges),
`effects/#1/borders/autoScale/maximumScale`, `effects/#1/advanced/showTrackPoints`. Method 3
(Subspace Warp, the default) bends a mesh per frame to the smoothed feature trajectories, so
scenes with depth stabilise where one perspective transform cannot;
`effects/#1/advanced/rollingShutterRipple` (0 Automatic Reduction, 1 Enhanced Reduction: a finer,
softer mesh) tunes it.

```sh
effectcraft-cli run shaky.ecproj track.warpStabilizer '{"layer":"#1","wait":true}' \
  prop.set '{"layer":"#1","path":"effects/#1/stabilization/result","value":1}' --save
effectcraft-cli exec warp.analyze '{"layer":"#1","wait":true}' shaky.ecproj --save --json
effectcraft-cli exec warp.status '{"layer":"#1"}' shaky.ecproj --json
```

### 3D Camera Tracker

Animation ▸ Track Camera and the Tracker panel's button (`track.camera {layer?, shotType?:
fixed|variable|specify, aov?, solveMethod?: auto|typical|flat|tripod, detailed?, lensDistortion?,
undistort?, wait?}`) apply
Effect ▸ Perspective ▸ 3D Camera Tracker (or reuse the layer's) and analyse it in the background:
"Analyzing in background (step 1 of 2)" tracks features, "Solving camera" solves.
`camera.analyze {layer?, effect?, wait?}` re-runs it, `camera.cancel` stops it without writing
anything. `camera.solveStatus` reports `progress`/`banner`, `analyzed`, `solved`, `methodUsed`,
`averageError` (pixels), `focalLength`, `horizontalAngleOfView`, the current frame's `camera`
(position, orientation, zoom), `groundPlane` and the solved `lensDistortion` (`k1`, `k2`, normalised by
half the frame diagonal) when Solve Lens Distortion (`effects/#1/advanced/lensDistortion`, also on
with Detailed Analysis) is on; `effects/#1/advanced/undistort` renders the footage undistorted so
the solved camera's 3D layers line up. `camera.points {time?}` lists the solved points
visible now with their `id`, `comp` position, `depth`, `world` position and `error`.

Select points with `camera.selectPoints {points, add?, toggle?}` (the viewer's click / Shift-click /
marquee), then the right-click menu's commands: `camera.setGroundPlane {points?}` (Set Ground Plane
and Origin), `camera.createFromSolve {kind: text|solid|null|shadowCatcher, points? | target:
{center, normal, size?}, multiple?}` (Create Text / Solid / Null / Shadow Catcher, Camera and Light,
Create Multiple …), `camera.deletePoints {points?, wait?}` (re-solves; with Auto-delete Points
Across Time the same feature's other tracks go too) and `camera.create` (the Create Camera button).
Each is one undo step; the first create adds the one-node "3D Tracker Camera" keyed on every frame.
Changing the layer's frames clears the analysis; changing `effects/#1/shotType`,
`effects/#1/horizontalAngleOfView`, `effects/#1/advanced/solveMethod` re-solves the stored tracks.

```sh
effectcraft-cli run shot.ecproj track.camera '{"layer":"#1","wait":true}' \
  camera.points '{}' --json
effectcraft-cli run shot.ecproj camera.createFromSolve '{"kind":"solid","points":[12,40,77]}' --save
```

### Roto Brush & Refine Edge

The Roto Brush tool (Alt+W cycles Roto Brush / Refine Edge) paints in the Layer panel; agents use
`roto.stroke {layer, kind: fg|bg|refine|refineErase, points: [[x, y], …], frame?, radius?}`
(effect space, layer frames). The first foreground stroke applies the Roto Brush & Refine Edge
effect and sets the base frame with a span of 20 frames each side. `roto.propagate {layer,
direction?: forward|backward|both, to?, wait?}` segments the span (in the background unless
`wait`); strokes on any other frame correct it and propagation restarts from there.
`roto.span {start?, end?}`, `roto.freeze {wait?}` / `roto.unfreeze`, `roto.clearStrokes {frame?,
kind?}`, `roto.cancel` and `roto.options {diameter?, refineDiameter?, view?: alphaBoundary|alpha|
alphaOverlay|none}` complete the set; every edit is one undo step. If trimming the layer or
shortening the comp leaves strokes outside the layer, `roto.span` returns an error without
changing the strokes: restore the timing or remove those strokes with `roto.clearStrokes` first.
`roto.status {layer, frame?,
matte?, compute?, compareTo?}` reports the base frame, span, computed and stroked frames, frozen
state, job progress and, for a frame, the matte's area, centroid, RLE matte and IoU against a
reference. Matte settings are ordinary properties (`effects/#1/rotoBrushMatte/searchRadius`,
`effects/#1/refineEdgeMatte/decontaminateEdgeColors`, …).

Trained model (Roto Brush 2.0 / 3.0):
- `roto.models` lists the built-in engine and every registered model, with its authors,
  licence, size, URL, SHA-256 and whether it is installed, selected and active.
- `roto.model.download {id}` fetches and verifies the official weights in the background;
  `roto.model.install {path, id?}` installs a file you already have.
- `roto.model.select {id: classical|mobilesam}` chooses which one Roto Brush uses. Loading runs
  in the background; `"wait": true` (also on download and install) returns once it is in use.
- `roto.model.remove {id}` deletes the installed weights.
- The `version` property (`effects/#1/version`: 0 = 1.0, 1 = 2.0, 2 = 3.0) picks between the
  classic engine (1.0) and the chosen model (2.0, 3.0).

```sh
effectcraft-cli run clip.ecproj roto.model.install '{"path":"mobile_sam.pt"}' roto.model.select '{"id":"mobilesam","wait":true}' \
  roto.stroke '{"layer":"#1","points":[[300,200],[360,230]],"radius":10}' roto.propagate '{"layer":"#1","wait":true}' --save
```

```sh
effectcraft-cli run clip.ecproj roto.stroke '{"layer":"#1","points":[[300,200],[360,230]],"radius":10}' \
  roto.stroke '{"layer":"#1","kind":"bg","points":[[40,40],[600,40]],"radius":12}' \
  roto.propagate '{"layer":"#1","wait":true}' roto.freeze '{"layer":"#1","wait":true}' --save
```

### Object Matte

The Object Matte tool (third in the Alt+W slot) works in the Composition viewer: click an object,
or drag a box around it; Shift adds another Object Matte instead of re-prompting the layer's.
Agents use `objectMatte.apply {layer, point: [x, y], box?: [x0, y0, x1, y1], frame?, track?,
before?, after?, effect?}` (layer pixels, layer frames; `effect` re-prompts that instance), one
undo step each. With a trained model installed (Settings ▸ Roto Brush) the box is the model's box
prompt. `objectMatte.propagate {layer, effect?, direction?, wait?}` tracks the matte over the
frames around the reference frame in the background (`roto.cancel` stops it; edits to the prompt
or the layer's frames queue it again), `objectMatte.freeze {wait?}` / `objectMatte.unfreeze`
store or drop the final mattes, and `objectMatte.status {layer, effect?}` reports the job's
progress, the frozen state, the tracked frames and which are computed. With the disk cache on (Settings ▸
Media & Disk Cache), computed Object Matte and Roto Brush segmentations are also kept there, keyed
by what they are made of, so reopening a project reuses them.

```sh
effectcraft-cli run clip.ecproj objectMatte.apply '{"layer":"#1","point":[320,200],"box":[250,120,400,300]}' \
  objectMatte.propagate '{"layer":"#1","wait":true}' objectMatte.freeze '{"layer":"#1","wait":true}' --save
```

### Text: styles and editing

Source Text holds character style runs and per-paragraph settings. CR (`\r`), LF (`\n`) and
CRLF (`\r\n`) each make a paragraph break; the original string and character indices are
preserved. This also applies to scripted `TextDocument` values and `sourceRectAtTime()` bounds.
`layer.setText` changes the whole layer, or only characters `range: [start, end]` (character
indices): character attributes
(`font`, `size`, `fill`, `tracking`, `kerning: metrics|optical|<1/1000 em>`, `tsume`,
`baseline: superscript|subscript`, `allCaps`…) split the text into runs; paragraph attributes
(`justify`, `indentLeft`, `indentFirst`, `spaceBefore`, `direction`, `composer`,
`hangingPunctuation`…) apply to the paragraphs the range touches. Editing mirrors the Type tool:

1. `layer.newText {"text":"", "box":[100,100,600,300], "edit":true}` makes paragraph text and
   starts editing (`position` instead of `box` makes point text; `vertical: true` vertical type).
2. `text.insert {"text":"Hello world"}` types at the caret (replacing the selection);
   `text.setSelection {"start":6,"end":11}` selects "world"; `text.moveCaret {"to":"wordLeft",
   "extend":true}` moves like the arrow keys; `text.delete {"word":true}` is Alt+Backspace.
3. `layer.setText {"range":[6,11], "size":40, "fill":"#ff5500"}` styles the selection (the
   Character panel does the same); with an empty range it sets the style the next typed text takes.
4. `edit.copy`, `edit.paste`, `edit.pasteTextMatchFormatting` and `edit.pasteTextFormattingOnly`
   work on the selected text; `text.endEdit` commits (an empty Type-tool layer is removed).

Fonts: `text.fonts` (MCP `list_fonts`) lists every family a text layer can use, bundled and installed, with its
styles, origin (`bundled`, `system`, `user`), `variable` (it has variation axes) and own-language name
(`{"query":"gothic"}` filters by name, `{"variable":true}` keeps only variable fonts, like the font
menu's Variable Fonts filter; `{"rescan":true}` first picks up fonts installed since launch). Pass a listed `family` and
one of its `styles` to `layer.setText {"font", "style"}`; `text.fontFeatures` says which OpenType
options a font draws with its own glyphs.
A tool with its own font library can hand it over without installing it: folders listed in
`EFFECTCRAFT_FONT_DIRS` (a path list, `:`-separated, `;` on Windows) are scanned like the system font folders.
On Linux and the BSDs the system font folders are `/usr/share/fonts`, `/usr/local/share/fonts`,
`~/.local/share/fonts`, `~/.fonts`, `$XDG_DATA_HOME/fonts`, `fonts` in each `$XDG_DATA_DIRS` folder,
every `<dir>` the fontconfig configuration names (`/etc/fonts/fonts.conf` with its includes,
`~/.config/fontconfig/fonts.conf`) and, in the Flatpak, the host's fonts under `/run/host`. The
Flatpak can't see other folders; grant one with
`flatpak override --user --filesystem=<dir>:ro --env=EFFECTCRAFT_FONT_DIRS=<dir> ai.storyteller.effectcraft`.

In expressions, `text.sourceText.style` / `getStyleAt(i, t)` read styles and the setters
(`setFontSize(v, start?, count?)`, `setFillColor`, `setText`, `setJustification`…) return a
styled document.

### Scripts and ScriptUI windows

Scripts that build ScriptUI windows publish them to the session; agents drive them like a user:

1. `scriptui.list` → `[{window, title, kind: dialog|palette|window|panel, script, modal, size}]`.
2. `scriptui.get {"window": id}` → the control tree (`type`, `name`, `text`, `value`, `checked`,
   `items`, `selection`, laid-out `bounds`, `handlers`…).
3. `scriptui.click {"widget": "ok"}` presses a button / toggles a checkbox / picks a radio button or
   tab; `scriptui.set {"widget": "#4", "value": "Shot_"}` types into edit text, moves a slider or
   picks a list item (index or text); `scriptui.close {"result": 2}` closes. Controls are addressed
   by id, `#id`, `properties.name` or text; `window` can be omitted when one window is open.
   `scriptui.mouse {"widget": "pad", "type": "mousedown", "x": 10, "y": 20}` sends a mouse event
   (`mousedown`, `mouseup`, `mousemove`, `mouseover`, `mouseout`, `click`; `x`/`y` relative to the
   control, `button`, `detail`, `shiftKey`…) to the script's `addEventListener` listeners. Each
   returns the handler run's `{ok, output, error}`.

A dialog's `show()` waits for the user: the `script.run` / `file.runScript` reply carries
`"waiting": true`, and the script continues (its final output arrives in the reply of the click
that closes the dialog). File ▸ Scripts: `file.scripts.list`, `file.runScript {"name": …}`,
`file.installScript` / `file.installScriptUIPanel {"path": …}`, `window.scriptPanel {"name": …}`,
`window.restoreScriptPanels` (the panels open when the app last quit). Scripts' `app.settings`:
`script.settings.get {"section": …, "key": …}` and `script.settings.save {…, "value": …}`.
Extension points and the bundled extensions: [plugins.md](plugins.md).

### Timeline: layer order, properties and shortcuts

* `layer.arrange {"layers": [...], "above": "Layer Name"}` moves layers to just above another
  one (what dragging Timeline rows does: the selected layers move together, one undo step);
  `to: front|forward|backward|back` and `index` still work.
* The reveal shortcuts are UI commands (`ui.menu.invoke` or the keys themselves): A Anchor Point,
  P Position, S Scale, R Rotation, T Opacity, M Mask Path, F Mask Feather, E Effects, L Audio
  Levels, U keyframed properties; pressed twice quickly AA Material Options, PP paint / Roto
  Brush / Puppet, SS selected properties, RR Time Remap, TT Mask Opacity, MM all mask
  properties, FF missing effects, EE Expressions, LL Waveform, UU everything changed from its
  default. Shift+key adds or removes a set; the same key again hides it; Ctrl/Cmd+` twirls the
  selected layers open or closed. With no layer selected they apply to every layer.
* `keys.toggleTransform {"layers"?: [...], "prop": "position"}` (Alt+Shift+A/P/S/R/T) adds or
  removes a keyframe at the current time on that Transform property of each layer, in one undo
  step (separated X/Y/Z Position; Orientation and X/Y/Z Rotation on 3D layers).
* Rename a layer with Enter (or double-click a name): the whole name is selected, Enter or a
  click elsewhere commits, Escape cancels; `layer.rename {layer, name}` does it directly.
* Ctrl/Cmd-click on the current-time display (Timeline or Composition panel) toggles the Time
  Display Style between Timecode and Frames: `file.projectSettings {"timeDisplay": "frames"}`.
* `prop.pickWhip {layer, prop, target: {layer, prop, dimension?}}` links a property as its
  expression pick whip does; `dimension` picks one of the target's values (`position[0]`), as a
  drop on that value field does.
* Every property row has a property pick whip in the Parent & Link column
  (`timeline.prop.<uid>.propertyPickWhip`; the expression row's is `timeline.prop.<uid>.pickWhip`).
  Dragged onto another property's name or value, in the Timeline or Effect Controls
  (`effectControls.prop.<uid>.value[.<d>]`), it runs `prop.pickWhip`: the reference replaces the
  property's expression, or is added as one, and the expression is enabled.
* A property's menu (right-click its name; entries `timeline.prop.<uid>.menu.<entry>`): Reset
  (`prop.reset`: no expression, and the property's default value, an effect parameter's or a
  Transform property's; an animated property keeps its keyframes and gets the default as a
  keyframe at the current time), Edit Value… (`prop.set {layer, prop}` without `value`
  opens the value dialog), Separate Dimensions (Position: `prop.separateDimensions {layer,
  value}`), Add / Remove Expression (`prop.setExpression {layer, prop}`) and Add Property to
  Essential Graphics (`essential.addProperty`).

### History, puppet recording, plug-ins

* `edit.history.list` lists every undo state as a tree (undoing then editing keeps the undone
  states as a branch); `edit.history.goto {"index": n}` (or `id`, or `steps`) jumps to any of
  them. MCP: the `history` tool.
* `puppet.recordPin {"layer": "#1", "pin": "Puppet Pin 1", "samples": [[t, x, y]…]}` records a
  drag (t = seconds since it began, effect space) into Position keys at the comp frame rate from the
  current time; `puppet.recordOptions {speed, smoothing, useDraftDeformation, showMesh}`.
* `puppet.selectPins {"layer": "#1", "pins": ["Puppet Pin 1"], "add"?, "toggle"?}` selects pins
  like the viewer's click / Shift-click (`pins: []` deselects); Edit ▸ Clear (Delete) then removes
  the selected pins, not their layer (`puppet.removePin {pins?}`). Advanced and Bend pins turn by
  their ring and scale by its square in the viewer (Shift: 15° / 5 % steps; automation ids
  `viewer.puppetPin.<uid>.rotate` / `.scale`), or `puppet.setPin {rotation, scale}`. A drag
  outside the art (or Alt-drag) marquee-selects pins; Edit ▸ Select All with a pin selected selects
  every pin of that kind; `puppet.recordPin {..., "pins": [...]}` records several pins moving
  together (⌘/Ctrl-drag one of several selected pins).
* Rigging pins: `paths.pointsFollowNulls {"layer": "#1", "pins"?: [...]}` gives each Position /
  Advanced pin a null that drives it (parent the nulls to each other to build limbs);
  `paths.nullsFollowPoints {..., "pins": [...]}` makes nulls that ride on pins (parent props to
  them). Without `pins` they use the selected pins, or every pin of a layer with no path.
* Follow-through (hair, cloth, tails): `puppet.follow {"leader": "Puppet Pin 1", "pins": [...],
  "delay": 0.1, "amount": 100, "cascade": true}` makes the pins trail the leader's motion (the
  k-th nearest by k × delay with `cascade`) through Position expressions; without `leader` /
  `pins` the first selected pin leads the other selected pins.
* `effect.plugins.load {"path": "x.wasm"}` / `effect.plugins.list`: WebAssembly effect plug-ins
  ([plugins.md](plugins.md)), then `effect.apply` by id like a built-in.

## CLI

Each invocation runs a headless engine with no window. It opens the demo project unless you pass
`--project F.ecproj`, a positional `*.ecproj` or `--empty`. Add `--json` for one compact JSON document
on stdout. Errors print `{"error": ...}` and exit with status 1; usage errors, such as an unknown
option, exit with status 2 before anything runs.

```sh
effectcraft-cli info --json
effectcraft-cli commands --filter keys
effectcraft-cli exec --list --schemas --json                     # every command with its params JSON Schema
effectcraft-cli exec comp.new --params '{"name":"Main","width":1280,"height":720}' --empty --save-as main.ecproj
effectcraft-cli exec layer.newSolid '{"color":"#3366ff"}' main.ecproj --save
effectcraft-cli props Main '#1' --project main.ecproj           # flat property list with paths
effectcraft-cli set Main '#1' transform/opacity 40 main.ecproj --save
effectcraft-cli set Main '#1' transform/position '[100,360]' --time 0 main.ecproj --save
effectcraft-cli get Main '#1' transform/position --time 0.5 main.ecproj --json
effectcraft-cli run main.ecproj comp.open '{"comp":"Main"}' time.set '{"time":1}' --json
effectcraft-cli render-frame main.ecproj --time 1 --max-side 640 --out f.png --json
effectcraft-cli compare golden.png f.png --diff diff.png        # PASS/FAIL + metrics; exit 1 when they differ
effectcraft-cli compare ae-ref.png f.png --min-psnr 35 --max-side 960 --json
effectcraft-cli exec layer.newNull --bridge 9877                 # same commands, against the live app
effectcraft-cli exec file.exportLottie '{"comp":"Main","path":"main.json","includeExpressions":true}' main.ecproj --json
effectcraft-cli exec file.importLottie '{"path":"anim.json"}' main.ecproj --save
```

Scripts written against After Effects' documented scripting API run with `script`:

```sh
effectcraft-cli script build.jsx --save-as main.ecproj     # empty project unless one is given
effectcraft-cli script --eval 'app.project.item(1).numLayers' main.ecproj --json
effectcraft-cli script tweak.jsx --bridge 9877              # against the live app
```

`writeLn`/`$.writeln`/`alert` output is printed, then the value of the last expression. A script
error exits with status 1 and `file:line:col: message`. Scripts may read files only in the
project's folder and may not write files or use the network unless the user turns on Preferences ▸
Scripting & Expressions ▸ Allow Scripts to Write Files and Access Network.

`file.exportLottie` returns `{path, bytes, warnings}`: the warnings list every feature Lottie
cannot express (most effects, cameras and lights, layer styles, audio, video footage…), so an
agent can check what a player will not show. A `.lottie` path writes a dotLottie archive.
`file.importLottie` returns `{comp, items, warnings}` and opens the new composition.

Premiere Pro interop goes through timeline interchange (MCP: the `command_run` tool; control channel: `engine.execute`
with the same ids):

```sh
effectcraft-cli exec file.importTimeline '{"path":"edit.xml"}' main.ecproj --save      # FCP7 XML / .fcpxml / .otio / .edl / .aaf / .omf
effectcraft-cli exec file.exportTimeline '{"comp":"Main","path":"Main.xml"}' main.ecproj --json
effectcraft-cli exec file.exportTimeline '{"comp":"Main","path":"Main.otio","prerender":"none"}' main.ecproj
```

`file.importTimeline` returns `{format, folder, comps, allComps, items, missing, warnings, supportsSourceTimeKeys}`
(`missing` lists media imported as placeholders). `file.exportTimeline` writes Final Cut Pro XML
(`.xml`, which Premiere Pro opens with File ▸ Import) unless `format` or the extension says
`fcpxml`, `otio`, `edl`, `aaf` or `omf`; `prerender` (`unsupported` by default, `all`, `none`) controls which
layers are rendered to ProRes 4444 movies next to the document. It returns `{path, format,
sequences, videoTracks, audioTracks, clips, prerendered, warnings}`. Native `.prproj` files are
not read or written.

`<comp>` is an id or name, or `-` for the active comp. `<value>` is JSON (`50`, `[960,540]`,
`"#ff0000"`) or a bare string.

## Agent exam

`evals/agent/` holds about twenty motion-design tasks (lower third, title card in the safe areas,
logo fly-in, easing, parenting, precomp, shape layer, blur and glow, masks, track matte, fixing
layout problems, retiming and renaming, a 9:16 reframe, a contrast fix) that an agent performs
through the MCP server, each scored by deterministic checks. `cargo xtask agent-exam --run` builds
each starting project, runs Claude Code headless against `effectcraft-cli mcp`, and writes
`report.json` / `report.md` (pass rate, minutes, tool calls, failures with reasons); it is meant
for a nightly cloud job. `--check P.ecproj --task ID` scores one saved project, `--list` and
`--validate` are cheap.

The checker is the engine query `exam.check {task | checks, base?}` → `{passed, score, total,
passedCount, results: [{id, desc, pass, reason, actual}]}`. A check runs one read-only query
(`comp.info`, `prop.get`, `layer.bounds`, `render.lint`, `comp.motion`…; commands that edit or
write files are refused) and tests its JSON result with JSON Pointers and ops (`eq`, `approx`,
`contains`, `any`, …), or uses a shorthand (`prop`, `layerCount`, `comp`, `lint`, `motion`,
`compare`). Agents can run it on their own work too. Format: [evals/agent/README.md](../evals/agent/README.md).

## Repro scripts

Every command the app runs (menus, panels, shortcuts, agents, scripts) goes into the session's
command journal. A **repro script** is that journal as JSON lines, so a bug or crash becomes
something an agent can replay and a regression test becomes a data file.

```text
// comments: lines starting with // or #; blank lines are skipped
{"start": "new" | "demo" | {"project": "rel/or/abs.ecproj"}, "allow_errors": false}   optional, first line
{"command": "comp.new", "params": {"name": "Main", "frameRate": 30}}
{"command": "comp.settings", "params": {"frameRate": 0}, "expect_error": "frameRate"}
{"expect": {"query": "comp.info", "pointer": "/frameRate", "equals": 30}}
{"expect_no_error": true}
{"golden": {"file": "goldens/main-1s.png", "time": 1, "min_psnr": 40}}
```

- `start`: the project to begin with (default `new`). A relative `project` path is relative to the
  script. Other header keys (`version`, `app`, `os`, `crash`, `note`) are for people.
- `command`: run like the app runs it. It must succeed, unless it has `expect_error` (`true`, or a
  text the error must contain) or the header says `allow_errors` (errors are reported but
  tolerated). A panic always fails the replay.
- `expect`: run a command, normally a query (`prop.get`, `comp.info`, `layer.tree`, `project.summary`),
  and compare the value at the JSON `pointer` of its result with `equals` (numbers within 1e-6).
  Without `equals` it only checks that the query succeeds.
- `expect_no_error`: the previous command succeeded.
- `golden`: render a frame (`comp` and `time` default to the active comp and its current time;
  `max_side`, `transparent` as in `render.frame`) and compare it with the PNG `file` (relative to
  the script); it passes at `min_psnr` dB or better (default 40). `effectcraft-cli replay --bless`
  (or `cargo xtask repros --bless`) writes the frames instead, to make or update the goldens.

Where scripts come from:

- `journal.export {path?}` returns `{script, steps}` (or writes `path`): the commands since the
  project was last replaced (`file.newProject`, `file.open`, `file.openDemoProject`). Queries and
  saves are not in it; commands a script or `file.runScript` ran are covered by that one step.
- `help.exportRepro {path?}` saves the same for a bug report, by default to `Repros/` in the
  settings folder (macOS `~/Library/Application Support/EffectCraft/Repros`, next to `Logs/`).
- **Crash repros**: when the last-resort guard catches a panic in a command, the app writes
  `Repros/crash-<unix time>-<command>.jsonl` there automatically (the newest 20 are kept): the
  journal, then the command that panicked, with `"allow_errors": true` and the error in the header.
  The error the user or agent sees names the file. Headless sessions without a settings folder
  (the CLI, tests) don't write one.

Privacy: a repro holds command ids and parameters only, never project, image or media file
contents. Parameters can include file paths, layer and item names and text typed into text
layers, so read a repro before sharing it.

Replay one headless (a fully wired session, like the CLI's other subcommands):

```sh
effectcraft-cli replay crash.jsonl            # per-step ok/FAIL lines, then the first failure
effectcraft-cli replay crash.jsonl --json     # {ok, panicked, steps, first_failure, skipped}
effectcraft-cli replay crash.jsonl --keep-going
```

It exits with status 1 when a step fails, an expectation is not met or a command panics, and
stops at the first failure unless `--keep-going`. Steps that work on the user's files (`file.open`
of their project, imports) need those files; replace them with a `start` project or generated
content when you turn a report into a test. A repro only sees commands: UI state changed
without one (a hover, a half-finished drag) is not in it.

**Regression tests are data files.** A crash fix's regression test is the repro, trimmed to the
steps that matter, with an `expect_error` or `expect` line where the fix changed the outcome,
dropped into [`tests/repros/`](../tests/repros). `crates/host/tests/repros.rs` replays every
`*.jsonl` there and fails on any panic or unmet expectation
(`cargo nextest run -p effectcraft-host --test host repros::`).

## Seeing the UI

The quickest way is the offscreen UI: the `effectcraft-ui` server in `.mcp.json`
(`effectcraft-cli mcp --ui --demo`) runs the whole app without a window, so screenshots and UI input
work while you use the machine, on any Space, with the display asleep, and in parallel agents. The
loop below works the same there. To watch changes live instead, start the app with
`cargo run -p effectcraft -- --control 9877` (add `--demo` to open the demo project; without it the
app starts with an empty project) and use MCP bridge mode or the raw control channel. A good loop is: `ui_elements` to find an id, `ui_click` or `ui_drag`
to act, then `screenshot {"panel":"Timeline"}` to check the result. `render_preview` shows the
composition itself at any zoom.

## Parallel agents

Several agents can work in one repository at once, each in its own git worktree with its own
target directory. `cargo xtask agents` keeps them apart:

| Command | Does |
|---|---|
| `cargo xtask agents status` | Every worktree (branch, age, commits ahead of `main`, dirty files and a verdict: `unmerged work`, `merged but dirty`, `safe to remove`), every `target/agent-*` directory (size, age, owner or `ORPHAN`) and the active claims. Also written to `plan/STATUS.md` |
| `cargo xtask agents claim <slug> --task "…" [--paths a,b]` | Records the claim in `plan/claims/<slug>.toml`, warns when a path overlaps another agent's claim, and prints the environment to use: `CARGO_TARGET_DIR=target/agent-<slug>`, a free `EFFECTCRAFT_CONTROL_PORT` (from 9880; 9877 is left to the user's app) and `CARGO_BUILD_JOBS=4` |
| `cargo xtask agents lane <slug> --task "…" [--paths a,b] [--branch b]` | Like `claim`, in a lane: one of a few permanent worktrees (`../effectcraft-lane-<n>`, 4 by default, `EFFECTCRAFT_LANES`) with its own warm build folder (`target/lane-<n>`), switched to a new branch `agents/<slug>` from `main`, so a build only compiles what changed. Fails when every lane is busy |
| `cargo xtask agents release <slug>` | Deletes the claim when you are done. A lane must have no uncommitted changes; it goes back to `main` and its branch is deleted if merged |
| `cargo xtask agents gc [--yes]` | Lists merged, clean worktrees and orphan target directories (a dry run); `--yes` removes only those, never a dirty, locked, unmerged or claimed one, or the main checkout |

Agents that need to look at the app use the offscreen UI (`mcp --ui`), which takes no port and no
window; a desktop app started with that environment listens on the claimed port.

### Effects & Presets

The native Effects & Presets panel's options menu includes **Copy Presets Folder Path**. It copies
the folder the panel scans: `EC_PRESETS_DIR` when set, otherwise `~/Documents/EffectCraft/Presets`
using `HOME` or `USERPROFILE`. Relative paths are resolved to an absolute path. The folder may
not exist yet; this action does not create it.
Over the live UI control channel, `ui.menu.invoke {"id":"effects.copyPresetsFolderPath"}` copies
the same path and returns `{path}`. The web app reports that it has no local presets folder.
