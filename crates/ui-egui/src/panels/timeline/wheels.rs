//! The Timeline's wheels (batch 1): the Graph wheel (AX2, hold G with keys selected) and the
//! Layer wheel (AX4, hold A with layers selected).
//!
//! A held key opens its wheel after [`HOLD`] seconds; a tap keeps the key's own meaning (G: Pen
//! tool, A: reveal Anchor Point, or whatever it is bound to): the press is held back and replayed
//! to the shortcut handler when the key comes up early. Every slice runs an engine command, so
//! agents do the same with `command_run` (and History lists it).

use egui::{Key, Pos2};
use serde_json::{Value, json};

use super::TimelineHost;
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

/// The Layer wheel's slices, clockwise from the top.
pub(crate) fn layer_items() -> Vec<WheelItem> {
    vec![
        WheelItem::new("fade", "Fade exit", "Opacity to 0 over the last 8 frames (layer.fadeExit)"),
        WheelItem::new("bounce", "Bounce", "Toolkit ▸ Bounce: a spring after the last key (motionLibrary.tool bounce)"),
        WheelItem::new("slide", "Fast In slide-up", "Devin ▸ Graphs ▸ Fast In on a slide up from the in point (layer.fastIn)"),
        WheelItem::new("cascade", "Cascade 1 f", "Each layer starts 1 frame after the one above (layer.cascade)"),
        WheelItem::new("glow", "Caption glow", "Elias ▸ Text styles ▸ Caption glow + double shadow (motionLibrary.apply)"),
        WheelItem::new("save", "Save to Library", "Save the selected layers to Motion Library ▸ Elias (motionLibrary.saveSelection)"),
    ]
}

/// The Motion Library text style the Caption glow slice and button apply.
pub(crate) const CAPTION_GLOW: &str = "Caption glow + double shadow";

/// Opens the Layer wheel (a held A, or a button).
pub(crate) fn open_layer(app: &mut dyn TimelineHost, ctx: &egui::Context, center: Pos2, hold: Option<Key>) {
    if app.session().state.selected_layers.is_empty() {
        app.ui_mut().status = "Select a layer first: click a layer name.".into();
        return;
    }
    wheel::open(ctx, LAYER, center, "A", layer_items(), hold);
}

/// Where a wheel opens: the pointer, else the middle of the window.
pub(crate) fn pointer_or_middle(ctx: &egui::Context) -> Pos2 {
    ctx.input(|i| i.pointer.hover_pos()).unwrap_or_else(|| ctx.content_rect().center())
}

/// Opens the Graph wheel (the quick row's button, or a held G).
pub(crate) fn open_graph(app: &mut dyn TimelineHost, ctx: &egui::Context, center: Pos2, hold: Option<Key>) {
    if app.session().state.selected_keys.is_empty() {
        app.ui_mut().status = "Select keys first: click a keyframe of a move.".into();
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
fn wheel_for(app: &dyn TimelineHost, key: Key) -> Option<&'static str> {
    match key {
        Key::G if !app.session().state.selected_keys.is_empty() => Some(GRAPH),
        Key::A if !app.session().state.selected_layers.is_empty() => Some(LAYER),
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
            // egui 0.36 `Event::Key {key, physical_key, pressed, repeat, modifiers}`; this runs before
            // menus::handle_shortcuts, which reads the frame's `Event::Key` presses (checked).
            ctx.input_mut(|i| {
                i.events.push(egui::Event::Key { key: p.key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE })
            });
            return;
        }
        if now - p.since >= HOLD {
            ctx.data_mut(|d| d.remove::<Pending>(pending_id()));
            match wheel_for(app, p.key) {
                Some(GRAPH) => open_graph(app, ctx, pointer_or_middle(ctx), Some(p.key)),
                Some(LAYER) => open_layer(app, ctx, pointer_or_middle(ctx), Some(p.key)),
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
    if ctx.egui_wants_keyboard_input() || app.dialog().is_some() || crate::widgets::any_menu_open(ctx) {
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
fn run(app: &mut dyn TimelineHost, ctx: &egui::Context, id: &str, params: Value) -> Option<Value> {
    match app.invoke(ctx, id, params) {
        Ok(v) => Some(v),
        Err(e) => {
            app.ui_mut().status = e;
            None
        }
    }
}

fn toast(app: &mut dyn TimelineHost, ctx: &egui::Context, msg: String) {
    app.toast(msg, ctx.input(|i| i.time));
}

/// A graph reply's name, with "(linked)" for a Library graph.
fn graph_label(v: &Value) -> String {
    let g = &v["graph"];
    let name = g["name"].as_str().unwrap_or("graph");
    if g["linked"].as_bool() == Some(true) { format!("{name} (linked to Motion Library)") } else { name.to_string() }
}

/// Runs a Graph wheel slice.
pub(crate) fn run_graph(app: &mut dyn TimelineHost, ctx: &egui::Context, item: &str) {
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

/// Applies the Motion Library card found by `query` (its name) to the selection.
pub(crate) fn apply_library_named(app: &mut dyn TimelineHost, ctx: &egui::Context, query: &str) -> Option<Value> {
    let found = run(app, ctx, "library.search", json!({"query": query, "source": "motion", "limit": 5}))?;
    let entries = found["entries"].as_array().cloned().unwrap_or_default();
    // The exact name first, else the best hit.
    let hit = entries.iter().find(|e| e["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(query))).or(entries.first());
    let Some(id) = hit.and_then(|e| e["id"].as_str()).map(str::to_string) else {
        app.ui_mut().status = format!("No Motion Library item “{query}” (is the library synced?)");
        return None;
    };
    // TODO(batch1-verify): a text-style preset card applies to the selected layers through motionLibrary.apply.
    run(app, ctx, "motionLibrary.apply", json!({"id": id}))
}

/// Runs a Layer wheel slice (also the quick row's buttons).
pub(crate) fn run_layer(app: &mut dyn TimelineHost, ctx: &egui::Context, item: &str) {
    let n = app.session().state.selected_layers.len();
    match item {
        "fade" => {
            if run(app, ctx, "layer.fadeExit", json!({})).is_some() {
                toast(app, ctx, "Fade exit: opacity 100 → 0 over the last 8 f".into());
            }
        }
        "bounce" => {
            // TODO(batch1-verify): the toolkit bounce springs the selected keys, else the Tools panel's property.
            if run(app, ctx, "motionLibrary.tool", json!({"tool": "bounce"})).is_some() {
                toast(app, ctx, "Bounce: Motion Library spring".into());
            }
        }
        "slide" => {
            if run(app, ctx, "layer.fastIn", json!({"property": "position"})).is_some() {
                toast(app, ctx, "Fast In slide-up added".into());
            }
        }
        "cascade" => {
            if n < 2 {
                app.ui_mut().status = "Cascade needs two or more layers: Shift-click their names.".into();
            } else if run(app, ctx, "layer.cascade", json!({"frames": 1})).is_some() {
                toast(app, ctx, format!("Cascaded {n} layers, 1 f apart"));
            }
        }
        "glow" => {
            if apply_library_named(app, ctx, CAPTION_GLOW).is_some() {
                toast(app, ctx, format!("{CAPTION_GLOW} applied"));
            }
        }
        "save" => {
            let first = app.session().state.selected_layers.first().copied();
            let name = first
                .and_then(|id| app.session().active_comp().and_then(|c| c.layer(id)).map(|l| format!("{} moves", l.name)))
                .unwrap_or_else(|| "My moves".into());
            if run(app, ctx, "motionLibrary.saveSelection", json!({"name": name, "category": "Elias", "kind": "layers"})).is_some() {
                toast(app, ctx, format!("Saving “{name}” to Motion Library ▸ Elias"));
            }
        }
        other => app.ui_mut().status = format!("Unknown Layer wheel item `{other}`"),
    }
}

/// Call after [`wheel::paint`] each frame: runs the slices picked on the Timeline's wheels.
pub fn take_picks(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if let Some(item) = wheel::take_pick(ctx, GRAPH) {
        run_graph(app, ctx, &item);
    }
    if let Some(item) = wheel::take_pick(ctx, LAYER) {
        run_layer(app, ctx, &item);
    }
    // AX3: an Ease card edited in Motion Library ▸ Curves updates the moves linked to it.
    crate::panels::motion_library_curves::drain_relink(app, ctx);
}
