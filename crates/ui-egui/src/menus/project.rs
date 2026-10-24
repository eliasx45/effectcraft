//! Project panel commands.

use serde_json::{Value, json};

use super::dispatch::Handler;
use crate::EffectcraftApp;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[
        Handler::ui("project.column", column),
        Handler::ui("project.deleteWithoutConfirmation", delete_without_confirmation),
        // Run by id (control channel, MCP): the given `items` (or `ids`), else the Project panel's
        // selection, whatever has focus.
        Handler::frontend("project.deleteWithoutConfirmation", delete_confirmed),
    ];
    H
}

/// Shift+Delete deletes the Project panel's selection without asking; it does nothing in the
/// other panels.
fn delete_without_confirmation(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    if app.ui.focused != crate::dock::PanelKind::Project {
        return Ok(Value::Null);
    }
    crate::panels::delete_items::delete_confirmed(app, ctx, json!({}))
}

/// PD1: the params go through to `project.delete`, so explicit items are what gets deleted; the
/// selection only when none are given (an agent lost a comp when its ids were dropped).
fn delete_confirmed(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    let params = if params.is_null() { json!({}) } else { params };
    crate::panels::delete_items::delete_confirmed(app, ctx, params)
}

fn column(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    use crate::panels::project::{COLUMNS, set_column};
    let col = params.get("column").and_then(Value::as_str).ok_or("project.column: need `column`")?;
    if !COLUMNS.iter().any(|c| c.0 == col) {
        return Err(format!("unknown column `{col}` (one of: {})", COLUMNS.map(|c| c.0).join(", ")));
    }
    let on = params.get("visible").and_then(Value::as_bool).unwrap_or(!app.ui.project_columns.iter().any(|c| c == col));
    set_column(&mut app.ui.project_columns, col, on);
    Ok(json!({"columns": app.ui.project_columns}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::project::ItemId;

    /// PD1 regression: Delete Without Confirmation run by id deletes the items it names, not the
    /// Project panel's selection.
    #[test]
    fn delete_without_confirmation_by_id_deletes_the_given_items_not_the_selection() {
        let ctx = egui::Context::default();
        let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
        let comp = |app: &mut EffectcraftApp, name: &str| {
            let r = app.session.execute("comp.new", json!({"name": name, "width": 16, "height": 16, "duration": 1})).unwrap();
            ItemId(r["comp"].as_u64().unwrap())
        };
        let keep = comp(&mut app, "Keep");
        let doomed = comp(&mut app, "Doomed");
        app.session.execute("project.select", json!({"items": [keep.0]})).unwrap();
        crate::menus::frontend(&mut app, &ctx, "project.deleteWithoutConfirmation", json!({"items": [doomed.0]})).unwrap();
        assert!(app.session.project.item(doomed).is_none(), "the given item is deleted");
        assert!(app.session.project.item(keep).is_some(), "the selected item is kept");
        // `ids` works too; with nothing given it falls back to the selection.
        let other = comp(&mut app, "Other");
        app.session.execute("project.select", json!({"items": [keep.0]})).unwrap();
        crate::menus::frontend(&mut app, &ctx, "project.deleteWithoutConfirmation", json!({"ids": [other.0]})).unwrap();
        assert!(app.session.project.item(other).is_none() && app.session.project.item(keep).is_some());
        crate::menus::frontend(&mut app, &ctx, "project.deleteWithoutConfirmation", json!({})).unwrap();
        assert!(app.session.project.item(keep).is_none(), "no items: the selection");
    }
}
