//! TEAM1: shadow compositions of the editing team (Ghostex EDITING, 28 Oct 2026).
//!
//! A part agent never edits the user's live composition. It works on a **shadow**: a deep copy of
//! the scene comp and every comp nested in it, kept in the hidden "Shadows" folder and named
//! `S1 06 ✎A2`. Once the shadow passes its checks it is swapped into the live comp as one undo
//! step (the live comp keeps its id, name and uses). This file holds what the project file keeps
//! about each shadow: the link to its source, the source's fingerprint when the copy was made
//! (so an edit the user makes to the source later shows the shadow is stale), the review status
//! and the check results. The engine's `shadow.*` commands (crates/engine/src/commands/shadow.rs)
//! do the work.

use serde::{Deserialize, Serialize};

use crate::ItemId;

/// The name of the folder the shadows live in.
pub const FOLDER_NAME: &str = "Shadows";

/// Where a shadow is in its review. `swapped` is not a status of its own: it is
/// [`ShadowLink::swapped`] (undo of the swap brings the status back).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShadowStatus {
    /// The agent is working on it.
    #[default]
    Working,
    /// Its checks are running.
    Checking,
    /// Every enabled check passed: it can be swapped in.
    Passed,
    /// A check failed: the lead sends the agent a follow-up.
    Failed,
}

impl ShadowStatus {
    pub fn from_name(s: &str) -> Option<ShadowStatus> {
        match s.trim().to_ascii_lowercase().as_str() {
            "working" => Some(ShadowStatus::Working),
            "checking" => Some(ShadowStatus::Checking),
            "passed" | "pass" | "ok" => Some(ShadowStatus::Passed),
            "failed" | "fail" => Some(ShadowStatus::Failed),
            _ => None,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            ShadowStatus::Working => "working",
            ShadowStatus::Checking => "checking",
            ShadowStatus::Passed => "passed",
            ShadowStatus::Failed => "failed",
        }
    }
}

/// One check's result, as the lead recorded it (`lint`, `taste`, `render`, `review`…).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShadowCheck {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A layer's fingerprint: its whole hash and one hash per top-level property group (Transform,
/// Effects, Masks, Contents, Text…), so a changed layer can say what changed after a reopen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerPrint {
    pub id: u64,
    pub name: String,
    pub hash: u64,
    /// (group match path, hash), in stack order of the groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<(String, u64)>,
}

/// A composition's fingerprint.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompPrint {
    pub comp: ItemId,
    pub name: String,
    /// The whole composition (settings, markers, layers).
    pub hash: u64,
    /// The composition without its layers (size, duration, rate, markers, work area…).
    pub settings: u64,
    #[serde(default)]
    pub layers: Vec<LayerPrint>,
}

/// One shadow and its link to the live composition.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShadowLink {
    /// The shadow's root composition (the copy of `source`): the shadow's id.
    pub shadow: ItemId,
    /// The live composition it was copied from (and is swapped into).
    pub source: ItemId,
    /// The agent working on it (`A2`, `chapter-3`…).
    pub agent: String,
    /// The folder the copies live in.
    pub folder: Option<ItemId>,
    /// (live comp, its copy), the source first: the source and every comp nested in it.
    pub copies: Vec<(ItemId, ItemId)>,
    /// The source's fingerprint when the copy was made (after a swap: the live comp as the swap
    /// left it), one entry per comp of `copies` (by live comp).
    pub base: Vec<CompPrint>,
    /// The engine's document revision when the copy was made (informative: revisions restart
    /// when the project is opened again).
    #[serde(default)]
    pub base_revision: u64,
    #[serde(default)]
    pub status: ShadowStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ShadowCheck>,
    /// The shadow's contents are in the live comp; the shadow holds what the live comp was
    /// before (Before), so swapping again (`shadow.revert`) restores it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub swapped: bool,
    /// How many times it was swapped in or out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub swaps: u32,
    /// Nobody works on it for now (`shadow.release`: the team run stopped, the agent is done or
    /// asleep): the shadow is kept but its source is no longer claimed. Rebasing it or setting it
    /// back to working / checking takes it up again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ShadowLink {
    /// The copy of live comp `live`.
    pub fn copy_of(&self, live: ItemId) -> Option<ItemId> {
        self.copies.iter().find(|(l, _)| *l == live).map(|(_, c)| *c)
    }
    /// Whether `item` is one of this shadow's copies.
    pub fn owns(&self, item: ItemId) -> bool {
        self.copies.iter().any(|(_, c)| *c == item)
    }
}

/// 64-bit FNV-1a: a stable hash (the same in every build and on every platform), so a
/// fingerprint saved in a project still compares after the project is opened again.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The stable hash of a value's JSON (0 if it can't be written, which a project value can).
pub fn hash_json<T: Serialize + ?Sized>(v: &T) -> u64 {
    serde_json::to_vec(v).map(|b| fnv1a(&b)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn status_names_round_trip() {
        for s in [ShadowStatus::Working, ShadowStatus::Checking, ShadowStatus::Passed, ShadowStatus::Failed] {
            assert_eq!(ShadowStatus::from_name(s.id()), Some(s));
        }
        assert_eq!(ShadowStatus::from_name("swapped"), None);
    }

    #[test]
    fn link_reads_back() {
        let l = ShadowLink { shadow: ItemId(9), source: ItemId(2), agent: "A2".into(), copies: vec![(ItemId(2), ItemId(9))], ..Default::default() };
        let back: ShadowLink = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.copy_of(ItemId(2)), Some(ItemId(9)));
        assert!(back.owns(ItemId(9)) && !back.owns(ItemId(2)));
    }
}
