//! EMBED3: EffectCraft's keyboard shortcuts for a panel hosted in another app.
//!
//! In EffectCraft the app's frame loop maps keys to commands (`menus::handle_shortcuts`); a hosted
//! panel has no such loop. While one of EffectCraft's panels has the keyboard in the host, the
//! host calls [`forward_shortcuts`] once per frame (after drawing the panel): the frame's key
//! presses run the commands EffectCraft binds to them (Spacebar, J/K, P/S/R/T/A reveals,
//! Alt+Shift+P add key, F9 eases, Shift+F3 Graph Editor…) through [`TimelineHost::invoke`]. Hosts
//! that route keys themselves use [`crate::menus::shortcut_command`] for the lookup alone.

use egui::{Event, Key, Modifiers};

use super::TimelineHost;
use super::ids::{IdScope, tl_id};
use crate::dock::PanelKind;

/// Set from a plain Spacebar press until its release: a tap runs Spacebar's command, a hold
/// (the Hand tool, a drag) doesn't, as in EffectCraft.
fn space_tap_id() -> egui::Id {
    tl_id("hosted-spacebar-tap")
}

/// Runs this frame's shortcut keys for `panel` (it has the keyboard focus in the host) and
/// consumes them; returns the command ids run. Does nothing while a text field, menu or dialog
/// has the keyboard. Unbound keys are left to the host. Errors go to `ui().status`.
pub fn forward_shortcuts(host: &mut dyn TimelineHost, ctx: &egui::Context, panel: PanelKind) -> Vec<String> {
    let _ids = IdScope::enter(host.id_scope());
    let mut ran = Vec::new();
    if ctx.input(|i| i.pointer.any_down()) || ctx.egui_wants_keyboard_input() {
        ctx.data_mut(|d| d.remove::<bool>(space_tap_id()));
    }
    if ctx.egui_wants_keyboard_input() || crate::widgets::any_menu_open(ctx) || host.dialog().is_some() {
        return ran;
    }
    let keys: Vec<(Key, Modifiers, bool, Option<Key>)> = ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                Event::Key { key, pressed: true, repeat, modifiers, physical_key }
                    if !*repeat || matches!(key, Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown) =>
                {
                    Some((*key, *modifiers, true, *physical_key))
                }
                Event::Key { key: Key::Space, pressed: false, .. } => Some((Key::Space, Modifiers::NONE, false, None)),
                _ => None,
            })
            .collect()
    });
    let graph_editor = host.ui().timeline.graph_editor;
    for (key, mods, pressed, physical_key) in keys {
        if key == Key::Space && (!pressed || !mods.any()) {
            if pressed {
                ctx.data_mut(|d| d.insert_temp(space_tap_id(), true));
                continue;
            }
            if !ctx.data_mut(|d| d.remove_temp::<bool>(space_tap_id())).unwrap_or(false) {
                continue;
            }
        }
        // Delete / Backspace clear what the panel has selected (Edit ▸ Clear), as in EffectCraft.
        let command = if pressed && !mods.any() && matches!(key, Key::Delete | Key::Backspace) {
            Some(("edit.clear".to_string(), serde_json::json!({})))
        } else {
            // Physical-key bindings first (Zoom In/Out Time's - and = by key position), as
            // EffectCraft's own dispatch: `bindings` leaves them out.
            physical_key
                .filter(|_| pressed)
                .and_then(|pk| physical_binding(host.session(), pk, mods))
                .or_else(|| crate::menus::shortcut_command(host.session(), panel, graph_editor, key, mods))
        };
        let Some((id, params)) = command else { continue };
        if pressed {
            ctx.input_mut(|i| i.consume_key(mods, key));
        }
        if let Err(e) = host.invoke(ctx, &id, params) {
            host.ui_mut().status = e;
        }
        ran.push(id);
    }
    ran
}

/// The command a `Physical+…` shortcut binds to physical key `key` with `mods`.
fn physical_binding(session: &effectcraft_engine::Session, key: Key, mods: Modifiers) -> Option<(String, serde_json::Value)> {
    session.shortcuts().bindings().into_iter().find_map(|(sc, b)| {
        let (m, k) = crate::menus::parse_shortcut(sc.strip_prefix("Physical+")?)?;
        (k == key && crate::menus::mods_match(m, mods)).then(|| (b.command.clone(), if b.params.is_null() { serde_json::json!({}) } else { b.params.clone() }))
    })
}
