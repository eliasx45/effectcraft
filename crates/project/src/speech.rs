//! A comp's speech map: the words of its voice-over and their syllable onsets, stressed syllables
//! marked, in comp time. Made outside the app (`scripts/fork/speech-map`: transcript, forced
//! alignment, acoustic onset snap) and stored with `speech.import`; agents read it to time
//! animation peaks to stressed syllables and the `off-accent` lint checks speed-graph peaks
//! against it. It draws nothing ([`crate::Comp::same_pixels`] ignores it).

use effectcraft_time::Tick;
use serde::{Deserialize, Serialize};

use crate::LayerId;

/// Most words one map keeps (about 10 hours of speech).
pub const MAX_WORDS: usize = 200_000;
/// Most syllables one word keeps.
pub const MAX_SYLLABLES: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeechMap {
    /// How it was made ("scribe_v1 + mfa english_us_arpa + forward snap").
    #[serde(default)]
    pub engine: String,
    /// The media file it maps.
    #[serde(default)]
    pub media: String,
    /// The layer whose audio it maps, when its media times were mapped through that layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<LayerId>,
    /// Sorted by start.
    pub words: Vec<SpeechWord>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeechWord {
    pub word: String,
    /// Comp time.
    pub start: Tick,
    pub end: Tick,
    #[serde(default)]
    pub syllables: Vec<Syllable>,
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
}
