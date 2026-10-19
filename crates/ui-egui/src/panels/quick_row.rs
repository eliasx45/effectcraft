//! The Timeline's quick-button row (batch 1, AX5 + AX7): a strip above the layers with one
//! button per pinned Motion Library item, grouped Graphs | Text | Effects | Exits | Cards, and
//! the Find word box at its right end.
//!
//! - Every button runs a command (its tooltip shows the Motion Library path and the command), so
//!   Claude runs the same thing with `command_run`.
//! - **Fast In ↑** is a split button: a click puts Fast In on the picked property (`layer.fastIn`);
//!   hovering it shows the chips P Position · O Opacity · S Scale · R Rotation, and its letter
//!   shows the pick. Without a pick it follows the property of the selected keys.
//! - **Make card ▾** opens the card wheel (lane C's `open_card_wheel`).
//! - **+ pin** pins the card selected in the Motion Library; right-click a pin to unpin it. Pins
//!   are kept with the workspace (`TimelineState::quick_row`).
//! - **Find word** (AX7): type a word and the playhead jumps to it (`speech.find`); Enter goes to
//!   the next time it is said.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::theme::Tokens;

/// The row's height.
pub const ROW_H: f32 = 28.0;

/// The groups, left to right.
pub const GROUPS: [&str; 5] = ["Graphs", "Text", "Effects", "Exits", "Cards"];

/// Commands the row runs itself rather than through the dispatcher.
pub const FAST_IN: &str = "ui:fastIn";
pub const GRAPH_WHEEL: &str = "ui:graphWheel";
pub const CARD_WHEEL: &str = "ui:cardWheel";
/// Applies the Motion Library item named `params.name`.
pub const LIBRARY_NAMED: &str = "ui:library";

/// One pinned button.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickPin {
    /// One of [`GROUPS`].
    pub group: String,
    pub label: String,
    /// Where it is in the Motion Library ("Devin ▸ Graphs ▸ Fast In").
    #[serde(default)]
    pub ml: String,
    /// The command it runs (an engine or UI command id, or one of the `ui:` ids above).
    pub command: String,
    #[serde(default)]
    pub params: Value,
}

impl QuickPin {
    fn new(group: &str, label: &str, ml: &str, command: &str, params: Value) -> QuickPin {
        QuickPin { group: group.into(), label: label.into(), ml: ml.into(), command: command.into(), params }
    }

    /// A stable id for automation (`timeline.quick.<id>`).
    pub fn auto_id(&self) -> String {
        self.label.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
    }
}

/// The row's state, kept with the workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickRowState {
    pub shown: bool,
    /// `None`: the default pins.
    pub pins: Option<Vec<QuickPin>>,
    /// Fast In's property pick (`p`, `o`, `s`, `r`); `None` follows the selected keys.
    pub fast_in_pick: Option<String>,
    /// The Find word box.
    #[serde(skip)]
    pub find: String,
    #[serde(skip)]
    pub found: Option<(String, u64, u64, i64)>,
    #[serde(skip)]
    pub find_error: Option<String>,
}

impl Default for QuickRowState {
    fn default() -> Self {
        QuickRowState { shown: true, pins: None, fast_in_pick: None, find: String::new(), found: None, find_error: None }
    }
}

/// The pins a new workspace starts with (the approved mock's row).
pub fn default_pins() -> Vec<QuickPin> {
    vec![
        QuickPin::new("Graphs", "Fast In ↑", "Devin ▸ Graphs ▸ Fast In", FAST_IN, json!({})),
        QuickPin::new("Graphs", "G ▾", "Devin ▸ Graphs (the Graph wheel; hold G)", GRAPH_WHEEL, json!({})),
        // TODO(batch1-verify): text.apply is lane D's command (Batch 1b); AX9 adds the add-on chips.
        QuickPin::new("Text", "T3b ↑", "Elias ▸ Text animations ▸ T3b Pop · mid", "text.apply", json!({"preset": "T3b"})),
        QuickPin::new(
            "Text",
            "Caption glow",
            "Elias ▸ Text styles ▸ Caption glow + double shadow",
            LIBRARY_NAMED,
            json!({"name": crate::panels::timeline::wheels::CAPTION_GLOW}),
        ),
        QuickPin::new("Effects", "Bounce", "Toolkit ▸ Bounce", "motionLibrary.tool", json!({"tool": "bounce"})),
        QuickPin::new("Exits", "Fade exit", "Elias ▸ Exits ▸ Fade 8 f", "layer.fadeExit", json!({"frames": 8})),
        QuickPin::new("Cards", "Make card ▾", "Elias ▸ Cards (wheel)", CARD_WHEEL, json!({})),
    ]
}

/// Fast In's property chips: (letter, name).
pub const FAST_IN_PROPS: [(&str, &str); 4] = [("p", "Position"), ("o", "Opacity"), ("s", "Scale"), ("r", "Rotation")];

/// The property of the selected keys as a Fast In pick (`p` without one).
fn keys_pick(app: &EffectcraftApp) -> &'static str {
    let Some(comp) = app.session.active_comp() else { return "p" };
    for k in &app.session.state.selected_keys {
        let Some(l) = comp.layer(k.layer) else { continue };
        let Some(path) = effectcraft_engine::commands::match_path_of(&l.props, k.prop) else { continue };
        let leaf = path.rsplit('/').next().unwrap_or("");
        let pick = match leaf {
            "position" | "xPosition" | "yPosition" | "zPosition" => "p",
            "opacity" => "o",
            "scale" => "s",
            "rotation" | "xRotation" | "yRotation" | "orientation" => "r",
            _ => continue,
        };
        return pick;
    }
    "p"
}

/// The Fast In pick shown on the split button.
pub fn fast_in_pick(app: &EffectcraftApp) -> String {
    app.ui.timeline.quick_row.fast_in_pick.clone().unwrap_or_else(|| keys_pick(app).to_string())
}

fn prop_name(pick: &str) -> &'static str {
    FAST_IN_PROPS.iter().find(|(l, _)| *l == pick).map_or("Position", |(_, n)| n)
}

fn toast(app: &mut EffectcraftApp, ctx: &egui::Context, msg: String) {
    app.toast = Some((msg, ctx.input(|i| i.time)));
}

/// Fast In on `pick` for the selected layers.
pub fn run_fast_in(app: &mut EffectcraftApp, ctx: &egui::Context, pick: &str) {
    if app.session.state.selected_layers.is_empty() {
        app.ui.status = format!("Picked {}. Select a layer, then click Fast In ↑", prop_name(pick));
        return;
    }
    let property = prop_name(pick).to_ascii_lowercase();
    match crate::menus::invoke(app, ctx, "layer.fastIn", json!({"property": property})) {
        Ok(_) => toast(app, ctx, format!("Fast In on {}", prop_name(pick))),
        Err(e) => app.ui.status = e,
    }
}

/// Runs a pin's command (`at`: the button, where a wheel opens).
pub fn run_pin(app: &mut EffectcraftApp, ctx: &egui::Context, pin: &QuickPin, at: Pos2) {
    match pin.command.as_str() {
        FAST_IN => {
            let pick = fast_in_pick(app);
            run_fast_in(app, ctx, &pick);
        }
        GRAPH_WHEEL => crate::panels::timeline::wheels::open_graph(app, ctx, at + vec2(0.0, 140.0), None),
        CARD_WHEEL => {
            if app.session.state.selected_layers.is_empty() {
                app.ui.status = "Select a card or a picture first: click its name.".into();
            } else {
                crate::panels::cards::open_card_wheel(ctx, at + vec2(0.0, 140.0));
            }
        }
        LIBRARY_NAMED => {
            let name = pin.params.get("name").and_then(Value::as_str).unwrap_or(&pin.label).to_string();
            if crate::panels::timeline::wheels::apply_library_named(app, ctx, &name).is_some() {
                toast(app, ctx, format!("{name} applied"));
            }
        }
        id => {
            if app.session.state.selected_layers.is_empty() {
                app.ui.status = "Select a layer first: click a layer name.".into();
                return;
            }
            let mut params = if pin.params.is_object() { pin.params.clone() } else { json!({}) };
            // AX9: the Text group's add-on chips change how text presets apply.
            if pin.group == "Text" && id == "text.apply" {
                add_text_options(app, &mut params);
            }
            match crate::menus::invoke(app, ctx, id, params) {
                Ok(_) => toast(app, ctx, format!("{} applied", pin.label)),
                Err(e) => app.ui.status = e,
            }
        }
    }
}

/// AX9 hook: the Text add-on chips' options (filled in by AX9).
fn add_text_options(_app: &EffectcraftApp, _params: &mut Value) {}

/// A pin for the card selected in the Motion Library.
fn pin_selected_card(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let Some(id) = app.ui.motion_library.selected.clone() else {
        app.ui.status = "Select a card in the Motion Library, then click + pin.".into();
        return;
    };
    let name = crate::panels::motion_library::card_name(app, ctx, &id).unwrap_or_else(|| id.clone());
    let low = name.to_ascii_lowercase();
    let group = if low.contains("card") {
        "Cards"
    } else if low.contains("exit") || low.contains(" out") {
        "Exits"
    } else if low.contains("text") || low.contains("caption") || low.contains("title") {
        "Text"
    } else if low.contains("graph") || low.contains("ease") || low.contains("curve") {
        "Graphs"
    } else {
        "Effects"
    };
    let label: String = name.chars().take(24).collect();
    let pin = QuickPin::new(group, &label, &format!("Motion Library ▸ {name}"), "motionLibrary.apply", json!({"id": id}));
    let pins = app.ui.timeline.quick_row.pins.get_or_insert_with(default_pins);
    if pins.iter().any(|p| p.command == pin.command && p.params == pin.params) {
        app.ui.status = format!("“{name}” is already pinned.");
        return;
    }
    pins.push(pin);
    toast(app, ctx, format!("Pinned “{name}” to the quick row"));
}

fn button(app: &mut EffectcraftApp, ui: &mut egui::Ui, at: Pos2, text: &str, auto: &str, tip: &str, on: bool) -> (Rect, egui::Response) {
    let t = app.tokens;
    let g = ui.painter().layout_no_wrap(text.to_string(), Tokens::ui(11.5), t.text);
    let r = Rect::from_min_size(at, vec2(g.size().x + 14.0, ROW_H - 8.0));
    let resp = ui.interact(r, egui::Id::new(("tl-quick", auto)), Sense::click()).on_hover_text(tip);
    let fill = if on || resp.hovered() { t.field_border } else { t.field_bg };
    ui.painter().rect_filled(r, 4.0, fill);
    ui.painter().galley(pos2(r.min.x + 7.0, r.center().y - g.size().y / 2.0), g, t.text);
    app.auto.add(&format!("timeline.quick.{auto}"), r, text);
    (r, resp)
}

fn divider(ui: &egui::Ui, x: f32, r: Rect, c: Color32) {
    ui.painter().line_segment([pos2(x, r.min.y + 6.0), pos2(x, r.max.y - 6.0)], Stroke::new(1.0, c));
}

/// Draws the row at the top of `rect` and returns the rest of `rect` for the Timeline.
// TODO(batch1-verify): nothing else in the Timeline reads the panel rect it was given (drop zones, the empty-comp drop).
pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: Rect) -> Rect {
    if !app.ui.timeline.quick_row.shown || rect.height() < ROW_H * 3.0 {
        return rect;
    }
    let ctx = ui.ctx().clone();
    let t = app.tokens;
    let row = Rect::from_min_size(rect.min, vec2(rect.width(), ROW_H));
    let rest = Rect::from_min_max(pos2(rect.min.x, row.max.y), rect.max);
    ui.painter().rect_filled(row, 0.0, t.panel_bg);
    ui.painter().line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, t.field_border));
    let clip = ui.clip_rect();
    ui.set_clip_rect(row.intersect(clip));
    // The Find word box keeps the right end.
    let find_w = 170.0_f32.min(row.width() * 0.3);
    let find_r = Rect::from_min_size(pos2(row.max.x - find_w - 8.0, row.min.y + 4.0), vec2(find_w, ROW_H - 8.0));
    let limit = find_r.min.x - 8.0;
    let mut x = row.min.x + 10.0;
    let y = row.min.y + 4.0;
    ui.painter().text(pos2(x, row.center().y), Align2::LEFT_CENTER, "Quick", Tokens::semibold(11.0), t.text_faint);
    x += 44.0;
    let pins = app.ui.timeline.quick_row.pins.clone().unwrap_or_else(default_pins);
    let mut clicked: Option<(QuickPin, Pos2)> = None;
    let mut unpin: Option<usize> = None;
    for (gi, group) in GROUPS.iter().enumerate() {
        let in_group: Vec<(usize, &QuickPin)> = pins.iter().enumerate().filter(|(_, p)| p.group == *group).collect();
        if in_group.is_empty() {
            continue;
        }
        if gi > 0 {
            divider(ui, x - 5.0, row, t.field_border);
        }
        ui.painter().text(pos2(x, row.center().y), Align2::LEFT_CENTER, *group, Tokens::ui(10.0), t.text_faint);
        x += ui.painter().layout_no_wrap(group.to_string(), Tokens::ui(10.0), t.text_faint).size().x + 6.0;
        for (i, pin) in in_group {
            if x > limit - 40.0 {
                break;
            }
            let tip = format!("Motion Library: {} · Claude runs: {} {}", pin.ml, pin.command, pin.params);
            if pin.command == FAST_IN {
                x = fast_in_split(app, ui, &ctx, pos2(x, y), pin, &tip) + 6.0;
                continue;
            }
            let (r, resp) = button(app, ui, pos2(x, y), &pin.label, &pin.auto_id(), &tip, false);
            if resp.clicked() {
                clicked = Some((pin.clone(), r.center_bottom()));
            }
            crate::widgets::context_menu(&resp, |ui| {
                if ui.button("Unpin").clicked() {
                    unpin = Some(i);
                    ui.close();
                }
            });
            x = r.max.x + 6.0;
        }
        // AX9: the Text group's add-on chips sit after its buttons.
        if *group == "Text" {
            x = text_chips(app, ui, pos2(x, y), limit);
        }
        x += 8.0;
    }
    divider(ui, x - 5.0, row, t.field_border);
    if x < limit - 40.0 {
        let (_, resp) = button(app, ui, pos2(x, y), "+ pin", "pin", "Pin the card selected in the Motion Library here", false);
        if resp.clicked() {
            pin_selected_card(app, &ctx);
        }
    }
    find_box(app, ui, &ctx, find_r);
    ui.set_clip_rect(clip);
    if let Some(i) = unpin {
        let list = app.ui.timeline.quick_row.pins.get_or_insert_with(default_pins);
        if i < list.len() {
            list.remove(i);
        }
    }
    if let Some((pin, at)) = clicked {
        run_pin(app, &ctx, &pin, at);
    }
    rest
}

/// AX9 hook: the Text add-on chips (filled in by AX9). Returns the x after them.
fn text_chips(_app: &mut EffectcraftApp, _ui: &mut egui::Ui, at: Pos2, _limit: f32) -> f32 {
    at.x
}

/// The Fast In split button; returns its right edge.
fn fast_in_split(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, at: Pos2, pin: &QuickPin, tip: &str) -> f32 {
    let t = app.tokens;
    let pick = fast_in_pick(app);
    let auto_pick = app.ui.timeline.quick_row.fast_in_pick.is_none();
    let (main_r, main) = button(app, ui, at, &pin.label, "fastIn", tip, false);
    let letter_tip = format!("Fast In goes on {}{} · hover to change", prop_name(&pick), if auto_pick { " (auto: your selected keys' property)" } else { "" });
    let (pick_r, pick_resp) = button(app, ui, pos2(main_r.max.x + 1.0, at.y), &pick.to_ascii_uppercase(), "fastIn.pick", &letter_tip, !auto_pick);
    let split = main_r.union(pick_r);
    if main.clicked() {
        run_fast_in(app, ctx, &pick);
    }
    // Hovering the split (or its chips) shows the property chips under it.
    let open_id = egui::Id::new("tl-quick-fastin-chips");
    let chips_r = Rect::from_min_size(pos2(split.min.x, split.max.y + 2.0), vec2(330.0, ROW_H - 4.0));
    let ptr = ctx.input(|i| i.pointer.hover_pos());
    let over = ptr.is_some_and(|p| {
        split.expand(2.0).contains(p) || chips_r.expand(4.0).contains(p) || Rect::from_min_max(split.left_bottom(), chips_r.right_top()).contains(p)
    });
    let open = over || pick_resp.hovered();
    ctx.data_mut(|d| d.insert_temp(open_id, open));
    if !open {
        return split.max.x;
    }
    let mut chosen: Option<String> = None;
    egui::Area::new(open_id).order(egui::Order::Foreground).fixed_pos(chips_r.min).constrain(true).show(ctx, |ui| {
        ui.painter().rect_filled(chips_r, 4.0, t.panel_bg);
        ui.painter().rect_stroke(chips_r, 4.0, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
        let mut cx = chips_r.min.x + 4.0;
        for (letter, name) in FAST_IN_PROPS {
            let text = format!("{} {name}", letter.to_ascii_uppercase());
            let g = ui.painter().layout_no_wrap(text.clone(), Tokens::ui(11.0), t.text);
            let r = Rect::from_min_size(pos2(cx, chips_r.min.y + 3.0), vec2(g.size().x + 12.0, chips_r.height() - 6.0));
            let resp = ui.interact(r, egui::Id::new(("tl-quick-fastin-chip", letter)), Sense::click());
            let on = pick == letter;
            ui.painter().rect_filled(r, 3.0, if on || resp.hovered() { t.accent } else { t.field_bg });
            ui.painter().galley(pos2(r.min.x + 6.0, r.center().y - g.size().y / 2.0), g, t.text);
            app.auto.add(&format!("timeline.quick.fastIn.{letter}"), r, &text);
            if resp.clicked() {
                chosen = Some(letter.to_string());
            }
            cx = r.max.x + 4.0;
        }
    });
    if let Some(letter) = chosen {
        app.ui.timeline.quick_row.fast_in_pick = Some(letter.clone());
        run_fast_in(app, ctx, &letter);
    }
    split.max.x
}

/// AX7: the Find word box. Typing jumps to the word; Enter goes to its next occurrence.
fn find_box(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, r: Rect) {
    let t = app.tokens;
    let mut text = app.ui.timeline.quick_row.find.clone();
    let (resp, clear) = crate::widgets::search_field_with_clear(ui, r, &mut text, "Find word", &t, false);
    app.auto.add_response("timeline.quick.find", &resp, "Find word");
    if let Some(c) = clear {
        app.auto.add("timeline.quick.find.clear", c.rect, "Clear");
        if c.clicked() {
            text.clear();
        }
    }
    let enter = resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter));
    let changed = text != app.ui.timeline.quick_row.find;
    app.ui.timeline.quick_row.find = text.clone();
    let q = text.trim().to_string();
    if q.is_empty() {
        app.ui.timeline.quick_row.found = None;
        app.ui.timeline.quick_row.find_error = None;
    } else if changed || enter {
        // Enter: the next time it is said (wrapping); typing: the first.
        let occurrence = match (&app.ui.timeline.quick_row.found, enter) {
            (Some((w, n, of, _)), true) if *w == q => n % (*of).max(1) + 1,
            _ => 1,
        };
        match crate::menus::invoke(app, ctx, "speech.find", json!({"word": q, "occurrence": occurrence})) {
            Ok(v) => {
                let of = v.get("of").and_then(Value::as_u64).unwrap_or(1);
                let frame = v.get("frame").and_then(Value::as_i64).unwrap_or(0);
                app.ui.timeline.quick_row.found = Some((q.clone(), occurrence, of, frame));
                app.ui.timeline.quick_row.find_error = None;
            }
            Err(e) => {
                app.ui.timeline.quick_row.found = None;
                app.ui.timeline.quick_row.find_error = Some(e);
            }
        }
        if enter {
            resp.request_focus();
        }
    }
    // Where it was found (or why not), left of the box.
    let note = match (&app.ui.timeline.quick_row.found, &app.ui.timeline.quick_row.find_error) {
        (Some((w, n, of, f)), _) => Some((format!("“{w}” f{f} · {n}/{of}"), t.cache_green)),
        (None, Some(_)) => Some(("not said".to_string(), t.text_faint)),
        _ => None,
    };
    if let Some((s, c)) = note {
        ui.painter().text(pos2(r.min.x - 6.0, r.center().y), Align2::RIGHT_CENTER, s, Tokens::ui(10.5), c);
    }
}
