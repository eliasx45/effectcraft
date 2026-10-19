//! The Why? strip under the Timeline (batch 1, CX6). After an agent built something and the user
//! then fixes it, a strip asks why, once per build: reason chips ("too early/late", "wrong word",
//! "too big/small", "not my style"), a free-text field, and skip. The answer runs `why.answer`
//! (an engine command, so agents can record whys too); its frontend half ([`record`]) appends it to
//! the learning loop's why log, `~/.claude/speech-edit/why-log.jsonl` (`EC_SPEECH_EDIT_HOME`
//! moves it), in the format `ec-why` writes (see `~/.claude/speech-edit/LOOP.md`):
//!
//! `{"ts": "2026-10-19T15:40:07", "burst": "app-1760888407", "why": "too big/small" | null,
//!   "skipped": false, "source": "app", "offered": [chips], "note"?: "free text",
//!   "edit": {"comp": {"id", "name"}, "layers": [{"id", "name"}], "changes": ["Scale"],
//!            "kinds": [], "agent": "claude-code-…", "ts": "…"}}`
//!
//! Nothing is written when that folder doesn't exist (another machine). The time is UTC.
// TODO(batch1-verify): loopd matches whys to its own bursts by burst id; "app-…" ids are new to it.

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::theme::Tokens;

/// The reason chips (the mock's WHYS).
pub const WHYS: [&str; 4] = ["too early/late", "wrong word", "too big/small", "not my style"];

/// What the strip asks about.
#[derive(Clone, Debug, PartialEq)]
struct Ask {
    /// The user's step ("Scale", "Move Keyframes").
    what: String,
    comp: Option<(u64, String)>,
    layers: Vec<(u64, String)>,
    agent: Option<String>,
    text: String,
}

/// Which build was asked about already (the address of Claude's version) and what is open.
#[derive(Clone, Debug, Default)]
struct Watch {
    asked_for: usize,
    revision: u64,
    open: Option<Ask>,
}

fn watch_id() -> egui::Id {
    egui::Id::new("why-chip-watch")
}

/// Look for the user's first fix after an agent's build (once per frame, from [`strip`]).
fn watch(app: &EffectcraftApp, ctx: &egui::Context) -> Watch {
    let mut w: Watch = ctx.data(|d| d.get_temp(watch_id())).unwrap_or_default();
    let s = &app.session;
    if s.revision == w.revision {
        return w;
    }
    w.revision = s.revision;
    let Some((claude, agent)) = effectcraft_engine::commands::suggestions::claude_version(s) else { return w };
    let ptr = std::sync::Arc::as_ptr(&claude) as usize;
    // The user's own step (no client), on top of what an agent built, not asked about yet.
    let user_step = s.history.author(&s.project).is_none() && !std::sync::Arc::ptr_eq(&claude, &s.project);
    if !user_step || w.asked_for == ptr || w.open.is_some() {
        return w;
    }
    let Some((what, _)) = s.history.undo.last() else { return w };
    let comp = s.active_comp_id().and_then(|c| Some((c.0, s.project.item(c)?.name.clone())));
    let layers: Vec<(u64, String)> = s
        .active_comp()
        .map(|c| s.state.selected_layers.iter().filter_map(|id| c.layer(*id).map(|l| (id.0, l.name.clone()))).take(8).collect())
        .unwrap_or_default();
    w.asked_for = ptr;
    w.open = Some(Ask { what: what.clone(), comp, layers, agent, text: String::new() });
    w
}

/// Draw the Why? strip at the bottom of the Timeline `rect` (above its footer); its height (0 when
/// closed).
pub fn strip(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: Rect) -> f32 {
    let ctx = ui.ctx().clone();
    let mut w = watch(app, &ctx);
    let Some(mut ask) = w.open.clone() else {
        ctx.data_mut(|d| d.insert_temp(watch_id(), w));
        return 0.0;
    };
    let t = app.tokens;
    let h = 30.0;
    let footer_h = 24.0;
    let r = Rect::from_min_max(pos2(rect.min.x, rect.max.y - footer_h - h), pos2(rect.max.x, rect.max.y - footer_h));
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(r, 0.0, Color32::from_rgb(0x22, 0x1f, 0x16));
    p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.warning.gamma_multiply(0.6)));
    app.auto.add("timeline.why", r, "Why?");
    let cy = r.center().y;
    let mut x = r.min.x + 10.0;
    p.text(pos2(x, cy), Align2::LEFT_CENTER, "Why?", Tokens::semibold(12.0), t.warning);
    x += 40.0;
    let on = ask.layers.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ");
    let about = if on.is_empty() { ask.what.clone() } else { format!("{} · {on}", ask.what) };
    let g = p.layout_no_wrap(about.clone(), Tokens::ui(11.0), t.text_dim);
    let gw = g.size().x.min(260.0);
    p.galley(pos2(x, cy - g.size().y / 2.0), g, t.text_dim);
    x += gw + 12.0;
    let mut answer: Option<(Option<String>, bool)> = None;
    for (i, why) in WHYS.iter().enumerate() {
        let bw = why.chars().count() as f32 * 6.4 + 18.0;
        let b = Rect::from_min_size(pos2(x, cy - 10.0), vec2(bw, 20.0));
        let resp = ui.interact(b, egui::Id::new(("why-chip", i)), Sense::click());
        p.rect_filled(b, 10.0, if resp.hovered() { t.hover } else { t.field_bg });
        p.text(b.center(), Align2::CENTER_CENTER, *why, Tokens::ui(11.0), t.text);
        app.auto.add(&format!("timeline.why.{i}"), b, why);
        if resp.clicked() {
            answer = Some((Some(why.to_string()), false));
        }
        x = b.max.x + 6.0;
    }
    // Free text (Enter sends it) and skip.
    let fw = (r.max.x - x - 70.0).clamp(60.0, 220.0);
    let fr = Rect::from_min_size(pos2(x, cy - 10.0), vec2(fw, 20.0));
    let fresp = ui.put(fr, egui::TextEdit::singleline(&mut ask.text).hint_text("or say why…"));
    app.auto.add_response("timeline.why.text", &fresp, "Why (free text)");
    if fresp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !ask.text.trim().is_empty() {
        answer = Some((Some(ask.text.trim().to_string()), false));
    }
    let sb = Rect::from_min_size(pos2(fr.max.x + 6.0, cy - 10.0), vec2(44.0, 20.0));
    let sresp = ui.interact(sb, egui::Id::new("why-skip"), Sense::click());
    p.text(sb.center(), Align2::CENTER_CENTER, "skip", Tokens::ui(11.0), if sresp.hovered() { t.text } else { t.text_faint });
    app.auto.add("timeline.why.skip", sb, "skip");
    if sresp.clicked() {
        answer = Some((None, true));
    }
    w.open = Some(ask.clone());
    if let Some((why, skip)) = answer {
        w.open = None;
        let params = json!({
            "why": why,
            "skip": skip,
            "what": ask.what,
            "comp": ask.comp.as_ref().map(|(id, name)| json!({"id": id, "name": name})),
            "layers": ask.layers.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            "agent": ask.agent,
        });
        match crate::menus::invoke(app, &ctx, "why.answer", params) {
            Ok(_) if !skip => app.toast = Some((format!("Saved: “{}”, Claude learns from it", why.unwrap_or_default()), ctx.input(|i| i.time))),
            Ok(_) => {}
            Err(e) => app.ui.status = e,
        }
    }
    ctx.data_mut(|d| d.insert_temp(watch_id(), w));
    h
}

/// The speech-edit folder (`EC_SPEECH_EDIT_HOME`, else `~/.claude/speech-edit`), when it exists.
pub(crate) fn speech_edit_home() -> Option<std::path::PathBuf> {
    let dir = match std::env::var_os("EC_SPEECH_EDIT_HOME") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".claude/speech-edit"),
    };
    dir.is_dir().then_some(dir)
}

/// Append one JSON line to `file` in the speech-edit folder (nothing when it doesn't exist).
pub(crate) fn append_jsonl(file: &str, lines: &[Value]) -> Result<bool, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::io::Write;
        let Some(dir) = speech_edit_home() else { return Ok(false) };
        let path = dir.join(file);
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut text = String::new();
        for l in lines {
            text.push_str(&l.to_string());
            text.push('\n');
        }
        f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(true)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (file, lines);
        Ok(false)
    }
}

/// Now as `YYYY-MM-DDTHH:MM:SS` (UTC) and as Unix seconds.
pub(crate) fn iso_now() -> (String, u64) {
    let secs = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    (iso_of(secs), secs)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SS` (UTC; civil-from-days).
pub(crate) fn iso_of(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The frontend half of `why.answer`: append it to the why log.
pub fn record(app: &mut EffectcraftApp, p: &Value) -> Result<Value, String> {
    let _ = app;
    let (ts, secs) = iso_now();
    let why = p.get("why").and_then(Value::as_str).map(str::to_string);
    let skipped = p.get("skip").and_then(Value::as_bool).unwrap_or(why.is_none());
    let mut entry = json!({
        "ts": ts,
        "burst": format!("app-{secs}"),
        "why": if skipped { Value::Null } else { json!(why) },
        "skipped": skipped,
        "source": "app",
        "offered": WHYS,
        "edit": {
            "comp": p.get("comp").cloned().unwrap_or(Value::Null),
            "layers": p.get("layers").cloned().unwrap_or(json!([])),
            "changes": p.get("what").and_then(Value::as_str).map(|w| vec![w.to_string()]).unwrap_or_default(),
            "kinds": [],
            "agent": p.get("agent").cloned().unwrap_or(Value::Null),
            "ts": ts,
        },
    });
    if let Some(w) = &why
        && !WHYS.contains(&w.as_str())
        && let Some(o) = entry.as_object_mut()
    {
        o.insert("note".into(), json!(w));
    }
    let written = append_jsonl("why-log.jsonl", &[entry])?;
    Ok(json!({"written": written}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso_of(0), "1970-01-01T00:00:00");
        // 19 Oct 2026 15:40:07 UTC.
        assert_eq!(iso_of(1_792_424_407), "2026-10-19T15:40:07");
        assert_eq!(iso_of(951_782_400), "2000-02-29T00:00:00");
    }
}
