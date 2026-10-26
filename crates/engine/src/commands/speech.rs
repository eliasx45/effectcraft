//! Speech maps: a comp's voice-over words with their syllable onsets, stressed syllables marked
//! ([`effectcraft_project::speech`]), for timing animation peaks to speech. `scripts/fork/speech-map`
//! makes one outside the app (transcript, forced alignment, acoustic onset snap);
//! `speech.import` stores it on the comp (undoable), mapping media time through the voice
//! layer; `speech.get` lists words or accents; `speech.accents` matches every keyframed
//! property's speed-graph peak, precomps included, to its nearest accent (what the `off-accent` lint flags);
//! `speech.clear` removes the map.

use std::sync::Arc;

use effectcraft_project::speech::{MAX_SYLLABLES, MAX_WORDS, SpeechMap, SpeechWord, Syllable, guess_syllables, normalize};
use effectcraft_project::{Comp, ItemId, ItemKind, Layer, LayerSource};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, comp_id, f_p, frontend, has_comp, layer_p, resolve_layer, str_p};
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
        out.push(SpeechWord { word: str_p(w, "word").unwrap_or("").chars().take(200).collect(), start, end, syllables, guessed: false });
    }
    out.sort_by_key(|w| w.start);
    // A forced-aligned map leaves words its dictionary doesn't know (brands: "YouTube") without
    // syllables: estimate them, so peaks can be aligned to every word. A words-only map
    // (scribe-words) stays words only.
    if out.iter().any(|w| !w.syllables.is_empty()) {
        for w in out.iter_mut().filter(|w| w.syllables.is_empty()) {
            w.syllables = guess_syllables(&w.word, w.start, w.end);
            w.guessed = !w.syllables.is_empty();
        }
    }
    let text = |k: &str| str_p(doc, k).unwrap_or("").chars().take(2000).collect::<String>();
    Ok((SpeechMap { engine: text("engine"), media: text("media"), layer: layer.map(|l| l.id), words: out, script: None }, dropped))
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
    let (mut map, dropped) = parse_map(doc, layer, C)?;
    // The comp's script stays (SX5).
    map.script = comp.speech.as_ref().and_then(|m| m.script.clone());
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
        "guessed": map.words.iter().filter(|w| w.guessed).map(|w| w.word.as_str()).take(50).collect::<Vec<_>>(),
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
                let mut o = json!({
                    "word": w.word,
                    "start": r3(w.start.seconds()), "end": r3(w.end.seconds()), "frame": frame(w.start),
                    "syllables": w.syllables.iter().map(|syl| json!({
                        "t": r3(syl.t.seconds()), "frame": frame(syl.t), "vowel": r3(syl.vowel.seconds()), "stress": syl.stress,
                    })).collect::<Vec<_>>(),
                });
                if w.guessed {
                    o["guessed"] = json!(true);
                }
                o
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
    // `layer` picks the comp's own layer, or a precomp layer and everything in it.
    let deep = lint::speed_peaks_deep(&s.project, cid, a, b);
    let peaks: Vec<_> = deep.peaks.into_iter().filter(|pk| only.is_none_or(|l| pk.via.first().map_or(pk.layer, |h| h.layer) == l)).collect();
    let matched = lint::match_accents(peaks, &map, fps, lead);
    let hits = matched.iter().filter(|m| m.offset_frames.abs() <= 1.0 + 1e-6).count();
    let list: Vec<Value> = matched
        .iter()
        .take(MAX_LIST)
        .map(|m| {
            let inner = m.peak.comp.unwrap_or(cid);
            let l = s.project.comp(inner).and_then(|c| c.layer(m.peak.layer));
            let via: Vec<u64> = m.peak.via.iter().map(|h| h.layer.0).collect();
            // Ready `keys.alignPeak` params: land this peak on its accent.
            let word = map.words.iter().position(|w| std::ptr::eq(w, m.word));
            let syllable = m.word.syllables.iter().position(|x| *x == m.syllable);
            let mut align =
                json!({"comp": inner.0, "layer": m.peak.layer.0, "path": m.peak.path, "speech": {"index": word, "syllable": syllable}, "accentLead": lead});
            if m.peak.comp.is_some() {
                (align["targetComp"], align["via"]) = (json!(cid.0), json!(via));
            }
            json!({
                "comp": inner.0,
                "compName": s.project.item(inner).map(|i| i.name.as_str()).unwrap_or(""),
                "via": via,
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
                "alignPeak": align,
            })
        })
        .collect();
    Ok(json!({
        "comp": cid.0,
        "accentLead": lead,
        "peaks": matched.len(),
        "hits": hits,
        "truncated": matched.len() > MAX_LIST,
        "skipped": deep.skipped,
        "list": list,
    }))
}

fn clear(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let old = s.project.comp(cid).ok_or(EngineError::NoComp)?.speech.clone();
    let had = old.as_ref().is_some_and(|m| !m.words.is_empty());
    if had {
        // The script stays (SX5): only the words go.
        let keep = old.and_then(|m| m.script.clone()).map(|script| Arc::new(SpeechMap { script: Some(script), ..SpeechMap::default() }));
        s.edit("Remove Speech Map", None, |proj, _| {
            proj.comp_mut(cid).ok_or(EngineError::NoComp)?.speech = keep;
            Ok(())
        })?;
    }
    Ok(json!({"comp": cid.0, "removed": had}))
}

/// One word as `layer.words` and `speech.find` list it: comp seconds and frames, and where a
/// speed-graph peak should land on it (`stress`: its first primary-stressed syllable's accent).
fn word_json(i: usize, w: &SpeechWord, fps: f64, lead: f64) -> Value {
    let frame = |t: Tick| (t.seconds() * fps).round() as i64;
    let mut o = json!({
        "index": i,
        "word": w.word,
        "start": r3(w.start.seconds()),
        "end": r3(w.end.seconds()),
        "frames": [frame(w.start), frame(w.end)],
    });
    if let Some((k, syl)) = w.stressed() {
        let accent = lint::accent_time(syl, fps, lead);
        o["stress"] = json!((accent * fps).round() as i64);
        o["stressTime"] = json!(r3(accent));
        o["syllable"] = json!(k);
        o["onsetFrame"] = json!(frame(syl.t));
    }
    if w.guessed {
        o["guessed"] = json!(true);
    }
    o
}

/// `layer.words {layer}` (SX1): the words said inside a layer's in/out (the voice layer's own, or
/// any layer's: the words a caption or card covers), with frames and stress.
fn layer_words(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.words";
    let (cid, lid) = layer_p(s, p, C)?;
    let map = the_map(s, cid, C)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let layer = comp.layer(lid).ok_or_else(|| bad(C, format!("no layer {}", lid.0)))?;
    let fps = comp.frame_rate.as_f64();
    let lead = lead_p(p, C)?;
    let frame = |t: Tick| (t.seconds() * fps).round() as i64;
    let (a, b) = (layer.in_point, layer.out_point);
    let list: Vec<Value> = map.audible(comp).filter(|(_, w)| w.end > a && w.start < b).take(MAX_LIST).map(|(i, w)| word_json(i, w, fps, lead)).collect();
    Ok(json!({
        "comp": cid.0,
        "layer": lid.0,
        "name": layer.name,
        "voiceLayer": map.layer.map(|l| l.0),
        "span": {"in": r3(a.seconds()), "out": r3(b.seconds()), "inFrame": frame(a), "outFrame": frame(b)},
        "fps": fps,
        "count": list.len(),
        "truncated": list.len() >= MAX_LIST,
        "words": list,
    }))
}

/// `speech.find {word, occurrence?}` (SX4): where a word (or a few words in a row) is said; the
/// playhead goes there (its stressed syllable, else its start) unless `go: false`.
fn find(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.find";
    let cid = comp_id(s, p)?;
    let map = the_map(s, cid, C)?;
    let text = str_p(p, "word").ok_or_else(|| bad(C, "missing `word` (a word, or a few said in a row)"))?;
    let q: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).take(32).collect();
    if q.is_empty() {
        return Err(bad(C, format!("{text:?} has no letters or digits to look for")));
    }
    let occurrence = match p.get("occurrence").filter(|v| !v.is_null()) {
        None => 1,
        Some(v) => v.as_u64().filter(|n| *n >= 1).ok_or_else(|| bad(C, format!("`occurrence` is 1 or more, not {v}")))? as usize,
    };
    let lead = lead_p(p, C)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    let audible: Vec<(usize, &SpeechWord)> = map.audible(comp).collect();
    let norm: Vec<String> = audible.iter().map(|(_, w)| normalize(&w.word)).collect();
    let hits: Vec<usize> = (0..norm.len()).filter(|&k| q.iter().enumerate().all(|(d, x)| norm.get(k + d) == Some(x))).collect();
    let Some(&pos) = hits.get(occurrence - 1) else {
        return Err(if hits.is_empty() {
            bad(C, format!("{text:?} isn't said in this comp"))
        } else {
            bad(C, format!("{text:?} is said {} time(s); there is no occurrence {occurrence}", hits.len()))
        });
    };
    let Some(&(first, w)) = audible.get(pos) else { return Err(bad(C, "the word moved while searching")) };
    let phrase: Vec<(usize, &SpeechWord)> = (0..q.len()).filter_map(|d| audible.get(pos + d).copied()).collect();
    let end = phrase.last().map_or(w.end, |(_, x)| x.end);
    // The phrase's first stressed syllable (a peak lands there), else its first word's start.
    let accent = phrase.iter().find_map(|(_, x)| x.stressed().map(|(_, syl)| lint::accent_time(syl, fps, lead)));
    let at = accent.unwrap_or(w.start.seconds());
    let frame = (at * fps).round() as i64;
    let all: Vec<i64> = hits.iter().take(200).filter_map(|k| audible.get(*k)).map(|(_, x)| (x.start.seconds() * fps).round() as i64).collect();
    let out = json!({
        "comp": cid.0,
        "word": text,
        "index": first,
        "occurrence": occurrence,
        "of": hits.len(),
        "time": r3(at),
        "frame": frame,
        "stressed": accent.is_some(),
        "start": r3(w.start.seconds()),
        "end": r3(end.seconds()),
        "words": phrase.iter().map(|(i, x)| word_json(*i, x, fps, lead)).collect::<Vec<_>>(),
        "occurrences": all,
    });
    // The playhead moves only when the comp searched is the open one (checked by hand, BUILD2).
    if b_p(p, "go").unwrap_or(true) && s.active_comp_id() == Some(cid) {
        s.set_time(fr.tick_of(frame));
    }
    Ok(out)
}

/// The part of its media (media seconds: start, duration) a layer plays inside the comp's
/// 0..duration: a voice layer longer than the comp isn't heard past its end, so it isn't sent to
/// the transcriber (BUILD2 SX2: a 575 s voice-over in a 12 s comp sent all 573 s).
pub(crate) fn media_span(layer: &Layer, comp: &Comp) -> Option<(f64, f64)> {
    let (a, b) = (layer.in_point.max(Tick::ZERO), layer.out_point.min(comp.duration));
    if b <= a {
        return None;
    }
    let (x, y) = (layer.layer_time(a).seconds(), layer.layer_time(b).seconds());
    let start = x.min(y).max(0.0);
    let dur = (x.max(y) - start).max(0.0);
    (dur > 0.0).then_some((start, dur))
}

/// `speech.transcribe {layer}` (SX2): the voice layer's words. With `path` / `words` (the JSON
/// `scribe-words` or `scripts/fork/speech-map` wrote, media seconds) it imports them through the
/// layer, as `speech.import {layer}`. Without, the desktop app runs the transcriber in the
/// background (`speech-map`: Scribe text, forced alignment and onset snap, so stressed
/// syllables are known; else `scribe-words`: words only) on the layer's part of its media, and
/// imports the result with this command when it is done. The engine itself runs nothing.
fn transcribe(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "speech.transcribe";
    let (cid, lid) = layer_p(s, p, C)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fps = comp.frame_rate.as_f64();
    let layer = comp.layer(lid).ok_or_else(|| bad(C, format!("no layer {}", lid.0)))?;
    let LayerSource::Footage { item } = layer.source else {
        return Err(bad(C, format!("{} is not a footage layer: transcribe the layer with the voice-over", layer.name)));
    };
    let footage = match s.project.item(item).map(|i| &i.kind) {
        Some(ItemKind::Footage(f)) if f.has_audio => f,
        _ => return Err(bad(C, format!("{} has no sound to transcribe", layer.name))),
    };
    let given = match (p.get("words").filter(|v| !v.is_null()), str_p(p, "path")) {
        (Some(w), None) => Some(json!({"comp": cid.0, "layer": lid.0, "map": w})),
        (None, Some(path)) => Some(json!({"comp": cid.0, "layer": lid.0, "path": path})),
        (None, None) => None,
        _ => return Err(bad(C, "give `path` or `words`, not both")),
    };
    if let Some(q) = given {
        let mut out = import(s, &q)?;
        out["transcribed"] = json!(true);
        return Ok(out);
    }
    if layer.props.get("timeRemap").is_some_and(|t| !t.keys.is_empty() || t.has_expression()) {
        return Err(bad(C, format!("{} is time-remapped: transcribe its media and import with speech.import (times in comp seconds)", layer.name)));
    }
    let Some((start, dur)) = media_span(layer, comp) else {
        return Err(bad(C, format!("{} plays no time of its media inside the comp", layer.name)));
    };
    let job = json!({
        "comp": cid.0,
        "layer": lid.0,
        "name": layer.name,
        "media": footage.path,
        "start": r3(start),
        "dur": r3(dur),
        "fps": fps,
        "local": b_p(p, "local").unwrap_or(false),
        "aligned": b_p(p, "aligned").unwrap_or(true),
    });
    frontend(s, C, &job)?;
    let mut out = job;
    out["started"] = json!(true);
    out["hint"] = json!("the desktop app transcribes in the background, then imports the words (one undo step); poll speech.get or layer.words");
    Ok(out)
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
            "{comp?, layer?, start? (s), end? (s), accentLead? (frames, default 1)} → {peaks, hits, list: [{comp (the layer's comp: a precomp for nested moves), compName, via (precomp layer ids from this comp down, outermost first), layer, layerName, property, path, time, frame (this comp's), speed, word, onset, vowel, accent, accentFrame, offsetFrames (peak − accent; negative = early), hit (within 1 frame), alignPeak (keys.alignPeak params that land it on the accent)}], skipped (precomps not searched: time-remapped)}: every keyframed property's speed-graph peak, also inside precomps (mapped through each precomp layer's start and stretch; kept only where every precomp layer shows it), with its nearest stressed syllable; `layer` may be a precomp layer (everything in it)",
            accents
        ),
        cmd!("speech.clear", "Remove Speech Map", [], None, "{comp?} (the comp's script stays)", always, clear),
        query!(
            "layer.words",
            "Layer Words",
            "{comp?, layer? (default: the selected layer; the voice layer or any layer: the words said inside its in/out), accentLead? (frames, default 1)} → {comp, layer, name, voiceLayer, span: {in, out, inFrame, outFrame}, fps, count, truncated, words: [{index (in the speech map), word, start, end (comp s), frames: [start, end], stress? (comp frame where a peak should land: the stressed syllable's vowel − accentLead), stressTime?, syllable?, onsetFrame?}]}. The words belong to the voice layer: moving, trimming, duplicating or precomposing it carries them",
            layer_words
        ),
        cmd!(
            "speech.find",
            "Find Word",
            [],
            None,
            "{comp?, word (a word or a few said in a row; case and punctuation ignored), occurrence? (1-based, default 1), go? (default true: the playhead goes there), accentLead? (frames, default 1)} → {comp, word, index, occurrence, of (how many times it is said), time, frame (its stressed syllable's accent, else its start), stressed, start, end, words: [...as layer.words], occurrences: [start frames]}",
            has_comp,
            find
        ),
        cmd!(
            "speech.transcribe",
            "Transcribe Voice Layer",
            [],
            None,
            "{comp?, layer? (default: the selected layer; footage with sound), path? | words? (a scribe-words / speech-map JSON, media seconds: imported through the layer, undoable), local? (faster-whisper instead of ElevenLabs Scribe), aligned? (default true: speech-map with forced alignment, so stressed syllables are known; false: scribe-words, words only)} → without path/words the desktop app transcribes the layer's part of its media in the background and imports it: {started, comp, layer, media, start, dur, hint}; with them as speech.import {…, transcribed}",
            has_comp,
            transcribe
        ),
    ]
}
