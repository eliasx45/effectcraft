//! Frontend halves of batch 1 lane C's commands: `link.paste` without files fetches the link in
//! the background ([`crate::panels::cards::start_fetch`]).

use serde_json::Value;

use super::dispatch::Handler;
use crate::EffectcraftApp;

// TODO(batch1-verify): tests_dispatch.rs pins the handled ids (PINNED_FRONTEND); add the ids below there.
pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[Handler::frontend("link.paste", link_paste)];
    H
}

fn link_paste(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    crate::panels::cards::start_fetch(app, ctx, &p)
}
