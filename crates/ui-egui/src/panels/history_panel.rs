//! History (batch 1, CX7): what the user did, from the command journal, for the user (the Motion
//! Library's History tab) and for the learning loop (a JSONL file loopd reads). `history.usage` is
//! the engine side (agents read it over MCP).
//!
//! Each frame [`frame`] looks at the journal's new entries, stamps them with the wall clock, the
//! playhead, the nearest spoken word and who made them (the undo step's author), keeps them for
//! the tab and appends the actions to the usage log.
//!
//! ## Usage log format (read by the learning loop, `~/.claude/speech-edit/loop/`)
//!
//! File: `~/.claude/speech-edit/usage-log.jsonl` (`EC_SPEECH_EDIT_HOME` moves the folder; nothing is
//! written when the folder doesn't exist). Append-only, UTF-8, one JSON object per line, one line
//! per command the user (or an agent) ran, in the order they ran. Selection and navigation
//! (`layer.select`, `time.*`, `view.*`, … see `suggestions::NOT_ACTIONS`) are left out.
//!
//! ```json
//! {"v": 1,
//!  "ts": "2026-10-19T15:40:07",          // UTC, seconds
//!  "unix": 1792424407,
//!  "session": "…",                       // Session::session_id (one app run)
//!  "n": 412,                             // index in that session's command journal
//!  "command": "graph.apply",             // the command id
//!  "label": "Apply Graph",               // its label
//!  "params": {…},                        // its parameters (cut to 4 KB: then {"truncated": true})
//!  "by": "user" | "<agent client>",      // who made the undo step this frame ("user" = no client)
//!  "undo": "Apply Graph" | null,         // the newest undo step's name at that moment
//!  "comp": {"id": 2562264, "name": "S2 06 Wall"} | null,
//!  "layers": [{"id": 2563304, "name": "Card · valentina"}],  // from `layer` / `layers` params, else the selection
//!  "frame": 128,                         // the playhead (comp frame) when it ran
//!  "near": {"word": "works", "time": 4.27} | null           // nearest spoken word (within 1 s)
//! }
//! ```
//!
//! Several entries in one frame (a script, a batch) share the frame's `by`, `undo` and `frame`.
//! The why answers go to `why-log.jsonl` next to it ([`super::why_chip`]).

use std::sync::{Mutex, MutexGuard, PoisonError};

use effectcraft_engine::commands::suggestions;
use egui::{Align2, Rect, pos2};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::theme::Tokens;

/// The usage log's file name in the speech-edit folder.
pub const USAGE_FILE: &str = "usage-log.jsonl";

/// One stamped action.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The local wall clock, `HH:MM` (the usage log's `ts` stays UTC).
    pub clock: String,
    pub command: String,
    pub label: String,
    pub layers: Vec<String>,
    pub near: Option<String>,
    pub by: String,
    pub line: Value,
}

#[derive(Default)]
struct State {
    /// Journal entries seen.
    seen: usize,
    session: String,
    entries: Vec<Entry>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

const MAX_ENTRIES: usize = 500;
const MAX_PARAMS: usize = 4096;

/// Once per frame: stamp the journal's new entries and append them to the usage log.
pub fn frame(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let _ = ctx;
    let s = &app.session;
    let len = s.journal.len();
    let mut g = state();
    let st = g.get_or_insert_with(State::default);
    if st.session != s.session_id {
        // A new session: start at its current end (its earlier commands were before this app ran).
        st.session = s.session_id.clone();
        st.seen = len;
        return;
    }
    if len < st.seen {
        // The journal was trimmed (it drops its oldest 1000 past 10 000) or reset (a project opened).
        // TODO(batch1-verify): a reset and a trim look alike here; a reset may skip a few entries.
        st.seen = if st.seen > 9_000 { st.seen.saturating_sub(1000).min(len) } else { 0 };
    }
    if len == st.seen {
        return;
    }
    let (ts, unix) = super::why_chip::iso_now();
    // The tab shows the local clock; the log keeps UTC (`ts`) and Unix seconds.
    let clock = local_clock(unix);
    let by = s.history.author(&s.project).unwrap_or("user").to_string();
    let undo = s.history.undo.last().map(|(l, _)| l.clone());
    let comp = s.active_comp();
    let cid = s.active_comp_id();
    let comp_v = cid.and_then(|c| Some(json!({"id": c.0, "name": s.project.item(c)?.name}))).unwrap_or(Value::Null);
    let frame_no = comp.map(|c| c.frame_rate.frame_at(s.time()));
    let mut lines = vec![];
    for (n, (id, params)) in s.journal.iter().enumerate().skip(st.seen) {
        if !suggestions::is_action(id) {
            continue;
        }
        let label = effectcraft_engine::commands::find(id).map(|c| c.label).unwrap_or("").to_string();
        let ids: Vec<u64> = match comp {
            Some(c) => {
                let named = suggestions::param_layers(c, params);
                if named.is_empty() { s.state.selected_layers.iter().map(|l| l.0).collect() } else { named }
            }
            None => vec![],
        };
        let layers: Vec<(u64, String)> = comp
            .map(|c| ids.iter().filter_map(|l| c.layer(effectcraft_engine::project::LayerId(*l)).map(|x| (*l, x.name.clone()))).take(16).collect())
            .unwrap_or_default();
        let near = comp.and_then(|c| suggestions::nearest_word(c, s.time()));
        let text = params.to_string();
        let params_v = if text.len() > MAX_PARAMS { json!({"truncated": true}) } else { params.clone() };
        let line = json!({
            "v": 1,
            "ts": ts,
            "unix": unix,
            "session": s.session_id,
            "n": n,
            "command": id,
            "label": label,
            "params": params_v,
            "by": by,
            "undo": undo,
            "comp": comp_v,
            "layers": layers.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            "frame": frame_no,
            "near": near.as_ref().map(|(w, t)| json!({"word": w, "time": t})),
        });
        st.entries.push(Entry {
            clock: clock.clone(),
            command: id.clone(),
            label,
            layers: layers.into_iter().map(|(_, n)| n).collect(),
            near: near.map(|(w, _)| w),
            by: by.clone(),
            line: line.clone(),
        });
        lines.push(line);
    }
    st.seen = len;
    let over = st.entries.len().saturating_sub(MAX_ENTRIES);
    st.entries.drain(..over);
    drop(g);
    if !lines.is_empty()
        && let Err(e) = super::why_chip::append_jsonl(USAGE_FILE, &lines)
    {
        log::warn!("usage log: {e}");
    }
}

/// `HH:MM` on the local clock for Unix seconds `unix` (UTC when the offset can't be read).
pub(crate) fn local_clock(unix: u64) -> String {
    clock_at(unix, local_offset_secs().unwrap_or(0))
}

/// `HH:MM` of Unix seconds `unix` at `offset` seconds east of UTC.
pub(crate) fn clock_at(unix: u64, offset: i64) -> String {
    let t = i64::try_from(unix).unwrap_or(0).saturating_add(offset).rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
}

/// The local UTC offset in seconds, from the OS (`date +%z`; on Windows the registry's
/// ActiveTimeBias), read again every 10 minutes so a DST change shows up. `None` on the web or when
/// the OS doesn't say.
pub(crate) fn local_offset_secs() -> Option<i64> {
    static CACHE: Mutex<Option<(u64, Option<i64>)>> = Mutex::new(None);
    let now = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut g = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, off)) = *g
        && now.abs_diff(at) < 600
    {
        return off;
    }
    let off = os_offset();
    *g = Some((now, off));
    off
}

#[cfg(all(not(target_arch = "wasm32"), not(windows)))]
fn os_offset() -> Option<i64> {
    let out = effectcraft_engine::system_tool::command("date").arg("+%z").output().ok()?;
    parse_z(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(all(not(target_arch = "wasm32"), windows))]
fn os_offset() -> Option<i64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args(["query", r"HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation", "/v", "ActiveTimeBias"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_reg_bias(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_arch = "wasm32")]
fn os_offset() -> Option<i64> {
    None
}

/// `date +%z` output (`+0200`, `-0530`) as seconds east of UTC.
#[cfg_attr(any(windows, target_arch = "wasm32"), allow(dead_code))]
fn parse_z(text: &str) -> Option<i64> {
    let t = text.trim();
    let sign = match t.get(..1)? {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let digits = t.get(1..)?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits.get(..2)?.parse().ok()?;
    let m: i64 = digits.get(2..)?.parse().ok()?;
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// `reg query … /v ActiveTimeBias` output (`ActiveTimeBias  REG_DWORD  0xffffff88`: minutes to add
/// to local time for UTC, as a signed 32-bit value) as seconds east of UTC.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_reg_bias(text: &str) -> Option<i64> {
    let line = text.lines().find(|l| l.contains("ActiveTimeBias"))?;
    let hex = line.split_whitespace().find_map(|w| w.strip_prefix("0x"))?;
    let bias = u32::from_str_radix(hex, 16).ok()? as i32;
    let off = -i64::from(bias) * 60;
    (off.abs() <= 14 * 3600).then_some(off)
}

/// The Motion Library's History tab: newest first, with the command each one ran.
pub fn history_tab(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect, s: f32) {
    let t = app.tokens;
    let p = ui.painter().with_clip_rect(area);
    p.rect_filled(area, 0.0, t.panel_bg);
    app.auto.add("motionLibrary.history", area, "History");
    let x0 = area.min.x + 12.0 * s;
    let mut y = area.min.y + 12.0 * s;
    p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, "History · Claude reads this and learns from it", Tokens::semibold(12.5 * s), t.text);
    y += 26.0 * s;
    let entries: Vec<Entry> = state().as_ref().map(|st| st.entries.clone()).unwrap_or_default();
    if entries.is_empty() {
        p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, "Nothing yet. Use a wheel, a button or a paste.", Tokens::ui(11.0 * s), t.text_dim);
        return;
    }
    for (i, e) in entries.iter().rev().enumerate() {
        if y > area.max.y - 30.0 * s {
            break;
        }
        let mut head = format!("{} · {}", e.clock, if e.label.is_empty() { e.command.as_str() } else { e.label.as_str() });
        if !e.layers.is_empty() {
            head.push_str(&format!(" · {}", e.layers.join(", ")));
        }
        if let Some(w) = &e.near {
            head.push_str(&format!(" · near “{w}”"));
        }
        if e.by != "user" {
            head.push_str(&format!(" · {}", e.by));
        }
        p.text(
            pos2(x0, y + 8.0 * s),
            Align2::LEFT_CENTER,
            &head,
            Tokens::medium(11.5 * s),
            if e.by == "user" { t.text } else { super::suggestions::CLAUDE_BLUE },
        );
        let cmd = format!("{} {}", e.command, e.line["params"]);
        let cmd: String = cmd.chars().take(140).collect();
        p.text(pos2(x0, y + 24.0 * s), Align2::LEFT_CENTER, &cmd, Tokens::mono(10.5 * s), t.text_dim);
        app.auto.add(&format!("motionLibrary.history.{i}"), Rect::from_min_max(pos2(x0, y), pos2(area.max.x, y + 32.0 * s)), &head);
        y += 38.0 * s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 19 Oct 2026 15:40:07 UTC (CX7: the tab showed 15:40 for 17:40 CEST).
    const T: u64 = 1_792_424_407;

    #[test]
    fn the_tab_clock_is_local_time() {
        assert_eq!(clock_at(T, 0), "15:40");
        assert_eq!(clock_at(T, 2 * 3600), "17:40");
        assert_eq!(clock_at(T, -(5 * 3600 + 30 * 60)), "10:10");
        // Across midnight both ways.
        assert_eq!(clock_at(T, 9 * 3600), "00:40");
        assert_eq!(clock_at(T - 15 * 3600, -3600), "23:40");
    }

    #[test]
    fn os_offsets_parse() {
        assert_eq!(parse_z("+0200\n"), Some(7200));
        assert_eq!(parse_z("-0530"), Some(-19_800));
        assert_eq!(parse_z("+0000"), Some(0));
        for bad in ["", "0200", "+02", "+2a00", "+9900", "CEST"] {
            assert_eq!(parse_z(bad), None, "{bad}");
        }
        let reg = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Control\\TimeZoneInformation\r\n    ActiveTimeBias    REG_DWORD    0xffffff88\r\n";
        assert_eq!(parse_reg_bias(reg), Some(7200));
        assert_eq!(parse_reg_bias("    ActiveTimeBias    REG_DWORD    0x12c"), Some(-18_000));
        assert_eq!(parse_reg_bias("nothing"), None);
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn the_os_gives_an_offset() {
        let off = local_offset_secs().expect("date +%z");
        assert!(off.abs() <= 14 * 3600);
        assert_eq!(local_clock(T), clock_at(T, off));
    }
}
