//! EMBED5: what a foreign host needs to run the Timeline's keys and edits like EffectCraft does.
//!
//! Most of the Timeline's keys run engine commands, which any host runs on its session. Others are
//! EffectCraft frontend commands (`menus/timeline.rs`) that only change the panel's own state or
//! wrap an engine command: the Graph Editor switch (Shift+F3) and its F (Fit All Graphs), time
//! zoom (- / =, ;, Shift+;), D / X (scroll to time / layer), B / N (work area), [ ] (layer in /
//! out to time), the reveal keys (P S R T A E M F L, their double presses and Shift+ adds),
//! Alt+Shift+P… (key at time), U / UU, F4 (switches / modes), Shift+F4 (parent column),
//! Cmd+` (twirl selected), and (EMBED8) the Composition viewer tools a host's comp has (V, W, Y,
//! Q, G, Cmd+T: `panels::viewer_hosted`). In EffectCraft `menus::invoke` runs them; a host runs
//! them here.
//!
//! A host:
//! - calls [`install`] on its session (binds these commands' keys; a host's session has no
//!   frontend commands of its own) and, to use the person's own EffectCraft shortcuts, [`use_user_shortcuts`]
//!   (reads them, never writes); again whenever it replaces its session ([`installed`]);
//! - in its `TimelineHost::invoke`, calls [`inline`] first (edits of a precomp expanded in the
//!   Timeline go to that precomp, as in EffectCraft), then [`run`], then its engine dispatch with
//!   [`engine_command`] (the precomp redirect for engine commands);
//! - after engine commands, calls [`handle_events`] (U's revealed properties, toasts).
//!
//! Dialog commands are not here: they need EffectCraft's app (`TimelineHost::request_dialog`).

use effectcraft_engine::Session;
use serde_json::{Value, json};

use super::TimelineHost;
use super::ids::{IdScope, tl_id};

/// Exact UI command ids [`run`] handles (plus the [`PREFIXES`]).
const EXACT: &[&str] = &[
    "timeline.graphEditor",
    "timeline.graph.fit",
    "timeline.zoomIn",
    "timeline.zoomOut",
    "timeline.zoomFit",
    "timeline.zoomFrameToggle",
    "timeline.scrollToTime",
    "timeline.scrollToLayer",
    "timeline.parentColumn",
    "timeline.switchesModes",
    "timeline.collapseAll",
    "timeline.twirlSelected",
    "timeline.sourceName",
    "timeline.workAreaBegin",
    "timeline.workAreaEnd",
    "timeline.moveInToTime",
    "timeline.moveOutToTime",
    "timeline.trimInToTime",
    "timeline.trimOutToTime",
    // EMBED8: the Composition viewer tools a host's card has (`panels::viewer_hosted`).
    "tool.selection",
    "tool.rotate",
    "tool.panBehind",
    "tool.shape",
    "tool.pen",
    "tool.type",
];
const PREFIXES: &[&str] = &["timeline.reveal.", "timeline.revealAdd.", "timeline.keyAt."];

/// Whether [`run`] handles UI command `id` (`anim.reveal`, an engine command, it also takes).
pub fn handles(id: &str) -> bool {
    EXACT.contains(&id) || PREFIXES.iter().any(|p| id.starts_with(p))
}

/// The biggest `shortcuts.json` read from EffectCraft's settings folder.
const MAX_SHORTCUTS_BYTES: u64 = 8 << 20;

/// Make the handled commands' keyboard shortcuts bindable in `session`: `menus::shortcut_command`
/// and `keys::forward_shortcuts` then find them. Run again after the host replaces its session.
pub fn install(session: &mut Session) {
    let cmds = crate::menus::UI_COMMANDS
        .iter()
        .filter(|c| handles(c.id))
        .map(|c| effectcraft_engine::shortcuts::UiCommand { id: c.id.into(), label: c.label.into(), shortcut: c.shortcut.map(str::to_string) })
        .collect();
    session.set_ui_commands(cmds);
}

/// Whether `session` has [`install`]'s commands bound (cheap: the shortcut table is cached).
pub fn installed(session: &Session) -> bool {
    ["timeline.graphEditor", "timeline.reveal.position"].iter().all(|id| session.shortcuts().find(id).is_some())
}

/// The person's own EffectCraft shortcut presets (the active one included), read from
/// EffectCraft's settings folder (`shortcuts.json`); `None` without one. Read only.
pub fn user_keymaps() -> Option<effectcraft_engine::shortcuts::Keymaps> {
    // The browser has no settings folder.
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    let path = effectcraft_host::config_dir()?.join(effectcraft_engine::shortcuts::SHORTCUTS_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SHORTCUTS_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(effectcraft_engine::shortcuts::Keymaps::from_json(&text))
}

/// The person's own EffectCraft preview options (Preview panel: each shortcut's range, play from,
/// loop, frame rate, skip…), read from `prefs.json` in EffectCraft's settings folder, never
/// written; `None` when there is none (a host then uses the defaults). For a host that plays a
/// comp the way EffectCraft's Space would.
pub fn user_preview() -> Option<effectcraft_engine::preview::PreviewSettings> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    let path = effectcraft_host::config_dir()?.join(effectcraft_engine::prefs::PREFS_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PREFS_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(effectcraft_engine::prefs::Prefs::from_json(&text).preview)
}

/// The largest `prefs.json` [`user_preview`] reads.
const MAX_PREFS_BYTES: u64 = 16 << 20;

/// Use the person's own EffectCraft shortcuts ([`user_keymaps`]) in `session`; returns whether
/// there were any. Nothing is written: the session keeps them in memory only (a host's session
/// has no settings store, so `Session::shortcuts_changed` would not save either).
pub fn use_user_shortcuts(session: &mut Session) -> bool {
    let Some(maps) = user_keymaps() else { return false };
    session.keymaps = maps;
    // Rebuilds the shortcut table against the new presets (no write).
    install(session);
    true
}

/// Runs `id` when it is one of the Timeline's UI commands this module handles; `None` for every
/// other id (the host runs those).
pub fn run(host: &mut dyn TimelineHost, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if !handles(id) && id != "anim.reveal" {
        return None;
    }
    let _ids = IdScope::enter(host.id_scope());
    let now = ctx.input(|i| i.time);
    let tl_toggle = |slot: &mut bool| {
        *slot = params.get("value").and_then(Value::as_bool).unwrap_or(!*slot);
        *slot
    };
    Some(match id {
        "timeline.graphEditor" => {
            let on = tl_toggle(&mut host.ui_mut().timeline.graph_editor);
            Ok(json!({"graphEditor": on}))
        }
        "timeline.graph.fit" => graph_fit(host, ctx),
        "timeline.zoomIn" | "timeline.zoomOut" => {
            let k = if id == "timeline.zoomIn" { super::ZOOM_STEP } else { 1.0 / super::ZOOM_STEP };
            super::zoom(host, ctx, k);
            Ok(Value::Null)
        }
        "timeline.zoomFit" => {
            super::toggle_fit_zoom(host, ctx);
            Ok(Value::Null)
        }
        "timeline.zoomFrameToggle" => {
            super::toggle_frame_zoom(host, ctx);
            Ok(Value::Null)
        }
        "timeline.scrollToTime" | "timeline.scrollToLayer" => {
            host.ui_mut().timeline.scroll_to = Some(if id == "timeline.scrollToTime" { crate::state::ScrollTo::Time } else { crate::state::ScrollTo::Layer });
            Ok(Value::Null)
        }
        "timeline.parentColumn" => {
            let tl = &mut host.ui_mut().timeline;
            let on = !crate::panels::timeline_columns::column_visible(tl, "parent");
            crate::panels::timeline_columns::set_column(tl, "parent", on).map(|()| json!({"parent": on}))
        }
        "timeline.switchesModes" => {
            crate::panels::timeline_columns::toggle_switches_modes(&mut host.ui_mut().timeline);
            Ok(Value::Null)
        }
        "timeline.collapseAll" => {
            let tl = &mut host.ui_mut().timeline;
            tl.open_layers.clear();
            tl.open_groups.clear();
            tl.reveal.clear();
            tl.layer_reveal.clear();
            Ok(Value::Null)
        }
        "timeline.twirlSelected" => {
            twirl_selected(host);
            Ok(Value::Null)
        }
        "timeline.sourceName" => {
            let on = tl_toggle(&mut host.ui_mut().timeline.source_name);
            Ok(json!({"sourceName": on}))
        }
        "timeline.workAreaBegin" => engine(host, ctx, "comp.workArea", json!({"set": "begin"})),
        "timeline.workAreaEnd" => engine(host, ctx, "comp.workArea", json!({"set": "end"})),
        "timeline.moveInToTime" | "timeline.moveOutToTime" | "timeline.trimInToTime" | "timeline.trimOutToTime" => {
            let op = id.strip_prefix("timeline.").unwrap_or(id);
            engine(host, ctx, "layer.timing", json!({"op": op}))
        }
        "anim.reveal" => anim_reveal(host, ctx, now, params),
        t if t.starts_with("tool.") => host.ui_mut().choose_tool(t).map(|tool| json!({"tool": tool})),
        _ => {
            if let Some(k) = id.strip_prefix("timeline.revealAdd.") {
                remember(ctx, Some((k.to_string(), now)));
                reveal(host, k, now, true, None);
                Ok(json!({"revealed": host.ui().timeline.reveal}))
            } else if let Some(k) = id.strip_prefix("timeline.reveal.") {
                if k == "animated" {
                    engine(host, ctx, "anim.reveal", json!({"kind": "keyframes"}))
                } else {
                    let last = recall(ctx);
                    let next = reveal(host, k, now, false, last);
                    remember(ctx, next);
                    Ok(Value::Null)
                }
            } else if let Some(k) = id.strip_prefix("timeline.keyAt.") {
                engine(host, ctx, "keys.toggleTransform", json!({"prop": k})).inspect(|r| reveal_keyed(host, r, k))
            } else {
                Err(format!("{id}: not a Timeline command"))
            }
        }
    })
}

/// An engine command run for a UI command: through the host (its guards and redirects), then the
/// Timeline's frontend events it emitted.
fn engine(host: &mut dyn TimelineHost, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
    let r = host.run_engine(ctx, id, params);
    handle_events(host);
    r
}

/// The last reveal key and when it was pressed (double presses), kept per Timeline.
fn reveal_memory() -> egui::Id {
    tl_id("hosted-last-reveal")
}

fn recall(ctx: &egui::Context) -> Option<(String, f64)> {
    ctx.data(|d| d.get_temp::<(String, f64)>(reveal_memory()))
}

fn remember(ctx: &egui::Context, last: Option<(String, f64)>) {
    ctx.data_mut(|d| match last {
        Some(l) => {
            d.insert_temp(reveal_memory(), l);
        }
        None => {
            d.remove::<(String, f64)>(reveal_memory());
        }
    });
}

/// U reveals keyframed properties; UU (twice quickly) all modified properties; U again later
/// hides them (as EffectCraft's `menus/timeline.rs`).
fn anim_reveal(host: &mut dyn TimelineHost, ctx: &egui::Context, now: f64, params: &Value) -> Result<Value, String> {
    if params.get("kind").and_then(Value::as_str) == Some("keyframes") {
        let last = recall(ctx);
        remember(ctx, Some(("keyframes".into(), now)));
        match last {
            Some((k, t)) if k == "keyframes" && now - t < 0.6 => {
                return engine(host, ctx, "anim.reveal", json!({"kind": "modified"}));
            }
            Some((k, _)) if k == "keyframes" && host.ui().timeline.reveal == ["props"] => {
                let targets = reveal_targets(host);
                host.ui_mut().timeline.apply_reveal(&targets, vec![]);
                return Ok(json!({"revealed": 0}));
            }
            _ => {}
        }
    }
    engine(host, ctx, "anim.reveal", params.clone())
}

/// Fit All Graphs to View: the Graph Editor frames the graphs when it is next drawn (in the host's
/// id scope, where the panel reads the request).
pub(crate) fn graph_fit(host: &dyn TimelineHost, ctx: &egui::Context) -> Result<Value, String> {
    if !host.ui().timeline.graph_editor {
        return Err("Fit All Graphs to View needs the Graph Editor: open it first (timeline.graphEditor, Shift+F3)".into());
    }
    let _ids = IdScope::enter(host.id_scope());
    crate::panels::graph::request_fit_all(ctx);
    Ok(json!({"fit": "all"}))
}

/// Cmd+`: twirls the selected layers open or closed; on a reveal shortcut's view it opens their
/// full property trees.
pub(crate) fn twirl_selected(host: &mut dyn TimelineHost) {
    let sel: Vec<u64> = host.session().state.selected_layers.iter().map(|l| l.0).collect();
    let tl = &mut host.ui_mut().timeline;
    let revealing = sel.iter().any(|l| tl.layer_reveal.contains_key(l));
    tl.reveal.clear();
    for l in &sel {
        tl.layer_reveal.remove(l);
    }
    if !revealing && sel.iter().all(|l| tl.open_layers.contains(l)) {
        for l in &sel {
            tl.open_layers.remove(l);
        }
    } else {
        tl.open_layers.extend(sel);
    }
}

/// The reveal shortcuts (P, S, R, T, A, E, M, F, L…). `add` (Shift+key) adds the property to (or
/// removes it from) those already revealed instead of replacing them. `last`: the previous reveal
/// key and its time (double presses); returns what to remember for the next one.
pub(crate) fn reveal(host: &mut dyn TimelineHost, kind: &str, now: f64, add: bool, last: Option<(String, f64)>) -> Option<(String, f64)> {
    // The double-press shortcuts: the same key again within 0.6 s reveals its second set (LL
    // Waveform, TT Mask Opacity, MM all mask properties, EE Expressions, RR Time Remap, AA
    // Material Options, PP paint / Roto Brush / Puppet, SS selected properties, FF missing
    // effects). A single M shows Mask Path.
    let double = |k: &str| -> Option<&'static str> {
        Some(match k {
            "levels" => "waveform",
            "opacity" => "maskOpacity",
            "masks" => "masks",
            "effects" => "expressions",
            "rotation" => "timeRemap",
            "anchor" => "material",
            "position" => "paint",
            "scale" => "selected",
            "feather" => "missingEffects",
            _ => return None,
        })
    };
    let pressed = kind;
    // (With Shift a second press removes the property again instead.)
    let again = !add && matches!(last, Some((k, t)) if k == pressed && now - t < 0.6);
    let next = Some((if again { String::new() } else { pressed.to_string() }, now));
    let kind = match (again, double(pressed)) {
        (true, Some(d)) => {
            // The second press replaces what the first one revealed.
            let first = if pressed == "masks" { "maskPath" } else { pressed };
            host.ui_mut().timeline.reveal.retain(|k| k != first);
            d
        }
        _ if pressed == "masks" => "maskPath",
        _ => pressed,
    };
    let targets = reveal_targets(host);
    let tl = &mut host.ui_mut().timeline;
    if add {
        // Shift+key adds to what each layer shows now, however it got there (another reveal
        // key, U, Alt+Shift+key), as in After Effects; when every layer shows it already, it
        // is taken away.
        let on = !targets.iter().all(|id| tl.layer_reveal.get(id).is_some_and(|k| k.iter().any(|k| k == kind)));
        let mut shown = Vec::with_capacity(targets.len());
        for id in &targets {
            let mut kinds = tl.layer_reveal.get(id).cloned().unwrap_or_default();
            kinds.retain(|k| k != kind && (k != "props" || !tl.reveal_props.is_empty()));
            if on {
                kinds.push(kind.to_string());
            }
            tl.apply_reveal(&[*id], kinds.clone());
            shown.push((*id, kinds));
        }
        for (id, kinds) in shown {
            super::open_revealed(host, &[id], &kinds);
        }
        return next;
    }
    let present = tl.reveal.iter().any(|k| k == kind);
    let mut kinds = tl.reveal.clone();
    if present && kinds.len() == 1 {
        kinds.clear();
    } else {
        kinds = vec![kind.to_string()];
    }
    tl.apply_reveal(&targets, kinds.clone());
    super::open_revealed(host, &targets, &kinds);
    next
}

/// After Alt+Shift+P (A, S, R, T) the Timeline shows the property on each layer it keyed, as in
/// After Effects: added to what a reveal shortcut shows, else with the layer's Transform twirled
/// open, else alone like P.
pub(crate) fn reveal_keyed(host: &mut dyn TimelineHost, keyed: &Value, kind: &str) {
    let layers: std::collections::BTreeSet<u64> = keyed.as_array().into_iter().flatten().filter_map(|k| k.get("layer").and_then(Value::as_u64)).collect();
    let Some(comp) = host.session().active_comp_arc() else { return };
    let tl = &mut host.ui_mut().timeline;
    for id in layers {
        let shown = tl.layer_reveal.get(&id).filter(|k| !k.is_empty()).cloned();
        match shown {
            Some(mut kinds) => {
                if !kinds.iter().any(|k| k == kind) {
                    kinds.push(kind.to_string());
                    tl.layer_reveal.insert(id, kinds);
                }
            }
            None if tl.open_layers.contains(&id) => {
                if let Some(tr) = comp.layer(effectcraft_engine::project::LayerId(id)).and_then(|l| l.transform()) {
                    tl.open_groups.insert(tr.uid);
                }
            }
            None => {
                tl.open_layers.insert(id);
                tl.layer_reveal.insert(id, vec![kind.to_string()]);
            }
        }
    }
}

/// The layers a reveal shortcut acts on: the selected ones, else every layer of the active comp.
pub(crate) fn reveal_targets(host: &dyn TimelineHost) -> Vec<u64> {
    let s = host.session();
    if s.state.selected_layers.is_empty() {
        s.active_comp().map(|c| c.layers.iter().map(|l| l.id.0).collect()).unwrap_or_default()
    } else {
        s.state.selected_layers.iter().map(|l| l.0).collect()
    }
}

/// `timeline.revealProps` (the frontend event of U, UU and the reveal commands): show the
/// properties it lists.
pub(crate) fn reveal_props(host: &mut dyn TimelineHost, p: &Value) -> Value {
    let props = p.get("props").and_then(Value::as_array).cloned().unwrap_or_default();
    let uids = |k: &str| -> Vec<u64> { props.iter().filter_map(|x| x.get(k).and_then(Value::as_u64)).collect() };
    let (found, with_props) = (uids("prop"), uids("layer"));
    // The layers it looked at (`layers`), else those it found something on. Layers that
    // have nothing to show close; the others' reveals stay.
    let targets: Vec<u64> = match p.get("layers").and_then(Value::as_array) {
        Some(l) => l.iter().filter_map(Value::as_u64).collect(),
        None => with_props.clone(),
    };
    // The targets' earlier picks go (U after UU shows the keyframed ones only).
    let comp = host.session().active_comp_arc();
    let theirs = |u: u64| {
        comp.as_ref().is_some_and(|c| {
            targets
                .iter()
                .filter_map(|l| c.layer(effectcraft_engine::project::LayerId(*l)))
                .any(|l| l.props.find(u).is_some() || l.props.find_group(u).is_some())
        })
    };
    let tl = &mut host.ui_mut().timeline;
    tl.reveal_props.retain(|u| !theirs(*u));
    tl.reveal_props.extend(found.iter().copied());
    let (shown, hidden): (Vec<u64>, Vec<u64>) = targets.iter().copied().partition(|l| with_props.contains(l));
    tl.apply_reveal(&hidden, vec![]);
    if !shown.is_empty() {
        tl.apply_reveal(&shown, vec!["props".into()]);
    }
    // Properties inside effects (puppet pins…) show under their effect, twirled open.
    for (layer, prop) in props.iter().filter_map(|x| Some((x.get("layer")?.as_u64()?, x.get("prop")?.as_u64()?))) {
        super::open_effect_paths(host, layer, &[prop]);
    }
    json!({"revealed": found.len()})
}

/// For a host that doesn't read its session's events itself: runs the Timeline's frontend events
/// (`timeline.revealProps`), shows toasts in the status line and drops the rest (they are
/// EffectCraft app chrome: viewers, caches, dialogs).
pub fn handle_events(host: &mut dyn TimelineHost) {
    for ev in host.session_mut().drain_events() {
        match ev {
            effectcraft_engine::Event::Frontend { command, params } if command == "timeline.revealProps" => {
                let _ = reveal_props(host, &params);
            }
            effectcraft_engine::Event::Toast { message, .. } => host.ui_mut().status = message,
            _ => {}
        }
    }
}

/// Edits of a precomp expanded in the Timeline (an inline source row has the focus) go to that
/// precomp, as EffectCraft's `menus::invoke` sends them: commands that support it run in the
/// source's scope (and come back to [`engine_command`] as `comp.inlineEdit`), the reveal keys and
/// a shown Graph Editor's zoom act on the source, and layer/property commands that can't be
/// redirected are refused rather than applied to the parent. `None`: not an inline edit.
pub fn inline(host: &mut dyn TimelineHost, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let _ids = IdScope::enter(host.id_scope());
    super::inline::invoke(host, ctx, id, params)
}

/// The engine command to run for `id`: `comp.inlineEdit` wrapping it while [`inline`] runs it for
/// an expanded precomp, else `id` itself.
pub fn engine_command(host: &dyn TimelineHost, id: &str, params: Value) -> (String, Value) {
    match super::inline::engine_target(host, id, &params) {
        Some(nested) => ("comp.inlineEdit".to_string(), nested),
        None => (id.to_string(), params),
    }
}
