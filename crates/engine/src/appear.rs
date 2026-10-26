//! When each layer should appear, from the word it shows (A/B round 3, "appear timing"):
//! [`plan`] matches a comp's text layers (their text) and other layers whose name says a spoken
//! word to that word in the comp's speech map, measures when the layer appears now (the first
//! frame it is seen, and its entrance's speed peak) and says where it should: Elias's rule is
//! that an entrance's peak lands on the word's stressed syllable, one frame before its vowel
//! ([`crate::lint::accent_time`]), so a layer that pops in lands on its word; a layer that just
//! cuts in starts on the word. `speech.appearPlan` lists it with a fix per layer; the `off-word`
//! lint flags layers more than [`OFF_WORD_FRAMES`] off.

use effectcraft_keyframe::Value as KValue;
use effectcraft_project::speech::{SpeechMap, SpeechWord, normalize};
use effectcraft_project::{Comp, Layer, LayerId, LayerSource};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::lint;

/// A layer more than this many frames from its word is `off-word`.
pub const OFF_WORD_FRAMES: f64 = 3.0;
/// How long after a layer appears its entrance's speed peak may come (seconds).
const ENTRANCE_WINDOW: f64 = 1.5;
/// Most layers one plan looks at.
const MAX_LAYERS: usize = 400;

/// Words that never identify a layer on their own.
const STOP: &[&str] = &[
    "a", "an", "the", "of", "to", "and", "or", "is", "it", "its", "it's", "i", "in", "on", "at", "for", "that", "this", "with", "you", "your", "be", "are",
    "was", "so", "but", "my", "me", "we", "as", "by", "from", "do", "not", "no", "s", "t",
];

/// Spoken forms of numbers written as digits ("1" → "one").
fn number_word(t: &str) -> Option<&'static str> {
    Some(match t {
        "0" => "zero",
        "1" => "one",
        "2" => "two",
        "3" => "three",
        "4" => "four",
        "5" => "five",
        "6" => "six",
        "7" => "seven",
        "8" => "eight",
        "9" => "nine",
        "10" => "ten",
        "12" => "twelve",
        "20" => "twenty",
        "50" => "fifty",
        "100" => "hundred",
        "1000" => "thousand",
        _ => return None,
    })
}

fn same(a: &str, b: &str) -> bool {
    a == b || number_word(a) == Some(b) || number_word(b) == Some(a)
}

/// Lower-case words of a text ("COPY THAT SELLS" → copy, that, sells).
pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '/' || c == '·').map(normalize).filter(|w| !w.is_empty()).take(40).collect()
}

/// What a layer shows, in words: a text layer's text, else its name.
fn layer_text(l: &Layer) -> (String, &'static str) {
    if matches!(l.source, LayerSource::Text) {
        let p = l.props.prop("text/sourceText");
        let v = p.and_then(|p| p.keys.first().map(|k| &k.value).or(Some(&p.value)));
        if let Some(KValue::Text(d)) = v {
            return (d.text.chars().take(400).collect(), "text");
        }
    }
    (l.name.clone(), "name")
}

/// The best place `toks` is said in `words`: (first matched word's index, matched count, whether
/// the match starts at the text's first word). The longest run of words said in order wins;
/// equal runs go to the one nearest `near` (comp seconds). A lone stop word doesn't count.
fn find(toks: &[String], words: &[(usize, &SpeechWord)], near: f64) -> Option<(usize, usize, bool)> {
    let said: Vec<String> = words.iter().map(|(_, w)| normalize(&w.word)).collect();
    let mut best: Option<(usize, usize, bool, f64)> = None;
    for (j, t) in toks.iter().enumerate() {
        for (i, w) in said.iter().enumerate() {
            if !same(t, w) {
                continue;
            }
            let mut run = 1;
            while toks.get(j + run).zip(said.get(i + run)).is_some_and(|(a, b)| same(a, b)) {
                run += 1;
            }
            if run == 1 && STOP.contains(&t.as_str()) && toks.len() > 1 {
                continue;
            }
            let Some((at, _)) = words.get(i) else { continue };
            let dist = words.get(i).map_or(f64::MAX, |(_, w)| (w.start.seconds() - near).abs());
            let better = match best {
                None => true,
                Some((_, r, first, d)) => run > r || (run == r && (j == 0) && !first) || (run == r && (j == 0) == first && dist < d),
            };
            if better {
                best = Some((*at, run, j == 0, dist));
            }
        }
    }
    best.map(|(i, r, first, _)| (i, r, first))
}

/// When a layer is first seen, from `comp.motion`'s description of it (the same reading the A/B
/// scorer uses): the start of its first entrance (enter, fade-in, scale up from 0), else the end
/// of an off-screen span it starts in, else its in point. Comp seconds.
fn appear_of(m: &Value, dur: f64, fps: f64) -> f64 {
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let segs: Vec<&Value> = m.get("segments").and_then(Value::as_array).into_iter().flatten().filter(|s| s["kind"] != "hold").collect();
    let inp = f(m, "inPoint").unwrap_or(0.0);
    let outp = f(m, "outPoint").unwrap_or(dur);
    let entrance =
        |s: &&&Value| matches!(s["kind"].as_str(), Some("enter" | "fade-in")) || (s["kind"] == "scale" && s.get("from").and_then(Value::as_f64) == Some(0.0));
    if let Some(t) = segs.iter().filter(entrance).filter_map(|s| f(s, "start")).reduce(f64::min) {
        return t;
    }
    let off = m.get("offscreen").and_then(Value::as_array).and_then(|a| a.first());
    if let Some(o) = off
        && f(o, "start").is_some_and(|s| s <= inp + 0.5 / fps)
        && f(o, "end").is_some_and(|e| e < outp.min(dur) - 0.5 / fps)
    {
        return f(o, "end").unwrap_or(inp);
    }
    inp
}

/// A layer the caller names, with the word it shows when they say it.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub layer: LayerId,
    /// Words to find instead of the layer's own text / name.
    pub words: Option<String>,
    /// Which time they are said (1-based), when they are said more than once.
    pub occurrence: Option<usize>,
}

/// One layer's plan.
#[derive(Clone, Debug)]
pub struct Entry {
    pub layer: LayerId,
    pub name: String,
    /// What was matched: the layer's text, name, or the caller's words.
    pub shows: String,
    pub from: &'static str,
    /// Index into the map's words, the word, and how many words of `shows` were said in a row.
    pub word: Option<(usize, String, usize)>,
    pub word_start: f64,
    /// Where an entrance's peak should land (the stressed syllable's accent), comp seconds.
    pub accent: f64,
    /// When the layer is first seen now.
    pub appear: f64,
    /// Its entrance: the property and the speed peak (comp seconds).
    pub peak: Option<(String, String, f64)>,
    /// Frames to move the layer's entrance (+ later), and what that measured.
    pub off_by: f64,
    pub measured: &'static str,
    /// Why no word was found.
    pub why: Option<String>,
}

impl Entry {
    /// Confident enough to flag: the text's first word was found, or several words in a row, or
    /// the caller said which.
    pub fn sure(&self) -> bool {
        self.from == "given" || self.word.as_ref().is_some_and(|(_, w, run)| *run >= 2 || (self.from == "text" && !STOP.contains(&normalize(w).as_str())))
    }
}

/// Layers worth planning when the caller names none: visible text layers, and layers whose name
/// has a spoken word that isn't a stop word; never the voice layer, guides, adjustment layers,
/// cameras, lights, nulls or audio.
fn candidates(comp: &Comp, map: &SpeechMap) -> Vec<LayerId> {
    let said: std::collections::HashSet<String> = map.words.iter().map(|w| normalize(&w.word)).collect();
    comp.layers
        .iter()
        .filter(|l| Some(l.id) != map.layer && l.switches.video && !l.switches.guide && !l.switches.adjustment)
        .filter(|l| match l.source {
            LayerSource::Text => true,
            LayerSource::Shape | LayerSource::Comp { .. } | LayerSource::Solid { .. } | LayerSource::Footage { .. } => {
                tokens(&l.name).iter().any(|t| t.chars().count() >= 3 && !STOP.contains(&t.as_str()) && said.iter().any(|w| same(t, w)))
            }
            _ => false,
        })
        .map(|l| l.id)
        .take(MAX_LAYERS)
        .collect()
}

/// The plan for `asks` (default: [`candidates`]) of the comp in `ctx`, against `map`. `lead` is
/// the accent lead in frames (1: one frame before the stressed vowel).
pub fn plan(ctx: &EvalCtx, map: &SpeechMap, asks: Option<&[Ask]>, lead: f64) -> Vec<Entry> {
    let comp = ctx.comp;
    let fps = comp.frame_rate.as_f64();
    if !fps.is_finite() || fps <= 0.0 {
        return vec![];
    }
    let auto: Vec<Ask>;
    let asks = match asks {
        Some(a) => a,
        None => {
            auto = candidates(comp, map).into_iter().map(|layer| Ask { layer, ..Default::default() }).collect();
            &auto
        }
    };
    let ids: Vec<LayerId> = asks.iter().map(|a| a.layer).collect();
    let dur = comp.duration.seconds();
    let motion = crate::motion::describe(ctx, &ids, Tick::ZERO, comp.duration, crate::motion::default_step(comp.duration, comp.frame_rate.frame_duration()));
    let described: Vec<&Value> = motion.get("layers").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let peaks = lint::speed_peaks(comp, 0.0, dur);
    let words: Vec<(usize, &SpeechWord)> = map.audible(comp).collect();
    let mut out = vec![];
    for a in asks {
        let Some(l) = comp.layer(a.layer) else { continue };
        let m = described.iter().find(|m| m.get("layer").and_then(Value::as_u64) == Some(l.id.0));
        let appear = m.map_or(l.in_point.seconds(), |m| appear_of(m, dur, fps));
        let peak = peaks
            .iter()
            .filter(|p| p.layer == l.id && p.time >= appear - 0.5 / fps && p.time <= appear + ENTRANCE_WINDOW)
            .min_by(|x, y| x.time.total_cmp(&y.time))
            .map(|p| (p.property.clone(), p.path.clone(), p.time));
        let (shows, from) = match &a.words {
            Some(w) => (w.clone(), "given"),
            None => layer_text(l),
        };
        let mut e = Entry {
            layer: l.id,
            name: l.name.clone(),
            shows: shows.clone(),
            from,
            word: None,
            word_start: 0.0,
            accent: 0.0,
            appear,
            peak,
            off_by: 0.0,
            measured: "",
            why: None,
        };
        let toks = tokens(&shows);
        let hit = match a.occurrence {
            // The n-th time the first word is said.
            Some(n) => {
                toks.first().and_then(|t| words.iter().filter(|(_, w)| same(t, &normalize(&w.word))).nth(n.saturating_sub(1)).map(|(i, _)| (*i, 1, true)))
            }
            None => find(&toks, &words, e.peak.as_ref().map_or(appear, |p| p.2)),
        };
        let Some((i, run, _)) = hit else {
            e.why = Some(if toks.is_empty() {
                "no words to match".into()
            } else {
                format!("{:?} isn't said in the speech map", shows.chars().take(60).collect::<String>())
            });
            out.push(e);
            continue;
        };
        let Some(w) = map.words.get(i) else { continue };
        e.word = Some((i, w.word.clone(), run));
        e.word_start = w.start.seconds();
        let syl = w.stressed().map(|(_, s)| s).or(w.syllables.first());
        e.accent = syl.map_or(e.word_start, |s| lint::accent_time(s, fps, lead));
        (e.off_by, e.measured) = match &e.peak {
            Some((_, _, t)) => ((t - e.accent) * fps, "peak"),
            None => ((appear - e.word_start) * fps, "appear"),
        };
        out.push(e);
    }
    out
}

/// Comp seconds → frame.
fn frame(t: f64, fps: f64) -> i64 {
    let f = (t * fps).round();
    if f.is_finite() { f as i64 } else { 0 }
}

/// The call that moves `e` onto its word.
pub fn fix(e: &Entry, fps: f64) -> Option<Value> {
    let (i, _, _) = e.word.as_ref()?;
    if e.off_by.abs() <= 0.5 {
        return None;
    }
    Some(match &e.peak {
        Some((_, path, _)) => json!({"command": "keys.alignPeak", "params": {"layer": e.layer.0, "path": path, "speech": {"index": i}}}),
        None => {
            json!({"command": "layer.timing", "params": {"layers": [e.layer.0], "in": e.word_start}, "note": format!("cuts in on frame {}", frame(e.word_start, fps))})
        }
    })
}

/// One entry as `speech.appearPlan` lists it.
pub fn entry_json(e: &Entry, fps: f64) -> Value {
    let mut o = json!({
        "layer": e.layer.0, "name": e.name, "shows": e.shows, "from": e.from,
        "appearFrame": frame(e.appear, fps),
    });
    if let Some((prop, path, t)) = &e.peak {
        o["entrance"] = json!({"property": prop, "path": path, "peakFrame": frame(*t, fps), "peak": (t * 1000.0).round() / 1000.0});
    }
    match &e.word {
        None => o["unmatched"] = json!(e.why.clone().unwrap_or_default()),
        Some((i, w, run)) => {
            let target = match e.peak {
                Some((_, _, t)) => e.appear + (e.accent - t),
                None => e.word_start,
            };
            o["word"] = json!(w);
            o["wordIndex"] = json!(i);
            o["wordsMatched"] = json!(run);
            o["wordFrame"] = json!(frame(e.word_start, fps));
            o["accentFrame"] = json!(frame(e.accent, fps));
            o["suggestedFrame"] = json!(frame(target, fps));
            o["offBy"] = json!((e.off_by * 10.0).round() / 10.0);
            o["measured"] = json!(e.measured);
            o["sure"] = json!(e.sure());
            if let Some(f) = fix(e, fps) {
                o["fix"] = f;
            }
        }
    }
    o
}
