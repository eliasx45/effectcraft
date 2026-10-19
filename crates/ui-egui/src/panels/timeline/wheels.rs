//! The Timeline's wheels (batch 1): the Graph wheel (AX2, hold G with keys selected) and the
//! Layer wheel (AX4, hold A with layers selected).
//!
//! A held key opens its wheel after [`HOLD`] seconds; a tap keeps the key's own meaning (G: Pen
//! tool, A: reveal Anchor Point, or whatever it is bound to): the press is held back and replayed
//! to the shortcut handler when the key comes up early. Every slice runs an engine command, so
//! agents do the same with `command_run` (and History lists it).

use egui::{Key, Pos2};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::wheel::{self, WheelItem};

/// How long a key is held before its wheel opens (seconds).
pub(crate) const HOLD: f64 = 0.2;

/// The Graph wheel's key (`wheel.graph.<item>` automation ids).
pub(crate) const GRAPH: &str = "graph";
/// The Layer wheel's key (`wheel.layer.<item>`).
pub(crate) const LAYER: &str = "layer";

/// The Graph wheel's slices, clockwise from the top (the mock's order).
pub(crate) fn graph_items() -> Vec<WheelItem> {
    vec![
        WheelItem::new("copy", "Copy graph", "Copy the selected keys' speed graph (graph.copy)"),
        WheelItem::new("fast-in", "Fast In", "Devin ▸ Graphs ▸ Fast In: for really fast slide-ins (graph.apply)"),
        WheelItem::new("quick-launch", "Quick launch", "Devin ▸ Graphs ▸ Quick launch · soft landing (graph.apply)"),
        WheelItem::new("smooth-emphasis", "Smooth emphasis", "Devin ▸ Graphs ▸ Smooth emphasis (graph.apply)"),
        WheelItem::new("paste", "Paste graph", "Paste the copied graph onto the selected keys; links stay (graph.paste)"),
        WheelItem::new("slow-launch", "Slow launch", "Devin ▸ Graphs ▸ Slow launch · quick finish (graph.apply)"),
        WheelItem::new("fast-out", "Fast out", "Devin ▸ Graphs ▸ Fast out: for super fast slide-outs (graph.apply)"),
        WheelItem::new("save", "Save to Library", "Save this graph to Motion Library ▸ Curves (graph.save)"),
    ]
}

/// Where a wheel opens: the pointer, else the middle of the window.
pub(crate) fn pointer_or_middle(ctx: &egui::Context) -> Pos2 {
    ctx.input(|i| i.pointer.hover_pos()).unwrap_or_else(|| ctx.content_rect().center())
}

/// Opens the Graph wheel (the quick row's button, or a held G).
pub(crate) fn open_graph(app: &mut EffectcraftApp, ctx: &egui::Context, center: Pos2, hold: Option<Key>) {
    if app.session.state.selected_keys.is_empty() {
        app.ui.status = "Select keys first: click a keyframe of a move.".into();
        return;
    }
    wheel::open(ctx, GRAPH, center, "G", graph_items(), hold);
}

/// A key press held back until we know whether it is a tap or a hold.
#[derive(Clone, Copy, Debug)]
struct Pending {
    key: Key,
    since: f64,
}

fn pending_id() -> egui::Id {
    egui::Id::new("tl-wheel-hold")
}

/// The wheel a held `key` opens, when it can open now.
fn wheel_for(app: &EffectcraftApp, key: Key) -> Option<&'static str> {
    match key {
        Key::G if !app.session.state.selected_keys.is_empty() => Some(GRAPH),
        // TODO(batch1-verify): AX4 adds A → Layer wheel here.
        _ => None,
    }
}

/// A plain (no modifier, not repeated) press of `key` this frame.
fn plain_press(ctx: &egui::Context, key: Key) -> bool {
    ctx.input(|i| {
        i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, modifiers, .. } if *k == key && !modifiers.any()))
    })
}

/// Call before the app's shortcuts each frame: holds back G / A presses that may open a wheel,
/// opens the wheel when the key stays down, and replays the press when it was a tap.
pub fn hold_keys(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if wheel::is_open(ctx) {
        // Escape closes the wheel before the shortcuts see it.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            wheel::close(ctx);
        }
        return;
    }
    let now = ctx.input(|i| i.time);
    if let Some(p) = ctx.data(|d| d.get_temp::<Pending>(pending_id())) {
        let down = ctx.input(|i| i.key_down(p.key));
        if !down {
            // A tap: the key's own meaning, through the normal shortcut path.
            ctx.data_mut(|d| d.remove::<Pending>(pending_id()));
            // TODO(batch1-verify): egui 0.36 `Event::Key` fields; the replayed press reaches menus::handle_shortcuts this frame.
            ctx.input_mut(|i| {
                i.events.push(egui::Event::Key { key: p.key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE })
            });
            return;
        }
        if now - p.since >= HOLD {
            ctx.data_mut(|d| d.remove::<Pending>(pending_id()));
            match wheel_for(app, p.key) {
                Some(GRAPH) => open_graph(app, ctx, pointer_or_middle(ctx), Some(p.key)),
                _ => {}
            }
            return;
        }
        // Still deciding: swallow the key's repeats and look again soon.
        ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, p.key));
        ctx.request_repaint_after(std::time::Duration::from_millis(30));
        return;
    }
    // Typing, a dialog or an open menu keep their keys.
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() || crate::widgets::any_menu_open(ctx) {
        return;
    }
    for key in [Key::G, Key::A] {
        if wheel_for(app, key).is_some() && plain_press(ctx, key) {
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
            ctx.data_mut(|d| d.insert_temp(pending_id(), Pending { key, since: now }));
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
            return;
        }
    }
}

/// Runs a command through the normal dispatcher; an error goes to the status line.
fn run(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, params: Value) -> Option<Value> {
    match crate::menus::invoke(app, ctx, id, params) {
        Ok(v) => Some(v),
        Err(e) => {
            app.ui.status = e;
            None
        }
    }
}

fn toast(app: &mut EffectcraftApp, ctx: &egui::Context, msg: String) {
    app.toast = Some((msg, ctx.input(|i| i.time)));
}

/// A graph reply's name, with "(linked)" for a Library graph.
fn graph_label(v: &Value) -> String {
    let g = &v["graph"];
    let name = g["name"].as_str().unwrap_or("graph");
    if g["linked"].as_bool() == Some(true) { format!("{name} (linked to Motion Library)") } else { name.to_string() }
}

/// Runs a Graph wheel slice.
pub(crate) fn run_graph(app: &mut EffectcraftApp, ctx: &egui::Context, item: &str) {
    match item {
        "copy" => {
            if let Some(v) = run(app, ctx, "graph.copy", json!({})) {
                toast(app, ctx, format!("Copied graph: {}", graph_label(&v)));
            }
        }
        "paste" => {
            if let Some(v) = run(app, ctx, "graph.paste", json!({})) {
                let n = v["moves"].as_array().map_or(0, Vec::len);
                toast(app, ctx, format!("Pasted {} onto {n} move{}", graph_label(&v), if n == 1 { "" } else { "s" }));
            }
        }
        "save" => {
            let n = ctx.data_mut(|d| {
                let n = d.get_temp::<u32>(egui::Id::new("tl-graph-saves")).unwrap_or(0).saturating_add(1);
                d.insert_temp(egui::Id::new("tl-graph-saves"), n);
                n
            });
            let name = format!("Elias graph {n}");
            if let Some(v) = run(app, ctx, "graph.save", json!({"name": name})) {
                // The Library keeps it (Motion Library ▸ Curves).
                let lib = v["library"].clone();
                let id = lib["command"].as_str().unwrap_or("motionLibrary.curve").to_string();
                if run(app, ctx, &id, lib["params"].clone()).is_some() {
                    toast(app, ctx, format!("Saving to Motion Library ▸ Curves as “{name}”"));
                }
            }
        }
        graph => {
            if let Some(v) = run(app, ctx, "graph.apply", json!({"item": graph})) {
                toast(app, ctx, format!("{} applied", graph_label(&v)));
            }
        }
    }
}

/// Call after [`wheel::paint`] each frame: runs the slices picked on the Timeline's wheels.
pub fn take_picks(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if let Some(item) = wheel::take_pick(ctx, GRAPH) {
        run_graph(app, ctx, &item);
    }
    // AX3: an Ease card edited in Motion Library ▸ Curves updates the moves linked to it.
    crate::panels::motion_library_curves::drain_relink(app, ctx);
}
