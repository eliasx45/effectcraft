//! Motion Library ▸ Curves (the rail's Curves button): the web panel's Curves workspace in the
//! native panel — a graph editor with two draggable handles, Read keys / Apply / Reverse on the
//! selected keyframes, and the saved and starter curve presets (`curve_*` operations of the
//! Motion Library backend and its EffectCraft plug).
//!
//! - The editor holds one normalized cubic Bézier `[x1, y1, x2, y2]` (time 0→1, value 0→1; the
//!   value handles may overshoot to −5…6). The graph shows the value curve and its speed, as the
//!   web panel. Arrow keys nudge the focused handle (Shift: ×10), Alt-drag skips the 0 % / 100 %
//!   magnet, Escape cancels a drag.
//! - **Read keys** shows the selected keyframes' curve (`curve_analyze` through the plug, which
//!   reads the open project over this app's control port); **Apply** sets their ease to the
//!   editor's curve (`curve_apply`). With **Live** on (the web panel's behaviour) every edit is
//!   applied to the selected keyframes when it ends.
//! - Presets: the saved ones (`curve_list`, `curve_save`, `curve_remove`, `curve_approve` in the
//!   library) and the starters; Ease cards open here to be edited (`editCard`) and the editor's
//!   curve saves as a new Ease card (`saveCard`).
//!
//! The backend runs in python processes: every operation runs on a background thread, one at a
//! time (`busy`), and writes its result into the shared [`CurvesState`].
//! `motionLibrary.curve {op, …}` drives everything ([`command`]).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::motion_library::ViewState;
use crate::i18n::tr;
use crate::theme::Tokens;
use crate::{EffectcraftApp, widgets};

// ── Curve math (curve-math.js) ─────────────────────────────────────────────────────────────────

/// A normalized cubic Bézier `[x1, y1, x2, y2]` from (0, 0) to (1, 1).
pub type Curve = [f64; 4];

/// The editor's curve when nothing else is loaded (Soft ease).
pub const DEFAULT_CURVE: Curve = [0.33, 0.0, 0.67, 1.0];

/// Limits of the handles (`curves.shape`): x1 0.001…1, x2 0…0.999, values −5…6.
const X1: (f64, f64) = (0.001, 1.0);
const X2: (f64, f64) = (0.0, 0.999);
const Y: (f64, f64) = (-5.0, 6.0);

/// The web panel's starter presets (curves.js).
pub const STARTERS: [(&str, Curve); 4] = [
    ("Linear", [1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]),
    ("Soft ease", [0.33, 0.0, 0.67, 1.0]),
    ("Quick entrance", [0.16, 1.0, 0.3, 1.0]),
    ("Slow departure", [0.7, 0.0, 0.84, 0.0]),
];

/// The point at parameter `t` (0…1).
pub fn point(c: &Curve, t: f64) -> [f64; 2] {
    let u = 1.0 - t;
    [3.0 * u * u * t * c[0] + 3.0 * u * t * t * c[2] + t * t * t, 3.0 * u * u * t * c[1] + 3.0 * u * t * t * c[3] + t * t * t]
}

/// 121 points along the curve.
pub fn samples(c: &Curve) -> Vec<[f64; 2]> {
    (0..=120).map(|i| point(c, f64::from(i) / 120.0)).collect()
}

/// The speed (value per time) at each point, by central differences.
pub fn speed(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let last = points.len().saturating_sub(1);
    points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (Some(a), Some(b)) = (points.get(i.saturating_sub(1)), points.get((i + 1).min(last))) else { return [p[0], 0.0] };
            [p[0], (b[1] - a[1]) / (b[0] - a[0]).max(1e-9)]
        })
        .collect()
}

/// The value at time `x` (bisection on the time coordinate).
pub fn value(c: &Curve, x: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..32 {
        let t = (lo + hi) / 2.0;
        if point(c, t)[0] < x {
            lo = t;
        } else {
            hi = t;
        }
    }
    point(c, (lo + hi) / 2.0)[1]
}

/// The limits of coordinate `axis` (0 x1, 1 y1, 2 x2, 3 y2).
fn limits(axis: usize) -> (f64, f64) {
    match axis {
        0 => X1,
        2 => X2,
        _ => Y,
    }
}

/// The curve with every coordinate in its limits (a non-finite one takes the default's).
pub fn clamp_curve(c: Curve) -> Curve {
    let mut out = c;
    for (axis, v) in out.iter_mut().enumerate() {
        let (lo, hi) = limits(axis);
        *v = if v.is_finite() { v.clamp(lo, hi) } else { DEFAULT_CURVE[axis] };
    }
    out
}

/// A curve given as JSON (four finite numbers in the limits), as `curves.shape` checks it.
pub fn parse_curve(v: &Value) -> Result<Curve, String> {
    let bad = || "A curve needs four finite Bezier coordinates.".to_string();
    let list = v.as_array().filter(|a| a.len() == 4).ok_or_else(bad)?;
    let mut c = [0.0; 4];
    for (slot, item) in c.iter_mut().zip(list) {
        *slot = item.as_f64().filter(|x| x.is_finite()).ok_or_else(bad)?;
    }
    if c.iter().enumerate().any(|(axis, v)| {
        let (lo, hi) = limits(axis);
        *v < lo || *v > hi
    }) {
        return Err("Time handles must stay within 0.1–100% influence; value handles must stay between -5 and 6.".into());
    }
    Ok(c)
}

/// The curve played backwards in time.
pub fn reverse(c: &Curve) -> Curve {
    [1.0 - c[2], 1.0 - c[3], 1.0 - c[0], 1.0 - c[1]]
}

/// The cubic closest to sampled motion (`CurveMath.fit`: coordinate descent from four seeds).
pub fn fit(points: &[[f64; 2]]) -> Curve {
    let error = |c: &Curve| -> f64 {
        points
            .iter()
            .step_by(4)
            .map(|p| {
                let d = value(c, p[0]) - p[1];
                d * d
            })
            .sum()
    };
    let mut best = DEFAULT_CURVE;
    let mut score = f64::INFINITY;
    let seeds: [Curve; 4] = [DEFAULT_CURVE, [0.16, 1.0, 0.3, 1.0], [0.7, 0.0, 0.84, 0.0], [1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]];
    for seed in seeds {
        let mut c = seed;
        let mut cost = error(&c);
        let mut step = 0.25;
        while step > 0.0004 {
            for _ in 0..12 {
                let mut improved = false;
                for axis in 0..4 {
                    for sign in [-1.0, 1.0] {
                        let mut n = c;
                        let (lo, hi) = limits(axis);
                        n[axis] = (n[axis] + sign * step).clamp(lo, hi);
                        let next = error(&n);
                        if next < cost {
                            c = n;
                            cost = next;
                            improved = true;
                        }
                    }
                }
                if !improved {
                    break;
                }
            }
            step /= 2.0;
        }
        if cost < score {
            best = c;
            score = cost;
        }
    }
    best
}

/// The normalized curve of an Ease card's two saved keys (`CurveMath.fromKeys`, the same mapping
/// as `effectcraft_plug.interpolation_curve`).
pub fn from_keys(keys: &Value) -> Result<Curve, String> {
    let keys = keys.as_array().filter(|k| k.len() == 2).ok_or("Saved graph is incomplete.")?;
    let (Some(a), Some(b)) = (keys.first(), keys.get(1)) else { return Err("Saved graph is incomplete.".into()) };
    let num = |k: &Value, f: &str| k.get(f).and_then(Value::as_f64).filter(|v| v.is_finite());
    let need = "Graph editing requires different numeric values and increasing keyframe times.";
    let (Some(ta), Some(tb), Some(va), Some(vb)) = (num(a, "time"), num(b, "time"), num(a, "value"), num(b, "value")) else { return Err(need.into()) };
    let (dt, dv) = (tb - ta, vb - va);
    if dt <= 0.0 || dv.abs() < 1e-8 {
        return Err(need.into());
    }
    let ease = |k: &Value, f: &str| k.get(f).and_then(Value::as_array).and_then(|l| l.first()).filter(|e| e.is_object()).cloned();
    let out = ease(a, "out_ease");
    let incoming = ease(b, "in_ease");
    let influence = |e: &Option<Value>| e.as_ref().and_then(|e| num(e, "influence")).map(|i| (i / 100.0).clamp(0.001, 1.0));
    let speed_of = |e: &Option<Value>| e.as_ref().and_then(|e| num(e, "speed"));
    let x1 = influence(&out).unwrap_or(1.0 / 3.0);
    let x2 = influence(&incoming).map_or(2.0 / 3.0, |i| 1.0 - i);
    let linear = |k: &Value, f: &str| k.get(f).and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("LINEAR"));
    let y1 = match speed_of(&out) {
        Some(sp) if !linear(a, "out_type") => sp * dt * x1 / dv,
        _ => x1,
    };
    let y2 = match speed_of(&incoming) {
        Some(sp) if !linear(b, "in_type") => 1.0 - sp * dt * (1.0 - x2) / dv,
        _ => x2,
    };
    let c = [x1, y1, x2, y2];
    if !c.iter().all(|v| v.is_finite()) {
        return Err("Saved graph has invalid easing handles.".into());
    }
    Ok(c)
}

/// The value at time `t` of sampled points (linear between samples).
pub fn sample_value(points: &[[f64; 2]], t: f64) -> f64 {
    let n = points.len();
    if n < 2 {
        return points.first().map_or(t, |p| p[1]);
    }
    let mut i = 1;
    while i < n - 1 && points.get(i).is_some_and(|p| p[0] < t) {
        i += 1;
    }
    let (Some(a), Some(b)) = (points.get(i - 1), points.get(i)) else { return t };
    a[1] + (b[1] - a[1]) * (t - a[0]) / (b[0] - a[0]).max(1e-9)
}

/// The 0 % / 100 % magnet of a dragged value handle: snaps within 8 points of an end and lets go
/// past 14 (a wider release zone, so a small movement doesn't flicker past the end). `bypass`
/// (Alt) turns it off.
pub fn magnetic(value: f64, snap: &mut Option<f64>, points_per_value: f64, bypass: bool) -> f64 {
    if bypass {
        *snap = None;
        return value;
    }
    if let Some(at) = *snap
        && (value - at).abs() * points_per_value <= 14.0
    {
        return at;
    }
    *snap = None;
    let end = if value.abs() < (value - 1.0).abs() { 0.0 } else { 1.0 };
    if (value - end).abs() * points_per_value <= 8.0 {
        *snap = Some(end);
        return end;
    }
    value
}

/// `"a, b ,, c"` → `["a", "b", "c"]`.
fn word_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// Words from JSON: a list of strings or one comma-separated string.
fn json_words(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => word_list(s),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::trim).filter(|w| !w.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

// ── State ──────────────────────────────────────────────────────────────────────────────────────

/// A saved curve preset (`curve_list`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub curve: Curve,
    pub uses: Vec<String>,
    pub aliases: Vec<String>,
    pub approved: bool,
}

impl Preset {
    /// A preset from the backend; `None` when it isn't one (no id or no valid curve).
    fn from_json(v: &Value) -> Option<Preset> {
        let curve = parse_curve(v.get("curve")?).ok()?;
        let text = |f: &str| v.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
        let id = text("id");
        if id.is_empty() {
            return None;
        }
        Some(Preset {
            id,
            name: text("name"),
            description: text("description"),
            curve,
            uses: json_words(v.get("uses")),
            aliases: json_words(v.get("aliases")),
            approved: v.get("approved").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn list(v: &Value) -> Vec<Preset> {
        v.get("presets").and_then(Value::as_array).map(|l| l.iter().take(2000).filter_map(Preset::from_json).collect()).unwrap_or_default()
    }

    fn matches(&self, query: &str) -> bool {
        let text = [self.name.as_str(), self.description.as_str()]
            .into_iter()
            .chain(self.uses.iter().map(String::as_str))
            .chain(self.aliases.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        query.to_lowercase().split_whitespace().all(|t| text.contains(t))
    }
}

/// One channel (property or dimension) of the selected keyframes (`curve_analyze`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub name: String,
    /// Normalized samples (time, value), 0…1 each way.
    pub points: Vec<[f64; 2]>,
    /// The keys' own Bézier, when they have one.
    pub curve: Option<Curve>,
    pub constant: bool,
    pub hold: bool,
}

/// What `curve_analyze` read from the selected keyframes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Analysis {
    pub key_count: u32,
    pub target_count: u32,
    pub channels: Vec<Channel>,
    pub blocked: String,
    pub property: String,
    pub expression: bool,
    /// Seconds between the keys.
    pub duration: f64,
}

impl Analysis {
    /// From the plug's answer; points are cleaned (finite, at most 4096) and channels capped.
    fn from_json(v: &Value) -> Result<Analysis, String> {
        let bad = || "EffectCraft sent an unexpected curve analysis.".to_string();
        let list = v.get("channels").and_then(Value::as_array).filter(|c| !c.is_empty()).ok_or_else(bad)?;
        let num = |v: &Value, f: &str| v.get(f).and_then(Value::as_f64).filter(|x| x.is_finite());
        let channels = list
            .iter()
            .take(32)
            .map(|c| Channel {
                name: c.get("name").and_then(Value::as_str).unwrap_or("Property").to_string(),
                points: c
                    .get("points")
                    .and_then(Value::as_array)
                    .map(|p| {
                        p.iter()
                            .take(4096)
                            .filter_map(|q| {
                                let q = q.as_array()?;
                                let (x, y) = (q.first()?.as_f64()?, q.get(1)?.as_f64()?);
                                (x.is_finite() && y.is_finite()).then_some([x, y])
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                curve: c.get("curve").filter(|c| !c.is_null()).and_then(|c| {
                    let list = c.as_array().filter(|l| l.len() == 4)?;
                    let mut out = [0.0; 4];
                    for (slot, v) in out.iter_mut().zip(list) {
                        *slot = v.as_f64().filter(|x| x.is_finite())?;
                    }
                    Some(clamp_curve(out))
                }),
                constant: c.get("constant").and_then(Value::as_bool).unwrap_or(false),
                hold: c.get("hold").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect();
        let count = |f: &str| v.get(f).and_then(Value::as_u64).map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX));
        Ok(Analysis {
            key_count: count("key_count"),
            target_count: count("target_count"),
            channels,
            blocked: v.get("blocked").and_then(Value::as_str).unwrap_or_default().to_string(),
            property: v.get("property").and_then(Value::as_str).unwrap_or_default().to_string(),
            expression: v.get("expression").and_then(Value::as_bool).unwrap_or(false),
            duration: num(v, "duration").unwrap_or(0.0).max(0.0),
        })
    }
}

/// The Save form: a preset (`preset`) or a new Ease card (`card`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveForm {
    pub target: String,
    pub name: String,
    pub description: String,
    pub uses: String,
    pub aliases: String,
    pub approved: bool,
    /// The collection a new card goes into.
    pub category: String,
}

/// The Ease card being edited (`editCard`): its keys as saved, checked again on save.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditingCard {
    pub id: String,
    pub name: String,
    pub keys: Value,
}

/// A handle drag in progress.
#[derive(Clone, Debug, PartialEq)]
struct Drag {
    handle: usize,
    initial: Curve,
    low: f64,
    high: f64,
}

/// The Curves workspace (UI state; `motionLibrary.curve {"op":"state"}` returns it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CurvesState {
    pub curve: Curve,
    /// The selected keyframes, as last read or applied.
    pub analysis: Option<Analysis>,
    /// The graph shows the keyframes' sampled motion (not the editor's Bézier).
    pub actual: bool,
    pub channel: usize,
    /// Edited since loaded or saved.
    pub dirty: bool,
    /// The preset tile shown as chosen (`<id>` or `starter:<name>`).
    pub active_preset: Option<String>,
    /// The saved preset loaded in the editor (Approve works on it).
    pub chosen: Option<Preset>,
    /// Motion preview: seconds (0…10) and frames per second (30 or 60).
    pub duration: f64,
    pub fps: u32,
    pub presets: Vec<Preset>,
    pub search: String,
    /// Edit mode of the preset lists (remove / hide buttons).
    pub managing: bool,
    pub hidden_starters: Vec<String>,
    pub form: Option<SaveForm>,
    pub editing: Option<EditingCard>,
    pub status: String,
    pub error: bool,
    /// Every edit is applied to the selected keyframes when it ends (the web panel's behaviour).
    pub live: bool,
    /// The running operation (`analyze`, `apply`, `save`…).
    #[serde(skip_deserializing)]
    pub busy: Option<String>,
    #[serde(skip)]
    loaded_at: Option<f64>,
    #[serde(skip)]
    last_shown: f64,
    #[serde(skip)]
    drag: Option<Drag>,
    #[serde(skip)]
    snap: Option<f64>,
    /// When the pending live apply runs (debounced edits).
    #[serde(skip)]
    live_due: Option<f64>,
    /// The motion preview's start time while it plays.
    #[serde(skip)]
    playing: Option<f64>,
    /// Content heights of the two columns (last frame).
    #[serde(skip)]
    heights: [f32; 2],
    /// An Ease card saved with a new curve: (curve before, curve now, card name), for
    /// `graph.relinkUpdate` (batch 1, AX3: the moves linked to it follow).
    #[serde(skip)]
    pub relink: Option<(Curve, Curve, String)>,
}

impl Default for CurvesState {
    fn default() -> Self {
        CurvesState {
            curve: DEFAULT_CURVE,
            analysis: None,
            actual: false,
            channel: 0,
            dirty: false,
            active_preset: None,
            chosen: None,
            duration: 1.0,
            fps: 30,
            presets: Vec::new(),
            search: String::new(),
            managing: false,
            hidden_starters: Vec::new(),
            form: None,
            editing: None,
            status: String::new(),
            error: false,
            live: true,
            busy: None,
            loaded_at: None,
            last_shown: f64::NEG_INFINITY,
            drag: None,
            snap: None,
            live_due: None,
            playing: None,
            heights: [0.0; 2],
            relink: None,
        }
    }
}

const WAIT: &str = "Wait for the current curve action.";

impl CurvesState {
    fn say(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.error = false;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.error = true;
    }

    /// The graph's handles can be edited (one segment, or a preview curve).
    pub fn editable(&self) -> bool {
        !self.actual || self.analysis.as_ref().is_none_or(|a| a.key_count == 2)
    }

    fn channel(&self) -> Option<&Channel> {
        self.analysis.as_ref().and_then(|a| a.channels.get(self.channel))
    }

    /// The points the graph and the preview show.
    fn points(&self) -> Vec<[f64; 2]> {
        match self.channel() {
            Some(c) if self.actual && c.points.len() >= 2 => c.points.clone(),
            _ => samples(&self.curve),
        }
    }

    fn expression_status(&self) -> &'static str {
        if self.analysis.as_ref().is_some_and(|a| a.expression) {
            "Editing keyframe easing. Expressions stay enabled; the graph shows values before expressions."
        } else {
            ""
        }
    }

    fn analysis_status(&mut self) {
        let Some(a) = &self.analysis else { return };
        let mut parts: Vec<String> = Vec::new();
        if a.target_count > 1 {
            parts.push(format!("{} segments selected. Apply uses this curve for the whole batch.", a.target_count));
        }
        if a.key_count == 2 && self.channel().is_some_and(|c| c.curve.is_none()) {
            parts.push("Handles approximate the sampled curve. Drag to edit the preview.".into());
        }
        parts.push(self.expression_status().into());
        parts.push(a.blocked.clone());
        let text = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
        self.say(text);
    }

    fn applied_status(&mut self) {
        let n = self.analysis.as_ref().map_or(1, |a| a.target_count.max(1));
        let extra = self.expression_status();
        let text = if extra.is_empty() { format!("Applied to {n} segment(s).") } else { format!("Applied to {n} segment(s). {extra}") };
        self.say(text);
    }

    /// The curve of channel `i` of the analysis (its own Bézier, else fitted to its samples).
    fn analyzed_curve(&self, i: usize) -> Curve {
        let Some(a) = &self.analysis else { return self.curve };
        match a.channels.get(i) {
            Some(Channel { curve: Some(c), .. }) => *c,
            Some(c) if a.key_count == 2 && c.points.len() >= 2 => clamp_curve(fit(&c.points)),
            _ => DEFAULT_CURVE,
        }
    }

    /// Show what the plug read (`curve_analyze` / `curve_apply` with `refresh`). `keep_curve`:
    /// the editor changed meanwhile (a live apply is pending), so its curve stays.
    fn accept(&mut self, result: &Value, keep_curve: bool) -> Result<(), String> {
        let a = Analysis::from_json(result)?;
        self.playing = None;
        self.analysis = Some(a);
        if keep_curve {
            self.channel = 0;
            return Ok(());
        }
        self.channel = 0;
        self.actual = true;
        self.curve = self.analyzed_curve(0);
        Ok(())
    }

    /// An edit of the curve (drag, nudge, numbers, preset, reverse).
    fn change(&mut self, c: Curve, now: f64, live_delay: f64) {
        self.playing = None;
        self.active_preset = None;
        self.curve = clamp_curve(c);
        self.actual = false;
        self.dirty = true;
        let text = match self.snap {
            Some(at) => format!("Snapped to {:.0}%", at * 100.0),
            None => self.analysis.as_ref().map(|a| a.blocked.clone()).filter(|b| !b.is_empty()).unwrap_or_else(|| self.expression_status().to_string()),
        };
        self.say(text);
        self.edited(now, live_delay);
    }

    /// After an edit: an Ease card says how to save it; otherwise a live apply is scheduled.
    fn edited(&mut self, now: f64, live_delay: f64) {
        if let Some(e) = &self.editing {
            let text = format!("Editing “{}”. Save changes to update this card.", e.name);
            if self.snap.is_none() {
                self.say(text);
            }
        } else if self.live {
            self.live_due = Some(now + live_delay);
        }
    }

    /// Load a preset (a saved one when `saved`).
    fn choose(&mut self, key: String, curve: Curve, saved: Option<Preset>, now: f64) {
        self.playing = None;
        self.active_preset = Some(key);
        self.chosen = saved;
        self.curve = clamp_curve(curve);
        self.actual = false;
        self.dirty = self.editing.is_some();
        self.say("");
        self.edited(now, 0.0);
    }

    fn reverse_current(&mut self, now: f64) -> Result<(), String> {
        if !self.editable() {
            return Err("Select one segment to edit its curve.".into());
        }
        self.change(reverse(&self.curve), now, 0.0);
        Ok(())
    }

    /// Open the Save form (`target` preset or card) with the web panel's defaults.
    fn open_form(&mut self, target: &str, category: &str) {
        if let Some(f) = &mut self.form {
            f.target = target.to_string();
            return;
        }
        let fitted = self.actual && self.channel().is_some_and(|c| c.curve.is_none() && c.points.len() >= 2);
        if fitted {
            self.curve = clamp_curve(fit(&self.points()));
            self.actual = false;
            self.dirty = true;
        }
        let analyzed = self.analysis.as_ref().map(|a| (a.property.clone(), self.channel().map(|c| c.name.clone()).unwrap_or_default()));
        let name = match (&self.chosen, &analyzed) {
            (Some(p), _) => format!("{} copy", p.name),
            (None, Some((prop, ch))) => format!("{prop} {ch}").trim().to_string(),
            _ => String::new(),
        };
        let description = match (&analyzed, &self.chosen) {
            (Some((prop, ch)), _) if fitted => format!("Approximation of analyzed {prop} ({ch})."),
            (_, Some(p)) => p.description.clone(),
            _ => String::new(),
        };
        self.form = Some(SaveForm {
            target: target.to_string(),
            name,
            description,
            uses: self.chosen.as_ref().map(|p| p.uses.join(", ")).unwrap_or_default(),
            aliases: self.chosen.as_ref().map(|p| p.aliases.join(", ")).unwrap_or_default(),
            approved: false,
            category: category.to_string(),
        });
    }

    fn json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Some(o) = v.as_object_mut() {
            o.insert("editable".into(), json!(self.editable()));
            o.insert("playing".into(), json!(self.playing.is_some()));
            o.insert("starters".into(), starters_json(&self.hidden_starters));
        }
        v
    }
}

fn starters_json(hidden: &[String]) -> Value {
    Value::Array(STARTERS.iter().filter(|(n, _)| !hidden.iter().any(|h| h == n)).map(|(n, c)| json!({"name": n, "curve": c})).collect())
}

type Shared = Arc<Mutex<CurvesState>>;

/// The workspace's state, kept with the egui context (the panel and `motionLibrary.curve` share it).
fn shared(ctx: &egui::Context) -> Shared {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with(egui::Id::new("ml-curves-state"), || Arc::new(Mutex::new(CurvesState::default()))).clone())
}

fn lock(s: &Shared) -> MutexGuard<'_, CurvesState> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── Background operations ──────────────────────────────────────────────────────────────────────

type Then = Box<dyn FnOnce(&mut CurvesState, Result<Value, String>) + Send>;

/// Run `op` (a backend or plug call) off the UI thread, then `then` on the state; one at a time.
fn spawn(ctx: &egui::Context, st: &mut CurvesState, label: &str, op: impl FnOnce() -> Result<Value, String> + Send + 'static, then: Then) {
    st.busy = Some(label.to_string());
    #[cfg(not(target_arch = "wasm32"))]
    {
        let shared = shared(ctx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = op();
            {
                let mut st = lock(&shared);
                st.busy = None;
                then(&mut st, result);
            }
            ctx.request_repaint();
        });
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        let result = op();
        st.busy = None;
        then(st, result);
    }
}

fn backend(op: &'static str, args: Value) -> impl FnOnce() -> Result<Value, String> + Send + 'static {
    move || super::motion_library::backend_rpc(op, args)
}

fn plug(port: u16, op: &'static str, args: Value) -> impl FnOnce() -> Result<Value, String> + Send + 'static {
    move || super::motion_library::plug_rpc(port, op, args)
}

/// A library change, then the saved presets again: `{result, presets}`.
fn then_list(op: impl FnOnce() -> Result<Value, String> + Send + 'static) -> impl FnOnce() -> Result<Value, String> + Send + 'static {
    move || {
        let result = op()?;
        let list = super::motion_library::backend_rpc("curve_list", json!({}))?;
        Ok(json!({"result": result, "presets": list.get("presets").cloned().unwrap_or_else(|| json!([]))}))
    }
}

/// Rebuild the library index after a card changed (the cards grid reads it).
fn reindexed(op: impl FnOnce() -> Result<Value, String> + Send + 'static) -> impl FnOnce() -> Result<Value, String> + Send + 'static {
    move || {
        let result = op()?;
        super::motion_library::reindex_library();
        Ok(result)
    }
}

fn start_load(ctx: &egui::Context, st: &mut CurvesState, now: f64) {
    st.loaded_at = Some(now);
    spawn(
        ctx,
        st,
        "list",
        backend("curve_list", json!({})),
        Box::new(|st, r| match r {
            Ok(v) if v.get("presets").is_some_and(Value::is_array) => {
                st.presets = Preset::list(&v);
                if let Some(c) = &st.chosen {
                    st.chosen = st.presets.iter().find(|p| p.id == c.id).cloned();
                }
            }
            Ok(_) => st.fail("Could not load curve presets. Try again."),
            Err(e) => st.fail(e),
        }),
    );
}

/// Read keys (`match_preview`: also take their timing for the motion preview).
fn start_analyze(app: &EffectcraftApp, ctx: &egui::Context, st: &mut CurvesState, match_preview: bool) -> Result<(), String> {
    let port = super::motion_library::control_port(app)?;
    spawn(
        ctx,
        st,
        if match_preview { "match" } else { "analyze" },
        plug(port, "curve_analyze", json!({})),
        Box::new(move |st, r| match r.and_then(|v| st.accept(&v, false)) {
            Ok(()) => {
                st.analysis_status();
                if match_preview {
                    let d = st.analysis.as_ref().map_or(1.0, |a| a.duration);
                    st.duration = (d.min(10.0) * 10.0).round() / 10.0;
                    st.say(if d > 10.0 { "Duration capped at 10 s." } else { "Timing matched." });
                }
            }
            Err(e) => {
                st.analysis = None;
                st.actual = false;
                st.fail(e);
            }
        }),
    );
    Ok(())
}

/// The `curve_apply` arguments: the editor's curve, or an approved saved preset by `id` + `usage`.
fn apply_args(curve: Option<Curve>, id: Option<&str>, usage: Option<&str>) -> Result<Value, String> {
    match (id, curve) {
        (Some(id), _) => Ok(json!({"id": id, "usage": usage.unwrap_or_default(), "apply_to_current": true, "refresh": true})),
        (None, Some(c)) => Ok(json!({"apply_to_current": true, "curve": parse_curve(&json!(c))?, "refresh": true})),
        (None, None) => Err("give a `curve` or a saved preset's `id` and `usage`".into()),
    }
}

fn start_apply(app: &EffectcraftApp, ctx: &egui::Context, st: &mut CurvesState, args: Value) -> Result<(), String> {
    let port = super::motion_library::control_port(app)?;
    st.live_due = None;
    spawn(
        ctx,
        st,
        "apply",
        plug(port, "curve_apply", args),
        Box::new(|st, r| match r.and_then(|v| st.accept(&v, st.live_due.is_some())) {
            Ok(()) => st.applied_status(),
            Err(e) => {
                st.analysis = None;
                st.actual = false;
                st.fail(e);
            }
        }),
    );
    Ok(())
}

/// Live: read the selected keys, then give them the editor's curve (unless they can't take one).
fn start_live(app: &EffectcraftApp, ctx: &egui::Context, st: &mut CurvesState) {
    let port = match super::motion_library::control_port(app) {
        Ok(p) => p,
        Err(e) => {
            st.say(format!("Preview only: {e}"));
            return;
        }
    };
    let curve = st.curve;
    let op = move || -> Result<Value, String> {
        let target = super::motion_library::plug_rpc(port, "curve_analyze", json!({})).map_err(|e| format!("Preview only: {e}"))?;
        let blocked = target.get("blocked").and_then(Value::as_str).unwrap_or_default().to_string();
        let constant =
            target.get("channels").and_then(Value::as_array).is_some_and(|c| c.iter().all(|c| c.get("constant").and_then(Value::as_bool).unwrap_or(false)));
        if !blocked.is_empty() || constant {
            let why = if blocked.is_empty() { "Preview only: selected keys have equal values.".to_string() } else { blocked };
            return Ok(json!({"previewOnly": why, "analysis": target}));
        }
        super::motion_library::plug_rpc(port, "curve_apply", json!({"apply_to_current": true, "curve": curve, "refresh": true}))
    };
    spawn(
        ctx,
        st,
        "live",
        op,
        Box::new(|st, r| match r {
            Ok(v) if v.get("previewOnly").is_some() => {
                if let Ok(a) = Analysis::from_json(v.get("analysis").unwrap_or(&Value::Null)) {
                    st.analysis = Some(a);
                }
                st.say(v.get("previewOnly").and_then(Value::as_str).unwrap_or_default());
            }
            Ok(v) => match st.accept(&v, st.live_due.is_some()) {
                Ok(()) => st.applied_status(),
                Err(e) => st.fail(e),
            },
            Err(e) => {
                st.analysis = None;
                st.say(e);
            }
        }),
    );
}

fn start_save(ctx: &egui::Context, st: &mut CurvesState, form: &SaveForm, curve: Curve) {
    let args = json!({
        "name": form.name.trim(),
        "description": form.description,
        "uses": word_list(&form.uses),
        "aliases": word_list(&form.aliases),
        "curve": curve,
        "approved": form.approved,
    });
    spawn(
        ctx,
        st,
        "save",
        then_list(backend("curve_save", args)),
        Box::new(|st, r| match r {
            Ok(v) => {
                st.presets = Preset::list(&v);
                let saved = v.get("result").and_then(Preset::from_json);
                let approved = saved.as_ref().is_some_and(|p| p.approved);
                st.active_preset = saved.as_ref().map(|p| p.id.clone());
                st.chosen = saved;
                st.dirty = false;
                st.form = None;
                st.say(if approved { "Saved and approved." } else { "Saved draft." });
            }
            Err(e) => st.fail(e),
        }),
    );
}

fn start_remove(ctx: &egui::Context, st: &mut CurvesState, id: String) {
    let removed = id.clone();
    spawn(
        ctx,
        st,
        "remove",
        then_list(backend("curve_remove", json!({"id": id}))),
        Box::new(move |st, r| match r {
            Ok(v) => {
                st.presets = Preset::list(&v);
                if st.chosen.as_ref().is_some_and(|c| c.id == removed) {
                    st.chosen = None;
                }
                st.say("Preset removed.");
            }
            Err(e) => st.fail(e),
        }),
    );
}

fn start_approve(ctx: &egui::Context, st: &mut CurvesState, id: String, approved: bool, uses: Option<Vec<String>>) {
    let mut args = json!({"id": id, "approved": approved});
    if let (Some(u), Some(o)) = (uses, args.as_object_mut()) {
        o.insert("uses".into(), json!(u));
    }
    spawn(
        ctx,
        st,
        "approve",
        then_list(backend("curve_approve", args)),
        Box::new(|st, r| match r {
            Ok(v) => {
                st.presets = Preset::list(&v);
                let p = v.get("result").and_then(Preset::from_json);
                let text = match &p {
                    Some(p) if p.approved => format!("Approved for: {}", p.uses.join(", ")),
                    _ => "Approval revoked.".into(),
                };
                if p.is_some() {
                    st.chosen = p;
                }
                st.say(text);
            }
            Err(e) => st.fail(e),
        }),
    );
}

/// Open Ease card `id` in the editor (`describe`, then its keys as a curve).
fn start_edit_card(ctx: &egui::Context, st: &mut CurvesState, id: String) {
    spawn(
        ctx,
        st,
        "editCard",
        backend("describe", json!({"id": id})),
        Box::new(|st, r| {
            let opened = r.and_then(|entry| {
                if entry.get("kind").and_then(Value::as_str) != Some("interpolation") {
                    return Err("Choose a saved graph to edit.".to_string());
                }
                let keys = entry.get("payload").and_then(|p| p.get("keys")).cloned().unwrap_or(Value::Null);
                let curve = from_keys(&keys)?;
                let card = EditingCard {
                    id: entry.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    name: entry.get("name").and_then(Value::as_str).unwrap_or("Ease").to_string(),
                    keys,
                };
                Ok((card, curve))
            });
            match opened {
                Ok((card, curve)) => {
                    st.playing = None;
                    st.say(format!("Editing “{}”. Drag the handles, then Save changes.", card.name));
                    st.editing = Some(card);
                    st.analysis = None;
                    st.actual = false;
                    st.chosen = None;
                    st.active_preset = None;
                    st.dirty = false;
                    st.form = None;
                    st.live_due = None;
                    st.curve = clamp_curve(curve);
                }
                Err(e) => st.fail(e),
            }
        }),
    );
}

/// Save the edited Ease card (`update_interpolation`, refused if the card changed meanwhile).
fn start_save_changes(ctx: &egui::Context, st: &mut CurvesState) -> Result<(), String> {
    let card = st.editing.clone().ok_or("No Ease card is open: open one with editCard")?;
    let args = json!({"id": card.id, "expected_keys": card.keys, "curve": parse_curve(&json!(st.curve))?});
    // The curve before and after, so the moves linked to the card follow it (AX3).
    let before = from_keys(&card.keys).ok();
    let after = st.curve;
    spawn(
        ctx,
        st,
        "saveChanges",
        reindexed(backend("update_interpolation", args)),
        Box::new(move |st, r| match r {
            Ok(v) => {
                if let Some(before) = before.filter(|b| *b != after) {
                    st.relink = Some((before, after, card.name.clone()));
                }
                let entry = v.get("entry").unwrap_or(&v);
                if let Some(e) = &mut st.editing {
                    if let Some(keys) = entry.get("payload").and_then(|p| p.get("keys")) {
                        e.keys = keys.clone();
                    }
                    if let Some(n) = entry.get("name").and_then(Value::as_str) {
                        e.name = n.to_string();
                    }
                }
                st.dirty = false;
                let name = st.editing.as_ref().map(|e| e.name.clone()).unwrap_or_default();
                st.say(format!("Saved changes to “{name}”."));
            }
            Err(e) => st.fail(e),
        }),
    );
    Ok(())
}

/// Runs a pending relink (an Ease card saved with a new curve): every move linked to the card
/// gets the new curve, one undo step (`graph.relinkUpdate`). Called every frame (batch 1, AX3).
pub fn drain_relink(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let shared = shared(ctx);
    let Ok(mut st) = shared.try_lock() else { return };
    let Some((before, after, name)) = st.relink.take() else { return };
    drop(st);
    let params = json!({"from": before, "to": after, "name": name});
    match crate::menus::invoke(app, ctx, "graph.relinkUpdate", params) {
        Ok(v) => {
            let n = v.get("updated").and_then(Value::as_u64).unwrap_or(0);
            if n > 0 {
                app.toast = Some((format!("“{name}” changed: {n} linked move{} updated", if n == 1 { "" } else { "s" }), ctx.input(|i| i.time)));
            }
        }
        Err(e) => app.ui.status = e,
    }
}

/// The `save_asset` arguments of a new Ease card made from `curve`.
fn card_args(name: &str, category: &str, description: &str, curve: Curve) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give this item a name.".into());
    }
    let category = if category.trim().is_empty() { "My motion" } else { category.trim() };
    Ok(json!({"kind": "interpolation", "name": name, "category": category, "description": description, "curve": parse_curve(&json!(curve))?}))
}

/// Save `curve` as a new Ease card (the plug's `save_asset`; without a control port the library
/// backend saves it, as a drawn graph needs no project).
fn start_save_card(app: &EffectcraftApp, ctx: &egui::Context, st: &mut CurvesState, args: Value) {
    let port = super::motion_library::control_port(app).ok();
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let category = args.get("category").and_then(Value::as_str).unwrap_or_default().to_string();
    let op = move || match port {
        Some(p) => super::motion_library::plug_rpc(p, "save_asset", args),
        None => super::motion_library::backend_rpc("save_asset", args),
    };
    spawn(
        ctx,
        st,
        "saveCard",
        reindexed(op),
        Box::new(move |st, r| match r {
            Ok(_) => {
                st.form = None;
                st.dirty = false;
                st.say(format!("Saved Ease card “{name}” to {category}."));
            }
            Err(e) => st.fail(e),
        }),
    );
}

// ── Painting ───────────────────────────────────────────────────────────────────────────────────

/// The speed curve's colour (the web panel's green).
const GREEN: Color32 = Color32::from_rgb(0x9f, 0xd4, 0x9b);

/// A text button that can be disabled; true when clicked (and enabled).
#[allow(clippy::too_many_arguments)]
fn button(app: &mut EffectcraftApp, ui: &mut egui::Ui, r: Rect, auto: &str, text: &str, primary: bool, enabled: bool, s: f32) -> bool {
    let t = app.tokens;
    let resp = ui.interact(r, egui::Id::new(("ml-curves-btn", auto)), if enabled { Sense::click() } else { Sense::hover() });
    let bg = match (primary, enabled, resp.hovered()) {
        (true, true, true) => t.accent_hover,
        (true, true, false) => t.accent,
        (_, false, _) => t.hover.gamma_multiply(0.6),
        (false, true, true) => t.pressed,
        (false, true, false) => t.hover,
    };
    ui.painter().rect_filled(r, r.height() / 2.0, bg);
    let color = if !enabled {
        t.text_faint
    } else if primary {
        Color32::WHITE
    } else {
        t.text
    };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, tr(text), Tokens::medium(11.5 * s), color);
    app.auto.add(auto, r, text);
    enabled && resp.clicked()
}

/// Lay `labels` out left to right from `x0` in width `w`, wrapping; their rects and the height.
fn flow(ui: &egui::Ui, labels: &[&str], at: Pos2, w: f32, s: f32) -> (Vec<Rect>, f32) {
    let h = 24.0 * s;
    let gap = 6.0 * s;
    let (mut x, mut y) = (at.x, at.y);
    let mut out = Vec::with_capacity(labels.len());
    for label in labels {
        let tw = ui.painter().layout_no_wrap(tr(label).to_string(), Tokens::medium(11.5 * s), Color32::WHITE).size().x;
        let bw = (tw + 22.0 * s).max(56.0 * s).min(w);
        if x > at.x && x + bw > at.x + w {
            x = at.x;
            y += h + gap;
        }
        out.push(Rect::from_min_size(pos2(x, y), vec2(bw, h)));
        x += bw + gap;
    }
    let height = if labels.is_empty() { 0.0 } else { y - at.y + h };
    (out, height)
}

/// A mini curve (value and speed) in `r`, as the web panel's preset tiles.
fn paint_mini(p: &egui::Painter, r: Rect, c: &Curve, accent: Color32, s: f32) {
    let values = samples(c);
    let speeds = speed(&values);
    for (series, color, width) in [(&speeds, GREEN.gamma_multiply(0.8), 1.2 * s), (&values, accent, 1.8 * s)] {
        let low = series.iter().map(|q| q[1]).fold(0.0_f64, f64::min);
        let high = series.iter().map(|q| q[1]).fold(1.0_f64, f64::max);
        let span = (high - low).max(1e-9);
        let pts: Vec<Pos2> = series.iter().map(|q| pos2(r.min.x + r.width() * q[0] as f32, r.max.y - r.height() * ((q[1] - low) / span) as f32)).collect();
        p.add(egui::Shape::line(pts, Stroke::new(width, color)));
    }
}

/// The graph: grid, speed and value curves, the two handles. Edits change `st`.
#[allow(clippy::too_many_lines)]
fn graph(app: &mut EffectcraftApp, ui: &mut egui::Ui, st: &mut CurvesState, g: Rect, now: f64, s: f32) {
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(g.intersect(ui.clip_rect()));
    p.rect_filled(g, CornerRadius::same(8), t.field_bg);
    p.rect_stroke(g, CornerRadius::same(8), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    let plot = Rect::from_min_max(pos2(g.min.x + 14.0 * s, g.min.y + 24.0 * s), pos2(g.max.x - 14.0 * s, g.max.y - 22.0 * s));
    if plot.width() < 20.0 || plot.height() < 20.0 {
        return;
    }
    let points = st.points();
    let speeds = speed(&points);
    let speed_low = speeds.iter().map(|q| q[1]).fold(0.0_f64, f64::min);
    let speed_high = speeds.iter().map(|q| q[1]).fold(1.0_f64, f64::max);
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    for v in points.iter().map(|q| q[1]).chain([st.curve[1], st.curve[3]]) {
        low = low.min(v);
        high = high.max(v);
    }
    if let Some(d) = &st.drag {
        (low, high) = (d.low, d.high);
    }
    let span = (high - low).max(1e-9);
    let map = |q: [f64; 2]| pos2(plot.min.x + plot.width() * q[0] as f32, plot.max.y - plot.height() * ((q[1] - low) / span) as f32);
    let grid = Stroke::new(1.0, t.separator.gamma_multiply(0.7));
    for i in 0..=4 {
        let f = i as f32 / 4.0;
        let x = plot.min.x + plot.width() * f;
        let y = plot.min.y + plot.height() * f;
        p.line_segment([pos2(x, plot.min.y), pos2(x, plot.max.y)], grid);
        p.line_segment([pos2(plot.min.x, y), pos2(plot.max.x, y)], grid);
    }
    // 0 % and 100 % when the curve overshoots.
    for v in [0.0, 1.0] {
        if low < -1e-6 || high > 1.0 + 1e-6 {
            let y = map([0.0, v]).y;
            p.line_segment([pos2(plot.min.x, y), pos2(plot.max.x, y)], Stroke::new(1.0, t.text_faint.gamma_multiply(0.5)));
        }
    }
    if let Some(at) = st.snap {
        let y = map([0.0, at]).y;
        p.add(egui::Shape::dashed_line(&[pos2(plot.min.x, y), pos2(plot.max.x, y)], Stroke::new(1.0, t.accent), 4.0 * s, 4.0 * s));
    }
    let sspan = (speed_high - speed_low).max(1e-9);
    let speed_pts: Vec<Pos2> =
        speeds.iter().map(|q| pos2(plot.min.x + plot.width() * q[0] as f32, plot.max.y - plot.height() * ((q[1] - speed_low) / sspan) as f32)).collect();
    p.add(egui::Shape::line(speed_pts, Stroke::new(1.8 * s, GREEN.gamma_multiply(0.8))));
    let value_pts: Vec<Pos2> = points.iter().map(|q| map(*q)).collect();
    p.add(egui::Shape::line(value_pts, Stroke::new(2.6 * s, t.accent)));
    let axis = Tokens::ui(10.0 * s);
    p.text(
        pos2(plot.min.x, g.min.y + 12.0 * s),
        Align2::LEFT_CENTER,
        format!("{} {:.0}% · {} {:.2}×", tr("Value"), high * 100.0, tr("Speed peak"), speed_high),
        axis.clone(),
        t.text_dim,
    );
    p.text(pos2(plot.min.x, g.max.y - 10.0 * s), Align2::LEFT_CENTER, "0", axis.clone(), t.text_dim);
    p.text(pos2(plot.max.x, g.max.y - 10.0 * s), Align2::RIGHT_CENTER, tr("Time → 100%"), axis, t.text_dim);
    let bg = ui.interact(g, egui::Id::new("ml-curves-graph"), Sense::click());
    app.auto.add("motionLibrary.curves.graph", g, "Easing graph");
    if !st.editable() {
        return;
    }
    let ends = [[0.0, 0.0], [1.0, 1.0]];
    let mut focused: Option<usize> = None;
    for h in 0..2 {
        let (Some(end), Some(&hx), Some(&hy)) = (ends.get(h), st.curve.get(h * 2), st.curve.get(h * 2 + 1)) else { continue };
        let a = map(*end);
        let mut b = map([hx, hy]);
        b.y = b.y.clamp(plot.min.y, plot.max.y);
        p.line_segment([a, b], Stroke::new(1.5 * s, GREEN));
        let id = egui::Id::new(("ml-curves-handle", h));
        let hit = Rect::from_center_size(b, vec2(22.0 * s, 22.0 * s));
        let resp = ui.interact(hit, id, Sense::click_and_drag());
        let name = if h == 0 { "out" } else { "in" };
        app.auto.add_response(&format!("motionLibrary.curves.handle.{name}"), &resp, &format!("{hx:.3}, {hy:.3}"));
        if resp.drag_started() || resp.clicked() {
            resp.request_focus();
        }
        let has_focus = resp.has_focus();
        if has_focus {
            focused = Some(h);
            ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() }));
        }
        if resp.drag_started() {
            st.playing = None;
            st.snap = None;
            st.live_due = None;
            st.drag = Some(Drag { handle: h, initial: st.curve, low, high });
        }
        if resp.dragged()
            && let Some(d) = st.drag.clone().filter(|d| d.handle == h)
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let alt = ui.input(|i| i.modifiers.alt);
            let span = (d.high - d.low).max(1e-9);
            let ppv = f64::from(plot.height()) / span;
            let mut c = st.curve;
            let (lo, hi) = limits(h * 2);
            let x = (f64::from(pos.x - plot.min.x) / f64::from(plot.width()).max(1.0)).clamp(lo, hi);
            let raw = d.low + f64::from(plot.max.y - pos.y) / f64::from(plot.height()).max(1.0) * span;
            let y = magnetic(raw, &mut st.snap, ppv, alt).clamp(Y.0, Y.1);
            if let Some(cx) = c.get_mut(h * 2) {
                *cx = x;
            }
            if let Some(cy) = c.get_mut(h * 2 + 1) {
                *cy = y;
            }
            if c != st.curve {
                st.change(c, now, f64::INFINITY);
            }
        }
        if resp.drag_stopped() && st.drag.as_ref().is_some_and(|d| d.handle == h) {
            st.drag = None;
            st.snap = None;
            if st.live && st.editing.is_none() && st.live_due.is_some() {
                st.live_due = Some(now);
            }
        }
        let r = 7.0 * s;
        p.circle_filled(b, r + 2.0 * s, t.field_bg);
        p.circle_filled(b, r, if resp.hovered() || resp.dragged() { GREEN.gamma_multiply(1.15) } else { GREEN });
        if has_focus {
            p.circle_stroke(b, r + 2.0 * s, Stroke::new(2.0 * s, Color32::WHITE));
        }
    }
    // Escape cancels a drag (back to where it started).
    if let Some(d) = st.drag.clone()
        && ui.input(|i| i.key_pressed(egui::Key::Escape))
    {
        st.drag = None;
        st.snap = None;
        st.live_due = None;
        st.curve = d.initial;
    }
    if bg.clicked() && focused.is_none() {
        ui.memory_mut(|m| {
            if let Some(f) = m.focused()
                && (f == egui::Id::new(("ml-curves-handle", 0)) || f == egui::Id::new(("ml-curves-handle", 1)))
            {
                m.surrender_focus(f);
            }
        });
    }
    // Arrow keys nudge the focused handle (Shift: 0.1).
    if let Some(h) = focused
        && st.drag.is_none()
    {
        let (step, keys) = ui.input(|i| {
            let step = if i.modifiers.shift { 0.1 } else { 0.01 };
            let k = [egui::Key::ArrowLeft, egui::Key::ArrowRight, egui::Key::ArrowDown, egui::Key::ArrowUp].map(|k| i.key_pressed(k));
            (step, k)
        });
        let mut c = st.curve;
        let deltas = [(0, -step), (0, step), (1, -step), (1, step)];
        for (pressed, (axis, d)) in keys.iter().zip(deltas) {
            if *pressed && let Some(v) = c.get_mut(h * 2 + axis) {
                *v += d;
            }
        }
        if c != st.curve {
            st.snap = None;
            st.change(c, now, 0.4);
        }
    }
}

/// The motion preview: a dot moving with the curve from Start to End.
fn preview_strip(app: &mut EffectcraftApp, ui: &mut egui::Ui, st: &mut CurvesState, r: Rect, now: f64, s: f32) {
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(r.intersect(ui.clip_rect()));
    p.rect_filled(r, CornerRadius::same(8), t.field_bg);
    p.rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    let play = Rect::from_min_max(r.min, pos2(r.min.x + 32.0 * s, r.max.y));
    let resp = ui.interact(play, egui::Id::new("ml-curves-play"), Sense::click());
    if resp.hovered() {
        p.rect_filled(play.shrink(1.0), CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 }, t.hover);
    }
    p.line_segment([pos2(play.max.x, r.min.y), pos2(play.max.x, r.max.y)], Stroke::new(1.0, t.separator));
    let playing = st.playing.is_some();
    let c = play.center();
    let k = 6.0 * s;
    if playing {
        p.rect_filled(Rect::from_center_size(c, vec2(k * 1.6, k * 1.6)), CornerRadius::same(1), t.text);
    } else {
        p.add(egui::Shape::convex_polygon(vec![pos2(c.x - k * 0.7, c.y - k), pos2(c.x - k * 0.7, c.y + k), pos2(c.x + k, c.y)], t.text, Stroke::NONE));
    }
    app.auto.add_response("motionLibrary.curves.preview", &resp, if playing { "Stop motion preview" } else { "Play motion preview" });
    let resp = resp.on_hover_text(tr(if playing { "Stop motion preview" } else { "Play motion preview" }));
    if resp.clicked() {
        st.playing = if playing { None } else { Some(now) };
    }
    // The stage.
    let stage = Rect::from_min_max(pos2(play.max.x, r.min.y), r.max);
    let frame = stage.shrink2(vec2(stage.width() * 0.1, stage.height() * 0.1));
    p.rect_stroke(frame, CornerRadius::ZERO, Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    let start = pos2(stage.min.x + stage.width() * 0.2, stage.center().y);
    let end = pos2(stage.min.x + stage.width() * 0.75, stage.center().y);
    p.add(egui::Shape::dashed_line(&[start, end], Stroke::new(1.0, t.text_faint), 3.0 * s, 4.0 * s));
    let size = (stage.height() * 0.4).min(stage.width() * 0.1).max(8.0 * s);
    for (at, label) in [(start, "Start"), (end, "End")] {
        let box_r = Rect::from_center_size(at, vec2(size, size));
        let pts = [box_r.left_top(), box_r.right_top(), box_r.right_bottom(), box_r.left_bottom(), box_r.left_top()];
        p.add(egui::Shape::dashed_line(&pts, Stroke::new(1.0, t.accent.gamma_multiply(0.6)), 3.0 * s, 3.0 * s));
        p.text(pos2(box_r.min.x, box_r.max.y + 2.0 * s), Align2::LEFT_TOP, tr(label), Tokens::ui(9.0 * s), t.text_faint);
    }
    let mut v = 0.0;
    if let Some(t0) = st.playing {
        let fps = f64::from(st.fps.max(1));
        let elapsed = (now - t0).max(0.0);
        let done = st.duration <= 0.0 || elapsed >= st.duration;
        let index = (elapsed * fps).round();
        let at = if done { 1.0 } else { (index / fps / st.duration).min(1.0) };
        v = sample_value(&st.points(), at);
        if done {
            st.playing = None;
            v = 0.0;
        } else {
            let next = ((index + 1.0) / fps - elapsed).max(0.001);
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(next));
        }
    }
    let dot = pos2(start.x + (end.x - start.x) * v as f32, start.y);
    p.rect_filled(Rect::from_center_size(dot, vec2(size, size)), CornerRadius::same(3), t.accent);
}

/// The duration slider (0…10 s); true when changed.
fn duration_slider(app: &mut EffectcraftApp, ui: &mut egui::Ui, st: &mut CurvesState, r: Rect, s: f32) -> bool {
    let t = app.tokens;
    let label_w = 40.0 * s;
    let track = Rect::from_min_max(r.min, pos2(r.max.x - label_w - 6.0 * s, r.max.y));
    let resp = ui.interact(track, egui::Id::new("ml-curves-duration"), Sense::click_and_drag());
    let mut changed = false;
    if (resp.dragged() || resp.clicked())
        && let Some(x) = resp.interact_pointer_pos().map(|p| p.x)
    {
        let f = f64::from(((x - track.min.x) / track.width().max(1.0)).clamp(0.0, 1.0));
        let d = (f * 100.0).round() / 10.0;
        if (d - st.duration).abs() > 1e-9 {
            st.duration = d;
            st.playing = None;
            changed = true;
        }
    }
    let y = track.center().y;
    let f = (st.duration / 10.0).clamp(0.0, 1.0) as f32;
    let line = Rect::from_min_max(pos2(track.min.x, y - 1.5 * s), pos2(track.max.x, y + 1.5 * s));
    let p = ui.painter();
    p.rect_filled(line, CornerRadius::same(2), t.field_border);
    p.rect_filled(Rect::from_min_max(line.min, pos2(track.min.x + f * track.width(), line.max.y)), CornerRadius::same(2), t.accent);
    p.circle_filled(pos2(track.min.x + f * track.width(), y), 6.0 * s, if resp.hovered() || resp.dragged() { t.accent_hover } else { t.accent });
    let text = format!("{:.1} s", st.duration);
    p.text(pos2(r.max.x, y), Align2::RIGHT_CENTER, &text, Tokens::ui(11.0 * s), t.text_dim);
    app.auto.add_response("motionLibrary.curves.duration", &resp, &text);
    changed
}

/// A labelled text field (`rows` > 1: multi-line); the height it takes.
#[allow(clippy::too_many_arguments)]
fn field(app: &mut EffectcraftApp, ui: &mut egui::Ui, at: Pos2, w: f32, auto: &str, label: &str, text: &mut String, hint: &str, rows: usize, s: f32) -> f32 {
    let t = app.tokens;
    ui.painter().text(at, Align2::LEFT_TOP, tr(label), Tokens::ui(11.0 * s), t.text_dim);
    let h = if rows > 1 { 18.0 * s * rows as f32 + 8.0 * s } else { 24.0 * s };
    let r = Rect::from_min_size(pos2(at.x, at.y + 16.0 * s), vec2(w, h));
    let edit = if rows > 1 { egui::TextEdit::multiline(text).desired_rows(rows) } else { egui::TextEdit::singleline(text) };
    let resp = ui.put(r, edit.hint_text(tr(hint)).font(Tokens::ui(11.5 * s)));
    app.auto.add_response(auto, &resp, label);
    16.0 * s + h + 8.0 * s
}

/// The Save form (preset or Ease card) at `at`; its height.
fn save_form(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, st: &mut CurvesState, at: Pos2, w: f32, s: f32) -> f32 {
    let t = app.tokens;
    let Some(mut form) = st.form.clone() else { return 0.0 };
    let card = form.target == "card";
    let mut y = at.y;
    ui.painter().line_segment([pos2(at.x, y), pos2(at.x + w, y)], Stroke::new(1.0, t.separator));
    y += 12.0 * s;
    ui.painter().text(pos2(at.x, y), Align2::LEFT_TOP, tr(if card { "Save as an Ease card" } else { "Save a curve" }), Tokens::semibold(12.0 * s), t.text);
    y += 24.0 * s;
    y += field(app, ui, pos2(at.x, y), w, "motionLibrary.curves.form.name", "Name", &mut form.name, "Soft card entrance", 1, s);
    y += field(
        app,
        ui,
        pos2(at.x, y),
        w,
        "motionLibrary.curves.form.description",
        "Description",
        &mut form.description,
        "Starts quickly, then settles gently without overshoot.",
        3,
        s,
    );
    if card {
        y += field(app, ui, pos2(at.x, y), w, "motionLibrary.curves.form.category", "Collection", &mut form.category, "My motion", 1, s);
    } else {
        y += field(app, ui, pos2(at.x, y), w, "motionLibrary.curves.form.uses", "Use for", &mut form.uses, "Card entrance, text reveal", 1, s);
        y += field(app, ui, pos2(at.x, y), w, "motionLibrary.curves.form.aliases", "Search terms", &mut form.aliases, "Soft landing, gentle reveal", 1, s);
        form.approved = super::panel_kit::checkbox(
            app,
            ui,
            pos2(at.x, y),
            "motionLibrary.curves.form.approved",
            tr("Let agents choose this curve for these uses."),
            form.approved,
        );
        y += 26.0 * s;
    }
    let busy = st.busy.is_some();
    let (rects, h) = flow(ui, &["Cancel", if card { "Save card" } else { "Save curve" }], pos2(at.x, y), w, s);
    let mut cancel = false;
    let mut submit = false;
    if let Some(r) = rects.first() {
        cancel = button(app, ui, *r, "motionLibrary.curves.form.cancel", "Cancel", false, true, s);
    }
    if let Some(r) = rects.get(1) {
        submit = button(
            app,
            ui,
            *r,
            "motionLibrary.curves.form.save",
            if card { "Save card" } else { "Save curve" },
            true,
            !busy && !form.name.trim().is_empty(),
            s,
        );
    }
    y += h + 8.0 * s;
    st.form = Some(form.clone());
    if cancel {
        st.form = None;
    } else if submit {
        if card {
            match card_args(&form.name, &form.category, &form.description, st.curve) {
                Ok(args) => start_save_card(app, ctx, st, args),
                Err(e) => st.fail(e),
            }
        } else {
            let curve = st.curve;
            start_save(ctx, st, &form, curve);
        }
    }
    y - at.y
}

/// The editor column at `r` (its width; top-left); returns its content height.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn editor(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    st: &mut CurvesState,
    view: &mut ViewState,
    r: Rect,
    wide: bool,
    now: f64,
    s: f32,
) -> f32 {
    let t = app.tokens;
    let (x0, w) = (r.min.x, r.width());
    let mut y = r.min.y;
    let busy = st.busy.is_some();
    // Legend, busy note and the channel menu.
    let p = ui.painter().clone();
    let legend = p.text(pos2(x0, y + 11.0 * s), Align2::LEFT_CENTER, tr("Value"), Tokens::ui(11.0 * s), t.accent);
    let speed_r = p.text(pos2(legend.max.x + 6.0 * s, y + 11.0 * s), Align2::LEFT_CENTER, format!("＋ {}", tr("Speed")), Tokens::ui(11.0 * s), GREEN);
    if let Some(b) = &st.busy {
        let text = match b.as_str() {
            "list" => "Loading presets…",
            "analyze" | "match" => "Reading keys…",
            "apply" | "live" => "Applying…",
            _ => "Saving…",
        };
        p.text(pos2(speed_r.max.x + 10.0 * s, y + 11.0 * s), Align2::LEFT_CENTER, tr(text), Tokens::ui(11.0 * s), t.text_faint);
    }
    if let Some(a) = st.analysis.clone().filter(|a| !a.channels.is_empty()) {
        let names: Vec<&str> = a.channels.iter().map(|c| c.name.as_str()).collect();
        let dw = (160.0 * s).min(w * 0.45);
        let dr = Rect::from_min_size(pos2(x0 + w - dw, y), vec2(dw, 22.0 * s));
        if let Some(i) = super::panel_kit::dropdown(app, ui, dr, "motionLibrary.curves.channel", &names, st.channel.min(names.len().saturating_sub(1))) {
            st.playing = None;
            st.channel = i;
            st.actual = true;
            st.curve = st.analyzed_curve(i);
            st.analysis_status();
        }
    }
    y += 28.0 * s;
    // The graph.
    let gh = (w / 1.25).clamp(150.0 * s, if wide { 340.0 * s } else { 250.0 * s });
    let g = Rect::from_min_size(pos2(x0, y), vec2(w, gh));
    graph(app, ui, st, g, now, s);
    y += gh + 6.0 * s;
    // Numeric readout: the two handles (drag or click to type).
    if st.editable() {
        let labels = [("Out", 0usize), ("In", 2usize)];
        let mut x = x0;
        let mut c = st.curve;
        for (label, base) in labels {
            let lr = p.text(pos2(x, y + 10.0 * s), Align2::LEFT_CENTER, tr(label), Tokens::ui(11.0 * s), t.text_dim);
            x = lr.max.x + 6.0 * s;
            for (k, name) in [(base, if base == 0 { "x1" } else { "x2" }), (base + 1, if base == 0 { "y1" } else { "y2" })] {
                let (lo, hi) = limits(k);
                let v = c.get(k).copied().unwrap_or_default();
                let (nr, nv, _) = widgets::hot_number_at(ui, pos2(x, y + 1.0 * s), egui::Id::new(("ml-curves-num", k)), v, 0.005, (lo, hi), 3, "", &t);
                app.auto.add(&format!("motionLibrary.curves.{name}"), nr, &format!("{v:.3}"));
                if let (Some(nv), Some(slot)) = (nv, c.get_mut(k)) {
                    *slot = nv.clamp(lo, hi);
                }
                x = nr.max.x + 6.0 * s;
            }
            x += 8.0 * s;
        }
        if c != st.curve {
            st.snap = None;
            st.change(c, now, 0.5);
        }
        y += 22.0 * s;
    }
    // Status.
    if !st.status.is_empty() {
        let color = if st.error { t.danger } else { t.text_dim };
        let galley = p.layout(st.status.clone(), Tokens::ui(11.0 * s), color, w.max(40.0));
        let h = galley.size().y;
        p.galley(pos2(x0, y + 3.0 * s), galley, color);
        app.auto.add("motionLibrary.curves.status", Rect::from_min_size(pos2(x0, y), vec2(w, h + 6.0 * s)), &st.status);
        y += h + 8.0 * s;
    }
    // Motion preview.
    let ph = if wide { 84.0 * s } else { 64.0 * s };
    preview_strip(app, ui, st, Rect::from_min_size(pos2(x0, y), vec2(w, ph)), now, s);
    y += ph + 8.0 * s;
    // Match preview, FPS, duration.
    let editing = st.editing.is_some();
    let (rects, h) = flow(ui, &["Match preview", "30", "60"], pos2(x0, y), w, s);
    if let Some(r) = rects.first()
        && button(app, ui, *r, "motionLibrary.curves.match", "Match preview", false, !busy && !editing, s)
        && let Err(e) = start_analyze(app, ctx, st, true)
    {
        st.fail(e);
    }
    for (i, fps) in [(1usize, 30u32), (2, 60)] {
        if let Some(r) = rects.get(i)
            && button(app, ui, *r, &format!("motionLibrary.curves.fps.{fps}"), &fps.to_string(), st.fps == fps, true, s)
        {
            st.fps = fps;
            st.playing = None;
        }
    }
    y += h + 8.0 * s;
    duration_slider(app, ui, st, Rect::from_min_size(pos2(x0, y), vec2(w, 18.0 * s)), s);
    y += 26.0 * s;
    // Actions.
    let labels: Vec<&str> = if editing {
        vec!["Reverse", "Save changes", if st.dirty { "Cancel editing" } else { "Done" }]
    } else {
        vec!["Read keys", "Apply", "Reverse", "Save preset", "Save as card"]
    };
    let (rects, h) = flow(ui, &labels, pos2(x0, y), w, s);
    for (label, r) in labels.iter().zip(rects) {
        match *label {
            "Read keys" => {
                if button(app, ui, r, "motionLibrary.curves.analyze", label, false, !busy, s)
                    && let Err(e) = start_analyze(app, ctx, st, false)
                {
                    st.fail(e);
                }
            }
            "Apply" => {
                if button(app, ui, r, "motionLibrary.curves.apply", label, true, !busy && st.editable(), s) {
                    let result = apply_args(Some(st.curve), None, None).and_then(|args| start_apply(app, ctx, st, args));
                    if let Err(e) = result {
                        st.fail(e);
                    }
                }
            }
            "Reverse" => {
                if button(app, ui, r, "motionLibrary.curves.reverse", label, false, st.editable(), s)
                    && let Err(e) = st.reverse_current(now)
                {
                    st.fail(e);
                }
            }
            "Save preset" => {
                if button(app, ui, r, "motionLibrary.curves.savePreset", label, false, true, s) {
                    st.open_form("preset", "");
                }
            }
            "Save as card" => {
                if button(app, ui, r, "motionLibrary.curves.saveCard", label, false, true, s) {
                    let folder = if view.folder.is_empty() { "My motion".to_string() } else { view.folder.clone() };
                    st.open_form("card", &folder);
                }
            }
            "Save changes" => {
                if button(app, ui, r, "motionLibrary.curves.update", label, true, !busy && st.dirty, s)
                    && let Err(e) = start_save_changes(ctx, st)
                {
                    st.fail(e);
                }
            }
            _ => {
                if button(app, ui, r, "motionLibrary.curves.done", label, false, !busy, s) {
                    finish_editing(st);
                    view.workspace.clear();
                }
            }
        }
    }
    y += h + 8.0 * s;
    // Easy Ease on the selected keyframes (the old Curves popover).
    let eases = ["Easy Ease (F9)", "Easy Ease In", "Easy Ease Out"];
    let (rects, h) = flow(ui, &eases, pos2(x0, y), w, s);
    for ((label, r), (auto, cmd)) in eases.iter().zip(rects).zip([
        ("motionLibrary.curves.easyEase", "keys.easyEase"),
        ("motionLibrary.curves.easeIn", "keys.easyEaseIn"),
        ("motionLibrary.curves.easeOut", "keys.easyEaseOut"),
    ]) {
        if button(app, ui, r, auto, label, false, true, s) && super::panel_kit::exec(app, cmd, json!({})).is_some() {
            let text = format!("{label} on the selected keyframes");
            app.ui.status = text.clone();
            st.say(text);
        }
    }
    y += h + 8.0 * s;
    if !editing {
        let on = super::panel_kit::checkbox(app, ui, pos2(x0, y), "motionLibrary.curves.live", tr("Live: edits apply to the selected keyframes"), st.live);
        if on != st.live {
            st.live = on;
            st.live_due = None;
        }
        y += 24.0 * s;
    }
    y += save_form(app, ui, ctx, st, pos2(x0, y), w, s);
    y - r.min.y
}

fn finish_editing(st: &mut CurvesState) {
    st.editing = None;
    st.dirty = false;
    st.analysis = None;
    st.actual = false;
    st.playing = None;
    st.say("");
}

/// A grid of preset tiles; the height. `saved`: the library's presets (else the starters).
#[allow(clippy::too_many_arguments)]
fn tiles(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    st: &mut CurvesState,
    items: &[(String, String, Curve, Option<Preset>)],
    add: bool,
    at: Pos2,
    w: f32,
    now: f64,
    s: f32,
) -> f32 {
    let t = app.tokens;
    let side = 48.0 * s;
    let gap = 6.0 * s;
    let cols = (((w + gap) / (side + gap)).floor() as usize).max(1);
    let busy = st.busy.is_some();
    let count = items.len() + usize::from(add);
    let mut remove: Option<(String, bool)> = None;
    let mut pick: Option<(String, Curve, Option<Preset>)> = None;
    for (i, (key, name, curve, saved)) in items.iter().enumerate() {
        let (row, col) = (i / cols, i % cols);
        let r = Rect::from_min_size(pos2(at.x + col as f32 * (side + gap), at.y + row as f32 * (side + gap)), vec2(side, side));
        // Loading a preset is local (only removing a saved one waits for the library).
        let resp = ui.interact(r, egui::Id::new(("ml-curves-tile", key)), Sense::click());
        let active = st.active_preset.as_deref() == Some(key.as_str());
        let bg = if active {
            t.accent.gamma_multiply(0.22)
        } else if resp.hovered() {
            t.hover
        } else {
            t.header_bg
        };
        ui.painter().rect_filled(r, CornerRadius::same(6), bg);
        ui.painter().rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, if active { t.accent } else { t.separator }), egui::StrokeKind::Inside);
        paint_mini(ui.painter(), r.shrink(7.0 * s), curve, t.accent, s);
        let tip = match saved {
            Some(p) => format!("{name} · {}", if p.approved { tr("Approved") } else { tr("Draft") }),
            None => name.clone(),
        };
        let auto = match saved {
            Some(p) => format!("motionLibrary.curves.preset.{}", p.id),
            None => format!("motionLibrary.curves.starter.{name}"),
        };
        app.auto.add(&auto, r, &tip);
        let resp = resp.on_hover_text(tip.as_str());
        if resp.clicked() {
            pick = Some((key.clone(), *curve, saved.clone()));
        }
        if st.managing {
            let b = Rect::from_center_size(pos2(r.max.x - 2.0 * s, r.min.y + 2.0 * s), vec2(18.0 * s, 18.0 * s));
            let br = ui.interact(b, egui::Id::new(("ml-curves-tile-remove", key)), if busy && saved.is_some() { Sense::hover() } else { Sense::click() });
            ui.painter().circle_filled(b.center(), 9.0 * s, t.danger);
            ui.painter().text(b.center(), Align2::CENTER_CENTER, "−", Tokens::semibold(12.0 * s), Color32::WHITE);
            let label = if saved.is_some() { format!("Remove {name}") } else { format!("Hide {name}") };
            app.auto.add(&format!("{auto}.remove"), b, &label);
            if br.on_hover_text(label.as_str()).clicked() {
                remove = Some((saved.as_ref().map_or_else(|| name.clone(), |p| p.id.clone()), saved.is_some()));
                pick = None;
            }
        }
    }
    if add {
        let i = items.len();
        let r = Rect::from_min_size(pos2(at.x + (i % cols) as f32 * (side + gap), at.y + (i / cols) as f32 * (side + gap)), vec2(side, side));
        let resp = ui.interact(r, egui::Id::new("ml-curves-add"), Sense::click());
        let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
        ui.painter().add(egui::Shape::dashed_line(&pts, Stroke::new(1.0, t.text_faint), 3.0 * s, 3.0 * s));
        let c = r.center();
        let k = 7.0 * s;
        let stroke = Stroke::new(1.6 * s, if resp.hovered() { t.text } else { t.text_dim });
        ui.painter().line_segment([pos2(c.x - k, c.y), pos2(c.x + k, c.y)], stroke);
        ui.painter().line_segment([pos2(c.x, c.y - k), pos2(c.x, c.y + k)], stroke);
        app.auto.add("motionLibrary.curves.add", r, "Save current curve");
        if resp.on_hover_text(tr("Save current curve")).clicked() {
            st.open_form("preset", "");
        }
    }
    if let Some((key, curve, saved)) = pick {
        st.choose(key, curve, saved, now);
    }
    if let Some((what, saved)) = remove {
        if saved {
            start_remove(ctx, st, what);
        } else {
            st.hidden_starters.push(what);
            st.say("Preset hidden.");
        }
    }
    let rows = count.div_ceil(cols);
    if rows == 0 { 0.0 } else { rows as f32 * (side + gap) - gap }
}

/// The presets column at `r`; its content height.
#[allow(clippy::too_many_lines)]
fn presets(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, st: &mut CurvesState, r: Rect, now: f64, s: f32) -> f32 {
    let t = app.tokens;
    let (x0, w) = (r.min.x, r.width());
    let mut y = r.min.y;
    let busy = st.busy.is_some();
    ui.painter().text(pos2(x0, y + 11.0 * s), Align2::LEFT_CENTER, tr("Saved"), Tokens::semibold(12.0 * s), t.text);
    let mr = Rect::from_min_size(pos2(x0 + w - 60.0 * s, y), vec2(60.0 * s, 22.0 * s));
    if button(app, ui, mr, "motionLibrary.curves.manage", if st.managing { "Done" } else { "Edit" }, st.managing, true, s) {
        st.managing = !st.managing;
    }
    if st.managing && ui.input(|i| i.key_pressed(egui::Key::Escape)) && st.drag.is_none() {
        st.managing = false;
    }
    y += 30.0 * s;
    let sr = Rect::from_min_size(pos2(x0, y), vec2(w, 24.0 * s));
    let resp = widgets::search_field(ui, sr, &mut st.search, tr("Search…"), &t);
    app.auto.add_response("motionLibrary.curves.search", &resp, "Search curves");
    y += 32.0 * s;
    let query = st.search.clone();
    let saved: Vec<(String, String, Curve, Option<Preset>)> =
        st.presets.iter().filter(|p| p.matches(&query)).map(|p| (p.id.clone(), p.name.clone(), p.curve, Some(p.clone()))).collect();
    if saved.is_empty() {
        let text = if st.busy.as_deref() == Some("list") && st.presets.is_empty() {
            "Loading…"
        } else if st.presets.is_empty() {
            "No saved curves yet: Save preset keeps the editor's curve here."
        } else {
            "No saved curve matches."
        };
        let galley = ui.painter().layout(tr(text).to_string(), Tokens::ui(11.0 * s), t.text_faint, w.max(40.0));
        let h = galley.size().y;
        ui.painter().galley(pos2(x0, y), galley, t.text_faint);
        y += h + 10.0 * s;
    } else {
        y += tiles(app, ui, ctx, st, &saved, false, pos2(x0, y), w, now, s) + 12.0 * s;
    }
    ui.painter().text(pos2(x0, y + 6.0 * s), Align2::LEFT_CENTER, tr("PRESETS"), Tokens::semibold(10.0 * s), t.text_faint);
    y += 18.0 * s;
    let starters: Vec<(String, String, Curve, Option<Preset>)> =
        STARTERS.iter().filter(|(n, _)| !st.hidden_starters.iter().any(|h| h == n)).map(|(n, c)| (format!("starter:{n}"), n.to_string(), *c, None)).collect();
    y += tiles(app, ui, ctx, st, &starters, true, pos2(x0, y), w, now, s) + 10.0 * s;
    if !st.hidden_starters.is_empty() {
        let (rects, h) = flow(ui, &["Restore starters"], pos2(x0, y), w, s);
        if let Some(r) = rects.first()
            && button(app, ui, *r, "motionLibrary.curves.restore", "Restore starters", false, true, s)
        {
            st.hidden_starters.clear();
            st.say("Starters restored.");
        }
        y += h + 10.0 * s;
    }
    // The chosen saved preset: what it's for, and agent approval.
    if let Some(p) = st.chosen.clone() {
        ui.painter().line_segment([pos2(x0, y), pos2(x0 + w, y)], Stroke::new(1.0, t.separator));
        y += 10.0 * s;
        let badge = if p.approved { tr("Approved") } else { tr("Draft") };
        let nr = ui.painter().text(pos2(x0, y), Align2::LEFT_TOP, &p.name, Tokens::semibold(12.0 * s), t.text);
        ui.painter().text(
            pos2(nr.max.x + 8.0 * s, nr.center().y),
            Align2::LEFT_CENTER,
            badge,
            Tokens::ui(10.0 * s),
            if p.approved { GREEN } else { t.text_faint },
        );
        y += nr.height() + 6.0 * s;
        let mut lines = Vec::new();
        if !p.description.is_empty() {
            lines.push(p.description.clone());
        }
        if !p.uses.is_empty() {
            lines.push(format!("{} {}", tr("Use for:"), p.uses.join(", ")));
        }
        if !lines.is_empty() {
            let galley = ui.painter().layout(lines.join("\n"), Tokens::ui(11.0 * s), t.text_dim, w.max(40.0));
            let h = galley.size().y;
            ui.painter().galley(pos2(x0, y), galley, t.text_dim);
            y += h + 8.0 * s;
        }
        let label = if p.approved { "Revoke approval" } else { "Approve for agents" };
        let (rects, h) = flow(ui, &[label], pos2(x0, y), w, s);
        if let Some(r) = rects.first()
            && button(app, ui, *r, "motionLibrary.curves.approve", label, false, !busy && !st.dirty, s)
        {
            if !p.approved && p.uses.is_empty() {
                st.fail("Save a new copy with intended uses before approving.");
            } else {
                start_approve(ctx, st, p.id.clone(), !p.approved, None);
            }
        }
        y += h + 8.0 * s;
    }
    y - r.min.y
}

/// A vertically scrolling column in `rect`; `body` draws into the content rect (top-left, width)
/// and returns the content's height (used next frame).
fn column(ui: &mut egui::Ui, rect: Rect, salt: &str, height: f32, body: impl FnOnce(&mut egui::Ui, Rect) -> f32) -> f32 {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical()
        .id_salt(("ml-curves-col", salt))
        .auto_shrink([false, false])
        .scroll_source(egui::scroll_area::ScrollSource { drag: egui::scroll_area::DragScroll::Never, ..egui::scroll_area::ScrollSource::ALL })
        .max_height(rect.height())
        .show(&mut child, |ui| {
            let w = (ui.available_width() - 8.0).max(40.0);
            let (r, _) = ui.allocate_exact_size(vec2(w, height.max(1.0)), Sense::hover());
            body(ui, Rect::from_min_size(r.min, vec2(w, r.height())))
        })
        .inner
}

/// The Curves workspace in `rect` (right of the rail).
pub fn workspace(app: &mut EffectcraftApp, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect, view: &mut ViewState, s: f32) {
    let shared = shared(ctx);
    let mut guard = lock(&shared);
    let st = &mut *guard;
    let now = ctx.input(|i| i.time);
    let t = app.tokens;
    ui.painter().with_clip_rect(rect).rect_filled(rect, 0.0, t.panel_bg);
    app.auto.add("motionLibrary.curves", rect, "Curves workspace");
    // Opening the workspace reads the saved presets (again after 30 s).
    let reopened = now - st.last_shown > 1.0;
    st.last_shown = now;
    if st.busy.is_none() && (st.loaded_at.is_none() || (reopened && st.loaded_at.is_some_and(|at| now - at > 30.0))) {
        start_load(ctx, st, now);
    }
    if reopened {
        st.playing = None;
    }
    let inner = rect.shrink(12.0 * s);
    if inner.width() < 60.0 || inner.height() < 60.0 {
        return;
    }
    let wide = inner.width() >= 600.0 * s;
    let [h_editor, h_presets] = st.heights;
    let mut heights = st.heights;
    if wide {
        let gap = 16.0 * s;
        let ew = ((inner.width() - gap) * 0.58).round();
        let er = Rect::from_min_max(inner.min, pos2(inner.min.x + ew, inner.max.y));
        let pr = Rect::from_min_max(pos2(er.max.x + gap, inner.min.y), inner.max);
        heights[0] = column(ui, er, "editor", h_editor, |ui, r| editor(app, ui, ctx, st, view, r, true, now, s));
        heights[1] = column(ui, pr, "presets", h_presets, |ui, r| presets(app, ui, ctx, st, r, now, s));
    } else {
        heights[0] = column(ui, inner, "stacked", h_editor, |ui, r| {
            let h = editor(app, ui, ctx, st, view, r, false, now, s);
            let y = r.min.y + h + 10.0 * s;
            ui.painter().line_segment([pos2(r.min.x, y), pos2(r.max.x, y)], Stroke::new(1.0, app.tokens.separator));
            let pr = Rect::from_min_max(pos2(r.min.x, y + 12.0 * s), pos2(r.max.x, r.max.y.max(y + 12.0 * s)));
            h + 22.0 * s + presets(app, ui, ctx, st, pr, now, s)
        });
    }
    if heights != st.heights {
        st.heights = heights;
        ctx.request_repaint();
    }
    // A live apply after an edit (debounced; never mid-drag, one operation at a time).
    if let Some(due) = st.live_due
        && st.drag.is_none()
        && due.is_finite()
    {
        if !st.live || st.editing.is_some() {
            st.live_due = None;
        } else if now >= due && st.busy.is_none() {
            st.live_due = None;
            start_live(app, ctx, st);
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((due - now).clamp(0.02, 1.0)));
        }
    }
}

// ── motionLibrary.curve ────────────────────────────────────────────────────────────────────────

/// Open Ease card `id` in the Curves workspace (its two keys as the editor's curve): the card
/// menu's Edit graph and `motionLibrary.curve {"op":"editCard"}`.
pub(crate) fn edit_card(ctx: &egui::Context, view: &mut ViewState, id: &str) -> Result<Value, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("missing `id`: the Ease card to edit".into());
    }
    let shared = shared(ctx);
    let Ok(mut st) = shared.try_lock() else { return Err(WAIT.into()) };
    if st.busy.is_some() {
        return Err(WAIT.into());
    }
    start_edit_card(ctx, &mut st, id.to_string());
    view.workspace = "curves".into();
    Ok(json!({"started": "editCard", "id": id}))
}

fn text<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// `motionLibrary.curve {op, …}`: the Curves workspace for agents.
///
/// - `state`: the editor (curve, analysis, presets, form, status, busy…).
/// - `list`: the saved and starter presets (and reloads the saved ones in the background).
/// - `load {curve}`, `reverse`: change the editor's curve (no live apply).
/// - `analyze`: read the selected keyframes; `apply {curve?}` (the editor's when absent) or
///   `apply {id, usage}` (an approved saved preset): ease the selected keyframe segments.
/// - `save {name, description?, uses?, aliases?, approved?, curve?}`, `remove {id}`,
///   `approve {id, approved?, uses?}`: saved presets.
/// - `editCard {id}`, `saveChanges`, `done`: edit an Ease card; `saveCard {name, category?,
///   description?, curve?}`: the editor's curve as a new Ease card.
/// - `preview {duration?, fps?, play?}`, `live {on}`.
///
/// Operations that call the library run in the background: they answer `{"started": op}` and
/// `state` shows `busy` until they finish, then `status` (and `error`).
pub fn command(app: &mut EffectcraftApp, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let op = text(params, "op")
        .ok_or("missing `op`: state, list, load, reverse, analyze, apply, save, remove, approve, editCard, saveChanges, saveCard, done, preview or live")?;
    if op == "editCard" {
        let id = text(params, "id").unwrap_or_default().to_string();
        let mut view = std::mem::take(&mut app.ui.motion_library);
        let r = edit_card(ctx, &mut view, &id);
        app.ui.motion_library = view;
        return r;
    }
    let shared = shared(ctx);
    let Ok(mut guard) = shared.try_lock() else { return Err(WAIT.into()) };
    let st = &mut *guard;
    let now = ctx.input(|i| i.time);
    let free = st.busy.is_none();
    if !free && ["analyze", "apply", "save", "remove", "approve", "saveChanges", "saveCard"].contains(&op) {
        return Err(WAIT.into());
    }
    let curve_param = |st: &CurvesState| -> Result<Curve, String> { params.get("curve").map_or(Ok(st.curve), parse_curve) };
    match op {
        "state" => Ok(st.json()),
        "list" => {
            if free {
                start_load(ctx, st, now);
            }
            Ok(json!({"presets": st.presets, "starters": starters_json(&st.hidden_starters), "loading": st.busy.as_deref() == Some("list")}))
        }
        "load" => {
            let c = parse_curve(params.get("curve").ok_or("missing `curve`: [x1, y1, x2, y2]")?)?;
            st.playing = None;
            st.active_preset = None;
            st.curve = c;
            st.actual = false;
            st.dirty = true;
            st.say("");
            Ok(json!({"curve": st.curve}))
        }
        "reverse" => {
            if !st.editable() {
                return Err("Select one segment to edit its curve.".into());
            }
            st.playing = None;
            st.curve = reverse(&st.curve);
            st.actual = false;
            st.dirty = true;
            Ok(json!({"curve": st.curve}))
        }
        "analyze" => {
            start_analyze(app, ctx, st, false)?;
            Ok(json!({"started": "analyze"}))
        }
        "apply" => {
            let id = text(params, "id");
            let args = if id.is_some() {
                apply_args(None, id, text(params, "usage"))?
            } else {
                let c = curve_param(st)?;
                st.curve = c;
                st.actual = false;
                st.chosen = None;
                st.dirty = false;
                apply_args(Some(c), None, None)?
            };
            start_apply(app, ctx, st, args.clone())?;
            Ok(json!({"started": "apply", "args": args}))
        }
        "save" => {
            let name = text(params, "name").map(str::trim).unwrap_or_default();
            if name.is_empty() {
                return Err("Name your curve first.".into());
            }
            let curve = curve_param(st)?;
            let form = SaveForm {
                target: "preset".into(),
                name: name.to_string(),
                description: text(params, "description").unwrap_or_default().to_string(),
                uses: json_words(params.get("uses")).join(", "),
                aliases: json_words(params.get("aliases")).join(", "),
                approved: params.get("approved").and_then(Value::as_bool).unwrap_or(false),
                category: String::new(),
            };
            start_save(ctx, st, &form, curve);
            Ok(json!({"started": "save", "name": name}))
        }
        "remove" => {
            let id = text(params, "id").filter(|i| !i.is_empty()).ok_or("missing `id`: the saved preset")?;
            start_remove(ctx, st, id.to_string());
            Ok(json!({"started": "remove", "id": id}))
        }
        "approve" => {
            let id = text(params, "id").filter(|i| !i.is_empty()).ok_or("missing `id`: the saved preset")?;
            let approved = params.get("approved").and_then(Value::as_bool).unwrap_or(true);
            let uses = params.get("uses").map(|u| json_words(Some(u)));
            start_approve(ctx, st, id.to_string(), approved, uses);
            Ok(json!({"started": "approve", "id": id, "approved": approved}))
        }
        "saveChanges" => {
            start_save_changes(ctx, st)?;
            Ok(json!({"started": "saveChanges"}))
        }
        "saveCard" => {
            let folder = &app.ui.motion_library.folder;
            let category = text(params, "category").map(str::to_string).unwrap_or_else(|| if folder.is_empty() { "My motion".into() } else { folder.clone() });
            let args = card_args(text(params, "name").unwrap_or_default(), &category, text(params, "description").unwrap_or_default(), curve_param(st)?)?;
            start_save_card(app, ctx, st, args.clone());
            Ok(json!({"started": "saveCard", "args": args}))
        }
        "done" => {
            finish_editing(st);
            app.ui.motion_library.workspace.clear();
            Ok(json!({"workspace": ""}))
        }
        "preview" => {
            if let Some(d) = params.get("duration") {
                let d = d.as_f64().filter(|d| d.is_finite()).ok_or("`duration` is seconds (0–10)")?;
                st.duration = (d.clamp(0.0, 10.0) * 10.0).round() / 10.0;
            }
            if let Some(f) = params.get("fps") {
                st.fps = match f.as_u64() {
                    Some(30) => 30,
                    Some(60) => 60,
                    _ => return Err("`fps` is 30 or 60".into()),
                };
            }
            if let Some(play) = params.get("play").and_then(Value::as_bool) {
                st.playing = play.then_some(now);
            }
            Ok(json!({"duration": st.duration, "fps": st.fps, "playing": st.playing.is_some()}))
        }
        "live" => {
            st.live = params.get("on").and_then(Value::as_bool).ok_or("`on` is true or false")?;
            st.live_due = None;
            Ok(json!({"live": st.live}))
        }
        other => Err(format!("unknown op `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn bezier_ends_and_values() {
        let c = [0.16, 1.0, 0.3, 1.0];
        assert_eq!(point(&c, 0.0), [0.0, 0.0]);
        assert_eq!(point(&c, 1.0), [1.0, 1.0]);
        let s = samples(&c);
        assert_eq!(s.len(), 121);
        // Linear: value = time, speed = 1.
        let lin = STARTERS[0].1;
        for x in [0.1, 0.25, 0.5, 0.9] {
            assert!(close(value(&lin, x), x, 1e-6));
        }
        assert!(speed(&samples(&lin)).iter().all(|p| close(p[1], 1.0, 1e-6)));
        // Quick entrance is ahead of linear.
        assert!(value(&c, 0.3) > 0.6);
        assert!(close(sample_value(&samples(&lin), 0.42), 0.42, 1e-6));
        assert_eq!(sample_value(&[], 0.5), 0.5);
        assert!(speed(&[]).is_empty());
    }

    #[test]
    fn reverse_is_its_own_inverse() {
        let c = [0.16, 1.0, 0.3, 1.0];
        assert_eq!(reverse(&c), [0.7, 0.0, 0.84, 0.0]);
        assert!(reverse(&reverse(&c)).iter().zip(c).all(|(a, b)| close(*a, b, 1e-12)));
        // Reversed in time: value(1 - x) = 1 - value(x).
        for x in [0.2, 0.5, 0.8] {
            assert!(close(value(&reverse(&c), 1.0 - x), 1.0 - value(&c, x), 1e-6));
        }
    }

    #[test]
    fn limits_and_parsing() {
        assert_eq!(parse_curve(&json!([0.33, 0, 0.67, 1])).unwrap(), DEFAULT_CURVE);
        assert!(parse_curve(&json!([0, 0, 0.5, 1])).is_err(), "x1 must be ≥ 0.001");
        assert!(parse_curve(&json!([0.5, 7, 0.5, 1])).is_err());
        assert!(parse_curve(&json!([0.5, 0, 1, 1])).is_err(), "x2 must be ≤ 0.999");
        assert!(parse_curve(&json!([0.5, 0, 0.5])).is_err());
        assert!(parse_curve(&json!("soft")).is_err());
        assert_eq!(clamp_curve([f64::NAN, -9.0, 2.0, f64::INFINITY]), [0.33, -5.0, 0.999, 1.0]);
    }

    #[test]
    fn fit_recovers_a_curve() {
        let target = [0.25, 0.1, 0.25, 1.0];
        let c = fit(&samples(&target));
        for x in [0.1, 0.3, 0.5, 0.7, 0.9] {
            assert!(close(value(&c, x), value(&target, x), 0.01), "{c:?} at {x}");
        }
    }

    /// The keys `backend.save_asset` writes for a drawn curve (dt = dv = 1).
    fn saved_keys(c: Curve) -> Value {
        let [x1, y1, x2, y2] = c;
        json!([
            {"time": 0, "value": 0, "in_type": "LINEAR", "out_type": "BEZIER",
             "in_ease": [{"speed": 1, "influence": 33.333}], "out_ease": [{"speed": y1 / x1, "influence": x1 * 100.0}]},
            {"time": 1, "value": 1, "in_type": "BEZIER", "out_type": "LINEAR",
             "out_ease": [{"speed": 1, "influence": 33.333}], "in_ease": [{"speed": (1.0 - y2) / (1.0 - x2), "influence": (1.0 - x2) * 100.0}]}
        ])
    }

    #[test]
    fn keys_to_curve() {
        for c in [DEFAULT_CURVE, [0.16, 1.0, 0.3, 1.0], [0.4, -0.3, 0.6, 1.4]] {
            let back = from_keys(&saved_keys(c)).unwrap();
            for (a, b) in back.iter().zip(c) {
                assert!(close(*a, b, 1e-9), "{back:?} vs {c:?}");
            }
        }
        // Real units: 2 s from 100 to 300, out speed 150/s at 50 % influence.
        let keys = json!([
            {"time": 1, "value": 100, "out_type": "BEZIER", "out_ease": [{"speed": 150, "influence": 50}]},
            {"time": 3, "value": 300, "in_type": "LINEAR"}
        ]);
        let c = from_keys(&keys).unwrap();
        assert!(close(c[0], 0.5, 1e-9) && close(c[1], 150.0 * 2.0 * 0.5 / 200.0, 1e-9));
        assert!(close(c[2], 2.0 / 3.0, 1e-9) && close(c[3], 2.0 / 3.0, 1e-9), "linear in side: {c:?}");
        // Refused: equal values, wrong times, not two keys, not numbers.
        assert!(from_keys(&json!([{"time": 0, "value": 1}, {"time": 1, "value": 1}])).is_err());
        assert!(from_keys(&json!([{"time": 1, "value": 0}, {"time": 1, "value": 1}])).is_err());
        assert!(from_keys(&json!([{"time": 0, "value": 0}])).is_err());
        assert!(from_keys(&json!([{"time": 0, "value": [0, 1]}, {"time": 1, "value": [1, 2]}])).is_err());
        assert!(from_keys(&Value::Null).is_err());
    }

    #[test]
    fn magnet_snaps_and_releases() {
        let mut snap = None;
        // 100 points per value unit: within 8 points of 1 snaps.
        assert_eq!(magnetic(0.9, &mut snap, 100.0, false), 0.9);
        assert_eq!(magnetic(1.05, &mut snap, 100.0, false), 1.0);
        assert_eq!(snap, Some(1.0));
        // Stays snapped up to 14 points away, lets go after.
        assert_eq!(magnetic(1.12, &mut snap, 100.0, false), 1.0);
        assert_eq!(magnetic(1.2, &mut snap, 100.0, false), 1.2);
        assert_eq!(snap, None);
        assert_eq!(magnetic(0.03, &mut snap, 100.0, false), 0.0);
        assert_eq!(magnetic(0.03, &mut snap, 100.0, true), 0.03, "Alt bypasses");
        assert_eq!(snap, None);
    }

    #[test]
    fn arguments() {
        let a = apply_args(Some(DEFAULT_CURVE), None, None).unwrap();
        assert_eq!(a, json!({"apply_to_current": true, "curve": [0.33, 0.0, 0.67, 1.0], "refresh": true}));
        let a = apply_args(None, Some("abc"), Some("Card entrance")).unwrap();
        assert_eq!(a["id"], "abc");
        assert_eq!(a["usage"], "Card entrance");
        assert!(apply_args(None, None, None).is_err());
        assert!(apply_args(Some([0.0, 0.0, 1.0, 1.0]), None, None).is_err());
        let c = card_args(" Soft in ", " ", "", DEFAULT_CURVE).unwrap();
        assert_eq!(c, json!({"kind": "interpolation", "name": "Soft in", "category": "My motion", "description": "", "curve": [0.33, 0.0, 0.67, 1.0]}));
        assert!(card_args("  ", "x", "", DEFAULT_CURVE).is_err());
        assert_eq!(word_list("a, b ,, c "), ["a", "b", "c"]);
        assert_eq!(json_words(Some(&json!(["x ", "", "y"]))), ["x", "y"]);
    }

    #[test]
    fn presets_and_analysis_parse_leniently() {
        let list = json!({"presets": [
            {"id": "a", "name": "Soft", "description": "d", "curve": [0.33, 0, 0.67, 1], "uses": ["Card entrance"], "aliases": [], "approved": true},
            {"id": "b", "name": "Bad", "curve": [0, 0, 1, 1]},
            {"name": "No id", "curve": [0.33, 0, 0.67, 1]},
            7
        ]});
        let p = Preset::list(&list);
        assert_eq!(p.len(), 1);
        assert!(p[0].approved && p[0].matches("card SOFT") && !p[0].matches("bounce"));
        let a = Analysis::from_json(&json!({
            "key_count": 2, "target_count": 3, "duration": 1.5, "property": "Opacity", "blocked": "",
            "channels": [{"name": "Opacity", "points": [[0, 0], [0.5, "x"], [1, 1]], "curve": [0.2, 0, 0.8, 9]}]
        }))
        .unwrap();
        assert_eq!(a.channels[0].points, vec![[0.0, 0.0], [1.0, 1.0]]);
        assert_eq!(a.channels[0].curve, Some([0.2, 0.0, 0.8, 6.0]));
        assert_eq!((a.target_count, a.duration), (3, 1.5));
        assert!(Analysis::from_json(&json!({"channels": []})).is_err());
        assert!(Analysis::from_json(&json!("nope")).is_err());
    }

    #[test]
    fn state_accepts_analysis_and_statuses() {
        let mut st = CurvesState::default();
        let v = json!({"key_count": 2, "target_count": 2, "property": "Position", "channels": [
            {"name": "X", "points": [[0, 0], [1, 1]], "curve": [0.5, 0, 0.5, 1]},
            {"name": "Y", "points": [[0, 0], [0.5, 0.8], [1, 1]], "curve": null}
        ]});
        st.accept(&v, false).unwrap();
        assert!(st.actual && st.editable());
        assert_eq!(st.curve, [0.5, 0.0, 0.5, 1.0]);
        st.analysis_status();
        assert!(st.status.starts_with("2 segments selected."));
        // A pending live edit keeps the editor's curve.
        st.curve = DEFAULT_CURVE;
        st.accept(&v, true).unwrap();
        assert_eq!(st.curve, DEFAULT_CURVE);
        // Save form defaults from the analysis; a fitted channel says it's approximate.
        st.channel = 1;
        st.actual = true;
        st.open_form("preset", "");
        let f = st.form.clone().unwrap();
        assert_eq!(f.name, "Position Y");
        assert!(f.description.starts_with("Approximation of analyzed Position (Y)"));
        assert!(!st.actual && st.dirty);
        // Editing a card: edits say how to save, no live apply.
        st.editing = Some(EditingCard { id: "e".into(), name: "Pop".into(), keys: Value::Null });
        st.change([0.2, 0.0, 0.8, 1.0], 0.0, 0.0);
        assert!(st.status.contains("Save changes") && st.live_due.is_none());
        finish_editing(&mut st);
        st.change([0.2, 0.0, 0.8, 1.0], 5.0, 0.4);
        assert_eq!(st.live_due, Some(5.4));
        let j = st.json();
        assert_eq!(j["editable"], true);
        assert_eq!(j["starters"].as_array().map(Vec::len), Some(4));
        let back: CurvesState = serde_json::from_value(j).unwrap();
        assert_eq!(back.curve, st.curve);
    }

    /// Run `op` through [`command`] and wait for its background job.
    fn run(app: &mut EffectcraftApp, ctx: &egui::Context, p: Value) -> Value {
        let r = command(app, ctx, &p).unwrap();
        for _ in 0..600 {
            if lock(&shared(ctx)).busy.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let shared = shared(ctx);
        let st = lock(&shared);
        assert!(!st.error, "{p}: {}", st.status);
        r
    }

    /// End to end with the real Motion Library backend on a throwaway library:
    /// `ELIAS_MOTION_LIBRARY=$(mktemp -d) cargo test -p effectcraft-ui-egui --lib backend_round_trip -- --ignored`
    #[test]
    #[ignore = "needs the Motion Library backend and ELIAS_MOTION_LIBRARY set to a throwaway folder"]
    fn backend_round_trip() {
        let root = std::env::var("ELIAS_MOTION_LIBRARY").expect("set ELIAS_MOTION_LIBRARY to a throwaway folder");
        assert!(!root.contains("Documents"), "never the real library");
        let ctx = egui::Context::default();
        let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
        // Presets: save (approved), list, approve off, remove.
        run(&mut app, &ctx, json!({"op": "load", "curve": [0.16, 1, 0.3, 1]}));
        run(&mut app, &ctx, json!({"op": "save", "name": "Quick in", "uses": "Card entrance, title", "approved": true}));
        let state = shared(&ctx);
        let (id, status) = {
            let st = lock(&state);
            (st.chosen.clone().unwrap().id, st.status.clone())
        };
        assert_eq!(status, "Saved and approved.");
        assert!(lock(&shared(&ctx)).presets.iter().any(|p| p.id == id && p.uses == ["Card entrance", "title"]));
        run(&mut app, &ctx, json!({"op": "approve", "id": id, "approved": false}));
        assert_eq!(lock(&shared(&ctx)).status, "Approval revoked.");
        run(&mut app, &ctx, json!({"op": "remove", "id": id}));
        assert!(lock(&shared(&ctx)).presets.is_empty());
        // An Ease card: save the editor's curve, open it, edit, save the change.
        run(&mut app, &ctx, json!({"op": "saveCard", "name": "Soft drop", "category": "Curves test"}));
        let list = super::super::motion_library::backend_rpc("list", json!({})).unwrap();
        let entries = list.get("entries").or_else(|| list.get("items")).and_then(Value::as_array).cloned().unwrap_or_default();
        let card = entries.iter().find(|e| e["name"] == "Soft drop").unwrap_or_else(|| panic!("no card in {list}"));
        let card_id = card["id"].as_str().unwrap().to_string();
        run(&mut app, &ctx, json!({"op": "editCard", "id": card_id}));
        assert_eq!(app.ui.motion_library.workspace, "curves");
        {
            let st = lock(&state);
            assert!(st.editing.is_some());
            assert!(st.curve.iter().zip([0.16, 1.0, 0.3, 1.0]).all(|(a, b)| close(*a, b, 1e-9)), "{:?}", st.curve);
        }
        run(&mut app, &ctx, json!({"op": "reverse"}));
        run(&mut app, &ctx, json!({"op": "saveChanges"}));
        assert!(lock(&shared(&ctx)).status.starts_with("Saved changes to"));
        let entry = super::super::motion_library::backend_rpc("describe", json!({"id": card_id})).unwrap();
        let saved = from_keys(&entry["payload"]["keys"]).unwrap();
        assert!(saved.iter().zip([0.7, 0.0, 0.84, 0.0]).all(|(a, b)| close(*a, b, 1e-9)), "{saved:?}");
        // Saving again uses the keys just saved (no "changed while you were editing").
        run(&mut app, &ctx, json!({"op": "load", "curve": [0.5, 0, 0.5, 1]}));
        lock(&shared(&ctx)).dirty = true;
        run(&mut app, &ctx, json!({"op": "saveChanges"}));
        run(&mut app, &ctx, json!({"op": "done"}));
        assert!(app.ui.motion_library.workspace.is_empty());
    }

    #[test]
    fn command_ops_without_the_backend() {
        let ctx = egui::Context::default();
        let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
        let r = command(&mut app, &ctx, &json!({"op": "load", "curve": [0.16, 1, 0.3, 1]})).unwrap();
        assert_eq!(r["curve"], json!([0.16, 1.0, 0.3, 1.0]));
        let r = command(&mut app, &ctx, &json!({"op": "reverse"})).unwrap();
        assert_eq!(r["curve"], json!([0.7, 0.0, 0.84, 0.0]));
        let r = command(&mut app, &ctx, &json!({"op": "preview", "duration": 12.34, "fps": 60})).unwrap();
        assert_eq!((r["duration"].as_f64(), r["fps"].as_u64()), (Some(10.0), Some(60)));
        assert!(command(&mut app, &ctx, &json!({"op": "preview", "fps": 24})).is_err());
        assert!(command(&mut app, &ctx, &json!({"op": "load", "curve": [0, 0, 1, 1]})).is_err());
        assert!(command(&mut app, &ctx, &json!({"op": "nope"})).is_err());
        assert!(command(&mut app, &ctx, &json!({})).is_err());
        assert!(command(&mut app, &ctx, &json!({"op": "save"})).is_err(), "a preset needs a name");
        assert!(command(&mut app, &ctx, &json!({"op": "saveChanges"})).is_err(), "no card open");
        let s = command(&mut app, &ctx, &json!({"op": "state"})).unwrap();
        assert_eq!(s["curve"], json!([0.7, 0.0, 0.84, 0.0]));
        assert_eq!(s["fps"], 60);
        // One operation at a time.
        lock(&shared(&ctx)).busy = Some("save".into());
        assert_eq!(command(&mut app, &ctx, &json!({"op": "remove", "id": "x"})).unwrap_err(), WAIT);
        assert!(command(&mut app, &ctx, &json!({"op": "editCard", "id": "x"})).is_err());
        lock(&shared(&ctx)).busy = None;
        app.ui.motion_library.workspace = "curves".into();
        command(&mut app, &ctx, &json!({"op": "done"})).unwrap();
        assert!(app.ui.motion_library.workspace.is_empty());
    }
}
