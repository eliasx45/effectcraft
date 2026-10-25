//! Which EffectCraft processes have which project open, and the save guard.
//!
//! **Host list.** Every process that holds projects (the desktop app, a headless MCP server)
//! announces itself with a small file `<config dir>/Hosts/<pid>.json`: its kind, its control port
//! (when agents can join it) and the project it has open. `hosts.list` shows the live ones, so an
//! agent finds where a project is open and joins it (`--bridge <port>`) instead of opening a copy;
//! opening a project another host has open warns. Files of processes that are gone are ignored and
//! removed when listed.
//!
//! **Save guard.** Two copies of one project file overwrite each other: whoever saves last wins
//! and the other's work is lost without a word. So each process remembers the file's size and
//! modification time when it opens or saves it, and a later save over that file is refused when
//! the file changed on disk meanwhile (another app, an agent, a script), unless `force`.
//!
//! Everything here is best effort: a missing or unreadable folder never fails an open or a save
//! (only a changed file refuses a save).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::{EngineError, Result};

/// The folder under the config directory that holds the host files.
pub const HOSTS_FOLDER: &str = "Hosts";
/// Host files read at most (a full folder never stalls an open).
const MAX_HOSTS: usize = 256;
/// A host file without a port, from a process whose liveness can't be checked, counts as gone
/// after this long.
const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);

/// This process's entry.
struct Me {
    dir: PathBuf,
    kind: String,
    port: Option<u16>,
    project: Option<String>,
    announced: bool,
}

static ME: Mutex<Option<Me>> = Mutex::new(None);
/// Size and modification time of each project file this process opened or saved, by path.
static SEEN: Mutex<BTreeMap<String, Stamp>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn stamp(path: &str) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    Some(Stamp { len: m.len(), modified: m.modified().ok() })
}

/// The path hosts compare (absolute when the file exists, so `a.ecproj` and `/x/a.ecproj` match).
fn key(path: &str) -> String {
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

/// Read the host list from `dir` (`<config dir>`) from now on, without announcing this process
/// (a one-shot CLI command).
pub fn init(config_dir: &Path) {
    let mut me = lock(&ME);
    if me.is_none() {
        *me = Some(Me { dir: config_dir.join(HOSTS_FOLDER), kind: String::new(), port: None, project: None, announced: false });
    }
}

/// Announce this process as a host of `kind` (`app`, `mcp`), joinable on `port` when given.
pub fn announce(config_dir: &Path, kind: &str, port: Option<u16>) {
    let mut me = lock(&ME);
    let project = me.as_mut().and_then(|m| m.project.take());
    *me = Some(Me { dir: config_dir.join(HOSTS_FOLDER), kind: kind.to_string(), port, project, announced: true });
    if let Some(m) = me.as_ref() {
        write_entry(m);
    }
    drop(me);
    // Files of hosts that are gone are removed when listed; opening a project checks only the
    // hosts that have it. A long-running host prunes the folder once, in the background (not
    // in this crate's tests, which check what open and list remove).
    #[cfg(not(any(target_arch = "wasm32", test)))]
    let _ = std::thread::Builder::new().name("hosts-prune".into()).spawn(|| {
        list();
    });
}

/// Remove this process's entry (on exit).
pub fn withdraw() {
    let mut me = lock(&ME);
    if let Some(m) = me.as_mut().filter(|m| m.announced) {
        let _ = std::fs::remove_file(m.dir.join(format!("{}.json", std::process::id())));
        m.announced = false;
    }
}

fn write_entry(m: &Me) {
    if !m.announced || std::fs::create_dir_all(&m.dir).is_err() {
        return;
    }
    let entry = json!({"pid": std::process::id(), "kind": m.kind, "port": m.port, "project": m.project});
    let path = m.dir.join(format!("{}.json", std::process::id()));
    let partial = path.with_extension("partial");
    if std::fs::write(&partial, entry.to_string()).is_ok() {
        let _ = std::fs::rename(&partial, &path);
    }
}

/// The project this process now has open (`None`: none, or an unsaved one).
pub fn set_project(path: Option<&str>) {
    let mut me = lock(&ME);
    if let Some(m) = me.as_mut() {
        m.project = path.map(key);
        write_entry(m);
    }
}

/// Whether the process `pid` still runs (unknown: `None`).
fn alive(pid: u32) -> Option<bool> {
    if pid == std::process::id() {
        return Some(true);
    }
    #[cfg(unix)]
    {
        crate::system_tool::command("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Every live host (this process too), `[{pid, kind, port, project, me}]`. Entries of processes
/// that are gone are removed.
pub fn list() -> Vec<Value> {
    live_hosts(|_| true)
}

/// The live hosts whose entry passes `keep`. Liveness is checked only for those (it starts a
/// process per entry): opening a project checks none when no other host has it open.
fn live_hosts(keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    let dir = match lock(&ME).as_ref() {
        Some(m) => m.dir.clone(),
        None => return vec![],
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut out = vec![];
    for entry in entries.flatten().take(MAX_HOSTS) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(v) = std::fs::read(&path).ok().filter(|b| b.len() <= 64 * 1024).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
        let Some(pid) = v.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else { continue };
        if !keep(&v) {
            continue;
        }
        let old = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_AFTER);
        let live = match alive(pid) {
            Some(live) => live,
            None => !old,
        };
        if !live {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let mut v = v;
        v["me"] = json!(pid == std::process::id());
        out.push(v);
    }
    out.sort_by_key(|v| v.get("pid").and_then(Value::as_u64));
    out
}

/// Other live hosts that have `path` open.
pub(crate) fn others_with(path: &str) -> Vec<Value> {
    let k = key(path);
    let me = u64::from(std::process::id());
    live_hosts(|h| h.get("pid").and_then(Value::as_u64) != Some(me) && h.get("project").and_then(Value::as_str) == Some(k.as_str()))
}

/// A warning for opening `path` while other hosts have it open (`None`: nobody else does).
pub(crate) fn open_elsewhere_warning(path: &str, others: &[Value]) -> Option<String> {
    let first = others.first()?;
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
    let join = match first.get("port").and_then(Value::as_u64) {
        Some(port) => format!(" Agents: join it with `--bridge {port}` instead of working on a copy."),
        None => String::new(),
    };
    Some(format!(
        "{name} is also open in another EffectCraft ({} {}). Two copies overwrite each other when saved: save this one under a new name, or close one.{join}",
        first.get("kind").and_then(Value::as_str).unwrap_or("process"),
        first.get("pid").and_then(Value::as_u64).unwrap_or(0),
    ))
}

/// Hand `path` over from a windowless host that has it open (`effectcraft-cli serve`): ask it to
/// save and stop (`host.release`), and wait until it has, so this process opens the saved file
/// instead of a second copy. Its agents find the project here again with the MCP `hosts` tool.
/// Returns what happened, for the open's reply (`None`: no host had it).
pub(crate) fn take_over(path: &str) -> Option<String> {
    if cfg!(target_arch = "wasm32") {
        return None;
    }
    let host = others_with(path).into_iter().find(|h| h["kind"] == "host")?;
    let pid = host.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok())?;
    let Some(port) = host.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()) else {
        return Some(format!("a windowless host (pid {pid}) has this project open and can't be asked to hand it over"));
    };
    match release(port) {
        Ok(()) => {
            // It saves, then stops: wait for it to leave (a big project takes a few seconds).
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while alive(pid) == Some(true) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            Some(format!("took the project over from the windowless host on port {port}, which saved it and stopped; its agents rejoin it here (MCP `hosts`)"))
        }
        Err(e) => Some(format!("the windowless host on port {port} has this project open and didn't hand it over ({e}): you are opening a copy")),
    }
}

/// Ask the host on `port` to save its project and stop.
fn release(port: u16) -> std::result::Result<(), String> {
    use std::io::{BufRead, BufReader, Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(120))).map_err(|e| e.to_string())?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", json!({"id": 1, "method": "host.release", "params": {}})).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream).take(64 * 1024).read_line(&mut line).map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if reply["ok"] == json!(true) { Ok(()) } else { Err(reply["error"].as_str().unwrap_or("refused").to_string()) }
}

/// Remember `path` as it is on disk now (just opened or saved by this process).
pub(crate) fn remember(path: &str) {
    if let Some(s) = stamp(path) {
        lock(&SEEN).insert(key(path), s);
    }
}

/// Refuse to save over `path` when it changed on disk since this process opened or saved it.
pub(crate) fn check_unchanged(path: &str) -> Result<()> {
    let k = key(path);
    let Some(seen) = lock(&SEEN).get(&k).copied() else { return Ok(()) };
    match stamp(path) {
        Some(now) if now != seen => Err(EngineError::Other(format!(
            "{path} was changed on disk since this project was opened or saved here (by another EffectCraft, an agent or a script). Saving would erase those changes: save under a new name (file.saveAs), reload theirs (file.revert), or save with force: true to overwrite them."
        ))),
        _ => Ok(()),
    }
}

/// Forget this process's entry (tests; the save guard's memory stays, other tests rely on it).
/// Unix only, as the test that uses it (dead code on Windows failed clippy there).
#[cfg(all(test, unix))]
pub(crate) fn reset() {
    *lock(&ME) = None;
}
