//! `render.lint`: design problems in a comp at a time or over a time range, in plain language an
//! agent can act on (MCP `lint_frame`). The rules are [`crate::lint`]; this samples the times,
//! renders the text-free backdrop `low-contrast` measures and the frames `exposed-edge` checks
//! for empty pixels, walks the precomps for the nested rules, mixes the audio `off-beat` needs
//! and reads the comp's speech map for `off-accent`.

use effectcraft_project::{ItemId, LayerSource, Project};
use effectcraft_render::{Backend, RenderOpts, Renderer};
use effectcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use super::layout_cmds::renderer;
use super::{CommandSpec, bad, comp_id, f_p, time_p};
use crate::lint::{self, Backdrop, Settings};
use crate::render_view::seconds_p;
use crate::{EngineError, Result, Session, query};

/// Sample rate the beat tracker mixes the audio at (Hz).
const BEAT_RATE: u32 = 12_000;
/// Audio analysed around the linted span on each side (seconds): enough to find the tempo.
const BEAT_PAD: f64 = 4.0;
/// Longest stretch of audio the beat tracker mixes (seconds).
const MAX_BEAT_SPAN: f64 = 600.0;

/// Render `project`'s `cid` at `t` for a backdrop (the session's hosts, no layer cache: the
/// project is an edited copy).
fn render_backdrop(s: &Session, project: &Project, cid: ItemId, t: Tick, scale: f64, background: [f32; 3]) -> Backdrop {
    let opts = RenderOpts {
        scale,
        backend: Backend::Auto,
        nested_switches: s.prefs.general.switches_affect_nested_comps,
        draft_shadows: s.prefs.three_d.realtime_shadows,
        ..Default::default()
    };
    let mut r = Renderer::new(project, s.footage.as_ref(), opts);
    r.expr = s.expr.as_deref();
    r.accel = s.accel.as_deref();
    Backdrop { img: r.comp_frame(cid, t), scale, background }
}

/// Beat times (comp seconds) of the comp's mix over `a0..a1`.
fn comp_beats(s: &Session, cid: ItemId, a0: f64, a1: f64) -> Vec<f64> {
    let frames = ((a1 - a0).max(0.0) * BEAT_RATE as f64).round() as usize;
    let mut mono = Vec::with_capacity(frames);
    let t0 = Tick::from_seconds_f64(a0);
    let per = TICKS_PER_SECOND / BEAT_RATE as i64;
    let mut done = 0usize;
    while done < frames {
        let n = (frames - done).min(BEAT_RATE as usize);
        let t = t0 + Tick((done as i64).saturating_mul(per));
        let buf = effectcraft_render::audio::mix_comp(&s.project, s.footage.as_ref(), s.expr.as_deref(), cid, t, n, BEAT_RATE);
        mono.extend(buf.as_chunks::<2>().0.iter().map(|f| (f[0] + f[1]) * 0.5));
        done += n;
    }
    lint::detect_beats(&mono, BEAT_RATE).into_iter().map(|b| b + a0).collect()
}

/// A frame count param (≥ `min`), `default` when absent.
fn frames_p(p: &Value, k: &str, default: f64, min: f64, cmd: &str) -> Result<f64> {
    match p.get(k).filter(|v| !v.is_null()) {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x >= min && *x <= 100.0)
            .ok_or_else(|| bad(cmd, format!("`{k}` is a number of frames ({min} to 100), not {v}"))),
    }
}

fn render_lint(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "render.lint";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut set = Settings::new(comp);
    set.title_safe = s.prefs.grids.title_safe.clamp(0.0, 90.0);
    set.action_safe = s.prefs.grids.action_safe.clamp(0.0, 90.0);
    match p.get("rules").filter(|v| !v.is_null()) {
        None => {}
        Some(v) => {
            let names: Vec<&str> = match v {
                Value::String(r) => vec![r.as_str()],
                Value::Array(a) => {
                    a.iter().map(|x| x.as_str().ok_or_else(|| bad(C, format!("rules are rule ids (strings), not {x}")))).collect::<Result<_>>()?
                }
                _ => return Err(bad(C, "`rules` is a list of rule ids")),
            };
            set.rules = names
                .iter()
                .map(|n| lint::RULES.iter().find(|r| *r == n).copied().ok_or_else(|| bad(C, format!("unknown rule `{n}`; rules: {}", lint::RULES.join(", ")))))
                .collect::<Result<_>>()?;
        }
    }
    if let Some(v) = p.get("minTextPx").filter(|v| !v.is_null()) {
        set.min_text_px =
            v.as_f64().filter(|x| x.is_finite() && *x > 0.0).ok_or_else(|| bad(C, format!("`minTextPx` is a size in comp pixels (> 0), not {v}")))?;
    }
    if let Some(v) = p.get("minContrast").filter(|v| !v.is_null()) {
        set.min_contrast = v
            .as_f64()
            .filter(|x| (1.0..=21.0).contains(x))
            .ok_or_else(|| bad(C, format!("`minContrast` is a WCAG contrast ratio between 1 and 21 (default 3), not {v}")))?;
    }
    let start = seconds_p(p, "start", C)?;
    let end = seconds_p(p, "end", C)?;
    let range = start.is_some() || end.is_some();
    if range && (p.get("time").is_some_and(|v| !v.is_null()) || p.get("frame").is_some_and(|v| !v.is_null())) {
        return Err(bad(C, "give either `time` (one comp time) or `start`/`end` (a range), not both"));
    }
    let duration = comp.duration.seconds();
    let (times, truncated, span) = if range {
        let (a, b) = (start.unwrap_or(0.0), end.unwrap_or(duration));
        if b < a {
            return Err(bad(C, format!("empty range: end ({b} s) is before start ({a} s)")));
        }
        let step = match p.get("step").filter(|v| !v.is_null()) {
            None => lint::DEFAULT_STEP,
            Some(v) => {
                v.as_f64().filter(|x| x.is_finite() && *x >= 0.001).ok_or_else(|| bad(C, format!("`step` is seconds between samples (≥ 0.001), not {v}")))?
            }
        };
        let (times, truncated) = lint::sample_times(comp, a, b, step, lint::MAX_SAMPLES);
        (times, truncated, (a, b))
    } else {
        if let Some(v) = f_p(p, "time").filter(|t| !t.is_finite()) {
            return Err(bad(C, format!("`time` must be a finite number of seconds, not {v}")));
        }
        let t = time_p(s, p, Some(comp));
        (vec![t], false, (t.seconds(), t.seconds()))
    };

    let r = renderer(s, RenderOpts::default());
    let has_text = comp.layers.iter().any(|l| matches!(l.source, LayerSource::Text));
    let text_free = if set.on("low-contrast") && has_text { lint::without_text(&s.project, cid) } else { None };
    let scale = (lint::BACKDROP_SIDE / comp.width.max(comp.height).max(1) as f64).min(1.0);
    let mut occurrences = vec![];
    let mut coverage = vec![];
    for &t in &times {
        let Some(ctx) = r.eval_ctx(cid, t) else { continue };
        let backdrop = match &text_free {
            Some(tp) if lint::any_text_visible(&ctx) => Some(render_backdrop(s, tp, cid, t, scale, comp.background)),
            _ => None,
        };
        occurrences.extend(lint::lint_at(&ctx, &set, backdrop.as_ref()).into_iter().map(|f| (t, f)));
        occurrences.extend(lint::lint_nested(&r, &ctx, &set).into_iter().map(|f| (t, f)));
        if set.on("exposed-edge")
            && let (claim, Some(main), covered) = lint::frame_claim(&ctx)
            && claim >= lint::CLAIM_SHARE
        {
            let c =
                if covered { lint::Coverage::default() } else { lint::coverage(&render_backdrop(s, &s.project, cid, t, scale, comp.background).img, scale) };
            coverage.push((t, c, main));
        }
    }
    occurrences.extend(lint::exposed_edges(comp, &coverage));

    let mut notes: Vec<String> = vec![];
    if set.on("off-beat") {
        let fps = comp.frame_rate.as_f64();
        if !effectcraft_render::audio::comp_has_audio(&s.project, cid) {
            notes.push("off-beat skipped: the comp has no audible layers".into());
        } else {
            let a0 = (span.0 - BEAT_PAD).max(0.0);
            let a1 = (span.1 + BEAT_PAD).min(duration).min(a0 + MAX_BEAT_SPAN);
            let beats = comp_beats(s, cid, a0, a1);
            if beats.len() < 2 {
                notes.push("off-beat skipped: no clear beat found in the audio".into());
            } else {
                // A single time checks the key moments on that frame.
                let half = if range || fps.is_nan() || fps <= 0.0 { 0.0 } else { 0.5 / fps };
                let moments = lint::key_moments(&s.project, comp, span.0 - half, span.1 + half);
                let first = beats.first().copied().unwrap_or(0.0);
                let last = beats.last().copied().unwrap_or(0.0);
                let bpm = 60.0 * (beats.len() - 1) as f64 / (last - first).max(1e-9);
                notes.push(format!("off-beat: {} beats found between {first:.3} s and {last:.3} s (about {bpm:.0} BPM)", beats.len()));
                occurrences.extend(lint::off_beat(comp, &moments, &beats, fps));
            }
        }
    }
    if set.on("off-accent") {
        match comp.speech.as_deref().filter(|m| !m.words.is_empty()) {
            None => notes.push("off-accent skipped: the comp has no speech map (make one with scripts/fork/speech-map, store it with speech.import)".into()),
            Some(map) => {
                let fps = comp.frame_rate.as_f64();
                let lead = frames_p(p, "accentLead", lint::DEFAULT_ACCENT_LEAD, 0.0, C)?;
                let window = frames_p(p, "accentWindow", lint::DEFAULT_ACCENT_WINDOW, 1.0, C)?;
                let half = if range || fps.is_nan() || fps <= 0.0 { 0.0 } else { 0.5 / fps };
                let (found, skipped) = lint::off_accent(&s.project, cid, map, span.0 - half, span.1 + half, fps, lead, window);
                notes.extend(skipped.into_iter().map(|why| format!("off-accent: a precomp was not searched: {why}")));
                notes.push(format!(
                    "off-accent: {} stressed syllables in the speech map; peaks within {window} frames of an accent ({lead} frame(s) before the stressed vowel) that miss it by more than one frame: {}",
                    map.stressed().len(),
                    found.len()
                ));
                occurrences.extend(found);
            }
        }
    }

    let problems = lint::merge(occurrences);
    let count = |sev: lint::Severity| problems.iter().filter(|p| p.finding.severity == sev).count();
    let (errors, warnings, infos) = (count(lint::Severity::Error), count(lint::Severity::Warning), count(lint::Severity::Info));
    let list: Vec<Value> = problems
        .iter()
        .map(|pr| {
            let f = &pr.finding;
            let mut o = json!({
                "rule": f.rule,
                "severity": f.severity.name(),
                "layer": f.layer.0,
                "layerName": comp.layer(f.layer).map(|l| l.name.as_str()).unwrap_or(""),
                "time": pr.time.seconds(),
                "until": pr.until.seconds(),
                "message": f.message,
            });
            if let Some(other) = f.other {
                o["other"] = json!(other.0);
                o["otherName"] = json!(comp.layer(other).map(|l| l.name.as_str()).unwrap_or(""));
            }
            if let Some(i) = &f.inner {
                o["inner"] = json!({"comp": i.comp.0, "layer": i.layer.0, "path": i.path});
            }
            o
        })
        .collect();
    Ok(json!({
        "comp": cid.0,
        "compName": s.project.item(cid).map(|i| i.name.as_str()).unwrap_or(""),
        "times": times.iter().map(|t| t.seconds()).collect::<Vec<f64>>(),
        "truncated": truncated,
        "clean": errors + warnings == 0,
        "counts": {"error": errors, "warning": warnings, "info": infos},
        "rules": set.rules,
        "problems": list,
        "notes": notes,
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![query!(
        "render.lint",
        "Design Lint",
        "{comp?, time? (s, default the current time) | start? (s), end? (s) (a range, sampled every step), step? (s, default 0.5; plus each layer's in point and keyframe times; at most 120 samples), rules?: [text-too-small|low-contrast|outside-title-safe|outside-action-safe|cut-off|nested-cut-off|text-overlap|text-off-centre|hidden-visible|exposed-edge|off-beat|off-accent] (default all), minTextPx? (default comp height / 30), minContrast? (WCAG ratio, default 3), accentLead? (frames before the stressed vowel a speed peak should land, default 1), accentWindow? (frames: peaks this close to an accent but more than 1 off are flagged, default 4)} → {comp, compName, times, truncated, clean (no errors or warnings), counts {error, warning, info}, problems: [{rule, severity, layer, layerName, time, until, message, other?, inner? {comp, layer, path} (a layer inside a precomp: `layer` is the precomp layer showing it)}], notes}",
        render_lint
    )]
}
