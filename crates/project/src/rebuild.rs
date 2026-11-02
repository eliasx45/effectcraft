//! A comp that rebuilds a reference animation (`taste.rebuild`, 2 Nov 2026): where the reference
//! is, and per keyed move the ease the reference editor used, as fitted from the reference by the
//! effectcraft-motion skill's `fit_graph.py` (a Graph's name or a custom ease) with the fit's
//! error. The taste check judges those moves against their reference instead of Elias's profile
//! (his rules are then reported as info only), so a faithful rebuild of an early-peaking reference
//! can be handed over. It draws nothing ([`crate::Comp::same_pixels`] ignores it) and is saved
//! with the comp, so it survives save/load and copying the comp into another project; moves are
//! found by layer name and property path, not ids, for the same reason.
//!
//! The JSON is the same as `taste.rebuild` takes (and its `file`):
//!
//! ```json
//! {"reference": "~/Downloads/refs/kunal/TR.mp4", "name": "kunal TR", "tolerance": 5,
//!  "moves": [{"layer": "KTR Cam X", "path": "transform/position", "key": 4,
//!             "graph": "free:0.40:0.91", "fit": {"rms": 2.1, "worst": 5.0, "unit": "px"},
//!             "samples": "fit/cam_x.json"}]}
//! ```

use serde::{Deserialize, Serialize};

/// Most moves one rebuild keeps.
pub const MAX_MOVES: usize = 4000;

/// A comp's reference and the reference editor's fitted eases.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rebuild {
    /// The reference: a path or URL (a video, a frame folder, a project).
    pub reference: String,
    /// A short name for it ("kunal TR"); candidates filed from its eases are tagged
    /// `reference <name>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Largest gap (% of the move's progress) between a keyed ease and its reference fit that
    /// still matches. Default: the taste profile's `matchWithin` (5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub moves: Vec<RefMove>,
}

impl Rebuild {
    /// The name moves and candidates are labelled with: `name`, else the reference's file stem.
    pub fn label(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        let tail = self.reference.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("");
        let stem = tail.rsplit_once('.').map_or(tail, |(s, _)| s);
        if stem.is_empty() { "reference".into() } else { stem.to_string() }
    }
}

/// One keyed move's evidence: which move, and the reference's ease for it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RefMove {
    /// The layer's name (or `#<id>`).
    pub layer: String,
    /// The property's path as taste.check prints it (`transform/position`; a leading `layer/`,
    /// as check_graphs.py prints it, or the last part alone, `position`, also match).
    pub path: String,
    /// The move's first key (0-based). Without `key` and `frame`, every move of the property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<usize>,
    /// Or the comp frame of the move's first key (within a frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<f64>,
    /// The fitted curve: a Graph or profile curve name or id ("Quick launch · soft landing",
    /// "Expo In-Out (Elias)"), fit_graph.py's `free:<out>:<in>` (influences as fractions, speed 0
    /// at both keys), or `linear`. Or give `ease`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graph: String,
    /// The fitted ease itself: normalised speed (× the move's average speed) and influence (%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ease: Option<RefEase>,
    /// How well that curve reproduces the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<FitError>,
    /// Where the measured samples are (`fit/cam_x.json`), for people re-checking it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// One side of a fitted ease.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefSide {
    #[serde(default)]
    pub speed: f64,
    pub influence: f64,
}

/// A fitted ease: the first key's out, the second key's in.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefEase {
    pub out: RefSide,
    #[serde(rename = "in")]
    pub inn: RefSide,
}

/// A fit's error, as fit_graph.py prints it ("fit RMS 2.10, worst 5.00").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitError {
    pub rms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst: Option<f64>,
    /// `px`: the property's own units (pixels, %, degrees), as fit_graph.py measures them; `%`:
    /// percent of the move's travel.
    #[serde(default = "px")]
    pub unit: String,
}

fn px() -> String {
    "px".into()
}
