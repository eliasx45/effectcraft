//! EffectCraft headless CLI: one-shot agent commands, frame rendering and the MCP server.
//!
//! ```text
//! effectcraft-cli info                                        project + engine summary
//! effectcraft-cli commands [--filter TEXT] [--enabled]        list engine commands
//! effectcraft-cli exec <command-id> [--params JSON]           run one command
//! effectcraft-cli exec --list [--filter TEXT] [--schemas]     list engine commands (as `commands`)
//! effectcraft-cli run <id> <json> [<id> <json> …]             run several commands in order
//! effectcraft-cli props <comp> <layer> [--flat] [--time S]    a layer's property tree (with paths)
//! effectcraft-cli get <comp> <layer> <path> [--time S]        read a property
//! effectcraft-cli set <comp> <layer> <path> <value> [--time S] [--expression E]
//! effectcraft-cli render-frame [--comp C] [--time S|--frame N] [--max-side PX|--scale K] [--out F.png] [--transparent]
//! effectcraft-cli render [--comp C] --out FILE [--format h264|hevc|av1|prores|webm|png|jpeg|tiff|exr|gif|wav|aiff] [--start S] [--end S]
//!     [--work-area] [--fps N] [--resolution full|half|third|quarter|K] [--quality best|draft] [--channels rgb|rgba]
//!     [--jpeg-quality N] [--bitrate KBPS] [--prores proxy|lt|standard|hq|4444|4444xq] [--audio auto|on|off]
//!     [--profile main|main10] [--level auto|4.1] [--rate-control bitrate|quality] [--video-quality 1-100]
//!     [--keyint FRAMES] [--webm-codec vp9|av1] [--audio-bitrate KBPS] [--opus-app audio|voice]
//!     (a relative --out is relative to the working directory; --queue outputs follow the project's own settings)
//! effectcraft-cli render --template T.ectemplate [--values V.json] --out FILE
//! effectcraft-cli render-frame --template T.ectemplate [--values V.json] [--time S] [--out F.png]
//! effectcraft-cli render F.ecproj --queue                    render the project's Render Queue
//! effectcraft-cli compare A.png B.png [--threshold L] [--min-psnr DB] [--max-side PX] [--diff D.png]
//!     compare two images (goldens, After Effects references): metrics; exit 1 when they differ
//! effectcraft-cli bench [--comp C] [--time S] [--scale K] [--n N] [--play N] [--gpu [--adv3d]]   render timings
//!     (--gpu: CPU vs GPU ms/frame for every comp at Full and Half)
//! effectcraft-cli bench --ops [--small] [--layers N] [--comps N] [--footage N]   everyday-operation timings
//!     on a large generated project (open, save, auto-save, undo/redo, timeline, Project panel)
//! effectcraft-cli script FILE.jsx [F.ecproj] | --eval CODE    run an After Effects-style script
//! effectcraft-cli replay SCRIPT.jsonl [--keep-going]        replay a repro script (docs/agents.md)
//! effectcraft-cli mcp [--autosave] [--bridge [PORT] | --ui] [--all-tools] [--client NAME]  MCP server on stdio
//! effectcraft-cli figma-bridge [--port 7789] [--control 9877] local endpoint for the Figma plug-in
//! effectcraft-cli mirror USER@HOST [--ssh-port P] [--identity KEY] [--project F.ecproj | --bridge 9877]
//!     [--font-dir DIR] [--remote-cli PATH] [--dry-run] [--all] [--verify-hash] [--no-retime] [--watch SECS]
//!     copy the media and fonts a project reads to a cloud render machine (same absolute paths), then preflight there
//!
//! Project:  --project F.ecproj (or a positional *.ecproj) | --demo | --empty   (default: demo; mcp: empty)
//! Saving:   --save (back to --project) | --save-as F.ecproj
//! GPU:      --gpu renders on the GPU compositor (Mercury GPU Acceleration) when an adapter exists;
//!           the default is the CPU (Mercury Software Only)
//! Bridge:   --bridge PORT drives a running `effectcraft --control PORT` instead of a headless session
//! UI:       mcp --ui [--ui-size WxH] [--ui-scale K] runs the full app offscreen (no window) for
//!           screenshot and the ui_* tools (feature `ui`, on by default); with --autosave its
//!           project is checkpointed like headless work
//! Output:   --json for one compact JSON document on stdout (errors: {"error": …}, exit 1)
//! ```
//!
//! `<comp>` is a comp id or name (`-` = the active comp); `<layer>` an id, `#n` or name; `<value>`
//! is JSON (`50`, `[960,540]`, `"#ff0000"`) or a bare string. See `docs/agents.md`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::Write;

mod asset_guard;
mod drive;
mod figma_bridge;
mod frame_serve;
mod library_pull;
mod mirror;
#[cfg(feature = "ui")]
mod offscreen_ui;
mod stream_cli;
mod template;

use effectcraft_automation::tools::{self, Reply};
use effectcraft_automation::{Backend, McpServer};
use serde_json::{Value, json};

/// `println!` that never panics: see [`write_line`].
macro_rules! say {
    ($($t:tt)*) => {
        write_line(format_args!($($t)*))
    };
}

/// `eprintln!` that never panics: a diagnostic that can't be written is dropped.
macro_rules! note {
    ($($t:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

/// Why stdout stopped taking output, once it has.
static STDOUT_FAILED: std::sync::OnceLock<std::io::ErrorKind> = std::sync::OnceLock::new();

/// Write a line to stdout. Once that fails, later output is dropped but the command still
/// finishes its work (a `run … --save` sequence isn't cut short). A reader that closes stdout
/// early (`info --json | head`) has read what it wanted; any other failure is reported and makes
/// the exit status 1 ([`stdout_broken`]).
fn write_line(text: std::fmt::Arguments) {
    if STDOUT_FAILED.get().is_some() {
        return;
    }
    let r = {
        let mut out = std::io::stdout().lock();
        out.write_fmt(text).and_then(|()| out.write_all(b"\n"))
    };
    if let Err(e) = r {
        let _ = STDOUT_FAILED.set(e.kind());
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            note!("effectcraft-cli: cannot write to stdout: {e}");
        }
    }
}

/// Whether stdout failed for a reason other than its reader closing it early.
fn stdout_broken() -> bool {
    STDOUT_FAILED.get().is_some_and(|k| *k != std::io::ErrorKind::BrokenPipe)
}

const USAGE: &str = "usage: effectcraft-cli <info|commands|exec|run|props|get|set|render-frame|render|compare|script|replay|mcp|figma-bridge> [args] [--json]
  info                                     project + engine summary
  commands [--filter TEXT] [--enabled]     list engine commands
  exec <command-id> [--params JSON]        run one engine command (exec --list: list them, as `commands`)
  run <id> <json> [<id> <json> ...]        run several commands in order
  props <comp> <layer> [--flat] [--time S] a layer's property tree with paths
  get <comp> <layer> <path> [--time S]     read a property
  set <comp> <layer> <path> <value> [--time S] [--expression E]
  render-frame [--comp C] [--time S | --frame N] [--max-side PX | --scale K] [--out F.png] [--transparent]
  render [--comp C] --out FILE [--format F] [--link-output] [--start S] [--end S] [--work-area] [--fps N]
         [--resolution full|half|third|quarter|K] [--quality best|draft] [--channels rgb|rgba]
         [--jpeg-quality N] [--bitrate KBPS] [--prores PROFILE] [--audio auto|on|off]
         [--profile main|main10] [--level auto|4.1] [--rate-control bitrate|quality] [--video-quality N]
         [--keyint FRAMES] [--webm-codec vp9|av1] [--audio-bitrate KBPS] [--opus-app audio|voice] | --queue
                                           (formats h264|hevc|av1|prores|webm|png|jpeg|tiff|exr|gif|wav|aiff;
                                           --profile..--keyint: HEVC / AV1, --audio-bitrate/--opus-app: WebM Opus)
                                           a relative --out is relative to the working directory
  compare A.png B.png [--threshold L] [--min-psnr DB] [--max-side PX] [--amplify K] [--diff D.png]
                                           compare two PNG/JPEG images (max/mean abs diff, PSNR, SSIM, changed
                                           pixels; --diff writes a heatmap); exit 1 when they don't pass
  render/render-frame --template T.ectemplate [--values V.json]   render one template instance
  bench [--comp C] [--time S] [--scale K] [--n N] [--play N] [--gpu [--adv3d]]   per-layer/effect render timings;
                                           --play N renders N consecutive frames with/without the layer cache;
                                           --gpu compares CPU and GPU ms/frame for every comp at Full and Half
                                           --hw-prores: ProRes footage decodes on the hardware decoder (macOS)
  bench --ops [--small] [--layers N] [--comps N] [--footage N]
                                           everyday operations on a large generated project: startup, open,
                                           save, auto-save, edits + undo/redo, timeline, Project panel
  script FILE.jsx [F.ecproj] | --eval CODE run JavaScript with the After Effects-style object model
                                           (app.project, comps, layers, properties…); prints writeLn
                                           output and the result; errors exit 1 with file:line:col
  replay SCRIPT.jsonl [--keep-going] [--bless]
                                           replay a repro script headless (journal.export, crash repros):
                                           per-step ok/error and the first failure; exits 1 when a step
                                           fails, an expectation is not met or a command panics;
                                           --bless writes `golden` frames instead of comparing them
  mcp [--autosave] [--bridge [PORT] | --ui] MCP server (JSON-RPC over stdio); --autosave preserves unsaved work
                                           (headless or --ui; a bridged app owns its auto-saves);
                                           --bridge without a port uses EFFECTCRAFT_CONTROL_PORT, else 9877;
                                           --render-box PORT sends renders and lint to a render box
                                           (serve --follow), edits still to the app;
                                           --ui [--ui-size 1680x1020] [--ui-scale 2] runs the app offscreen
                                           (no window) so screenshot and the ui_* tools work;
                                           --all-tools also lists the older tool aliases;
                                           --client NAME (or EFFECTCRAFT_CLIENT) runs every tool call as
                                           that agent: its own selection, comp and time, comp claims
      [--automation-read-root DIR] [--automation-write-root DIR]
                                           confine the agent's file access: reads under a read or write root,
                                           writes under a write root (each repeatable); scripts are refused
  serve [F.ecproj | --demo | --follow APP_PORT] [--control PORT] [--gpu] [--frames PORT [--token T]]
                                           a windowless host: one project that several agents work on at
                                           once over the control channel (join with mcp --bridge PORT or
                                           the MCP hosts tool); prints the port; checkpoints unsaved work;
                                           in the host list until it stops (host.release saves and stops it);
                                           --frames PORT also serves viewer frames to a desktop app over the
                                           binary frame channel (loopback: reach it through an SSH tunnel),
                                           token from --token, EFFECTCRAFT_FRAME_TOKEN or <config>/frame-server-token;
                                           --follow APP_PORT makes it a render box: a read-only mirror of the
                                           app on that control port that agents render and lint on
                                           (mcp --bridge APP_PORT --render-box PORT), off the app's GPU work
  frames-bench --frames HOST:PORT [--token T] [F.ecproj | --demo] [--comp ID] [--scale S] [--max-side PX]
      [--jpeg-quality Q] [--n N] [--duration SECS]
                                           measure a frame server: round trips, scrub latency, a soak of
                                           random requests
  stream-bench [F.ecproj | --demo] [--comp C] [--start SECS] [--n FRAMES] [--gpu] [--scale S] [--render-only]
                                           render consecutive frames of a comp and time the playback
                                           stream's H.264 chunk encoding (sizes, threads, bitrate)
  stream-test --frames HOST:PORT [--token T] [F.ecproj | --demo] [--comp C] [--start SECS] [--duration SECS]
      [--edit-at SECS] [--ahead SECS] [--scale S] [--max-side PX] [--fixed]
                                           play a comp from a frame server's playback stream: warm-up, each
                                           chunk against its deadline, bitrate, stalls; --edit-at changes a layer
  figma-bridge [--port 7789] [--control 9877] [--code CODE]
                                           local endpoint for the EffectCraft Figma plug-in: imports what it
                                           sends into the running app (file.importFigma); prints the pairing code
  mirror USER@HOST [--ssh-port P] [--identity KEY] [--project F.ecproj | --bridge 9877] [--font-dir DIR]
         [--remote-cli PATH] [--env-file PATH] [--dry-run] [--all] [--verify-hash] [--no-retime] [--watch SECS]
                                           media + fonts to a cloud render machine (same paths), then cloud.preflight there
  library-pull --jobs FILE.json             on the cloud machine: Motion Library files from the library's Worker (read token)
options: --project F.ecproj | --demo | --empty   --save | --save-as F   --bridge PORT   --json   --gpu
         --frame-threads N  render at most N frames at once (viewer frame pool, export batches;
                            default one per core, 2-16; also EFFECTCRAFT_FRAME_THREADS)
<comp>: id or name, '-' = active comp; <layer>: id, '#n' or name; <value>: JSON or bare string";

/// Options that take a value.
const VALUED: &[&str] = &[
    "--frame-threads",
    "--trace",
    "--profile-offset",
    "--rt-inflight",
    "--n",
    "--profile-offset",
    "--play",
    "--layers",
    "--comps",
    "--footage",
    "--params",
    "--project",
    "--bridge",
    "--render-box",
    "--follow",
    "--client",
    "--save-as",
    "--filter",
    "--time",
    "--frame",
    "--comp",
    "--max-side",
    "--scale",
    "--out",
    "--expression",
    "--depth",
    "--eval",
    "--port",
    "--control",
    "--code",
    // serve --frames / frames-bench
    "--frames",
    "--token",
    "--duration",
    "--edit-at",
    "--ahead",
    // compare
    "--threshold",
    "--min-psnr",
    "--diff",
    "--amplify",
    "--ui-size",
    "--ui-scale",
    // render
    "--template",
    "--values",
    "--format",
    "--start",
    "--end",
    "--fps",
    "--resolution",
    "--quality",
    "--channels",
    "--jpeg-quality",
    "--bitrate",
    "--prores",
    "--audio",
    "--profile",
    "--level",
    "--rate-control",
    "--video-quality",
    "--keyint",
    "--webm-codec",
    "--audio-bitrate",
    "--opus-app",
    // mirror
    "--ssh-port",
    "--identity",
    "--font-dir",
    "--remote-cli",
    "--env-file",
    "--watch",
    "--jobs",
    "--upload",
    "--upload-limit-mb",
    // mcp
    "--automation-read-root",
    "--automation-write-root",
];

/// Options without a value. Any other `--option` is a usage error.
const FLAGS: &[&str] = &[
    "--rt-profile",
    "--rt-warm",
    "--rt-checksum",
    "--hw-prores",
    "--rt-drag",
    "--rt-warmup",
    "--json",
    "--save",
    "--demo",
    "--empty",
    "--gpu",
    "--fixed",
    "--render-only",
    "--list",
    "--schemas",
    "--enabled",
    "--flat",
    "--transparent",
    "--work-area",
    "--queue",
    "--link-output",
    "--ops",
    "--small",
    "--adv3d",
    "--layers",
    "--autosave",
    "--all-tools",
    "--ui",
    "--keep-going",
    "--bless",
    // mirror
    "--dry-run",
    "--all",
    "--verify-hash",
    "--retime",
    "--no-retime",
];

struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
    /// `--project F` or a positional `*.ecproj`.
    project: Option<String>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Result<Args, String> {
        let (mut pos, mut opts, mut unknown) = (vec![], vec![], vec![]);
        let mut it = raw.into_iter().peekable();
        while let Some(a) = it.next() {
            if a.starts_with("--") && a.len() > 2 {
                let (k, inline) = match a.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (a, None),
                };
                if VALUED.contains(&k.as_str()) {
                    // `--bridge` / `--control` without a port: the app's default control port.
                    let port_default = matches!(k.as_str(), "--bridge" | "--control") && inline.is_none() && it.peek().is_none_or(|n| n.starts_with("--"));
                    let v = if port_default { Some(default_control_port()) } else { inline.or_else(|| it.next()) };
                    let v = v.ok_or_else(|| format!("{k} needs a value"))?;
                    opts.push((k, Some(v)));
                } else if !FLAGS.contains(&k.as_str()) {
                    unknown.push(format!("`{k}`"));
                } else if inline.is_some() {
                    return Err(format!("{k} takes no value"));
                } else {
                    opts.push((k, None));
                }
            } else {
                pos.push(a);
            }
        }
        // Before anything runs: a misspelt option must not run the command with defaults, or
        // leave its value behind as a positional (`--saveas x.ecproj` opening x.ecproj).
        if !unknown.is_empty() {
            let s = if unknown.len() == 1 { "" } else { "s" };
            return Err(format!("unknown option{s} {}", unknown.join(", ")));
        }
        let mut a = Args { pos, opts, project: None };
        a.project = match a.opt("--project") {
            Some(p) => Some(p.to_string()),
            None => a.pos.iter().position(|x| x.ends_with(".ecproj")).map(|i| a.pos.remove(i)),
        };
        Ok(a)
    }
    fn flag(&self, k: &str) -> bool {
        self.opts.iter().any(|(o, _)| o == k)
    }
    /// Every value of a repeatable option, in order.
    fn opts(&self, k: &str) -> Vec<&str> {
        self.opts.iter().filter(|(o, _)| o == k).filter_map(|(_, v)| v.as_deref()).collect()
    }
    fn opt(&self, k: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(o, _)| o == k).and_then(|(_, v)| v.as_deref())
    }
    fn num(&self, k: &str) -> Result<Option<f64>, String> {
        self.opt(k).map(|v| v.parse::<f64>().map_err(|_| format!("{k}: not a number: {v}"))).transpose()
    }
}

/// The desktop app's control port: `EFFECTCRAFT_CONTROL_PORT` when set, else 9877.
fn default_control_port() -> String {
    control_port_or_default(std::env::var("EFFECTCRAFT_CONTROL_PORT").ok())
}

fn control_port_or_default(env: Option<String>) -> String {
    env.map(|p| p.trim().to_string()).filter(|p| p.parse::<u16>().is_ok()).unwrap_or_else(|| "9877".to_string())
}

/// A comp/layer reference: number → id, `-` → none (active), else a name / `#n`.
fn reference(s: &str) -> Option<Value> {
    match s {
        "-" | "" => None,
        _ => Some(s.parse::<u64>().map(Value::from).unwrap_or_else(|_| json!(s))),
    }
}

/// JSON if it parses, else the bare string.
fn value_arg(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!(s))
}

/// The command line as text. `std::env::args` panics on an argument that is not Unicode (a
/// Latin-1 file name on Linux). Every argument is used as a string, and a lossy copy would read or
/// write another file than the one named, so such an argument is a usage error naming it.
fn utf8_args(raw: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Vec<String>, String> {
    raw.into_iter().map(|a| a.into_string().map_err(|a| format!("argument `{}` is not valid Unicode", a.to_string_lossy()))).collect()
}

fn main() {
    let raw = match utf8_args(std::env::args_os().skip(1)) {
        Ok(raw) => raw,
        Err(e) => fail_usage(&e),
    };
    if matches!(raw.first().map(String::as_str), Some("--version" | "-V" | "version")) {
        say!("effectcraft-cli {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if raw.is_empty() || raw.iter().any(|a| a == "-h" || a == "--help") {
        say!("{USAGE}");
        std::process::exit(if raw.is_empty() { 2 } else { 0 });
    }
    let mut args = match Args::parse(raw) {
        Ok(a) => a,
        Err(e) => fail_usage(&e),
    };
    let json_out = args.flag("--json");
    if args.pos.is_empty() {
        fail_usage("missing subcommand");
    }
    // Warnings (a GPU that ran out of memory, a fallback to the CPU) on standard error when
    // asked for with RUST_LOG (`RUST_LOG=warn effectcraft-cli mcp --ui …`).
    if std::env::var_os("RUST_LOG").is_some() {
        effectcraft_engine::logging::install();
    }
    // CR1: before any frame pool is made.
    match args.num("--frame-threads") {
        Ok(Some(n)) if n >= 1.0 && n.fract() == 0.0 => effectcraft_render::concurrency::set_frame_threads(n as usize),
        Ok(None) => {}
        _ => fail_usage("--frame-threads: expected a whole number of at least 1"),
    }
    // Hardware ProRes decoding for previews (the app's Settings ▸ Previews ▸ Hardware ProRes
    // Decoding; a CLI session turns it on with `bench --hw-prores`). Exports always decode exactly.
    let _ = effectcraft_platform::register();
    let cmd = args.pos.remove(0);
    match run(&cmd, &args, json_out) {
        Ok(()) if stdout_broken() => std::process::exit(1),
        Ok(()) => {}
        Err(Failure::Usage(e)) => fail_usage(&e),
        Err(Failure::Error(e)) => {
            if json_out {
                say!("{}", json!({"error": e}));
            }
            note!("effectcraft-cli {cmd}: {e}");
            std::process::exit(1);
        }
    }
}

fn fail_usage(e: &str) -> ! {
    note!("effectcraft-cli: {e}\n{USAGE}");
    std::process::exit(2)
}

enum Failure {
    Usage(String),
    Error(String),
}
impl From<effectcraft_automation::Error> for Failure {
    fn from(e: effectcraft_automation::Error) -> Self {
        Failure::Error(e.to_string())
    }
}
impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Error(e)
    }
}

fn usage_err<T>(m: &str) -> Result<T, Failure> {
    Err(Failure::Usage(m.to_string()))
}

/// Build the backend: a bridge to the app, the app offscreen (`--ui`), or a headless session, with
/// the requested project.
fn backend(args: &Args, default_demo: bool) -> Result<Backend, Failure> {
    // Opening a project another EffectCraft has open warns (the host list), and saving over a
    // file changed meanwhile is refused.
    if let Some(dir) = effectcraft_host::config_dir() {
        effectcraft_engine::hosts::init(&dir);
    }
    if let Some(addr) = args.opt("--bridge") {
        if args.project.is_some() || args.flag("--demo") {
            return usage_err("--bridge drives the running app's project; use `exec file.open` to open another");
        }
        let b = Backend::bridge(addr)?;
        return Ok(match args.opt("--render-box") {
            Some(r) => b.with_render_box(r)?,
            None => b,
        });
    }
    if args.opt("--render-box").is_some() {
        return usage_err("--render-box needs --bridge PORT: the box mirrors a running app");
    }
    if args.flag("--ui") {
        return offscreen_backend(args, default_demo);
    }
    let mut b = Backend::headless(session(args)?);
    if let Some(p) = &args.project {
        b.exec("file.open", json!({"path": p}))?;
    } else if args.flag("--demo") || (default_demo && !args.flag("--empty")) {
        b.exec("file.openDemoProject", json!({}))?;
    }
    Ok(b)
}

/// `--ui`: the full app offscreen, reached through its control channel like a bridged app.
#[cfg(feature = "ui")]
fn offscreen_backend(args: &Args, default_demo: bool) -> Result<Backend, Failure> {
    use effectcraft_ui_egui::offscreen::OffscreenOptions;
    let mut opts = OffscreenOptions::default();
    if let Some(v) = args.opt("--ui-size") {
        let parsed = v.split_once('x').and_then(|(w, h)| Some([w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?]));
        opts.size = parsed.ok_or_else(|| Failure::Usage(format!("--ui-size: expected WxH in points, e.g. 1680x1020, got {v}")))?;
    }
    if let Some(k) = args.num("--ui-scale")? {
        if !(0.5..=4.0).contains(&k) {
            return usage_err("--ui-scale: expected 0.5 to 4");
        }
        opts.pixels_per_point = k as f32;
    }
    // The project opens before the app starts (errors surface now, and `initialize` needn't wait
    // for the app's first frames). The app attaches the GPU compositor on its own wgpu device, as
    // the desktop app does.
    let open = match &args.project {
        Some(p) => Some(("file.open", json!({"path": p}))),
        None if args.flag("--demo") || (default_demo && !args.flag("--empty")) => Some(("file.openDemoProject", json!({}))),
        None => None,
    };
    let auto_save = autosave_prefs(args)?;
    let ui = offscreen_ui::Offscreen::start(opts, move || {
        let mut s = effectcraft_host::session();
        if let Some(a) = auto_save {
            s.prefs.auto_save = a;
            s.prefs.normalize();
        }
        if let Some((id, params)) = open {
            s.execute_checked(id, params).map_err(|e| e.to_string())?;
        }
        Ok(s)
    })?;
    note!("effectcraft mcp: offscreen UI on {}", ui.adapter());
    Ok(Backend::transport(ui))
}

#[cfg(not(feature = "ui"))]
fn offscreen_backend(_args: &Args, _default_demo: bool) -> Result<Backend, Failure> {
    Err(Failure::Error("--ui: this effectcraft-cli was built without the `ui` feature (cargo build -p effectcraft-cli --features ui)".into()))
}

/// `--autosave`: the Auto-Save settings from the config directory (defaults without a settings
/// file). Only those are read: opting in must not change rendering or load the desktop's models,
/// shortcuts, disk cache or recovery sentinel.
fn autosave_prefs(args: &Args) -> Result<Option<effectcraft_engine::prefs::AutoSave>, Failure> {
    if !args.flag("--autosave") {
        return Ok(None);
    }
    let dir = effectcraft_host::config_dir().ok_or_else(|| Failure::Error("--autosave: no config directory; set EFFECTCRAFT_CONFIG_DIR".into()))?;
    use effectcraft_engine::config::ConfigStore;
    let config = effectcraft_engine::config::DirConfig::new(dir);
    Ok(Some(match config.read(effectcraft_engine::prefs::PREFS_FILE) {
        Some(text) => effectcraft_engine::prefs::Prefs::from_json(&text).auto_save,
        None => effectcraft_engine::prefs::Prefs::default().auto_save,
    }))
}

/// A wired session; `--gpu` attaches the GPU compositor (renders then follow the project's
/// renderer setting, Mercury GPU Acceleration by default).
fn session(args: &Args) -> Result<effectcraft_engine::Session, Failure> {
    let mut s = effectcraft_host::session();
    if let Some(a) = autosave_prefs(args)? {
        s.prefs.auto_save = a;
        s.prefs.normalize();
    }
    if args.flag("--gpu") {
        let g = effectcraft_gpu::Gpu::try_headless().map_err(|e| Failure::Error(format!("--gpu: no usable GPU adapter ({e})")))?;
        s.accel = Some(std::sync::Arc::new(g));
    } else {
        s.accel_note = Some("headless: renders on the CPU unless started with --gpu".into());
    }
    Ok(s)
}

/// `--save` / `--save-as` after a mutating command.
fn maybe_save(b: &mut Backend, args: &Args) -> Result<Option<String>, Failure> {
    let r = if let Some(p) = args.opt("--save-as") {
        b.exec("file.saveAs", json!({"path": p}))?
    } else if args.flag("--save") {
        b.exec("file.save", json!({})).map_err(|e| Failure::Error(format!("{e} (use --project F to save in place, or --save-as F)")))?
    } else {
        return Ok(None);
    };
    Ok(r.get("path").and_then(Value::as_str).map(str::to_string).or_else(|| args.opt("--save-as").map(str::to_string)))
}

fn tool(b: &mut Backend, name: &str, mut a: Value) -> Result<Value, Failure> {
    // A one-shot process cannot serve retained-result handles after it exits. Keep stdout
    // complete (callers can redirect it); only the persistent MCP server pages its responses.
    let fields = a.as_object_mut().ok_or_else(|| Failure::Error("tool arguments must be an object".into()))?;
    fields.insert("full".into(), json!(true));
    match tools::run(b, name, &a)? {
        Reply::Json(v) => Ok(v),
        Reply::Image { info, .. } => Ok(info),
    }
}

/// `script FILE.jsx [project]` / `script --eval CODE`: run JavaScript against a headless session
/// (an empty project unless one is given, or `--demo`) or the running app (`--bridge`).
fn script_cmd(args: &Args, json_out: bool) -> Result<(), Failure> {
    let (code, name) = match (args.opt("--eval"), args.pos.first()) {
        (Some(c), _) => (c.to_string(), "eval".to_string()),
        (None, Some(f)) => (std::fs::read_to_string(f).map_err(|e| Failure::Error(format!("cannot read {f}: {e}")))?, f.clone()),
        (None, None) => return usage_err("script needs a .jsx/.js file or --eval CODE"),
    };
    let mut b = backend(args, false)?;
    let r = b.exec("script.run", json!({"code": code, "name": name}))?;
    let saved = maybe_save(&mut b, args)?;
    if json_out {
        let mut o = r.clone();
        if let Some(p) = &saved {
            o["saved"] = json!(p);
        }
        emit(&o, true);
    } else {
        if let Some(out) = r["output"].as_str().filter(|o| !o.is_empty()) {
            say!("{out}");
        }
        if !r["result"].is_null() && r["error"].is_null() {
            say!("{}", r["result"]);
        }
        if let Some(p) = saved {
            note!("saved {p}");
        }
    }
    if let Some(e) = r.get("error").filter(|e| !e.is_null()) {
        let pos = match (e["line"].as_u64(), e["column"].as_u64()) {
            (Some(l), Some(c)) => format!(":{l}:{c}"),
            (Some(l), None) => format!(":{l}"),
            _ => String::new(),
        };
        let msg = format!("{}{pos}: {}", e["file"].as_str().unwrap_or(&name), e["message"].as_str().unwrap_or("script error"));
        if json_out {
            std::process::exit(1);
        }
        return Err(Failure::Error(msg));
    }
    Ok(())
}

/// `replay SCRIPT.jsonl`: run a repro script against a headless session (its start line picks
/// the project) and report every step. Exits 1 when the replay fails.
fn replay_cmd(args: &Args, json_out: bool) -> Result<(), Failure> {
    use effectcraft_engine::replay;
    let Some(file) = args.pos.first() else { return usage_err("replay needs a .jsonl script") };
    if args.opt("--bridge").is_some() || args.project.is_some() || args.flag("--demo") || args.flag("--empty") {
        return usage_err("replay runs headless from the script's start line ({\"start\": \"new\" | \"demo\" | {\"project\": path}})");
    }
    let path = std::path::Path::new(file);
    let size = std::fs::metadata(path).map_err(|e| Failure::Error(format!("cannot read {file}: {e}")))?.len();
    if size > replay::MAX_SCRIPT_BYTES as u64 {
        return Err(Failure::Error(format!("{file} is larger than {} MB", replay::MAX_SCRIPT_BYTES >> 20)));
    }
    let script = std::fs::read_to_string(path).map_err(|e| Failure::Error(format!("cannot read {file}: {e}")))?;
    let mut s = session(args)?;
    let opts =
        replay::Options { keep_going: args.flag("--keep-going"), base_dir: path.parent().map(std::path::Path::to_path_buf), bless: args.flag("--bless") };
    let report = replay::run(&mut s, &script, &opts);
    if json_out {
        emit(&serde_json::to_value(&report).unwrap_or_else(|e| json!({"error": e.to_string()})), true);
    } else {
        for st in &report.steps {
            let what = st.command.as_deref().unwrap_or(st.kind);
            match (&st.error, st.ok) {
                (None, _) => say!("line {:>4}  ok    {what}", st.line),
                (Some(e), true) => say!("line {:>4}  ok    {what} (tolerated error: {e})", st.line),
                (Some(e), false) => say!("line {:>4}  FAIL  {what}: {e}", st.line),
            }
        }
    }
    let Some(first) = &report.first_failure else {
        if !json_out {
            say!("replay ok: {} steps", report.steps.len());
        }
        return Ok(());
    };
    let msg = format!(
        "failed at line {} ({}){}: {}{}",
        first.line,
        first.command.as_deref().unwrap_or(first.kind),
        if report.panicked { ", a command panicked" } else { "" },
        first.error.as_deref().unwrap_or(""),
        if report.skipped > 0 { format!(" ({} later steps not run)", report.skipped) } else { String::new() }
    );
    if json_out {
        note!("effectcraft-cli replay: {msg}");
        std::process::exit(1);
    }
    Err(Failure::Error(msg))
}

/// Print a result: compact JSON with `--json`, else pretty JSON.
fn emit(v: &Value, json_out: bool) {
    if json_out {
        say!("{v}");
    } else {
        say!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    }
}

fn run(cmd: &str, args: &Args, json_out: bool) -> Result<(), Failure> {
    if args.flag("--autosave") && cmd != "mcp" {
        return usage_err("--autosave is only supported by mcp");
    }
    if args.flag("--all-tools") && cmd != "mcp" {
        return usage_err("--all-tools is only supported by mcp");
    }
    if cmd != "mcp" {
        if let Some(o) = ["--ui", "--ui-size", "--ui-scale"].into_iter().find(|o| args.opts.iter().any(|(k, _)| k == o)) {
            return usage_err(&format!("{o} is only supported by mcp"));
        }
    } else if args.flag("--ui") {
        for (o, why) in [
            ("--bridge", "--ui runs its own app offscreen; --bridge drives a running one"),
            ("--gpu", "the offscreen UI renders on the GPU compositor of its own device when one is usable"),
        ] {
            if args.opts.iter().any(|(k, _)| k == o) {
                return usage_err(why);
            }
        }
    } else if args.opt("--ui-size").is_some() || args.opt("--ui-scale").is_some() {
        return usage_err("--ui-size and --ui-scale need --ui");
    }
    template::validate(cmd, args)?;
    match cmd {
        "render" => render(args, json_out)?,
        "figma-bridge" => {
            let port = match args.opt("--port") {
                Some(p) => p.parse::<u16>().map_err(|_| Failure::Usage(format!("--port: not a port: {p}")))?,
                None => 7789,
            };
            let control = args.opt("--control").or(args.opt("--bridge")).map_or_else(default_control_port, str::to_string);
            figma_bridge::serve(figma_bridge::Config { port, control, code: args.opt("--code").map(str::to_string) })?;
        }
        "bench" => bench_cmd(args)?,
        "compare" => compare_cmd(args, json_out)?,
        "info" => {
            let mut b = backend(args, true)?;
            let p = tool(&mut b, "get_project", json!({}))?;
            let comp = tool(&mut b, "get_comp", json!({})).ok();
            let info = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "mode": b.mode(),
                "commands": effectcraft_engine::command_specs().len(),
                "effects": effectcraft_engine::effects::registry().len(),
                "project": p,
                "activeComp": comp,
            });
            if json_out {
                emit(&info, true);
            } else {
                say!(
                    "EffectCraft {} ({}), {} commands, {} effects",
                    info["version"].as_str().unwrap_or(""),
                    info["mode"].as_str().unwrap_or(""),
                    info["commands"],
                    info["effects"]
                );
                say!("project: {}", p["path"].as_str().unwrap_or("(unsaved)"));
                for i in p["items"].as_array().into_iter().flatten() {
                    say!("  item {:>3}  {:<12} {}", i["id"], i["type"].as_str().unwrap_or(""), i["name"].as_str().unwrap_or(""));
                }
                if let Some(c) = comp {
                    say!(
                        "active comp {} \"{}\" {}x{} @ {} fps, {} s",
                        c["id"],
                        c["name"].as_str().unwrap_or(""),
                        c["width"],
                        c["height"],
                        c["frameRate"],
                        c["duration"]
                    );
                    for l in c["layers"].as_array().into_iter().flatten() {
                        say!("  #{:<3} id {:<4} {:<10} {}", l["index"], l["id"], l["type"].as_str().unwrap_or(""), l["name"].as_str().unwrap_or(""));
                    }
                }
            }
        }
        "commands" => list_commands_cmd(args, json_out)?,
        "exec" if args.flag("--list") => list_commands_cmd(args, json_out)?,
        "script" => script_cmd(args, json_out)?,
        "replay" => replay_cmd(args, json_out)?,
        "exec" => {
            let Some(id) = args.pos.first().cloned() else { return usage_err("exec needs a command id") };
            let params = match (args.opt("--params"), args.pos.get(1).map(String::as_str)) {
                (Some(p), _) | (None, Some(p)) => serde_json::from_str(p).map_err(|e| Failure::Usage(format!("params are not valid JSON: {e}")))?,
                (None, None) => json!({}),
            };
            let mut b = backend(args, true)?;
            let r = b.exec(&id, params)?;
            if let Some(e) = failed_render(&id, &r) {
                return Err(Failure::Error(e));
            }
            let saved = maybe_save(&mut b, args)?;
            emit(&with_saved(r, saved), json_out);
        }
        "run" => {
            if args.pos.is_empty() {
                return usage_err("run needs <command-id> [<json>] pairs");
            }
            let pairs = args.pos.clone();
            let mut b = backend(args, true)?;
            let mut results = vec![];
            let mut i = 0;
            while i < pairs.len() {
                let id = &pairs[i];
                let params = match pairs.get(i + 1).filter(|p| p.trim_start().starts_with('{')) {
                    Some(p) => {
                        i += 1;
                        serde_json::from_str(p).map_err(|e| Failure::Usage(format!("{id}: params are not valid JSON: {e}")))?
                    }
                    None => json!({}),
                };
                i += 1;
                let r = b.exec(id, params).map_err(|e| Failure::Error(format!("{id}: {e}")))?;
                if let Some(e) = failed_render(id, &r) {
                    return Err(Failure::Error(format!("{id}: {e}")));
                }
                if !json_out {
                    emit(&r, false);
                }
                results.push(json!({"command": id, "result": r}));
            }
            let saved = maybe_save(&mut b, args)?;
            if json_out {
                emit(&with_saved(json!(results), saved), true);
            }
        }
        "props" => {
            let [comp, layer] = positional::<2>(args, "props <comp> <layer>")?;
            let mut b = backend(args, true)?;
            let a = json!({"comp": reference(&comp), "layer": reference(&layer), "flat": args.flag("--flat") || !json_out, "time": args.num("--time")?, "depth": args.num("--depth")?});
            let v = tool(&mut b, "get_layer", a)?;
            if json_out {
                emit(&v, true);
            } else {
                say!("layer {} \"{}\" ({}) at {} s", v["id"], v["name"].as_str().unwrap_or(""), v["type"].as_str().unwrap_or(""), v["time"]);
                for p in v["properties"].as_array().into_iter().flatten() {
                    let mut extra = String::new();
                    if let Some(k) = p.get("keys") {
                        extra += &format!("  [{k} keys]");
                    }
                    if let Some(e) = p.get("expression").and_then(Value::as_str) {
                        extra += &format!("  expr: {e}");
                    }
                    say!("  {:<44} {:<8} {}{extra}", p["path"].as_str().unwrap_or(""), p["type"].as_str().unwrap_or(""), p["value"]);
                }
            }
        }
        "get" => {
            let [comp, layer, path] = positional::<3>(args, "get <comp> <layer> <path>")?;
            let mut b = backend(args, true)?;
            let v = tool(&mut b, "get_property", json!({"comp": reference(&comp), "layer": reference(&layer), "path": path, "time": args.num("--time")?}))?;
            emit(&v, json_out);
        }
        "set" => {
            let expr = args.opt("--expression").map(str::to_string);
            let need = if expr.is_some() { 3 } else { 4 };
            if args.pos.len() < need {
                return usage_err("set <comp> <layer> <path> <value> [--time S] [--expression E]");
            }
            let (comp, layer, path) = (args.pos[0].clone(), args.pos[1].clone(), args.pos[2].clone());
            let value = args.pos.get(3).map(|v| value_arg(v));
            let mut b = backend(args, true)?;
            let a =
                json!({"comp": reference(&comp), "layer": reference(&layer), "path": path, "value": value, "time": args.num("--time")?, "expression": expr});
            let v = tool(&mut b, "set_property", a)?;
            let saved = maybe_save(&mut b, args)?;
            emit(&with_saved(v, saved), json_out);
        }
        "render-frame" | "frame" => {
            let mut b = if args.opt("--template").is_some() {
                let mut s = session(args)?;
                template::prepare(&mut s, args)?;
                Backend::headless(s)
            } else {
                backend(args, true)?
            };
            let comp = args.opt("--comp").and_then(reference);
            let mut time = args.num("--time")?;
            let mut max_side = args.num("--max-side")?.map(|m| m as u32).unwrap_or(0);
            let needs_info = time.is_some() || args.opt("--frame").is_some() || args.opt("--scale").is_some();
            if needs_info {
                let c = tool(&mut b, "get_comp", json!({"comp": comp}))?;
                let fps = c["frameRate"].as_f64().filter(|r| r.is_finite() && *r > 0.0).unwrap_or(30.0);
                if let Some(f) = args.num("--frame")? {
                    time = Some(f / fps);
                }
                // A time printed in rounded seconds ("1.483333" for frame 89 at 60 fps) means that
                // frame, not the instant just before it where layers cut at the frame still show.
                time = time.map(|t| snap_to_frame(t, fps));
                if let Some(k) = args.num("--scale")? {
                    let long = c["width"].as_f64().unwrap_or(0.0).max(c["height"].as_f64().unwrap_or(0.0));
                    max_side = (long * k).round().max(1.0) as u32;
                }
            }
            let out = args.opt("--out").unwrap_or("frame.png").to_string();
            let t0 = std::time::Instant::now();
            let f = b.render_with(comp.as_ref(), time, max_side, args.flag("--transparent"))?;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            std::fs::write(&out, &f.png).map_err(|e| format!("cannot write {out}: {e}"))?;
            let mut info = json!({"path": out, "comp": f.comp, "time": f.time, "width": f.width, "height": f.height, "ms": (ms * 10.0).round() / 10.0});
            if let Some(on) = &f.rendered_on {
                info["renderedOn"] = json!(on);
            }
            if json_out {
                emit(&info, true);
            } else {
                note!("rendered {}x{} at {:.3}s in {ms:.1} ms -> {out}", f.width, f.height, f.time);
            }
        }
        "mcp" => {
            if args.flag("--autosave") && args.opt("--bridge").is_some() {
                return usage_err("--autosave is for MCP with the project in this process (headless or --ui); the bridged app owns its auto-saves");
            }
            // A project in this process (headless / --ui) is in the host list (announced before
            // `--project` opens); a bridge isn't a host.
            if args.opt("--bridge").is_none()
                && let Some(dir) = effectcraft_host::config_dir()
            {
                effectcraft_engine::hosts::announce(&dir, "mcp", None);
            }
            let mut b = backend(args, false)?;
            let (read, write) = (args.opts("--automation-read-root"), args.opts("--automation-write-root"));
            if !read.is_empty() || !write.is_empty() {
                let roots = |r: Vec<&str>| r.into_iter().map(std::path::PathBuf::from).collect::<Vec<_>>();
                b.confine(effectcraft_automation::confine::Confinement::new(&roots(read), &roots(write))?);
            }
            // --client NAME (or EFFECTCRAFT_CLIENT): every tool call runs as that agent (its own
            // selection, active comp and time; comp claims).
            let client = args.opt("--client").map(str::to_string).or_else(|| std::env::var("EFFECTCRAFT_CLIENT").ok()).filter(|c| !c.is_empty());
            if client.as_ref().is_some_and(|c| c.len() > effectcraft_engine::agents::MAX_CLIENT_LEN) {
                return usage_err("--client: the name is longer than 128 bytes");
            }
            let server = McpServer::new(b).with_all_tools(args.flag("--all-tools")).with_client(client);
            let mut server = if args.flag("--autosave") {
                let root =
                    effectcraft_host::config_dir().ok_or_else(|| Failure::Error("--autosave: no config directory; set EFFECTCRAFT_CONFIG_DIR".into()))?;
                server.with_autosave(&root)?
            } else {
                server
            };
            // A client that closes its end of stdout has ended the session.
            let served = server.serve_stdio();
            effectcraft_engine::hosts::withdraw();
            match served {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return Err(e.to_string().into()),
                _ => {}
            }
        }
        "serve" => {
            let port = match args.opt("--control") {
                Some(p) => p.parse::<u16>().map_err(|_| Failure::Usage(format!("--control: `{p}` is not a port")))?,
                None => 0,
            };
            let (project, demo) = (args.project.clone(), args.flag("--demo"));
            // One device for the host and the frame server.
            let gpu = match args.flag("--gpu") {
                true => {
                    Some(std::sync::Arc::new(effectcraft_gpu::Gpu::try_headless().map_err(|e| Failure::Error(format!("--gpu: no usable GPU adapter ({e})")))?))
                }
                false => None,
            };
            let control_port = std::sync::Arc::new(std::sync::OnceLock::new());
            let frames = match args.opt("--frames") {
                Some(p) => {
                    let p = p.parse::<u16>().map_err(|_| Failure::Usage(format!("--frames: `{p}` is not a port")))?;
                    Some(frame_serve::start(args, p, gpu.clone(), control_port.clone())?)
                }
                None => None,
            };
            // With --frames the device is the frame server's alone: it rebuilds it after a loss,
            // and a host holding the old one would keep its memory.
            let host_gpu = if frames.is_some() { None } else { gpu.clone() };
            let make = Box::new(move || -> Result<effectcraft_engine::Session, String> {
                let mut s = effectcraft_host::session();
                if let Some(g) = host_gpu {
                    s.accel = Some(g);
                }
                if let Some(p) = &project {
                    s.execute("file.open", json!({"path": p})).map_err(|e| e.to_string())?;
                } else if demo {
                    s.execute("file.openDemoProject", json!({})).map_err(|e| e.to_string())?;
                }
                Ok(s)
            });
            let follow = match args.opt("--follow") {
                Some(p) => Some(p.parse::<u16>().map_err(|_| Failure::Usage(format!("--follow: `{p}` is not a port")))?),
                None => None,
            };
            if follow.is_some() && (args.project.is_some() || args.flag("--demo")) {
                return usage_err("--follow mirrors the app's project; leave out --project / --demo");
            }
            let config = effectcraft_automation::host_server::HostConfig { port, config_dir: effectcraft_host::config_dir(), follow };
            effectcraft_automation::host_server::serve(config, make, |port| {
                let _ = control_port.set(port);
                match follow {
                    Some(app) => println!("EffectCraft render box listening on 127.0.0.1:{port}, following the app on {app} (agents: effectcraft-cli mcp --bridge {app} --render-box {port})"),
                    None => println!("EffectCraft host listening on 127.0.0.1:{port} (agents: effectcraft-cli mcp --bridge {port})"),
                }
            })
            .map_err(|e| Failure::Error(format!("serve: {e}")))?;
            if let Some(f) = frames {
                f.stop();
            }
        }
        "frames-bench" => frame_serve::bench(args)?,
        "stream-bench" => stream_cli::bench(args)?,
        "stream-test" => stream_cli::test(args)?,
        "mirror" => mirror::run(args, json_out)?,
        "drive-pull" => drive::run(args)?,
        "library-pull" => library_pull::run(args)?,
        other => return usage_err(&format!("unknown subcommand `{other}`")),
    }
    Ok(())
}

/// `compare A B`: `render.compare` on two image files in an empty headless session. Prints the
/// metrics; exits 1 when the images don't pass (golden tests and AE fidelity checks).
fn compare_cmd(args: &Args, json_out: bool) -> Result<(), Failure> {
    let [a, b] = positional::<2>(args, "compare <a.png> <b.png> [--threshold L] [--min-psnr DB] [--max-side PX] [--diff D.png]")?;
    let mut p = json!({"a": {"path": a}, "b": {"path": b}});
    for (opt, key) in [("--threshold", "threshold"), ("--min-psnr", "min_psnr"), ("--amplify", "amplify")] {
        if let Some(v) = args.num(opt)? {
            p[key] = json!(v);
        }
    }
    if let Some(m) = args.num("--max-side")? {
        p["max_side"] = json!(m.max(0.0) as u64);
    }
    if let Some(d) = args.opt("--diff") {
        p["diff_path"] = json!(d);
    }
    let mut bk = Backend::headless(session(args)?);
    let r = bk.exec("render.compare", p)?;
    let pass = r["pass"].as_bool().unwrap_or(false);
    if json_out {
        emit(&r, true);
    } else {
        say!(
            "{} {}x{}  max {} mean {} psnr {} dB ssim {} changed {}% ({} px){}",
            if pass { "PASS" } else { "FAIL" },
            r["width"],
            r["height"],
            r["max_abs"],
            r["mean_abs"],
            r["psnr_db"].as_str().map(str::to_string).unwrap_or_else(|| r["psnr_db"].to_string()),
            r["ssim"],
            r["changed_pct"],
            r["changed_pixels"],
            r["diff_path"].as_str().map(|d| format!("  diff -> {d}")).unwrap_or_default()
        );
    }
    if !pass {
        std::process::exit(1);
    }
    Ok(())
}

/// `t` (seconds) moved onto the nearest frame of a `fps` grid when it is within 1/1000 frame of it.
fn snap_to_frame(t: f64, fps: f64) -> f64 {
    let f = t * fps;
    let r = f.round();
    if (f - r).abs() < 1e-3 { r / fps } else { t }
}

fn positional<const N: usize>(args: &Args, usage: &str) -> Result<[String; N], Failure> {
    if args.pos.len() < N {
        return usage_err(usage);
    }
    Ok(std::array::from_fn(|i| args.pos[i].clone()))
}

/// `renderQueue.render` reports a failed item in its result, not as an error. Under `run`/`exec`
/// that item fails the command (#1426), as it fails `render --queue`.
fn failed_render(id: &str, r: &Value) -> Option<String> {
    if id != "renderQueue.render" {
        return None;
    }
    let failed: Vec<String> = r["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| Some(format!("{}: {}", i["compName"].as_str().unwrap_or("?"), i["status"].get("Failed")?.as_str().unwrap_or("failed"))))
        .collect();
    (!failed.is_empty()).then(|| failed.join("; "))
}

fn with_saved(v: Value, saved: Option<String>) -> Value {
    match saved {
        None => v,
        Some(p) => json!({"result": v, "saved": p}),
    }
}

/// A relative `--out` is relative to the working directory, like `--project` and every other CLI
/// path. The Render Queue alone would resolve it against the project's folder.
fn from_cwd(path: &str) -> Result<String, Failure> {
    std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).map_err(|e| Failure::Error(format!("render: cannot resolve --out {path}: {e}")))
}

/// `render`: queue `--comp` (or the active comp) with the given settings unless `--queue`, then
/// render the queue with a progress line on stderr. Fails if any item fails.
fn render(args: &Args, json_out: bool) -> Result<(), Failure> {
    use effectcraft_engine::project::render_queue::{OutputFormat, RenderStatus};
    if args.opt("--bridge").is_some() {
        return usage_err("render runs headless; use `exec renderQueue.add` / `renderQueue.render` with --bridge");
    }
    let mut s = session(args)?;
    if args.opt("--template").is_some() {
        template::prepare(&mut s, args)?;
    } else if let Some(p) = &args.project {
        s.execute("file.open", json!({"path": p})).map_err(|e| Failure::Error(e.to_string()))?;
    } else if args.flag("--demo") || !args.flag("--empty") {
        s.execute("file.openDemoProject", json!({})).map_err(|e| Failure::Error(e.to_string()))?;
    }
    let err = |e: effectcraft_engine::EngineError| Failure::Error(e.to_string());
    if !args.flag("--queue") {
        let Some(out) = args.opt("--out") else { return usage_err("render: --out FILE is required (or --queue)") };
        let out = &from_cwd(out)?;
        let mut p = json!({"output": out});
        if args.flag("--link-output") {
            p["linkOutput"] = json!(true);
        }
        // Same resolution as `render-frame`: a number is a comp id, `-` the active comp, else a name.
        if let Some(c) = args.opt("--comp").and_then(reference) {
            p["comp"] = c;
        }
        let ext = std::path::Path::new(out).extension().map(|e| e.to_string_lossy().to_string());
        if let Some(f) = args.opt("--format").map(str::to_string).or(ext) {
            p["format"] = json!(f);
        }
        match (args.num("--start")?, args.num("--end")?) {
            (None, None) => p["timeSpan"] = json!(if args.flag("--work-area") { "workArea" } else { "comp" }),
            (a, b) => {
                p["start"] = json!(a.unwrap_or(0.0));
                if let Some(b) = b {
                    p["end"] = json!(b);
                }
            }
        }
        if let Some(v) = args.num("--fps")? {
            p["frameRate"] = json!(v);
        }
        if let Some(r) = args.opt("--resolution").or(args.opt("--scale")) {
            p["resolution"] = r.parse::<f64>().map(|v| json!(v)).unwrap_or(json!(r));
        }
        for (flag, key) in [("--quality", "quality"), ("--channels", "channels"), ("--prores", "proresProfile"), ("--audio", "audio")] {
            if let Some(v) = args.opt(flag) {
                p[key] = json!(v);
            }
        }
        if let Some(v) = args.num("--jpeg-quality")? {
            p["quality"] = json!(v);
        }
        if let Some(v) = args.num("--bitrate")? {
            p["bitrate"] = json!(v);
            // VP9 WebM encodes by quality unless told to target the bitrate (#440).
            // The format parser decides what is WebM, so its aliases (`vp9`, `webmvp9opus`) count.
            if p["format"].as_str().and_then(OutputFormat::from_name) == Some(OutputFormat::WebM) {
                p["webmBitrate"] = json!(true);
            }
        }
        for (flag, key) in [
            ("--profile", "profile"),
            ("--level", "level"),
            ("--rate-control", "rateControl"),
            ("--webm-codec", "webmCodec"),
            ("--opus-app", "opusApplication"),
        ] {
            if let Some(v) = args.opt(flag) {
                p[key] = json!(v);
            }
        }
        for (flag, key) in [("--video-quality", "quality"), ("--keyint", "keyframeInterval"), ("--audio-bitrate", "audioBitrate")] {
            if let Some(v) = args.num(flag)? {
                p[key] = json!(v);
            }
        }
        // Render exactly this item: unqueue whatever the project's queue already holds.
        for k in 0..s.project.render_queue.len() {
            let _ = s.execute("renderQueue.setRender", json!({"index": k + 1, "render": false}));
        }
        let r = s.execute("renderQueue.add", p).map_err(err)?;
        note!(
            "{} → {} ({}×{}, {} frames, {})",
            r["compName"].as_str().unwrap_or("?"),
            r["outputPath"].as_str().unwrap_or("?"),
            r["width"],
            r["height"],
            r["frames"],
            r["outputModuleSummary"].as_str().unwrap_or("")
        );
    }
    s.execute("renderQueue.render", json!({"wait": false})).map_err(err)?;
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    while s.is_rendering() {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if tty && let Some(p) = s.render_progress() {
            let left = p.remaining.map(|r| format!(", ~{r:.1}s left")).unwrap_or_default();
            let _ = write!(
                std::io::stderr(),
                "
  [{}/{}] frame {}/{}  {:.1}s{left}\x1b[K",
                (p.items_done + 1).min(p.items_total),
                p.items_total,
                p.done,
                p.total,
                p.item_elapsed
            );
        }
        s.poll_render();
    }
    s.poll_render();
    if tty {
        note!();
    }
    let mut results = vec![];
    let mut failed = vec![];
    for it in s.project.render_queue.iter().filter(|i| i.render && i.started.is_some()) {
        let name = s.project.item(it.comp).map(|i| i.name.clone()).unwrap_or_else(|| "?".into());
        match &it.status {
            RenderStatus::Done => {
                note!("done: {name} → {} in {:.2}s", it.last_output.as_deref().unwrap_or("?"), it.render_time.unwrap_or(0.0));
                results.push(json!({"comp": name, "output": it.last_output, "seconds": it.render_time}));
            }
            RenderStatus::Failed(e) => failed.push(format!("{name}: {e}")),
            other => failed.push(format!("{name}: {}", other.label())),
        }
    }
    if !failed.is_empty() {
        return Err(Failure::Error(failed.join("; ")));
    }
    if json_out {
        emit(&json!({"rendered": results}), true);
    }
    Ok(())
}

/// `commands` / `exec --list`: every engine command (id, label, shortcut, params doc; with
/// `--schemas` also each command's params JSON Schema).
fn list_commands_cmd(args: &Args, json_out: bool) -> Result<(), Failure> {
    let mut b = backend(args, false)?;
    let v = tool(&mut b, "list_commands", json!({"filter": args.opt("--filter"), "enabled_only": args.flag("--enabled"), "schemas": args.flag("--schemas")}))?;
    if json_out {
        emit(&v, true);
    } else {
        for c in v.as_array().into_iter().flatten() {
            say!(
                "{:<36} {:<36} {:<16} {}",
                c["id"].as_str().unwrap_or(""),
                c["label"].as_str().unwrap_or(""),
                c["shortcut"].as_str().unwrap_or(""),
                c["params"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

/// `bench`: time one frame N times without the layer cache (per-layer and per-effect breakdown),
/// and optionally `--play N` consecutive frames with and without the cache.
#[cfg(feature = "profile")]
mod rt_bench;

fn bench_cmd(args: &Args) -> Result<(), Failure> {
    if args.flag("--ops") {
        return bench_ops(args);
    }
    let mut s = effectcraft_host::session();
    match &args.project {
        Some(p) => s.execute("file.open", json!({"path": p})),
        None => s.execute("file.openDemoProject", json!({})),
    }
    .map_err(|e| Failure::Error(e.to_string()))?;
    if let Some(c) = args.opt("--comp") {
        s.execute("comp.open", json!({"comp": c})).map_err(|e| Failure::Error(e.to_string()))?;
    }
    // Preview reads of ProRes footage decode on the hardware decoder (as the app's default).
    if args.flag("--hw-prores") {
        s.footage.set_hardware_decoding(true);
    }
    if args.flag("--rt-profile") {
        #[cfg(feature = "profile")]
        return rt_bench::run(&s, args);
        #[cfg(not(feature = "profile"))]
        return Err(Failure::Error("--rt-profile requires a CLI built with --features profile".into()));
    }
    if args.flag("--gpu") {
        return bench_gpu(&s, args);
    }
    let cid = s.active_comp_id().ok_or_else(|| Failure::Error("no composition".into()))?;
    let t = Tick::from_seconds_f64(args.num("--time")?.unwrap_or(3.0));
    let opts = RenderOpts { scale: args.num("--scale")?.unwrap_or(1.0), ..Default::default() };
    bench(&s, cid, t, opts, args.num("--n")?.unwrap_or(10.0).max(1.0) as usize);
    if let Some(n) = args.num("--play")? {
        bench_play(&s, cid, t, opts, n.max(1.0) as usize);
    }
    Ok(())
}

/// `bench --ops`: everyday operations on a large generated project (200 comps, 5,000 layers in
/// the main comp, 300 footage items, 20-deep precomp nesting, expressions): startup to first
/// frame, open, save, auto-save, small and large edits with undo/redo, first frame, timeline
/// scrolling and the Project panel. `--small` for a quick run; `--layers N`, `--comps N`,
/// `--footage N` resize it. See docs/architecture.md ▸ Performance.
fn bench_ops(args: &Args) -> Result<(), Failure> {
    use effectcraft_engine::perf::{self, LargeSpec};
    let mut spec = if args.flag("--small") { LargeSpec::small() } else { LargeSpec::default() };
    if let Some(n) = args.num("--layers")? {
        spec.main_layers = n.max(1.0) as usize;
    }
    if let Some(n) = args.num("--comps")? {
        spec.comps = n.max(1.0) as usize;
    }
    if let Some(n) = args.num("--footage")? {
        spec.footage = n.max(0.0) as usize;
    }
    let dir = std::env::temp_dir().join(format!("effectcraft-bench-ops-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| Failure::Error(format!("{}: {e}", dir.display())))?;
    let t0 = std::time::Instant::now();
    drop(effectcraft_host::session());
    let host_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let (mut m, _) = perf::ops_bench(&spec, &dir, &effectcraft_host::session);
    m.insert(0, perf::Measure { name: "hostSession", ms: host_ms, note: "a fully wired session (media, expressions, scripting, export)".into() });
    let path = dir.join("bench-large.ecproj").to_string_lossy().to_string();
    m.extend(effectcraft_ui_egui::bench::ui_ops(|| {
        let mut s = effectcraft_host::session();
        if let Err(e) = s.execute("file.open", json!({"path": path})) {
            note!("bench --ops: {e}");
        }
        s
    }));
    let _ = std::fs::remove_dir_all(&dir);
    if args.flag("--json") {
        emit(&json!({"spec": format!("{spec:?}"), "measures": m.iter().map(perf::Measure::json).collect::<Vec<_>>()}), true);
    } else {
        say!("bench --ops: {} comps, {} layers in Main, {} footage items", spec.comps, spec.main_layers, spec.footage);
        for x in &m {
            say!("  {:<22} {:>10.2} ms  {}", x.name, x.ms, x.note);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- benchmark helpers

use effectcraft_engine::Session;
use effectcraft_engine::project::ItemId;
use effectcraft_render::{LayerCache, LayerTiming, RenderOpts, Renderer};
use effectcraft_time::Tick;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

fn min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

/// CPU time of the whole process (all threads) in ms: a load-independent measure of work.
fn cpu_ms() -> f64 {
    cpu_time::ProcessTime::try_now().map(|t| t.as_duration().as_secs_f64() * 1e3).unwrap_or(0.0)
}

/// One frame: (wall ms, CPU ms, per-layer timings).
fn render_timed(s: &Session, cid: ItemId, t: Tick, opts: RenderOpts, cache: Option<&LayerCache>) -> (f64, f64, Vec<LayerTiming>) {
    let prof = std::sync::Mutex::new(Vec::<LayerTiming>::new());
    let mut r = Renderer::new(&s.project, s.footage.as_ref(), opts);
    r.expr = s.expr.as_deref();
    r.cache = cache;
    r.profile = Some(&prof);
    let c0 = cpu_ms();
    let t0 = std::time::Instant::now();
    let img = r.comp_frame(cid, t);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let cpu = cpu_ms() - c0;
    std::hint::black_box(&img);
    (ms, cpu, prof.into_inner().unwrap_or_default())
}

type LayerRow = (usize, String, Vec<f64>, Vec<f64>, Vec<(String, Vec<f64>)>);

/// `frame --bench N`: render the same frame N times without the layer cache (the full cost of a
/// frame) and print min/median frame time plus min per-layer and per-effect times.
fn bench(s: &Session, cid: ItemId, t: Tick, opts: RenderOpts, n: usize) {
    let mut totals = Vec::with_capacity(n);
    let mut cpus = Vec::with_capacity(n);
    let mut per_layer: Vec<LayerRow> = Vec::new();
    for _ in 0..n {
        let (ms, cpu, prof) = render_timed(s, cid, t, opts, None);
        totals.push(ms);
        cpus.push(cpu);
        for (i, lt) in prof.into_iter().enumerate() {
            if per_layer.len() <= i {
                per_layer.push((lt.depth, lt.layer.clone(), vec![], vec![], vec![]));
            }
            let e = &mut per_layer[i];
            e.2.push(lt.process_ms);
            e.3.push(lt.composite_ms);
            for (j, (id, ms)) in lt.effects.into_iter().enumerate() {
                if e.4.len() <= j {
                    e.4.push((id, vec![]));
                }
                e.4[j].1.push(ms);
            }
        }
    }
    note!(
        "bench (no cache): {n} runs at scale {}: wall min {:.2} ms, median {:.2} ms; CPU (all threads) min {:.2} ms",
        opts.scale,
        min(&totals),
        median(&mut totals),
        min(&cpus)
    );
    for (d, name, p, c, fx) in per_layer {
        note!("  {:indent$}{name:<28} process {:8.2} ms  composite {:8.2} ms  (min)", "", min(&p), min(&c), indent = d * 2);
        for (id, v) in fx {
            note!("  {:indent$}  fx {id:<30} {:8.2} ms", "", min(&v), indent = d * 2);
        }
    }
}

/// `frame --bench-play N`: render N consecutive frames from `t` as playback does, without and
/// with the layer cache, and print per-frame times.
fn bench_play(s: &Session, cid: ItemId, t: Tick, opts: RenderOpts, n: usize) {
    let Some(comp) = s.project.comp(cid) else { return };
    let f0 = comp.frame_rate.frame_at(t);
    let cache = LayerCache::default();
    for (label, c) in [("no cache", None), ("layer cache", Some(&cache))] {
        let runs: Vec<(f64, f64)> = (0..n as i64)
            .map(|i| {
                let (ms, cpu, _) = render_timed(s, cid, comp.frame_rate.tick_of(f0 + i), opts, c);
                (ms, cpu)
            })
            .collect();
        let mut v: Vec<f64> = runs.iter().map(|r| r.0).collect();
        let mean = v.iter().sum::<f64>() / n as f64;
        let cpu = runs.iter().map(|r| r.1).sum::<f64>() / n as f64;
        note!(
            "bench-play ({label}): {n} frames at scale {}: wall mean {mean:.2} ms, median {:.2} ms, min {:.2} ms; CPU mean {cpu:.2} ms",
            opts.scale,
            median(&mut v),
            min(&v)
        );
    }
    let st = cache.stats();
    note!("  layer cache: {} hits, {} misses, {} entries, {:.1} MB", st.hits, st.misses, st.entries, st.bytes as f64 / 1e6);
}

/// Median wall time (ms) of `n` runs of `f` after one warm-up run.
fn time_ms(n: usize, mut f: impl FnMut()) -> f64 {
    f();
    let mut v: Vec<f64> = (0..n)
        .map(|_| {
            let t0 = std::time::Instant::now();
            f();
            t0.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    median(&mut v)
}

/// `bench --gpu`'s adjustment-layer comp: a copy of the first comp with a full-frame adjustment
/// layer on top running Gaussian Blur, Levels, Vibrance and Glow (all GPU effects).
fn adjustment_bench_comp(p: &mut effectcraft_engine::project::Project) -> Option<ItemId> {
    use effectcraft_engine::effects;
    use effectcraft_engine::keyframe::Value;
    use effectcraft_engine::project::build::{self, Ids};
    use effectcraft_engine::project::{ItemKind, LayerSource, Solid};
    let (_, base) = p.comps().next().map(|(id, c)| (*id, c.clone()))?;
    let mut comp = base.clone();
    let (w, h) = (comp.width, comp.height);
    let sid = p.add_item(
        "Adjustment (bench)",
        effectcraft_engine::color::Label::None,
        None,
        ItemKind::Solid(Solid { color: [1.0; 3], width: w, height: h, pixel_aspect: 1.0 }),
    );
    let mut l = build::layer(p, &comp, "Adjustment (bench)", LayerSource::Solid { item: sid }, (w, h), None);
    l.switches.adjustment = true;
    for (id, vals) in [
        ("ec.blur.gaussian", vec![("blurriness", Value::Scalar(12.0))]),
        ("ec.color.levels", vec![("gamma", Value::Scalar(1.3))]),
        ("ec.color.vibrance", vec![("vibrance", Value::Scalar(40.0))]),
        ("ec.stylize.glow", vec![("threshold", Value::Scalar(55.0))]),
    ] {
        let spec = effects::find(id)?;
        let mut next = p.next_id;
        let mut g = effects::instantiate(spec, &mut Ids(&mut next), spec.name, [w as f64, h as f64]);
        p.next_id = next;
        for (k, v) in vals {
            g.prop_mut(k)?.value = v;
        }
        l.props.sub_mut("effects")?.children.push(g.into());
    }
    comp.layers.insert(0, l);
    Some(p.add_item("Adjustment Layers (bench)", effectcraft_engine::color::Label::None, None, ItemKind::Comp(comp.into())))
}

/// `bench --gpu`'s Advanced 3D comp (1920×1080): a ground plane, PBR primitives, extruded
/// bevelled text, a shadow-casting spot light, a point light and an environment light, seen
/// through a depth-of-field camera (hexagonal iris, highlights) with a motion-blurred sphere.
fn adv3d_bench_comp(p: &mut effectcraft_engine::project::Project) -> Option<ItemId> {
    use effectcraft_engine::keyframe::{Keyframe, TextDoc, Value};
    use effectcraft_engine::project::build::{self, Ids};
    use effectcraft_engine::project::{Comp, ItemKind, Layer, LayerSource, LightKind, PrimitiveKind, Renderer as R3, Solid};
    let (w, h) = (1920u32, 1080u32);
    let mut comp = Comp::new(w, h, effectcraft_time::FrameRate::FPS_30, Tick::from_seconds_f64(10.0));
    comp.renderer = R3::Advanced3D;
    comp.enable_motion_blur = true;
    comp.motion_blur_samples = 8;
    let mut layers: Vec<Layer> = vec![];
    let mut add = |p: &mut effectcraft_engine::project::Project, comp: &Comp, src: LayerSource, edit: &dyn Fn(&mut Layer)| {
        let mut l = build::layer(p, comp, "L", src, (w, h), None);
        edit(&mut l);
        layers.insert(0, l);
    };
    fn set(l: &mut Layer, path: &str, v: Value) {
        if let Some(pr) = l.props.prop_mut(path) {
            pr.value = v;
        }
    }
    add(p, &comp, LayerSource::Primitive { kind: PrimitiveKind::Plane }, &|l| {
        set(l, "geometryOptions/width", Value::Scalar(6000.0));
        set(l, "geometryOptions/height", Value::Scalar(6000.0));
        set(l, "transform/position", Value::Vec3([960.0, 800.0, 0.0]));
        set(l, "transform/rotationX", Value::Scalar(90.0));
    });
    for (i, kind) in [PrimitiveKind::Sphere, PrimitiveKind::Torus, PrimitiveKind::Cube, PrimitiveKind::Sphere].into_iter().enumerate() {
        add(p, &comp, LayerSource::Primitive { kind }, &|l| {
            let x = 400.0 + 380.0 * i as f64;
            set(l, "geometryOptions/radius", Value::Scalar(140.0));
            set(l, "geometryOptions/tubeRadius", Value::Scalar(45.0));
            for k in ["width", "height", "depth"] {
                set(l, &format!("geometryOptions/{k}"), Value::Scalar(220.0));
            }
            set(l, "transform/position", Value::Vec3([x, 640.0, 300.0 * i as f64 - 300.0]));
            set(l, "transform/rotationX", Value::Scalar(35.0));
            set(l, "transform/rotationY", Value::Scalar(25.0));
            set(l, "materialOptions/metallic", Value::Scalar(if i % 2 == 1 { 100.0 } else { 0.0 }));
            set(l, "materialOptions/roughness", Value::Scalar(30.0));
            set(l, "materialOptions/castsShadows", Value::Enum(1));
            if i == 0 {
                l.switches.motion_blur = true;
                if let Some(pr) = l.props.prop_mut("transform/position") {
                    pr.keys = vec![
                        Keyframe::new(Tick::ZERO, Value::Vec3([x, 640.0, -300.0])),
                        Keyframe::new(Tick::from_seconds_f64(10.0), Value::Vec3([x + 300.0 * 60.0, 640.0, -300.0])),
                    ];
                }
            }
        });
    }
    add(p, &comp, LayerSource::Text, &|l| {
        l.switches.three_d = true;
        let doc = TextDoc { text: "EffectCraft".into(), size: 180.0, fill: [0.95, 0.75, 0.2, 1.0], apply_fill: true, ..Default::default() };
        set(l, "text/sourceText", Value::Text(Box::new(doc)));
        let mut next = 900_000u64;
        l.props.children.push(build::extrusion_geometry_options(&mut Ids(&mut next)).into());
        set(l, "geometryOptions/extrusionDepth", Value::Scalar(40.0));
        set(l, "geometryOptions/bevelStyle", Value::Enum(1));
        set(l, "transform/position", Value::Vec3([960.0, 260.0, 200.0]));
        set(l, "transform/rotationY", Value::Scalar(-15.0));
    });
    let env = p.add_item(
        "Environment (bench)",
        effectcraft_engine::color::Label::None,
        None,
        ItemKind::Solid(Solid { color: [0.55, 0.65, 0.9], width: 64, height: 32, pixel_aspect: 1.0 }),
    );
    add(p, &comp, LayerSource::Solid { item: env }, &|l| {
        l.environment = true;
        l.switches.three_d = true;
    });
    add(p, &comp, LayerSource::Light { kind: LightKind::Environment }, &|l| set(l, "lightOptions/intensity", Value::Scalar(40.0)));
    add(p, &comp, LayerSource::Light { kind: LightKind::Spot }, &|l| {
        set(l, "transform/position", Value::Vec3([500.0, -600.0, -900.0]));
        set(l, "transform/poi", Value::Vec3([960.0, 640.0, 0.0]));
        set(l, "lightOptions/castsShadows", Value::Bool(true));
        set(l, "lightOptions/coneAngle", Value::Scalar(100.0));
        set(l, "lightOptions/shadowDiffusion", Value::Scalar(8.0));
    });
    add(p, &comp, LayerSource::Light { kind: LightKind::Point }, &|l| {
        set(l, "transform/position", Value::Vec3([1500.0, 200.0, -500.0]));
        set(l, "lightOptions/intensity", Value::Scalar(50.0));
    });
    add(p, &comp, LayerSource::Camera, &|l| {
        let zoom = match l.props.prop_mut("cameraOptions/zoom").map(|pr| pr.value.clone()) {
            Some(Value::Scalar(z)) => z,
            _ => 2666.7,
        };
        set(l, "cameraOptions/dof", Value::Bool(true));
        set(l, "cameraOptions/focusDistance", Value::Scalar(zoom));
        set(l, "cameraOptions/aperture", Value::Scalar(120.0));
        set(l, "cameraOptions/irisShape", Value::Enum(4));
        set(l, "cameraOptions/highlightGain", Value::Scalar(20.0));
        set(l, "cameraOptions/highlightThreshold", Value::Scalar(200.0));
    });
    comp.layers = layers.into();
    Some(p.add_item("Advanced 3D (bench)", effectcraft_engine::color::Label::None, None, ItemKind::Comp(comp.into())))
}

/// `bench --gpu`: CPU vs GPU ms/frame for every comp (or `--comp`) at Full and Half. "cold"
/// renders everything (no layer cache); "warm" reuses the layer cache (playback / scrubbing of
/// unchanged layers: compositing cost); "viewer" is the GPU display path without readback;
/// "auto" is Backend::Auto (Mercury GPU Acceleration) choosing per comp from measured times,
/// warm; "up/dn MB" are the GPU's uploads / readbacks per warm GPU frame.
fn bench_gpu(s: &Session, args: &Args) -> Result<(), Failure> {
    use effectcraft_render::Backend;
    let gpu = effectcraft_gpu::Gpu::try_headless().map_err(|e| Failure::Error(format!("--gpu: no usable GPU adapter ({e})")))?;
    let n = args.num("--n")?.unwrap_or(10.0).max(1.0) as usize;
    // Every comp, plus an adjustment-layer comp built for the benchmark (the main comp under a
    // full-frame adjustment layer with a GPU effect stack) and an Advanced 3D comp (`--adv3d`:
    // that one only).
    let mut project = (*s.project).clone();
    project.settings.gpu_acceleration = true;
    let comps: Vec<ItemId> = match args.opt("--comp") {
        _ if args.flag("--adv3d") => adv3d_bench_comp(&mut project).into_iter().collect(),
        Some(_) => s.active_comp_id().into_iter().collect(),
        None => {
            let mut v: Vec<ItemId> = s.project.comps().map(|(id, _)| *id).collect();
            v.extend(adjustment_bench_comp(&mut project));
            v.extend(adv3d_bench_comp(&mut project));
            v
        }
    };
    let project = &project;
    note!("GPU: {} — median of {n} runs (ms/frame)", effectcraft_render::Accelerator::name(&gpu));
    note!(
        "{:<28} {:>5} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>9} {:>9} {:>11}",
        "comp",
        "res",
        "size",
        "cpu cold",
        "gpu cold",
        "cpu warm",
        "gpu warm",
        "gpu view",
        "speedup",
        "auto",
        "gpu≠cpu",
        "up/dn MB"
    );
    for cid in comps {
        let Some(comp) = project.comp(cid) else { continue };
        let name = project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
        let t = Tick::from_seconds_f64(args.num("--time")?.unwrap_or(3.0).min(comp.duration.seconds() * 0.5));
        for (label, scale) in [("Full", 1.0), ("Half", 0.5)] {
            let mk = |backend: Backend, cache: Option<&LayerCache>| {
                let mut r = Renderer::new(project, s.footage.as_ref(), RenderOpts { scale, backend, ..Default::default() });
                r.expr = s.expr.as_deref();
                r.cache = cache;
                r.accel = Some(&gpu);
                r.comp_frame(cid, t)
            };
            let cpu_cold = time_ms(n, || {
                std::hint::black_box(mk(Backend::Cpu, None));
            });
            let gpu_cold = time_ms(n, || {
                std::hint::black_box(mk(Backend::Gpu, None));
            });
            let cache = LayerCache::default();
            let cpu_warm = time_ms(n, || {
                std::hint::black_box(mk(Backend::Cpu, Some(&cache)));
            });
            let gcache = LayerCache::default();
            // Traffic in steady state: after the frame that fills the layer cache.
            std::hint::black_box(mk(Backend::Gpu, Some(&gcache)));
            let t0 = gpu.context().transfer_stats();
            let gpu_warm = time_ms(n, || {
                std::hint::black_box(mk(Backend::Gpu, Some(&gcache)));
            });
            let t1 = gpu.context().transfer_stats();
            let per = |b: u64| b as f64 / (n + 1) as f64 / (1u64 << 20) as f64;
            let traffic = format!("{:.1}/{:.1}", per(t1.upload_bytes - t0.upload_bytes), per(t1.readback_bytes - t0.readback_bytes));
            // Auto: warm-up frames on both sides, then the measured choice.
            for _ in 0..2 * effectcraft_render::AutoPick::WARMUP {
                std::hint::black_box(mk(Backend::Auto, Some(&gcache)));
            }
            let auto = time_ms(n, || {
                std::hint::black_box(mk(Backend::Auto, Some(&gcache)));
            });
            let view = time_ms(n, || {
                let mut r = Renderer::new(project, s.footage.as_ref(), RenderOpts { scale, backend: Backend::Gpu, ..Default::default() });
                r.expr = s.expr.as_deref();
                r.cache = Some(&gcache);
                r.accel = Some(&gpu);
                std::hint::black_box(gpu.render_display(&r, cid, t));
                gpu.wait();
            });
            let size = format!("{}x{}", (comp.width as f64 * scale).round(), (comp.height as f64 * scale).round());
            // Agreement: share of pixels where the GPU frame differs from the CPU reference by
            // more than 1/255 on any channel.
            let (a, b) = (mk(Backend::Cpu, Some(&cache)), mk(Backend::Gpu, Some(&gcache)));
            let off = a
                .data
                .iter()
                .zip(&b.data)
                .filter(|(p, q)| {
                    (0..4).any(|c| {
                        let d = (p[c] - q[c]).abs();
                        d.is_nan() || d > 1.0 / 255.0 + 1e-6
                    })
                })
                .count();
            let pct = 100.0 * off as f64 / a.data.len().max(1) as f64;
            let speedup = cpu_warm / gpu_warm.max(1e-9);
            note!(
                "{name:<28} {label:>5} {size:>10} {cpu_cold:>9.2} {gpu_cold:>9.2} {cpu_warm:>9.2} {gpu_warm:>9.2} {view:>9.2} {speedup:>7.2}x {auto:>9.2} {pct:>8.3}% {traffic:>11}"
            );
            // `--layers`: per-layer GPU times of a frame with an empty layer cache (after an edit),
            // twice: the first pass also pays first-use costs (pipelines, pools), the second not.
            if args.flag("--layers") {
                for pass in ["first", "again"] {
                    let times = effectcraft_render::LayerTimes::default();
                    let mut r = Renderer::new(project, s.footage.as_ref(), RenderOpts { scale, backend: Backend::Gpu, ..Default::default() });
                    r.expr = s.expr.as_deref();
                    // A fresh cache: static layers take their cached-layer path, rendered now.
                    let fresh = LayerCache::default();
                    r.cache = Some(&fresh);
                    r.accel = Some(&gpu);
                    r.times = Some(&times);
                    let t0 = std::time::Instant::now();
                    std::hint::black_box(gpu.render_display(&r, cid, t));
                    gpu.wait();
                    note!("  layers, empty cache ({pass}): {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1e3);
                    let mut rows: Vec<(f64, String)> = times
                        .into_map()
                        .into_iter()
                        .map(|((c, l), ms)| {
                            let lname = project.comp(c).and_then(|cc| cc.layers.iter().find(|x| x.id == l)).map(|x| x.name.clone()).unwrap_or_default();
                            (ms, lname)
                        })
                        .collect();
                    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
                    for (ms, lname) in rows.iter().take(8) {
                        note!("    {lname:<28} {ms:8.2} ms");
                    }
                }
                // Playing: the frames after `t` with one cache kept between them (what playback
                // pays per frame once a layer is cached): mean per layer over 8 frames.
                let playing = LayerCache::default();
                let step = comp.frame_rate.frame_duration();
                let frames = 8;
                let mut sum = std::collections::HashMap::new();
                let mut total = 0.0;
                for i in 0..=frames {
                    let times = effectcraft_render::LayerTimes::default();
                    let mut r = Renderer::new(project, s.footage.as_ref(), RenderOpts { scale, backend: Backend::Gpu, ..Default::default() });
                    r.expr = s.expr.as_deref();
                    r.cache = Some(&playing);
                    r.accel = Some(&gpu);
                    r.times = Some(&times);
                    let at = Tick(t.0.saturating_add(step.0.saturating_mul(i)));
                    let t0 = std::time::Instant::now();
                    std::hint::black_box(gpu.render_display(&r, cid, at));
                    gpu.wait();
                    // Frame 0 fills the cache.
                    if i > 0 {
                        total += t0.elapsed().as_secs_f64() * 1e3;
                        for (k, ms) in times.into_map() {
                            *sum.entry(k).or_insert(0.0) += ms;
                        }
                    }
                }
                note!("  layers, playing ({frames} frames after t, warm cache): {:.2} ms/frame", total / frames as f64);
                let mut rows: Vec<(f64, String)> = sum
                    .into_iter()
                    .map(|((c, l), ms)| {
                        let lname = project.comp(c).and_then(|cc| cc.layers.iter().find(|x| x.id == l)).map(|x| x.name.clone()).unwrap_or_default();
                        (ms / frames as f64, lname)
                    })
                    .collect();
                rows.sort_by(|a, b| b.0.total_cmp(&a.0));
                for (ms, lname) in rows.iter().take(8) {
                    note!("    {lname:<28} {ms:8.2} ms");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_tool_keeps_complete_output_after_process_exit() {
        let mut b = Backend::headless(effectcraft_engine::Session::default());
        b.exec("comp.new", json!({"name":"Large"})).unwrap();
        for _ in 0..110 {
            b.exec("layer.newSolid", json!({})).unwrap();
        }
        let read = tool(&mut b, "get_comp", json!({})).ok().expect("CLI read");
        assert_eq!(read["layers"].as_array().unwrap().len(), 110);
        assert!(read.get("result_id").is_none());
        assert!(read.get("next_offset").is_none());
    }

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|a| a.to_string()).collect())
    }

    /// Unknown options are usage errors naming them, not ignored (#168).
    #[test]
    fn unknown_options_are_refused() {
        let err = |args: &[&str]| parse(args).err().unwrap();
        assert_eq!(err(&["exec", "comp.new", "--bogus", "--empty", "--saveas", "z.ecproj"]), "unknown options `--bogus`, `--saveas`");
        assert_eq!(err(&["render", "--bogus=1"]), "unknown option `--bogus`");
        assert_eq!(err(&["info", "--json=yes"]), "--json takes no value");
        let a = parse(&["render", "t.ecproj", "--comp=Main", "--work-area", "--out", "x.mp4", "--json"]).unwrap();
        assert_eq!((a.project.as_deref(), a.opt("--comp"), a.opt("--out")), (Some("t.ecproj"), Some("Main"), Some("x.mp4")));
        assert!(a.flag("--work-area") && a.flag("--json"));
    }

    /// `--bridge` without a port uses the app's control port (`EFFECTCRAFT_CONTROL_PORT`, else 9877).
    #[test]
    fn bridge_without_a_port_uses_the_default_control_port() {
        let a = parse(&["mcp", "--bridge", "9000"]).unwrap();
        assert_eq!(a.opt("--bridge"), Some("9000"));
        let a = parse(&["mcp", "--bridge"]).unwrap();
        assert_eq!(a.opt("--bridge"), Some(default_control_port().as_str()));
        let a = parse(&["info", "--bridge", "--json"]).unwrap();
        assert!(a.opt("--bridge").is_some() && a.flag("--json"));
        assert_eq!(control_port_or_default(Some("9901".into())), "9901");
        assert_eq!(control_port_or_default(Some("nope".into())), "9877");
        assert_eq!(control_port_or_default(None), "9877");
    }

    /// Rounded seconds name the frame they print (SceneYard Typo: 1.483333 s = frame 89 at 60 fps,
    /// where the first scene's layers end); other times are kept.
    #[test]
    fn rounded_render_times_snap_to_their_frame() {
        assert_eq!(snap_to_frame(1.483333, 60.0), 89.0 / 60.0);
        assert_eq!(snap_to_frame(0.813559, 29.5), 24.0 / 29.5);
        assert_eq!(snap_to_frame(1.49, 60.0), 1.49);
    }

    /// `std::env::args` panicked on an argument that is not Unicode; it is a usage error naming
    /// it, and Unicode arguments pass through unchanged.
    #[cfg(unix)]
    #[test]
    fn non_unicode_arguments_are_refused() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let args = [OsString::from("render-frame"), OsString::from("--out"), OsString::from_vec(b"caf\xe9.png".to_vec())];
        assert_eq!(utf8_args(args).unwrap_err(), "argument `caf\u{fffd}.png` is not valid Unicode");
        assert_eq!(utf8_args([OsString::from("info"), OsString::from("caf\u{e9}.png")]).unwrap(), ["info", "caf\u{e9}.png"]);
    }

    /// Every `--option` the CLI reads is declared in VALUED or FLAGS, so none is refused.
    #[test]
    fn every_option_read_is_declared() {
        let code = include_str!("main.rs").split("#[cfg(test)]").next().unwrap();
        for rest in code.split("\"--").skip(1) {
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
            let opt = format!("--{name}");
            // `--help` and `--version` are handled before parsing.
            if name.is_empty() || !rest[name.len()..].starts_with('"') || ["--help", "--version"].contains(&opt.as_str()) {
                continue;
            }
            assert!(VALUED.contains(&opt.as_str()) || FLAGS.contains(&opt.as_str()), "{opt} is read but not declared");
        }
    }
}
