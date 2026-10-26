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
| EMBED4 | Remaining Timeline features | Markers, quick row, waveforms, dialogs (as requests) and the eyedropper through the host; 6 minor sites left |
| EMBED5 | Keys, UI commands, inline edits in a host | `panels::timeline::hosted`: Timeline UI commands and their keys (the person's own shortcuts), frontend events, precomp redirect |
| EMBED6 | Panels in a scaled layer | `layer_pointer::Lp`: raw pointer reads, drag overlays and popup lists in the layer's own coordinates (a host may draw the Timeline in a layer it transforms, e.g. scaled on a canvas) |

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

### Calls that still need the whole app (6 sites, 6 features after EMBED4) — the blockers left

Through `app()/app_ref()`; a foreign host draws the Timeline without them.

| Feature | Call | Plan |
|---|---|---|
| ~~Graph Editor in the Timeline's place~~ | `graph::show`, `graph_tools::alt_scale_begin/update`, inline source graph | **Done (EMBED2)**: `graph.rs`/`graph_tools.rs` take `&dyn TimelineHost` |
| ~~Quick-button row / Find word box~~ | `quick_row::show` | **Done (EMBED4)**: `quick_row.rs`, the Graph/Layer wheel actions (`timeline/wheels.rs`, except the app-level `hold_keys`/`take_picks`), `text_apply.rs` and `motion_library::card_name` take the host; eases, Fast In, Motion Library pins and Find run through `TimelineHost::invoke` |
| Smart-freeze badges | `smart_freeze::timeline_badges` | takes `&Session` only in practice: change its signature |
| Why? strip height | `why_chip::height` | ctx-data only: change its signature |
| ~~Work-area context menu entries~~ | `menus::entry_for` | **Done (EMBED3)**: `TimelineHost::menu_entry` |
| ~~Layer markers, comp markers, marker bin~~ | `markers_ui::*` | **Done (EMBED4)**: drawing and gestures take the host; Marker Settings is `request_dialog(DialogRequest::Marker)` |
| "Convert To Bezier Path" label | `i18n::label` | needs only the language: pass it in |
| ~~Audio waveform row~~ | `waveform::summary` | **Done (EMBED4)**: `TimelineHost::waveform`; its default keeps a `waveform::Cache` per egui context (`waveform::summary_in`), so any host gets waveforms |
| Frozen-layer hint | `freeze_hint::show` | trait method (optional) |
| RAM chip in the footer | `memory_panel::chip` | EffectCraft-only, keep optional |
| ~~Change Renderer~~ | `dialogs::open_comp_settings` + `dialog_state` | **Done (EMBED4)**: `request_dialog(DialogRequest::CompSettings { tab })` |
| Suggestions overlay | `suggestions::timeline_overlay` | EffectCraft-only, keep optional |
| ~~Colour eyedropper~~ | `effect_controls::toggle_pick` | **Done (EMBED3)**: takes the host (it only sets `ui.fx_pick`; sampling happens in the app's viewer) |
| ~~Gradient editor~~ | `gradient_editor::open` (inline rows) | **Done (EMBED4)**: `request_dialog(DialogRequest::Gradient(params))` |

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

## Keys, UI commands and inline edits in a host (EMBED5)

The Graph Editor and the speed curves in the property rows (footer ∿ toggle, `speed_rows.rs`)
draw in any host since EMBED2; their edits (key drags, Bezier handles, `keys.setEase[Selected]`,
`keys.transform`, the transform box) are engine commands pushed as Timeline actions and run through
`TimelineHost::invoke`, with drags merged into one undo step (`merge` params, `__endMerge`). What a
host lacked was everything EffectCraft's `menus::invoke` / `run_engine` do around the engine:

| Missing in a host | Now (`panels::timeline::hosted`) |
|---|---|
| UI-only Timeline commands (`menus/timeline.rs`) | `run(host, ctx, id, params)`: Graph Editor (Shift+F3) and its F, time zoom (- / =, `;`, Shift+;), D / X scroll, B / N work area, `[` `]` and Alt+`[` `]`, the reveal keys with double presses and Shift+ adds, Alt+Shift+P/A/S/R/T, U / UU, F4, Shift+F4, Cmd+`, Source Name. `None` for any other id |
| Their keys (a host's session has no frontend bindables) | `install(session)` binds just those; `installed(session)` says whether a (replaced) session has them |
| Physical-key bindings (`Physical+=` / `Physical+-` zoom) | `keys::forward_shortcuts` looks them up first, as `menus::handle_shortcuts` does |
| The person's own shortcuts | `use_user_shortcuts(session)` reads `shortcuts.json` from EffectCraft's settings folder (`effectcraft_host::config_dir`, ≤ 8 MB) into the session's keymaps. Read only: the session has no settings store, nothing is written |
| Frontend events of engine commands (U's `timeline.revealProps`, toasts) | `handle_events(host)` after engine commands: runs `timeline.revealProps`, puts toasts in the status line, drops the rest (viewer and cache chrome) |
| Edits of a precomp expanded in the Timeline went to the parent | `inline(host, ctx, id, params)` (EffectCraft's own `inline::invoke`, now on `&mut dyn TimelineHost`): with an inline source row focused, supported commands run in its scope and come back to the host's engine dispatch, which wraps them with `engine_command(host, id, params)` as `comp.inlineEdit`; untargeted layer/property commands are refused ("needs the source composition open") instead of touching the parent |

A host's `invoke`: `hosted::inline`, then `hosted::run`, then its engine dispatch through
`hosted::engine_command`, then `hosted::handle_events`. EffectCraft's own handlers share the code
(`reveal`, `reveal_keyed`, `reveal_targets`, `reveal_props`, `twirl_selected`, Graph Editor switch
and fit); its app keeps the double-press memory in `last_reveal`, a host in egui memory (per id
scope). Not hosted: anything that opens a dialog, Free Transform, clipboard glue of `menus::invoke`
(system clipboard text/SVG pastes), tools. Graph type changes (`set_graph_type`) call
`Session::save_prefs`, a no-op for a session without a settings store.

Tests: `host::tests::{the_graph_editor_opens_and_draws_in_a_host_without_the_app,
hosted_reveal_and_zoom_keys_run_without_the_app, hosted_inline_edits_go_to_the_expanded_precomp}`
(the fake host's `invoke` is the reference host dispatch).

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
- EMBED5: `panels::timeline::hosted::{handles, install, installed, user_keymaps,
  use_user_shortcuts, run, handle_events, inline, engine_command}`; `keys::forward_shortcuts` also
  runs `Physical+` bindings. The default `TimelineHost::invoke` is unchanged (engine only).
- EMBED6: `layer_pointer::Lp` (`of`, `layer`, `mark`, `panel`, `p`, `pos`, `pair`, `vec`, `overlay`,
  `window_p`, `to_window`). A host may draw the Timeline and Effect Controls in a layer it gives a
  transform (`Context::set_transform_layer`): egui's responses already come back in the layer's
  coordinates, and every raw pointer read in those panels (hover, latest, interact, press origin,
  delta) now goes through `Lp`, as do the drag ghosts, the pick whip's line and `widgets::popup_list`
  (opened at the control's place in the window). In EffectCraft's window nothing is transformed and
  `Lp` changes nothing. New raw pointer reads in these panels must go through `Lp` too.
- EMBED4: `TimelineHost::{request_dialog, waveform, pointer_color}` (defaulted: dialogs decline
  with a status message, waveforms are computed per context, no pointer colour);
  `panels::timeline::{DialogRequest, PointerColor}` (`DialogRequest` is `#[non_exhaustive]`: new
  dialog kinds may be added, so a host's `match` needs a `_` arm);
  `panels::effect_controls::hosted_pick(host, ctx) -> bool` (call once per frame while the host
  shows the comp: an armed colour eyedropper takes `pointer_color` on its click and sets the
  property or text colour; point picks need EffectCraft's viewer and stay armed; a keyer takes the
  displayed colour rather than its input's); `panels::waveform::{summary_in, Cache: Clone}`.
  Signatures that now take `&mut dyn TimelineHost` (the app still coerces): `quick_row::show`,
  `markers_ui::{comp_markers, marker_bin, layer_markers}`, `text_apply::{run, run_follow_ups,
  preset_list}`, `motion_library::card_name`, the wheel actions in `timeline/wheels.rs`.
