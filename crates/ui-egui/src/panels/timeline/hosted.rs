//! EMBED5: the Timeline's UI-only commands a foreign host can run.
//!
//! Most of the Timeline's keys run engine commands, which any host runs on its session. A few are
//! EffectCraft frontend commands (`menus/timeline.rs`) that only change the panel's own state:
//! the Graph Editor switch (Shift+F3, `timeline.graphEditor`) and Fit All Graphs to View (F while
//! the Graph Editor shows, `timeline.graph.fit`). In EffectCraft `menus::invoke` runs them; a host
//! without the app runs them here. [`install`] makes their keys bindable in the host's session
//! (EffectCraft registers all of `menus::UI_COMMANDS` itself), [`run`] runs them; the default
//! [`TimelineHost::invoke`] calls it, and a host that overrides `invoke` calls it first.

use effectcraft_engine::Session;
use serde_json::{Value, json};

use super::TimelineHost;
use super::ids::IdScope;

/// The UI-only commands [`run`] handles.
pub const COMMANDS: &[&str] = &["timeline.graphEditor", "timeline.graph.fit"];

/// Make [`COMMANDS`]' keyboard shortcuts bindable in `session` (a host's session has no
/// frontend commands of its own): `menus::shortcut_command` / `keys::forward_shortcuts` then
/// find Shift+F3 and the Graph Editor's F. Run again after the host replaces its session.
pub fn install(session: &mut Session) {
    let cmds = crate::menus::UI_COMMANDS
        .iter()
        .filter(|c| COMMANDS.contains(&c.id))
        .map(|c| effectcraft_engine::shortcuts::UiCommand { id: c.id.into(), label: c.label.into(), shortcut: c.shortcut.map(str::to_string) })
        .collect();
    session.set_ui_commands(cmds);
}

/// Whether `session` has [`install`]'s commands bound (cheap: the shortcut table is cached).
pub fn installed(session: &Session) -> bool {
    COMMANDS.iter().all(|id| session.shortcuts().find(id).is_some())
}

/// Runs `id` when it is one of [`COMMANDS`]; `None` for every other id (the host runs those).
pub fn run<H: TimelineHost + ?Sized>(host: &mut H, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        "timeline.graphEditor" => {
            let tl = &mut host.ui_mut().timeline;
            tl.graph_editor = params.get("value").and_then(Value::as_bool).unwrap_or(!tl.graph_editor);
            Some(Ok(json!({"graphEditor": tl.graph_editor})))
        }
        "timeline.graph.fit" => Some(graph_fit(host, ctx)),
        _ => None,
    }
}

/// Fit All Graphs to View: the Graph Editor frames the graphs when it is next drawn (in the host's
/// id scope, where the panel reads the request).
pub(crate) fn graph_fit<H: TimelineHost + ?Sized>(host: &H, ctx: &egui::Context) -> Result<Value, String> {
    if !host.ui().timeline.graph_editor {
        return Err("Fit All Graphs to View needs the Graph Editor: open it first (timeline.graphEditor, Shift+F3)".into());
    }
    let _ids = IdScope::enter(host.id_scope());
    crate::panels::graph::request_fit_all(ctx);
    Ok(json!({"fit": "all"}))
}
