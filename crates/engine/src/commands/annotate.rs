//! PN1: Pen / Comment review marks drawn on the live Composition viewer (`annotate.*`).
//!
//! A host (Ghostex's ✏️ Pen · Comment strip, an agent, the control channel) arms a tool with
//! `annotate.start {tool}`; the viewer then takes the pointer: Pen drags a freehand stroke, Comment
//! clicks a point, both in comp pixels (they stay put under zoom and pan), and a small box next to
//! the mark asks "Say what should change (optional)" (Enter adds, Esc cancels). Adding one runs
//! `annotate.add`, which measures the mark (box, timecode, the topmost layers under it) and keeps it
//! here until a host takes it to its chat (`annotate.list {since}`, or in process through
//! `effectcraft_embed::Embedded::new_annotations`) and removes it once that card is sent or dropped
//! (`annotate.remove`).
//!
//! The marks are transient session state: not layers, not in the project, never an undo step.
//! While one waits (a queued card) the viewer draws it faintly on its frame.
//!
//! Elias, 28 Oct 2026: "why does the pen appear here and not under the comp like it does in
//! hyperframes?", then "its just that the popup appears directly in the comp im viewing" and
//! "instead of in the chat, but then when i press enter it gets queued in the chat".

pub mod ink;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CommandSpec, bad, comp_id, str_p, time_p};
use crate::{EngineError, Result, Session, query};

/// The most marks kept (oldest dropped): a host that never takes them can't grow this forever.
pub const MAX_MARKS: usize = 64;
/// The longest stroke kept (points); longer strokes are thinned evenly.
pub const MAX_POINTS: usize = 2000;
/// A layer covering this share of the frame or more is a background: never what a mark means.
const BACKGROUND_SHARE: f64 = 0.8;
/// A layer counts as inside a stroke only if its box is at most this many times the stroke's.
const INSIDE_MAX_RATIO: f64 = 2.0;
/// The stroke's box is sampled on a GRID × GRID grid for the layers inside it.
const GRID: usize = 6;
/// The most layers named for one mark.
const MAX_LAYERS: usize = 6;

/// The armed tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Pen,
    Comment,
}

impl Tool {
    pub fn from_name(s: &str) -> Option<Tool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pen" | "draw" | "stroke" => Some(Tool::Pen),
            "comment" | "pin" | "point" => Some(Tool::Comment),
            _ => None,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Tool::Pen => "pen",
            Tool::Comment => "comment",
        }
    }
}

/// A layer under a mark: the deepest layer drawn there (through precomps), as `layer.at {deep}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkLayer {
    pub id: u64,
    pub name: String,
    /// The comp that holds the layer (the marked comp, or a precomp's).
    pub comp: u64,
    pub comp_name: String,
    /// The marked comp's top-level layer it was reached through.
    pub top: u64,
    /// The marked comp's name, then each layer down to this one.
    pub path: Vec<String>,
}

/// One mark.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub id: u64,
    pub tool: Tool,
    pub comp: u64,
    pub comp_name: String,
    /// Comp time (seconds) of the frame it was drawn on, and that frame.
    pub time: f64,
    pub frame: i64,
    /// The time as the project's Time Display Style shows it (`0:00:04:12`).
    pub timecode: String,
    /// The stroke (Pen) or the one point (Comment), comp pixels.
    pub points: Vec<[f64; 2]>,
    /// `[x, y, w, h]` around the points, comp pixels.
    pub bbox: [f64; 4],
    /// The comp's size, so a reader can place the points on a frame of any size.
    pub comp_size: [u32; 2],
    pub note: String,
    /// The topmost layers under it, most likely first.
    pub layers: Vec<MarkLayer>,
    /// Who added it (`viewer` for the Composition viewer, else the client's name).
    pub source: String,
}

/// The session's marks and the armed tool.
#[derive(Clone, Debug, Default)]
pub struct Annotations {
    pub armed: Option<Tool>,
    /// Bumped on every arm (not on disarm): the viewer pauses playback when it changes.
    pub arm_serial: u64,
    pub marks: Vec<Mark>,
    pub next_id: u64,
    /// Bumped on every change (arm, add, remove).
    pub revision: u64,
    /// A host takes the marks to its chat (Ghostex in process): the viewer doesn't copy a
    /// "Copy for Claude" reference to the clipboard then, and keeps showing waiting marks.
    pub hosted: bool,
}

impl Annotations {
    pub fn arm(&mut self, tool: Option<Tool>) {
        if tool.is_some() {
            self.arm_serial = self.arm_serial.wrapping_add(1);
        }
        if self.armed != tool || tool.is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.armed = tool;
    }

    pub fn push(&mut self, mut mark: Mark) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        mark.id = self.next_id;
        let id = mark.id;
        self.marks.push(mark);
        if self.marks.len() > MAX_MARKS {
            let extra = self.marks.len() - MAX_MARKS;
            self.marks.drain(..extra);
        }
        self.revision = self.revision.wrapping_add(1);
        id
    }

    pub fn remove(&mut self, ids: &[u64]) -> usize {
        let before = self.marks.len();
        self.marks.retain(|m| !ids.contains(&m.id));
        let n = before - self.marks.len();
        if n > 0 {
            self.revision = self.revision.wrapping_add(1);
        }
        n
    }

    pub fn get(&self, id: u64) -> Option<&Mark> {
        self.marks.iter().find(|m| m.id == id)
    }

    /// Marks added after mark `since` (ids only grow), oldest first.
    pub fn since(&self, since: u64) -> impl Iterator<Item = &Mark> {
        self.marks.iter().filter(move |m| m.id > since)
    }
}

/// `[x, y, w, h]` around `points` (zero for none).
pub fn bbox(points: &[[f64; 2]]) -> [f64; 4] {
    let Some(first) = points.first() else {
        return [0.0; 4];
    };
    let mut b = [first[0], first[1], first[0], first[1]];
    for p in points {
        b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
    }
    [b[0], b[1], b[2] - b[0], b[3] - b[1]]
}

/// Whether `p` is inside the closed polygon `poly` (ray casting).
pub fn inside_polygon(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (Some(a), Some(b)) = (poly.get(i), poly.get(j)) else { break };
        let dy = b[1] - a[1];
        if (a[1] > p[1]) != (b[1] > p[1]) && dy.abs() > f64::EPSILON && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / dy + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The points a stroke's layers are looked up at: its centre, then a GRID × GRID grid over its box
/// kept where it falls inside the (closed) stroke; a line or a dot only at its centre and ends.
pub fn stroke_samples(stroke: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let [x, y, w, h] = bbox(stroke);
    let centre = [x + w / 2.0, y + h / 2.0];
    let mut out = vec![centre];
    if stroke.len() < 3 || w < 1.0 || h < 1.0 {
        out.extend(stroke.first().copied());
        out.extend(stroke.last().copied());
        return out;
    }
    for i in 0..GRID {
        for j in 0..GRID {
            let p = [x + w * (i as f64 + 0.5) / GRID as f64, y + h * (j as f64 + 0.5) / GRID as f64];
            if inside_polygon(p, stroke) {
                out.push(p);
            }
        }
    }
    out
}

/// At most [`MAX_POINTS`] finite points, thinned evenly.
fn tidy_points(points: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    let pts: Vec<[f64; 2]> = points.into_iter().filter(|p| p[0].is_finite() && p[1].is_finite()).collect();
    if pts.len() <= MAX_POINTS {
        return pts;
    }
    let step = pts.len() as f64 / MAX_POINTS as f64;
    (0..MAX_POINTS).filter_map(|i| pts.get((i as f64 * step) as usize).copied()).collect()
}

fn points_p(p: &Value, cmd: &str) -> Result<Vec<[f64; 2]>> {
    let pair = |v: &Value| -> Option<[f64; 2]> {
        match v {
            Value::Array(a) => Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]),
            Value::Object(_) => Some([v.get("x")?.as_f64()?, v.get("y")?.as_f64()?]),
            _ => None,
        }
    };
    if let Some(a) = p.get("points").and_then(Value::as_array) {
        return a.iter().map(|v| pair(v).ok_or_else(|| bad(cmd, "each of `points` is [x, y] (comp pixels)"))).collect();
    }
    if let Some(v) = p.get("point") {
        return Ok(vec![pair(v).ok_or_else(|| bad(cmd, "`point` is [x, y] (comp pixels)"))?]);
    }
    if let (Some(x), Some(y)) = (p.get("x").and_then(Value::as_f64), p.get("y").and_then(Value::as_f64)) {
        return Ok(vec![[x, y]]);
    }
    Err(bad(cmd, "give `points` ([[x, y], …] in comp pixels), or `point` / `x`, `y` for a comment"))
}

/// The deepest layer drawn at `at` (`layer.at {deep}`) and the top-level layer it was reached
/// through.
fn layer_under(s: &mut Session, comp: u64, time: f64, at: [f64; 2]) -> Option<MarkLayer> {
    let hit = s.execute("layer.at", json!({"comp": comp, "time": time, "x": at[0], "y": at[1], "deep": true})).ok()?;
    let top = hit["layer"]["id"].as_u64()?;
    let levels = hit["path"].as_array().cloned().unwrap_or_default();
    let deepest = levels.iter().rev().find(|l| l["layer"]["id"].as_u64().is_some());
    let (layer, comp_id, comp_name) = match deepest {
        Some(l) => (&l["layer"], l["comp"].as_u64().unwrap_or(comp), l["compName"].as_str().unwrap_or_default().to_string()),
        None => (&hit["layer"], comp, hit["compName"].as_str().unwrap_or_default().to_string()),
    };
    let mut path = vec![hit["compName"].as_str().unwrap_or_default().to_string()];
    path.extend(levels.iter().filter_map(|l| l["layer"]["name"].as_str().map(str::to_string)));
    Some(MarkLayer { id: layer["id"].as_u64()?, name: layer["name"].as_str().unwrap_or_default().to_string(), comp: comp_id, comp_name, top, path })
}

/// Top-level layer boxes `[x, y, w, h]` (`layer.bounds`).
fn layer_boxes(s: &mut Session, comp: u64, time: f64) -> Vec<(u64, [f64; 4])> {
    let Ok(v) = s.execute("layer.bounds", json!({"comp": comp, "time": time})) else {
        return Vec::new();
    };
    v["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let b = l["bounds"].as_array()?;
            Some((l["id"].as_u64()?, [b.first()?.as_f64()?, b.get(1)?.as_f64()?, b.get(2)?.as_f64()?, b.get(3)?.as_f64()?]))
        })
        .collect()
}

/// The layers a mark means (the same rule as Ghostex's Pen, after HyperFrames): a comment names the
/// layer under it; a stroke names the top layer at each sample inside it, kept when its centre is
/// inside the stroke and its box is at most twice the stroke's (else the first one hit). Layers
/// covering ≥ 80 % of the frame are backgrounds and never count.
fn mark_layers(s: &mut Session, comp: u64, size: [u32; 2], time: f64, tool: Tool, points: &[[f64; 2]]) -> Vec<MarkLayer> {
    let boxes = layer_boxes(s, comp, time);
    let frame = f64::from(size[0]) * f64::from(size[1]);
    let background = |top: u64| boxes.iter().find(|(id, _)| *id == top).is_some_and(|(_, b)| b[2] * b[3] >= BACKGROUND_SHARE * frame);
    if tool == Tool::Comment {
        return points.first().and_then(|p| layer_under(s, comp, time, *p)).filter(|l| !background(l.top)).into_iter().collect();
    }
    let [_, _, sw, sh] = bbox(points);
    let stroke_area = (sw * sh).max(1.0);
    let closed = points.len() > 2;
    let mut seen: Vec<u64> = Vec::new();
    let mut kept: Vec<MarkLayer> = Vec::new();
    let mut fallback: Option<MarkLayer> = None;
    for at in stroke_samples(points) {
        let Some(layer) = layer_under(s, comp, time, at) else { continue };
        if seen.contains(&layer.top) || background(layer.top) {
            continue;
        }
        seen.push(layer.top);
        if fallback.is_none() {
            fallback = Some(layer.clone());
        }
        let keep = match boxes.iter().find(|(id, _)| *id == layer.top) {
            None => true,
            Some((_, b)) => {
                let centre = [b[0] + b[2] / 2.0, b[1] + b[3] / 2.0];
                b[2] * b[3] <= INSIDE_MAX_RATIO * stroke_area && (!closed || inside_polygon(centre, points))
            }
        };
        if keep && kept.len() < MAX_LAYERS {
            kept.push(layer);
        }
    }
    if kept.is_empty() {
        kept.extend(fallback);
    }
    kept
}

/// "Copy for Claude": one line naming the comp, time, what was marked and the note.
pub fn copy_ref(m: &Mark) -> String {
    let what = match m.tool {
        Tool::Pen => {
            let [x, y, w, h] = m.bbox;
            format!("✏️ drawn around x {:.0}–{:.0}, y {:.0}–{:.0} ({} points)", x, x + w, y, y + h, m.points.len())
        }
        Tool::Comment => {
            let p = m.points.first().copied().unwrap_or([0.0, 0.0]);
            format!("💬 comment at ({:.0}, {:.0})", p[0], p[1])
        }
    };
    let mut text = format!("{} · {} (f{}) · {what}", m.comp_name, m.timecode, m.frame);
    if !m.layers.is_empty() {
        let names: Vec<String> = m.layers.iter().map(|l| if l.path.len() > 2 { l.path[1..].join(" › ") } else { l.name.clone() }).collect();
        text.push_str(&format!(" · on {}", names.join(", ")));
    }
    let note = m.note.trim();
    text.push_str(&if note.is_empty() { " · (no note)".to_string() } else { format!(" — \"{note}\"") });
    text
}

/// A mark as `annotate.get` returns it (with its `copyRef`).
pub fn mark_json(m: &Mark) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("copyRef".into(), json!(copy_ref(m)));
    }
    v
}

fn state(s: &Session) -> Value {
    json!({"armed": s.annotations.armed.map(Tool::id), "revision": s.annotations.revision, "count": s.annotations.marks.len(), "hosted": s.annotations.hosted})
}

// ---------------------------------------------------------------- commands

fn start(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "annotate.start";
    let tool = match str_p(p, "tool") {
        None => Tool::Pen,
        Some(t) => Tool::from_name(t).ok_or_else(|| bad(C, format!("`tool` is pen or comment, got {t:?}")))?,
    };
    s.annotations.arm(Some(tool));
    Ok(state(s))
}

fn stop(s: &mut Session, _: &Value) -> Result<Value> {
    s.annotations.arm(None);
    Ok(state(s))
}

fn add(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "annotate.add";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?.clone();
    let comp_name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    let points = tidy_points(points_p(p, C)?);
    if points.is_empty() {
        return Err(bad(C, "the mark has no points"));
    }
    let tool = match str_p(p, "tool") {
        Some(t) => Tool::from_name(t).ok_or_else(|| bad(C, format!("`tool` is pen or comment, got {t:?}")))?,
        None if points.len() == 1 => Tool::Comment,
        None => Tool::Pen,
    };
    let points = if tool == Tool::Comment { points.into_iter().take(1).collect() } else { points };
    let t = comp.frame_rate.snap(time_p(s, p, Some(&comp)));
    let frame = comp.frame_rate.frame_at(t);
    let time = t.seconds();
    let size = [comp.width, comp.height];
    let layers = mark_layers(s, cid.0, size, time, tool, &points);
    let note = str_p(p, "note").unwrap_or_default().trim().to_string();
    let source = str_p(p, "source").map(str::to_string).or_else(|| s.client.clone()).unwrap_or_else(|| "viewer".into());
    let mark = Mark {
        id: 0,
        tool,
        comp: cid.0,
        comp_name,
        time,
        frame,
        timecode: super::time::display_time(s, &comp, t),
        bbox: bbox(&points),
        points,
        comp_size: size,
        note,
        layers,
        source,
    };
    let id = s.annotations.push(mark);
    let m = s.annotations.get(id).ok_or_else(|| bad(C, "the mark was dropped"))?;
    Ok(json!({"mark": mark_json(m), "copyRef": copy_ref(m), "state": state(s)}))
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let since = p.get("since").and_then(Value::as_u64).unwrap_or(0);
    let comp = match p.get("comp") {
        Some(c) if !c.is_null() => Some(s.resolve_comp(Some(c))?.0),
        _ => None,
    };
    let marks: Vec<Value> = s.annotations.since(since).filter(|m| comp.is_none_or(|c| m.comp == c)).map(mark_json).collect();
    let last = s.annotations.marks.last().map_or(0, |m| m.id);
    Ok(json!({"armed": s.annotations.armed.map(Tool::id), "revision": s.annotations.revision, "last": last, "marks": marks}))
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "annotate.get";
    let id = p.get("id").and_then(Value::as_u64).ok_or_else(|| bad(C, "give `id` (annotate.list lists them)"))?;
    let m = s.annotations.get(id).ok_or_else(|| bad(C, format!("no mark {id}")))?;
    Ok(mark_json(m))
}

fn ids_p(p: &Value) -> Vec<u64> {
    let mut ids: Vec<u64> = p.get("ids").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    ids.extend(p.get("id").and_then(Value::as_u64));
    ids
}

fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "annotate.remove";
    let ids = ids_p(p);
    if ids.is_empty() {
        return Err(bad(C, "give `id` or `ids`"));
    }
    let removed = s.annotations.remove(&ids);
    Ok(json!({"removed": removed, "state": state(s)}))
}

fn clear(s: &mut Session, p: &Value) -> Result<Value> {
    let comp = match p.get("comp") {
        Some(c) if !c.is_null() => Some(s.resolve_comp(Some(c))?.0),
        _ => None,
    };
    let ids: Vec<u64> = s.annotations.marks.iter().filter(|m| comp.is_none_or(|c| m.comp == c)).map(|m| m.id).collect();
    let removed = s.annotations.remove(&ids);
    Ok(json!({"removed": removed, "state": state(s)}))
}

fn copy_ref_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "annotate.copyRef";
    let id = match p.get("id").and_then(Value::as_u64) {
        Some(id) => id,
        None => s.annotations.marks.last().map(|m| m.id).ok_or_else(|| bad(C, "there are no marks"))?,
    };
    let m = s.annotations.get(id).ok_or_else(|| bad(C, format!("no mark {id}")))?;
    Ok(json!({"id": id, "text": copy_ref(m)}))
}

fn state_cmd(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(state(s))
}

pub fn specs() -> Vec<CommandSpec> {
    // Session state only (no project change, no undo step): queries in the registry's terms.
    vec![
        query!(
            "annotate.start",
            "Pen / Comment on the Viewer",
            "{tool?: pen (default)|comment (alias pin)} → {armed, revision, count, hosted}: arms the Composition viewer's review mark: Pen drags a freehand stroke, Comment clicks a point; a box next to it asks what should change (Enter adds, Esc cancels). Pauses playback. Arming again switches the tool",
            start
        ),
        query!("annotate.stop", "Stop Pen / Comment", "{} → {armed: null, …}: the viewer takes the pointer back (Esc in the viewer does the same)", stop),
        query!(
            "annotate.add",
            "Add Review Mark",
            "{comp?, time?|frame?, tool?: pen|comment (default: comment for one point), points?: [[x, y], …] in comp pixels, point?: [x, y] (a comment), x?, y?, note?, source?} → {mark: {id, tool, comp, compName, time, frame, timecode, points, bbox: [x, y, w, h], compSize, note, layers: [{id, name, comp, compName, top, path}], source, copyRef}, copyRef, state}: what the viewer's Enter runs; the time snaps to the frame",
            add
        ),
        query!(
            "annotate.list",
            "List Review Marks",
            "{since?: mark id (only newer ones), comp?} → {armed, revision, last: newest id, marks: [mark…]}: the marks waiting for a host to take them (each with its copyRef)",
            list
        ),
        query!("annotate.get", "Get Review Mark", "{id} → mark", get),
        query!(
            "annotate.remove",
            "Remove Review Mark",
            "{id | ids} → {removed, state}: a host took the mark (its card was sent or dropped): it leaves the viewer",
            remove
        ),
        query!("annotate.clear", "Clear Review Marks", "{comp?} → {removed, state}: every mark (of one comp)", clear),
        query!(
            "annotate.copyRef",
            "Copy Review Mark for Claude",
            "{id? (default the newest)} → {id, text}: the one-line reference the standalone viewer puts on the clipboard",
            copy_ref_cmd
        ),
        query!("annotate.state", "Review Mark State", "{} → {armed, revision, count, hosted}", state_cmd),
    ]
}
