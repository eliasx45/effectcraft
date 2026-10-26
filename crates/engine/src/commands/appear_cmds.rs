//! `speech.appearPlan`: when each layer should appear, from the word it shows ([`crate::appear`]).

use serde_json::{Value, json};

use super::{CommandSpec, bad, comp_id, no_layer, resolve_layer};
use crate::appear::{self, Ask};
use crate::{EngineError, Result, Session, lint, query};

fn asks(comp: &effectcraft_project::Comp, p: &Value, cmd: &str) -> Result<Option<Vec<Ask>>> {
    let Some(list) = p.get("layers").filter(|v| !v.is_null()) else { return Ok(None) };
    let list = list.as_array().ok_or_else(|| bad(cmd, "`layers` is a list of layers (id, name, \"#n\") or {layer, words?, occurrence?}"))?;
    let mut out = vec![];
    for v in list.iter().take(400) {
        let (l, words, occurrence) = match v.as_object() {
            Some(o) => (
                o.get("layer").ok_or_else(|| bad(cmd, format!("{v}: missing `layer`")))?,
                o.get("words").or_else(|| o.get("word")).and_then(Value::as_str).map(str::to_string),
                o.get("occurrence").and_then(Value::as_u64).map(|n| n.max(1) as usize),
            ),
            None => (v, None, None),
        };
        let layer = resolve_layer(comp, l).ok_or_else(|| bad(cmd, no_layer(l)))?;
        out.push(Ask { layer, words, occurrence });
    }
    Ok(Some(out))
}

fn appear_plan(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.appearPlan";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let map = comp
        .speech
        .as_deref()
        .filter(|m| !m.words.is_empty())
        .ok_or_else(|| bad(C, "the comp has no speech map: make one with speech.transcribe (or scripts/fork/speech-map + speech.import)"))?;
    let lead = match p.get("accentLead").filter(|v| !v.is_null()) {
        None => lint::DEFAULT_ACCENT_LEAD,
        Some(v) => v.as_f64().filter(|x| x.is_finite() && (0.0..=100.0).contains(x)).ok_or_else(|| bad(C, "`accentLead` is frames (0 to 100)"))?,
    };
    let asks = asks(comp, p, C)?;
    let fps = comp.frame_rate.as_f64();
    let ctx = effectcraft_render::EvalCtx::new(&s.project, cid, comp, effectcraft_time::Tick::ZERO);
    let entries = appear::plan(&ctx, map, asks.as_deref(), lead);
    let off: Vec<&appear::Entry> = entries.iter().filter(|e| e.word.is_some() && e.off_by.abs() > appear::OFF_WORD_FRAMES).collect();
    let fixes: Vec<Value> = off.iter().filter_map(|e| appear::fix(e, fps)).collect();
    Ok(json!({
        "comp": cid.0,
        "fps": fps,
        "rule": format!("an entrance's speed peak lands on its word's stressed syllable ({lead} frame(s) before the vowel); a layer that cuts in starts on its word. More than {} frames off is off-word.", appear::OFF_WORD_FRAMES),
        "layers": entries.iter().map(|e| appear::entry_json(e, fps)).collect::<Vec<_>>(),
        "off": off.len(),
        "unmatched": entries.iter().filter(|e| e.word.is_none()).count(),
        "fixes": fixes,
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![query!(
        "speech.appearPlan",
        "Appear Plan",
        "{comp?, layers? ([id|name|#n | {layer, words? (what it shows, when its text/name doesn't say), occurrence? (1-based)}]; default: every text layer, and layers whose name has a spoken word), accentLead? (frames, default 1)} → {comp, fps, rule, layers: [{layer, name, shows, from (text|name|given), appearFrame, entrance? {property, path, peakFrame}, word, wordIndex, wordFrame, accentFrame, suggestedFrame (where the layer should appear), offBy (frames: the entrance's peak − the accent, or the appear − the word's start), measured (peak|appear), sure, fix? {command, params}} | {…, unmatched}], off, unmatched, fixes}: each layer's first frame against the spoken word it shows (needs the comp's speech map)",
        appear_plan
    )]
}
