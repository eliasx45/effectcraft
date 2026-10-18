//! Speech maps: a comp's voice-over words with their syllable onsets, stressed syllables marked
//! ([`effectcraft_project::speech`]), for timing animation peaks to speech. `scripts/fork/speech-map`
//! makes one outside the app (transcript, forced alignment, acoustic onset snap);
//! `speech.import` stores it on the comp (undoable), mapping media time through the voice
//! layer; `speech.get` lists words or accents; `speech.accents` matches every keyframed
//! property's speed-graph peak to its nearest accent (what the `off-accent` lint flags);
//! `speech.clear` removes the map.

use std::sync::Arc;

use effectcraft_project::speech::{MAX_SYLLABLES, MAX_WORDS, SpeechMap, SpeechWord, Syllable};
use effectcraft_project::{Comp, ItemId, Layer};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, comp_id, f_p, resolve_layer, str_p};
use crate::lint;
use crate::render_view::seconds_p;
use crate::{EngineError, Result, Session, cmd, query};

/// Most entries `speech.get` and `speech.accents` list.
const MAX_LIST: usize = 5_000;

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Media seconds (as in the speech-map file, minus its `offset`) → comp time through `layer`
/// (start time and stretch); `None` outside the layer's in/out.
fn through_layer(l: &Layer, media: f64) -> Option<Tick> {
    let t = l.comp_time(Tick::from_seconds_f64(media));
    (t >= l.in_point && t <= l.out_point).then_some(t)
}

/// A speech-map JSON (`{offset?, engine?, media?, words: [{word, start, end, syllables: [{t,
/// vowel, stress}]}]}`) as a comp map. Times are comp seconds, or media seconds + `offset`
/// mapped through `layer` when given (words outside its in/out are dropped). Returns the map
/// and the number of dropped words.
pub fn parse_map(doc: &Value, layer: Option<&Layer>, cmd: &str) -> Result<(SpeechMap, usize)> {
    let words = doc.get("words").and_then(Value::as_array).ok_or_else(|| bad(cmd, "the speech map has no `words` list"))?;
    let offset = f_p(doc, "offset").filter(|v| v.is_finite()).unwrap_or(0.0);
    let to_comp = |x: f64| -> Option<Tick> {
        if !x.is_finite() || x.abs() > 1e7 {
            return None;
        }
        match layer {
            Some(l) => through_layer(l, x - offset),
            None => Some(Tick::from_seconds_f64(x)),
        }
    };
    let mut out = vec![];
    let mut dropped = 0usize;
    for (i, w) in words.iter().take(MAX_WORDS).enumerate() {
        let num = |v: &Value, k: &str| f_p(v, k).ok_or_else(|| bad(cmd, format!("word {i}: `{k}` must be a number of seconds")));
        let (start, end) = (num(w, "start")?, num(w, "end")?);
        let (Some(start), Some(end)) = (to_comp(start), to_comp(end.max(start))) else {
            dropped += 1;
            continue;
        };
        let mut syllables = vec![];
        for s in w.get("syllables").and_then(Value::as_array).into_iter().flatten().take(MAX_SYLLABLES) {
            let (Some(t), Some(v)) = (f_p(s, "t").and_then(to_comp), f_p(s, "vowel").and_then(to_comp)) else { continue };
            let stress = s.get("stress").and_then(Value::as_u64).unwrap_or(0).min(2) as u8;
            syllables.push(Syllable { t, vowel: v.max(t), stress });
        }
        syllables.sort_by_key(|s| s.t);
        out.push(SpeechWord { word: str_p(w, "word").unwrap_or("").chars().take(200).collect(), start, end, syllables });
    }
    out.sort_by_key(|w| w.start);
    let text = |k: &str| str_p(doc, k).unwrap_or("").chars().take(2000).collect::<String>();
    Ok((SpeechMap { engine: text("engine"), media: text("media"), layer: layer.map(|l| l.id), words: out }, dropped))
}

fn import(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.import";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let layer = match p.get("layer").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(comp.layer(resolve_layer(comp, v).ok_or_else(|| bad(C, format!("no layer {v}")))?).ok_or_else(|| bad(C, format!("no layer {v}")))?),
    };
    if let Some(l) = layer
        && l.props.get("timeRemap").is_some_and(|t| !t.keys.is_empty() || t.has_expression())
    {
        return Err(bad(
            C,
            format!(
                "{} is time-remapped, so its media time can't be mapped to the comp: import without `layer`, with the map's times already in comp seconds (speech-map --offset)",
                l.name
            ),
        ));
    }
    let owned;
    let doc = match (p.get("map").filter(|v| !v.is_null()), str_p(p, "path")) {
        (Some(m), None) => m,
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path).map_err(|e| bad(C, format!("can't read {path}: {e}")))?;
            owned = serde_json::from_str::<Value>(&text).map_err(|e| bad(C, format!("{path} is not a speech-map JSON: {e}")))?;
            &owned
        }
        _ => return Err(bad(C, "give `path` (a speech-map JSON file) or `map` (its contents), not both")),
    };
    let (map, dropped) = parse_map(doc, layer, C)?;
    if map.words.is_empty() {
        return Err(bad(
            C,
            format!("no words of the map fall inside the comp{}", layer.map(|l| format!(" (through {}'s in/out)", l.name)).unwrap_or_default()),
        ));
    }
    let fps = comp.frame_rate.as_f64();
    let summary = summary(cid, &map, fps);
    let map = Arc::new(map);
    s.edit("Import Speech Map", None, |proj, _| {
        proj.comp_mut(cid).ok_or(EngineError::NoComp)?.speech = Some(map);
        Ok(())
    })?;
    let mut out = summary;
    out["dropped"] = json!(dropped);
    Ok(out)
}

fn summary(cid: ItemId, map: &SpeechMap, fps: f64) -> Value {
    let first = map.words.first().map(|w| w.start.seconds()).unwrap_or(0.0);
    let last = map.words.last().map(|w| w.end.seconds()).unwrap_or(0.0);
    json!({
        "comp": cid.0,
        "engine": map.engine,
        "media": map.media,
        "layer": map.layer.map(|l| l.0),
        "words": map.words.len(),
        "syllables": map.words.iter().map(|w| w.syllables.len()).sum::<usize>(),
        "stressed": map.stressed().len(),
        "start": r3(first),
        "end": r3(last),
        "fps": fps,
    })
}

fn the_map(s: &Session, cid: ItemId, cmd: &str) -> Result<Arc<SpeechMap>> {
    s.project
        .comp(cid)
        .and_then(|c| c.speech.clone())
        .ok_or_else(|| bad(cmd, "the comp has no speech map: make one with scripts/fork/speech-map and store it with speech.import"))
}

fn span(p: &Value, comp: &Comp, cmd: &str) -> Result<(f64, f64)> {
    let a = seconds_p(p, "start", cmd)?.unwrap_or(f64::NEG_INFINITY);
    let b = seconds_p(p, "end", cmd)?.unwrap_or(comp.duration.seconds().max(0.0) + 1.0);
    if b < a {
        return Err(bad(cmd, format!("empty range: end ({b} s) is before start ({a} s)")));
    }
    Ok((a, b))
}

fn lead_p(p: &Value, cmd: &str) -> Result<f64> {
    match p.get("accentLead").filter(|v| !v.is_null()) {
        None => Ok(lint::DEFAULT_ACCENT_LEAD),
        Some(v) => {
            v.as_f64().filter(|x| x.is_finite() && (0.0..=100.0).contains(x)).ok_or_else(|| bad(cmd, format!("`accentLead` is frames (0 to 100), not {v}")))
        }
    }
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.get";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let map = the_map(s, cid, C)?;
    let fps = comp.frame_rate.as_f64();
    let (a, b) = span(p, comp, C)?;
    let lead = lead_p(p, C)?;
    let frame = |t: Tick| r2(t.seconds() * fps);
    let mut out = summary(cid, &map, fps);
    let inside = |t: Tick| t.seconds() >= a - 1e-9 && t.seconds() <= b + 1e-9;
    if b_p(p, "stressed").unwrap_or(false) {
        let list: Vec<Value> = map
            .stressed()
            .into_iter()
            .filter(|(_, syl)| inside(syl.t))
            .take(MAX_LIST)
            .map(|(w, syl)| {
                let accent = lint::accent_time(syl, fps, lead);
                json!({
                    "word": w.word,
                    "onset": r3(syl.t.seconds()), "onsetFrame": frame(syl.t),
                    "vowel": r3(syl.vowel.seconds()), "vowelFrame": frame(syl.vowel),
                    "accent": r3(accent), "accentFrame": (accent * fps).round(),
                })
            })
            .collect();
        out["truncated"] = json!(list.len() >= MAX_LIST);
        out["accents"] = Value::Array(list);
    } else {
        let list: Vec<Value> = map
            .words
            .iter()
            .filter(|w| inside(w.start) || inside(w.end))
            .take(MAX_LIST)
            .map(|w| {
                json!({
                    "word": w.word,
                    "start": r3(w.start.seconds()), "end": r3(w.end.seconds()), "frame": frame(w.start),
                    "syllables": w.syllables.iter().map(|syl| json!({
                        "t": r3(syl.t.seconds()), "frame": frame(syl.t), "vowel": r3(syl.vowel.seconds()), "stress": syl.stress,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        out["truncated"] = json!(list.len() >= MAX_LIST);
        out["list"] = Value::Array(list);
    }
    Ok(out)
}

fn accents(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.accents";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let map = the_map(s, cid, C)?;
    let fps = comp.frame_rate.as_f64();
    let (a, b) = span(p, comp, C)?;
    let lead = lead_p(p, C)?;
    let only = match p.get("layer").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(resolve_layer(comp, v).ok_or_else(|| bad(C, format!("no layer {v}")))?),
    };
    let peaks: Vec<_> = lint::speed_peaks(comp, a, b).into_iter().filter(|pk| only.is_none_or(|l| pk.layer == l)).collect();
    let matched = lint::match_accents(peaks, &map, fps, lead);
    let hits = matched.iter().filter(|m| m.offset_frames.abs() <= 1.0 + 1e-6).count();
    let list: Vec<Value> = matched
        .iter()
        .take(MAX_LIST)
        .map(|m| {
            let l = comp.layer(m.peak.layer);
            json!({
                "layer": m.peak.layer.0,
                "layerName": l.map(|l| l.name.as_str()).unwrap_or(""),
                "property": m.peak.property,
                "path": m.peak.path,
                "time": r3(m.peak.time),
                "frame": r2(m.peak.time * fps),
                "speed": r2(m.peak.speed),
                "word": m.word.word,
                "onset": r3(m.syllable.t.seconds()),
                "vowel": r3(m.syllable.vowel.seconds()),
                "accent": r3(m.target),
                "accentFrame": (m.target * fps).round(),
                "offsetFrames": r2(m.offset_frames),
                "hit": m.offset_frames.abs() <= 1.0 + 1e-6,
            })
        })
        .collect();
    Ok(json!({
        "comp": cid.0,
        "accentLead": lead,
        "peaks": matched.len(),
        "hits": hits,
        "truncated": matched.len() > MAX_LIST,
        "list": list,
    }))
}

fn clear(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let had = s.project.comp(cid).ok_or(EngineError::NoComp)?.speech.is_some();
    if had {
        s.edit("Remove Speech Map", None, |proj, _| {
            proj.comp_mut(cid).ok_or(EngineError::NoComp)?.speech = None;
            Ok(())
        })?;
    }
    Ok(json!({"comp": cid.0, "removed": had}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "speech.import",
            "Import Speech Map",
            [],
            None,
            "{comp?, path? (speech-map JSON from scripts/fork/speech-map) | map? (its contents), layer? (the voice layer: the map's media seconds − its `offset` are mapped through this layer's start and stretch, words outside its in/out dropped; omit when the times are already comp seconds)} → {comp, engine, media, layer, words, syllables, stressed, start, end, fps, dropped}. Undoable; replaces the comp's map.",
            always,
            import
        ),
        query!(
            "speech.get",
            "Speech Map",
            "{comp?, start? (s), end? (s), stressed?: true (list the stressed syllables' accents: where a speed-graph peak should land), accentLead? (frames before the stressed vowel, default 1)} → {words, syllables, stressed, …, list: [{word, start, end, frame, syllables: [{t, frame, vowel, stress}]}] | accents: [{word, onset, onsetFrame, vowel, vowelFrame, accent, accentFrame}]}",
            get
        ),
        query!(
            "speech.accents",
            "Speed Peaks vs Accents",
            "{comp?, layer?, start? (s), end? (s), accentLead? (frames, default 1)} → {peaks, hits, list: [{layer, layerName, property, path, time, frame, speed, word, onset, vowel, accent, accentFrame, offsetFrames (peak − accent; negative = early), hit (within 1 frame)}]}: every keyframed property's speed-graph peak with its nearest stressed syllable",
            accents
        ),
        cmd!("speech.clear", "Remove Speech Map", [], None, "{comp?}", always, clear),
    ]
}
