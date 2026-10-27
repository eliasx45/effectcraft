//! SX7: the Script tab native to the editing harness (27 Oct, Elias approved the playground).
//! `script.get` fills itself from the comp's speech map when no .txt is loaded, and every line
//! says which chapter (the comp markers Ghostex's Scenes strip shows) and which scene (the
//! precomps covering it, followed down) it is in, or that no scene covers it (a gap).
//!
//! - [`speech_lines`]: the speech map's words cut into lines on pauses and a word cap.
//! - [`chapters`]: the comp's markers, each running to the next one (Ghostex `model::scenes`).
//! - [`SceneFinder`]: the scene path at a comp time, deep: MAIN › `26-10 P1` › `S1 03 Everyday`.
//!   It maps the time into each precomp the way `layer.at {deep}` does (ED2:
//!   [`effectcraft_render::EvalCtx::nested_time`]: time remap, stretch, preserved frame rate).
//!
//! Which precomp is a scene: at each level the topmost visible precomp layer active at the time
//! (not a guide or adjustment layer, not the voice). A precomp whose own precomps play one after
//! another ([`is_sequence`]: a part holding S1 01 … S1 07) is a container: the path goes into
//! it, and when none of its scenes plays there the time is a gap. Any other precomp is the scene
//! and the path stops there (cards inside a scene don't matter). A precomp that spans
//! (nearly) its whole parent and isn't a container is a backdrop (the presenter, a BG) and is
//! skipped. With no precomp, a text or shape layer in the container is the scene. Footage,
//! solids and nulls are never scenes: voice over the presenter alone is a gap.

use std::collections::HashMap;

use effectcraft_project::speech::SpeechWord;
use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Project};
use effectcraft_render::Renderer;
use effectcraft_time::Tick;

/// A pause longer than this ends a line (seconds).
pub const DEFAULT_PAUSE_S: f64 = 0.32;
/// A line ends after this many words.
pub const DEFAULT_MAX_WORDS: usize = 11;
/// Most precomp levels a scene path goes down (a cycle can't loop it).
const MAX_SCENE_DEPTH: usize = 8;
/// Most times sampled per line.
const MAX_SAMPLES: usize = 24;
/// A non-container precomp spanning this much of its parent comp is a backdrop, not a scene.
const BACKDROP_SHARE: f64 = 0.9;
/// A comp's precomps overlapping each other by at most this share of their length play in sequence.
const SEQUENCE_OVERLAP: f64 = 0.1;
/// Most precomp layers [`is_sequence`] looks at.
const MAX_SEQUENCE_LAYERS: usize = 256;

/// The speech map's words cut into lines: a pause over `pause_s` seconds, or `max_words` words,
/// ends a line. Inclusive word ranges, in order; every word is on exactly one line.
pub fn speech_lines(words: &[SpeechWord], pause_s: f64, max_words: usize) -> Vec<(usize, usize)> {
    let max_words = max_words.max(1);
    let pause_s = if pause_s.is_finite() { pause_s.max(0.0) } else { DEFAULT_PAUSE_S };
    let mut out = vec![];
    let mut first = 0usize;
    for (i, w) in words.iter().enumerate() {
        let n = i.saturating_sub(first).saturating_add(1);
        let ends = match words.get(i + 1) {
            None => true,
            Some(next) => (next.start - w.end).seconds() > pause_s || n >= max_words,
        };
        if ends {
            out.push((first, i));
            first = i + 1;
        }
    }
    out
}

/// One chapter: a comp marker, running to the next one (or the comp's end).
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    /// The marker's comment ("IDEA · TikTokers got paid"), else its time ("1:15").
    pub name: String,
    /// Comp seconds.
    pub start: f64,
    pub end: f64,
}

/// The comp's markers as chapters, sorted, the same spans Ghostex's Scenes strip shows.
pub fn chapters(comp: &Comp) -> Vec<Chapter> {
    let mut ms: Vec<(f64, String)> = comp.markers.iter().map(|m| (m.time.seconds(), m.comment.trim().to_string())).filter(|(t, _)| t.is_finite()).collect();
    ms.sort_by(|a, b| a.0.total_cmp(&b.0));
    let dur = comp.duration.seconds();
    let starts: Vec<f64> = ms.iter().map(|m| m.0).collect();
    ms.into_iter()
        .enumerate()
        .map(|(i, (t, c))| {
            let end = starts.get(i + 1).copied().unwrap_or(dur.max(t));
            let name = if c.is_empty() { short_time(t) } else { c };
            Chapter { name, start: t, end }
        })
        .collect()
}

/// The chapter a comp time is in: the last one starting at or before it.
pub fn chapter_at(chs: &[Chapter], t: f64) -> Option<usize> {
    chs.iter().rposition(|c| c.start <= t + 1e-9)
}

fn short_time(t: f64) -> String {
    let s = if t.is_finite() { t.max(0.0) as u64 } else { 0 };
    format!("{}:{:02}", s / 60, s % 60)
}

/// The scene at one time: the precomp (or text/shape) layers from the root comp down.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneHit {
    /// Layer names below the root comp, outermost first (`["26-10 P1", "S1 03 Everyday"]`).
    pub path: Vec<String>,
    /// (comp holding the layer, layer) per path entry.
    pub layers: Vec<(ItemId, LayerId)>,
    /// A scene plays there. False: only containers (or nothing) do; the voice runs bare.
    pub covered: bool,
}

/// Finds the scene path at comp times of one root comp.
pub struct SceneFinder<'r, 'a> {
    r: &'r Renderer<'a>,
    project: &'a Project,
    root: ItemId,
    voice: Option<LayerId>,
    sequences: HashMap<ItemId, bool>,
}

impl<'r, 'a> SceneFinder<'r, 'a> {
    /// `voice`: the root comp's voice layer, never a scene.
    pub fn new(r: &'r Renderer<'a>, project: &'a Project, root: ItemId, voice: Option<LayerId>) -> Self {
        SceneFinder { r, project, root, voice, sequences: HashMap::new() }
    }

    fn sequence(&mut self, item: ItemId) -> bool {
        if let Some(v) = self.sequences.get(&item) {
            return *v;
        }
        let v = self.project.comp(item).is_some_and(is_sequence);
        self.sequences.insert(item, v);
        v
    }

    /// The scene path at a root comp time.
    pub fn at(&mut self, t: Tick) -> SceneHit {
        let mut hit = SceneHit::default();
        let Some(mut ctx) = self.r.eval_ctx(self.root, t) else { return hit };
        let mut cid = self.root;
        for depth in 0..MAX_SCENE_DEPTH {
            let comp: &'a Comp = ctx.comp;
            let now = ctx.time;
            let voice = if depth == 0 { self.voice } else { None };
            let active: Vec<&'a Layer> = comp
                .layers
                .iter()
                .filter(|l| l.is_active_at(now) && l.switches.video && !l.switches.guide && !l.switches.adjustment && Some(l.id) != voice)
                .collect();
            // The topmost precomp that is a container or a scene (not a backdrop).
            let mut pick = None;
            for l in &active {
                let LayerSource::Comp { item } = l.source else { continue };
                let seq = self.sequence(item);
                if !seq && is_backdrop(l, comp) {
                    continue;
                }
                pick = Some((*l, item, seq));
                break;
            }
            if let Some((l, item, seq)) = pick {
                hit.path.push(l.name.clone());
                hit.layers.push((cid, l.id));
                if !seq {
                    hit.covered = true;
                    break;
                }
                let nt = ctx.nested_time(l);
                let Some(next) = self.r.eval_ctx(item, nt) else { break };
                ctx = next;
                cid = item;
                continue;
            }
            if let Some(l) = active.iter().find(|l| matches!(l.source, LayerSource::Text | LayerSource::Shape)) {
                hit.path.push(l.name.clone());
                hit.layers.push((cid, l.id));
                hit.covered = true;
            }
            break;
        }
        hit
    }
}

/// A precomp layer spanning (nearly) all of its comp: a presenter or BG precomp, not a scene.
fn is_backdrop(l: &Layer, parent: &Comp) -> bool {
    let dur = parent.duration.seconds();
    if !dur.is_finite() || dur <= 0.0 {
        return false;
    }
    let a = l.in_point.seconds().max(0.0);
    let b = l.out_point.seconds().min(dur);
    (b - a) >= dur * BACKDROP_SHARE
}

/// The comp holds two or more precomps that play one after another (scenes of a part), not on
/// top of each other (cards of a scene).
pub fn is_sequence(comp: &Comp) -> bool {
    let mut spans: Vec<(f64, f64)> = comp
        .layers
        .iter()
        .filter(|l| matches!(l.source, LayerSource::Comp { .. }) && l.switches.video && !l.switches.guide)
        .take(MAX_SEQUENCE_LAYERS)
        .map(|l| (l.in_point.seconds(), l.out_point.seconds()))
        .filter(|(a, b)| a.is_finite() && b.is_finite() && b > a)
        .collect();
    if spans.len() < 2 {
        return false;
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = spans.iter().map(|(a, b)| b - a).sum();
    let mut overlap = 0.0;
    let mut reach = f64::NEG_INFINITY;
    for (a, b) in &spans {
        overlap += (reach.min(*b) - a).max(0.0);
        reach = reach.max(*b);
    }
    overlap <= total * SEQUENCE_OVERLAP
}

/// What covers one line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineScenes {
    /// Every scene playing during the line, in order of first appearance (covered only).
    pub scenes: Vec<SceneHit>,
    /// The path where the voice runs bare (the first uncovered sample), for a gap's "where".
    pub bare: Option<SceneHit>,
    /// Seconds of the line's words a scene covers.
    pub covered_s: f64,
    /// Seconds of the line's words.
    pub said_s: f64,
}

impl LineScenes {
    /// No scene covers any of the line.
    pub fn gap(&self) -> bool {
        self.scenes.is_empty()
    }
}

/// The scenes covering a line's words: each word is sampled at its middle (at most
/// [`MAX_SAMPLES`] per line, spread evenly), weighted by its length.
pub fn line_scenes(f: &mut SceneFinder, words: &[SpeechWord]) -> LineScenes {
    let mut out = LineScenes::default();
    let n = words.len();
    if n == 0 {
        return out;
    }
    let step = n.div_ceil(MAX_SAMPLES).max(1);
    for chunk in words.chunks(step) {
        let Some(w) = chunk.get(chunk.len() / 2) else { continue };
        let len: f64 = chunk.iter().map(|w| (w.end - w.start).seconds().max(0.0)).sum();
        let mid = Tick(w.start.0.saturating_add((w.end.0.saturating_sub(w.start.0)).max(0) / 2));
        let hit = f.at(mid);
        out.said_s += len;
        if hit.covered {
            out.covered_s += len;
            if !out.scenes.iter().any(|s| s.layers == hit.layers) {
                out.scenes.push(hit);
            }
        } else if out.bare.is_none() {
            out.bare = Some(hit);
        }
    }
    out
}
