//! Times and eases as an editor reads them (editing harness step 2, "senses").
//!
//! Replies give times as seconds for programs, and next to them the Timeline's timecode in its
//! short form (`6:20` = 6 s 20 frames, `1:02:15` = 1 min 2 s 15 frames) plus the comp frame number,
//! so an agent and the user talk about the same frame. Time parameters accept the same forms back:
//! `"6:20"`, `"0:00:06:20"`, `"200f"` or `{"frame": 200}` (see [`resolve_time_params`]). Keyframe
//! eases read as the Graph Editor shows them: speed and influence per side and dimension.

use effectcraft_keyframe::{Interp, Keyframe};
use effectcraft_project::Comp;
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::{EngineError, Result, Session};

/// The comp frame `t` falls on (nearest: key times stored as seconds can sit a tick early).
pub fn frame_of(rate: FrameRate, t: Tick) -> i64 {
    let f = t.seconds() * rate.as_f64();
    if f.is_finite() { f.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64 } else { 0 }
}

/// The Timeline's timecode for comp time `t`, short: leading zero fields dropped (`6:20`,
/// `0:05`, `1:02:15`; hours only when there are any).
pub fn short_timecode(c: &Comp, t: Tick) -> String {
    let start = frame_of(c.frame_rate, c.display_start);
    let (neg, h, m, s, f) = effectcraft_time::frames_to_fields(frame_of(c.frame_rate, t).saturating_add(start), c.frame_rate, false);
    let fw = if c.frame_rate.timecode_base() >= 100 { 3 } else { 2 };
    let sign = if neg { "-" } else { "" };
    if h != 0 {
        format!("{sign}{h}:{m:02}:{s:02}:{f:0fw$}")
    } else if m != 0 {
        format!("{sign}{m}:{s:02}:{f:0fw$}")
    } else {
        format!("{sign}{s}:{f:0fw$}")
    }
}

/// `{"tc", "frame"}` for comp time `t`, to sit next to its seconds.
pub fn time_fields(c: &Comp, t: Tick) -> Value {
    json!({"tc": short_timecode(c, t), "frame": frame_of(c.frame_rate, t)})
}

/// Adds `<prefix>Tc` / `<prefix>Frame` (or `tc` / `frame` for an empty prefix) for comp time `t`.
pub fn put_time(o: &mut Value, prefix: &str, c: &Comp, t: Tick) {
    if !o.is_object() {
        return;
    }
    let (tc, frame) = if prefix.is_empty() { ("tc".to_string(), "frame".to_string()) } else { (format!("{prefix}Tc"), format!("{prefix}Frame")) };
    o[tc] = json!(short_timecode(c, t));
    o[frame] = json!(frame_of(c.frame_rate, t));
}

fn num(x: f64) -> String {
    let r = (x * 100.0).round() / 100.0;
    if r == 0.0 { "0".into() } else { format!("{r}") }
}

/// One side of key `i` per dimension as the Graph Editor shows it: `[{speed, influence}]`
/// (influence in %), `None` for hold sides and sides without a neighbour.
pub fn side_eases(keys: &[Keyframe], i: usize, spatial: bool, out: bool) -> Option<Vec<(f64, f64)>> {
    let k = keys.get(i)?;
    let dims = if spatial { 1 } else { k.value.dims().max(1) };
    let v: Vec<(f64, f64)> =
        (0..dims).filter_map(|d| effectcraft_keyframe::side_ease(keys, i, d, spatial, out)).map(|e| (e.speed, e.influence * 100.0)).collect();
    (!v.is_empty()).then_some(v)
}

/// A side in words: `hold`, `linear`, or `bezier 0/s 33.3%` (one entry per dimension when
/// they differ: `bezier [0, 120]/s 33.3%`, or `x 0/s 33.3% | y 120/s 75%`).
fn side_text(interp: Interp, eases: Option<&[(f64, f64)]>) -> String {
    match (interp, eases) {
        (Interp::Hold, _) => "hold".into(),
        (Interp::Linear, _) => "linear".into(),
        (Interp::Bezier, None) => "bezier".into(),
        (Interp::Bezier, Some(e)) => {
            let first = e.first().copied().unwrap_or((0.0, 0.0));
            let same = |a: f64, b: f64| (a - b).abs() < 0.005;
            if e.iter().all(|x| same(x.0, first.0) && same(x.1, first.1)) {
                format!("bezier {}/s {}%", num(first.0), num(first.1))
            } else if e.iter().all(|x| same(x.1, first.1)) {
                format!("bezier [{}]/s {}%", e.iter().map(|x| num(x.0)).collect::<Vec<_>>().join(", "), num(first.1))
            } else {
                let axis = ["x", "y", "z", "w"];
                let parts: Vec<String> = e.iter().enumerate().map(|(d, x)| format!("{} {}/s {}%", axis.get(d).unwrap_or(&"?"), num(x.0), num(x.1))).collect();
                format!("bezier {}", parts.join(" | "))
            }
        }
    }
}

/// Ease readout of key `i`: `inEase` / `outEase` (per dimension, `{speed, influence}`, influence
/// in %; null on hold sides and the open side of the first/last key), `continuous` and
/// `autoBezier` when either side is Bezier, and `ease`, the same in words
/// (`in bezier 0/s 33.3% · out bezier 0/s 75%`, or one side when both match).
pub fn ease_fields(keys: &[Keyframe], i: usize, spatial: bool) -> Value {
    let Some(k) = keys.get(i) else { return json!({}) };
    let side = |out: bool| side_eases(keys, i, spatial, out);
    let (ie, oe) = (side(false), side(true));
    let as_json = |e: &Option<Vec<(f64, f64)>>| match e {
        Some(v) => json!(v.iter().map(|(s, f)| json!({"speed": s, "influence": f})).collect::<Vec<_>>()),
        None => Value::Null,
    };
    let a = side_text(k.in_interp, ie.as_deref());
    let b = side_text(k.out_interp, oe.as_deref());
    let mut o = json!({
        "inEase": as_json(&ie), "outEase": as_json(&oe),
        "ease": if a == b { a } else { format!("in {a} · out {b}") },
    });
    if k.in_interp == Interp::Bezier || k.out_interp == Interp::Bezier {
        o["continuous"] = json!(k.continuous);
        o["autoBezier"] = json!(k.auto_bezier);
    }
    o
}

/// Names of time parameters that take timecode (`"6:20"`) and frames (`{"frame": 200}`, `"200f"`)
/// as well as seconds. Applies at the top level and inside lists of objects (`keys: [{time}]`).
const TIME_KEYS: &[&str] =
    &["time", "times", "start", "end", "from", "to", "at", "inPoint", "outPoint", "startTime", "endTime", "compTime", "layerTime", "newTime", "duration"];

/// Seconds for a timecode or frame form of a time value of comp `c`: `Some(Err)` for a string that
/// is meant as a time but doesn't parse, `None` when `v` isn't one of those forms (a number of
/// seconds, a `$1.compTime` reference, …: left as it is).
pub fn timecode_seconds(c: &Comp, v: &Value) -> Option<std::result::Result<f64, String>> {
    let rate = c.frame_rate;
    let frames = match v {
        Value::Object(o) if o.len() == 1 => {
            let f = o.get("frame")?;
            match f.as_i64() {
                Some(n) => Ok(n),
                None => Err(format!("`frame` must be a whole frame number, got {f}")),
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if let Some(n) = t.strip_suffix('f').filter(|n| !n.is_empty() && n.trim_start_matches('-').bytes().all(|b| b.is_ascii_digit())) {
                n.parse::<i64>().map_err(|e| format!("bad frame number `{t}`: {e}"))
            } else if t.contains([':', ';']) && t.bytes().all(|b| b.is_ascii_digit() || b":;-+".contains(&b)) {
                let start = frame_of(rate, c.display_start);
                effectcraft_time::parse_timecode(t, rate, t.contains(';'), 0).map(|f| f.saturating_sub(start)).map_err(|e| format!("`{t}`: {e}"))
            } else {
                // Seconds sent as a string: still seconds (never read as s.ff).
                let secs = t.parse::<f64>().ok().filter(|x| x.is_finite())?;
                return Some(Ok(secs));
            }
        }
        _ => return None,
    };
    Some(frames.map(|f| rate.tick_of(f).seconds()))
}

fn resolve_value(c: &Comp, key: &str, v: &mut Value) -> std::result::Result<(), String> {
    if let Value::Array(a) = v {
        for x in a.iter_mut() {
            resolve_value(c, key, x)?;
        }
        return Ok(());
    }
    if let Some(r) = timecode_seconds(c, v) {
        *v = json!(r.map_err(|e| format!("`{key}`: {e}"))?);
    }
    Ok(())
}

/// Whether `v` holds a timecode or frame form anywhere [`resolve_time_params`] looks.
pub fn has_time_forms(p: &Value) -> bool {
    fn form(v: &Value) -> bool {
        match v {
            Value::String(s) => s.contains([':', ';']) || s.trim().ends_with('f') || s.trim().parse::<f64>().is_ok(),
            Value::Object(o) => o.len() == 1 && o.contains_key("frame"),
            Value::Array(a) => a.iter().any(form),
            _ => false,
        }
    }
    let Some(o) = p.as_object() else { return false };
    o.iter().any(|(k, v)| {
        (TIME_KEYS.contains(&k.as_str()) && form(v))
            || v.as_array().is_some_and(|a| a.iter().filter_map(Value::as_object).any(|e| e.iter().any(|(k, v)| TIME_KEYS.contains(&k.as_str()) && form(v))))
    })
}

/// Turn timecode / frame forms of the time parameters of `p` into seconds of its comp (`comp`,
/// else the active one), in place. Plain numbers stay seconds. A no-op (no comp needed) when
/// nothing is in those forms.
pub fn resolve_time_params(s: &Session, id: &str, p: &mut Value) -> Result<()> {
    if !has_time_forms(p) {
        return Ok(());
    }
    let cid = s
        .resolve_comp(p.get("comp").filter(|c| !c.is_null()))
        .map_err(|_| crate::commands::bad(id, "a timecode or frame time needs a composition (`comp`, or an open one)"))?;
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let Some(o) = p.as_object_mut() else { return Ok(()) };
    for (k, v) in o.iter_mut() {
        if TIME_KEYS.contains(&k.as_str()) {
            resolve_value(c, k, v).map_err(|e| crate::commands::bad(id, e))?;
        } else if let Value::Array(a) = v {
            for e in a.iter_mut().filter_map(Value::as_object_mut) {
                for (k2, v2) in e.iter_mut().filter(|(k2, _)| TIME_KEYS.contains(&k2.as_str())) {
                    resolve_value(c, k2, v2).map_err(|e| crate::commands::bad(id, e))?;
                }
            }
        }
    }
    Ok(())
}

/// A value short enough for a line of text: numbers to 3 decimals, long text cut at 60 chars.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Number(n) => n.as_f64().map(|f| format!("{}", (f * 1000.0).round() / 1000.0)).unwrap_or_else(|| n.to_string()),
        Value::Array(a) => format!("[{}]", a.iter().map(value_text).collect::<Vec<_>>().join(", ")),
        other => {
            let s = other.to_string();
            if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
        }
    }
}
