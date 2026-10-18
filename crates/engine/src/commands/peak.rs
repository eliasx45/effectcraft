//! The speed-peak solver: `keys.alignPeak` makes a keyframed move's speed-graph peak (the one
//! the `off-accent` lint finds, [`lint::segment_peak`]) land on a target frame: a comp frame, a
//! time, or a stressed syllable of the comp's speech map. It keeps the move's values and, unless
//! a stretch mode is asked for, its duration: `shift` moves both keys, `stretch-in` /
//! `stretch-out` retime one key, `ease` only changes the influences within the graph's family.
//! Every move can take one of Devin Jatho's five ease graphs ([`GRAPHS`], Motion Library →
//! Devin Jatho → Graphs); without one, the graph whose peak sits where the target falls in the
//! move is picked. Keys inside precomps are mapped through each precomp layer's start and
//! stretch up to the comp the target frame is in. One undo step for every property.

use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe, Value as KValue, side_ease, spatial_segment_length};
use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Project, Uid};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::prop::prop_ref;
use super::{CommandSpec, b_p, bad, comp_id, f_p, has_comp, match_path_of, resolve_layer, str_p, unlocked};
use crate::lint;
use crate::precomp_time::{self, Hop, MAX_DEPTH, chains, down, scale, up};
use crate::{EngineError, KeyRef, Result, Session, cmd, query};

const C: &str = "keys.alignPeak";
/// Most properties one call aligns.
const MAX_PROPS: usize = 64;
/// Smallest influence the `ease` mode sets (0.1 %).
const MIN_INFLUENCE: f64 = 0.001;
/// How far the `ease` mode scales the influences (e^±6).
const EASE_SPAN: f64 = 6.0;

/// One of Devin's ease graphs: the move's out side (first key) and in side (second key).
/// Speeds are normalised (value change per move duration: 1 = the linear speed), influences
/// 0..1. Values read from the Motion Library entries (Devin Jatho/Graphs) on 18 Oct 2026.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Graph {
    pub id: &'static str,
    pub name: &'static str,
    pub out: Ease,
    pub inn: Ease,
}

/// Devin Jatho → Graphs, in order of where their speed peak sits.
pub const GRAPHS: [Graph; 5] = [
    // Library entry 2b85eb981d3543cd8298d1f066ab74fb, "for really fast slide ins".
    Graph {
        id: "fast-in",
        name: "Fast In",
        out: Ease { speed: 23.752_732_274_402_83, influence: 0.005_926_622_109_416_769 },
        inn: Ease { speed: 0.0, influence: 0.929_213_858_190_53 },
    },
    // starter-curve-4.
    Graph {
        id: "quick-launch",
        name: "Quick launch · soft landing",
        out: Ease { speed: 0.0, influence: 0.398_782_730_102_539_06 },
        inn: Ease { speed: 0.0, influence: 0.909_237_649_705_674_9 },
    },
    // starter-curve-3.
    Graph {
        id: "smooth-emphasis",
        name: "Smooth emphasis",
        out: Ease { speed: 0.0, influence: 0.702_064_355_214_437 },
        inn: Ease { speed: 0.0, influence: 0.700_731_542_375_352_6 },
    },
    // starter-curve-5.
    Graph {
        id: "slow-launch",
        name: "Slow launch · quick finish",
        out: Ease { speed: 0.0, influence: 0.830_011_261_834_038_5 },
        inn: Ease { speed: 0.0, influence: 0.601_217_269_897_460_9 },
    },
    // Library entry 131137cbe3474b509a3760133d62e8d5, "for super fast slide outs".
    Graph { id: "fast-out", name: "Fast out", out: Ease { speed: 0.0, influence: 1.0 }, inn: Ease { speed: 0.0, influence: 0.008_870_442_708_333_37 } },
];

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// A graph by id or name, ignoring case, spaces and punctuation ("Fast In", "fast-in",
/// "quick launch · soft landing", "smooth"); first words are enough when they are unique.
pub fn graph_named(name: &str) -> Option<Graph> {
    let q = squash(name);
    if q.is_empty() {
        return None;
    }
    if let Some(g) = GRAPHS.iter().find(|g| squash(g.id) == q || squash(g.name) == q) {
        return Some(*g);
    }
    let mut hits = GRAPHS.iter().filter(|g| squash(g.id).starts_with(&q) || squash(g.name).starts_with(&q));
    let first = hits.next()?;
    hits.next().is_none().then_some(*first)
}

/// Where a graph's speed peak sits in its move, 0 (start) to 1 (end): measured with the lint's
/// finder on a 0 → 1 move of one second.
pub fn graph_peak(g: &Graph) -> Option<f64> {
    let keys = graph_keys(g, 0.0, 1.0);
    lint::segment_peak(&keys, 0, false).map(|(t, _)| t)
}

fn graph_keys(g: &Graph, v0: f64, v1: f64) -> Vec<Keyframe> {
    let mut a = Keyframe::new(Tick(0), KValue::Scalar(v0));
    let mut b = Keyframe::new(Tick::from_seconds_f64(1.0), KValue::Scalar(v1));
    (a.out_interp, b.in_interp) = (Interp::Bezier, Interp::Bezier);
    a.out_ease = Eases::splat(Ease { speed: g.out.speed * (v1 - v0), influence: g.out.influence }, 1);
    b.in_ease = Eases::splat(Ease { speed: g.inn.speed * (v1 - v0), influence: g.inn.influence }, 1);
    vec![a, b]
}

/// The `graph` parameter: a name (see [`graph_named`]), `"keep"` (the move's own ease), an object
/// `{out: {speed, influence}, in: {speed, influence}}` (normalised speeds, influences in percent,
/// as the Motion Library stores them), or nothing (pick by the target).
enum GraphChoice {
    Auto,
    Keep,
    Use(Graph),
}

fn graph_p(p: &Value) -> Result<GraphChoice> {
    let Some(v) = p.get("graph").filter(|v| !v.is_null()) else { return Ok(GraphChoice::Auto) };
    if let Some(name) = v.as_str() {
        let n = squash(name);
        if n == "keep" || n == "current" {
            return Ok(GraphChoice::Keep);
        }
        if n == "auto" {
            return Ok(GraphChoice::Auto);
        }
        return graph_named(name).map(GraphChoice::Use).ok_or_else(|| {
            bad(
                C,
                format!(
                    "no ease graph {name:?}: use one of {} (Devin Jatho → Graphs), \"keep\" or {{out: {{speed, influence}}, in: {{speed, influence}}}}",
                    GRAPHS.iter().map(|g| format!("\"{}\"", g.id)).collect::<Vec<_>>().join(", ")
                ),
            )
        });
    }
    let side = |k: &str| -> Result<Ease> {
        let s = v.get(k).ok_or_else(|| bad(C, format!("custom `graph` needs `{k}: {{speed, influence}}`")))?;
        let speed = f_p(s, "speed").unwrap_or(0.0);
        let influence = f_p(s, "influence").ok_or_else(|| bad(C, format!("`graph.{k}.influence` (percent) is missing")))?;
        if !speed.is_finite() || speed.abs() > 1e6 || !influence.is_finite() || !(0.1..=100.0).contains(&influence) {
            return Err(bad(C, format!("`graph.{k}`: speed must be finite, influence 0.1 to 100 (percent)")));
        }
        Ok(Ease { speed, influence: influence / 100.0 })
    };
    Ok(GraphChoice::Use(Graph { id: "custom", name: "Custom", out: side("out")?, inn: side("in")? }))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Shift,
    StretchIn,
    StretchOut,
    Ease,
}

impl Mode {
    fn id(self) -> &'static str {
        match self {
            Mode::Shift => "shift",
            Mode::StretchIn => "stretch-in",
            Mode::StretchOut => "stretch-out",
            Mode::Ease => "ease",
        }
    }
}

fn mode_p(p: &Value) -> Result<Mode> {
    match str_p(p, "mode").map(squash).as_deref() {
        None | Some("shift") => Ok(Mode::Shift),
        Some("stretchin") => Ok(Mode::StretchIn),
        Some("stretchout") => Ok(Mode::StretchOut),
        Some("ease") => Ok(Mode::Ease),
        Some(_) => Err(bad(
            C,
            "`mode` is \"shift\" (move both keys), \"stretch-in\" (keep the second key), \"stretch-out\" (keep the first key) or \"ease\" (keys stay, influences change)",
        )),
    }
}

// ---------------------------------------------------------------- time through precomps

fn hop(comp: ItemId, l: &Layer) -> Result<Hop> {
    precomp_time::hop(comp, l).map_err(|e| bad(C, e))
}

fn comp_ref(s: &Session, v: &Value) -> Result<ItemId> {
    comp_id(s, &json!({ "comp": v }))
}

fn comp_name(project: &Project, id: ItemId) -> String {
    project.items.get(&id).map(|i| i.name.clone()).unwrap_or_default()
}

/// The precomp layers from `outer` to `inner`: `via` (layer refs, outermost first) or the only
/// nesting there is.
fn chain_p(s: &Session, p: &Value, outer: ItemId, inner: ItemId) -> Result<Vec<Hop>> {
    let project = &s.project;
    if let Some(via) = p.get("via").filter(|v| !v.is_null()) {
        let list = via.as_array().cloned().unwrap_or_else(|| vec![via.clone()]);
        let mut at = outer;
        let mut out = vec![];
        for v in list.iter().take(MAX_DEPTH) {
            let comp = project.comp(at).ok_or(EngineError::NoComp)?;
            let lid = resolve_layer(comp, v).ok_or_else(|| bad(C, format!("`via`: no layer {v} in {}", comp_name(project, at))))?;
            let l = comp.layer(lid).ok_or_else(|| bad(C, format!("`via`: no layer {v}")))?;
            let LayerSource::Comp { item } = l.source else { return Err(bad(C, format!("`via`: {} is not a precomp layer", l.name))) };
            out.push(hop(at, l)?);
            at = item;
        }
        if at != inner {
            return Err(bad(C, format!("`via` ends in {}, not in {} (the layer's comp)", comp_name(project, at), comp_name(project, inner))));
        }
        return Ok(out);
    }
    if outer == inner {
        return Ok(vec![]);
    }
    let found = chains(project, outer, inner);
    match found.as_slice() {
        [] => Err(bad(C, format!("{} is not nested in {} (targetComp)", comp_name(project, inner), comp_name(project, outer)))),
        [one] => {
            one.iter().map(|(c, l)| project.comp(*c).and_then(|comp| comp.layer(*l)).ok_or(EngineError::NoComp).and_then(|layer| hop(*c, layer))).collect()
        }
        many => {
            let list: Vec<Value> = many.iter().map(|ch| json!(ch.iter().map(|(_, l)| l.0).collect::<Vec<_>>())).collect();
            Err(bad(
                C,
                format!(
                    "{} is nested in {} more than once: pass `via` (precomp layer ids, outermost first), one of {}",
                    comp_name(project, inner),
                    comp_name(project, outer),
                    Value::Array(list)
                ),
            ))
        }
    }
}

// ---------------------------------------------------------------- the target

struct Target {
    /// Target comp seconds.
    time: f64,
    word: Option<Value>,
}

fn clean_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// `frame` / `time` (target comp) or `speech` (a word of the target comp's speech map: its
/// stressed syllable's accent). `hint` (target comp seconds) picks among repeated words.
fn target_p(p: &Value, comp: &Comp, hint: Option<f64>) -> Result<Target> {
    let fps = comp.frame_rate.as_f64();
    let given = ["frame", "time", "speech"].iter().filter(|k| p.get(**k).is_some_and(|v| !v.is_null())).count();
    if given != 1 {
        return Err(bad(C, "give one target: `frame` (target comp frame), `time` (target comp seconds) or `speech` (a word of the comp's speech map)"));
    }
    if let Some(f) = f_p(p, "frame") {
        if !f.is_finite() || f.abs() > 1e9 {
            return Err(bad(C, "`frame` must be a finite frame number"));
        }
        return Ok(Target { time: f / fps, word: None });
    }
    if let Some(t) = f_p(p, "time") {
        if !t.is_finite() || t.abs() > 1e7 {
            return Err(bad(C, "`time` must be finite seconds"));
        }
        return Ok(Target { time: t, word: None });
    }
    let sp = p.get("speech").ok_or_else(|| bad(C, "missing `speech`"))?;
    let map = comp.speech.as_ref().ok_or_else(|| bad(C, "the target comp has no speech map (speech.import), or pass `targetComp`"))?;
    let lead = match p.get("accentLead").filter(|v| !v.is_null()) {
        None => lint::DEFAULT_ACCENT_LEAD,
        Some(v) => v.as_f64().filter(|x| x.is_finite() && (0.0..=100.0).contains(x)).ok_or_else(|| bad(C, "`accentLead` is frames (0 to 100)"))?,
    };
    let (word, index, occurrence, syllable) = match sp {
        Value::String(w) => (Some(w.as_str()), None, None, None),
        Value::Number(n) => (None, n.as_u64(), None, None),
        _ => (
            str_p(sp, "word"),
            sp.get("index").and_then(Value::as_u64),
            sp.get("occurrence").and_then(Value::as_u64),
            sp.get("syllable").and_then(Value::as_u64),
        ),
    };
    let candidates: Vec<(usize, &effectcraft_project::speech::SpeechWord)> = match (word, index) {
        (_, Some(i)) => map
            .words
            .get(i as usize)
            .map(|w| vec![(i as usize, w)])
            .ok_or_else(|| bad(C, format!("the speech map has {} words; no index {i}", map.words.len())))?,
        (Some(w), None) => {
            let q = clean_word(w);
            map.words.iter().enumerate().filter(|(_, x)| clean_word(&x.word) == q).collect()
        }
        (None, None) => return Err(bad(C, "`speech` is a word, a word index, or {word | index, occurrence? (1-based), syllable? (0-based)}")),
    };
    let accent = |w: &effectcraft_project::speech::SpeechWord| -> Option<(f64, usize)> {
        let i = match syllable {
            Some(i) => i as usize,
            None => w.syllables.iter().position(|s| s.stress == 1).or_else(|| w.syllables.iter().position(|s| s.stress == 2)).unwrap_or(0),
        };
        w.syllables.get(i).map(|s| (lint::accent_time(s, fps, lead), i))
    };
    let pick = match (occurrence, candidates.len()) {
        (_, 0) => return Err(bad(C, format!("no word {:?} in the speech map", word.unwrap_or("")))),
        (Some(n), len) => candidates
            .get((n as usize).checked_sub(1).ok_or_else(|| bad(C, "`occurrence` is 1-based"))?)
            .copied()
            .ok_or_else(|| bad(C, format!("{:?} occurs {len} times", word.unwrap_or(""))))?,
        (None, 1) => candidates.first().copied().ok_or_else(|| bad(C, "no word"))?,
        (None, _) => match hint {
            Some(h) => candidates
                .iter()
                .copied()
                .filter_map(|c| accent(c.1).map(|a| (c, (a.0 - h).abs())))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(c, _)| c)
                .ok_or_else(|| bad(C, "the word has no syllables"))?,
            None => {
                let list: Vec<String> = candidates.iter().map(|(i, w)| format!("#{i} at {:.3} s", w.start.seconds())).collect();
                return Err(bad(
                    C,
                    format!(
                        "{:?} occurs {} times ({}): pass `speech: {{word, occurrence}}` or `{{index}}`",
                        word.unwrap_or(""),
                        candidates.len(),
                        list.join(", ")
                    ),
                ));
            }
        },
    };
    let (time, syl) = accent(pick.1).ok_or_else(|| bad(C, format!("{:?} has no syllable onsets", pick.1.word)))?;
    let sy = pick.1.syllables.get(syl);
    Ok(Target {
        time,
        word: Some(json!({
            "word": pick.1.word, "index": pick.0, "syllable": syl,
            "onset": sy.map(|s| s.t.seconds()), "vowel": sy.map(|s| s.vowel.seconds()),
            "accentLead": lead,
        })),
    })
}

/// The nearest comp holding a speech map that `inner` is nested in (itself first).
fn speech_comp(project: &Project, inner: ItemId) -> Option<ItemId> {
    if project.comp(inner).is_some_and(|c| c.speech.is_some()) {
        return Some(inner);
    }
    let mut with: Vec<ItemId> = project.items.values().filter(|i| i.as_comp().is_some_and(|c| c.speech.is_some())).map(|i| i.id).collect();
    with.retain(|c| !chains(project, *c, inner).is_empty());
    (with.len() == 1).then(|| with.first().copied()).flatten()
}

// ---------------------------------------------------------------- the solver

/// Dimensions of a property's eases: one for spatial values and paths, else one per component.
fn dims(v: &KValue, spatial: bool) -> usize {
    if spatial && matches!(v, KValue::Vec2(_) | KValue::Vec3(_)) { 1 } else { v.components().len().max(1) }
}

/// Puts `g` on the segment `i` → `i + 1`: the first key's out side, the second key's in side,
/// both Bezier. Auto-Bezier or continuous keys are broken first, freezing the neighbouring
/// segments' sides as they were.
fn apply_graph(keys: &mut [Keyframe], i: usize, spatial: bool, g: &Graph) -> Result<()> {
    let j = i.checked_add(1).ok_or_else(|| bad(C, "no key pair"))?;
    let (Some(a), Some(b)) = (keys.get(i), keys.get(j)) else { return Err(bad(C, "no key pair")) };
    let n = dims(&a.value, spatial);
    let dur = (b.time.seconds() - a.time.seconds()).max(1e-9);
    let (ca, cb) = (a.value.components(), b.value.components());
    let is_spatial = spatial && matches!(a.value, KValue::Vec2(_) | KValue::Vec3(_));
    let freeze_in: Option<Vec<Ease>> =
        (a.auto_bezier || a.continuous).then(|| (0..n).map(|d| side_ease(keys, i, d, spatial, false).unwrap_or_default()).collect());
    let freeze_out: Option<Vec<Ease>> =
        (b.auto_bezier || b.continuous).then(|| (0..n).map(|d| side_ease(keys, j, d, spatial, true).unwrap_or_default()).collect());
    // Value change per dimension the normalised speeds scale.
    let delta: Vec<f64> = if is_spatial {
        vec![spatial_segment_length(keys, i)]
    } else if ca.is_empty() || ca.len() != cb.len() {
        vec![1.0]
    } else {
        ca.iter().zip(&cb).map(|(x, y)| y - x).collect()
    };
    let eases = |e: Ease| -> Eases {
        let list: Vec<Ease> = (0..n).map(|d| Ease { speed: e.speed * delta.get(d).copied().unwrap_or(0.0) / dur, influence: e.influence }).collect();
        Eases::from(list)
    };
    if let Some(a) = keys.get_mut(i) {
        if let Some(f) = freeze_in.filter(|_| a.in_interp != Interp::Linear && a.in_interp != Interp::Hold) {
            a.in_ease = Eases::from(f);
        }
        (a.auto_bezier, a.continuous, a.out_interp) = (false, false, Interp::Bezier);
        a.out_ease = eases(g.out);
    }
    if let Some(b) = keys.get_mut(j) {
        if let Some(f) = freeze_out.filter(|_| b.out_interp != Interp::Linear && b.out_interp != Interp::Hold) {
            b.out_ease = Eases::from(f);
        }
        (b.auto_bezier, b.continuous, b.in_interp) = (false, false, Interp::Bezier);
        b.in_ease = eases(g.inn);
    }
    Ok(())
}

/// Moves the pair's keys to `ta` / `tb` (layer time), scaling the pair's own Bezier speeds so
/// the graph keeps its shape (speed × duration stays the same).
fn retime(base: &[Keyframe], i: usize, ta: Tick, tb: Tick) -> Vec<Keyframe> {
    let mut keys = base.to_vec();
    let (Some(a), Some(b)) = (base.get(i), base.get(i + 1)) else { return keys };
    let old = (b.time.seconds() - a.time.seconds()).max(1e-9);
    let new = (tb.seconds() - ta.seconds()).max(1e-9);
    let k = old / new;
    if let Some(a) = keys.get_mut(i) {
        a.time = ta;
        if a.out_interp == Interp::Bezier && !a.auto_bezier {
            a.out_ease.as_mut_slice().iter_mut().for_each(|e| e.speed *= k);
        }
    }
    if let Some(b) = keys.get_mut(i + 1) {
        b.time = tb;
        if b.in_interp == Interp::Bezier && !b.auto_bezier {
            b.in_ease.as_mut_slice().iter_mut().for_each(|e| e.speed *= k);
        }
    }
    keys
}

/// The pair's influences scaled by e^`s` (out side) and e^−`s` (in side): later peaks for `s` > 0.
fn ease_family(base: &[Keyframe], i: usize, s: f64) -> Vec<Keyframe> {
    let mut keys = base.to_vec();
    let f = |e: &mut Ease, k: f64| e.influence = (e.influence * k).clamp(MIN_INFLUENCE, 1.0);
    if let Some(a) = keys.get_mut(i) {
        a.out_ease.as_mut_slice().iter_mut().for_each(|e| f(e, s.exp()));
    }
    if let Some(b) = keys.get_mut(i + 1) {
        b.in_ease.as_mut_slice().iter_mut().for_each(|e| f(e, (-s).exp()));
    }
    keys
}

/// One property's move to align.
struct Job {
    lid: LayerId,
    uid: Uid,
    /// First key of the pair, if given (`key` or the selection).
    key: Option<usize>,
}

/// What the solver decided for one property.
struct Plan {
    job_layer: LayerId,
    uid: Uid,
    keys: Vec<Keyframe>,
    /// The pair's key times before and after (layer time), for the keyframe selection.
    moved: [(Tick, Tick); 2],
    report: Value,
}

/// Layer seconds ↔ target comp seconds for one layer.
struct Clock<'a> {
    layer: &'a Layer,
    chain: &'a [Hop],
    fps: f64,
}

impl Clock<'_> {
    fn frame(&self, layer_s: f64) -> f64 {
        up(self.chain, self.layer.comp_time(Tick::from_seconds_f64(layer_s)).seconds()) * self.fps
    }
    fn frame_of(&self, t: Tick) -> f64 {
        up(self.chain, self.layer.comp_time(t).seconds()) * self.fps
    }
    fn layer_s(&self, target_s: f64) -> f64 {
        self.layer.layer_time(Tick::from_seconds_f64(down(self.chain, target_s))).seconds()
    }
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The layer times of the whole frames (of the layer's comp `fr`) on either side of `layer_s`.
fn frame_candidates(layer: &Layer, fr: FrameRate, layer_s: f64) -> Vec<Tick> {
    let ct = layer.comp_time(Tick::from_seconds_f64(layer_s)).seconds() * fr.as_f64();
    if !ct.is_finite() || ct.abs() > 1e10 {
        return vec![];
    }
    let mut out: Vec<Tick> = [ct.floor(), ct.ceil()].iter().map(|f| layer.layer_time(fr.tick_of(*f as i64))).collect();
    out.dedup();
    out
}

#[allow(clippy::too_many_arguments)]
fn solve(
    prop: &effectcraft_project::Property,
    i: usize,
    clock: &Clock,
    fr: FrameRate,
    target_s: f64,
    mode: Mode,
    graph: &GraphChoice,
    snap: bool,
    tolerance: f64,
) -> Result<(Vec<Keyframe>, Value)> {
    let keys = &prop.keys;
    let spatial = prop.spatial;
    let (Some(a), Some(b)) = (keys.get(i), keys.get(i + 1)) else { return Err(bad(C, format!("{}: no keyframe pair at key {i}", prop.name))) };
    if a.out_interp == Interp::Hold {
        return Err(bad(C, format!("{}: the keys at {i}/{} hold, so nothing moves between them", prop.name, i + 1)));
    }
    if a.value == b.value {
        return Err(bad(C, format!("{}: keys {i} and {} have the same value: no move to time", prop.name, i + 1)));
    }
    let target_lt = clock.layer_s(target_s);
    let target_f = target_s * clock.fps;
    let (ta, tb) = (a.time.seconds(), b.time.seconds());
    let before = lint::segment_peak(keys, i, spatial).map(|(t, _)| clock.frame(t));
    // The graph.
    let (graph, chosen): (Option<Graph>, &str) = match graph {
        GraphChoice::Keep => (None, "keep"),
        GraphChoice::Use(g) => (Some(*g), g.id),
        GraphChoice::Auto => {
            // Where the target falls in the move (moved keys: where it should sit after the move).
            let x = ((target_lt - ta) / (tb - ta).max(1e-9)).clamp(0.0, 1.0);
            let best = GRAPHS
                .iter()
                .filter_map(|g| graph_peak(g).map(|p| (g, (p - x).abs())))
                .filter(|(g, _)| match mode {
                    // A stretch must keep a key where the peak is not.
                    Mode::StretchIn => graph_peak(g).is_some_and(|p| p < 0.95),
                    Mode::StretchOut => graph_peak(g).is_some_and(|p| p > 0.05),
                    _ => true,
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(g, _)| *g)
                .ok_or_else(|| bad(C, "no graph fits"))?;
            (Some(best), best.id)
        }
    };
    let mut base = keys.clone();
    if let Some(g) = &graph {
        apply_graph(&mut base, i, spatial, g)?;
    }
    let peak_of = |k: &[Keyframe]| lint::segment_peak(k, i, spatial).map(|(t, _)| t);
    let phi = |k: &[Keyframe]| -> Option<f64> {
        let (a, b) = (k.get(i)?.time.seconds(), k.get(i + 1)?.time.seconds());
        peak_of(k).map(|p| (p - a) / (b - a).max(1e-9))
    };
    let Some(phi0) = phi(&base) else {
        return Err(bad(C, format!("{}: the move is linear (constant speed, no peak): pass a `graph`", prop.name)));
    };
    let prev = i.checked_sub(1).and_then(|j| keys.get(j)).map(|k| k.time);
    let next = keys.get(i + 2).map(|k| k.time);
    let min_dur = fr.frame_duration().seconds() * 100.0 / clock.layer.stretch.abs().max(1e-6);
    let fits = |ka: Tick, kb: Tick| -> bool { prev.is_none_or(|p| ka > p) && next.is_none_or(|n| kb < n) && kb.seconds() - ka.seconds() >= min_dur * 0.999 };
    // Candidate key times (layer ticks) for the pair.
    let options = |t: f64| -> Vec<(Tick, bool)> {
        let mut out: Vec<(Tick, bool)> = if snap { frame_candidates(clock.layer, fr, t).into_iter().map(|t| (t, true)).collect() } else { vec![] };
        out.push((Tick::from_seconds_f64(t), false));
        out
    };
    let mut candidates: Vec<(Tick, Tick, bool)> = vec![];
    let mut ease_s: Option<f64> = None;
    match mode {
        Mode::Shift => {
            let pk = peak_of(&base).unwrap_or(ta);
            let new_a = ta + (target_lt - pk);
            for (na, snapped) in options(new_a) {
                let d = na - a.time;
                candidates.push((na, b.time + d, snapped));
            }
        }
        Mode::StretchIn | Mode::StretchOut => {
            let mut cur = base.clone();
            let mut moved = if mode == Mode::StretchIn { ta } else { tb };
            for _ in 0..6 {
                let f = phi(&cur).unwrap_or(phi0);
                let (ca, cb) = (cur.get(i).map(|k| k.time.seconds()).unwrap_or(ta), cur.get(i + 1).map(|k| k.time.seconds()).unwrap_or(tb));
                moved = if mode == Mode::StretchIn {
                    if f > 0.995 {
                        return Err(bad(C, format!("{}: the peak sits on the second key, which stretch-in keeps: use stretch-out or shift", prop.name)));
                    }
                    (target_lt - f * cb) / (1.0 - f)
                } else {
                    if f < 0.005 {
                        return Err(bad(C, format!("{}: the peak sits on the first key, which stretch-out keeps: use stretch-in or shift", prop.name)));
                    }
                    ca + (target_lt - ca) / f
                };
                if !moved.is_finite() || moved.abs() > 1e7 {
                    return Err(bad(C, format!("{}: no key time puts the peak there", prop.name)));
                }
                let (na, nb) = if mode == Mode::StretchIn { (Tick::from_seconds_f64(moved), b.time) } else { (a.time, Tick::from_seconds_f64(moved)) };
                if nb <= na {
                    break;
                }
                cur = retime(&base, i, na, nb);
            }
            for (t, snapped) in options(moved) {
                candidates.push(if mode == Mode::StretchIn { (t, b.time, snapped) } else { (a.time, t, snapped) });
            }
        }
        Mode::Ease => {
            let at = |s: f64| peak_of(&ease_family(&base, i, s));
            let (Some(lo), Some(hi)) = (at(-EASE_SPAN), at(EASE_SPAN)) else {
                return Err(bad(C, format!("{}: the ease mode needs Bezier eases on the move: pass a `graph`", prop.name)));
            };
            if target_lt < lo.min(hi) - 1e-9 || target_lt > lo.max(hi) + 1e-9 {
                return Err(bad(
                    C,
                    format!(
                        "{}: with its keys where they are, the {} family can only peak between frame {:.1} and {:.1}: use shift or a stretch mode",
                        prop.name,
                        chosen,
                        clock.frame(lo.min(hi)),
                        clock.frame(lo.max(hi))
                    ),
                ));
            }
            let (mut l, mut h) = (-EASE_SPAN, EASE_SPAN);
            let rising = hi >= lo;
            for _ in 0..48 {
                let m = (l + h) / 2.0;
                let v = at(m).unwrap_or(target_lt);
                if (v < target_lt) == rising { l = m } else { h = m }
            }
            ease_s = Some((l + h) / 2.0);
            candidates.push((a.time, b.time, true));
        }
    }
    // Whole frames first; sub-frame key times only when no whole frame is close enough (a
    // precomp stretched or at another frame rate makes the target's frames finer).
    let mut best: Option<(Vec<Keyframe>, f64, bool)> = None;
    for (na, nb, snapped) in candidates {
        if !fits(na, nb) {
            continue;
        }
        let k = match ease_s {
            Some(s) => ease_family(&base, i, s),
            None => retime(&base, i, na, nb),
        };
        let Some(pk) = peak_of(&k) else { continue };
        let err = clock.frame(pk) - target_f;
        let better = match &best {
            None => true,
            Some((_, e, was)) => {
                let ok = |e: f64| e.abs() <= tolerance + 1e-6;
                match (*was && ok(*e), snapped && ok(err)) {
                    (true, false) => false,
                    (false, true) => true,
                    _ => err.abs() < e.abs(),
                }
            }
        };
        if better {
            best = Some((k, err, snapped));
        }
    }
    let Some((out, err, snapped)) = best else {
        let around = match (prev, next) {
            (Some(p), _) if mode != Mode::StretchOut => format!(" (the key before is at frame {:.0})", clock.frame_of(p)),
            (_, Some(n)) => format!(" (the key after is at frame {:.0})", clock.frame_of(n)),
            _ => String::new(),
        };
        return Err(bad(
            C,
            format!("{}: the keys can't move there without crossing a neighbouring key or getting shorter than a frame{around}: try another mode", prop.name),
        ));
    };
    if err.abs() > tolerance + 1e-6 {
        return Err(bad(
            C,
            format!("{}: the closest peak is {err:+.2} frames off the target (tolerance ±{tolerance}): try another mode or `snap: false`", prop.name),
        ));
    }
    let after = peak_of(&out).map(|t| clock.frame(t));
    let key_frames = |k: &[Keyframe]| json!([k.get(i).map(|k| r2(clock.frame_of(k.time))), k.get(i + 1).map(|k| r2(clock.frame_of(k.time)))]);
    let influences = |k: &[Keyframe]| {
        json!({
            "out": k.get(i).and_then(|k| k.out_ease.as_slice().first().map(|e| r2(e.influence * 100.0))),
            "in": k.get(i + 1).and_then(|k| k.in_ease.as_slice().first().map(|e| r2(e.influence * 100.0))),
        })
    };
    let report = json!({
        "key": i,
        "graph": chosen,
        "graphName": graph.map(|g| g.name).unwrap_or("(kept)"),
        "peakShare": phi(&out).map(|v| (v * 1000.0).round() / 1000.0),
        "peakBefore": before.map(r2),
        "peakAfter": after.map(r2),
        "offsetFrames": r2(err),
        "keysBefore": key_frames(keys),
        "keysAfter": key_frames(&out),
        "influence": influences(&out),
        "onFrames": snapped,
        "expression": prop.has_expression(),
    });
    Ok((out, report))
}

/// The properties to align: `path` / `prop` / `paths` on `layer`, else the selected keyframes
/// (one pair per property: its first two selected keys).
fn jobs_p(s: &Session, p: &Value, cid: ItemId) -> Result<Vec<Job>> {
    let key = p.get("key").and_then(Value::as_u64).map(|k| k as usize);
    if let Some(paths) = p.get("paths").filter(|v| !v.is_null()) {
        let list = paths.as_array().ok_or_else(|| bad(C, "`paths` is a list of property paths"))?;
        let mut out = vec![];
        for path in list.iter().take(MAX_PROPS) {
            let mut q = json!({"comp": cid.0, "path": path});
            if let Some(l) = p.get("layer") {
                q["layer"] = l.clone();
            }
            let (_, lid, uid) = prop_ref(s, &q, C)?;
            out.push(Job { lid, uid, key });
        }
        return Ok(out);
    }
    if p.get("path").is_some() || p.get("prop").is_some() {
        let (_, lid, uid) = prop_ref(s, p, C)?;
        return Ok(vec![Job { lid, uid, key }]);
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut out: Vec<Job> = vec![];
    for k in &s.state.selected_keys {
        if out.iter().any(|j| j.lid == k.layer && j.uid == k.prop) || out.len() >= MAX_PROPS {
            continue;
        }
        let Some(pr) = comp.layer(k.layer).and_then(|l| l.props.find(k.prop)) else { continue };
        let mut idx: Vec<usize> = s
            .state
            .selected_keys
            .iter()
            .filter(|x| x.layer == k.layer && x.prop == k.prop)
            .filter_map(|x| pr.keys.iter().position(|kf| kf.time == x.time))
            .collect();
        idx.sort_unstable();
        idx.dedup();
        let Some(&first) = idx.first() else { continue };
        if idx.get(1) == Some(&(first + 1)) {
            out.push(Job { lid: k.layer, uid: k.prop, key: Some(first) });
        }
    }
    if out.is_empty() {
        return Err(bad(C, "pass `layer` + `path` (or `paths`), or select two neighbouring keyframes of the move"));
    }
    Ok(out)
}

/// The segment to align: `key`, the only pair, else the one whose peak is nearest `target_s`.
fn segment(prop: &effectcraft_project::Property, key: Option<usize>, clock: &Clock, target_s: Option<f64>) -> Result<usize> {
    if prop.keys.len() < 2 {
        return Err(bad(C, format!("{} has fewer than two keyframes", prop.name)));
    }
    if let Some(k) = key {
        return if k + 1 < prop.keys.len() {
            Ok(k)
        } else {
            Err(bad(C, format!("{} has {} keys: `key` (the pair's first, 0-based) must be below {}", prop.name, prop.keys.len(), prop.keys.len() - 1)))
        };
    }
    if prop.keys.len() == 2 {
        return Ok(0);
    }
    let target_f = target_s.map(|t| t * clock.fps);
    let pick = (0..prop.keys.len() - 1)
        .filter_map(|i| {
            let pk = lint::segment_peak(&prop.keys, i, prop.spatial)?;
            Some((i, target_f.map(|t| (clock.frame(pk.0) - t).abs()).unwrap_or(0.0)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    pick.ok_or_else(|| bad(C, format!("{} has no eased move (all linear or hold): pass `key` and a `graph`", prop.name)))
}

fn align(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let mode = mode_p(p)?;
    let graph = graph_p(p)?;
    let snap = b_p(p, "snap").unwrap_or(true);
    let tolerance = match p.get("tolerance").filter(|v| !v.is_null()) {
        None => 0.5,
        Some(v) => v.as_f64().filter(|x| x.is_finite() && (0.0..=100.0).contains(x)).ok_or_else(|| bad(C, "`tolerance` is frames (0 to 100)"))?,
    };
    let tid = match p.get("targetComp").filter(|v| !v.is_null()) {
        Some(v) => comp_ref(s, v)?,
        None if p.get("speech").is_some_and(|v| !v.is_null()) => speech_comp(&s.project, cid).unwrap_or(cid),
        None => cid,
    };
    let chain = chain_p(s, p, tid, cid)?;
    let jobs = jobs_p(s, p, cid)?;
    let ids: Vec<LayerId> = jobs.iter().map(|j| j.lid).collect();
    let open = unlocked(s, cid, ids, C)?;
    let project = s.project.clone();
    let comp = project.comp(cid).ok_or(EngineError::NoComp)?;
    let tcomp = project.comp(tid).ok_or(EngineError::NoComp)?;
    let fps = tcomp.frame_rate.as_f64();
    // A hint for repeated words: the first move's current peak.
    let hint = jobs.first().and_then(|j| {
        let l = comp.layer(j.lid)?;
        let pr = l.props.find(j.uid)?;
        let clock = Clock { layer: l, chain: &chain, fps };
        let i = segment(pr, j.key, &clock, None).ok()?;
        lint::segment_peak(&pr.keys, i, pr.spatial).map(|(t, _)| clock.frame(t) / fps)
    });
    let target = target_p(p, tcomp, hint)?;
    let mut plans: Vec<Plan> = vec![];
    for j in jobs.iter().filter(|j| open.contains(&j.lid)) {
        let l = comp.layer(j.lid).ok_or_else(|| bad(C, "no layer"))?;
        let pr = l.props.find(j.uid).ok_or_else(|| bad(C, "no property"))?;
        let clock = Clock { layer: l, chain: &chain, fps };
        let i = segment(pr, j.key, &clock, Some(target.time))?;
        let (keys, mut report) = solve(pr, i, &clock, comp.frame_rate, target.time, mode, &graph, snap, tolerance)?;
        report["layer"] = json!(l.id.0);
        report["layerName"] = json!(l.name);
        report["property"] = json!(pr.name);
        report["path"] = json!(match_path_of(&l.props, pr.uid));
        let time = |k: &[Keyframe], j: usize| k.get(j).map(|k| k.time).unwrap_or_default();
        let moved = [(time(&pr.keys, i), time(&keys, i)), (time(&pr.keys, i + 1), time(&keys, i + 1))];
        plans.push(Plan { job_layer: l.id, uid: pr.uid, keys, moved, report });
    }
    let reports: Vec<Value> = plans.iter().map(|pl| pl.report.clone()).collect();
    let follow: Vec<(LayerId, Uid, [(Tick, Tick); 2])> = plans.iter().map(|pl| (pl.job_layer, pl.uid, pl.moved)).collect();
    s.edit("Align Speed Peak", None, |proj, st| {
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        for pl in plans {
            let l = comp.layer_mut(pl.job_layer).ok_or_else(|| bad(C, "no layer"))?;
            let pr = l.props.find_mut(pl.uid).ok_or_else(|| bad(C, "no property"))?;
            pr.keys = pl.keys;
        }
        // Selected keys of the pair follow them.
        for k in st.selected_keys.iter_mut() {
            let hit = follow.iter().find(|(l, u, _)| *l == k.layer && *u == k.prop).and_then(|(_, _, m)| m.iter().find(|(old, _)| *old == k.time));
            if let Some((_, new)) = hit {
                *k = KeyRef { layer: k.layer, prop: k.prop, time: *new };
            }
        }
        Ok(())
    })?;
    Ok(json!({
        "comp": cid.0,
        "targetComp": tid.0,
        "via": chain.iter().map(|h| json!({"comp": h.comp.0, "layer": h.layer.0})).collect::<Vec<_>>(),
        "mode": mode.id(),
        "target": {"time": (target.time * 1000.0).round() / 1000.0, "frame": r2(target.time * fps), "speech": target.word},
        "frameScale": scale(&chain),
        "results": reports,
    }))
}

fn graphs(_: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({
        "graphs": GRAPHS.iter().map(|g| json!({
            "id": g.id, "name": g.name,
            "peak": graph_peak(g).map(|v| (v * 1000.0).round() / 1000.0),
            "out": {"speed": g.out.speed, "influence": g.out.influence * 100.0},
            "in": {"speed": g.inn.speed, "influence": g.inn.influence * 100.0},
        })).collect::<Vec<_>>(),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "keys.alignPeak",
            "Align Speed Peak",
            [],
            None,
            "{comp? (the layer's comp), layer?, path|prop|paths? (property paths; else the selected keyframes: each property's first two neighbouring selected keys), key? (the pair's first key, 0-based; default: the only pair, else the one peaking nearest the target), frame? (target comp frame) | time? (target comp s) | speech? (a word of the target comp's speech map, or {word | index, occurrence? (1-based), syllable?}; lands on its accent), accentLead? (frames, default 1), targetComp? (the comp the target is in; default: the layer's comp, or with speech the comp with the map), via? (precomp layer ids from targetComp down, when nested more than once), mode?: shift (default: move both keys) | stretch-in (keep the second key) | stretch-out (keep the first key) | ease (keys stay; influences change within the graph's family), graph? (fast-in | quick-launch | smooth-emphasis | slow-launch | fast-out: Devin's graphs; keep; or {out: {speed, influence}, in: {speed, influence}} normalised speed, influence %; default: the graph whose peak sits where the target falls), snap? (keys on whole frames, default true), tolerance? (frames, default 0.5)} → {comp, targetComp, via, mode, target: {time, frame, speech}, results: [{layer, layerName, property, path, key, graph, peakBefore, peakAfter (target comp frames), offsetFrames, keysBefore, keysAfter, influence, peakShare}]}. One undo step; values are kept.",
            has_comp,
            align
        ),
        query!(
            "keys.peakGraphs",
            "Peak Ease Graphs",
            "{} → {graphs: [{id, name, peak (0 start … 1 end), out, in}]}: Devin's ease graphs keys.alignPeak uses",
            graphs
        ),
    ]
}
