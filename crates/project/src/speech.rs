//! A comp's speech map: the words of its voice-over and their syllable onsets, stressed syllables
//! marked, in comp time. Made outside the app (`scripts/fork/speech-map`: transcript, forced
//! alignment, acoustic onset snap, or `speech.transcribe`, which runs it) and stored with
//! `speech.import`; agents read it to time animation peaks to stressed syllables and the
//! `off-accent` lint checks speed-graph peaks against it. It draws nothing
//! ([`crate::Comp::same_pixels`] ignores it).
//!
//! The words belong to their voice layer ([`SpeechMap::layer`], SX1): every edit runs
//! [`sync_project`], so moving or stretching the layer moves its words, precomposing it takes
//! them into the new comp (the parent comp keeps them through the precomp layer), and a copy
//! pasted into a comp without a map brings them along. Trimming hides the words outside the
//! layer's in/out ([`SpeechMap::voice_span`]) without deleting them, so trimming back shows them
//! again.
//!
//! The map also carries the comp's script (SX5, [`Script`]): the lines the speaker was meant to
//! say, lined up with the words by [`align_script`].

use std::sync::Arc;

use effectcraft_time::Tick;
use serde::{Deserialize, Serialize};

use crate::{Comp, ItemId, Layer, LayerId, LayerSource, Project};

/// Most words one map keeps (about 10 hours of speech).
pub const MAX_WORDS: usize = 200_000;
/// Most syllables one word keeps.
pub const MAX_SYLLABLES: usize = 16;
/// Most lines one script keeps.
pub const MAX_SCRIPT_LINES: usize = 20_000;
/// Most characters one script line keeps.
pub const MAX_LINE_CHARS: usize = 4_000;
/// How far [`align_script`] looks ahead (words, on each side) to find the speech again after a
/// word that differs.
const LOOKAHEAD: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeechMap {
    /// How it was made ("scribe_v1 + mfa english_us_arpa + forward snap").
    #[serde(default)]
    pub engine: String,
    /// The media file it maps.
    #[serde(default)]
    pub media: String,
    /// The layer whose audio it maps, when its media times were mapped through that layer. The
    /// words follow that layer ([`sync_project`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<LayerId>,
    /// Sorted by start.
    pub words: Vec<SpeechWord>,
    /// The comp's script, lined up with the words (`script.load`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<Script>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeechWord {
    pub word: String,
    /// Comp time.
    pub start: Tick,
    pub end: Tick,
    #[serde(default)]
    pub syllables: Vec<Syllable>,
    /// The syllables are estimated from the spelling and the word's timing ([`guess_syllables`]):
    /// the aligner's dictionary didn't know the word (a brand or a made-up word).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guessed: bool,
}

/// One syllable of a word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Syllable {
    /// Onset: the burst of a stop, the start of a hiss, else the start of voicing (comp time).
    pub t: Tick,
    /// Vowel onset (comp time, never before `t`).
    pub vowel: Tick,
    /// ARPAbet stress: 1 primary, 2 secondary, 0 unstressed.
    pub stress: u8,
}

impl SpeechWord {
    /// The word's first primary-stressed syllable (its index and the syllable).
    pub fn stressed(&self) -> Option<(usize, &Syllable)> {
        self.syllables.iter().enumerate().find(|(_, s)| s.stress == 1)
    }
}

impl SpeechMap {
    /// Primary-stressed syllables with their words, in time order.
    pub fn stressed(&self) -> Vec<(&SpeechWord, &Syllable)> {
        let mut out: Vec<_> = self.words.iter().flat_map(|w| w.syllables.iter().filter(|s| s.stress == 1).map(move |s| (w, s))).collect();
        out.sort_by_key(|(_, s)| s.t);
        out
    }

    /// Words overlapping `start..=end` (comp time).
    pub fn words_in(&self, start: Tick, end: Tick) -> impl Iterator<Item = &SpeechWord> {
        self.words.iter().filter(move |w| w.end >= start && w.start <= end)
    }

    /// The transcript, words joined by spaces.
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// Where the words can be heard in `comp`: its voice layer's in/out, or everywhere when the map
    /// has no voice layer (or the layer is gone), always inside the comp's own 0..duration (a voice
    /// layer longer than the comp is not heard past its end). Words outside it are trimmed away.
    pub fn voice_span(&self, comp: &Comp) -> (Tick, Tick) {
        let (a, b) = match self.layer.and_then(|l| comp.layer(l)) {
            Some(l) => (l.in_point, l.out_point),
            None => (Tick::MIN, Tick::MAX),
        };
        (a.max(Tick::ZERO), b.min(comp.duration))
    }

    /// Indices of the words audible in `comp` (inside [`Self::voice_span`]).
    pub fn audible(&self, comp: &Comp) -> impl Iterator<Item = (usize, &SpeechWord)> {
        let (a, b) = self.voice_span(comp);
        self.words.iter().enumerate().filter(move |(_, w)| w.end > a && w.start < b)
    }
}

// ---------------------------------------------------------------- SX1: words follow the layer

/// A layer's time mapping can't be followed (time remapping): its words stay where they are.
fn remapped(l: &Layer) -> bool {
    l.props.get("timeRemap").is_some_and(|t| !t.keys.is_empty() || t.has_expression())
}

fn same_timing(a: &Layer, b: &Layer) -> bool {
    a.start_time == b.start_time && (a.stretch - b.stretch).abs() < 1e-12
}

/// `map` moved from layer `from` (its timing before the edit) to layer `to` (after; another
/// comp's layer for a precompose or a copy), as voice layer `id`.
fn follow(map: &SpeechMap, from: &Layer, to: &Layer, id: LayerId) -> Arc<SpeechMap> {
    let t = |x: Tick| to.comp_time(from.layer_time(x));
    let mut out = map.clone();
    out.layer = Some(id);
    for w in &mut out.words {
        let (a, b) = (t(w.start), t(w.end));
        // A reversed layer (negative stretch) swaps a word's ends.
        (w.start, w.end) = (a.min(b), a.max(b));
        for s in &mut w.syllables {
            let (x, v) = (t(s.t), t(s.vowel));
            (s.t, s.vowel) = (x.min(v), x.max(v));
        }
        w.syllables.sort_by_key(|s| s.t);
    }
    out.words.sort_by_key(|w| w.start);
    Arc::new(out)
}

/// After an edit (`before` → `after`): each comp's speech map follows its voice layer. Called by
/// the engine's `Session::edit` for every undoable edit, like the other `sync_*` passes.
///
/// - The layer moved or was stretched: its words move with it.
/// - The layer left its comp for another one (Pre-compose ▸ Move all attributes keeps the layer
///   id): the new comp gets the words (unless it has a map), and the old comp keeps them through
///   the precomp layer that took its place.
/// - The layer was deleted and a copy of it (same source) is in the comp: the copy takes them.
/// - A copy of the layer was pasted or duplicated into another comp that has no map: that comp
///   gets the words, through the copy.
///
/// An edit that sets or removes a map itself (`speech.import`, `speech.clear`) is left alone, as
/// are time-remapped layers (their media time can't be followed).
pub fn sync_project(before: &Project, after: &mut Project) {
    let mut set: Vec<(ItemId, Arc<SpeechMap>)> = vec![];
    for (cid, cb) in before.comps() {
        let Some(map) = cb.speech.as_ref() else { continue };
        let Some(lid) = map.layer else { continue };
        let Some(lb) = cb.layer(lid) else { continue };
        let Some(ca) = after.comp(*cid) else { continue };
        // The edit set or removed this map itself: leave it.
        if !ca.speech.as_ref().is_some_and(|m| Arc::ptr_eq(m, map)) {
            continue;
        }
        if remapped(lb) {
            continue;
        }
        let gives = |set: &[(ItemId, Arc<SpeechMap>)], to: ItemId| set.iter().any(|(c, _)| *c == to);
        if let Some(la) = ca.layer(lid) {
            if !same_timing(lb, la) && !remapped(la) {
                set.push((*cid, follow(map, lb, la, lid)));
            }
        } else if let Some((did, la)) = after.comps().filter(|(id, _)| *id != cid).find_map(|(id, c)| c.layer(lid).map(|l| (*id, l))) {
            // Precomposed: the layer (same id) is in another comp now.
            let target_free = after.comp(did).is_some_and(|c| c.speech.is_none());
            if target_free && !remapped(la) && !gives(&set, did) {
                set.push((did, follow(map, lb, la, lid)));
            }
            // The parent comp keeps its words (they play at the same comp times) through the
            // precomp layer, which they follow from now on.
            if let Some(p) = ca.layers.iter().find(|l| matches!(l.source, LayerSource::Comp { item } if item == did)) {
                let mut kept = (**map).clone();
                kept.layer = Some(p.id);
                set.push((*cid, Arc::new(kept)));
            }
        } else if let Some(la) = ca.layers.iter().find(|l| l.source == lb.source && cb.layer(l.id).is_none() && !remapped(l)) {
            // Deleted, and a new copy of it is in the comp (cut and paste): the copy has them.
            set.push((*cid, follow(map, lb, la, la.id)));
        }
        // Copies pasted or duplicated into other comps that have no map of their own.
        for (did, cd) in after.comps() {
            if did == cid || cd.speech.is_some() || gives(&set, *did) {
                continue;
            }
            // Only comps that existed before: a new comp made from the layer is handled above.
            let Some(before_d) = before.comp(*did) else { continue };
            if let Some(la) = cd.layers.iter().find(|l| l.source == lb.source && before_d.layer(l.id).is_none() && !remapped(l)) {
                set.push((*did, follow(map, lb, la, la.id)));
            }
        }
    }
    for (cid, map) in set {
        if let Some(c) = after.comp_mut(cid) {
            c.speech = Some(map);
        }
    }
}

// ---------------------------------------------------------------- SX5: the script

/// The script of a comp's voice-over: the lines as written (`script.load`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Script {
    /// The file it was read from ("10-10 script.docx"), as shown in the Script panel.
    #[serde(default)]
    pub name: String,
    pub lines: Vec<ScriptLine>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptLine {
    pub text: String,
}

impl Script {
    /// A script from plain text: one line per non-empty text line (trimmed), at most
    /// [`MAX_SCRIPT_LINES`] of at most [`MAX_LINE_CHARS`] characters.
    pub fn from_text(name: &str, text: &str) -> Script {
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(MAX_SCRIPT_LINES)
            .map(|l| ScriptLine { text: l.chars().take(MAX_LINE_CHARS).collect() })
            .collect();
        Script { name: name.chars().take(400).collect(), lines }
    }
}

/// A word as compared between script and speech: lower case, letters, digits and apostrophes
/// only (curly ones straightened), so "Works," matches "works".
pub fn normalize(w: &str) -> String {
    w.chars()
        .map(|c| if c == '\u{2019}' || c == '\u{2018}' { '\'' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .trim_matches('\'')
        .to_string()
}

/// Syllables for a word the aligner's dictionary doesn't know ("YouTube", "TikTok"), estimated
/// from its spelling and timing: one per vowel group (`y` counts after the first letter; a final
/// silent `e` doesn't), each onset at the consonant before its vowels, letters spread evenly over
/// `start`..`end`. The first syllable takes the stress (English names mostly stress it). Empty for
/// a word without letters (a number is said as other words).
pub fn guess_syllables(word: &str, start: Tick, end: Tick) -> Vec<Syllable> {
    let letters: Vec<char> = word.chars().filter(|c| c.is_alphabetic()).flat_map(char::to_lowercase).take(64).collect();
    if letters.is_empty() {
        return vec![];
    }
    let vowel = |i: usize| letters.get(i).is_some_and(|c| "aeiouäöüéèêáàíóú".contains(*c) || (*c == 'y' && i > 0));
    // Vowel groups: (first letter, letter after the group).
    let mut groups: Vec<(usize, usize)> = vec![];
    let mut i = 0;
    while i < letters.len() {
        if vowel(i) {
            let a = i;
            while vowel(i) {
                i += 1;
            }
            groups.push((a, i));
        } else {
            i += 1;
        }
    }
    let n = letters.len();
    // A final lone `e` after a consonant is silent ("tube"), unless it makes "-le" ("table").
    if groups.len() > 1
        && groups.last().is_some_and(|&(a, b)| b == n && a + 1 == n && letters.get(a) == Some(&'e'))
        && letters.get(n.saturating_sub(2)) != Some(&'l')
    {
        groups.pop();
    }
    if groups.is_empty() {
        groups.push((0, n));
    }
    let (a, b) = (start.seconds(), end.seconds().max(start.seconds()));
    let at = |k: usize| Tick::from_seconds_f64(a + (b - a) * k as f64 / n as f64);
    let mut prev_end = 0;
    let mut out = Vec::with_capacity(groups.len());
    for (k, &(g, e)) in groups.iter().enumerate().take(MAX_SYLLABLES) {
        // The first syllable starts with the word; later ones take the consonant before their vowels.
        let onset = if k == 0 { 0 } else { g.saturating_sub(1).max(prev_end) };
        prev_end = e;
        let t = at(onset);
        out.push(Syllable { t, vowel: at(g).max(t), stress: u8::from(k == 0) });
    }
    out
}

/// One token of a script line lined up with the speech.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LineTok {
    /// A word of the script: `said` is the speech-map word it was said as (`None`: not said, drawn
    /// crossed out).
    Script { text: String, said: Option<usize> },
    /// A word said that the script doesn't have (an ad-lib, drawn green): the speech-map word.
    AdLib { word: usize },
}

/// A script line lined up with the speech.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AlignedLine {
    pub toks: Vec<LineTok>,
    /// The first and last speech-map words said on the line (`None`: none of it was said).
    pub first: Option<usize>,
    pub last: Option<usize>,
}

impl AlignedLine {
    /// Words of the script not said on this line.
    pub fn not_said(&self) -> usize {
        self.toks.iter().filter(|t| matches!(t, LineTok::Script { said: None, .. })).count()
    }
    /// Ad-libs on this line.
    pub fn adlibs(&self) -> usize {
        self.toks.iter().filter(|t| matches!(t, LineTok::AdLib { .. })).count()
    }
    /// The line was cut: none of its words was said.
    pub fn cut(&self) -> bool {
        self.first.is_none()
    }
    /// Comp time span of the words said (`words` = the map's words).
    pub fn span(&self, words: &[SpeechWord]) -> Option<(Tick, Tick)> {
        let a = words.get(self.first?)?.start;
        let b = words.get(self.last?)?.end;
        Some((a, b.max(a)))
    }
}

/// Line the script up with the words said: a word-by-word walk that, where they differ, looks up
/// to a few words ahead on both sides for the place they agree again (two words in a row), so a
/// word not said or an ad-lib doesn't throw off the rest. Linear in the length of both.
pub fn align_script(script: &Script, words: &[SpeechWord]) -> Vec<AlignedLine> {
    // Script tokens (line, text, normalised), punctuation-only tokens kept on their line unaligned.
    let mut toks: Vec<(usize, String, String)> = vec![];
    for (li, l) in script.lines.iter().enumerate() {
        for t in l.text.split_whitespace() {
            toks.push((li, t.to_string(), normalize(t)));
        }
    }
    let said: Vec<String> = words.iter().map(|w| normalize(&w.word)).collect();
    let mut lines: Vec<AlignedLine> = vec![AlignedLine::default(); script.lines.len()];
    let (n, m) = (toks.len(), said.len());
    let eq = |i: usize, j: usize| match (toks.get(i), said.get(j)) {
        (Some((_, _, a)), Some(b)) => !a.is_empty() && a == b,
        _ => false,
    };
    let push_said = |lines: &mut Vec<AlignedLine>, li: usize, text: String, j: Option<usize>| {
        if let Some(l) = lines.get_mut(li) {
            if let Some(j) = j {
                l.first = Some(l.first.map_or(j, |f| f.min(j)));
                l.last = Some(l.last.map_or(j, |x| x.max(j)));
            }
            l.toks.push(LineTok::Script { text, said: j });
        }
    };
    let push_adlib = |lines: &mut Vec<AlignedLine>, li: usize, j: usize| {
        if let Some(l) = lines.get_mut(li) {
            l.first = Some(l.first.map_or(j, |f| f.min(j)));
            l.last = Some(l.last.map_or(j, |x| x.max(j)));
            l.toks.push(LineTok::AdLib { word: j });
        }
    };
    // The line ad-libs go to: the line of the last script token placed, else the next one's.
    let line_of = |i: usize, last: Option<usize>| last.or_else(|| toks.get(i).map(|t| t.0)).unwrap_or(0);
    // SX5: where three script words in a row are said, for a resync past a long unscripted stretch
    // (an intro, a cut line): said trigram -> its word indices, ascending.
    let mut tri: std::collections::HashMap<[&str; 3], Vec<usize>> = std::collections::HashMap::new();
    for (k, t) in said.windows(3).enumerate() {
        if let [a, b, c] = t
            && !a.is_empty()
            && !b.is_empty()
            && !c.is_empty()
        {
            tri.entry([a.as_str(), b.as_str(), c.as_str()]).or_default().push(k);
        }
    }
    let far_resync = |i: usize, j: usize| -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for di in 0..n.saturating_sub(i) {
            if best.is_some_and(|(a, b)| di >= a + b) {
                break;
            }
            let key = (0..3).map(|k| toks.get(i + di + k).map(|t| t.2.as_str()).filter(|x| !x.is_empty())).collect::<Option<Vec<&str>>>();
            let Some([a, b, c]) = key.as_deref() else { continue };
            let Some(at) = tri.get(&[*a, *b, *c]) else { continue };
            let Some(&jj) = at.get(at.partition_point(|&x| x < j)) else { continue };
            if best.is_none_or(|(x, y)| di + (jj - j) < x + y) {
                best = Some((di, jj - j));
            }
        }
        best
    };
    let (mut i, mut j) = (0usize, 0usize);
    let mut last_line: Option<usize> = None;
    while i < n && j < m {
        let Some((li, text, norm)) = toks.get(i).cloned() else { break };
        if norm.is_empty() {
            push_said(&mut lines, li, text, None);
            last_line = Some(li);
            i += 1;
            continue;
        }
        if eq(i, j) {
            push_said(&mut lines, li, text, Some(j));
            last_line = Some(li);
            i += 1;
            j += 1;
            continue;
        }
        // The nearest place they agree again, confirmed by the next pair (or the end).
        let mut resync: Option<(usize, usize)> = None;
        'search: for total in 1..=2 * LOOKAHEAD {
            for di in 0..=total.min(LOOKAHEAD) {
                let dj = total - di;
                if dj > LOOKAHEAD {
                    continue;
                }
                let (a, b) = (i + di, j + dj);
                if eq(a, b) && (a + 1 >= n || b + 1 >= m || eq(a + 1, b + 1)) {
                    resync = Some((di, dj));
                    break 'search;
                }
            }
        }
        // Nothing near: the next place three words in a row agree; nothing anywhere: the rest of
        // the script isn't said (one wrong word used to throw every later line off).
        let Some((di, dj)) = resync.or_else(|| far_resync(i, j)) else { break };
        for k in i..(i + di).min(n) {
            if let Some((li, text, _)) = toks.get(k).cloned() {
                push_said(&mut lines, li, text, None);
                last_line = Some(li);
            }
        }
        // A long unscripted stretch (more than LOOKAHEAD words) isn't an ad-lib of a line.
        if dj <= LOOKAHEAD {
            for k in j..(j + dj).min(m) {
                push_adlib(&mut lines, line_of(i + di, last_line), k);
            }
        }
        i += di;
        j += dj;
    }
    for k in i..n {
        if let Some((li, text, _)) = toks.get(k).cloned() {
            push_said(&mut lines, li, text, None);
            last_line = Some(li);
        }
    }
    if m.saturating_sub(j) <= LOOKAHEAD {
        for k in j..m {
            push_adlib(&mut lines, line_of(n, last_line), k);
        }
    }
    lines
}

/// How strongly each word is said, 0..1, for the Script panel's emphasis glow: drawn-out words
/// (time per letter against the map's median) and words with a primary-stressed syllable count
/// more. TODO(batch1-verify): add loudness from the voice layer's waveform when the UI has it.
pub fn emphasis(words: &[SpeechWord]) -> Vec<f32> {
    let per_letter = |w: &SpeechWord| {
        let letters = w.word.chars().filter(|c| c.is_alphanumeric()).count().max(1) as f64;
        ((w.end - w.start).seconds() / letters).max(0.0)
    };
    let mut d: Vec<f64> = words.iter().map(per_letter).filter(|x| x.is_finite()).collect();
    d.sort_by(f64::total_cmp);
    let median = d.get(d.len() / 2).copied().unwrap_or(0.06).max(1e-3);
    words
        .iter()
        .map(|w| {
            let drawn = ((per_letter(w) / median - 0.8) / 1.5).clamp(0.0, 1.0) * 0.6;
            let stressed = if w.syllables.iter().any(|s| s.stress == 1) && w.syllables.len() > 1 { 0.25 } else { 0.0 };
            let loud = if w.word.chars().filter(|c| c.is_alphabetic()).count() > 1 && w.word.chars().all(|c| !c.is_lowercase()) { 0.15 } else { 0.0 };
            (drawn + stressed + loud).clamp(0.0, 1.0) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_time::FrameRate;

    fn w(word: &str, a: f64, b: f64) -> SpeechWord {
        SpeechWord { word: word.into(), start: Tick::from_seconds_f64(a), end: Tick::from_seconds_f64(b), ..Default::default() }
    }

    #[test]
    fn a_script_lines_up_with_words_not_said_and_adlibs() {
        let script = Script::from_text("s.txt", "But this one works every single time.\nHere's what these pages have in common.");
        let words: Vec<SpeechWord> =
            ["this", "one", "really", "works", "every", "single", "time"].iter().enumerate().map(|(i, x)| w(x, i as f64, i as f64 + 0.5)).collect();
        let lines = align_script(&script, &words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].not_said(), 1, "“But” wasn't said");
        assert_eq!(lines[0].adlibs(), 1, "“really” is an ad-lib");
        assert_eq!((lines[0].first, lines[0].last), (Some(0), Some(6)));
        assert!(lines[1].cut());
    }

    #[test]
    fn a_script_finds_its_words_after_a_long_unscripted_intro() {
        // BUILD2 SX5: a voice-over with 30 unscripted words before the script's first line put
        // every script word on "not said" (the look-ahead is 8 words) and the intro on line 1.
        let script = Script::from_text("s.txt", "Studying the best AI influencer pages in the whole world.\nFrom everyday people posting.");
        let mut said: Vec<String> = (0..30).map(|k| format!("intro{k}")).collect();
        said.extend("studying the best ai influencer pages in the world from everyday people posting and more".split(' ').map(String::from));
        let words: Vec<SpeechWord> = said.iter().enumerate().map(|(i, x)| w(x, i as f64, i as f64 + 0.5)).collect();
        let lines = align_script(&script, &words);
        assert_eq!(lines[0].not_said(), 1, "only “whole” wasn't said");
        assert_eq!(lines[0].adlibs(), 0, "the intro isn't an ad-lib of line 1");
        assert_eq!((lines[0].first, lines[0].last), (Some(30), Some(38)));
        assert_eq!((lines[1].first, lines[1].not_said()), (Some(39), 0));
        // A script that is never said stays unsaid (and fast).
        let other = Script::from_text("o.txt", "Nothing here matches at all today.");
        let l = align_script(&other, &words);
        assert_eq!(l[0].not_said(), 6);
        assert!(l[0].cut());
    }

    #[test]
    fn words_past_the_comp_end_or_before_its_start_are_not_audible() {
        // BUILD2 AX7: a 575 s voice layer in a 12 s comp; Find word counted (and jumped to) words
        // after the comp's end.
        let mut comp = Comp::new(1920, 1080, FrameRate::new(30, 1), Tick::from_seconds_f64(12.0));
        let map = SpeechMap { words: vec![w("before", -2.0, -1.5), w("in", 1.0, 1.5), w("after", 13.0, 13.5)], ..Default::default() };
        let heard: Vec<&str> = map.audible(&comp).map(|(_, x)| x.word.as_str()).collect();
        assert_eq!(heard, ["in"]);
        comp.duration = Tick::from_seconds_f64(20.0);
        assert_eq!(map.audible(&comp).count(), 2);
    }

    #[test]
    fn normalize_ignores_case_and_punctuation() {
        assert_eq!(normalize("Works,"), "works");
        assert_eq!(normalize("Here’s"), "here's");
        assert_eq!(normalize("—"), "");
    }
}
