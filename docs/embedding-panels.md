# Embedding EffectCraft panels in another egui app

Goal: FigCraft (same egui 0.36.2) hosts EffectCraft's real Timeline, Graph Editor and Effect
Controls, so double-clicking a comp card opens them inside FigCraft. The panels used to take
EffectCraft's whole `EffectcraftApp`. This page lists what each panel depends on, what blocks
hosting it and how each blocker is (or will be) resolved.

| Milestone | Panel | State |
|---|---|---|
| EMBED1 | Timeline | Draws against `TimelineHost`; a fake host without the app draws it (test below) |
| EMBED2 | Graph Editor (+ id scopes, host fonts) | Draws against `TimelineHost` too; per-instance ids; `theme::install_panel_fonts` |
| EMBED3 | Effect Controls (+ shortcuts, widget ids) | Draws against `TimelineHost`; `keys::forward_shortcuts` / `menus::shortcut_command`; scroll-bar and Tab-order ids scoped |

## The host interface (EMBED1)

`crates/ui-egui/src/panels/timeline/host.rs`: `pub trait TimelineHost`, re-exported as
`panels::timeline::TimelineHost`.

- **Required:** `session()/session_mut()` (the engine `Session`), `ui()/ui_mut()` (the frontend
  `UiState`; the panel uses `ui.timeline`, `ui.focused`, `ui.status`, `ui.viewer`, `ui.fx_pick`),
  `auto()/auto_mut()` (the automation `Registry`), `tokens()` (theme `Tokens`) and `parts()` (all
  four borrowed at once, for code that holds one while it uses another).
- **Services with defaults** (a host without them gets sensible no-ops): `invoke(ctx, id, params)`
  (menu-style dispatch; default: `session.execute`), `run_engine`, `toast`, `dialog/set_dialog`,
  `playing`, `stop`, `show_panel`, `video_preview_off`, `record_scrub`, `scrub_audio`, `cache_bar`
  (green/draft/disk frames), `shown_frame_times`, `last_frame_ms`.
- **Escape hatch:** `app()/app_ref()` return `Some(EffectcraftApp)` in EffectCraft and `None`
  elsewhere. Each remaining call site that still needs the whole app goes through it, so the list
  below is greppable (`app.app()` / `app.app_ref()` in the timeline files) and a foreign host simply
  skips those features.

`EffectcraftApp` implements the trait by forwarding to exactly what the panel did before
(`menus::invoke`, `menus::run_engine`, its own `stop`, `record_scrub`, frame cache…): behaviour in
EffectCraft is unchanged. Every timeline function that took `&EffectcraftApp`/`&mut EffectcraftApp`
now takes `&dyn TimelineHost`/`&mut dyn TimelineHost` (callers passing the app coerce, so no call
site outside the panel changed).

Converted files: `panels/timeline.rs`, `timeline_columns.rs`, `timeline_words.rs`,
`timeline/{beat,inline,speed_rows}.rs`, plus `panels::project_drop_items` and
`panels::toggle_time_display`. `timeline_ruler.rs` and `timeline/snap.rs` never touched the app.

Test: `panels::timeline::host::tests::the_timeline_draws_in_a_host_without_the_app` builds a
`FakeHost` (session + `UiState::default()` + registry + tokens, nothing else), runs headless egui
frames of `timeline::show`, checks the header, search field and layer rows registered, and clicks the
Motion Blur comp switch through to the session.

## Timeline inventory (before EMBED1)

### App fields and methods

| Member | Uses | Resolution |
|---|---|---|
| `session` | ~300 | `TimelineHost::session[_mut]` |
| `ui` (`timeline`, `status`, `focused`, `viewer`, `fx_pick`) | ~310 | `TimelineHost::ui[_mut]` (narrowing to `TimelineState` + status later) |
| `auto` | ~120 | `TimelineHost::auto[_mut]` |
| `tokens` | ~25 | `TimelineHost::tokens` |
| `menus::invoke` / `menus::run_engine` (dispatch) | 50 | `TimelineHost::invoke` / `run_engine` |
| `toast` | 3 | `TimelineHost::toast` |
| `dialog` | 4 | `TimelineHost::dialog/set_dialog` |
| `playback.playing`, `stop()` | 2 | `playing` / `stop` |
| `video_preview_off()`, `show_panel()` | 2 | same-named trait methods |
| `scrub_preview`, `record_scrub()`, `scrub_audio()` | 3 | `record_scrub(now, dragged)`, `scrub_audio` |
| `frames`, `shown_series()`, `frame_opts()`, `render_source()` (cache bar) | 7 | `cache_bar(comp, frames)` |
| `viewer_shown`, `frames.times`, `frames.last_ms` (Render Time) | 3 | `shown_frame_times`, `last_frame_ms` |
| `dialog_state` | 1 | escape hatch (Change Renderer opens Composition Settings) |
| `hooks.clipboard_text` | 1 | stays on the app (`inline::enabled`, dispatch glue) |
| `timeline_layout_saved` | 3 | stays on the app (`load_layout` / `store_layout` run by the app's frame loop) |
| `last_reveal` | tests only | unchanged |

### Calls that still need the whole app (13 sites, 11 features after EMBED3) — the blockers left

Through `app()/app_ref()`; a foreign host draws the Timeline without them.

| Feature | Call | Plan |
|---|---|---|
| ~~Graph Editor in the Timeline's place~~ | `graph::show`, `graph_tools::alt_scale_begin/update`, inline source graph | **Done (EMBED2)**: `graph.rs`/`graph_tools.rs` take `&dyn TimelineHost` |
| Quick-button row / Find word box | `quick_row::show` | host trait method or move behind `TimelineHost` (it only reads session/ui) |
| Smart-freeze badges | `smart_freeze::timeline_badges` | takes `&Session` only in practice: change its signature |
| Why? strip height | `why_chip::height` | ctx-data only: change its signature |
| ~~Work-area context menu entries~~ | `menus::entry_for` | **Done (EMBED3)**: `TimelineHost::menu_entry` |
| Layer markers, comp markers, marker bin | `markers_ui::*` | convert `markers_ui.rs` (session/ui/auto + marker dialog) |
| "Convert To Bezier Path" label | `i18n::label` | needs only the language: pass it in |
| Audio waveform row | `waveform::summary` | needs the app's audio analysis cache: trait method |
| Frozen-layer hint | `freeze_hint::show` | trait method (optional) |
| RAM chip in the footer | `memory_panel::chip` | EffectCraft-only, keep optional |
| Change Renderer | `dialogs::open_comp_settings` + `dialog_state` | dialogs are app-owned: `TimelineHost::open_dialog(kind)` |
| Suggestions overlay | `suggestions::timeline_overlay` | EffectCraft-only, keep optional |
| ~~Colour eyedropper~~ | `effect_controls::toggle_pick` | **Done (EMBED3)**: takes the host (it only sets `ui.fx_pick`; sampling happens in the app's viewer) |
| Gradient editor | `gradient_editor::open` (inline rows) | dialog: `open_dialog` |

Dispatch glue that stays on `EffectcraftApp` (called by `menus::invoke`, not by the panel's draw):
`timeline::{invoke_inline, inline_enabled, show_inline_dialog}`, `inline::{invoke, enabled}`.
`inline::with_scope` is generic over the host.

### Globals and process-wide state

No `static`/`thread_local!`/`OnceLock` in the timeline files. Indirect ones:

| Global | Where | Blocks hosting? | Resolution |
|---|---|---|---|
| Named font families `semibold`, `medium` | `Tokens::semibold/medium` | **Resolved (EMBED2)** | Host calls `theme::install_panel_fonts(ctx)` once after its own `set_fonts` (adds only the `semibold`/`medium` families via `ctx.add_font`; safe before the first pass, idempotent), or `theme::add_panel_fonts(&mut defs)` on the definitions it sets. The host's `Proportional`/`Monospace` stay (the panel then draws body text in the host's font) |
| Interface language `OnceLock` | `i18n::tr` | No (one language per process; FigCraft's process gets English unless set) | Leave; later pass the language through the host |
| Font fallback state `OnceLock<Mutex>` | `theme::install_fonts` | No | Leave |
| Owner-asset cursors (disk, cached in ctx data) | `local_assets::cursor` | No (falls back to egui cursors) | Leave |
| egui context temp data: ~126 reads/writes, 43 fixed `Id::new("…")` keys (`timeline-graph-area`, `tl-tc-edit`, `wa-bar-start`, `tl-cache-bar`…) | timeline files | **Resolved (EMBED2)** | Every `egui::Id::new` in the Timeline and Graph Editor (≈175 sites) is `ids::tl_id`, salted with `TimelineHost::id_scope()` while `timeline::show` draws (`ids::IdScope`, a frame-scoped thread-local restored on drop). EffectCraft's app returns `None`: its ids are byte-identical to before. Default for other hosts: one fixed scope; give each instance its own for several Timelines. Caveats: egui's auto ids are not salted; in `widgets.rs` the scroll bar and Tab-order ids are
(EMBED3), while `popup-list-pass` and `label-swatch` stay global on purpose (an open popup anywhere
holds shortcuts; one shared swatch); ids that other (non-hosted) panels write for the Timeline (pick-whip targets from Effect Controls) or that app-level commands read outside `show` (`timeline::locate`, `begin_rename`) must run inside `IdScope::enter(host.id_scope())` in a foreign host |
| Drag-and-drop payload `panels::DragPayload` | layer/footage drops | No (foreign payloads are ignored) | A host that wants drops sends `DragPayload::Files` |
| OS-drop map `os_drop::publish_timeline` (ctx data) | file drops | No | Host reads it if it handles OS drops |

### Keyboard handling

- The panel itself reads only Escape/Enter in its text fields (6 sites) and Shift/Cmd/Alt modifiers
  for gestures: these work in any host.
- Timeline shortcuts (U/UU reveal, zoom, J/K, I/O, Cmd+arrows…) are **not** in the panel: the app's
  `menus::handle_shortcuts` maps keys to command ids. **EMBED3:** a host calls
  `panels::timeline::keys::forward_shortcuts(host, ctx, panel)` once per frame while an EffectCraft
  panel has its keyboard focus: the frame's key presses run EffectCraft's active bindings for that
  panel (Spacebar on tap release, J/K, P/S/R/T/A, Alt+Shift+P, F9, Shift+F3…; Graph Editor keys
  while it shows; Delete/Backspace = Edit ▸ Clear) through `TimelineHost::invoke`, consume them
  and return the ids run. Nothing while a text field, menu or dialog has the keyboard. Hosts that
  route keys themselves call `menus::shortcut_command(session, panel, graph_editor, key, mods)`,
  the same choice as EffectCraft's dispatch (shared `menus::pick_binding`), minus its app-only
  cases (dialogs, clipboard events, Free Transform, physical-key bindings).
- `timeline/wheels.rs` (hold-a-key radial wheels) consumes keys at the app level each frame
  (`hold_keys`, `take_picks` from `lib.rs`): it is app chrome, not part of the hosted panel, and was
  left on `EffectcraftApp`.

## Graph Editor (EMBED2, done)

Before: `graph.rs` (2.3k lines) + `graph_tools.rs`: ~165 app references, almost all `session` (79),
`ui` (82), `auto` (10), `tokens` (3), i.e. what `TimelineHost` already provides. Helpers taking the
app: `curves`, `shown_props`, `fit_all/fit_keys/fit_selection`, `navigation`, `options_menu`,
`set_graph_type`, `speed_graph`, `expression_legend`, `comparison_samples`, `text_button`, `more`,
`graph_tools::transform_box/sel_range`. No statics. Plan: same mechanical conversion to
`&mut dyn TimelineHost` (rename to `PanelHost` then), which also removes the timeline's Graph Editor
escape-hatch sites.

Now: every non-test function in `graph.rs` and `graph_tools.rs` takes `&dyn`/`&mut dyn
TimelineHost`; its command dispatch goes through `TimelineHost::invoke`. The 4 escape sites are gone
(`graph::show`, `alt_scale_begin`, `alt_scale_update`, the inline source graph), so a foreign host
gets the Graph Editor in the Timeline. `graph::shown_props` is still called with the app from
`inline::invoke` (dispatch glue). No statics.

## Effect Controls (EMBED3, done)

Before: `effect_controls.rs` (1.7k lines) + `fx_editors.rs` (1.2k) + `camera_tracker_ui.rs`: ~220 app
references: `auto` (77), `ui` (58), `session` (50), `tokens` (24) plus app-only members:
`viewer_image` / `viewer_pixels()` (histograms, Levels, eyedroppers: needs the viewer's last
frame), `pointer_comp`, `hooks` (clipboard), `raise_panel()`, `menus::menu_contents` /
`menus::invoke`. Plan: the same host plus `viewer_pixels()`, `pointer_comp()`, `raise_panel()` and
`clipboard_text()` services with `None` defaults (histogram/eyedropper editors show "no frame" in a
foreign host).

Now: `effect_controls.rs`, `fx_editors.rs` and `camera_tracker_ui.rs` take `&dyn`/`&mut dyn
TimelineHost`; `effect_controls::show` enters the host's id scope and its ≈85 ids are `tl_id`.
New trait services (additive, all defaulted): `viewer_pixels()` (Levels/Curves histograms; `None`
draws them empty), `pick_files(filters)` (`None`: "no file dialog available"), `raise_panel`,
`menu_contents(ui, menu)` (the Effect context menu; `None`: empty) and `menu_entry(ui, command,
clicked)`. Left on `EffectcraftApp` (app-level, not the panel's draw): `effect_controls::viewer_hook`
and `camera_tracker_ui::viewer_hook` (eyedropper sampling and track points in the Composition
viewer, which uses `viewer_image`/`pointer_comp`), `choose_files_after_apply` (run by `effect.apply`
dispatch). A foreign host's colour eyedropper therefore arms but cannot sample until it hosts the
viewer.

## Next steps

1. ~~EMBED2: Graph Editor, per-instance ids, host font installer.~~ Done.
2. ~~Shortcut lookup/forwarding for hosts; `widgets.rs` ids.~~ Done (EMBED3).
3. Replace the small escape sites (smart-freeze badges, why-strip height, Bezier label) with
   signatures that take `&Session`/the language.
4. ~~EMBED3: Effect Controls.~~ Done.
5. FigCraft side: depend on `effectcraft-ui-egui`, implement `TimelineHost` over FigCraft's
   EffectCraft session, and open it on comp-card double-click.

## Public API changes

All additive; nothing a host implements or calls was renamed or removed.

- EMBED1: `TimelineHost`, `TimelineParts`, `CacheBar`.
- EMBED2: `TimelineHost::id_scope` (default `Some(fixed id)`), `panels::timeline::ids::{tl_id, IdScope}`,
  `theme::{install_panel_fonts, add_panel_fonts}`.
- EMBED3: `TimelineHost::{viewer_pixels, pick_files, raise_panel, menu_contents, menu_entry}`
  (defaulted), `panels::timeline::keys::forward_shortcuts`, `menus::shortcut_command`;
  `effect_controls::show(&mut dyn TimelineHost, ui, rect)` (was `&mut EffectcraftApp`; the app still
  coerces).
