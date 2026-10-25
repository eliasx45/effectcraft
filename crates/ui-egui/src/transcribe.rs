//! The desktop app's transcriber (SX2): `speech.transcribe {layer}` without a words file hands the
//! job here (`Event::Frontend`, `menus/speech.rs`). A background thread runs, per the owner's
//! transcription rules (~/.claude/CLAUDE.md):
//!
//! 1. `scripts/fork/speech-map` (Scribe text → Montreal Forced Aligner → forward onset snap: the
//!    stressed syllables an animation peak lands on), when `aligned` and the script is found;
//! 2. else, or when it fails, `scribe-words` (ElevenLabs Scribe, its own faster-whisper fallback
//!    offline; words only, no stress). Never VAD.
//!
//! on the layer's part of its media (`--start`/`--dur`, media seconds), then [`poll`] imports the
//! JSON with `speech.transcribe {comp, layer, path}` (one undo step) on the UI thread. The key comes
//! from the environment or the Keychain (the scripts read it themselves); nothing here sees it.
//!
//! Paths: `EFFECTCRAFT_SPEECH_MAP` / `EFFECTCRAFT_SCRIBE_WORDS` override where the scripts are.

use std::sync::Mutex;
use std::sync::mpsc::Receiver;

use serde_json::{Value, json};

use crate::EffectcraftApp;

/// A transcription running in the background.
struct Job {
    comp: u64,
    layer: u64,
    name: String,
    started: std::time::Instant,
    /// The words JSON written (path, what made it), or why it failed.
    rx: Receiver<Result<(String, String), String>>,
}

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());

fn jobs() -> std::sync::MutexGuard<'static, Vec<Job>> {
    JOBS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Seconds the transcription of `layer` has been running, if one is.
pub fn running(layer: u64) -> Option<f64> {
    jobs().iter().find(|j| j.layer == layer).map(|j| j.started.elapsed().as_secs_f64())
}

/// Start transcribing (the `speech.transcribe` frontend event's params: `{comp, layer, name, media,
/// start, dur, fps, local, aligned}`).
pub fn start(ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    let num = |k: &str| p.get(k).and_then(Value::as_f64);
    let comp = p.get("comp").and_then(Value::as_u64).ok_or("speech.transcribe: no comp")?;
    let layer = p.get("layer").and_then(Value::as_u64).ok_or("speech.transcribe: no layer")?;
    let media = p.get("media").and_then(Value::as_str).filter(|m| !m.is_empty()).ok_or("speech.transcribe: the layer's media has no file")?.to_string();
    let name = p.get("name").and_then(Value::as_str).unwrap_or("voice").to_string();
    if running(layer).is_some() {
        return Err(format!("{name} is already being transcribed"));
    }
    let args = Args {
        media,
        start: num("start").unwrap_or(0.0).max(0.0),
        dur: num("dur").filter(|d| *d > 0.0),
        fps: num("fps").filter(|f| f.is_finite() && *f > 0.0).unwrap_or(29.97),
        local: p.get("local").and_then(Value::as_bool).unwrap_or(false),
        aligned: p.get("aligned").and_then(Value::as_bool).unwrap_or(true),
        layer,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx2 = ctx.clone();
    std::thread::Builder::new()
        .name("transcribe".into())
        .spawn(move || {
            let _ = tx.send(run(&args));
            ctx2.request_repaint();
        })
        .map_err(|e| format!("can't start the transcriber: {e}"))?;
    jobs().push(Job { comp, layer, name: name.clone(), started: std::time::Instant::now(), rx });
    Ok(json!({"started": true, "layer": layer}))
}

/// Once per frame (app top level): import what finished.
pub fn poll(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let mut done: Vec<(u64, u64, String, Result<(String, String), String>)> = vec![];
    {
        let mut js = jobs();
        if js.is_empty() {
            return;
        }
        js.retain(|j| match j.rx.try_recv() {
            Ok(r) => {
                done.push((j.comp, j.layer, j.name.clone(), r));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                done.push((j.comp, j.layer, j.name.clone(), Err("the transcriber stopped".into())));
                false
            }
        });
        if !js.is_empty() {
            // Keep the elapsed-time readouts moving.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
    let now = ctx.input(|i| i.time);
    for (comp, layer, name, r) in done {
        match r {
            Ok((path, engine)) => match crate::menus::run_engine(app, ctx, "speech.transcribe", json!({"comp": comp, "layer": layer, "path": path})) {
                Ok(v) => {
                    let n = v.get("words").and_then(Value::as_u64).unwrap_or(0);
                    let stressed = v.get("stressed").and_then(Value::as_u64).unwrap_or(0);
                    app.toast = Some((format!("{name}: {n} words ({stressed} stressed syllables), {engine}"), now));
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => app.ui.status = format!("Transcribing {name}: {e} (the words are in {path})"),
            },
            Err(e) => app.ui.status = format!("Transcribing {name} failed: {e}"),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
struct Args {
    media: String,
    start: f64,
    dur: Option<f64>,
    fps: f64,
    local: bool,
    aligned: bool,
    layer: u64,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default()
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
/// `speech-map`: the override, next to the app's repo checkout, else the owner's checkout.
fn speech_map() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("EFFECTCRAFT_SPEECH_MAP").map(std::path::PathBuf::from) {
        return Some(p);
    }
    let exe = std::env::current_exe().ok();
    let from_exe = exe.as_ref().and_then(|e| e.ancestors().map(|d| d.join("scripts/fork/speech-map")).find(|c| c.is_file()));
    from_exe.or_else(|| Some(home().join("Developer/effectcraft/scripts/fork/speech-map")).filter(|p| p.is_file()))
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn scribe_words() -> std::path::PathBuf {
    std::env::var_os("EFFECTCRAFT_SCRIBE_WORDS")
        .map(std::path::PathBuf::from)
        .or_else(|| Some(home().join(".local/bin/scribe-words")).filter(|p| p.is_file()))
        .unwrap_or_else(|| "scribe-words".into())
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
/// PATH with the places ffmpeg, uv, curl and the scripts live (an app opened from the Dock gets a
/// bare PATH).
fn path_env() -> std::ffi::OsString {
    let mut dirs = vec![home().join(".local/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn run(a: &Args) -> Result<(String, String), String> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out = std::env::temp_dir().join(format!("effectcraft-words-{}-{}-{stamp}.json", std::process::id(), a.layer));
    let tool = |exe: &std::path::Path| -> Result<(), String> {
        let mut c = std::process::Command::new(exe);
        c.arg(&a.media).arg("--start").arg(format!("{:.3}", a.start)).arg("--fps").arg(format!("{}", a.fps)).arg("--out").arg(&out);
        if let Some(d) = a.dur {
            c.arg("--dur").arg(format!("{d:.3}"));
        }
        if a.local {
            c.arg("--local");
        }
        c.env("PATH", path_env()).stdin(std::process::Stdio::null());
        let r = c.output().map_err(|e| format!("can't run {}: {e}", exe.display()))?;
        if r.status.success() && out.is_file() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&r.stderr);
            let tail: String = err.trim().chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
            Err(format!("{} failed: {tail}", exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
        }
    };
    let mut why = String::new();
    if a.aligned
        && let Some(sm) = speech_map()
    {
        match tool(&sm) {
            Ok(()) => return Ok((out.to_string_lossy().to_string(), "speech-map (Scribe + forced alignment)".into())),
            Err(e) => why = format!("{e}; "),
        }
    }
    match tool(&scribe_words()) {
        Ok(()) => Ok((out.to_string_lossy().to_string(), if a.local { "faster-whisper, words only".into() } else { "Scribe, words only (no stress)".into() })),
        Err(e) => Err(format!("{why}{e}")),
    }
}

#[cfg(target_arch = "wasm32")]
fn run(_: &Args) -> Result<(String, String), String> {
    Err("transcription runs in the desktop app".into())
}
