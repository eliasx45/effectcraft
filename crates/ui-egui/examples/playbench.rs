//! Headless spacebar-playback benchmark: the real [`EffectcraftApp`](effectcraft_ui_egui::EffectcraftApp)
//! offscreen (as `snapshot`), playing comp ranges in real time and reporting what reached the
//! viewer: frames shown per second, time stalled (no new frame for longer than two frame
//! durations) and how far the playhead got.
//!
//! ```text
//! cargo run -p effectcraft-ui-egui --example playbench -- --project copy.ecproj --comp "10-10 MAIN" \
//!     [--range 0-6 --range 54-59] [--passes 2] [--step '<control json>']...
//! ```
//!
//! Each range plays from its start for its length in wall time, `--passes` times (the first pass
//! starts with nothing cached; later ones find what the first left in the RAM preview). `--step`
//! requests run once before the first range (`{"method":"engine.execute","params":{…}}`). The disk
//! cache is off so runs don't warm each other, settings are never saved (`--pref key=json` changes
//! one for the run) and previews render on this machine. One JSON line per range and pass.

use std::time::{Duration, Instant};

use effectcraft_ui_egui::offscreen::{OffscreenOptions, OffscreenUi};
use serde_json::{Value, json};

struct Args {
    project: String,
    comp: String,
    ranges: Vec<(f64, f64)>,
    passes: usize,
    prefs: Vec<(String, Value)>,
    steps: Vec<Value>,
    opts: OffscreenOptions,
}

const USAGE: &str =
    "usage: playbench --project p.ecproj --comp NAME [--range A-B]... [--passes N] [--size WxH] [--scale 2] [--pref key=json]... [--step json]...";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        project: String::new(),
        comp: String::new(),
        ranges: Vec::new(),
        passes: 2,
        prefs: Vec::new(),
        steps: Vec::new(),
        opts: OffscreenOptions::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--project" => a.project = val()?,
            "--comp" => a.comp = val()?,
            "--range" => {
                let v = val()?;
                let (s, e) = v.split_once('-').ok_or("--range A-B")?;
                let (s, e): (f64, f64) = (s.parse().map_err(|_| "bad range start")?, e.parse().map_err(|_| "bad range end")?);
                if !(s.is_finite() && e.is_finite() && e > s && s >= 0.0) {
                    return Err(format!("--range {v}: expected start < end"));
                }
                a.ranges.push((s, e));
            }
            "--pref" => {
                let v = val()?;
                let (k, json) = v.split_once('=').ok_or("--pref key=json")?;
                a.prefs.push((k.to_string(), serde_json::from_str(json).map_err(|e| format!("--pref {v}: {e}"))?));
            }
            "--size" => {
                let v = val()?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                a.opts.size = [w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?];
            }
            "--scale" => a.opts.pixels_per_point = val()?.parse().map_err(|_| "bad --scale")?,
            "--passes" => a.passes = val()?.parse().map_err(|_| "bad --passes")?,
            "--step" => a.steps.push(serde_json::from_str(&val()?).map_err(|e| format!("--step: {e}"))?),
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    if a.project.is_empty() || a.comp.is_empty() {
        return Err(USAGE.into());
    }
    if a.ranges.is_empty() {
        a.ranges.push((0.0, 6.0));
    }
    Ok(a)
}

fn ok(ui: &OffscreenUi, method: &str, params: Value) -> Result<Value, String> {
    let r = ui.call(method, params.clone())?;
    if r["ok"] != json!(true) {
        return Err(format!("{method} {params}: {}", r["error"]));
    }
    Ok(r["result"].clone())
}

fn exec(ui: &OffscreenUi, command: &str, params: Value) -> Result<Value, String> {
    ok(ui, "engine.execute", json!({"command": command, "params": params}))
}

/// Play `range` once and measure it.
fn pass(ui: &OffscreenUi, (start, end): (f64, f64), n: usize) -> Result<Value, String> {
    exec(ui, "time.set", json!({"time": start}))?;
    // The paused frame at the start renders first (playback starts where a user would press play).
    std::thread::sleep(Duration::from_secs(3));
    let paused = ok(ui, "ui.inspect", json!({}))?;
    let fps = paused["preview"]["targetFps"].as_f64().unwrap_or(0.0);
    let paused_scale = paused["latency"]["shownScale"].as_f64().map(|s| s / 1000.0);
    ok(ui, "ui.playback", json!({"action": "play"}))?;
    let t0 = Instant::now();
    let wall = end - start;
    let mut stall = 0.0;
    let mut last = (Instant::now(), 0u64);
    let mut polled = Instant::now();
    let mut scales = std::collections::BTreeMap::<String, u64>::new();
    let mut fps_target = fps;
    while t0.elapsed().as_secs_f64() < wall {
        std::thread::sleep(Duration::from_millis(20));
        let i = ok(ui, "ui.inspect", json!({}))?;
        let p = &i["preview"];
        fps_target = p["targetFps"].as_f64().unwrap_or(fps_target);
        let shown = p["shown"].as_u64().unwrap_or(0);
        let now = Instant::now();
        if shown != last.1 {
            last = (now, shown);
            if let Some(s) = i["latency"]["shownScale"].as_u64() {
                *scales.entry(format!("{:.3}", s as f64 / 1000.0)).or_default() += 1;
            }
        } else if fps_target > 0.0 {
            // The part of the time since the last poll that is more than two frames after the
            // last new frame.
            let over = now.duration_since(last.0).as_secs_f64() - 2.0 / fps_target;
            stall += over.min(now.duration_since(polled).as_secs_f64()).max(0.0);
        }
        polled = now;
    }
    let i = ok(ui, "ui.inspect", json!({}))?;
    ok(ui, "ui.playback", json!({"action": "stop"}))?;
    let p = &i["preview"];
    let secs = t0.elapsed().as_secs_f64();
    let shown = p["intervals"]["shown"].as_u64().unwrap_or(0);
    Ok(json!({
        "range": format!("{start}-{end}"),
        "pass": n,
        "targetFps": fps_target,
        "fps": (shown as f64 / secs * 10.0).round() / 10.0,
        "stallSecs": (stall * 100.0_f64).round() / 100.0,
        "secs": (secs * 100.0).round() / 100.0,
        "reached": p["time"],
        "intervals": p["intervals"],
        "scales": scales,
        "pausedScale": paused_scale,
    }))
}

fn run(args: Args) -> Result<(), String> {
    let project = args.project.clone();
    let comp = args.comp.clone();
    let prefs = args.prefs.clone();
    let ui = OffscreenUi::start(args.opts, move || {
        let mut session = effectcraft_host::session();
        // Your settings as they are, but never saved back (a bench must not change them), and
        // previews on this machine (never a cloud frame server).
        session.config = None;
        session.prefs.set("previews.previewOn", json!("local"))?;
        for (k, v) in prefs {
            session.prefs.set(&k, v)?;
        }
        session.prefs_changed();
        session.execute_checked("file.open", json!({"path": project})).map_err(|e| e.to_string())?;
        session.execute_checked("comp.open", json!({"comp": comp})).map_err(|e| e.to_string())?;
        session.disk_cache = None;
        Ok(session)
    })?;
    ui.wait_started()?;
    eprintln!("adapter: {}", ui.adapter());
    for step in &args.steps {
        let method = step["method"].as_str().unwrap_or_default();
        let reply = ui.call(method, step.get("params").cloned().unwrap_or(json!({})))?;
        eprintln!("{method}: {reply}");
    }
    for n in 1..=args.passes {
        for &r in &args.ranges {
            println!("{}", pass(&ui, r, n)?);
        }
    }
    Ok(())
}

fn main() {
    let result = parse_args().and_then(run);
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
