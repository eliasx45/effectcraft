//! Frontend halves of the speech and script commands (batch 1 lane B): `speech.transcribe` runs the
//! transcriber in the background ([`crate::transcribe`]), `script.copyRef` puts the reference on the
//! clipboard, `script.board` sends the storyboard to FigCraft ([`crate::figcraft_board`]).

use serde_json::{Value, json};

use super::dispatch::Handler;
use crate::EffectcraftApp;

// tests_dispatch.rs pins these ids (PINNED_FRONTEND).
pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] =
        &[Handler::frontend("speech.transcribe", transcribe), Handler::frontend("script.copyRef", copy_ref), Handler::frontend("script.board", board)];
    H
}

fn transcribe(_app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    crate::transcribe::start(ctx, &p)
}

fn copy_ref(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let text = p.get("text").and_then(Value::as_str).ok_or("script.copyRef: no text")?;
    ctx.copy_text(text.to_string());
    app.toast = Some(("Copied for Claude: paste it in the chat".into(), ctx.input(|i| i.time)));
    Ok(json!({"copied": true}))
}

fn board(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let r = crate::figcraft_board::send(&p);
    let now = ctx.input(|i| i.time);
    match &r {
        Ok(_) => app.toast = Some(("Storyboard sent to FigCraft".into(), now)),
        Err(e) => app.ui.status = e.clone(),
    }
    r
}
