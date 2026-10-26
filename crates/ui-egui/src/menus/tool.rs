//! Tools panel shortcuts (`tool.<name>`): pick a tool; a slot shortcut cycles through its tools.

use serde_json::{Value, json};

use super::dispatch::Handler;
use crate::EffectcraftApp;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[Handler::ui_prefix("tool.", tool)];
    H
}

fn tool(app: &mut EffectcraftApp, _ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let tool = app.ui.choose_tool(id)?;
    Ok(json!({"tool": tool}))
}
