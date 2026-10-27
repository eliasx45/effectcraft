//! The Script tab's commands (SX5, batch 1 lane B). A comp's script is kept with its speech map
//! ([`effectcraft_project::speech::Script`]) and lined up with the words said
//! ([`effectcraft_project::speech::align_script`]): words not said, ad-libs, each line's span,
//! the layers covering it and the stretches with no animation.
//!
//! - `script.load {path | text}`: store the script (undoable).
//! - `script.get`: everything the Script tab shows, for Claude over MCP and Ghostex's Script panel.
//!   SX7: with no .txt it builds the lines from the speech map, and every line carries its chapter
//!   (comp markers), deep scene path and gap flag ([`crate::script_lines`]).
//! - `script.copyRef {line | from, to}`: a short reference for the chat ("10-10 MAIN · script line 2
//!   "really WORKS every single time" · f98–f127 · words: …"); the app puts it on the clipboard.
//! - `script.board {lines}`: a storyboard of lines; the app sends it to FigCraft.
//! - `word.apply {index, to?, item}`: build an item on a word or a phrase, its peak on the
//!   stressed syllable (Pop text, Caption, Pill, Highlight, Zoom punch, SFX, a Motion Library
//!   preset adapted to the phrase's length, Copy for Claude). One undo step.

use std::sync::Arc;

use effectcraft_project::speech::{AlignedLine, LineTok, Script, SpeechMap, SpeechWord, align_script, emphasis};
use effectcraft_project::{Comp, FootageKind, ItemId, ItemKind, LayerId, LayerSource, Project};
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, f_p, frontend, has_comp, str_p};
use crate::lint;
use crate::script_lines::{self, Chapter, SceneFinder, SceneHit};
use crate::{EngineError, Result, Session, cmd, query};

/// Most characters a script file may have (about 2 MB of text).
const MAX_SCRIPT_BYTES: usize = 2 << 20;
/// Default: a stretch of the voice-over longer than this with no animation is a gap (seconds).
const DEFAULT_GAP_S: f64 = 10.0;
/// Most words a reference lists.
const REF_WORDS: usize = 24;

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn comp_name(project: &Project, cid: ItemId) -> String {
    project.item(cid).map(|i| i.name.clone()).unwrap_or_default()
}

fn the_map(s: &Session, cid: ItemId, cmd: &str) -> Result<Arc<SpeechMap>> {
    s.project.comp(cid).and_then(|c| c.speech.clone()).ok_or_else(|| bad(cmd, "the comp has no words yet: speech.transcribe {layer} the voice layer first"))
}

fn the_script<'a>(map: &'a SpeechMap, cmd: &str) -> Result<&'a Script> {
    map.script.as_ref().ok_or_else(|| bad(cmd, "the comp has no script: script.load {path} first"))
}

/// Where a peak lands on a word (its first stressed syllable's accent), else its start (comp s).
fn peak_of(w: &SpeechWord, fps: f64) -> (f64, bool) {
    match w.stressed() {
        Some((_, syl)) => (lint::accent_time(syl, fps, lint::DEFAULT_ACCENT_LEAD), true),
        None => (w.start.seconds(), false),
    }
}

// ---------------------------------------------------------------- script.load

fn load(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "script.load";
    let cid = comp_id(s, p)?;
    let (name, text) = match (str_p(p, "path"), str_p(p, "text")) {
        (Some(path), None) => {
            let bytes = s.services.read_file(path).map_err(|e| bad(C, format!("can't read {path}: {e}")))?;
            if bytes.starts_with(b"PK") {
                // TODO(batch1-verify): .docx needs a zip reader (no new dependencies in batch 1).
                return Err(bad(C, format!("{path} looks like a Word or zip file: save it as plain text (.txt) and load that")));
            }
            if bytes.len() > MAX_SCRIPT_BYTES {
                return Err(bad(C, format!("{path} is over {} MB of text", MAX_SCRIPT_BYTES >> 20)));
            }
            let file = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
            (str_p(p, "name").map(str::to_string).unwrap_or(file), String::from_utf8_lossy(&bytes).to_string())
        }
        (None, Some(text)) => (str_p(p, "name").unwrap_or("Script").to_string(), text.chars().take(MAX_SCRIPT_BYTES).collect()),
        _ => return Err(bad(C, "give `path` (a .txt file) or `text`, not both")),
    };
    let script = Script::from_text(&name, &text);
    if script.lines.is_empty() {
        return Err(bad(C, "the script has no lines"));
    }
    let old = s.project.comp(cid).ok_or(EngineError::NoComp)?.speech.clone();
    let mut map = old.as_deref().cloned().unwrap_or_default();
    map.script = Some(script);
    let map = Arc::new(map);
    s.edit("Load Script", None, |proj, _| {
        proj.comp_mut(cid).ok_or(EngineError::NoComp)?.speech = Some(map);
        Ok(())
    })?;
    // What it lined up to.
    get(s, &json!({"comp": cid.0, "summary": true}))
}

// ---------------------------------------------------------------- script.get

/// Comp times where something animates: keys and in-points of every layer but the voice and
/// sound-only layers (sorted).
fn animation_times(project: &Project, comp: &Comp, voice: Option<LayerId>) -> Vec<f64> {
    let mut out = vec![];
    for l in comp.layers.iter() {
        if Some(l.id) == voice || sound_only(project, &l.source) {
            continue;
        }
        out.push(l.in_point.seconds());
        l.props.walk("", &mut |_, prop| {
            for k in &prop.keys {
                out.push(l.comp_time(k.time).seconds());
            }
        });
        if out.len() > 200_000 {
            break;
        }
    }
    out.sort_by(f64::total_cmp);
    out
}

fn sound_only(project: &Project, src: &LayerSource) -> bool {
    match src {
        LayerSource::Footage { item } => {
            matches!(project.item(*item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if f.kind == FootageKind::Audio || !f.has_video)
        }
        _ => false,
    }
}

/// Stretches of `a..b` longer than `min` seconds with no animation time inside.
fn gaps(times: &[f64], a: f64, b: f64, min: f64) -> Vec<(f64, f64)> {
    let mut out = vec![];
    let mut last = a;
    for &t in times.iter().filter(|t| **t > a && **t < b) {
        if t - last > min {
            out.push((last, t));
        }
        last = t;
    }
    if b - last > min {
        out.push((last, b));
    }
    out
}

/// The layers covering `a..b` (comp s): every layer but the voice and sound-only ones.
fn coverage(project: &Project, comp: &Comp, voice: Option<LayerId>, a: f64, b: f64) -> Vec<Value> {
    comp.layers
        .iter()
        .filter(|l| Some(l.id) != voice && !sound_only(project, &l.source))
        .filter(|l| l.in_point.seconds() < b && l.out_point.seconds() > a)
        .take(50)
        .map(|l| json!({"layer": l.id.0, "name": l.name}))
        .collect()
}

fn line_json(i: usize, line: &AlignedLine, text: &str, words: &[SpeechWord], emph: &[f32], fps: f64) -> Value {
    let frame = |t: Tick| (t.seconds() * fps).round() as i64;
    let toks: Vec<Value> = line
        .toks
        .iter()
        .map(|t| match t {
            LineTok::Script { text, said: Some(w) } => {
                let mut o = json!({"text": text, "said": w});
                if let Some(x) = words.get(*w) {
                    o["frame"] = json!(frame(x.start));
                    o["emphasis"] = json!(emph.get(*w).copied().unwrap_or(0.0));
                    if x.stressed().is_some() {
                        o["stress"] = json!((peak_of(x, fps).0 * fps).round() as i64);
                    }
                }
                o
            }
            LineTok::Script { text, said: None } => json!({"text": text, "notSaid": true}),
            LineTok::AdLib { word } => {
                let w = words.get(*word);
                json!({"text": w.map(|w| w.word.as_str()).unwrap_or(""), "adlib": true, "said": word, "frame": w.map(|w| frame(w.start))})
            }
        })
        .collect();
    let mut o = json!({
        "line": i + 1,
        "text": text,
        "cut": line.cut(),
        "notSaid": line.not_said(),
        "adlibs": line.adlibs(),
        "tokens": toks,
    });
    if let Some((a, b)) = line.span(words) {
        o["start"] = json!(r3(a.seconds()));
        o["end"] = json!(r3(b.seconds()));
        o["frames"] = json!([frame(a), frame(b)]);
        o["firstWord"] = json!(line.first);
        o["lastWord"] = json!(line.last);
    }
    o
}

/// One said word for `script.get`'s `words`: index, text, times, frame, stress and emphasis.
fn word_json(i: usize, w: &SpeechWord, emph: &[f32], fps: f64) -> Value {
    let e = emph.get(i).copied().unwrap_or(0.0);
    let mut o = json!({
        "index": i,
        "text": w.word,
        "start": r3(w.start.seconds()),
        "end": r3(w.end.seconds()),
        "frame": (w.start.seconds() * fps).round() as i64,
        "emphasis": e,
        "strong": e > 0.5,
        "stressed": w.stressed().is_some(),
    });
    if w.stressed().is_some() {
        o["stress"] = json!((peak_of(w, fps).0 * fps).round() as i64);
    }
    o
}

/// SX7: chapter, scene path, gap and words of a line spanning words `a..=b`.
fn annotate(o: &mut Value, finder: &mut SceneFinder, chs: &[Chapter], words: &[SpeechWord], (a, b): (usize, usize), emph: &[f32], fps: f64) {
    let Some(ws) = words.get(a..=b) else { return };
    let start = ws.first().map_or(0.0, |w| w.start.seconds());
    let end = ws.last().map_or(start, |w| w.end.seconds());
    match script_lines::chapter_at(chs, start).and_then(|i| chs.get(i).map(|c| (i, c))) {
        Some((i, c)) => {
            o["chapter"] = json!(c.name);
            o["chapterIndex"] = json!(i);
        }
        None => {
            o["chapter"] = Value::Null;
            o["chapterIndex"] = Value::Null;
        }
    }
    let ls = script_lines::line_scenes(finder, ws);
    let scene = |h: &SceneHit| {
        json!({
            "path": h.path,
            "layers": h.layers.iter().zip(&h.path).map(|((c, l), n)| json!({"comp": c.0, "layer": l.0, "name": n})).collect::<Vec<_>>(),
        })
    };
    o["scene"] = ls.scenes.first().map(scene).unwrap_or(Value::Null);
    o["scenePath"] = json!(ls.scenes.first().map(|h| h.path.clone()).unwrap_or_default());
    o["scenes"] = Value::Array(ls.scenes.iter().map(scene).collect());
    let gap = ls.gap();
    o["gap"] = json!(gap);
    o["gapSeconds"] = json!(if gap { r3((end - start).max(0.0)) } else { 0.0 });
    o["coveredSeconds"] = json!(r3(ls.covered_s));
    o["bare"] = ls.bare.as_ref().map(scene).unwrap_or(Value::Null);
    o["words"] = Value::Array(ws.iter().enumerate().map(|(k, w)| word_json(a + k, w, emph, fps)).collect());
}

/// The layers with sound in a comp: what "Transcribe voice" (`speech.transcribe {layer}`) offers.
fn sound_layers(project: &Project, comp: &Comp) -> Vec<Value> {
    comp.layers
        .iter()
        .filter(
            |l| matches!(l.source, LayerSource::Footage { item } if matches!(project.item(item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if f.has_audio)),
        )
        .take(50)
        .map(|l| json!({"layer": l.id.0, "name": l.name}))
        .collect()
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fps = comp.frame_rate.as_f64();
    let chs = script_lines::chapters(comp);
    let chapters_json: Vec<Value> = chs
        .iter()
        .enumerate()
        .map(|(i, c)| json!({"index": i, "name": c.name, "start": r3(c.start), "end": r3(c.end), "frame": (c.start * fps).round() as i64}))
        .collect();
    let map = comp.speech.clone();
    let has_script = map.as_ref().is_some_and(|m| m.script.is_some());
    let want_speech = str_p(p, "source") == Some("speech");
    let has_words = map.as_ref().is_some_and(|m| !m.words.is_empty());
    let Some(map) = map.filter(|_| (has_script && !want_speech) || has_words) else {
        // No words yet: Ghostex shows one "Transcribe voice" button (speech.transcribe {layer}).
        return Ok(json!({
            "comp": cid.0,
            "compName": comp_name(&s.project, cid),
            "source": "none",
            "needsTranscribe": true,
            "transcribe": sound_layers(&s.project, comp),
            "fps": fps,
            "chapters": chapters_json,
            "lines": [],
        }));
    };
    let min_gap = f_p(p, "gapSeconds").filter(|g| g.is_finite() && *g > 0.0).unwrap_or(DEFAULT_GAP_S);
    let pause = f_p(p, "pauseSeconds").filter(|g| g.is_finite() && *g >= 0.0).unwrap_or(script_lines::DEFAULT_PAUSE_S);
    let max_words = p.get("maxWords").and_then(Value::as_u64).map(|n| (n as usize).clamp(1, 200)).unwrap_or(script_lines::DEFAULT_MAX_WORDS);
    let emph = emphasis(&map.words);
    let voice = map.layer;
    // Only where the voice is heard in the comp: a 575 s voice-over in a 12 s comp reported
    // 12 s..573 s as a gap.
    let (va, vb) = (map.words.first().map_or(0.0, |w| w.start.seconds()), map.words.last().map_or(0.0, |w| w.end.seconds()));
    let (va, vb) = (va.max(0.0), vb.min(comp.duration.seconds()).max(va.max(0.0)));
    let gap_list = gaps(&animation_times(&s.project, comp, voice), va, vb, min_gap);
    let summary = b_p(p, "summary").unwrap_or(false);
    let r = super::layout_cmds::renderer(s, RenderOpts::default());
    let mut finder = SceneFinder::new(&r, &s.project, cid, voice);
    let mut out_lines = vec![];
    let script = map.script.as_ref().filter(|_| !want_speech);
    let (source, name, said, cut, not_said, adlibs) = match script {
        Some(script) => {
            let lines = align_script(script, &map.words);
            for (i, (al, sl)) in lines.iter().zip(&script.lines).enumerate() {
                let mut o = line_json(i, al, &sl.text, &map.words, &emph, fps);
                if let Some((a, b)) = al.span(&map.words) {
                    let (a, b) = (a.seconds(), b.seconds());
                    o["coverage"] = Value::Array(coverage(&s.project, comp, voice, a, b));
                    o["quiet"] = json!(gap_list.iter().any(|(x, y)| *x < b && *y > a));
                }
                if let (Some(a), Some(b)) = (al.first, al.last) {
                    annotate(&mut o, &mut finder, &chs, &map.words, (a.min(b), a.max(b)), &emph, fps);
                }
                out_lines.push(o);
            }
            (
                "script",
                script.name.clone(),
                lines.iter().filter(|l| !l.cut()).count(),
                lines.iter().filter(|l| l.cut()).count(),
                lines.iter().map(AlignedLine::not_said).sum::<usize>(),
                lines.iter().map(AlignedLine::adlibs).sum::<usize>(),
            )
        }
        None => {
            let ranges = script_lines::speech_lines(&map.words, pause, max_words);
            for (i, (a, b)) in ranges.iter().copied().enumerate() {
                let Some(ws) = map.words.get(a..=b) else { continue };
                let text = ws.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ");
                let (ta, tb) = (ws.first().map_or(0.0, |w| w.start.seconds()), ws.last().map_or(0.0, |w| w.end.seconds()));
                // The same tokens as a script line whose every word is said (the egui panel reads them).
                let toks: Vec<Value> = ws
                    .iter()
                    .enumerate()
                    .map(|(k, w)| {
                        let mut t = json!({"text": w.word, "said": a + k, "frame": (w.start.seconds() * fps).round() as i64, "emphasis": emph.get(a + k).copied().unwrap_or(0.0)});
                        if w.stressed().is_some() {
                            t["stress"] = json!((peak_of(w, fps).0 * fps).round() as i64);
                        }
                        t
                    })
                    .collect();
                let mut o = json!({
                    "line": i + 1,
                    "text": text,
                    "cut": false,
                    "notSaid": 0,
                    "adlibs": 0,
                    "tokens": toks,
                    "start": r3(ta),
                    "end": r3(tb),
                    "frames": [(ta * fps).round() as i64, (tb * fps).round() as i64],
                    "firstWord": a,
                    "lastWord": b,
                    "coverage": coverage(&s.project, comp, voice, ta, tb),
                    "quiet": gap_list.iter().any(|(x, y)| *x < tb && *y > ta),
                });
                annotate(&mut o, &mut finder, &chs, &map.words, (a, b), &emph, fps);
                out_lines.push(o);
            }
            ("speech", "Voice (speech map)".to_string(), ranges.len(), 0, 0, 0)
        }
    };
    let scene_gaps: Vec<Value> = out_lines
        .iter()
        .filter(|l| l.get("gap").and_then(Value::as_bool).unwrap_or(false))
        .map(|l| json!({"line": l["line"], "start": l["start"], "end": l["end"], "frames": l["frames"], "seconds": l["gapSeconds"], "chapter": l["chapter"]}))
        .collect();
    if summary {
        for o in out_lines.iter_mut().filter_map(Value::as_object_mut) {
            o.remove("tokens");
            o.remove("words");
        }
    }
    Ok(json!({
        "comp": cid.0,
        "compName": comp_name(&s.project, cid),
        "source": source,
        "name": name,
        "engine": map.engine,
        "voiceLayer": voice.map(|l| l.0),
        "fps": fps,
        "pauseSeconds": pause,
        "maxWords": max_words,
        "chapters": chapters_json,
        "lines": out_lines,
        "said": said,
        "cut": cut,
        "notSaid": not_said,
        "adlibs": adlibs,
        "sceneGaps": scene_gaps,
        "gapSeconds": min_gap,
        "gaps": gap_list.iter().map(|(a, b)| json!({"start": r3(*a), "end": r3(*b), "frames": [(a * fps).round() as i64, (b * fps).round() as i64]})).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------- script.copyRef

/// Word range `from..=to` of the map, checked.
fn range_p(p: &Value, map: &SpeechMap, cmd: &str) -> Result<(usize, usize)> {
    let n = map.words.len();
    let idx = |k: &str| p.get(k).and_then(Value::as_u64).map(|v| v as usize);
    let from = idx("from")
        .or_else(|| idx("index"))
        .ok_or_else(|| bad(cmd, "give `index` (a word of the speech map: layer.words / script.get list them) or `line`"))?;
    let to = idx("to").unwrap_or(from);
    let (a, b) = (from.min(to), from.max(to));
    if b >= n {
        return Err(bad(cmd, format!("word {b} is past the speech map's {n} words")));
    }
    Ok((a, b))
}

fn copy_ref(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "script.copyRef";
    let cid = comp_id(s, p)?;
    let map = the_map(s, cid, C)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fps = comp.frame_rate.as_f64();
    let (line_no, (a, b)) = match p.get("line").and_then(Value::as_u64) {
        Some(n) => {
            let script = the_script(&map, C)?;
            let lines = align_script(script, &map.words);
            let l = lines.get((n as usize).saturating_sub(1)).ok_or_else(|| bad(C, format!("the script has no line {n}")))?;
            match (l.first, l.last) {
                (Some(a), Some(b)) => (Some(n as usize), (a, b)),
                _ => return Err(bad(C, format!("line {n} isn't said in the cut"))),
            }
        }
        None => {
            let r = range_p(p, &map, C)?;
            // The script line the words are on, when there is a script.
            let n = map
                .script
                .as_ref()
                .and_then(|sc| align_script(sc, &map.words).iter().position(|l| l.first.is_some_and(|f| f <= r.1) && l.last.is_some_and(|x| x >= r.0)));
            (n.map(|i| i + 1), r)
        }
    };
    let ws: Vec<&SpeechWord> = map.words.get(a..=b).map(|w| w.iter().collect()).unwrap_or_default();
    let frame = |t: f64| (t * fps).round() as i64;
    let quote: Vec<String> = ws.iter().map(|w| if w.stressed().is_some() && ws.len() > 1 { w.word.to_uppercase() } else { w.word.clone() }).collect();
    let (ta, tb) = (ws.first().map_or(0.0, |w| w.start.seconds()), ws.last().map_or(0.0, |w| w.end.seconds()));
    let list: Vec<String> = ws
        .iter()
        .take(REF_WORDS)
        .map(|w| match w.stressed() {
            Some(_) => format!("{} f{} (stress f{})", w.word, frame(w.start.seconds()), frame(peak_of(w, fps).0)),
            None => format!("{} f{}", w.word, frame(w.start.seconds())),
        })
        .collect();
    let more = if ws.len() > REF_WORDS { format!(", … {} more", ws.len() - REF_WORDS) } else { String::new() };
    let mut text = comp_name(&s.project, cid);
    if let Some(n) = line_no {
        text.push_str(&format!(" · script line {n}"));
    }
    text.push_str(&format!(" \"{}\" · f{}–f{} · words {a}–{b}: {}{more}", quote.join(" "), frame(ta), frame(tb), list.join(", ")));
    let out = json!({"text": text, "comp": cid.0, "line": line_no, "from": a, "to": b, "frames": [frame(ta), frame(tb)]});
    frontend(s, C, &json!({"text": text}))?;
    Ok(out)
}

// ---------------------------------------------------------------- script.board

fn board(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "script.board";
    let cid = comp_id(s, p)?;
    let map = the_map(s, cid, C)?;
    let script = the_script(&map, C)?;
    let fps = s.project.comp(cid).ok_or(EngineError::NoComp)?.frame_rate.as_f64();
    let picked: Vec<usize> =
        p.get("lines").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as usize).collect()).unwrap_or_default();
    if picked.is_empty() {
        return Err(bad(C, "give `lines` (1-based script line numbers)"));
    }
    let lines = align_script(script, &map.words);
    let emph = emphasis(&map.words);
    let frame = |t: f64| (t * fps).round() as i64;
    let mut frames = vec![];
    for n in picked.iter().take(100) {
        let Some(l) = lines.get(n.saturating_sub(1)) else { return Err(bad(C, format!("the script has no line {n}"))) };
        let (Some(a), Some(b)) = (l.first, l.last) else { continue };
        let Some(ws) = map.words.get(a..=b) else { continue };
        // Three beats: the words before the line's strongest word, that word (the peak), the rest.
        let peak = (a..=b).max_by(|x, y| emph.get(*x).unwrap_or(&0.0).total_cmp(emph.get(*y).unwrap_or(&0.0))).unwrap_or(a);
        let join = |r: std::ops::Range<usize>| map.words.get(r).map(|w| w.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        if peak > a {
            frames.push(json!({"line": n, "text": join(a..peak), "frame": frame(ws.first().map_or(0.0, |w| w.start.seconds()))}));
        }
        if let Some(w) = map.words.get(peak) {
            frames.push(json!({"line": n, "text": w.word.to_uppercase(), "peak": w.word, "frame": frame(peak_of(w, fps).0), "word": peak}));
        }
        if peak < b {
            frames.push(json!({"line": n, "text": join(peak + 1..b + 1), "frame": frame(map.words.get(peak + 1).map_or(0.0, |w| w.start.seconds()))}));
        }
    }
    if frames.is_empty() {
        return Err(bad(C, "none of those lines is said in the cut"));
    }
    let board = json!({"name": format!("{} · {}", comp_name(&s.project, cid), script.name), "comp": cid.0, "lines": picked, "frames": frames});
    if let Some(path) = str_p(p, "path") {
        let text = serde_json::to_vec_pretty(&board).map_err(|e| bad(C, e.to_string()))?;
        s.services.write_file(path, &text).map_err(|e| bad(C, format!("can't write {path}: {e}")))?;
    }
    if b_p(p, "send").unwrap_or(true) {
        frontend(s, C, &board)?;
    }
    Ok(board)
}

// ---------------------------------------------------------------- word.apply

/// What `word.apply` builds.
const ITEMS: &[&str] = &["pop", "caption", "pill", "highlight", "zoom", "sfx", "preset", "copy"];

fn word_apply(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "word.apply";
    let item = str_p(p, "item").ok_or_else(|| bad(C, format!("missing `item` ({})", ITEMS.join(", "))))?.to_ascii_lowercase();
    if !ITEMS.contains(&item.as_str()) {
        return Err(bad(C, format!("unknown item {item:?} ({})", ITEMS.join(", "))));
    }
    let cid = comp_id(s, p)?;
    let map = the_map(s, cid, C)?;
    let (a, b) = match str_p(p, "word") {
        // A word by text: its first occurrence (speech.find gives the others' indices).
        Some(w) if p.get("index").is_none() && p.get("from").is_none() => {
            let q = effectcraft_project::speech::normalize(w);
            let i = map
                .words
                .iter()
                .position(|x| effectcraft_project::speech::normalize(&x.word) == q)
                .ok_or_else(|| bad(C, format!("{w:?} isn't said in this comp")))?;
            (i, i)
        }
        _ => range_p(p, &map, C)?,
    };
    if item == "copy" {
        return copy_ref(s, &json!({"comp": cid.0, "from": a, "to": b}));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = comp.frame_rate;
    let fps = fr.as_f64();
    let fd = 1.0 / fps.max(1.0);
    let (w, wl) = match (map.words.get(a), map.words.get(b)) {
        (Some(x), Some(y)) => (x.clone(), y.clone()),
        _ => return Err(bad(C, "no such word")),
    };
    let phrase: String = map.words.get(a..=b).map(|ws| ws.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    // The peak: the phrase's first stressed syllable, else its first word's start.
    let stressed = (a..=b).find(|i| map.words.get(*i).is_some_and(|x| x.stressed().is_some()));
    let peak_word = stressed.and_then(|i| map.words.get(i)).unwrap_or(&w).clone();
    let (peak, on_stress) = peak_of(&peak_word, fps);
    let start = w.start.seconds();
    let end = wl.end.seconds();
    let comp_end = comp.duration.seconds();
    let (cw, ch) = (comp.width as f64, comp.height as f64);
    let label = format!("{} on “{}”", item_label(&item), phrase.chars().take(40).collect::<String>());
    let snap = s.project.clone();
    let r = build(s, p, &Build { cid, item: &item, phrase: &phrase, start, end, peak, on_stress, stressed, fd, comp_end, cw, ch });
    super::batch::fold(s, &snap, &label);
    match r {
        Ok(mut out) => {
            out["item"] = json!(item);
            out["words"] = json!([a, b]);
            out["text"] = json!(phrase);
            out["peakFrame"] = json!((peak * fps).round() as i64);
            out["onStress"] = json!(on_stress);
            Ok(out)
        }
        Err(e) => {
            // Leave nothing half built.
            if !Arc::ptr_eq(&s.project, &snap) {
                let _ = s.undo();
            }
            Err(e)
        }
    }
}

fn item_label(item: &str) -> &'static str {
    match item {
        "pop" => "Pop text",
        "caption" => "Caption",
        "pill" => "Pill",
        "highlight" => "Highlight",
        "zoom" => "Zoom punch",
        "sfx" => "SFX",
        "preset" => "Preset",
        _ => "Copy for Claude",
    }
}

struct Build<'a> {
    cid: ItemId,
    item: &'a str,
    phrase: &'a str,
    /// Comp seconds.
    start: f64,
    end: f64,
    peak: f64,
    on_stress: bool,
    /// The stressed word (map index), for keys.alignPeak's `speech` target.
    stressed: Option<usize>,
    /// One frame (s).
    fd: f64,
    comp_end: f64,
    cw: f64,
    ch: f64,
}

/// The peak target for keys.alignPeak: the stressed syllable, else the time.
fn target(b: &Build) -> Value {
    match (b.on_stress, b.stressed) {
        (true, Some(i)) => json!({"speech": {"index": i}}),
        _ => json!({"time": b.peak}),
    }
}

fn add_key(s: &mut Session, b: &Build, layer: u64, path: &str, t: f64, value: Value) -> Result<Value> {
    s.execute("prop.addKey", json!({"comp": b.cid.0, "layer": layer, "path": path, "time": t.max(0.0), "timeBase": "comp", "value": value}))
}

/// Land the move of `path` (its first two keys) on the peak with one of Devin's graphs.
fn align(s: &mut Session, b: &Build, layer: u64, path: &str, graph: &str) -> Result<Value> {
    let mut q = json!({"comp": b.cid.0, "layer": layer, "path": path, "key": 0, "mode": "shift", "graph": graph});
    if let (Some(o), Value::Object(t)) = (q.as_object_mut(), target(b)) {
        o.extend(t);
    }
    s.execute("keys.alignPeak", q)
}

/// Set a new layer's in/out (comp s), its start at its in-point.
fn timing(s: &mut Session, b: &Build, layer: u64, a: f64, z: f64) -> Result<()> {
    let (a, z) = (a.max(0.0), z.min(b.comp_end).max(a + b.fd));
    let cid = b.cid;
    s.edit("Word Item Timing", None, |proj, _| {
        let l = super::layer_mut(proj, cid, LayerId(layer))?;
        l.start_time = Tick::from_seconds_f64(a);
        l.in_point = Tick::from_seconds_f64(a);
        l.out_point = Tick::from_seconds_f64(z);
        Ok(())
    })
}

fn build(s: &mut Session, p: &Value, b: &Build) -> Result<Value> {
    let f = b.fd;
    let new_text = |s: &mut Session, text: String, name: String, extra: Value| -> Result<u64> {
        let mut q = json!({"comp": b.cid.0, "text": text, "name": name});
        if let (Some(o), Value::Object(e)) = (q.as_object_mut(), extra) {
            o.extend(e);
        }
        s.execute("layer.newText", q)?.get("layer").and_then(Value::as_u64).ok_or_else(|| bad("word.apply", "layer.newText returned no layer"))
    };
    match b.item {
        "pop" | "pill" => {
            // TODO(batch1-verify): the Pill finish (rounded plate, ML bounce on scale, Gradient Ramp +
            // Deep Glow) needs lane C's card.style / Motion Library style; this is the plain text pop.
            let name = format!("{} · {}", b.phrase.to_uppercase(), if b.item == "pill" { "Pill" } else { "Pop" });
            let l = new_text(s, b.phrase.to_uppercase(), name, json!({"size": (b.ch * 0.08).round(), "position": [b.cw / 2.0, b.ch * 0.45]}))?;
            timing(s, b, l, b.peak - 6.0 * f, b.end + 12.0 * f)?;
            add_key(s, b, l, "transform/scale", b.peak - 4.0 * f, json!([0.0, 0.0, 100.0]))?;
            add_key(s, b, l, "transform/scale", b.peak + 4.0 * f, json!([100.0, 100.0, 100.0]))?;
            let a = align(s, b, l, "transform/scale", if b.item == "pill" { "quick-launch" } else { "fast-in" })?;
            Ok(json!({"layer": l, "align": a}))
        }
        "caption" | "highlight" => {
            // TODO(batch1-verify): apply the "Caption glow + double shadow" style family (lane C, CX3).
            let mut extra = json!({"size": (b.ch * 0.05).round(), "position": [b.cw / 2.0, b.ch * 0.8]});
            if b.item == "highlight" {
                extra["fill"] = json!("#ffd84a");
            }
            let name = format!("{} · {}", b.phrase, if b.item == "highlight" { "Highlight" } else { "Caption" });
            let l = new_text(s, b.phrase.to_string(), name, extra)?;
            timing(s, b, l, b.start - 3.0 * f, b.end + 6.0 * f)?;
            add_key(s, b, l, "transform/opacity", b.start - 3.0 * f, json!(0.0))?;
            add_key(s, b, l, "transform/opacity", b.start, json!(100.0))?;
            if b.item == "highlight" {
                let a = align(s, b, l, "transform/opacity", "fast-in")?;
                return Ok(json!({"layer": l, "align": a}));
            }
            Ok(json!({"layer": l}))
        }
        "zoom" => {
            // The layers to punch in on: `layers`, else the selection (the voice layer never).
            let ids: Vec<u64> = match p.get("layers").and_then(Value::as_array) {
                Some(a) => a.iter().filter_map(Value::as_u64).collect(),
                None => s.state.selected_layers.iter().map(|l| l.0).collect(),
            };
            let voice = s.project.comp(b.cid).and_then(|c| c.speech.as_ref()).and_then(|m| m.layer).map(|l| l.0);
            let ids: Vec<u64> = ids.into_iter().filter(|i| Some(*i) != voice).take(32).collect();
            if ids.is_empty() {
                return Err(bad("word.apply", "Zoom punch: select the layer(s) to punch in on (or pass `layers`)"));
            }
            let mut out = vec![];
            for l in ids {
                // TODO(batch1-verify): keys from the layer's current scale instead of 100 %.
                add_key(s, b, l, "transform/scale", b.peak - 4.0 * f, json!([100.0, 100.0, 100.0]))?;
                add_key(s, b, l, "transform/scale", b.peak + 2.0 * f, json!([108.0, 108.0, 100.0]))?;
                add_key(s, b, l, "transform/scale", b.peak + 12.0 * f, json!([100.0, 100.0, 100.0]))?;
                out.push(json!({"layer": l, "align": align(s, b, l, "transform/scale", "fast-in")?}));
            }
            Ok(json!({"layers": out}))
        }
        "sfx" => {
            // TODO(batch1-verify): place an SFX clip from the Motion Library; a comp marker marks the spot.
            let r = s.execute("comp.addMarker", json!({"comp": b.cid.0, "time": b.peak, "comment": format!("SFX: whoosh on “{}”", b.phrase)}))?;
            Ok(json!({"marker": r}))
        }
        "preset" => {
            let id = match (str_p(p, "id"), str_p(p, "query")) {
                (Some(id), _) => id.to_string(),
                (None, Some(q)) => {
                    let r = s.execute("library.search", json!({"query": q, "limit": 1}))?;
                    // library.search → {total, entries: [{id, name, …}]} (checked in commands/library.rs).
                    r.get("entries")
                        .and_then(|e| e.get(0))
                        .and_then(|e| e.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| bad("word.apply", format!("no Motion Library entry matches {q:?}")))?
                }
                (None, None) => return Err(bad("word.apply", "Preset: give `id` (a Motion Library card) or `query`")),
            };
            let at = (b.start - 2.0 * f).max(0.0);
            let r = s.execute("library.apply", json!({"id": id, "comp": b.cid.0, "time": at}))?;
            // Adapt to the phrase: stretch the nested card to the phrase's length.
            if b_p(p, "adapt").unwrap_or(true)
                && let Some(l) = r.get("layer").and_then(Value::as_u64)
            {
                let want = (b.end - b.start + 4.0 * f).max(f);
                let cid = b.cid;
                // library.apply nests the card in `comp` at `time` and returns that `layer` (checked).
                s.edit("Adapt Preset to Phrase", None, |proj, _| {
                    let l = super::layer_mut(proj, cid, LayerId(l))?;
                    let len = (l.out_point - l.in_point).seconds();
                    if len > 1e-6 && l.stretch.abs() > 1e-9 {
                        l.stretch = (l.stretch * want / len).clamp(10.0, 1000.0);
                        l.out_point = l.in_point + Tick::from_seconds_f64(want);
                    }
                    Ok(())
                })?;
            }
            Ok(json!({"preset": id, "apply": r}))
        }
        other => Err(bad("word.apply", format!("unknown item {other:?}"))),
    }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "script.load",
            "Load Script",
            [],
            None,
            "{comp?, path? (a plain-text script, one line per line) | text?, name?} → as script.get {summary: true}. Stored with the comp (undoable) and lined up with its words",
            has_comp,
            load
        ),
        query!(
            "script.get",
            "Script",
            "{comp?, source? (\"speech\": lines from the speech map even when a .txt is loaded), pauseSeconds? (default 0.32: a longer pause ends a speech line), maxWords? (default 11), gapSeconds? (default 10: a stretch this long with no keyframes marks a line `quiet`), summary? (no tokens / words)} → {comp, compName, source: script | speech | none, name, engine, voiceLayer, fps, chapters: [{index, name, start, end, frame}] (the comp markers), said, cut, notSaid, adlibs, sceneGaps: [{line, start, end, frames, seconds, chapter}], gaps: [{start, end, frames}], lines: [{line (1-based), text, cut, notSaid, adlibs, start?, end?, frames?, firstWord?, lastWord? (speech-map indices for word.apply / script.copyRef), chapter, chapterIndex, scene: {path, layers: [{comp, layer, name}]} | null, scenePath, scenes (every scene during the line), gap (no scene covers it), gapSeconds, coveredSeconds, bare (where a gap is), quiet, coverage: [{layer, name}], words: [{index, text, start, end, frame, stressed, stress? (peak frame), emphasis, strong}], tokens}]}. source none (no words): {needsTranscribe, transcribe: [{layer, name}]} for speech.transcribe. docs/control-protocol.md, \"Script\"",
            get
        ),
        cmd!(
            "script.copyRef",
            "Copy for Claude",
            [],
            None,
            "{comp?, line? (1-based) | index?|from?, to? (speech-map word indices)} → {text, comp, line, from, to, frames}: a short reference (comp · script line · \"words\" · frames · each word's frame and stress); the desktop app puts it on the clipboard",
            has_comp,
            copy_ref
        ),
        cmd!(
            "script.board",
            "Board This (FigCraft)",
            [],
            None,
            "{comp?, lines: [1-based line numbers], path? (also write the board JSON here), send? (default true: the desktop app sends it to FigCraft on 127.0.0.1:FIGCRAFT_CONTROL_PORT, default 7979)} → {name, comp, lines, frames: [{line, text, frame, peak?, word?}]} (per line: the words before its strongest word, that word, the rest)",
            has_comp,
            board
        ),
        cmd!(
            "word.apply",
            "Apply to Word",
            [],
            None,
            "{comp?, index? | from?, to? (speech-map word indices: a word or a phrase) | word? (its first occurrence), item: pop | caption | pill | highlight | zoom | sfx | preset | copy, layers? (zoom: the layers to punch in on; default the selection), id? | query? (preset: a Motion Library card, or the first that matches, e.g. \"Views Counter\"), adapt? (preset: stretch it to the phrase, default true)} → {item, words, text, peakFrame, onStress, layer? …}. The item's speed peak lands on the phrase's first stressed syllable (keys.alignPeak, Devin's graphs). One undo step",
            has_comp,
            word_apply
        ),
    ]
}
