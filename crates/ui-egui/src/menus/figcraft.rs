//! The FigCraft link's frontend command: `comp.sendToFigCraft` sends the `effectcraft.place`
//! params the engine worked out to FigCraft ([`crate::figcraft_link`]).

use serde_json::Value;

use super::dispatch::Handler;
use crate::EffectcraftApp;

// tests_dispatch.rs pins this id (PINNED_FRONTEND).
pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[Handler::frontend("comp.sendToFigCraft", send)];
    H
}

fn send(_app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    crate::figcraft_link::send(ctx, p)
}
