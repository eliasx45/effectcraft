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

use crate::panels::timeline::TimelineHost;
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
    /// AX9: the Text group's add-on chips. `sync`: the preset's reveal on the stressed
    /// syllable; `glow`: also Caption glow; `distance`: near | mid | far (the preset's Travel
    /// controls at 40 / 90 / 150 px); `on_word`: aim at the word under the playhead.
    pub text_sync: bool,
    pub text_glow: bool,
    pub text_distance: String,
    pub text_on_word: bool,
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
        QuickRowState {
            shown: true,
            pins: None,
            fast_in_pick: None,
            text_sync: true,
            text_glow: false,
            text_distance: "mid".into(),
            text_on_word: false,
            find: String::new(),
            found: None,
            find_error: None,
        }
    }
}

/// The pins a new workspace starts with (the approved mock's row).
pub fn default_pins() -> Vec<QuickPin> {
    vec![
        QuickPin::new("Graphs", "Fast In ↑", "Devin ▸ Graphs ▸ Fast In", FAST_IN, json!({})),
        QuickPin::new("Graphs", "G ▾", "Devin ▸ Graphs (the Graph wheel; hold G)", GRAPH_WHEEL, json!({})),
        // The Text group also shows one button per text preset card (Elias ▸ Text animations,
        // `text.presetFamilies`; CX8c), before the add-on chips: nothing about them is pinned here.
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
fn keys_pick(app: &dyn TimelineHost) -> &'static str {
    let Some(comp) = app.session().active_comp() else { return "p" };
    for k in &app.session().state.selected_keys {
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
pub fn fast_in_pick(app: &dyn TimelineHost) -> String {
    app.ui().timeline.quick_row.fast_in_pick.clone().unwrap_or_else(|| keys_pick(app).to_string())
}

fn prop_name(pick: &str) -> &'static str {
    FAST_IN_PROPS.iter().find(|(l, _)| *l == pick).map_or("Position", |(_, n)| n)
}

fn toast(app: &mut dyn TimelineHost, ctx: &egui::Context, msg: String) {
    app.toast(msg, ctx.input(|i| i.time));
}

/// Fast In on `pick` for the selected layers.
pub fn run_fast_in(app: &mut dyn TimelineHost, ctx: &egui::Context, pick: &str) {
    if app.session().state.selected_layers.is_empty() {
        app.ui_mut().status = format!("Picked {}. Select a layer, then click Fast In ↑", prop_name(pick));
        return;
    }
    let property = prop_name(pick).to_ascii_lowercase();
    match app.invoke(ctx, "layer.fastIn", json!({"property": property})) {
        Ok(_) => toast(app, ctx, format!("Fast In on {}", prop_name(pick))),
        Err(e) => app.ui_mut().status = e,
    }
}

/// Runs a pin's command (`at`: the button, where a wheel opens).
pub fn run_pin(app: &mut dyn TimelineHost, ctx: &egui::Context, pin: &QuickPin, at: Pos2) {
    match pin.command.as_str() {
        FAST_IN => {
            let pick = fast_in_pick(app);
            run_fast_in(app, ctx, &pick);
        }
        GRAPH_WHEEL => crate::panels::timeline::wheels::open_graph(app, ctx, at + vec2(0.0, 140.0), None),
        CARD_WHEEL => {
            if app.session().state.selected_layers.is_empty() {
                app.ui_mut().status = "Select a card or a picture first: click its name.".into();
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
            if app.session().state.selected_layers.is_empty() {
                app.ui_mut().status = "Select a layer first: click a layer name.".into();
                return;
            }
            let mut params = if pin.params.is_object() { pin.params.clone() } else { json!({}) };
            // AX9: the Text group's add-on chips change how text presets apply; text.apply's
            // followUp (the glow style) runs after it.
            let r = if id == "text.apply" {
                add_text_options(app, &mut params);
                super::text_apply::run(app, ctx, params)
            } else {
                app.invoke(ctx, id, params)
            };
            match r {
                Ok(_) => toast(app, ctx, format!("{} applied", pin.label)),
                Err(e) => app.ui_mut().status = e,
            }
        }
    }
}

/// AX9: the Text add-on chips' options on a `text.apply` call (lane D's command). The selected
/// layers are passed as `layers`: with only `word`, text.apply would make a new layer instead of
/// animating the selection.
fn add_text_options(app: &dyn TimelineHost, params: &mut Value) {
    if params.get("layers").is_none() {
        params["layers"] = json!(app.session().state.selected_layers.iter().map(|l| l.0).collect::<Vec<_>>());
    }
    let q = &app.ui().timeline.quick_row;
    params["sync"] = json!(q.text_sync);
    params["glow"] = json!(q.text_glow);
    params["distance"] = json!(q.text_distance);
    if q.text_on_word
        && let Some(w) = word_at_playhead(app)
    {
        params["word"] = json!(w);
    }
}

/// The word of the comp's speech map nearest the playhead (stressed words first), among the words
/// that can be heard: lane B (SX1) keeps the words in `comp.speech` (comp time, moved with the
/// voice layer) and hides the ones trimmed off the voice layer (`SpeechMap::audible`).
pub fn word_at_playhead(app: &dyn TimelineHost) -> Option<String> {
    let comp = app.session().active_comp()?;
    let map = comp.speech.as_ref()?;
    let now = app.session().time().seconds();
    let dist = |w: &effectcraft_engine::project::speech::SpeechWord| {
        let (a, b) = (w.start.seconds(), w.end.seconds());
        if now < a {
            a - now
        } else if now > b {
            now - b
        } else {
            0.0
        }
    };
    let stressed = |w: &&effectcraft_engine::project::speech::SpeechWord| w.syllables.iter().any(|s| s.stress == 1);
    let heard: Vec<&effectcraft_engine::project::speech::SpeechWord> = map.audible(comp).map(|(_, w)| w).collect();
    let best = heard
        .iter()
        .copied()
        .filter(stressed)
        .min_by(|a, b| dist(a).total_cmp(&dist(b)))
        .or_else(|| heard.iter().copied().min_by(|a, b| dist(a).total_cmp(&dist(b))))?;
    Some(best.word.clone())
}

/// The text preset cards (name, label with their controls), read again when the project changes
/// and every 10 s (a card saved in the Motion Library shows up).
pub(crate) fn text_presets(app: &mut dyn TimelineHost, ctx: &egui::Context) -> Vec<(String, String)> {
    type Cached = (u64, f64, Vec<(String, String)>);
    let id = egui::Id::new("tl-quick-text-presets");
    let rev = app.session().revision;
    let now = ctx.input(|i| i.time);
    if let Some((r, t, list)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && !presets_stale(r, t, rev, now)
    {
        return list;
    }
    let list = super::text_apply::preset_list(app);
    ctx.data_mut(|d| d.insert_temp::<Cached>(id, (rev, now, list.clone())));
    list
}

/// A pin for the card selected in the Motion Library.
fn pin_selected_card(app: &mut dyn TimelineHost, ctx: &egui::Context) {
    let Some(id) = app.ui().motion_library.selected.clone() else {
        app.ui_mut().status = "Select a card in the Motion Library, then click + pin.".into();
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
    let pins = app.ui_mut().timeline.quick_row.pins.get_or_insert_with(default_pins);
    if pins.iter().any(|p| p.command == pin.command && p.params == pin.params) {
        app.ui_mut().status = format!("“{name}” is already pinned.");
        return;
    }
    pins.push(pin);
    toast(app, ctx, format!("Pinned “{name}” to the quick row"));
}

fn button(app: &mut dyn TimelineHost, ui: &mut egui::Ui, at: Pos2, text: &str, auto: &str, tip: &str, on: bool) -> (Rect, egui::Response) {
    let t = app.tokens();
    let g = ui.painter().layout_no_wrap(text.to_string(), Tokens::ui(11.5), t.text);
    let r = Rect::from_min_size(at, vec2(g.size().x + 14.0, ROW_H - 8.0));
    let resp = ui.interact(r, egui::Id::new(("tl-quick", auto)), Sense::click()).on_hover_text(tip);
    let fill = if on || resp.hovered() { t.field_border } else { t.field_bg };
    ui.painter().rect_filled(r, 4.0, fill);
    ui.painter().galley(pos2(r.min.x + 7.0, r.center().y - g.size().y / 2.0), g, t.text);
    app.auto_mut().add(&format!("timeline.quick.{auto}"), r, text);
    (r, resp)
}

const PIN_LABEL: &str = "+ pin";

/// Whether the text preset cards read at revision `read_rev`, time `read_at` must be read again.
pub(crate) fn presets_stale(read_rev: u64, read_at: f64, rev: u64, now: f64) -> bool {
    let age = now - read_at;
    read_rev != rev || age.is_nan() || age >= 10.0
}

/// A quick-row button's width for `text` (as `button` draws it).
fn button_w(ui: &egui::Ui, text: &str) -> f32 {
    ui.painter().layout_no_wrap(text.to_string(), Tokens::ui(11.5), Color32::WHITE).size().x + 14.0
}

/// AX5: where the pins and chips must end so "+ pin" (`pin_w` wide, after the last pin) and the
/// Find box's note (`note_w`, left of the box at `find_left`) still fit.
pub(crate) fn pins_limit(find_left: f32, note_w: f32, pin_w: f32) -> f32 {
    find_left - 8.0 - note_w - pin_w - 14.0
}

fn divider(ui: &egui::Ui, x: f32, r: Rect, c: Color32) {
    ui.painter().line_segment([pos2(x, r.min.y + 6.0), pos2(x, r.max.y - 6.0)], Stroke::new(1.0, c));
}

/// Whether [`show`] draws the row at the top of a Timeline in `rect` (it is on and there's room).
pub fn shown(app: &dyn TimelineHost, rect: Rect) -> bool {
    app.ui().timeline.quick_row.shown && rect.height() >= ROW_H * 3.0
}

fn lift_id(scope: Option<egui::Id>) -> egui::Id {
    scope.unwrap_or(egui::Id::NULL).with("quick-row-lift")
}

/// How far up (the Timeline's points) the host whose [`TimelineHost::id_scope`] is `scope` wants
/// the row drawn this frame, 0 when it never said.
pub fn lift(ctx: &egui::Context, scope: Option<egui::Id>) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(lift_id(scope))).filter(|l| l.is_finite() && *l > 0.0).unwrap_or(0.0)
}

/// For a host: draw the row `up` points higher this frame (FigCraft lifts it above its floating
/// toolbar when the toolbar would cover it; Elias, 29 Oct 2026: "Fast In, Caption glow … + pin"
/// under the toolbar at 77 %+). Its slot in the Timeline stays where it was.
pub fn set_lift(ctx: &egui::Context, scope: Option<egui::Id>, up: f32) {
    ctx.data_mut(|d| d.insert_temp(lift_id(scope), if up.is_finite() { up.max(0.0) } else { 0.0 }));
}

/// Draws the row at the top of `rect` and returns the rest of `rect` for the Timeline.
// The Timeline lays out everything from the returned rect; the empty-comp drop runs before the row
// (no comp, no row), and the panel clip is restored before returning.
pub fn show(app: &mut dyn TimelineHost, ui: &mut egui::Ui, rect: Rect) -> Rect {
    if !shown(app, rect) {
        return rect;
    }
    let ctx = ui.ctx().clone();
    let t = app.tokens();
    let slot = Rect::from_min_size(rect.min, vec2(rect.width(), ROW_H));
    // A host can lift the row above something of its own covering it (its slot stays empty).
    let row = slot.translate(vec2(0.0, -lift(&ctx, app.id_scope())));
    let rest = Rect::from_min_max(pos2(rect.min.x, slot.max.y), rect.max);
    ui.painter().rect_filled(row, 0.0, t.panel_bg);
    ui.painter().line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, t.field_border));
    let clip = ui.clip_rect();
    ui.set_clip_rect(row.intersect(clip));
    // The Find word box keeps the right end.
    let find_w = 170.0_f32.min(row.width() * 0.3);
    let find_r = Rect::from_min_size(pos2(row.max.x - find_w - 8.0, row.min.y + 4.0), vec2(find_w, ROW_H - 8.0));
    // The Find box (and its "not said" / "f74 · 1/12" note) and "+ pin" keep their room: a full
    // row drops its last pins, never "+ pin" or the note.
    let note_w = find_box(app, ui, &ctx, find_r);
    let pin_w = button_w(ui, PIN_LABEL);
    let limit = pins_limit(find_r.min.x, note_w, pin_w);
    let mut x = row.min.x + 10.0;
    let y = row.min.y + 4.0;
    ui.painter().text(pos2(x, row.center().y), Align2::LEFT_CENTER, "Quick", Tokens::semibold(11.0), t.text_faint);
    x += 44.0;
    let pins = app.ui().timeline.quick_row.pins.clone().unwrap_or_else(default_pins);
    let mut clicked: Option<(QuickPin, Pos2)> = None;
    let mut unpin: Option<usize> = None;
    for (gi, group) in GROUPS.iter().enumerate() {
        let in_group: Vec<(usize, &QuickPin)> = pins.iter().enumerate().filter(|(_, p)| p.group == *group).collect();
        if in_group.is_empty() {
            continue;
        }
        // A group whose first button no longer fits is left out with its label.
        let label_w = ui.painter().layout_no_wrap(group.to_string(), Tokens::ui(10.0), t.text_faint).size().x + 6.0;
        if in_group.first().is_some_and(|(_, p)| x + label_w + button_w(ui, &p.label) > limit) {
            break;
        }
        if gi > 0 {
            divider(ui, x - 5.0, row, t.field_border);
        }
        ui.painter().text(pos2(x, row.center().y), Align2::LEFT_CENTER, *group, Tokens::ui(10.0), t.text_faint);
        x += ui.painter().layout_no_wrap(group.to_string(), Tokens::ui(10.0), t.text_faint).size().x + 6.0;
        for (i, pin) in in_group {
            let w = if pin.command == FAST_IN { button_w(ui, &pin.label) + button_w(ui, "P") + 1.0 } else { button_w(ui, &pin.label) };
            if x + w > limit {
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
        // CX8c: a button per text preset card, then (AX9) the add-on chips.
        if *group == "Text" {
            for (name, label) in text_presets(app, &ctx) {
                if x + button_w(ui, &format!("{name} ↑")) > limit {
                    break;
                }
                let pin = QuickPin::new("Text", &format!("{name} ↑"), &format!("Elias ▸ Text animations ▸ {name}"), "text.apply", json!({"preset": name}));
                let tip = format!("Motion Library: {label} · Claude runs: text.apply {}", pin.params);
                let (r, resp) = button(app, ui, pos2(x, y), &pin.label, &format!("textPreset.{}", pin.auto_id()), &tip, false);
                if resp.clicked() {
                    clicked = Some((pin, r.center_bottom()));
                }
                x = r.max.x + 6.0;
            }
            x = text_chips(app, ui, pos2(x, y), limit);
        }
        x += 8.0;
    }
    divider(ui, x - 5.0, row, t.field_border);
    let (_, resp) = button(app, ui, pos2(x, y), PIN_LABEL, "pin", "Pin the card selected in the Motion Library here", false);
    if resp.clicked() {
        pin_selected_card(app, &ctx);
    }
    ui.set_clip_rect(clip);
    if let Some(i) = unpin {
        let list = app.ui_mut().timeline.quick_row.pins.get_or_insert_with(default_pins);
        if i < list.len() {
            list.remove(i);
        }
    }
    if let Some((pin, at)) = clicked {
        run_pin(app, &ctx, &pin, at);
    }
    rest
}

/// One add-on chip: a small toggle; returns (its rect, clicked).
fn chip(app: &mut dyn TimelineHost, ui: &mut egui::Ui, at: Pos2, text: &str, auto: &str, tip: &str, on: bool) -> (Rect, bool) {
    let t = app.tokens();
    let g = ui.painter().layout_no_wrap(text.to_string(), Tokens::ui(10.5), if on { Color32::WHITE } else { t.text_dim });
    let r = Rect::from_min_size(pos2(at.x, at.y + 2.0), vec2(g.size().x + 10.0, ROW_H - 12.0));
    let resp = ui.interact(r, egui::Id::new(("tl-quick-chip", auto)), Sense::click()).on_hover_text(tip);
    if on {
        ui.painter().rect_filled(r, 8.0, t.accent);
    } else {
        ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, if resp.hovered() { t.text_dim } else { t.field_border }), StrokeKind::Inside);
    }
    ui.painter().galley(pos2(r.min.x + 5.0, r.center().y - g.size().y / 2.0), g, if on { Color32::WHITE } else { t.text_dim });
    app.auto_mut().add(&format!("timeline.quick.text.{auto}"), r, &format!("{text}{}", if on { " (on)" } else { "" }));
    (r, resp.clicked())
}

/// AX9: the Text group's add-on chips (`sync`, `+glow`, `near/mid/far`, `on "<word>"`): toggles
/// for how the Text buttons apply (`text.apply {sync, glow, distance, word}`; `distance` sets the
/// preset's Travel controls to 40 / 90 / 150 px, CX8b). Returns the x after them.
fn text_chips(app: &mut dyn TimelineHost, ui: &mut egui::Ui, at: Pos2, limit: f32) -> f32 {
    let mut x = at.x;
    let q = app.ui().timeline.quick_row.clone();
    let word = word_at_playhead(app);
    let word_label = format!("on “{}”", word.as_deref().unwrap_or("word"));
    let chips: [(&str, String, &str, bool); 4] = [
        ("sync", "sync".into(), "Put the preset's speed peak on the stressed syllable", q.text_sync),
        ("glow", "+glow".into(), "Also apply Caption glow + double shadow", q.text_glow),
        (
            "distance",
            super::text_apply::distance_label(&q.text_distance),
            "How far the text travels: sets the preset's Travel controls (Travel Distance (px), Bounce Travel (px)) to near 40 / mid 90 / far 150 px; click to change",
            true,
        ),
        ("word", word_label, "Aim at the word under the playhead (else the first stressed word)", q.text_on_word),
    ];
    for (id, text, tip, on) in chips {
        if x + ui.painter().layout_no_wrap(text.clone(), Tokens::ui(10.5), Color32::WHITE).size().x + 10.0 > limit {
            break;
        }
        let (r, clicked) = chip(app, ui, pos2(x, at.y), &text, id, tip, on);
        if clicked {
            let st = &mut app.ui_mut().timeline.quick_row;
            match id {
                "sync" => st.text_sync = !st.text_sync,
                "glow" => st.text_glow = !st.text_glow,
                "distance" => {
                    st.text_distance = match st.text_distance.as_str() {
                        "near" => "mid",
                        "mid" => "far",
                        _ => "near",
                    }
                    .into();
                }
                _ => st.text_on_word = !st.text_on_word,
            }
        }
        x = r.max.x + 4.0;
    }
    x
}

/// The Fast In split button; returns its right edge.
fn fast_in_split(app: &mut dyn TimelineHost, ui: &mut egui::Ui, ctx: &egui::Context, at: Pos2, pin: &QuickPin, tip: &str) -> f32 {
    let t = app.tokens();
    let pick = fast_in_pick(app);
    let auto_pick = app.ui().timeline.quick_row.fast_in_pick.is_none();
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
    let lp = crate::layer_pointer::Lp::of(ui);
    // The chips are a window-level area: under the split's place in the window, at panel size
    // (a host draws the Timeline in a scaled layer; as `widgets::popup_list`).
    let chips_r = Rect::from_min_size(lp.window_p(chips_r.min), chips_r.size());
    let raw = ctx.input(|i| i.pointer.hover_pos());
    let ptr = lp.pos(raw);
    let over = ptr.is_some_and(|p| split.expand(2.0).contains(p))
        || raw.is_some_and(|p| chips_r.expand(4.0).contains(p) || Rect::from_min_max(lp.window_p(split.left_bottom()), chips_r.right_top()).contains(p));
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
            app.auto_mut().add(&format!("timeline.quick.fastIn.{letter}"), r, &text);
            if resp.clicked() {
                chosen = Some(letter.to_string());
            }
            cx = r.max.x + 4.0;
        }
    });
    if let Some(letter) = chosen {
        app.ui_mut().timeline.quick_row.fast_in_pick = Some(letter.clone());
        run_fast_in(app, ctx, &letter);
    }
    split.max.x
}

/// AX7: the Find word box. Typing jumps to the word; Enter goes to its next occurrence.
/// Returns the width its note takes left of the box (0 without one).
fn find_box(app: &mut dyn TimelineHost, ui: &mut egui::Ui, ctx: &egui::Context, r: Rect) -> f32 {
    let t = app.tokens();
    let mut text = app.ui().timeline.quick_row.find.clone();
    let (resp, clear) = crate::widgets::search_field_with_clear(ui, r, &mut text, "Find word", &t, false);
    app.auto_mut().add_response("timeline.quick.find", &resp, "Find word");
    if let Some(c) = clear {
        app.auto_mut().add("timeline.quick.find.clear", c.rect, "Clear");
        if c.clicked() {
            text.clear();
        }
    }
    let enter = resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter));
    let changed = text != app.ui().timeline.quick_row.find;
    app.ui_mut().timeline.quick_row.find = text.clone();
    let q = text.trim().to_string();
    if q.is_empty() {
        app.ui_mut().timeline.quick_row.found = None;
        app.ui_mut().timeline.quick_row.find_error = None;
    } else if changed || enter {
        // Enter: the next time it is said (wrapping); typing: the first.
        let occurrence = match (&app.ui().timeline.quick_row.found, enter) {
            (Some((w, n, of, _)), true) if *w == q => n % (*of).max(1) + 1,
            _ => 1,
        };
        match app.invoke(ctx, "speech.find", json!({"word": q, "occurrence": occurrence})) {
            Ok(v) => {
                let of = v.get("of").and_then(Value::as_u64).unwrap_or(1);
                let frame = v.get("frame").and_then(Value::as_i64).unwrap_or(0);
                app.ui_mut().timeline.quick_row.found = Some((q.clone(), occurrence, of, frame));
                app.ui_mut().timeline.quick_row.find_error = None;
            }
            Err(e) => {
                app.ui_mut().timeline.quick_row.found = None;
                app.ui_mut().timeline.quick_row.find_error = Some(e);
            }
        }
        if enter {
            resp.request_focus();
        }
    }
    // Where it was found (or why not), left of the box.
    let note = match (&app.ui().timeline.quick_row.found, &app.ui().timeline.quick_row.find_error) {
        (Some((w, n, of, f)), _) => Some((format!("“{w}” f{f} · {n}/{of}"), t.cache_green)),
        (None, Some(_)) => Some(("not said".to_string(), t.text_faint)),
        _ => None,
    };
    match note {
        Some((s, c)) => ui.painter().text(pos2(r.min.x - 6.0, r.center().y), Align2::RIGHT_CENTER, s, Tokens::ui(10.5), c).width() + 6.0,
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AX5: after "+ pin" pinned one more card, the full row dropped the Cards group's buttons and
    // "+ pin" itself, and the Find note ("not said") was drawn over the "Cards" label.
    #[test]
    fn a_full_row_keeps_room_for_plus_pin_and_the_find_note() {
        let (find_left, note_w, pin_w) = (1200.0, 60.0, 42.0);
        let limit = pins_limit(find_left, note_w, pin_w);
        // The last pin ends at most at `limit`; "+ pin" follows it (6 px gap) and ends before the note.
        let pin_end = limit + 6.0 + pin_w;
        assert!(pin_end < find_left - 8.0 - note_w, "{pin_end}");
        // Without a note the pins get that room back.
        assert!(pins_limit(find_left, 0.0, pin_w) > limit);
    }

    // SX6: the Script popover read the text preset cards once, so a list read before the Motion
    // Library loaded stayed empty; it now shares this cache, read again on an edit or after 10 s.
    #[test]
    fn text_preset_cards_are_read_again_after_an_edit_or_ten_seconds() {
        assert!(!presets_stale(5, 100.0, 5, 105.0));
        assert!(presets_stale(5, 100.0, 6, 101.0), "an edit");
        assert!(presets_stale(5, 100.0, 5, 110.5), "10 s later");
        assert!(presets_stale(5, 100.0, 5, f64::NAN));
    }
}
