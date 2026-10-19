//! Window ▸ Script (SX5 + SX6, batch 1 lane B): the voice-over's script lined up with the words
//! said. Everything it shows comes from `script.get` (what Claude reads over MCP too), and every
//! action is a command (`script.load`, `script.copyRef`, `script.board`, `word.apply`,
//! `text.apply`, `layer.select`, `time.set`, `speech.transcribe`).
//!
//! - Load or drop a plain-text script. Words not said are struck out, ad-libs green.
//! - Teleprompter: a click on a line (not on a word) moves the playhead there; while it plays the
//!   word being said lights up.
//! - A click on a word opens the word wheel (`script.word`): Pop text, Caption, Pill, Highlight,
//!   Zoom punch, SFX, Copy for Claude, Preset…; the item's peak lands on the stressed syllable.
//! - A drag across words opens the phrase popover (floating, it never moves the lines): the split
//!   button "Caption · <pick>" with the Motion Library text animations (`text.presets`) and
//!   "Preset… (Motion Library)", the add-on toggles sync / +glow / near·mid·far (SX6), Views
//!   Counter when the phrase has a number, Copy for Claude.
//! - Emphasis glow (brighter = said harder), a gap bar where the voice runs on with no animation,
//!   coverage chips under each line (a click selects those layers), a line's context menu (Copy
//!   for Claude, Go to line, Board this line), and Board this (FigCraft) for the ticked lines.
//!
//! Nothing here inserts rows above the lines: popovers are `egui::Area`s anchored at the words.
//! The panel's own state (ticks, picks, toggles) is serde, kept in the egui context.
//!
//! Automation ids: `script.load`, `script.board`, `script.transcribe.<layer>`, `script.line.<n>`,
//! `script.line.<n>.check`, `script.word.<index>`, `script.chip.<n>.<layer>`, `script.phrase.*`.

use egui::{Color32, RichText, Sense, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::wheel::{self, WheelItem};

/// The word wheel's key (`wheel.script.word.<item>`).
const WHEEL: &str = "script.word";
const ADLIB: Color32 = Color32::from_rgb(0x5f, 0xd0, 0x8a);
const NOW: Color32 = Color32::from_rgb(0x2d, 0x8c, 0xff);
const PHRASE: Color32 = Color32::from_rgb(0x3a, 0x34, 0x10);
const GAP: Color32 = Color32::from_rgb(0xe0, 0x5a, 0x4a);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PanelState {
    /// Ticked lines (1-based), for Board this.
    checked: Vec<u64>,
    /// A drag across words in progress: its first word (speech-map index).
    drag_from: Option<usize>,
    drag_to: Option<usize>,
    /// The phrase popover: word range and where it is anchored (screen point).
    phrase: Option<(usize, usize, [f32; 2])>,
    /// The phrase popover shows the Motion Library preset list.
    preset_list: bool,
    /// The word the wheel was opened on.
    wheel_word: Option<usize>,
    /// The text animation the phrase button applies (`text.presets` short name, "T3b").
    pick: Option<String>,
    /// SX6 add-ons.
    sync: bool,
    glow: bool,
    distance: String,
    /// `script.get` cache: (comp, revision, result).
    #[serde(skip)]
    cache: Option<(u64, u64, Value)>,
    /// `text.presets` (short name, label) and Motion Library presets (id, name), read once.
    #[serde(skip)]
    text_presets: Option<Vec<(String, String)>>,
    #[serde(skip)]
    library: Option<Vec<(String, String)>>,
}

impl Default for PanelState {
    fn default() -> Self {
        PanelState {
            checked: vec![],
            drag_from: None,
            drag_to: None,
            phrase: None,
            preset_list: false,
            wheel_word: None,
            pick: None,
            sync: true,
            glow: false,
            distance: "mid".into(),
            cache: None,
            text_presets: None,
            library: None,
        }
    }
}

fn state_id() -> egui::Id {
    egui::Id::new("script-panel-state")
}

fn word_items() -> Vec<WheelItem> {
    vec![
        WheelItem::new("pop", "Pop text", "Pops the word in, its peak on the stressed syllable"),
        WheelItem::new("caption", "Caption", "A caption in your Captions style"),
        WheelItem::new("pill", "Pill", "A pill on the word"),
        WheelItem::new("highlight", "Highlight", "Highlights the word"),
        WheelItem::new("zoom", "Zoom punch", "Punches in on the selected layers on the word"),
        WheelItem::new("sfx", "SFX", "Marks an SFX hit on the word"),
        WheelItem::new("copy", "Copy for Claude", "Copies a reference to paste in the chat"),
        WheelItem::new("preset", "Preset…", "A Motion Library preset, adapted to the word"),
    ]
}

/// Text animations from `text.presets` (lane D): every object with a short name (`short`,
/// `preset` or `id`) and a `name`, wherever it sits in the result.
/// TODO(batch1-verify): text.presets' result shape (lane D); the old text_anim `text.presets` id
/// is the same id.
fn collect_presets(v: &Value, out: &mut Vec<(String, String)>, depth: usize) {
    if depth > 6 || out.len() >= 64 {
        return;
    }
    match v {
        Value::Object(o) => {
            let short = ["short", "preset", "id"].iter().find_map(|k| o.get(*k).and_then(Value::as_str));
            let name = o.get("name").and_then(Value::as_str);
            if let (Some(s), Some(n)) = (short, name)
                && !o.contains_key("variants")
            {
                out.push((s.to_string(), n.to_string()));
            }
            for x in o.values() {
                collect_presets(x, out, depth + 1);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_presets(x, out, depth + 1)),
        _ => {}
    }
}

pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: egui::Rect) {
    let ctx = ui.ctx().clone();
    let t = app.tokens;
    ui.painter().rect_filled(rect, 0.0, t.panel_bg);
    let mut st: PanelState = ctx.data_mut(|d| d.get_temp(state_id())).unwrap_or_default();
    let mut actions: Vec<(String, Value)> = vec![];
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(8.0)));
    body(app, &mut child, &ctx, &mut st, &mut actions);
    // The word wheel's pick.
    if let Some(item) = wheel::take_pick(&ctx, WHEEL)
        && let Some(i) = st.wheel_word.take()
    {
        match item.as_str() {
            "preset" => {
                let at = ctx.pointer_latest_pos().unwrap_or(rect.center());
                st.phrase = Some((i, i, [at.x, at.y]));
                st.preset_list = true;
            }
            "copy" => actions.push(("script.copyRef".into(), json!({"from": i, "to": i}))),
            other => actions.push(("word.apply".into(), json!({"index": i, "item": other}))),
        }
    }
    phrase_popover(app, &ctx, &mut st, &mut actions);
    for (id, p) in actions {
        if let Err(e) = crate::menus::invoke(app, &ctx, &id, p) {
            app.ui.status = e;
        }
        // Anything run may change what the script shows.
        st.cache = None;
    }
    ctx.data_mut(|d| d.insert_temp(state_id(), st));
}

fn body(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, st: &mut PanelState, actions: &mut Vec<(String, Value)>) {
    let t = app.tokens;
    let Some(cid) = app.session.active_comp_id() else {
        ui.label(RichText::new("Open a composition with a voice-over.").color(t.text_faint));
        return;
    };
    let Some(comp) = app.session.project.comp_arc(cid) else { return };
    let map = comp.speech.clone();
    let has_words = map.as_ref().is_some_and(|m| !m.words.is_empty());
    // No words yet: offer to transcribe the layers with sound (SX2).
    if !has_words {
        ui.label(RichText::new("No words yet. Transcribe the voice layer (ElevenLabs Scribe, aligned for stressed syllables):").color(t.text_dim));
        for l in comp.layers.iter() {
            let effectcraft_engine::project::LayerSource::Footage { item } = l.source else { continue };
            let sound = matches!(app.session.project.item(item).map(|i| &i.kind), Some(effectcraft_engine::project::ItemKind::Footage(f)) if f.has_audio);
            if !sound {
                continue;
            }
            let label = match crate::transcribe::running(l.id.0) {
                Some(s) => format!("Transcribing {}… {s:.0} s", l.name),
                None => format!("Transcribe {}", l.name),
            };
            let r = ui.add_enabled(crate::transcribe::running(l.id.0).is_none(), egui::Button::new(label));
            app.auto.add(&format!("script.transcribe.{}", l.id.0), r.rect, "Transcribe");
            if r.clicked() {
                actions.push(("speech.transcribe".into(), json!({"layer": l.id.0})));
            }
        }
    }
    // Script files dropped on the panel (handled here, not imported as footage).
    let over = ui.rect_contains_pointer(ui.max_rect());
    if over {
        let dropped: Vec<String> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                // egui 0.36: `path()` (as prefs_live reads it); empty for drops without a local file.
                .map(|f| f.path().to_string_lossy().to_string())
                .filter(|p| !p.is_empty())
                .filter(|p| {
                    let l = p.to_ascii_lowercase();
                    l.ends_with(".txt") || l.ends_with(".md") || l.ends_with(".docx")
                })
                .collect()
        });
        if let Some(path) = dropped.first() {
            actions.push(("script.load".into(), json!({"path": path})));
            let taken = dropped.clone();
            ctx.input_mut(|i| i.raw.dropped_files.retain(|f| !taken.contains(&f.path().to_string_lossy().to_string())));
        }
    }
    let has_script = map.as_ref().is_some_and(|m| m.script.is_some());
    if !has_script {
        ui.add_space(8.0);
        let r = ui.add_sized(
            vec2(ui.available_width(), 80.0),
            egui::Button::new(RichText::new("Drop your script here  ·  .txt\nor click to load one").color(t.text)),
        );
        app.auto.add("script.load", r.rect, "Load script");
        if r.clicked() {
            actions.push(("script.load".into(), json!({})));
        }
        return;
    }
    if !has_words {
        return;
    }
    // What the tab shows = script.get (cached per project revision).
    let rev = app.session.revision;
    let fresh = st.cache.as_ref().is_some_and(|(c, r, _)| *c == cid.0 && *r == rev);
    if !fresh {
        match app.session.execute("script.get", json!({"comp": cid.0})) {
            Ok(v) => st.cache = Some((cid.0, rev, v)),
            Err(e) => {
                ui.label(RichText::new(e.to_string()).color(t.danger));
                return;
            }
        }
    }
    let Some((_, _, data)) = st.cache.clone() else { return };
    let Some(map) = map else { return };
    let now = app.session.time();
    let playing = app.playback.playing;
    // Header.
    ui.horizontal(|ui| {
        let name = data.get("name").and_then(Value::as_str).unwrap_or("Script");
        ui.label(RichText::new(name).strong().color(t.text));
        let n = |k: &str| data.get(k).and_then(Value::as_u64).unwrap_or(0);
        ui.label(RichText::new(format!("{} not said · {} ad-libs · {} lines cut", n("notSaid"), n("adlibs"), n("cut"))).color(t.text_dim).size(11.0));
    });
    ui.horizontal(|ui| {
        let ticked = st.checked.len();
        let b = ui.add_enabled(ticked > 0, egui::Button::new("Board this (FigCraft)")).on_hover_text("Tick lines first: a storyboard of them goes to FigCraft");
        app.auto.add("script.board", b.rect, "Board this (FigCraft)");
        if b.clicked() {
            actions.push(("script.board".into(), json!({"lines": st.checked})));
        }
        let l = ui.button("Load…");
        app.auto.add("script.reload", l.rect, "Load another script");
        if l.clicked() {
            actions.push(("script.load".into(), json!({})));
        }
    });
    ui.separator();
    let lines = data.get("lines").and_then(Value::as_array).cloned().unwrap_or_default();
    let ptr = ctx.pointer_latest_pos();
    let released = ctx.input(|i| i.pointer.any_released());
    let (pa, pb) = match (st.drag_from, st.drag_to, st.phrase) {
        (Some(a), Some(b), _) => (a.min(b), a.max(b)),
        (_, _, Some((a, b, _))) => (a, b),
        _ => (usize::MAX, usize::MAX),
    };
    egui::ScrollArea::vertical().id_salt("script-lines").auto_shrink([false, false]).show(ui, |ui| {
        for line in &lines {
            let n = line.get("line").and_then(Value::as_u64).unwrap_or(0);
            let cut = line.get("cut").and_then(Value::as_bool).unwrap_or(false);
            let gap = line.get("gap").and_then(Value::as_bool).unwrap_or(false);
            let first = line.get("frames").and_then(|f| f.get(0)).and_then(Value::as_i64);
            let row_top = ui.cursor().min;
            // The line's background, under its words (registered first: the words stay on top),
            // from where the line was last frame: a click goes there (teleprompter), right-click
            // opens its menu.
            let rect_id = egui::Id::new(("script-line-rect", n));
            let prev: Option<egui::Rect> = ctx.data(|d| d.get_temp(rect_id));
            if let Some(row) = prev.map(|r| r.translate(row_top - r.min)) {
                let bg = ui.interact(row, egui::Id::new(("script-line", n)), Sense::click());
                app.auto.add(&format!("script.line.{n}"), row, &format!("Script line {n}"));
                if bg.clicked()
                    && let Some(f) = first
                {
                    actions.push(("time.set".into(), json!({"frame": f})));
                }
                crate::widgets::context_menu(&bg, |ui| {
                    if ui.button("Copy for Claude").clicked() {
                        actions.push(("script.copyRef".into(), json!({"line": n})));
                        ui.close();
                    }
                    if let Some(f) = first
                        && ui.button("Go to line").clicked()
                    {
                        actions.push(("time.set".into(), json!({"frame": f})));
                        ui.close();
                    }
                    if !cut && ui.button("Board this line (FigCraft)").clicked() {
                        actions.push(("script.board".into(), json!({"lines": [n]})));
                        ui.close();
                    }
                });
            }
            let resp = ui
                .horizontal_wrapped(|ui| {
                    let mut ck = st.checked.contains(&n);
                    let c = ui.checkbox(&mut ck, "");
                    app.auto.add(&format!("script.line.{n}.check"), c.rect, "Tick line");
                    if c.changed() {
                        if ck {
                            st.checked.push(n);
                        } else {
                            st.checked.retain(|x| *x != n);
                        }
                    }
                    if gap {
                        let (r, g) = ui.allocate_exact_size(vec2(4.0, 16.0), Sense::hover());
                        ui.painter().rect_filled(r, 1.0, GAP);
                        g.on_hover_text("No animation here for a while: a pattern interrupt every 10–15 s keeps people watching");
                    }
                    for tok in line.get("tokens").and_then(Value::as_array).into_iter().flatten() {
                        let text = tok.get("text").and_then(Value::as_str).unwrap_or("");
                        let said = tok.get("said").and_then(Value::as_u64).map(|x| x as usize);
                        let mut rt = RichText::new(text).size(14.0);
                        if tok.get("notSaid").and_then(Value::as_bool).unwrap_or(false) {
                            rt = rt.strikethrough().color(t.text_faint);
                        } else if tok.get("adlib").and_then(Value::as_bool).unwrap_or(false) {
                            rt = rt.color(ADLIB);
                        } else {
                            // Emphasis glow: brighter and warmer the harder it is said.
                            let e = tok.get("emphasis").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                            let warm = Color32::from_rgb(255, (200.0 - e * 150.0) as u8, 80);
                            rt = rt.color(lerp(t.text_dim, warm, e));
                            if e > 0.5 {
                                rt = rt.strong();
                            }
                        }
                        let Some(i) = said else {
                            ui.label(rt);
                            continue;
                        };
                        let is_now = map.words.get(i).is_some_and(|w| now >= w.start && now < w.end);
                        if is_now {
                            rt = rt.underline().color(NOW);
                        }
                        if i >= pa && i <= pb {
                            rt = rt.background_color(PHRASE);
                        }
                        let r = ui.add(egui::Label::new(rt).sense(Sense::click_and_drag()));
                        app.auto.add(&format!("script.word.{i}"), r.rect, text);
                        if is_now && playing {
                            r.scroll_to_me(Some(egui::Align::Center));
                        }
                        if r.clicked() {
                            st.wheel_word = Some(i);
                            st.phrase = None;
                            let at = r.interact_pointer_pos().unwrap_or(r.rect.center());
                            wheel::open(ctx, WHEEL, at, text, word_items(), None);
                        }
                        if r.drag_started() {
                            st.drag_from = Some(i);
                            st.drag_to = Some(i);
                            st.phrase = None;
                        }
                        if st.drag_from.is_some() && ptr.is_some_and(|p| r.rect.contains(p)) {
                            st.drag_to = Some(i);
                        }
                    }
                    let span = match (first, line.get("frames").and_then(|f| f.get(1)).and_then(Value::as_i64)) {
                        (Some(a), Some(b)) => format!("f{a}–f{b}"),
                        _ if cut => "not in the cut".into(),
                        _ => String::new(),
                    };
                    ui.label(RichText::new(span).size(10.5).color(t.text_faint));
                })
                .response;
            // Coverage chips: the layers on screen during the line.
            let chips: Vec<(u64, String)> = line
                .get("coverage")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| Some((c.get("layer")?.as_u64()?, c.get("name")?.as_str()?.to_string())))
                .collect();
            if !chips.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(22.0);
                    for (lid, name) in &chips {
                        let b = ui.add(egui::Button::new(RichText::new(name).size(10.5)).small());
                        app.auto.add(&format!("script.chip.{n}.{lid}"), b.rect, name);
                        if b.clicked() {
                            actions.push(("layer.select".into(), json!({"layers": [lid]})));
                        }
                    }
                });
            }
            let row = egui::Rect::from_min_max(row_top, egui::pos2(ui.max_rect().max.x, resp.rect.max.y));
            ctx.data_mut(|d| d.insert_temp(rect_id, row));
            ui.add_space(6.0);
        }
    });
    // A drag across words ends: the phrase popover opens where it ended.
    if released
        && let (Some(a), Some(b)) = (st.drag_from.take(), st.drag_to.take())
        && a != b
    {
        let at = ptr.unwrap_or(ui.max_rect().center());
        st.phrase = Some((a.min(b), a.max(b), [at.x, at.y + 8.0]));
        st.preset_list = false;
    }
    if playing {
        ctx.request_repaint();
    }
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// The phrase popover (SX5 + SX6): floating at the selection, never in the layout.
fn phrase_popover(app: &mut EffectcraftApp, ctx: &egui::Context, st: &mut PanelState, actions: &mut Vec<(String, Value)>) {
    let Some((a, b, at)) = st.phrase else { return };
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        st.phrase = None;
        return;
    }
    let Some(cid) = app.session.active_comp_id() else { return };
    let Some(comp) = app.session.project.comp_arc(cid) else { return };
    let Some(map) = comp.speech.clone() else { return };
    let Some(words) = map.words.get(a..=b) else {
        st.phrase = None;
        return;
    };
    let text: String = words.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ");
    let stressed = words.iter().find(|w| w.stressed().is_some()).map(|w| w.word.clone());
    let has_number = text.chars().any(|c| c.is_ascii_digit());
    if st.text_presets.is_none() {
        let mut v = vec![];
        if let Ok(r) = app.session.execute("text.presets", json!({})) {
            collect_presets(&r, &mut v, 0);
        }
        st.text_presets = Some(v);
    }
    if st.preset_list && st.library.is_none() {
        // TODO(batch1-verify): library.search's result shape ({entries: [{id, name}]}).
        let r = app.session.execute("library.search", json!({"query": "text", "limit": 24})).unwrap_or(Value::Null);
        let list = r
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e.get("id")?.as_str()?.to_string(), e.get("name")?.as_str()?.to_string())))
            .collect();
        st.library = Some(list);
    }
    let t = app.tokens;
    let mut close = false;
    let area = egui::Area::new(egui::Id::new("script-phrase-popover")).order(egui::Order::Foreground).fixed_pos(egui::pos2(at[0], at[1])).constrain(true).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(420.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("“{text}”")).strong().color(t.text));
                    let x = ui.small_button("✕");
                    app.auto.add("script.phrase.close", x.rect, "Close");
                    if x.clicked() {
                        close = true;
                    }
                });
                // The split button: Caption · <pick> ▾.
                ui.horizontal(|ui| {
                    let pick_label = st.pick.clone().unwrap_or_else(|| "plain".into());
                    let main = ui.button(RichText::new(format!("Caption · {pick_label}")).strong());
                    app.auto.add("script.phrase.caption", main.rect, "Caption this");
                    if main.clicked() {
                        match &st.pick {
                            // TODO(batch1-verify): text.apply's params (lane D): `words` as the phrase's text,
                            // `from`/`to` speech-map indices not in its pinned list.
                            Some(p) => actions.push((
                                "text.apply".into(),
                                json!({"preset": p, "words": text, "word": stressed, "sync": st.sync, "glow": st.glow, "distance": st.distance}),
                            )),
                            None => actions.push(("word.apply".into(), json!({"from": a, "to": b, "item": "caption"}))),
                        }
                        close = true;
                    }
                    let arrow = ui.button("▾");
                    app.auto.add("script.phrase.pick", arrow.rect, "Pick the text animation");
                    let open_id = egui::Id::new("script-phrase-pick-open");
                    let mut open: bool = ctx.data(|d| d.get_temp(open_id)).unwrap_or(false);
                    if arrow.clicked() {
                        open = !open;
                    }
                    if arrow.hovered() {
                        open = true;
                    }
                    ctx.data_mut(|d| d.insert_temp(open_id, open));
                    if open {
                        let below = arrow.rect.left_bottom() + vec2(0.0, 2.0);
                        egui::Area::new(egui::Id::new("script-phrase-pick-list")).order(egui::Order::Tooltip).fixed_pos(below).constrain(true).show(
                            ctx,
                            |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    let presets = st.text_presets.clone().unwrap_or_default();
                                    if presets.is_empty() {
                                        ui.label(RichText::new("No text animations in Motion Library ▸ Elias ▸ Text animations yet").color(t.text_faint));
                                    }
                                    for (short, name) in presets {
                                        let on = st.pick.as_deref() == Some(short.as_str());
                                        let r = ui.selectable_label(on, format!("{short}  {name}"));
                                        app.auto.add(&format!("script.phrase.pick.{short}"), r.rect, &name);
                                        if r.clicked() {
                                            st.pick = Some(short.clone());
                                            ctx.data_mut(|d| d.insert_temp(open_id, false));
                                        }
                                    }
                                    ui.separator();
                                    let r = ui.selectable_label(false, "Preset… (Motion Library)");
                                    app.auto.add("script.phrase.pick.library", r.rect, "Preset… (Motion Library)");
                                    if r.clicked() {
                                        st.preset_list = true;
                                        ctx.data_mut(|d| d.insert_temp(open_id, false));
                                    }
                                });
                            },
                        );
                    }
                });
                // SX6 add-on toggles.
                ui.horizontal(|ui| {
                    let s = ui.selectable_label(st.sync, "sync").on_hover_text("The animation's speed peak lands on the stressed syllable");
                    app.auto.add("script.phrase.sync", s.rect, "sync");
                    if s.clicked() {
                        st.sync = !st.sync;
                    }
                    let g = ui.selectable_label(st.glow, "+glow").on_hover_text("Also Caption glow + double shadow");
                    app.auto.add("script.phrase.glow", g.rect, "+glow");
                    if g.clicked() {
                        st.glow = !st.glow;
                    }
                    ui.add_space(6.0);
                    for d in ["near", "mid", "far"] {
                        let r = ui.selectable_label(st.distance == d, d);
                        app.auto.add(&format!("script.phrase.distance.{d}"), r.rect, d);
                        if r.clicked() {
                            st.distance = d.to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if has_number {
                        let v = ui.button("Views Counter").on_hover_text("The Motion Library Views Counter scene, counting to this number");
                        app.auto.add("script.phrase.views", v.rect, "Views Counter");
                        if v.clicked() {
                            actions.push(("word.apply".into(), json!({"from": a, "to": b, "item": "preset", "query": "Views Counter"})));
                            close = true;
                        }
                    }
                    let c = ui.button("Copy for Claude");
                    app.auto.add("script.phrase.copy", c.rect, "Copy for Claude");
                    if c.clicked() {
                        actions.push(("script.copyRef".into(), json!({"from": a, "to": b})));
                        close = true;
                    }
                });
                if st.preset_list {
                    ui.separator();
                    ui.label(RichText::new("Motion Library preset, adapted to the phrase:").color(t.text_dim).size(11.0));
                    let list = st.library.clone().unwrap_or_default();
                    if list.is_empty() {
                        ui.label(RichText::new("No presets found").color(t.text_faint));
                    }
                    for (id, name) in list {
                        let r = ui.button(&name);
                        app.auto.add(&format!("script.phrase.preset.{id}"), r.rect, &name);
                        if r.clicked() {
                            actions.push(("word.apply".into(), json!({"from": a, "to": b, "item": "preset", "id": id})));
                            close = true;
                        }
                    }
                }
            });
        },
    );
    // A click elsewhere closes it.
    // (The pick list is an area of its own, above: a press there isn't outside.)
    let clicked_out = ctx.input(|i| i.pointer.any_pressed())
        && ctx.pointer_latest_pos().is_some_and(|p| {
            !area.response.rect.expand(8.0).contains(p)
                && !ctx.layer_id_at(p).is_some_and(|l| matches!(l.order, egui::Order::Foreground | egui::Order::Tooltip))
        });
    if close || clicked_out {
        st.phrase = None;
        st.preset_list = false;
    }
}
