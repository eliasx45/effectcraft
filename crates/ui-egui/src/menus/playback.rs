//! Preview playback: the Preview panel's shortcuts, stop, audio scrubbing and options.

use serde_json::{Value, json};

use super::dispatch::Handler;
use super::toggle;
use crate::EffectcraftApp;

pub(crate) fn handlers() -> &'static [Handler] {
    const H: &[Handler] = &[
        Handler::ui("playback.ramPreview", ram_preview),
        Handler::ui("playback.preview.shiftSpacebar", preview),
        Handler::ui("playback.preview.shiftNumpad0", preview),
        Handler::ui("playback.preview.altNumpad0", preview),
        Handler::ui("playback.stop", stop),
        Handler::ui("playback.faster", faster),
        Handler::ui("playback.slower", slower),
        // Audio scrubbing: one frame of audio at `time` (default the current time).
        Handler::frontend("playback.scrubAudio", scrub_audio),
        Handler::frontend("playback.toggle", play_toggle),
        Handler::frontend("playback.cacheWhenIdle", cache_when_idle),
        Handler::frontend("playback.audio", audio),
    ];
    H
}

fn ram_preview(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    app.toggle_play_with(now, effectcraft_engine::preview::PreviewShortcut::Numpad0);
    Ok(json!({"playing": app.playback.playing}))
}

fn preview(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    let sc = effectcraft_engine::preview::PreviewShortcut::parse(id.trim_start_matches("playback.preview.")).ok_or("unknown preview shortcut")?;
    app.toggle_play_with(now, sc);
    Ok(json!({"playing": app.playback.playing}))
}

fn stop(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    app.stop();
    Ok(Value::Null)
}

/// Play Faster: ×2, then ×4, ×8 with each press (FilmCraft's Shuttle Right).
fn faster(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    app.play_faster(now);
    Ok(json!({"playing": app.playback.playing, "speed": app.playback.speed.max(1)}))
}

/// Play Slower: halves Play Faster's speed, down to normal speed.
fn slower(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, _params: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    app.play_slower(now);
    Ok(json!({"playing": app.playback.playing, "speed": app.playback.speed.max(1)}))
}

fn scrub_audio(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    let cid = app.session.active_comp_id().ok_or("no composition")?;
    let t = p.get("time").and_then(Value::as_f64).map_or(app.session.time(), effectcraft_engine::time::Tick::from_seconds_f64);
    app.scrub_audio(cid, t, now);
    Ok(json!({"playing": app.scrub.is_some()}))
}

fn play_toggle(app: &mut EffectcraftApp, ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let now = ctx.input(|i| i.time);
    // The play button and Spacebar: `{shortcut?}` picks which Preview panel shortcut's
    // options to play with (default Spacebar).
    let sc = match p.get("shortcut").and_then(Value::as_str) {
        Some(k) => effectcraft_engine::preview::PreviewShortcut::parse(k).ok_or_else(|| format!("unknown preview shortcut `{k}`"))?,
        None => effectcraft_engine::preview::PreviewShortcut::Spacebar,
    };
    app.toggle_play_with(now, sc);
    Ok(json!({"playing": app.playback.playing, "shortcut": sc.id()}))
}

fn cache_when_idle(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    let r = toggle(&mut app.ui.cache_when_idle, &p);
    app.set_pref("previews.cacheFramesWhenIdle", r.clone())?;
    Ok(r)
}

fn audio(app: &mut EffectcraftApp, _ctx: &egui::Context, _id: &str, p: Value) -> Result<Value, String> {
    // Include Audio of the shortcut shown in the Preview panel.
    let mut on = app.session.prefs.preview.active().include_audio;
    let r = toggle(&mut on, &p);
    app.session.execute("playback.settings.set", json!({"includeAudio": on})).map_err(|e| e.to_string())?;
    if !on {
        app.audio = None;
    }
    Ok(r)
}
