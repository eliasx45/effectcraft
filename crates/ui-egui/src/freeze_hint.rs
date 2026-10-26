//! The Timeline's hint for a layer too heavy to play in real time ("Stat Stack renders at
//! 1.8 s/frame — Freeze it?") or footage worth a preview proxy ("Smoke is 6K and costs ~46
//! ms/frame here. Make a 1080p proxy?", the engine's `proxy.suggest`), and the feed of the viewer's per-layer render times that finds
//! it (the engine's `freeze.suggest`). Freezing always goes through `freeze.freeze`; with
//! `previews.freezeAutoHeavy` off nothing is frozen until the button is pressed.

use effectcraft_engine::project::ItemId;
use egui::{Align2, Rect, Stroke, pos2, vec2};
use serde_json::json;

use crate::{EffectcraftApp, frames::FrameKey, panels::panel_kit as kit, theme::Tokens};

/// Hand the engine the layer times of the frame the viewer shows, once per frame.
pub fn feed(app: &mut EffectcraftApp, ctx: &egui::Context) {
    let Some(key) = app.viewer_shown.as_ref().map(|(_, key)| *key) else {
        return;
    };
    let id = egui::Id::new("freeze-hint-fed");
    if ctx.data(|d| d.get_temp::<FrameKey>(id)) == Some(key) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, key));
    let Some(times) = app.frames.times.get(&key).filter(|t| !t.remote) else {
        return;
    };
    let costs: Vec<_> = times.layers.iter().map(|(&(comp, layer), &ms)| (comp, layer, ms)).collect();
    app.session.note_layer_costs(ItemId(key.comp), costs);
}

/// What the last click on the hint led to, shown in the hint's place.
#[derive(Clone)]
struct Outcome {
    /// What was frozen ("Stat Stack").
    name: String,
    /// The last Smart Freeze job on record before the click.
    before: Option<String>,
    /// `Some`: finished, show it until `until` (egui time); `None`: still freezing.
    done: Option<(String, bool, f64)>,
    started: f64,
}

const OUTCOME: &str = "freeze-hint-outcome";
/// Seconds a finished outcome stays up.
const SHOWN_FOR: f64 = 6.0;

/// The last Smart Freeze job on record: its id, status and message.
fn last_freeze_job(app: &EffectcraftApp) -> Option<(String, String, String)> {
    app.session.job_log.iter().rev().find(|j| j.kind == "smartFreeze").map(|j| (j.id.clone(), j.status.clone(), j.message.clone()))
}

/// The Timeline's hint, in `area` (the Timeline's graph above its footer): the open comp's
/// heaviest layer that can be frozen, with Freeze / dismiss buttons; then, in its place, how
/// freezing went ("Frozen ✓" or the reason it did not).
pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect) {
    if cfg!(target_arch = "wasm32") {
        return;
    }
    let ctx = ui.ctx().clone();
    let now = ctx.input(|i| i.time);
    let id = egui::Id::new(OUTCOME);
    let mut outcome: Option<Outcome> = ctx.data(|d| d.get_temp(id));
    if let Some(o) = outcome.as_mut() {
        let running = app.session.jobs().into_iter().find(|j| j.kind == "smartFreeze");
        if o.done.is_none() {
            if let Some(job) = &running {
                let percent = job.fraction.map(|f| format!(" {:.0}%", f * 100.0)).unwrap_or_default();
                let text = format!("Freezing {}…{percent}", o.name);
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
                note(app, ui, area, &text, None);
                ctx.data_mut(|d| d.insert_temp(id, o.clone()));
                return;
            }
            match last_freeze_job(app) {
                Some((job_id, status, message)) if Some(&job_id) != o.before.as_ref() => {
                    o.done = Some(if status == "done" {
                        (format!("Frozen ✓ {}", o.name), true, now + SHOWN_FOR)
                    } else {
                        (format!("Couldn't freeze {}: {}", o.name, message.trim_end_matches('.')), false, now + 3.0 * SHOWN_FOR)
                    });
                }
                // Nothing started (yet): give up quietly after a moment.
                _ if now - o.started > 3.0 => outcome = None,
                _ => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if let Some((text, ok, until)) = outcome.as_ref().and_then(|o| o.done.clone()) {
            if now < until {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
                note(app, ui, area, &text, Some(ok));
                ctx.data_mut(|d| d.insert_temp(id, outcome.clone()));
                return;
            }
            outcome = None;
        }
        ctx.data_mut(|d| match &outcome {
            Some(o) => {
                d.insert_temp(id, o.clone());
            }
            None => d.remove::<Outcome>(id),
        });
        if outcome.is_some() {
            return;
        }
    }
    if running_freeze(app) {
        return;
    }
    // Heavy footage first: a proxy fixes the decode cost at its source.
    if proxy_hint(app, ui, area, &ctx, now) {
        return;
    }
    let Some(top) = app.session.freeze_suggestions().into_iter().next() else {
        return;
    };
    let t = app.tokens;
    let text = top.question();
    let font = Tokens::ui(11.5);
    let width = ui.painter().layout_no_wrap(text.clone(), font.clone(), t.text).size().x;
    let bar = Rect::from_min_size(pos2(area.min.x + 10.0, area.max.y - 30.0), vec2((width + 140.0).min(area.width() - 20.0), 24.0));
    if bar.width() < 120.0 {
        return;
    }
    let p = ui.painter();
    p.rect_filled(bar, 6.0, t.panel_bg);
    p.rect_stroke(bar, 6.0, Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
    p.text(pos2(bar.min.x + 10.0, bar.center().y), Align2::LEFT_CENTER, &text, font, t.text);
    app.auto.add("timeline.freezeHint", bar, &text);
    let freeze = Rect::from_min_size(pos2(bar.max.x - 90.0, bar.min.y + 2.0), vec2(60.0, 20.0));
    if kit::button(app, ui, freeze, "timeline.freezeHint.freeze", "Freeze", true) {
        let before = last_freeze_job(app).map(|j| j.0);
        let started = now;
        match app.session.execute("freeze.freeze", top.freeze_params()) {
            Ok(_) => ctx.data_mut(|d| {
                d.insert_temp(id, Outcome { name: top.name.clone(), before, done: None, started });
            }),
            Err(e) => {
                let reason = format!("Couldn't freeze {}: {e}", top.name);
                app.ui.status = reason.clone();
                ctx.data_mut(|d| {
                    d.insert_temp(id, Outcome { name: top.name.clone(), before, done: Some((reason, false, now + 3.0 * SHOWN_FOR)), started });
                });
            }
        }
    }
    let dismiss = Rect::from_min_size(pos2(bar.max.x - 26.0, bar.min.y + 2.0), vec2(20.0, 20.0));
    if kit::button(app, ui, dismiss, "timeline.freezeHint.dismiss", "×", false) {
        kit::exec(app, "freeze.suggest", json!({"dismiss": {"comp": top.comp.0, "layer": top.layer.0}}));
    }
}

const PROXY_TOP: &str = "proxy-hint-top";

/// The automatic proxy suggestion in the hint's place ("Smoke is 6K and costs ~46 ms/frame here.
/// Make a 1080p proxy?" with Yes / No: `proxy.autoCreate` / `proxy.dismissSuggestion`, which is
/// saved in the project), or, while a proxy is being made, its progress. `true` when shown.
fn proxy_hint(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect, ctx: &egui::Context, now: f64) -> bool {
    use effectcraft_engine::auto_proxy::{ProxySuggestion, TASK_KIND};
    if let Some(job) = app.session.jobs().into_iter().find(|j| j.kind == TASK_KIND && j.running) {
        let what = job.label.rsplit_once(" (#").map(|(w, _)| w.to_owned()).unwrap_or_else(|| job.label.clone());
        let percent = job.fraction.map(|f| format!(" {:.0}%", f * 100.0)).unwrap_or_default();
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        note(app, ui, area, &format!("Making {what}…{percent}"), None);
        return true;
    }
    // Looked up twice a second, not every frame (it walks every comp's layers).
    let id = egui::Id::new(PROXY_TOP);
    let top = match ctx.data(|d| d.get_temp::<(f64, Option<ProxySuggestion>)>(id)) {
        Some((at, top)) if (0.0..0.5).contains(&(now - at)) => top,
        _ => {
            let top = app.session.proxy_suggestions().into_iter().next();
            ctx.data_mut(|d| d.insert_temp(id, (now, top.clone())));
            top
        }
    };
    let Some(top) = top else {
        return false;
    };
    let t = app.tokens;
    let text = top.question();
    let font = Tokens::ui(11.5);
    let width = ui.painter().layout_no_wrap(text.clone(), font.clone(), t.text).size().x;
    let bar = Rect::from_min_size(pos2(area.min.x + 10.0, area.max.y - 30.0), vec2((width + 120.0).min(area.width() - 20.0), 24.0));
    if bar.width() < 120.0 {
        return false;
    }
    let p = ui.painter();
    p.rect_filled(bar, 6.0, t.panel_bg);
    p.rect_stroke(bar, 6.0, Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
    p.text(pos2(bar.min.x + 10.0, bar.center().y), Align2::LEFT_CENTER, &text, font, t.text);
    app.auto.add("timeline.proxyHint", bar, &text);
    let yes = Rect::from_min_size(pos2(bar.max.x - 96.0, bar.min.y + 2.0), vec2(46.0, 20.0));
    let no = Rect::from_min_size(pos2(bar.max.x - 46.0, bar.min.y + 2.0), vec2(40.0, 20.0));
    let mut answered = false;
    if kit::button(app, ui, yes, "timeline.proxyHint.yes", "Yes", true) {
        kit::exec(app, "proxy.autoCreate", json!({"item": top.item.0}));
        answered = true;
    }
    if kit::button(app, ui, no, "timeline.proxyHint.no", "No", false) {
        kit::exec(app, "proxy.dismissSuggestion", json!({"item": top.item.0}));
        answered = true;
    }
    if answered {
        ctx.data_mut(|d| d.remove::<(f64, Option<ProxySuggestion>)>(id));
    }
    true
}

fn running_freeze(app: &EffectcraftApp) -> bool {
    app.session.jobs().iter().any(|j| j.kind == "smartFreeze")
}

/// A one-line bar in the hint's place: `ok` colours it (`None`: neutral, still working).
fn note(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect, text: &str, ok: Option<bool>) {
    let t = app.tokens;
    let font = Tokens::ui(11.5);
    let width = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), t.text).size().x;
    let bar = Rect::from_min_size(pos2(area.min.x + 10.0, area.max.y - 30.0), vec2((width + 24.0).min(area.width() - 20.0), 24.0));
    if bar.width() < 60.0 {
        return;
    }
    let edge = match ok {
        Some(false) => t.danger,
        _ => t.accent,
    };
    let p = ui.painter();
    p.rect_filled(bar, 6.0, t.panel_bg);
    p.rect_stroke(bar, 6.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    p.text(pos2(bar.min.x + 10.0, bar.center().y), Align2::LEFT_CENTER, text, font, t.text);
    app.auto.add("timeline.freezeHint.result", bar, text);
}
