//! Motion Library ▸ Tools (the rail's Tools button): the web panel's EffectCraft toolkit in the
//! native panel — Workflow, Text Split, Bounce and Colour.
//!
//! - **Native** where EffectCraft has the same command (no Python, one undo step each): new null /
//!   solid / adjustment layer (`layer.newNull`, `layer.newSolid`, `layer.newAdjustment`),
//!   Precompose (`layer.smartPrecompose`), Precomp groups (`layer.multiPrecompose`),
//!   Un-precompose (`layer.decompose`), Deep duplicate (`project.deepDuplicate`), Gradient lock
//!   (`shape.gradientLock`), Purge previews (`edit.purge` memory) and Text Split (`text.split`).
//! - **The Motion Library's EffectCraft plug** (`effectcraft_plug.py`, `MLMotionToolkit` in
//!   `motion-toolkit.jsx`, through this app's control port) for Fill, the nine anchor positions,
//!   Bounce, Colour and Paste image. These run on a background thread, one action at a time; the
//!   feedback line and the status bar say how it went.
//!
//! Sections collapse independently, the row of section names jumps to one, and wide docks show
//! two columns. The settings ([`ToolSettings`]) stay while the app runs; `motionLibrary.tool`
//! (`{op: "settings", …}`) reads and changes them, and runs every tool (`{tool, …args}`).

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use egui::{Align2, CornerRadius, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::motion_library::{ViewState, control_port, plug_rpc};
use super::panel_kit as kit;
use crate::i18n::tr;
use crate::theme::Tokens;
use crate::{EffectcraftApp, widgets};

/// The sections: key (automation ids, `section` params), name, tip.
pub const SECTIONS: [(&str, &str, &str); 4] = [
    ("workflow", "Workflow", "Create, organise and reuse your selected layers."),
    ("text", "Text Split", "Split selected text into clean editable layers. Shaped clusters stay together."),
    ("bounce", "Bounce", "Add a spring after the last keyframe. Your original keyframes stay editable."),
    ("colour", "Colour", "Native gradients and an animated light sweep on selected layers."),
];
const WORKFLOW: usize = 0;
const TEXT: usize = 1;
const BOUNCE: usize = 2;
const COLOUR: usize = 3;

/// The nine anchor positions: automation name, description (x = i % 3 / 2, y = i / 3 / 2).
const ANCHORS: [(&str, &str); 9] = [
    ("topLeft", "top left"),
    ("top", "top centre"),
    ("topRight", "top right"),
    ("left", "centre left"),
    ("center", "centre"),
    ("right", "centre right"),
    ("bottomLeft", "bottom left"),
    ("bottom", "bottom centre"),
    ("bottomRight", "bottom right"),
];
const SPLIT_MODES: [(&str, &str); 4] = [("words", "Words"), ("characters", "Characters / clusters"), ("lines", "Lines"), ("custom", "Isolate word or phrase")];
const PROPERTIES: [(&str, &str); 4] = [("position", "Position"), ("scale", "Scale"), ("rotation", "Rotation"), ("opacity", "Opacity")];
/// Bounce targets: the dropdown's properties, or `selected` (the properties with selected keys).
const TARGETS: [(&str, &str); 5] =
    [("selected", "Selected keys"), ("position", "Position"), ("scale", "Scale"), ("rotation", "Rotation"), ("opacity", "Opacity")];
const MOTIONS: [(&str, &str); 2] = [("overshoot", "Overshoot / elastic"), ("bounce", "Bounce back")];
const STYLES: [(&str, &str); 3] = [("ramp", "Ramp gradient"), ("four", "4-colour gradient"), ("sweep", "Light sweep")];
const DEFAULT_COLORS: [&str; 4] = ["#1c8cff", "#8b5cf6", "#ff9640", "#22d3ee"];
/// Numeric bounds, as `motion-toolkit.jsx` checks them (count: the web panel's Group size).
const STRENGTH: (f64, f64) = (0.0, 200.0);
const FREQUENCY: (f64, f64) = (0.1, 20.0);
const DECAY: (f64, f64) = (0.1, 30.0);
const DURATION: (f64, f64) = (0.05, 120.0);
const COUNT: (f64, f64) = (1.0, 256.0);
/// Paste image: PNG up to 5 MB (the plug's limit).
const MAX_PNG: u64 = 5_000_000;

/// The tools' inputs, kept while the app runs (`motionLibrary.tool {op: "settings"}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolSettings {
    /// Workflow: solid / fill colour (`#rrggbb`).
    pub color: String,
    /// Workflow: precomp name.
    pub name: String,
    /// Workflow: layers per precomp for Precomp groups.
    pub count: u32,
    /// Text Split: words, characters, lines or custom.
    pub split_mode: String,
    /// Text Split: the word or phrase to isolate (custom).
    pub split_text: String,
    pub reverse: bool,
    pub delete_original: bool,
    /// Bounce: position, scale, rotation or opacity.
    pub property: String,
    /// Bounce: overshoot or bounce.
    pub motion: String,
    /// Bounce: strength (%), bounces per second, settle speed.
    pub strength: f64,
    pub frequency: f64,
    pub decay: f64,
    /// Colour: the four colours (`#rrggbb`).
    pub colors: [String; 4],
    /// Colour: animation duration (seconds) and whether gradients animate.
    pub duration: f64,
    pub animate: bool,
    /// Collapsed sections (keys of [`SECTIONS`]).
    pub collapsed: Vec<String>,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            color: "#ffffff".into(),
            name: "Precomp".into(),
            count: 1,
            split_mode: "words".into(),
            split_text: String::new(),
            reverse: false,
            delete_original: false,
            property: "position".into(),
            motion: "overshoot".into(),
            strength: 20.0,
            frequency: 3.0,
            decay: 5.0,
            colors: DEFAULT_COLORS.map(String::from),
            duration: 2.0,
            animate: false,
            // As the web panel: Workflow open, the others folded.
            collapsed: vec!["text".into(), "bounce".into(), "colour".into()],
        }
    }
}

impl ToolSettings {
    fn is_collapsed(&self, i: usize) -> bool {
        SECTIONS.get(i).is_some_and(|(k, _, _)| self.collapsed.iter().any(|c| c == k))
    }

    fn set_collapsed(&mut self, i: usize, collapsed: bool) {
        let Some((key, _, _)) = SECTIONS.get(i) else { return };
        self.collapsed.retain(|c| c != key);
        if collapsed {
            self.collapsed.push(key.to_string());
        }
    }

    /// Check every input (the same bounds as the toolkit script); colours become `#rrggbb`.
    fn validate(&mut self) -> Result<(), String> {
        self.color = hex6(&self.color).ok_or("`color`: choose a six-digit colour (#rrggbb)")?;
        for (i, c) in self.colors.iter_mut().enumerate() {
            *c = hex6(c).ok_or_else(|| format!("`colors[{i}]`: choose a six-digit colour (#rrggbb)"))?;
        }
        if self.name.chars().count() > 255 {
            return Err("`name`: at most 255 characters".into());
        }
        in_range("count", f64::from(self.count), COUNT)?;
        one_of("splitMode", &self.split_mode, &SPLIT_MODES)?;
        if self.split_text.len() > 1024 {
            return Err("`splitText`: at most 1024 bytes".into());
        }
        one_of("property", &self.property, &PROPERTIES)?;
        one_of("motion", &self.motion, &MOTIONS)?;
        in_range("strength", self.strength, STRENGTH)?;
        in_range("frequency", self.frequency, FREQUENCY)?;
        in_range("decay", self.decay, DECAY)?;
        in_range("duration", self.duration, DURATION)?;
        if let Some(c) = self.collapsed.iter().find(|c| !SECTIONS.iter().any(|(k, _, _)| k == c)) {
            return Err(format!("`collapsed`: unknown section `{c}` (workflow, text, bounce, colour)"));
        }
        Ok(())
    }
}

/// `value` in `[lo, hi]` (and finite), else the toolkit's message.
fn in_range(name: &str, value: f64, (lo, hi): (f64, f64)) -> Result<f64, String> {
    if value.is_finite() && value >= lo && value <= hi { Ok(value) } else { Err(format!("{name} must be between {lo} and {hi}.")) }
}

fn one_of(name: &str, value: &str, options: &[(&str, &str)]) -> Result<(), String> {
    if options.iter().any(|(k, _)| *k == value) {
        Ok(())
    } else {
        Err(format!("`{name}`: unknown value `{value}` ({})", options.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")))
    }
}

/// A colour as `#rrggbb` (accepts `#RGB`, `RRGGBB`…), `None` when it isn't one.
fn hex6(s: &str) -> Option<String> {
    widgets::hex_rgb(s).map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
}

/// A `#rrggbb` colour as sRGB components (white when malformed).
fn rgb_of(hex: &str) -> [f32; 3] {
    widgets::hex_rgb(hex).map_or([1.0; 3], |c| c.map(|v| f32::from(v) / 255.0))
}

/// What a finished action said.
#[derive(Clone, Debug)]
struct Feedback {
    text: String,
    error: bool,
}

/// The workspace's state between frames, shared with the background actions.
#[derive(Default)]
struct Shared {
    settings: Option<ToolSettings>,
    /// The action running in the background (its label).
    busy: Option<String>,
    feedback: Option<Feedback>,
    /// The last action's result: `{tool, ok, result | error}`.
    last: Option<Value>,
    /// The feedback is new: copy it to the status bar on the next frame.
    status_pending: bool,
    /// A section to scroll to on the next frame.
    jump: Option<usize>,
    /// The section last jumped to (the nav row shows it).
    focus: usize,
    scroll: f32,
    /// Each section's height last frame (points, unscaled by nothing: as drawn).
    heights: [f32; 4],
}

fn shared() -> MutexGuard<'static, Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default())).lock().unwrap_or_else(PoisonError::into_inner)
}

fn settings() -> ToolSettings {
    shared().settings.clone().unwrap_or_default()
}

/// The message to show for a result (the web panel drops the undo / keyframe reminders).
fn result_message(r: &Value) -> String {
    let m = r.get("message").and_then(Value::as_str).unwrap_or("Done.");
    m.replace(" Undo with Cmd+Z.", "").replace(" Original keyframes kept.", "")
}

/// Record a finished action (`tool`) for the feedback line, the status bar and `{op: "status"}`.
fn record(tool: &str, r: &Result<Value, String>) {
    let mut sh = shared();
    let (feedback, last) = match r {
        Ok(v) => (Feedback { text: result_message(v), error: false }, json!({"tool": tool, "ok": true, "result": v})),
        Err(e) => (Feedback { text: e.clone(), error: true }, json!({"tool": tool, "ok": false, "error": e})),
    };
    sh.feedback = Some(feedback);
    sh.last = Some(last);
    sh.status_pending = true;
}

// ── Tools ──────────────────────────────────────────────────────────────────────────────────────

/// Where Paste image takes its PNG.
#[derive(Clone, Debug, PartialEq)]
enum ImageSource {
    /// The system clipboard (macOS; a file dialog elsewhere).
    Clipboard,
    /// A file dialog.
    Dialog,
    Path(String),
    /// Base64 PNG data.
    Png(String),
}

/// How a tool runs.
#[derive(Clone, Debug, PartialEq)]
enum Plan {
    /// An EffectCraft command, right away.
    Native {
        cmd: &'static str,
        args: Value,
    },
    /// A plug operation, in the background.
    Plug {
        op: &'static str,
        args: Value,
    },
    Image(ImageSource),
}

/// A tool ready to run: how, its section and a label for the busy line.
#[derive(Clone, Debug, PartialEq)]
struct Built {
    plan: Plan,
    section: usize,
    label: String,
}

fn arg_str(p: &Value, key: &str, default: &str) -> Result<String, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("`{key}` is a string")),
    }
}

fn arg_num(p: &Value, key: &str, default: f64, range: (f64, f64)) -> Result<f64, String> {
    let v = match p.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => v.as_f64().ok_or_else(|| format!("`{key}` is a number"))?,
    };
    in_range(key, v, range)
}

fn arg_bool(p: &Value, key: &str, default: bool) -> Result<bool, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| format!("`{key}` is true or false")),
    }
}

fn arg_color(p: &Value, key: &str, default: &str) -> Result<String, String> {
    let s = arg_str(p, key, default)?;
    hex6(&s).ok_or_else(|| format!("`{key}`: choose a six-digit colour (#rrggbb), not `{s}`"))
}

fn arg_choice(p: &Value, key: &str, default: &str, options: &[(&str, &str)]) -> Result<String, String> {
    let s = arg_str(p, key, default)?;
    one_of(key, &s, options)?;
    Ok(s)
}

/// The names of the properties with selected keyframes in the active comp, in selection order.
/// Bounce springs these rather than the Property dropdown, as After Effects tools act on the
/// selected keys.
fn keyed_props(s: &effectcraft_engine::Session) -> Vec<String> {
    let Some(c) = s.active_comp() else { return vec![] };
    let mut names: Vec<String> = vec![];
    for k in &s.state.selected_keys {
        if let Some(pr) = c.layer(k.layer).and_then(|l| l.props.find(k.prop))
            && !names.contains(&pr.name)
        {
            names.push(pr.name.clone());
        }
    }
    names
}

/// Whether a selected key is on a text Range Selector's Start, End or Offset (CX9): a spring there
/// shows nothing (Ramp Up clamps the selection to 0..1), so Bounce springs each unit instead
/// (`text.unitBounce`).
fn reveal_keys(s: &effectcraft_engine::Session) -> bool {
    let Some(c) = s.active_comp() else { return false };
    s.state.selected_keys.iter().any(|k| {
        c.layer(k.layer).and_then(|l| l.props.match_path_of(k.prop)).is_some_and(|m| is_range_reveal(&m))
    })
}

/// `text/animators/animator#2/selectors/rangeSelector/offset` and the like
/// (the engine's `styles::is_range_reveal_path`, which the UI can't reach).
fn is_range_reveal(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    let base = |s: &str| s.split('#').next().unwrap_or("").to_string();
    let n = segs.len();
    n >= 2
        && segs.first().copied() == Some("text")
        && segs.get(n - 2).is_some_and(|s| base(s) == "rangeSelector")
        && segs.get(n - 1).is_some_and(|s| matches!(base(s).as_str(), "start" | "end" | "offset"))
}

/// The tool `tool` with the arguments in `p`; missing arguments take the panel's current values.
/// `keyed` names the properties with selected keyframes ([`keyed_props`]): when there are any,
/// Bounce without a `property` springs those keys instead of the dropdown's property.
fn build(tool: &str, p: &Value, set: &ToolSettings, keyed: &[String]) -> Result<Built, String> {
    let native = |cmd: &'static str, args: Value, label: &str| Built { plan: Plan::Native { cmd, args }, section: WORKFLOW, label: label.to_string() };
    let plug = |op: &'static str, args: Value, section: usize, label: &str| Built { plan: Plan::Plug { op, args }, section, label: label.to_string() };
    Ok(match tool {
        "create" => {
            let kind = arg_choice(p, "kind", "null", &[("null", ""), ("solid", ""), ("adjustment", "")])?;
            match kind.as_str() {
                "solid" => native("layer.newSolid", json!({"color": arg_color(p, "color", &set.color)?}), "Solid"),
                "adjustment" => native("layer.newAdjustment", json!({}), "Adjustment"),
                _ => native("layer.newNull", json!({}), "Null"),
            }
        }
        "fill" => {
            let c = arg_color(p, "color", &set.color)?;
            plug("toolkit_style", json!({"kind": "fill", "colors": [c, "#ffffff", "#ffffff", "#ffffff"]}), WORKFLOW, "Fill")
        }
        "anchor" => {
            let (x, y) = match p.get("at").and_then(Value::as_str) {
                Some(at) => {
                    let i = ANCHORS
                        .iter()
                        .position(|(k, _)| *k == at)
                        .ok_or_else(|| format!("`at`: unknown anchor `{at}` ({})", ANCHORS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")))?;
                    ((i % 3) as f64 / 2.0, (i / 3) as f64 / 2.0)
                }
                None => (arg_num(p, "x", 0.5, (0.0, 1.0))?, arg_num(p, "y", 0.5, (0.0, 1.0))?),
            };
            plug("toolkit_anchor", json!({"x": x, "y": y}), WORKFLOW, "Anchor")
        }
        "precompose" => {
            let name = arg_str(p, "name", &set.name)?.trim().to_string();
            let name = if name.is_empty() { "Precomp".to_string() } else { name };
            native("layer.smartPrecompose", json!({"name": name}), "Precompose")
        }
        "precompGroups" => {
            let name = arg_str(p, "name", &set.name)?.trim().to_string();
            let name = if name.is_empty() { "Precomp".to_string() } else { name };
            let count = arg_num(p, "count", f64::from(set.count), COUNT)?;
            if count.fract() != 0.0 {
                return Err("count must be a whole number.".into());
            }
            native("layer.multiPrecompose", json!({"name": name, "count": count as u64}), "Precomp groups")
        }
        "decompose" => native("layer.decompose", json!({}), "Un-precompose"),
        "deepDuplicate" => native("project.deepDuplicate", json!({}), "Deep duplicate"),
        "gradientLock" => native("shape.gradientLock", json!({}), "Gradient lock"),
        "purge" => native("edit.purge", json!({"what": "memory"}), "Purge previews"),
        "image" => {
            let source = if let Some(path) = p.get("path").and_then(Value::as_str) {
                ImageSource::Path(path.to_string())
            } else if let Some(png) = p.get("png").and_then(Value::as_str) {
                // The plug's limit for base64 text (≈ 5 MB of PNG).
                if png.len() > 7_000_000 {
                    return Err("Paste a PNG image smaller than 5 MB.".into());
                }
                ImageSource::Png(png.to_string())
            } else if arg_bool(p, "dialog", false)? {
                ImageSource::Dialog
            } else {
                ImageSource::Clipboard
            };
            Built { plan: Plan::Image(source), section: WORKFLOW, label: "Paste image".into() }
        }
        "split" => {
            let mode = arg_choice(p, "mode", &set.split_mode, &SPLIT_MODES)?;
            let text = arg_str(p, "text", &set.split_text)?;
            if mode == "custom" && text.trim().is_empty() {
                return Err("Enter a word or phrase to isolate.".into());
            }
            if text.len() > 1024 {
                return Err("The word or phrase is at most 1024 bytes.".into());
            }
            let args = json!({
                "mode": mode,
                "text": if mode == "custom" { text } else { String::new() },
                "reverse": arg_bool(p, "reverse", set.reverse)?,
                "deleteOriginal": arg_bool(p, "deleteOriginal", set.delete_original)?,
            });
            Built { plan: Plan::Native { cmd: "text.split", args }, section: TEXT, label: "Split text".into() }
        }
        "bounce" => {
            let property = match p.get("property") {
                None | Some(Value::Null) if !keyed.is_empty() => "selected".to_string(),
                _ => arg_choice(p, "property", &set.property, &TARGETS)?,
            };
            let mode = arg_choice(p, "mode", &set.motion, &[("overshoot", ""), ("bounce", ""), ("remove", "")])?;
            if mode == "remove" {
                plug("toolkit_bounce", json!({"property": property, "mode": "remove"}), BOUNCE, "Remove spring")
            } else {
                let args = json!({
                    "property": property,
                    "mode": mode,
                    "strength": arg_num(p, "strength", set.strength, STRENGTH)?,
                    "frequency": arg_num(p, "frequency", set.frequency, FREQUENCY)?,
                    "decay": arg_num(p, "decay", set.decay, DECAY)?,
                });
                plug("toolkit_bounce", args, BOUNCE, "Apply spring")
            }
        }
        "style" => {
            let kind = arg_choice(p, "kind", "ramp", &[("ramp", ""), ("four", ""), ("fill", ""), ("sweep", "")])?;
            let colors: Vec<String> = match p.get("colors") {
                None | Some(Value::Null) => set.colors.to_vec(),
                Some(Value::Array(a)) if a.len() == 4 => a
                    .iter()
                    .map(|c| c.as_str().and_then(hex6).ok_or_else(|| "`colors`: choose four six-digit colours (#rrggbb)".to_string()))
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err("Choose four colours (`colors`: four #rrggbb).".into()),
            };
            let args = json!({
                "kind": kind,
                "colors": colors,
                "duration": arg_num(p, "duration", set.duration, DURATION)?,
                "animate": arg_bool(p, "animate", set.animate)?,
            });
            let label = STYLES.iter().find(|(k, _)| *k == kind).map_or("Fill", |(_, l)| l);
            plug("toolkit_style", args, if kind == "fill" { WORKFLOW } else { COLOUR }, label)
        }
        other => {
            return Err(format!(
                "unknown tool `{other}` (create, fill, anchor, precompose, precompGroups, decompose, deepDuplicate, gradientLock, purge, image, split, bounce, style)"
            ));
        }
    })
}

/// What a native command did, in the web panel's words.
fn native_message(tool: &str, p: &Value, r: &Value) -> String {
    let n = |k: &str| r.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    let num = |k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0);
    match tool {
        "create" => format!("Created {} layer.", p.get("kind").and_then(Value::as_str).unwrap_or("null")),
        "precompose" => format!("Precomposed into “{}”.", r.get("name").and_then(Value::as_str).unwrap_or("Precomp")),
        "precompGroups" => format!("{} precomps.", n("precomps")),
        "decompose" => format!("{} layers expanded; the precomp stays in the project.", num("expanded")),
        "deepDuplicate" => format!("Duplicated {} compositions.", n("items").max(1)),
        "gradientLock" => format!("Gradient lock: {} locked, {} already.", num("locked"), num("already")),
        "purge" => "Preview memory cleared. Undo history kept.".into(),
        _ => "Done.".into(),
    }
}

/// Run tool `tool` (the panel's buttons and `motionLibrary.tool` both come here). Native tools
/// return their result; plug tools start in the background and return `{running}`.
fn start(app: &mut EffectcraftApp, ctx: &egui::Context, tool: &str, p: &Value) -> Result<Value, String> {
    if let Some(running) = shared().busy.clone() {
        return Err(format!("{running} is still running: one action at a time."));
    }
    let fail = |app: &mut EffectcraftApp, e: String| {
        record(tool, &Err(e.clone()));
        app.ui.status = e.clone();
        Err(e)
    };
    let mut built = match build(tool, p, &settings(), &keyed_props(&app.session)) {
        Ok(b) => b,
        Err(e) => return fail(app, e),
    };
    // CX9: a spring on a Range Selector's Start / End / Offset does nothing visible; bounce each
    // unit with an Expression Selector animator instead (the reveal keeps the opacity).
    if let Plan::Plug { op: "toolkit_bounce", args } = &built.plan
        && args["property"] == json!("selected")
        && args["mode"] != json!("remove")
        && reveal_keys(&app.session)
    {
        let mut a = json!({"mode": args["mode"], "strength": args["strength"], "frequency": args["frequency"], "decay": args["decay"]});
        if let Some(m) = p.get("moves") {
            a["moves"] = m.clone();
        }
        built = Built { plan: Plan::Native { cmd: "text.unitBounce", args: a }, section: BOUNCE, label: "Unit bounce".into() };
    }
    match built.plan {
        Plan::Native { cmd, args } => {
            let r = app.session.execute(cmd, args).map_err(|e| e.to_string()).map(|mut v| {
                if v.get("message").is_none() {
                    let m = native_message(tool, p, &v);
                    match v.as_object_mut() {
                        Some(o) => {
                            o.insert("message".into(), m.into());
                        }
                        None => v = json!({"result": v, "message": m}),
                    }
                }
                v
            });
            record(tool, &r);
            match &r {
                Ok(v) => app.ui.status = result_message(v),
                Err(e) => app.ui.status = e.clone(),
            }
            r
        }
        Plan::Plug { op, args } => {
            let port = match control_port(app) {
                Ok(port) => port,
                Err(e) => return fail(app, e),
            };
            spawn(ctx, tool, &built.label, move || plug_rpc(port, op, args));
            Ok(json!({"running": tool, "label": built.label}))
        }
        Plan::Image(source) => {
            let port = match control_port(app) {
                Ok(port) => port,
                Err(e) => return fail(app, e),
            };
            let paste = move |png: Vec<u8>| plug_rpc(port, "toolkit_image", json!({"png": crate::control::base64(&png)}));
            match source {
                ImageSource::Png(data) => spawn(ctx, tool, &built.label, move || plug_rpc(port, "toolkit_image", json!({"png": data}))),
                ImageSource::Path(path) => spawn(ctx, tool, &built.label, move || paste(read_png(&path)?)),
                ImageSource::Clipboard if cfg!(target_os = "macos") => spawn(ctx, tool, &built.label, move || paste(clipboard_png()?)),
                ImageSource::Clipboard | ImageSource::Dialog => {
                    if let Some(open) = app.hooks.file_dialog_async.as_ref() {
                        let ctx = ctx.clone();
                        open(
                            crate::FileDialog::Open { exts: vec!["png".into()], many: false },
                            Box::new(move |paths| {
                                if let Some(path) = paths.into_iter().next() {
                                    spawn(&ctx, "image", "Paste image", move || paste(read_png(&path)?));
                                }
                            }),
                        );
                        return Ok(json!({"dialog": "png"}));
                    }
                    let picked = app.hooks.pick_files.as_ref().map(|f| f(&["png"])).unwrap_or_default();
                    let Some(path) = picked.into_iter().next() else {
                        return fail(app, "Choose a PNG file (or pass `path`).".into());
                    };
                    spawn(ctx, tool, &built.label, move || paste(read_png(&path)?));
                }
            }
            Ok(json!({"running": tool, "label": built.label}))
        }
    }
}

/// Run `job` on a background thread (inline on the web); its result goes to the feedback line.
fn spawn(ctx: &egui::Context, tool: &str, label: &str, job: impl FnOnce() -> Result<Value, String> + Send + 'static) {
    {
        let mut sh = shared();
        sh.busy = Some(label.to_string());
        sh.feedback = None;
    }
    let (ctx, tool) = (ctx.clone(), tool.to_string());
    let run = move || {
        let r = job();
        shared().busy = None;
        record(&tool, &r);
        ctx.request_repaint();
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Err(e) = std::thread::Builder::new().name("ml-tool".into()).spawn(run) {
        shared().busy = None;
        record("thread", &Err(format!("couldn't start the action: {e}")));
    }
    #[cfg(target_arch = "wasm32")]
    run();
}

/// A PNG file's bytes (at most 5 MB).
fn read_png(path: &str) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| format!("Can't read {path}: {e}"))?.len();
    if len > MAX_PNG {
        return Err("Choose a PNG image smaller than 5 MB.".into());
    }
    let data = std::fs::read(path).map_err(|e| format!("Can't read {path}: {e}"))?;
    check_png(&data)?;
    Ok(data)
}

fn check_png(data: &[u8]) -> Result<(), String> {
    if data.len() as u64 > MAX_PNG {
        return Err("Paste a PNG image smaller than 5 MB.".into());
    }
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("Choose a PNG image.".into());
    }
    Ok(())
}

/// The clipboard's image as PNG (macOS: the pasteboard's PNG flavour, which macOS also offers for
/// TIFF and other images).
#[cfg(target_os = "macos")]
fn clipboard_png() -> Result<Vec<u8>, String> {
    let file = std::env::temp_dir().join(format!("effectcraft-ml-clipboard-{}.png", std::process::id()));
    let script = [
        "on run argv",
        "try",
        "set d to the clipboard as «class PNGf»",
        "on error",
        "return \"none\"",
        "end try",
        "set f to open for access (POSIX file (item 1 of argv)) with write permission",
        "try",
        "set eof f to 0",
        "write d to f",
        "end try",
        "close access f",
        "return \"ok\"",
        "end run",
    ];
    let mut cmd = std::process::Command::new("osascript");
    for line in script {
        cmd.arg("-e").arg(line);
    }
    let out = cmd.arg(&file).output().map_err(|e| format!("Can't read the clipboard: {e}"))?;
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if answer != "ok" {
        let _ = std::fs::remove_file(&file);
        return Err(if out.status.success() { "Copy an image first (or choose a PNG file).".into() } else { "Can't read the clipboard.".into() });
    }
    let data = std::fs::read(&file).map_err(|e| format!("Can't read the clipboard image: {e}"));
    let _ = std::fs::remove_file(&file);
    let data = data?;
    check_png(&data)?;
    Ok(data)
}

#[cfg(not(target_os = "macos"))]
fn clipboard_png() -> Result<Vec<u8>, String> {
    Err("Clipboard images are read on macOS; choose a PNG file instead.".into())
}

/// Four random colours for Shuffle colours.
fn shuffled_colors() -> [String; 4] {
    use std::hash::BuildHasher;
    let state = std::collections::hash_map::RandomState::new();
    std::array::from_fn(|i| format!("#{:06x}", state.hash_one(i) & 0xff_ffff))
}

// ── Command ────────────────────────────────────────────────────────────────────────────────────

/// The settings, the running action and the last result.
fn status_json() -> Value {
    let sh = shared();
    json!({
        "settings": sh.settings.clone().unwrap_or_default(),
        "busy": sh.busy,
        "feedback": sh.feedback.as_ref().map(|f| json!({"text": f.text, "error": f.error})),
        "last": sh.last,
    })
}

/// `settings` with the fields of `patch` (unknown keys are an error), checked.
fn patched(settings: &ToolSettings, patch: &serde_json::Map<String, Value>) -> Result<ToolSettings, String> {
    let mut v = serde_json::to_value(settings).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("settings aren't an object")?;
    for (k, val) in patch {
        if k == "op" {
            continue;
        }
        if !obj.contains_key(k) {
            let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            return Err(format!("unknown setting `{k}` ({})", keys.join(", ")));
        }
        obj.insert(k.clone(), val.clone());
    }
    let mut next: ToolSettings = serde_json::from_value(v).map_err(|e| format!("bad setting: {e}"))?;
    next.validate()?;
    Ok(next)
}

fn section_index(p: &Value) -> Result<usize, String> {
    let key = p.get("section").and_then(Value::as_str).ok_or("missing `section` (workflow, text, bounce, colour)")?;
    SECTIONS.iter().position(|(k, _, _)| *k == key).ok_or_else(|| format!("unknown section `{key}` (workflow, text, bounce, colour)"))
}

/// `motionLibrary.tool`: `{tool, …args}` runs a tool (missing arguments take the panel's
/// values): create {kind: null|solid|adjustment, color?}, fill {color?}, anchor {at: topLeft…
/// bottomRight | x, y (0–1)}, precompose {name?}, precompGroups {name?, count?}, decompose,
/// deepDuplicate, gradientLock, purge, image {path? | png? (base64) | dialog?; else the
/// clipboard}, split {mode?, text?, reverse?, deleteOriginal?}, bounce {property?: selected|position|
/// scale|rotation|opacity (default: the selected keys, else the dropdown), mode?:
/// overshoot|bounce|remove, strength?, frequency?, decay?}, style {kind: ramp|four|fill|sweep,
/// colors?, duration?, animate?}. Native tools return their result; the others run in the
/// background and return `{running}` (then `{op: "status"}` has the result).
///
/// `{op: "settings", …fields}` changes and returns the settings; `{op: "status"}` the settings,
/// the running action and the last result; `{op: "show", section}` opens the Tools workspace on
/// a section; `{op: "collapse", section, collapsed?}` folds one (toggles without `collapsed`);
/// `{op: "shuffle"}` picks four random colours.
pub fn command(app: &mut EffectcraftApp, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    if let Some(op) = params.get("op").and_then(Value::as_str) {
        match op {
            "settings" => {
                if let Some(patch) = params.as_object().filter(|o| o.len() > 1) {
                    let next = patched(&settings(), patch)?;
                    shared().settings = Some(next);
                    ctx.request_repaint();
                }
                return serde_json::to_value(settings()).map_err(|e| e.to_string());
            }
            "status" => return Ok(status_json()),
            "show" => {
                let i = section_index(params)?;
                app.ui.motion_library.workspace = "tools".into();
                let mut sh = shared();
                let mut set = sh.settings.clone().unwrap_or_default();
                set.set_collapsed(i, false);
                sh.settings = Some(set);
                sh.jump = Some(i);
                sh.focus = i;
                ctx.request_repaint();
                return Ok(json!({"section": SECTIONS.get(i).map(|s| s.0)}));
            }
            "collapse" => {
                let i = section_index(params)?;
                let mut set = settings();
                let collapsed = match params.get("collapsed") {
                    Some(v) => v.as_bool().ok_or("`collapsed` is true or false")?,
                    None => !set.is_collapsed(i),
                };
                set.set_collapsed(i, collapsed);
                shared().settings = Some(set);
                ctx.request_repaint();
                return Ok(json!({"section": SECTIONS.get(i).map(|s| s.0), "collapsed": collapsed}));
            }
            "shuffle" => {
                let mut set = settings();
                set.colors = shuffled_colors();
                let colors = set.colors.clone();
                shared().settings = Some(set);
                ctx.request_repaint();
                return Ok(json!({"colors": colors}));
            }
            other => return Err(format!("unknown op `{other}` (settings, status, show, collapse, shuffle; or pass `tool`)")),
        }
    }
    let tool = params.get("tool").and_then(Value::as_str).ok_or(
        "missing `tool` (create, fill, anchor, precompose, precompGroups, decompose, deepDuplicate, gradientLock, purge, image, split, bounce, style) or `op` (settings, status, show, collapse, shuffle)",
    )?;
    start(app, ctx, tool, params)
}

// ── Workspace ──────────────────────────────────────────────────────────────────────────────────

/// Which buttons work right now.
#[derive(Clone, Copy)]
struct Enable {
    /// No action is running.
    idle: bool,
    /// The plug can reach this app (the control port is on).
    plug: bool,
}

impl Enable {
    fn native(self) -> bool {
        self.idle
    }
    fn plug(self) -> bool {
        self.idle && self.plug
    }
}

/// The layout cursor in a section's body.
struct Cursor {
    x: f32,
    w: f32,
    y: f32,
    s: f32,
}

impl Cursor {
    fn row(&self, h: f32) -> Rect {
        Rect::from_min_size(pos2(self.x, self.y), vec2(self.w, h * self.s))
    }
    /// The label column's width.
    fn label_w(&self) -> f32 {
        (self.w * 0.42).clamp(64.0 * self.s, 128.0 * self.s)
    }
}

/// A clicked button's tool and arguments.
type Action = Option<(&'static str, Value)>;

/// A text button that is greyed out and inert unless `enabled`.
fn act(app: &mut EffectcraftApp, ui: &mut egui::Ui, r: Rect, auto: &str, text: &str, primary: bool, enabled: bool) -> bool {
    let mut sub = ui.new_child(egui::UiBuilder::new().max_rect(r));
    if !enabled {
        sub.disable();
    }
    kit::button(app, &mut sub, r, auto, tr(text), primary) && enabled
}

/// A label on the left of a row; returns the control's rect on the right.
fn labelled(ui: &egui::Ui, c: &Cursor, t: &Tokens, label: &str) -> Rect {
    let row = c.row(24.0);
    let lw = c.label_w();
    widgets::text_fit(ui.painter(), pos2(row.min.x, row.center().y), Align2::LEFT_CENTER, tr(label), Tokens::ui(11.5 * c.s), lw - 6.0 * c.s, t.text_dim);
    Rect::from_min_max(pos2(row.min.x + lw, row.min.y), row.max)
}

/// A row of buttons (wrapping to more rows when narrow): `(automation name, label, primary,
/// enabled)`. Returns the clicked one's index.
fn button_rows(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, section: &str, items: &[(&str, &str, bool, bool)], min_w: f32) -> Option<usize> {
    let gap = 6.0 * c.s;
    let per_row = (((c.w + gap) / (min_w * c.s + gap)).floor() as usize).clamp(1, items.len().max(1));
    let bw = (c.w - gap * (per_row as f32 - 1.0)) / per_row as f32;
    let mut clicked = None;
    for (i, (name, text, primary, enabled)) in items.iter().enumerate() {
        let (col, row) = (i % per_row, i / per_row);
        let r = Rect::from_min_size(pos2(c.x + col as f32 * (bw + gap), c.y + row as f32 * (24.0 * c.s + gap)), vec2(bw, 24.0 * c.s));
        if act(app, ui, r, &format!("motionLibrary.tools.{section}.{name}"), text, *primary, *enabled) {
            clicked = Some(i);
        }
    }
    let rows = items.len().div_ceil(per_row);
    c.y += rows as f32 * (24.0 * c.s + gap) + 2.0 * c.s;
    clicked
}

/// A colour swatch that opens the colour picker; true when the colour changed.
fn swatch(app: &mut EffectcraftApp, ui: &mut egui::Ui, r: Rect, auto: &str, hex: &mut String) -> bool {
    let t = app.tokens;
    let mut rgb = rgb_of(hex);
    let id = egui::Id::new(("ml-tools-color", auto));
    let resp = widgets::swatch(ui, r, [rgb[0], rgb[1], rgb[2], 1.0], id, &t);
    app.auto.add_response(auto, &resp, hex);
    if resp.on_hover_text(hex.as_str()).clicked() {
        widgets::open_popup(ui, id);
    }
    if widgets::color_popup(ui, id, r.left_bottom(), &mut rgb) {
        *hex = widgets::srgb_hex(rgb).to_ascii_lowercase();
        return true;
    }
    false
}

/// A labelled number (`decimals` places) clamped to `range`.
#[allow(clippy::too_many_arguments)]
fn number_row(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, auto: &str, label: &str, v: f64, range: (f64, f64), decimals: usize) -> f64 {
    let t = app.tokens;
    let ctrl = labelled(ui, c, &t, label);
    let speed = if decimals == 0 { 1.0 } else { 0.1 };
    let nv = kit::number(app, ui, pos2(ctrl.min.x, ctrl.center().y - 9.0), auto, v, speed, range, decimals, "");
    c.y += 30.0 * c.s;
    if nv.is_finite() { nv.clamp(range.0, range.1) } else { v }
}

/// A labelled dropdown over `options` (key, label); returns the chosen key.
fn choice_row(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, auto: &str, label: &str, options: &[(&str, &str)], cur: &mut String) {
    let t = app.tokens;
    let ctrl = labelled(ui, c, &t, label);
    let labels: Vec<&str> = options.iter().map(|(_, l)| tr(l)).collect();
    let i = options.iter().position(|(k, _)| *k == cur.as_str()).unwrap_or(0);
    if let Some(j) = kit::dropdown(app, ui, ctrl, auto, &labels, i)
        && let Some((k, _)) = options.get(j)
    {
        *cur = k.to_string();
    }
    c.y += 30.0 * c.s;
}

fn checkbox_row(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, auto: &str, label: &str, on: &mut bool) {
    *on = kit::checkbox(app, ui, pos2(c.x, c.y + 4.0 * c.s), auto, tr(label), *on);
    c.y += 26.0 * c.s;
}

fn text_row(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, auto: &str, label: &str, hint: &str, text: &mut String) {
    let t = app.tokens;
    let ctrl = labelled(ui, c, &t, label);
    let resp = widgets::text_field(ui, ctrl, text, tr(hint), &t);
    app.auto.add_response(auto, &resp, label);
    c.y += 30.0 * c.s;
}

/// Dim help text wrapped to the section's width.
fn note(ui: &egui::Ui, c: &mut Cursor, t: &Tokens, text: &str) {
    let galley = ui.painter().layout(tr(text).to_string(), Tokens::ui(11.0 * c.s), t.text_faint, c.w.max(40.0));
    let h = galley.size().y;
    ui.painter().galley(pos2(c.x, c.y), galley, t.text_faint);
    c.y += h + 8.0 * c.s;
}

fn workflow_body(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, set: &mut ToolSettings, en: Enable) -> Action {
    let t = app.tokens;
    let s = c.s;
    let mut action: Action = None;
    // Solid / fill colour: the label takes what the swatch and its hex code leave.
    let row = c.row(24.0);
    let hex_w = 62.0 * s;
    let sw = Rect::from_min_size(pos2(row.max.x - hex_w - 34.0 * s, row.min.y + 2.0 * s), vec2(34.0 * s, 20.0 * s));
    widgets::text_fit(
        ui.painter(),
        pos2(row.min.x, row.center().y),
        Align2::LEFT_CENTER,
        tr("Solid / fill colour"),
        Tokens::ui(11.5 * s),
        sw.min.x - row.min.x - 6.0 * s,
        t.text_dim,
    );
    swatch(app, ui, sw, "motionLibrary.tools.workflow.color", &mut set.color);
    ui.painter().text(pos2(sw.max.x + 8.0 * s, sw.center().y), Align2::LEFT_CENTER, set.color.to_ascii_uppercase(), Tokens::mono(11.0 * s), t.text_dim);
    c.y += 30.0 * s;
    let items = [
        ("null", "Null", false, en.native()),
        ("adjustment", "Adjustment", false, en.native()),
        ("solid", "Solid", false, en.native()),
        ("fill", "Fill", false, en.plug()),
    ];
    match button_rows(app, ui, c, "workflow", &items, 74.0) {
        Some(3) => action = Some(("fill", json!({"color": set.color}))),
        Some(i) => {
            let kind = ["null", "adjustment", "solid"].get(i).copied().unwrap_or("null");
            action = Some(("create", json!({"kind": kind, "color": set.color})));
        }
        None => {}
    }
    // Anchor: a 3 × 3 grid of positions.
    let cell = (30.0 * s).round();
    let grid_x = c.x + c.label_w();
    widgets::text_fit(ui.painter(), pos2(c.x, c.y + cell * 1.5), Align2::LEFT_CENTER, tr("Anchor"), Tokens::ui(11.5 * s), c.label_w(), t.text_dim);
    let mut sub = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(grid_x, c.y), vec2(cell * 3.0, cell * 3.0))));
    if !en.plug() {
        sub.disable();
    }
    for (i, (name, what)) in ANCHORS.iter().enumerate() {
        let r = Rect::from_min_size(pos2(grid_x + (i % 3) as f32 * cell, c.y + (i / 3) as f32 * cell), vec2(cell - 3.0 * s, cell - 3.0 * s));
        let resp = sub.interact(r, egui::Id::new(("ml-tools-anchor", *name)), Sense::click());
        let p = sub.painter();
        p.rect_filled(r, CornerRadius::same((4.0 * s) as u8), if resp.hovered() && en.plug() { t.pressed } else { t.hover });
        let frame = r.shrink(7.0 * s);
        p.rect_stroke(frame, 1.0, Stroke::new(1.0, t.text_faint), egui::StrokeKind::Inside);
        let dot = pos2(frame.min.x + frame.width() * (i % 3) as f32 / 2.0, frame.min.y + frame.height() * (i / 3) as f32 / 2.0);
        p.circle_filled(dot, 2.6 * s, if resp.hovered() && en.plug() { t.accent_hover } else { t.accent });
        let tip = format!("Anchor {what}: move the anchor without moving the layer (static 2D transforms)");
        app.auto.add(&format!("motionLibrary.tools.workflow.anchor.{name}"), r, &tip);
        if resp.on_hover_text(tip).clicked() && en.plug() {
            action = Some(("anchor", json!({"at": name})));
        }
    }
    c.y += cell * 3.0 + 8.0 * s;
    // Precomps.
    text_row(app, ui, c, "motionLibrary.tools.workflow.name", "Name", "Precomp", &mut set.name);
    set.count = number_row(app, ui, c, "motionLibrary.tools.workflow.count", "Group size", f64::from(set.count), COUNT, 0) as u32;
    let items = [
        ("precompose", "Precompose", false, en.native()),
        ("precompGroups", "Precomp groups", false, en.native()),
        ("decompose", "Un-precompose", false, en.native()),
        ("deepDuplicate", "Deep duplicate", false, en.native()),
        ("gradientLock", "Gradient lock", false, en.native()),
        ("purge", "Purge previews", false, en.native()),
    ];
    if let Some(i) = button_rows(app, ui, c, "workflow", &items, 104.0)
        && let Some((tool, _, _, _)) = items.get(i)
    {
        action = Some(match *tool {
            "precompose" => ("precompose", json!({"name": set.name})),
            "precompGroups" => ("precompGroups", json!({"name": set.name, "count": set.count})),
            "decompose" => ("decompose", json!({})),
            "deepDuplicate" => ("deepDuplicate", json!({})),
            "gradientLock" => ("gradientLock", json!({})),
            _ => ("purge", json!({})),
        });
    }
    let items = [("pasteImage", "Paste image", false, en.plug()), ("imageFile", "PNG file…", false, en.plug())];
    match button_rows(app, ui, c, "workflow", &items, 104.0) {
        Some(0) => action = Some(("image", json!({}))),
        Some(_) => action = Some(("image", json!({"dialog": true}))),
        None => {}
    }
    action
}

fn text_body(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, set: &mut ToolSettings, en: Enable) -> Action {
    let t = app.tokens;
    choice_row(app, ui, c, "motionLibrary.tools.text.mode", "Split into", &SPLIT_MODES, &mut set.split_mode);
    if set.split_mode == "custom" {
        text_row(app, ui, c, "motionLibrary.tools.text.phrase", "Word / phrase", "e.g. Hello", &mut set.split_text);
        if set.split_text.len() > 1024 {
            let mut end = 1024;
            while !set.split_text.is_char_boundary(end) {
                end -= 1;
            }
            set.split_text.truncate(end);
        }
    }
    checkbox_row(app, ui, c, "motionLibrary.tools.text.reverse", "Reverse layer order", &mut set.reverse);
    checkbox_row(app, ui, c, "motionLibrary.tools.text.deleteOriginal", "Delete original", &mut set.delete_original);
    note(ui, c, &t, "By default the original is kept hidden. Split before adding text animators, masks or effects.");
    let ready = set.split_mode != "custom" || !set.split_text.trim().is_empty();
    let items = [("split", "Split text", true, en.native() && ready)];
    button_rows(app, ui, c, "text", &items, 120.0).map(|_| ("split", json!({})))
}

fn bounce_body(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, set: &mut ToolSettings, en: Enable) -> Action {
    let keyed = keyed_props(&app.session);
    if keyed.is_empty() {
        choice_row(app, ui, c, "motionLibrary.tools.bounce.property", "Property", &PROPERTIES, &mut set.property);
    } else {
        // Selected keys win over the dropdown (Apply spring springs them); deselect to pick one.
        let t = app.tokens;
        let ctrl = labelled(ui, c, &t, "Property");
        let label = format!("{} ({})", tr("Selected keys"), keyed.join(", "));
        kit::dropdown(app, ui, ctrl, "motionLibrary.tools.bounce.property", &[label.as_str()], 0);
        c.y += 30.0 * c.s;
    }
    // CX9: Range Selector keys get a per-unit bounce (native, no plug needed).
    let reveal = reveal_keys(&app.session);
    if reveal {
        let t = app.tokens;
        note(ui, c, &t, "Range Selector keys only reveal (Ramp Up clamps them), so Apply spring bounces each unit as the reveal reaches it: an animator with an Expression Selector moves Position; the reveal keeps the opacity.");
    }
    choice_row(app, ui, c, "motionLibrary.tools.bounce.motion", "Motion", &MOTIONS, &mut set.motion);
    set.strength = number_row(app, ui, c, "motionLibrary.tools.bounce.strength", "Strength (%)", set.strength, STRENGTH, 0);
    set.frequency = number_row(app, ui, c, "motionLibrary.tools.bounce.frequency", "Bounces / s", set.frequency, FREQUENCY, 1);
    set.decay = number_row(app, ui, c, "motionLibrary.tools.bounce.decay", "Settle speed", set.decay, DECAY, 1);
    let items = [("apply", "Apply spring", true, (reveal && en.idle) || en.plug()), ("remove", "Remove spring", false, en.plug())];
    match button_rows(app, ui, c, "bounce", &items, 110.0) {
        Some(0) => Some(("bounce", json!({}))),
        Some(_) => Some(("bounce", json!({"mode": "remove"}))),
        None => None,
    }
}

fn colour_body(app: &mut EffectcraftApp, ui: &mut egui::Ui, c: &mut Cursor, set: &mut ToolSettings, en: Enable) -> Action {
    let s = c.s;
    let gap = 6.0 * s;
    let shuffle_w = 120.0 * s;
    let sw = ((c.w - shuffle_w - gap * 4.0) / 4.0).clamp(22.0 * s, 44.0 * s);
    for (i, hex) in set.colors.iter_mut().enumerate() {
        let r = Rect::from_min_size(pos2(c.x + i as f32 * (sw + gap), c.y + 2.0 * s), vec2(sw, 20.0 * s));
        swatch(app, ui, r, &format!("motionLibrary.tools.colour.color{}", i + 1), hex);
    }
    let shuffle = Rect::from_min_size(pos2(c.x + c.w - shuffle_w, c.y), vec2(shuffle_w, 24.0 * s));
    if act(app, ui, shuffle, "motionLibrary.tools.colour.shuffle", "Shuffle colours", false, true) {
        set.colors = shuffled_colors();
    }
    c.y += 32.0 * s;
    set.duration = number_row(app, ui, c, "motionLibrary.tools.colour.duration", "Duration (s)", set.duration, DURATION, 2);
    checkbox_row(app, ui, c, "motionLibrary.tools.colour.animate", "Animate gradient", &mut set.animate);
    c.y += 2.0 * s;
    let items: Vec<(&str, &str, bool, bool)> = STYLES.iter().map(|(k, l)| (*k, *l, false, en.plug())).collect();
    button_rows(app, ui, c, "colour", &items, 120.0).and_then(|i| STYLES.get(i)).map(|(k, _)| ("style", json!({"kind": k})))
}

/// The Tools workspace in `rect` (right of the rail): the section row, the feedback line, and the
/// sections (two columns when wide), scrolled.
pub fn workspace(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect, view: &mut ViewState, s: f32) {
    let _ = view;
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, t.panel_bg);
    app.auto.add("motionLibrary.tools", rect, "Motion Library tools");
    // This frame's copy of the shared state.
    let (mut set, busy, feedback, jump, mut scroll, mut heights, mut focus) = {
        let mut sh = shared();
        if std::mem::take(&mut sh.status_pending)
            && let Some(f) = &sh.feedback
        {
            app.ui.status = f.text.clone();
        }
        (sh.settings.clone().unwrap_or_default(), sh.busy.clone(), sh.feedback.clone(), sh.jump.take(), sh.scroll, sh.heights, sh.focus)
    };
    let before = set.clone();
    let port = control_port(app);
    let en = Enable { idle: busy.is_none(), plug: port.is_ok() };
    let pad = (12.0 * s).round();
    let inner_w = rect.width() - 2.0 * pad;

    // The section row: click a name to open that section and scroll to it.
    // Four in a row, or two rows of two in a narrow dock (so the names fit).
    let gap = 4.0 * s;
    let per_row = if (inner_w - gap * 3.0) / 4.0 < 84.0 * s { 2 } else { 4 };
    let chip_w = (inner_w - gap * (per_row as f32 - 1.0)) / per_row as f32;
    let chip_h = 24.0 * s;
    let nav = Rect::from_min_size(pos2(rect.min.x + pad, rect.min.y + 8.0 * s), vec2(inner_w, chip_h * (4 / per_row) as f32 + gap * (4 / per_row - 1) as f32));
    let mut jump = jump;
    for (i, (key, name, tip)) in SECTIONS.iter().enumerate() {
        let (col, row) = (i % per_row, i / per_row);
        let r = Rect::from_min_size(pos2(nav.min.x + col as f32 * (chip_w + gap), nav.min.y + row as f32 * (chip_h + gap)), vec2(chip_w, chip_h));
        let resp = ui.interact(r, egui::Id::new(("ml-tools-nav", *key)), Sense::click());
        let fill = if i == focus {
            t.accent.gamma_multiply(0.28)
        } else if resp.hovered() {
            t.pressed
        } else {
            t.hover
        };
        p.rect_filled(r, CornerRadius::same((r.height() / 2.0) as u8), fill);
        widgets::text_fit(
            &p,
            r.center(),
            Align2::CENTER_CENTER,
            tr(name),
            Tokens::medium(11.5 * s),
            r.width() - 10.0 * s,
            if i == focus { t.text } else { t.text_dim },
        );
        app.auto.add(&format!("motionLibrary.tools.nav.{key}"), r, name);
        if resp.on_hover_text(tr(tip)).clicked() {
            set.set_collapsed(i, false);
            jump = Some(i);
            focus = i;
        }
    }
    let mut y = nav.max.y + 8.0 * s;

    // Feedback: what is running, or how the last action went; then why plug tools are off.
    let line = match (&busy, &feedback) {
        (Some(label), _) => Some((format!("{} {label}…", tr("Working:")), t.text_dim)),
        (None, Some(f)) => Some((f.text.clone(), if f.error { t.danger } else { t.text })),
        (None, None) => None,
    };
    if let Some((text, color)) = line {
        let galley = p.layout(text.clone(), Tokens::ui(11.5 * s), color, inner_w.max(40.0));
        let r = Rect::from_min_size(pos2(rect.min.x + pad, y), galley.size());
        p.galley(r.min, galley, color);
        app.auto.add("motionLibrary.tools.feedback", r, &text);
        y = r.max.y + 6.0 * s;
        if busy.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
    if port.is_err() {
        let text =
            tr("Fill, Anchor, Bounce, Colour and Paste image run through the control channel: turn on Settings ▸ Scripting ▸ Let Agents Control EffectCraft.");
        let galley = p.layout(text.to_string(), Tokens::ui(11.0 * s), t.warning, inner_w.max(40.0));
        let r = Rect::from_min_size(pos2(rect.min.x + pad, y), galley.size());
        p.galley(r.min, galley, t.warning);
        app.auto.add("motionLibrary.tools.connection", r, text);
        y = r.max.y + 6.0 * s;
    }
    p.line_segment([pos2(rect.min.x, y), pos2(rect.max.x, y)], Stroke::new(1.0, t.separator));

    // The sections: one column, or two when wide (Workflow on the left, the others on the right).
    let content = Rect::from_min_max(pos2(rect.min.x, y + 1.0), rect.max);
    if !content.is_positive() {
        shared().settings = Some(set);
        return;
    }
    let col_gap = 10.0 * s;
    let two = inner_w >= 2.0 * 300.0 * s + col_gap;
    let col_w = if two { (inner_w - col_gap) / 2.0 } else { inner_w };
    let head_h = (30.0 * s).round();
    let sec_gap = 8.0 * s;
    let prev = heights;
    let height = |i: usize, set: &ToolSettings| if set.is_collapsed(i) { head_h } else { prev.get(i).copied().filter(|h| *h > head_h).unwrap_or(240.0 * s) };
    let mut tops = [0.0f32; 4];
    let mut cols = [0.0f32; 2];
    let mut which = [0usize; 4];
    for (i, top) in tops.iter_mut().enumerate() {
        let col = usize::from(two && i != WORKFLOW);
        which[i] = col;
        *top = cols[col];
        cols[col] += height(i, &set) + sec_gap;
    }
    let total = cols[0].max(cols[1]) + 12.0 * s;
    if let Some(i) = jump
        && let Some(top) = tops.get(i)
    {
        scroll = *top;
    }
    if ui.rect_contains_pointer(content) {
        let dy = ctx.input(|i| i.smooth_scroll_delta.y);
        if dy.is_finite() && dy != 0.0 {
            scroll -= dy;
        }
    }
    let max_scroll = (total - content.height() + 10.0 * s).max(0.0);
    scroll = scroll.clamp(0.0, max_scroll);

    let mut cu = ui.new_child(egui::UiBuilder::new().max_rect(content));
    cu.set_clip_rect(content.intersect(ui.clip_rect()));
    let mut action: Action = None;
    for (i, (key, name, tip)) in SECTIONS.iter().enumerate() {
        let x = rect.min.x + pad + if which[i] == 1 { col_w + col_gap } else { 0.0 };
        let top = content.min.y + 8.0 * s + tops[i] - scroll;
        let card_h = height(i, &set);
        let card = Rect::from_min_size(pos2(x, top), vec2(col_w, card_h));
        let bg = cu.painter().add(egui::Shape::Noop);
        // Header: the twirl and the name; a click folds the section.
        let head = Rect::from_min_size(card.min, vec2(col_w, head_h));
        let collapsed = set.is_collapsed(i);
        let resp = cu.interact(head, egui::Id::new(("ml-tools-head", *key)), Sense::click());
        let tw = Rect::from_center_size(pos2(head.min.x + 14.0 * s, head.center().y), vec2(14.0 * s, 14.0 * s));
        let twirled = widgets::twirl(&mut cu, tw, !collapsed, egui::Id::new(("ml-tools-twirl", *key)), &t).clicked();
        cu.painter().text(pos2(tw.max.x + 6.0 * s, head.center().y), Align2::LEFT_CENTER, tr(name), Tokens::semibold(12.0 * s), t.text);
        app.auto.add(&format!("motionLibrary.tools.{key}.header"), head, if collapsed { "collapsed" } else { "expanded" });
        if resp.on_hover_text(tr(tip)).clicked() || twirled {
            set.set_collapsed(i, !collapsed);
            focus = i;
        }
        let mut measured = head_h;
        if !collapsed {
            let mut c = Cursor { x: card.min.x + 12.0 * s, w: col_w - 24.0 * s, y: head.max.y + 2.0 * s, s };
            // Only lay out a body that is on screen (its height is kept from when it was).
            let visible = card.max.y >= content.min.y && card.min.y <= content.max.y;
            if visible || heights.get(i).copied().unwrap_or(0.0) <= head_h {
                let a = match i {
                    WORKFLOW => workflow_body(app, &mut cu, &mut c, &mut set, en),
                    TEXT => text_body(app, &mut cu, &mut c, &mut set, en),
                    BOUNCE => bounce_body(app, &mut cu, &mut c, &mut set, en),
                    _ => colour_body(app, &mut cu, &mut c, &mut set, en),
                };
                if a.is_some() {
                    action = a;
                }
                measured = c.y - card.min.y + 6.0 * s;
            } else {
                measured = card_h;
            }
        }
        if let Some(h) = heights.get_mut(i)
            && !collapsed
            && (*h - measured).abs() > 0.5
        {
            *h = measured;
            ctx.request_repaint();
        }
        let shown = Rect::from_min_size(card.min, vec2(col_w, if collapsed { head_h } else { measured }));
        cu.painter().set(bg, egui::Shape::rect_filled(shown, CornerRadius::same((8.0 * s) as u8), t.header_bg));
        cu.painter().rect_stroke(shown, CornerRadius::same((8.0 * s) as u8), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    }
    if max_scroll > 0.0 {
        let track = Rect::from_min_max(pos2(content.max.x - 8.0, content.min.y + 2.0), pos2(content.max.x - 2.0, content.max.y - 2.0));
        scroll = widgets::scroll_bar(ui, &mut app.auto, "motionLibrary.tools.scroll", track, scroll, max_scroll, &t);
    }
    {
        let mut sh = shared();
        // Settings changed by a command during this frame win over this frame's unchanged copy.
        if set != before || sh.settings.is_none() {
            sh.settings = Some(set);
        }
        sh.scroll = scroll;
        sh.heights = heights;
        sh.focus = focus;
    }
    if let Some((tool, args)) = action {
        // Errors are on the feedback line and in the status bar already.
        let _ = start(app, ctx, tool, &args);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_to_six_digits() {
        assert_eq!(hex6("#FFF").as_deref(), Some("#ffffff"));
        assert_eq!(hex6(" 1C8CFF ").as_deref(), Some("#1c8cff"));
        assert_eq!(hex6("#12345"), None);
        assert_eq!(hex6("red"), None);
        assert_eq!(hex6("#ééé"), None);
        assert_eq!(rgb_of("#ff0000"), [1.0, 0.0, 0.0]);
        assert_eq!(rgb_of("nope"), [1.0; 3]);
        for c in shuffled_colors() {
            assert_eq!(hex6(&c).as_deref(), Some(c.as_str()));
        }
    }

    /// Regression (19 Oct): with Opacity keys selected and the dropdown left on Position, Apply
    /// spring bounced Position ("Nothing to bounce: … › Position (needs 2 keyframes)"). Selected
    /// keys now win: the plug gets `selected` and springs the keys' own properties.
    #[test]
    fn bounce_springs_the_selected_keys_not_the_dropdown_property() {
        let mut s = effectcraft_engine::Session::default();
        s.execute("comp.new", json!({"width": 400, "height": 300, "duration": 2})).unwrap();
        let l = s.execute("layer.newSolid", json!({"name": "Tag", "width": 10, "height": 10})).unwrap()["layer"].as_u64().unwrap();
        for (t, v) in [(0.0, 0.0), (0.5, 100.0), (1.0, 80.0)] {
            s.execute("prop.addKey", json!({"layer": l, "path": "transform/opacity", "time": t, "value": v})).unwrap();
        }
        let set = ToolSettings::default();
        assert_eq!(set.property, "position");
        assert!(keyed_props(&s).is_empty(), "no keys selected yet");
        let keys: Vec<Value> = [0.0, 0.5, 1.0].iter().map(|t| json!({"layer": l, "path": "transform/opacity", "time": t})).collect();
        s.execute("keys.select", json!({"keys": keys})).unwrap();
        let keyed = keyed_props(&s);
        assert_eq!(keyed, vec!["Opacity".to_string()]);
        let b = build("bounce", &json!({}), &set, &keyed).unwrap();
        assert_eq!(
            b.plan,
            Plan::Plug { op: "toolkit_bounce", args: json!({"property": "selected", "mode": "overshoot", "strength": 20.0, "frequency": 3.0, "decay": 5.0}) }
        );
        let b = build("bounce", &json!({"mode": "remove"}), &set, &keyed).unwrap();
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_bounce", args: json!({"property": "selected", "mode": "remove"}) });
        // An explicit property still wins, and `selected` can be asked for by name.
        let b = build("bounce", &json!({"property": "scale"}), &set, &keyed).unwrap();
        assert_eq!(
            b.plan,
            Plan::Plug { op: "toolkit_bounce", args: json!({"property": "scale", "mode": "overshoot", "strength": 20.0, "frequency": 3.0, "decay": 5.0}) }
        );
        assert!(build("bounce", &json!({"property": "selected"}), &set, &[]).is_ok());
    }

    #[test]
    fn tools_take_the_panel_values_and_check_their_arguments() {
        let set = ToolSettings::default();
        let b = build("bounce", &json!({}), &set, &[]).unwrap();
        assert_eq!(b.section, BOUNCE);
        assert_eq!(
            b.plan,
            Plan::Plug { op: "toolkit_bounce", args: json!({"property": "position", "mode": "overshoot", "strength": 20.0, "frequency": 3.0, "decay": 5.0}) }
        );
        let b = build("bounce", &json!({"mode": "remove", "property": "scale"}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_bounce", args: json!({"property": "scale", "mode": "remove"}) });
        assert!(build("bounce", &json!({"strength": 201}), &set, &[]).unwrap_err().contains("strength must be between 0 and 200"));
        assert!(build("bounce", &json!({"frequency": 0}), &set, &[]).is_err());
        assert!(build("bounce", &json!({"decay": "fast"}), &set, &[]).is_err());
        assert!(build("bounce", &json!({"property": "skew"}), &set, &[]).is_err());

        let b = build("anchor", &json!({"at": "bottomRight"}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_anchor", args: json!({"x": 1.0, "y": 1.0}) });
        let b = build("anchor", &json!({"x": 0.0}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_anchor", args: json!({"x": 0.0, "y": 0.5}) });
        assert!(build("anchor", &json!({"at": "middle"}), &set, &[]).is_err());
        assert!(build("anchor", &json!({"x": 1.5}), &set, &[]).is_err());

        let b = build("create", &json!({"kind": "solid", "color": "#F00"}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Native { cmd: "layer.newSolid", args: json!({"color": "#ff0000"}) });
        assert!(build("create", &json!({"kind": "camera"}), &set, &[]).is_err());
        let b = build("fill", &json!({}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_style", args: json!({"kind": "fill", "colors": ["#ffffff", "#ffffff", "#ffffff", "#ffffff"]}) });

        let b = build("style", &json!({"kind": "four", "animate": true}), &set, &[]).unwrap();
        assert_eq!(b.section, COLOUR);
        assert_eq!(b.plan, Plan::Plug { op: "toolkit_style", args: json!({"kind": "four", "colors": DEFAULT_COLORS, "duration": 2.0, "animate": true}) });
        assert!(build("style", &json!({"colors": ["#fff"]}), &set, &[]).is_err());
        assert!(build("style", &json!({"colors": ["#fff", "#000", "#123456", "blue"]}), &set, &[]).is_err());
        assert!(build("style", &json!({"duration": 0.01}), &set, &[]).is_err());
        assert!(build("style", &json!({"kind": "plaid"}), &set, &[]).is_err());

        let b = build("precompGroups", &json!({"count": 3, "name": "  "}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Native { cmd: "layer.multiPrecompose", args: json!({"name": "Precomp", "count": 3}) });
        assert!(build("precompGroups", &json!({"count": 2.5}), &set, &[]).is_err());
        assert!(build("precompGroups", &json!({"count": 0}), &set, &[]).is_err());
        assert!(build("precompGroups", &json!({"count": 257}), &set, &[]).is_err());
        assert_eq!(build("purge", &json!({}), &set, &[]).unwrap().plan, Plan::Native { cmd: "edit.purge", args: json!({"what": "memory"}) });
        assert!(build("explode", &json!({}), &set, &[]).is_err());
    }

    #[test]
    fn text_split_needs_a_phrase_only_to_isolate_one() {
        let mut set = ToolSettings::default();
        let b = build("split", &json!({"reverse": true}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Native { cmd: "text.split", args: json!({"mode": "words", "text": "", "reverse": true, "deleteOriginal": false}) });
        assert!(build("split", &json!({"mode": "custom"}), &set, &[]).is_err());
        set.split_text = "Hello".into();
        let b = build("split", &json!({"mode": "custom"}), &set, &[]).unwrap();
        assert_eq!(b.plan, Plan::Native { cmd: "text.split", args: json!({"mode": "custom", "text": "Hello", "reverse": false, "deleteOriginal": false}) });
        assert!(build("split", &json!({"mode": "sentences"}), &set, &[]).is_err());
        assert!(build("split", &json!({"mode": "custom", "text": "x".repeat(1025)}), &set, &[]).is_err());
    }

    #[test]
    fn images_come_from_a_path_data_a_dialog_or_the_clipboard() {
        let set = ToolSettings::default();
        let src = |p: Value| match build("image", &p, &set, &[]).unwrap().plan {
            Plan::Image(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(src(json!({})), ImageSource::Clipboard);
        assert_eq!(src(json!({"dialog": true})), ImageSource::Dialog);
        assert_eq!(src(json!({"path": "/a.png"})), ImageSource::Path("/a.png".into()));
        assert_eq!(src(json!({"png": "iVBO"})), ImageSource::Png("iVBO".into()));
        assert!(build("image", &json!({"png": "A".repeat(7_000_001)}), &set, &[]).is_err());
        assert!(check_png(b"\x89PNG\r\n\x1a\nrest").is_ok());
        assert!(check_png(b"GIF89a").is_err());
        assert!(read_png("/definitely/not/here.png").is_err());
    }

    #[test]
    fn settings_patch_checks_every_field() {
        let set = ToolSettings::default();
        let patch = |v: Value| patched(&set, v.as_object().unwrap());
        let next = patch(json!({"op": "settings", "strength": 40, "colors": ["#FFF", "#000000", "#123", "#abcdef"], "collapsed": []})).unwrap();
        assert_eq!(next.strength, 40.0);
        assert_eq!(next.colors[0], "#ffffff");
        assert_eq!(next.colors[2], "#112233");
        assert!(!next.is_collapsed(BOUNCE));
        assert!(patch(json!({"strength": 500})).is_err());
        assert!(patch(json!({"color": "white"})).is_err());
        assert!(patch(json!({"colors": ["#fff"]})).is_err());
        assert!(patch(json!({"splitMode": "regex"})).is_err());
        assert!(patch(json!({"collapsed": ["curves"]})).is_err());
        assert!(patch(json!({"wobble": 1})).is_err());
        assert!(patch(json!({"count": -1})).is_err());
        // The defaults are valid, and round-trip.
        let mut d = ToolSettings::default();
        d.validate().unwrap();
        let back: ToolSettings = serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn sections_fold_independently() {
        let mut set = ToolSettings::default();
        assert!(!set.is_collapsed(WORKFLOW) && set.is_collapsed(TEXT));
        set.set_collapsed(TEXT, false);
        set.set_collapsed(WORKFLOW, true);
        set.set_collapsed(WORKFLOW, true);
        assert!(set.is_collapsed(WORKFLOW) && !set.is_collapsed(TEXT) && set.is_collapsed(BOUNCE));
        assert_eq!(set.collapsed.iter().filter(|c| *c == "workflow").count(), 1);
        set.set_collapsed(9, true);
        assert!(!set.is_collapsed(9));
    }

    #[test]
    fn messages_drop_the_reminders() {
        assert_eq!(
            result_message(&json!({"message": "Applied overshoot after the last keyframe. Original keyframes kept."})),
            "Applied overshoot after the last keyframe."
        );
        assert_eq!(result_message(&json!({"message": "Created null layer. Undo with Cmd+Z."})), "Created null layer.");
        assert_eq!(result_message(&json!({})), "Done.");
        assert_eq!(native_message("gradientLock", &json!({}), &json!({"locked": 2, "already": 1})), "Gradient lock: 2 locked, 1 already.");
    }
}

// ── Style families (batch 1, CX3) ──────────────────────────────────────────────────────────────

/// The Motion Library's Families tab: every style family of the project (`style.family.list`)
/// with its size; dragging a size and letting go changes every linked layer
/// (`style.family.set`, one undo step). "Link selected" adds the selected layers to a family.
pub fn families_tab(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect, s: f32) {
    let t = app.tokens;
    let ctx = ui.ctx().clone();
    let p = ui.painter().with_clip_rect(area);
    p.rect_filled(area, 0.0, t.panel_bg);
    app.auto.add("motionLibrary.families", area, "Style families");
    let x0 = area.min.x + 12.0 * s;
    let w = (area.width() - 24.0 * s).max(80.0);
    let mut y = area.min.y + 12.0 * s;
    p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, tr("Style families · change one, all follow"), Tokens::semibold(12.5 * s), t.text);
    y += 26.0 * s;
    // The list, read again when the document changes.
    let cache = egui::Id::new("ml-families-cache");
    let rev = app.session.revision;
    let list: Value = match ctx.data(|d| d.get_temp::<(u64, Value)>(cache)) {
        Some((r, v)) if r == rev => v,
        _ => {
            let v = app.session.execute("style.family.list", json!({})).unwrap_or(Value::Null);
            ctx.data_mut(|d| d.insert_temp(cache, (rev, v.clone())));
            v
        }
    };
    let fams = list["families"].as_array().cloned().unwrap_or_default();
    if fams.is_empty() {
        let g = p.layout(
            tr("No families yet. Select the layers that share a style (all your captions), type a family name below and press Link selected.").to_string(),
            Tokens::ui(11.0 * s),
            t.text_dim,
            w,
        );
        let gh = g.size().y;
        p.galley(pos2(x0, y), g, t.text_dim);
        y += gh + 10.0 * s;
    }
    for f in &fams {
        let (Some(name), Some(count)) = (f["family"].as_str(), f["count"].as_u64()) else { continue };
        let size = f["size"].as_f64().unwrap_or(100.0);
        p.text(pos2(x0, y + 9.0 * s), Align2::LEFT_CENTER, name, Tokens::semibold(12.0 * s), t.text);
        p.text(pos2(x0 + 150.0 * s, y + 9.0 * s), Align2::LEFT_CENTER, format!("{count} layer{}", if count == 1 { "" } else { "s" }), Tokens::ui(11.0 * s), t.text_dim);
        // Size: the slider shows the value while dragging; letting go runs the command once.
        let drag_id = egui::Id::new(("ml-family-size", name));
        let mut v: f64 = ctx.data(|d| d.get_temp(drag_id)).unwrap_or(size);
        let sr = Rect::from_min_size(pos2(x0, y + 20.0 * s), vec2(w.min(320.0 * s), 18.0 * s));
        let resp = ui.put(sr, egui::Slider::new(&mut v, 50.0..=250.0).suffix(" %").text(tr("Size")));
        app.auto.add_response(&format!("motionLibrary.families.{name}.size"), &resp, &format!("{name} size"));
        if resp.dragged() || resp.has_focus() {
            ctx.data_mut(|d| d.insert_temp(drag_id, v));
        }
        let commit = resp.drag_stopped() || (resp.changed() && !resp.dragged());
        if commit && (v - size).abs() > 1e-6 {
            ctx.data_mut(|d| d.remove::<f64>(drag_id));
            match crate::menus::invoke(app, &ctx, "style.family.set", json!({"family": name, "size": v})) {
                Ok(r) => {
                    let n = r["layers"].as_u64().unwrap_or(0);
                    app.toast = Some((format!("{n} {name} updated: size {v:.0} %"), ctx.input(|i| i.time)));
                }
                Err(e) => app.ui.status = e,
            }
        } else if !resp.dragged() && !resp.has_focus() {
            ctx.data_mut(|d| d.remove::<f64>(drag_id));
        }
        y += 48.0 * s;
    }
    // Link the selection to a family (new or existing).
    y += 6.0 * s;
    let name_id = egui::Id::new("ml-family-new-name");
    let mut fname: String = ctx.data(|d| d.get_temp(name_id)).unwrap_or_else(|| "Captions".to_string());
    let fr = Rect::from_min_size(pos2(x0, y), vec2((w - 130.0 * s).max(60.0), 22.0 * s));
    let fresp = ui.put(fr, egui::TextEdit::singleline(&mut fname).hint_text(tr("Family name")));
    app.auto.add_response("motionLibrary.families.name", &fresp, "Family name");
    ctx.data_mut(|d| d.insert_temp(name_id, fname.clone()));
    let b = Rect::from_min_size(pos2(fr.max.x + 8.0 * s, y), vec2(120.0 * s, 22.0 * s));
    if kit::button(app, ui, b, "motionLibrary.families.link", "Link selected", true) {
        match crate::menus::invoke(app, &ctx, "style.family.link", json!({"family": fname.trim()})) {
            Ok(r) => {
                app.toast = Some((format!("Linked {} layer(s) to {}", r["linked"].as_u64().unwrap_or(0), fname.trim()), ctx.input(|i| i.time)));
            }
            Err(e) => app.ui.status = e,
        }
    }
    text_animations(app, ui, &ctx, area, y + 40.0 * s, s);
}

/// Text animations (CX8): the Motion Library's text presets as families (`T3 Pop`) with their
/// near / mid / far variants (`T3b Pop · mid`), from `text.presetFamilies` (lane D). A variant
/// applies to the selected text layers (`text.apply`); "Save selected as preset" saves the
/// selected workshop layer (`ml.saveTextPreset`, again = update).
fn text_animations(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect, top: f32, s: f32) {
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(area);
    let x0 = area.min.x + 12.0 * s;
    let w = (area.width() - 24.0 * s).max(80.0);
    let mut y = top;
    p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, tr("Text animations · families with near / mid / far"), Tokens::semibold(12.5 * s), t.text);
    y += 26.0 * s;
    let cache = egui::Id::new("ml-text-families-cache");
    let rev = app.session.revision;
    // TODO(batch1-verify): lane D's listing is `text.presetFamilies` (the brief said `text.presets`).
    let list: Value = match ctx.data(|d| d.get_temp::<(u64, Value)>(cache)) {
        Some((r, v)) if r == rev => v,
        _ => {
            let v = app.session.execute("text.presetFamilies", json!({})).unwrap_or(Value::Null);
            ctx.data_mut(|d| d.insert_temp(cache, (rev, v.clone())));
            v
        }
    };
    let fams = list["families"].as_array().cloned().unwrap_or_default();
    if fams.is_empty() {
        p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, tr("No text presets yet (layers named like “T3b Pop · mid”)."), Tokens::ui(11.0 * s), t.text_dim);
        y += 24.0 * s;
    }
    let mut apply: Option<String> = None;
    for f in &fams {
        if y > area.max.y - 60.0 * s {
            break;
        }
        let fam = f["family"].as_str().unwrap_or("T?");
        let title = f["name"].as_str().unwrap_or("");
        p.text(pos2(x0, y + 10.0 * s), Align2::LEFT_CENTER, format!("{fam} {title}"), Tokens::semibold(11.5 * s), t.text);
        let mut x = x0 + 110.0 * s;
        for v in f["variants"].as_array().cloned().unwrap_or_default() {
            let Some(short) = v["short"].as_str() else { continue };
            let label = match v["distance"].as_str() {
                Some(d) => format!("{short} · {d}"),
                None => short.to_string(),
            };
            let bw = (label.chars().count() as f32 * 6.6 + 16.0) * s;
            if x + bw > x0 + w {
                break;
            }
            let b = Rect::from_min_size(pos2(x, y), vec2(bw, 20.0 * s));
            if kit::button(app, ui, b, &format!("motionLibrary.textAnimations.{short}"), &label, false) {
                apply = Some(short.to_string());
            }
            ui.interact(b, egui::Id::new(("ml-text-variant-tip", short)), Sense::hover()).on_hover_text(v["name"].as_str().unwrap_or(short));
            x += bw + 6.0 * s;
        }
        y += 26.0 * s;
    }
    if let Some(short) = apply {
        match crate::menus::invoke(app, ctx, "text.apply", json!({"preset": short})) {
            Ok(_) => app.toast = Some((format!("{short} applied"), ctx.input(|i| i.time))),
            Err(e) => app.ui.status = e,
        }
    }
    // Save the selected workshop layer as its preset.
    let b = Rect::from_min_size(pos2(x0, y + 4.0 * s), vec2(190.0 * s, 22.0 * s));
    if kit::button(app, ui, b, "motionLibrary.textAnimations.save", "Save selected as preset", false) {
        let layer = app.session.state.selected_layers.first().map(|l| l.0);
        match layer {
            Some(l) => match crate::menus::invoke(app, ctx, "ml.saveTextPreset", json!({"layer": l})) {
                Ok(r) => app.ui.status = format!("Saving “{}” to Motion Library ▸ {}", r["saving"].as_str().unwrap_or(""), r["folder"].as_str().unwrap_or("")),
                Err(e) => app.ui.status = e,
            },
            None => app.ui.status = "Select the workshop layer to save (Creating Text Presets)".into(),
        }
    }
}
