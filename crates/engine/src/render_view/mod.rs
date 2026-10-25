//! Renders for agents and scripts: `render.frame` (one frame) and `render.contactSheet` (several
//! frames on one labelled sheet, optionally also an animated GIF).
//!
//! A render can show a comp frame, a footage or solid item on its own (`item`), or one layer solo
//! (`layer`, in its place in the comp); `region` renders just that rectangle of the comp at full
//! resolution (the renderer's region of interest, so only those pixels are composited); and
//! `overlays` draws what the Composition viewer draws on top (layer bounds, anchor points, motion
//! paths, safe margins, grid), so a headless agent can see where layers are.
//!
//! The MCP tools (`render_frame`, `render_preview`, `render_frames`), the control channel's
//! `render.frame` and the CLI all go through [`request`] and [`Session::render_view`].

mod overlay;
mod sheet;

use std::borrow::Cow;
use std::sync::Arc;

use effectcraft_project::{Comp, FootageKind, ItemId, ItemKind, LayerId, LayerSource, Project, build};
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

pub use overlay::{LayerGeom, Overlays, overlay_layers};
pub use sheet::MAX_CELLS;

use crate::commands::{CommandSpec, b_p, bad, checked_layer_ref, comp_id, str_p};
use crate::{EngineError, Result, Session, query};

/// Longest output side a single render may ask for (`max_side`); larger values are clamped.
pub const MAX_SIDE: u32 = 16_384;

/// What a render shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A composition frame (comp time).
    Comp(ItemId),
    /// A footage or solid item on its own (source time).
    Item(ItemId),
}

/// A parsed `render.frame` request.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRequest {
    pub target: Target,
    /// Render only this layer (solo, in its place in the comp).
    pub layer: Option<LayerId>,
    pub time: Tick,
    /// Longest side of the output (0 = full resolution of the region / frame).
    pub max_side: u32,
    /// Keep the frame's alpha instead of compositing over the comp background.
    pub transparent: bool,
    /// `[x, y, w, h]` in comp pixels, whole pixels, inside the frame.
    pub region: Option<[f64; 4]>,
    pub overlays: Overlays,
}

/// A rendered view: straight 8-bit RGBA plus a JSON description.
pub struct View {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub info: Value,
}

/// `item` (id or name) → id.
fn item_ref(s: &Session, v: &Value, cmd: &str) -> Result<ItemId> {
    if let Some(name) = v.as_str() {
        let candidates: Vec<_> =
            s.project.items.values().filter(|i| i.name == name).map(|i| json!({"id": i.id.0, "name": i.name, "parent": i.parent.map(|p| p.0)})).collect();
        if candidates.len() > 1 {
            return Err(bad(cmd, format!("ambiguous project item name {name:?}; candidates: {}; use a numeric item id", Value::Array(candidates))));
        }
    }
    match v {
        Value::Number(n) => n.as_u64().map(ItemId).filter(|i| s.project.item(*i).is_some()),
        Value::String(name) => s.project.find_by_name(name).map(|i| i.id),
        _ => None,
    }
    .ok_or_else(|| bad(cmd, format!("no project item {v} (see project.summary)")))
}

fn present<'a>(p: &'a Value, k: &str) -> Option<&'a Value> {
    p.get(k).filter(|v| !v.is_null())
}

/// Seconds from a param: a finite number or an error naming it.
pub(crate) fn seconds_p(p: &Value, k: &str, cmd: &str) -> Result<Option<f64>> {
    match present(p, k) {
        None => Ok(None),
        Some(v) => v.as_f64().filter(|t| t.is_finite()).map(Some).ok_or_else(|| bad(cmd, format!("`{k}` must be a finite number of seconds, not {v}"))),
    }
}

/// A non-negative integer param, clamped to `max`.
pub(crate) fn uint_p(p: &Value, keys: &[&str], max: u64, cmd: &str) -> Result<Option<u64>> {
    let Some((k, v)) = keys.iter().find_map(|k| present(p, k).map(|v| (k, v))) else { return Ok(None) };
    let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0).map(|f| f.min(u64::MAX as f64) as u64));
    n.map(|n| Some(n.min(max))).ok_or_else(|| bad(cmd, format!("`{k}` must be a whole number ≥ 0, not {v}")))
}

/// Size (comp pixels) and frame rate of what a target shows.
fn target_frame(s: &Session, t: Target) -> Option<(u32, u32, FrameRate)> {
    match t {
        Target::Comp(id) => s.project.comp(id).map(|c| (c.width.max(1), c.height.max(1), c.frame_rate)),
        Target::Item(id) => {
            let it = s.project.item(id)?;
            let (w, h) = it.dimensions()?;
            let rate = match &it.kind {
                ItemKind::Footage(f) if f.frame_rate.num > 0 && f.frame_rate.den > 0 => f.frame_rate,
                _ => s.active_comp().map(|c| c.frame_rate).unwrap_or(FrameRate::FPS_30),
            };
            Some((w.max(1), h.max(1), rate))
        }
    }
}

/// Parse the shared render parameters (`comp | item`, `layer`, `time | frame`, `max_side`,
/// `transparent`, `region`, `overlays`). Keys a command doesn't take are rejected before this by
/// the command's params doc; the control channel also accepts `seconds` and `maxSide`.
pub fn request(s: &Session, p: &Value, cmd: &str) -> Result<ViewRequest> {
    let target = match present(p, "item") {
        Some(v) => {
            if present(p, "comp").is_some() {
                return Err(bad(cmd, "give `comp` or `item`, not both"));
            }
            let id = item_ref(s, v, cmd)?;
            match s.project.item(id).map(|i| &i.kind) {
                Some(ItemKind::Comp(_)) => Target::Comp(id),
                Some(ItemKind::Footage(f)) if f.has_video && !matches!(f.kind, FootageKind::Audio | FootageKind::Data | FootageKind::Model) => Target::Item(id),
                Some(ItemKind::Solid(_)) => Target::Item(id),
                _ => return Err(bad(cmd, format!("item {v} has no pixels to render (a folder, audio, data or 3D model item)"))),
            }
        }
        None => Target::Comp(comp_id(s, p)?),
    };
    let (fw, fh, rate) = target_frame(s, target).ok_or_else(|| bad(cmd, "the item has no size"))?;
    let layer = match (present(p, "layer"), target) {
        (None, _) => None,
        (Some(v), Target::Comp(cid)) => {
            let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
            Some(
                checked_layer_ref(comp, cid, v, cmd)?
                    .ok_or_else(|| bad(cmd, format!("no layer {v} in this comp (use an id, \"#n\" or a name from comp.info)")))?,
            )
        }
        (Some(_), Target::Item(_)) => return Err(bad(cmd, "`layer` needs a composition, not a footage item")),
    };
    let time = match (seconds_p(p, "time", cmd)?.or(seconds_p(p, "seconds", cmd)?), present(p, "frame")) {
        (Some(t), _) => Tick::from_seconds_f64(t),
        (None, Some(f)) => rate.tick_of(f.as_i64().ok_or_else(|| bad(cmd, format!("`frame` must be a whole number, not {f}")))?),
        (None, None) => match target {
            Target::Comp(_) => s.time(),
            Target::Item(_) => Tick::ZERO,
        },
    };
    let max_side = uint_p(p, &["max_side", "maxSide"], MAX_SIDE as u64, cmd)?.unwrap_or(0) as u32;
    let transparent = match present(p, "transparent") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(cmd, "`transparent` must be true or false"))?,
    };
    let region = present(p, "region").map(|v| region_p(v, fw, fh, cmd)).transpose()?;
    let overlays = Overlays::parse(present(p, "overlays")).map_err(|m| bad(cmd, m))?;
    Ok(ViewRequest { target, layer, time, max_side, transparent, region, overlays })
}

/// `region: [x, y, w, h]` (comp pixels) → whole pixels clipped to the `fw` × `fh` frame.
pub(crate) fn region_p(v: &Value, fw: u32, fh: u32, cmd: &str) -> Result<[f64; 4]> {
    let nums: Vec<f64> = v.as_array().map(|a| a.iter().filter_map(Value::as_f64).filter(|n| n.is_finite()).collect()).unwrap_or_default();
    let [x, y, w, h] = nums[..] else {
        return Err(bad(cmd, format!("`region` must be [x, y, width, height] in comp pixels, not {v}")));
    };
    if w <= 0.0 || h <= 0.0 {
        return Err(bad(cmd, format!("`region` {v} needs a positive width and height")));
    }
    let (x0, y0) = (x.floor().max(0.0), y.floor().max(0.0));
    let (x1, y1) = ((x + w).ceil().min(fw as f64), (y + h).ceil().min(fh as f64));
    if x1 <= x0 || y1 <= y0 {
        return Err(bad(cmd, format!("`region` {v} lies outside the {fw}×{fh} frame")));
    }
    Ok([x0, y0, x1 - x0, y1 - y0])
}

/// The project with only `lid` soloed (and its Video switch on) in `cid`, and whether it is a
/// guide layer (rendered then, though guides never render in comps).
fn solo_project(s: &Session, cid: ItemId, lid: LayerId) -> Result<(Project, bool)> {
    let mut p: Project = (*s.project).clone();
    let c = p.comp_mut(cid).ok_or(EngineError::NoComp)?;
    let mut guide = false;
    for l in c.layers.iter_mut() {
        l.switches.solo = l.id == lid;
        if l.id == lid {
            l.switches.video = true;
            guide = l.switches.guide;
        }
    }
    Ok((p, guide))
}

/// A project holding a comp the size of `item` with the item as its only layer (how the
/// Footage panel shows an item on its own).
fn item_project(s: &Session, item: ItemId, rate: FrameRate) -> Result<(Project, ItemId)> {
    let mut p: Project = (*s.project).clone();
    let it = p.item(item).ok_or_else(|| bad("render.frame", "no such item"))?.clone();
    let (w, h) = it.dimensions().ok_or_else(|| bad("render.frame", "the item has no size"))?;
    let (source, dur) = match &it.kind {
        ItemKind::Footage(f) => (LayerSource::Footage { item }, f.duration),
        ItemKind::Solid(_) => (LayerSource::Solid { item }, Tick::ZERO),
        _ => return Err(bad("render.frame", "only footage and solid items render on their own")),
    };
    // Stills have no duration: one long frame.
    let dur = if dur > Tick::ZERO { dur } else { Tick::from_seconds_f64(3600.0) };
    let mut comp = Comp::new(w.max(1), h.max(1), rate, dur);
    let layer = build::layer(&mut p, &comp, &it.name, source, (w, h), Some(dur));
    comp.layers.push(layer);
    let cid = p.add_item(&format!("{} (render)", it.name), effectcraft_color::Label::None, None, ItemKind::Comp(Arc::new(comp)));
    Ok((p, cid))
}

/// Render `cid` of `project` with the session's hosts at `scale`, optionally just `roi`.
fn render_image(s: &Session, project: &Project, cid: ItemId, t: Tick, scale: f64, roi: Option<[f64; 4]>, guides: bool) -> effectcraft_raster::Image {
    let opts = RenderOpts {
        scale,
        roi,
        guides,
        backend: Backend::Auto,
        nested_switches: s.prefs.general.switches_affect_nested_comps,
        draft_shadows: s.prefs.three_d.realtime_shadows,
        ..Default::default()
    };
    let mut r = Renderer::new(project, s.footage.as_ref(), opts);
    r.expr = s.expr.as_deref();
    r.accel = s.accel.as_deref();
    // The layer cache is keyed to the session's own project; edited copies render without it.
    if std::ptr::eq(project, &*s.project) {
        r.cache = Some(&s.layer_cache);
    }
    r.comp_frame(cid, t)
}

impl Session {
    /// Render what `r` asks for: pixels, overlays drawn on top, and a JSON description
    /// (`comp` or `item`, `layer`, `time`, `frame`, `width`, `height`, `scale`, `region`, and with
    /// bounds / anchor overlays the drawn `layers` in comp space).
    pub fn render_view(&self, r: &ViewRequest) -> Result<View> {
        let (fw, fh, rate) = target_frame(self, r.target).ok_or(EngineError::NoComp)?;
        let (project, cid, guides): (Cow<Project>, ItemId, bool) = match (r.target, r.layer) {
            (Target::Comp(cid), None) => (Cow::Borrowed(&*self.project), cid, false),
            (Target::Comp(cid), Some(lid)) => {
                let (p, guide) = solo_project(self, cid, lid)?;
                (Cow::Owned(p), cid, guide)
            }
            (Target::Item(item), _) => {
                let (p, cid) = item_project(self, item, rate)?;
                (Cow::Owned(p), cid, false)
            }
        };
        let comp = project.comp(cid).ok_or(EngineError::NoComp)?;
        let full = [0.0, 0.0, fw as f64, fh as f64];
        let region = r.region.unwrap_or(full);
        let long = region[2].max(region[3]).max(1.0);
        let scale = if r.max_side == 0 { 1.0 } else { (r.max_side as f64 / long).min(1.0) };
        let roi = r.region.filter(|g| *g != full);
        let img = render_image(self, &project, cid, r.time, scale, roi, guides);
        let (w, h) = (img.width, img.height);
        let mut rgba = if r.transparent { img.to_rgba8() } else { img.to_rgba8_over(comp.background) };
        drop(img);
        let mut info = match r.target {
            Target::Comp(c) => json!({"comp": c.0}),
            Target::Item(i) => json!({"comp": null, "item": i.0}),
        };
        info["time"] = json!(r.time.seconds());
        info["frame"] = json!(rate.frame_at(r.time));
        info["width"] = json!(w);
        info["height"] = json!(h);
        info["scale"] = json!(scale);
        if let Some(l) = r.layer {
            info["layer"] = json!(l.0);
            let active = self.project.comp(cid).and_then(|c| c.layer(l)).map(|l| l.is_active_at(r.time));
            if active == Some(false) {
                info["note"] = json!("the layer is not active at this time (outside its in/out points)");
            }
        }
        if let Some(g) = r.region {
            info["region"] = json!(g);
        }
        if r.overlays.any() {
            let geo = match r.target {
                Target::Comp(c) => overlay_layers(self, c, r.time, r.layer, &r.overlays),
                Target::Item(_) => vec![],
            };
            let place = overlay::Placement { origin: [region[0], region[1]], scale, frame: [fw as f64, fh as f64] };
            overlay::draw(&mut rgba, w, h, &place, &self.prefs, &geo, &r.overlays);
            info["overlays"] = json!(r.overlays.names());
            if r.overlays.bounds || r.overlays.anchor || r.overlays.motion_path {
                info["layers"] = Value::Array(geo.iter().map(LayerGeom::to_json).collect());
            }
        }
        Ok(View { width: w, height: h, rgba, info })
    }
}

/// Encode straight 8-bit RGBA as PNG; fully opaque images are written as RGB.
pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let mut out = Vec::new();
    let enc = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive);
    let px = rgba.as_chunks::<4>().0;
    let r = if px.iter().all(|p| p[3] == 255) {
        let rgb: Vec<u8> = px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        enc.write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
    } else {
        enc.write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
    };
    r.map_err(|e| EngineError::Other(format!("PNG encoding failed: {e}")))?;
    Ok(out)
}

/// Standard padded base64 of `data`.
pub(crate) fn base64(data: &[u8]) -> String {
    effectcraft_track::roto::rle::base64_encode(data)
}

/// Hand a PNG back: inline as base64 (`base64: true`, or `inline_default` without a `path`), else
/// written to `path` (default a temp file).
fn deliver_png(s: &Session, p: &Value, png: &[u8], out: &mut Value, inline_default: bool, stem: &str) -> Result<()> {
    let path = str_p(p, "path");
    // (the web has no file system and no temp dir: inline unless a path is given)
    let no_fs = cfg!(target_arch = "wasm32");
    if b_p(p, "base64").unwrap_or((inline_default || no_fs) && path.is_none()) {
        out["png"] = json!(base64(png));
        if path.is_none() {
            return Ok(());
        }
    }
    let path = match path {
        Some(p) => p.to_string(),
        None if no_fs => return Err(EngineError::Other("pass `path` or `base64: true` (no temp folder here)".into())),
        None => std::env::temp_dir().join(format!("effectcraft-{stem}-{}.png", std::process::id())).to_string_lossy().into_owned(),
    };
    s.services.write_file(&path, png).map_err(|e| EngineError::Other(format!("cannot write {path}: {e}")))?;
    out["path"] = json!(path);
    Ok(())
}

/// `render.frame`: render, then write `path` (default a temp file) or return base64. The control
/// channel calls this with `inline_default` on the web (no file system).
pub fn render_frame_reply(s: &Session, p: &Value, inline_default: bool) -> Result<Value> {
    let req = request(s, p, "render.frame")?;
    let v = s.render_view(&req)?;
    let png = encode_png(v.width, v.height, &v.rgba)?;
    let mut out = v.info;
    deliver_png(s, p, &png, &mut out, inline_default, "frame")?;
    Ok(out)
}

fn render_frame_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    if b_p(p, "wait") == Some(false) {
        return in_background(s, "render.frame", "Render Frame", p, |w, p| render_frame_reply(w, p, false));
    }
    render_frame_reply(s, p, false)
}

fn contact_sheet_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    if b_p(p, "wait") == Some(false) {
        return in_background(s, "render.contactSheet", "Render Contact Sheet", p, sheet::contact_sheet_reply);
    }
    sheet::contact_sheet_reply(s, p)
}

/// Run `render` as a background task (`wait: false`): on a session that shares what a render
/// reads (project, footage, expressions, GPU, layer cache, preferences, files) and copies the
/// caller's cursor (its comp and time, also an agent's own), so the UI thread and other clients
/// carry on meanwhile. Returns `{job}`; the task's result (quiet: no toast) is what the blocking
/// command returns, and a control-channel request waits for it. A bad request (no such comp,
/// item or layer, a bad time or region) fails up front, not in the task.
fn in_background(s: &mut Session, cmd: &str, label: &str, p: &Value, render: fn(&Session, &Value) -> Result<Value>) -> Result<Value> {
    let mut p = p.clone();
    if let Some(o) = p.as_object_mut() {
        o.remove("wait");
    }
    request(s, &p, cmd)?;
    let (project, services, footage, expr) = (s.project.clone(), s.services.clone(), s.footage.clone(), s.expr.clone());
    let (accel, layer_cache, prefs, state) = (s.accel.clone(), s.layer_cache.clone(), s.prefs.clone(), s.state.clone());
    s.spawn_task("render", label.to_string(), false, move |_ctl| {
        let w = Session { project, services, footage, expr, accel, layer_cache, prefs, state, ..Session::default() };
        let mut v = render(&w, &p).map_err(|e| e.to_string())?;
        v["quiet"] = json!(true);
        let apply: crate::jobs::Apply = Box::new(move |_| Ok(v));
        Ok(apply)
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "render.frame",
            "Render Frame",
            "{comp?, item?, layer?, time? (s), frame?, max_side?, transparent?, region?: [x, y, w, h], overlays?: true|[bounds, anchor, motionPath, safeMargins, grid], path?, base64?, wait? (false: a background task)}",
            render_frame_cmd
        ),
        query!(
            "render.contactSheet",
            "Render Contact Sheet",
            "{comp?, item?, layer?, times?: [s], frames?: [comp frame] | {from, to, every?}, count?, start? (s), end? (s), cols?, max_side?, labels?, transparent?, region?: [x, y, w, h], overlays?: true|[bounds, anchor, motionPath, safeMargins, grid], path?, gif?, base64?, wait? (false: a background task)}",
            contact_sheet_cmd
        ),
    ]
}

#[cfg(test)]
mod tests;
