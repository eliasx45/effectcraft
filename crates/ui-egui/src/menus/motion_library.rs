//! Motion Library panel view settings (`motionLibrary.view`): the folder and search shown, the
//! interface scale and card size, open folders, filters, popovers and details; `motionLibrary.apply`
//! (a card dropped on the Timeline or the viewer); `motionLibrary.copyName` and `motionLibrary.save` (a card's menu); `motionLibrary.folder`
//! (a collection folder's menu: new, rename, icon, move, delete); `motionLibrary.card` (a card's
//! menu: rename, describe, move, thumbnail, preview, delete, graphs); `motionLibrary.group`
//! (collection groups); `motionLibrary.importFiles` (files dropped on the panel);
//! `motionLibrary.refresh` (local library sources); `motionLibrary.curve` (Curves) and
//! `motionLibrary.tool` (Tools).

use serde_json::Value;

use super::dispatch::Handler;
use crate::EffectcraftApp;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[
        Handler::ui("motionLibrary.view", view),
        Handler::ui("motionLibrary.apply", apply),
        Handler::ui("motionLibrary.copyName", copy_name),
        Handler::ui("motionLibrary.save", save),
        Handler::ui("motionLibrary.folder", folder),
        Handler::ui("motionLibrary.saveSelection", save_selection),
        Handler::ui("motionLibrary.card", card),
        Handler::ui("motionLibrary.group", group),
        Handler::ui("motionLibrary.importFiles", import_files),
        Handler::ui("motionLibrary.refresh", refresh),
        Handler::ui("motionLibrary.curve", curve),
        Handler::ui("motionLibrary.tool", tool),
    ];
    H
}

fn view(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::set_view(&mut app.ui.motion_library, &params)
}

fn apply(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::apply_command(app, ctx, &params)
}

fn copy_name(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    let id = params.get("id").and_then(Value::as_str).ok_or("missing `id` (a Motion Library card id)")?;
    let name = crate::panels::motion_library::card_name(app, ctx, id).ok_or(format!("no Motion Library card `{id}`"))?;
    ctx.copy_text(name.clone());
    Ok(serde_json::json!({"copied": name}))
}

fn folder(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::folder_command(app, ctx, &params)
}

fn save_selection(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::save_selection_command(app, ctx, &params)
}

fn save(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::start_save(app, &params)
}

fn card(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::card_command(app, ctx, &params)
}

fn group(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::group_command(app, ctx, &params)
}

fn import_files(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::import_files_command(app, ctx, &params)
}

fn refresh(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library::refresh_command(app, ctx, &params)
}

fn curve(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library_curves::command(app, ctx, &params)
}

fn tool(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, params: Value) -> Result<Value, String> {
    crate::panels::motion_library_tools::command(app, ctx, &params)
}
