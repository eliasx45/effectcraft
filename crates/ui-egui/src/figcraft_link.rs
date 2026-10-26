//! The FigCraft link, like Dynamic Link: FigCraft shows and plays EffectCraft comps on its canvas.
//!
//! - Control-channel methods FigCraft calls (docs/control-protocol.md, "FigCraft link"):
//!   `figcraft.frames` (JPEG frames from the RAM preview, answered at once with what is cached;
//!   the rest are queued as prefetch renders and reported `pending`), `figcraft.audio` (the comp's
//!   mix as a WAV), `figcraft.tree` (the precomps that show) and `figcraft.reveal` (open a comp at
//!   a time, as double-clicking it in the Project panel does).
//! - `comp.sendToFigCraft`'s frontend half: the engine works out the comp, range and the
//!   `effectcraft.place` params (`effectcraft_engine::commands::figcraft_link`); this sends them to
//!   FigCraft's control channel (127.0.0.1, port `FIGCRAFT_CONTROL_PORT`, default 7979) on a
//!   thread and toasts the outcome ([`poll`]).
//!
//! The UI thread only looks frames up and clones their `Arc`s; reading GPU frames back, scaling,
//! JPEG and WAV encoding and base64 run on a thread that answers the request itself
//! ([`crate::control::Outcome::Held`]). Frames are never requested urgently: an urgent request
//! would cancel the viewer's own frames of the comp.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use effectcraft_engine::time::Tick;
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::control::{ControlRequest, Outcome};
use crate::frames::{FrameImage, FrameKey};

/// The most frames one `figcraft.frames` request may ask for.
pub const MAX_FRAMES: usize = 64;
/// The longest audio range one `figcraft.audio` request may ask for (seconds).
pub const MAX_AUDIO_SECONDS: f64 = 300.0;
/// Render scales (thousandths) whose cached frames FigCraft can get, smallest first.
pub const SCALES: [u32; 4] = [250, 333, 500, 1000];
/// Encoding threads running at once; frames beyond that are reported pending.
const MAX_RUNNING: usize = 4;
/// The default FigCraft control port.
pub const FIGCRAFT_PORT: u16 = 7979;

static RUNNING: AtomicUsize = AtomicUsize::new(0);
/// Outcomes of `comp.sendToFigCraft` sends that finished, for [`poll`] to show.
static SENT: Mutex<Vec<Result<(), String>>> = Mutex::new(Vec::new());

/// One of the [`MAX_RUNNING`] encoding slots, given back when dropped.
struct Busy;

impl Busy {
    fn take() -> Option<Busy> {
        if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Busy)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A render scale in thousandths as the factor the viewer renders at (1/3 exactly).
pub fn scale_factor(milli: u32) -> f64 {
    if milli == 333 { 1.0 / 3.0 } else { f64::from(milli) / 1000.0 }
}

/// The scale (thousandths) frames are rendered at for FigCraft: the smallest of [`SCALES`]
/// whose longest side reaches `max_side`, else full size.
pub fn request_scale(comp_long_side: u32, max_side: u32) -> u32 {
    SCALES.into_iter().find(|s| f64::from(comp_long_side) * scale_factor(*s) + 0.5 >= f64::from(max_side)).unwrap_or(1000)
}

/// Which of the cached frames of `sizes` to send for `max_side`: the smallest whose longest
/// side reaches it, else the largest.
pub fn pick(sizes: &[[usize; 2]], max_side: usize) -> Option<usize> {
    let long = |i: &usize| sizes.get(*i).map_or(0, |&[w, h]| w.max(h));
    let enough = (0..sizes.len()).filter(|i| long(i) >= max_side).min_by_key(long);
    enough.or_else(|| (0..sizes.len()).max_by_key(long))
}

/// The size `w`×`h` fits in `max_side` (never larger than it is).
pub fn fit(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h);
    if max_side == 0 || long <= max_side {
        return (w, h);
    }
    let s = f64::from(max_side) / f64::from(long);
    (((f64::from(w) * s).round() as u32).max(1), ((f64::from(h) * s).round() as u32).max(1))
}

/// A cached frame (premultiplied RGBA8) as a JPEG over black, longest side at most `max_side`:
/// `(width, height, jpeg)`. Premultiplied colour over black is the colour channels as they are.
pub fn encode_jpeg(rgba: &[u8], w: u32, h: u32, max_side: u32, quality: u8) -> Result<(u32, u32, Vec<u8>), String> {
    let n = (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4)).ok_or("frame too large")?;
    if w == 0 || h == 0 || rgba.len() != n {
        return Err("the frame's size doesn't match its pixels".into());
    }
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
    let img = image::RgbImage::from_raw(w, h, rgb).ok_or("the frame's size doesn't match its pixels")?;
    let (nw, nh) = fit(w, h, max_side);
    let img = if (nw, nh) == (w, h) { img } else { image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle) };
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .write_image(img.as_raw(), nw, nh, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("jpeg: {e}"))?;
    Ok((nw, nh, out))
}

/// Interleaved 16-bit PCM samples as a WAV file.
pub fn wav_pcm16(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data = u32::try_from(samples.len().saturating_mul(2)).unwrap_or(u32::MAX);
    let block = channels.saturating_mul(2);
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&rate.saturating_mul(u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// A float sample as 16-bit PCM (clipped; NaN is silence).
pub fn pcm16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// `frames` stereo sample frames of a mix from `start` at `rate`, mixed a second at a time by
/// `mix(start, frames, rate)` (interleaved L R), as 16-bit PCM; `None` when it is all silence.
pub fn mix_pcm(mix: impl Fn(Tick, usize, u32) -> Vec<f32>, start: Tick, frames: usize, rate: u32) -> Option<Vec<i16>> {
    let mut pcm = Vec::with_capacity(frames.saturating_mul(2));
    let mut audible = false;
    let mut done = 0usize;
    while done < frames {
        let n = (frames - done).min(rate.max(1) as usize);
        let at = start + Tick::from_units(done as i64, i64::from(rate.max(1)));
        let block = mix(at, n, rate);
        for i in 0..n * 2 {
            let v = pcm16(block.get(i).copied().unwrap_or(0.0));
            audible |= v != 0;
            pcm.push(v);
        }
        done += n;
    }
    audible.then_some(pcm)
}

// ---------------------------------------------------------------- params

/// `figcraft.frames`' params.
#[derive(Clone, Debug, PartialEq)]
pub struct FramesParams {
    pub comp: Value,
    /// Integral frame numbers, without duplicates, in the order asked.
    pub frames: Vec<i64>,
    pub max_side: u32,
    pub quality: u8,
}

/// `figcraft.audio`'s params.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioParams {
    pub comp: Value,
    pub start: f64,
    pub end: f64,
    pub rate: u32,
}

fn comp_param(p: &Value) -> Result<Value, String> {
    match p.get("comp") {
        Some(v @ (Value::Number(_) | Value::String(_))) => Ok(v.clone()),
        _ => Err("need `comp` (a composition id or name)".into()),
    }
}

/// A whole number (`3` or `3.0`).
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|x| x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15).map(|x| x as i64))
}

/// A number clamped to `lo..=hi`; absent is `default` (an error when there is none).
fn clamped(p: &Value, key: &str, lo: i64, hi: i64, default: Option<i64>) -> Result<i64, String> {
    match p.get(key) {
        None | Some(Value::Null) => default.ok_or_else(|| format!("need `{key}`")),
        Some(v) => v.as_f64().filter(|x| x.is_finite()).map(|x| (x.round() as i64).clamp(lo, hi)).ok_or_else(|| format!("`{key}` must be a number")),
    }
}

/// Optional finite seconds.
fn seconds(p: &Value, key: &str) -> Result<Option<f64>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().filter(|x| x.is_finite()).map(Some).ok_or_else(|| format!("`{key}` must be comp seconds")),
    }
}

pub fn parse_frames(p: &Value) -> Result<FramesParams, String> {
    let comp = comp_param(p)?;
    let list = p.get("frames").and_then(Value::as_array).ok_or("need `frames` (comp frame numbers)")?;
    if list.len() > MAX_FRAMES {
        return Err(format!("at most {MAX_FRAMES} frames per request (got {})", list.len()));
    }
    let mut frames = Vec::with_capacity(list.len());
    for f in list.iter().filter_map(int) {
        if !frames.contains(&f) {
            frames.push(f);
        }
    }
    let max_side = clamped(p, "maxSide", 32, 1920, None)? as u32;
    let quality = clamped(p, "quality", 30, 95, Some(80))? as u8;
    Ok(FramesParams { comp, frames, max_side, quality })
}

pub fn parse_audio(p: &Value) -> Result<AudioParams, String> {
    let comp = comp_param(p)?;
    let start = seconds(p, "start")?.ok_or("need `start` (comp seconds)")?.max(0.0);
    let end = seconds(p, "end")?.ok_or("need `end` (comp seconds)")?;
    if end <= start {
        return Err("`end` must be after `start`".into());
    }
    if end - start > MAX_AUDIO_SECONDS {
        return Err(format!("at most {MAX_AUDIO_SECONDS} s of audio per request"));
    }
    let rate = clamped(p, "rate", 8000, 96000, Some(48000))? as u32;
    Ok(AudioParams { comp, start, end, rate })
}

// ---------------------------------------------------------------- control methods

fn ok(v: Value) -> Outcome {
    Outcome::Done(json!({"ok": true, "result": v}))
}

fn err(e: impl std::fmt::Display) -> Outcome {
    Outcome::Done(json!({"ok": false, "error": e.to_string()}))
}

/// `figcraft.frames` / `figcraft.audio` / `figcraft.tree` / `figcraft.reveal`.
pub fn handle(app: &mut EffectcraftApp, ctx: &egui::Context, req: &ControlRequest) -> Outcome {
    let p = &req.params;
    let r = match req.method.as_str() {
        "figcraft.frames" => frames(app, p, &req.reply),
        "figcraft.audio" => audio(app, p, &req.reply),
        "figcraft.tree" => tree(app, p).map(Some),
        "figcraft.reveal" => reveal(app, ctx, p).map(Some),
        m => Err(format!("unknown method `{m}`")),
    };
    match r {
        Ok(Some(v)) => ok(v),
        Ok(None) => Outcome::Held,
        Err(e) => err(format!("{}: {e}", req.method)),
    }
}

fn resolve(app: &EffectcraftApp, comp: &Value) -> Result<effectcraft_engine::project::ItemId, String> {
    app.session.resolve_comp(Some(comp)).map_err(|e| e.to_string())
}

/// What FigCraft gets for one frame.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum Slot {
    Ready(FrameImage),
    Pending,
    Failed,
}

/// Look the frames up (and queue the missing ones); `None` when a thread answers.
fn frames(app: &mut EffectcraftApp, p: &Value, reply: &std::sync::mpsc::Sender<Value>) -> Result<Option<Value>, String> {
    let req = parse_frames(p)?;
    let id = resolve(app, &req.comp)?;
    let comp = app.session.project.comp_arc(id).ok_or("no such composition")?;
    let (rate, duration) = (comp.frame_rate, comp.duration);
    // The viewer's series of the comp at each scale (and the scale it shows now).
    let mut scales = SCALES.to_vec();
    let shown = app.viewer_shown.as_ref().filter(|(_, k)| k.comp == id.0).map(|(_, k)| k.scale);
    if let Some(s) = shown.filter(|s| *s > 0 && !SCALES.contains(s)) {
        scales.push(s);
    }
    let series: Vec<FrameKey> = scales.iter().map(|s| app.frame_series(id, scale_factor(*s))).collect();
    let want = request_scale(comp.width.max(comp.height).max(1), req.max_side);
    let want_series = series.iter().find(|k| k.scale == want).copied().unwrap_or_else(|| app.frame_series(id, scale_factor(want)));
    let opts = app.frame_opts(id, scale_factor(want));
    let mut src = None;
    let mut slots = Vec::with_capacity(req.frames.len());
    for f in req.frames.iter().copied().filter(|f| *f >= 0 && rate.tick_of(*f) < duration) {
        let found: Vec<FrameImage> = series.iter().filter_map(|k| app.frames.get(&FrameKey { frame: f, ..*k })).collect();
        let sizes: Vec<[usize; 2]> = found.iter().map(FrameImage::size).collect();
        if let Some(img) = pick(&sizes, req.max_side as usize).and_then(|i| found.get(i)) {
            slots.push((f, Slot::Ready(img.clone())));
            continue;
        }
        let key = FrameKey { frame: f, ..want_series };
        if app.frames.failed(&key) {
            slots.push((f, Slot::Failed));
            continue;
        }
        // Prefetch priority with the viewer's options: the viewer reuses what this renders, and
        // a frame in the disk cache is just read back.
        let src = src.get_or_insert_with(|| app.render_source());
        app.frames.request(src, key, id, rate.tick_of(f), opts);
        slots.push((f, Slot::Pending));
    }
    let head = json!({"comp": id.0, "revision": app.session.revision, "width": comp.width, "height": comp.height, "frameRate": rate.as_f64()});
    let ready = slots.iter().any(|(_, s)| matches!(s, Slot::Ready(_)));
    let busy = if ready && !cfg!(target_arch = "wasm32") { Busy::take() } else { None };
    let Some(busy) = busy else {
        // Nothing to encode, or every encoder is busy (FigCraft asks again for pending frames).
        let list: Vec<Value> = slots
            .iter()
            .map(|(f, s)| if matches!(s, Slot::Failed) { json!({"frame": f, "failed": true}) } else { json!({"frame": f, "pending": true}) })
            .collect();
        return Ok(Some(with_frames(head, list)));
    };
    spawn_frames(app.gpu.clone(), slots, req.max_side, req.quality, head, reply.clone(), busy)
}

fn with_frames(mut head: Value, frames: Vec<Value>) -> Value {
    if let Some(m) = head.as_object_mut() {
        m.insert("frames".into(), Value::Array(frames));
    }
    head
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_frames(
    gpu: Option<effectcraft_gpu::Gpu>,
    slots: Vec<(i64, Slot)>,
    max_side: u32,
    quality: u8,
    head: Value,
    reply: std::sync::mpsc::Sender<Value>,
    busy: Busy,
) -> Result<Option<Value>, String> {
    std::thread::Builder::new()
        .name("ec-figcraft-frames".into())
        .spawn(move || {
            let _busy = busy;
            let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode_slots(&slots, gpu.as_ref(), max_side, quality)));
            let v = match encoded {
                Ok(list) => json!({"ok": true, "result": with_frames(head, list)}),
                Err(_) => json!({"ok": false, "error": "figcraft.frames: encoding the frames failed"}),
            };
            let _ = reply.send(v);
        })
        .map_err(|e| format!("can't start the encoder: {e}"))?;
    Ok(None)
}

#[cfg(target_arch = "wasm32")]
fn spawn_frames(
    _: Option<effectcraft_gpu::Gpu>,
    _: Vec<(i64, Slot)>,
    _: u32,
    _: u8,
    _: Value,
    _: std::sync::mpsc::Sender<Value>,
    _: Busy,
) -> Result<Option<Value>, String> {
    Err("the FigCraft link runs in the desktop app".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn encode_slots(slots: &[(i64, Slot)], gpu: Option<&effectcraft_gpu::Gpu>, max_side: u32, quality: u8) -> Vec<Value> {
    slots
        .iter()
        .map(|(f, s)| match s {
            Slot::Pending => json!({"frame": f, "pending": true}),
            Slot::Failed => json!({"frame": f, "failed": true}),
            Slot::Ready(img) => match jpeg_of(img, gpu, max_side, quality) {
                Some((w, h, jpeg)) => json!({"frame": f, "w": w, "h": h, "jpeg": crate::control::base64(&jpeg)}),
                None => json!({"frame": f, "failed": true}),
            },
        })
        .collect()
}

/// A cached frame as a JPEG (a GPU frame is read back first).
#[cfg(not(target_arch = "wasm32"))]
fn jpeg_of(img: &FrameImage, gpu: Option<&effectcraft_gpu::Gpu>, max_side: u32, quality: u8) -> Option<(u32, u32, Vec<u8>)> {
    let [w, h] = img.size();
    let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let r = match img {
        FrameImage::Cpu(c) => encode_jpeg(c.as_raw(), w, h, max_side, quality),
        FrameImage::Gpu(f) => {
            let bytes = gpu?.read_display(f)?;
            encode_jpeg(&bytes, w, h, max_side, quality)
        }
    };
    r.map_err(|e| log::warn!("figcraft.frames: {e}")).ok()
}

/// `figcraft.audio`: `{silent: true}` at once for a comp without audio, else mixed on a thread.
fn audio(app: &mut EffectcraftApp, p: &Value, reply: &std::sync::mpsc::Sender<Value>) -> Result<Option<Value>, String> {
    let req = parse_audio(p)?;
    let id = resolve(app, &req.comp)?;
    let src = app.render_source();
    if !effectcraft_engine::render::audio::comp_has_audio(&src.project, id) {
        return Ok(Some(json!({"silent": true})));
    }
    spawn_audio(src, id, req, reply.clone())
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_audio(
    src: crate::frames::RenderSource,
    id: effectcraft_engine::project::ItemId,
    req: AudioParams,
    reply: std::sync::mpsc::Sender<Value>,
) -> Result<Option<Value>, String> {
    let busy = Busy::take().ok_or("busy encoding other requests: ask again")?;
    let (start, end, rate) = (req.start, req.end, req.rate);
    let frames = ((end - start) * f64::from(rate)).round() as usize;
    std::thread::Builder::new()
        .name("ec-figcraft-audio".into())
        .spawn(move || {
            let _busy = busy;
            let mixed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mix = |at: Tick, n: usize, r: u32| {
                    effectcraft_engine::render::audio::mix_comp(&src.project, src.footage.as_ref(), src.expr.as_deref(), id, at, n, r)
                };
                mix_pcm(mix, Tick::from_seconds_f64(start), frames, rate).map(|pcm| crate::control::base64(&wav_pcm16(&pcm, rate, 2)))
            }));
            let v = match mixed {
                Ok(Some(wav)) => json!({"ok": true, "result": {"rate": rate, "channels": 2, "start": start, "end": end, "wav": wav}}),
                Ok(None) => json!({"ok": true, "result": {"silent": true}}),
                Err(_) => json!({"ok": false, "error": "figcraft.audio: mixing failed"}),
            };
            let _ = reply.send(v);
        })
        .map_err(|e| format!("can't start the mixer: {e}"))?;
    Ok(None)
}

#[cfg(target_arch = "wasm32")]
fn spawn_audio(
    _: crate::frames::RenderSource,
    _: effectcraft_engine::project::ItemId,
    _: AudioParams,
    _: std::sync::mpsc::Sender<Value>,
) -> Result<Option<Value>, String> {
    Err("the FigCraft link runs in the desktop app".into())
}

/// `figcraft.tree`.
fn tree(app: &EffectcraftApp, p: &Value) -> Result<Value, String> {
    let comp = comp_param(p)?;
    let id = resolve(app, &comp)?;
    let duration = app.session.project.comp(id).ok_or("no such composition")?.duration.seconds();
    let start = seconds(p, "start")?.unwrap_or(0.0);
    let end = seconds(p, "end")?.unwrap_or(duration);
    if end < start {
        return Err("`end` must not be before `start`".into());
    }
    let depth = clamped(p, "depth", 1, i64::from(effectcraft_engine::commands::figcraft_link::TREE_DEPTH_MAX), Some(3))? as u32;
    effectcraft_engine::commands::figcraft_link::tree(&app.session.project, id, Tick::from_seconds_f64(start), Tick::from_seconds_f64(end), depth)
        .ok_or_else(|| "no such composition".into())
}

/// `figcraft.reveal`: the comp opens (its viewer and Timeline tab, as a double-click in the
/// Project panel does) at `time`, and the window comes to the front.
fn reveal(app: &mut EffectcraftApp, ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    let comp = comp_param(p)?;
    let time = seconds(p, "time")?.ok_or("need `time` (comp seconds)")?;
    let id = resolve(app, &comp)?;
    crate::menus::invoke(app, ctx, "comp.open", json!({"comp": id.0}))?;
    if app.session.active_comp_id() != Some(id) {
        return Err("the composition didn't open".into());
    }
    crate::menus::run_engine(app, ctx, "time.set", json!({"time": time.max(0.0)}))?;
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    Ok(json!({"comp": id.0, "time": app.session.time().seconds()}))
}

// ---------------------------------------------------------------- Send to FigCraft

/// FigCraft's control port: `FIGCRAFT_CONTROL_PORT`, else [`FIGCRAFT_PORT`].
pub fn figcraft_port() -> u16 {
    std::env::var("FIGCRAFT_CONTROL_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(FIGCRAFT_PORT)
}

/// The request line that places a comp on FigCraft's canvas (`comp.sendToFigCraft`'s result).
pub fn place_line(params: &Value) -> Result<String, String> {
    let req = json!({"id": 1, "method": "engine.execute", "params": {"command": "effectcraft.place", "params": params}});
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');
    Ok(line)
}

/// FigCraft's reply line: `Ok` when it placed the comp, else what went wrong.
pub fn read_reply(line: &str) -> Result<(), String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("FigCraft closed the connection without answering".into());
    }
    let v: Value = serde_json::from_str(line).map_err(|e| format!("FigCraft's answer isn't JSON: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("FigCraft: {}", v.get("error").and_then(Value::as_str).unwrap_or("it didn't place the comp")))
    }
}

/// The frontend half of `comp.sendToFigCraft`: send on a thread (FigCraft may ask this app for
/// frames before it answers), shown by [`poll`] when it finishes.
#[cfg(not(target_arch = "wasm32"))]
pub fn send(ctx: &egui::Context, params: Value) -> Result<Value, String> {
    let line = place_line(&params)?;
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("ec-figcraft-send".into())
        .spawn(move || {
            let r = deliver(&line, figcraft_port());
            SENT.lock().unwrap_or_else(PoisonError::into_inner).push(r);
            ctx.request_repaint();
        })
        .map_err(|e| format!("can't reach FigCraft: {e}"))?;
    Ok(json!({"sending": true}))
}

#[cfg(target_arch = "wasm32")]
pub fn send(_: &egui::Context, _: Value) -> Result<Value, String> {
    Err("Send to FigCraft runs in the desktop app".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn deliver(line: &str, port: u16) -> Result<(), String> {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::time::Duration;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(1500)).map_err(|e| match e.kind() {
        ErrorKind::ConnectionRefused => "FigCraft isn't open".to_string(),
        ErrorKind::TimedOut | ErrorKind::WouldBlock => format!("FigCraft isn't answering on port {port}"),
        _ => format!("FigCraft isn't open ({e})"),
    })?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
    s.write_all(line.as_bytes()).map_err(|e| format!("sending to FigCraft: {e}"))?;
    let mut reply = String::new();
    // One reply line, at most 1 MiB.
    BufReader::new(&s).take(1 << 20).read_line(&mut reply).map_err(|e| format!("FigCraft didn't answer: {e}"))?;
    read_reply(&reply)
}

/// Once per frame: toast the sends that finished ("Sent to FigCraft", or what went wrong).
pub fn poll(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let done = {
        let mut sent = SENT.lock().unwrap_or_else(PoisonError::into_inner);
        if sent.is_empty() {
            return;
        }
        std::mem::take(&mut *sent)
    };
    let now = ctx.input(|i| i.time);
    for r in done {
        let text = match r {
            Ok(()) => "Sent to FigCraft".to_string(),
            Err(e) => {
                app.ui.status = e.clone();
                e
            }
        };
        app.toast = Some((text, now));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use effectcraft_engine::Session;
    use serde_json::{Value, json};

    use super::*;

    fn request(app: &mut EffectcraftApp, ctx: &egui::Context, method: &str, params: Value) -> (Outcome, std::sync::mpsc::Receiver<Value>) {
        let (req, rx) = ControlRequest::new(method, params);
        (crate::control::handle(app, ctx, &req), rx)
    }

    fn done(o: Outcome) -> Value {
        match o {
            Outcome::Done(v) => v,
            _ => json!({"ok": "not answered at once"}),
        }
    }

    /// An app with comp "Outer" (640×360, 25 fps, 4 s) holding a layer of comp "Inner".
    fn app() -> (EffectcraftApp, u64, u64) {
        let mut s = Session::default();
        let new = |s: &mut Session, name: &str| {
            s.execute("comp.new", json!({"name": name, "width": 640, "height": 360, "frameRate": 25, "duration": 4})).unwrap()["comp"].as_u64().unwrap()
        };
        let inner = new(&mut s, "Inner");
        let outer = new(&mut s, "Outer");
        s.execute("comp.open", json!({"comp": outer})).unwrap();
        s.execute("layer.addItem", json!({"item": inner})).unwrap();
        (EffectcraftApp::new(s), outer, inner)
    }

    #[test]
    fn scales_and_picks() {
        assert_eq!(request_scale(1920, 480), 250);
        assert_eq!(request_scale(1920, 481), 333);
        assert_eq!(request_scale(1920, 640), 333);
        assert_eq!(request_scale(1920, 960), 500);
        assert_eq!(request_scale(1920, 961), 1000);
        assert_eq!(request_scale(1080, 1920), 1000);
        assert_eq!(request_scale(0, 32), 1000);
        // The smallest that reaches maxSide, else the largest.
        let sizes = [[480, 270], [960, 540], [1920, 1080], [640, 360]];
        assert_eq!(pick(&sizes, 500), Some(3));
        assert_eq!(pick(&sizes, 2000), Some(2));
        assert_eq!(pick(&sizes, 32), Some(0));
        assert_eq!(pick(&[], 100), None);
        assert_eq!(fit(1920, 1080, 480), (480, 270));
        assert_eq!(fit(100, 50, 480), (100, 50));
        assert_eq!(fit(1, 4000, 32), (1, 32));
    }

    #[test]
    fn jpeg_is_over_black_and_fits_max_side() {
        // Half-transparent premultiplied red over black is dark red.
        let px: Vec<u8> = (0..64 * 32).flat_map(|_| [128u8, 0, 0, 128]).collect();
        let (w, h, jpeg) = encode_jpeg(&px, 64, 32, 32, 90).unwrap();
        assert_eq!((w, h), (32, 16));
        assert_eq!(jpeg.get(..3), Some(&[0xFF, 0xD8, 0xFF][..]));
        let back = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(back.dimensions(), (32, 16));
        let p = back.get_pixel(16, 8).0;
        assert!((p[0] as i32 - 128).abs() < 12 && p[1] < 12 && p[2] < 12, "{p:?}");
        // Sizes that don't match the pixels are refused.
        assert!(encode_jpeg(&px, 65, 32, 32, 90).is_err());
        assert!(encode_jpeg(&[], 0, 0, 32, 90).is_err());
        assert!(encode_jpeg(&px, u32::MAX, u32::MAX, 32, 90).is_err());
    }

    #[test]
    fn wav_header_is_16_bit_pcm() {
        let wav = wav_pcm16(&[0, 1, -1, i16::MAX], 48_000, 2);
        assert_eq!(wav.len(), 44 + 8);
        let u32_at = |i: usize| u32::from_le_bytes(wav[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(wav[i..i + 2].try_into().unwrap());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32_at(4), 36 + 8);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!((u32_at(16), u16_at(20), u16_at(22)), (16, 1, 2), "PCM, stereo");
        assert_eq!((u32_at(24), u32_at(28), u16_at(32), u16_at(34)), (48_000, 192_000, 4, 16));
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32_at(40), 8);
        assert_eq!(&wav[44..46], &[0, 0]);
        assert_eq!(&wav[50..52], &i16::MAX.to_le_bytes());
        assert_eq!((pcm16(2.0), pcm16(-2.0), pcm16(f32::NAN), pcm16(0.5)), (32767, -32767, 0, 16384));
    }

    #[test]
    fn audio_is_mixed_a_second_at_a_time_and_silence_is_none() {
        // 1.5 s at 8 kHz: two blocks, the second starting exactly 1 s later.
        let starts = std::cell::RefCell::new(vec![]);
        let pcm = mix_pcm(
            |at, n, _| {
                starts.borrow_mut().push((at, n));
                vec![0.25; n * 2]
            },
            Tick::ZERO,
            12_000,
            8_000,
        )
        .unwrap();
        assert_eq!(pcm.len(), 24_000);
        assert_eq!(*starts.borrow(), vec![(Tick::ZERO, 8_000), (Tick::from_seconds_f64(1.0), 4_000)]);
        // A short block is padded; all zero is silence.
        assert_eq!(mix_pcm(|_, _, _| vec![0.5], Tick::ZERO, 10, 8_000).map(|p| p.len()), Some(20));
        assert!(mix_pcm(|_, n, _| vec![0.0; n * 2], Tick::ZERO, 100, 8_000).is_none());
    }

    #[test]
    fn params_are_validated_and_clamped() {
        let f = parse_frames(&json!({"comp": 3, "frames": [5, 5, 2.0, 2.5, "x", null, -1], "maxSide": 1e9})).unwrap();
        assert_eq!((f.frames, f.max_side, f.quality), (vec![5, 2, -1], 1920, 80));
        assert_eq!(parse_frames(&json!({"comp": "A", "frames": [], "maxSide": 1, "quality": 200})).map(|f| (f.max_side, f.quality)), Ok((32, 95)));
        for junk in [
            json!(null),
            json!([1, 2]),
            json!({}),
            json!({"comp": {}, "frames": [1], "maxSide": 100}),
            json!({"comp": 1, "frames": 3, "maxSide": 100}),
            json!({"comp": 1, "frames": [1]}),
            json!({"comp": 1, "frames": [1], "maxSide": "big"}),
            json!({"comp": 1, "frames": [1], "maxSide": 100, "quality": [80]}),
            json!({"comp": 1, "frames": vec![0; MAX_FRAMES + 1], "maxSide": 100}),
        ] {
            assert!(parse_frames(&junk).is_err(), "{junk}");
        }
        let a = parse_audio(&json!({"comp": 1, "start": -2, "end": 3})).unwrap();
        assert_eq!((a.start, a.end, a.rate), (0.0, 3.0, 48_000));
        assert_eq!(parse_audio(&json!({"comp": 1, "start": 0, "end": 1, "rate": 1})).map(|a| a.rate), Ok(8_000));
        for junk in [
            json!({"comp": 1, "end": 3}),
            json!({"comp": 1, "start": 0}),
            json!({"comp": 1, "start": 3, "end": 3}),
            json!({"comp": 1, "start": 0, "end": 301}),
            json!({"comp": 1, "start": 0, "end": "1"}),
            json!({"comp": 1, "start": 0, "end": 1, "rate": "fast"}),
            json!({"start": 0, "end": 1}),
        ] {
            assert!(parse_audio(&junk).is_err(), "{junk}");
        }
    }

    #[test]
    fn junk_requests_get_errors_never_panics() {
        let (mut app, outer, _) = app();
        let ctx = egui::Context::default();
        let junk = [
            json!(null),
            json!("x"),
            json!([]),
            json!({}),
            json!({"comp": "No Such Comp", "frames": [0], "maxSide": 100, "start": 0, "end": 1, "time": 0}),
            json!({"comp": 999_999, "frames": [0], "maxSide": 100, "start": 0, "end": 1, "time": 0}),
            json!({"comp": outer, "frames": "all", "maxSide": -5, "start": "a", "end": [1], "time": "now", "depth": "deep"}),
            json!({"comp": outer, "start": 3, "end": 1, "time": f64::MAX, "maxSide": 100, "frames": [0]}),
        ];
        for method in ["figcraft.frames", "figcraft.audio", "figcraft.tree", "figcraft.reveal"] {
            for p in &junk {
                let v = done(request(&mut app, &ctx, method, p.clone()).0);
                if v["ok"] == true {
                    // Only a valid request may pass (the last one is valid for frames and reveal).
                    assert!(matches!(method, "figcraft.frames" | "figcraft.reveal"), "{method} {p}: {v}");
                } else {
                    assert!(v["error"].as_str().is_some_and(|e| e.starts_with(method)), "{method} {p}: {v}");
                }
            }
        }
    }

    #[test]
    fn frames_answer_at_once_and_queue_what_is_missing() {
        let (mut app, outer, _) = app();
        let ctx = egui::Context::default();
        // Frame 100 is past the 4 s comp and dropped; nothing is cached yet: pending, queued once.
        let p = json!({"comp": "Outer", "frames": [0, 1, 1, 100, -3], "maxSide": 160});
        let v = done(request(&mut app, &ctx, "figcraft.frames", p.clone()).0);
        assert_eq!(v["ok"], true, "{v}");
        let r = &v["result"];
        assert_eq!((r["comp"].as_u64(), r["width"].as_u64(), r["height"].as_u64(), r["frameRate"].as_f64()), (Some(outer), Some(640), Some(360), Some(25.0)));
        assert_eq!(r["revision"], app.session.revision);
        assert_eq!(r["frames"], json!([{"frame": 0, "pending": true}, {"frame": 1, "pending": true}]));
        // Asking again is fine (the frames are queued once, or have landed meanwhile).
        let (o, _rx) = request(&mut app, &ctx, "figcraft.frames", p);
        assert!(matches!(o, Outcome::Held) || done(o)["ok"] == true);

        // A cached quarter-size frame (160×90) is sent from a thread, scaled to maxSide.
        let key = app.frame_key(effectcraft_engine::project::ItemId(outer), 2, 0.25);
        let img = egui::ColorImage::new([160, 90], vec![egui::Color32::from_rgba_premultiplied(200, 100, 50, 255); 160 * 90]);
        app.frames.insert_streamed(key, Arc::new(img), false);
        let (o, rx) = request(&mut app, &ctx, "figcraft.frames", json!({"comp": outer, "frames": [2], "maxSide": 64, "quality": 90}));
        assert!(matches!(o, Outcome::Held));
        let v = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        let f = &v["result"]["frames"][0];
        assert_eq!((f["frame"].as_i64(), f["w"].as_u64(), f["h"].as_u64()), (Some(2), Some(64), Some(36)), "{v}");
        assert!(f["jpeg"].as_str().is_some_and(|j| j.starts_with("/9j/")), "a base64 JPEG");
    }

    #[test]
    fn audio_of_a_silent_comp_and_the_tree_and_reveal() {
        let (mut app, outer, inner) = app();
        let ctx = egui::Context::default();
        let v = done(request(&mut app, &ctx, "figcraft.audio", json!({"comp": outer, "start": 0, "end": 2})).0);
        assert_eq!(v["result"], json!({"silent": true}), "{v}");

        let v = done(request(&mut app, &ctx, "figcraft.tree", json!({"comp": outer})).0);
        let t = &v["result"];
        assert_eq!(t["comp"]["name"], "Outer", "{v}");
        assert_eq!(t["comp"]["duration"], 4.0);
        assert_eq!(t["children"].as_array().map(Vec::len), Some(1));
        assert_eq!(t["children"][0]["comp"]["id"], inner);
        assert_eq!(t["children"][0]["timeRemap"], false);
        let v = done(request(&mut app, &ctx, "figcraft.tree", json!({"comp": outer, "start": 5, "end": 6})).0);
        assert_eq!(v["result"]["children"], json!([]), "nothing shows after the comp ends");

        let v = done(request(&mut app, &ctx, "figcraft.reveal", json!({"comp": "Inner", "time": 2})).0);
        assert_eq!(v["result"], json!({"comp": inner, "time": 2.0}), "{v}");
        assert_eq!(app.session.active_comp_id().map(|c| c.0), Some(inner));
    }

    #[test]
    fn place_line_and_replies() {
        let line = place_line(&json!({"comp": "A", "compId": 3})).unwrap();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["method"], "engine.execute");
        assert_eq!(v["params"]["command"], "effectcraft.place");
        assert_eq!(v["params"]["params"]["compId"], 3);
        assert_eq!(read_reply("{\"id\":1,\"ok\":true,\"result\":{}}\n"), Ok(()));
        assert_eq!(read_reply("{\"id\":1,\"ok\":false,\"error\":\"no page\"}"), Err("FigCraft: no page".into()));
        assert!(read_reply("").is_err());
        assert!(read_reply("nope").is_err());
    }

    #[test]
    fn a_send_nobody_listens_to_says_figcraft_isnt_open() {
        // A port nothing listens on (bound, then freed).
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        assert_eq!(deliver("{}\n", port), Err("FigCraft isn't open".into()));
        // The outcome is toasted on the next frame.
        let (mut app, _, _) = app();
        let ctx = egui::Context::default();
        SENT.lock().unwrap().push(Err("FigCraft isn't open".into()));
        poll(&mut app, &ctx);
        assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("FigCraft isn't open"));
    }
}
