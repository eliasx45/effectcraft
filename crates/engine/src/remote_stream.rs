//! The playback stream between the desktop app and a cloud frame server (`plan/cloud-render.md`
//! CR6): instead of one request per frame, the app asks for a comp to be *played* from a time,
//! and the server renders ahead of the playhead in play order and sends the frames back as
//! self-contained chunks of about half a second (one closed H.264 GOP each,
//! `effectcraft_media::preview_stream`).
//!
//! Messages ride the frame channel ([`crate::remote_wire`]) as [`FrameMsg::Stream`] /
//! [`FrameReply::Stream`]; a chunk's encoded frames are the reply's payload.
//!
//! - **Timing.** Chunks sit on a grid of [`StreamMsg::Start`]'s `chunkFrames` frames in the comp's
//!   frame rate, anchored at the frame playback starts from (Start's `from`, or the latest
//!   [`StreamMsg::Seek`]): chunk `k` holds frames `a + k·n … a + k·n + n − 1` (`k` is negative
//!   before the anchor, in a loop; chunks at the ends of the range may be shorter). Frame `i` of a
//!   chunk is at comp time `rate.tick_of(first + i)`; [`StreamReply::Chunk`] carries `first` and
//!   `start`.
//! - **Flow control.** The app reports its playhead and the chunks it has
//!   ([`StreamMsg::Playhead`], a few times a second). The server keeps at most `aheadSecs` of
//!   chunks ahead of that playhead and sends them in play order.
//! - **Adaptive resolution.** While the lead (what the app has beyond its playhead) is short,
//!   chunks render at half or quarter of the asked size ([`StreamReply::Chunk::scale`]); once it is
//!   ahead again, full size, and when idle the server sends the low-resolution chunks still ahead
//!   of the playhead again at full size. The newest chunk for a time wins on the app's side.
//! - **Edits.** A sync ([`FrameMsg::Patch`]) moves the replica to a new revision. Chunks from the
//!   first time the edit can change (worked out from the changed layers, or given by
//!   [`StreamMsg::Invalidate`]) on are rendered again; [`StreamReply::Stale`] tells the app which
//!   of its chunks are old.
//! - Chunks are opaque (composited over the comp's background colour on the server).

use serde::{Deserialize, Serialize};

use crate::project::{ItemId, ItemKind, LayerSource, Project};
#[cfg(doc)]
use crate::remote::{FrameMsg, FrameReply};
use crate::render::RenderOpts;
use crate::time::{FrameRate, Tick};

/// Default frames per chunk (≈ 0.5 s at 29.97 fps).
pub const DEFAULT_CHUNK_FRAMES: u32 = 15;
/// Default seconds of chunks the server keeps ahead of the app's playhead.
pub const DEFAULT_AHEAD_SECS: f32 = 4.0;
/// Most frames per chunk.
pub const MAX_CHUNK_FRAMES: u32 = 120;

fn default_chunk_frames() -> u32 {
    DEFAULT_CHUNK_FRAMES
}
fn default_ahead() -> f32 {
    DEFAULT_AHEAD_SECS
}
fn yes() -> bool {
    true
}
fn default_crf() -> f32 {
    23.0
}

/// How a stream's chunks are encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamCodec {
    /// One closed H.264 GOP per chunk (Annex B, SPS/PPS + IDR first, no B frames).
    #[default]
    H264,
}

/// App → server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum StreamMsg {
    /// Play `comp` (of the replica at `revision`) from `from`. Starting a stream with the id of a
    /// running one restarts it.
    #[serde(rename_all = "camelCase")]
    Start {
        stream: u64,
        revision: u64,
        comp: ItemId,
        from: Tick,
        /// Where playback ends (exclusive); `None`: the comp's work area end.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Tick>,
        /// Start of the range played again when `looping` (default: the work area start).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loop_from: Option<Tick>,
        #[serde(default)]
        looping: bool,
        /// The viewer's render options (`scale`: its resolution, 1 = Full).
        opts: Box<RenderOpts>,
        #[serde(default)]
        codec: StreamCodec,
        /// H.264 constant quality (lower is better; 23 by default).
        #[serde(default = "default_crf")]
        crf: f32,
        #[serde(default = "default_chunk_frames")]
        chunk_frames: u32,
        #[serde(default = "default_ahead")]
        ahead_secs: f32,
        /// Lower the resolution while the app's lead is short.
        #[serde(default = "yes")]
        adaptive: bool,
        /// Largest frame sent (the viewer's size); frames are scaled to fit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_size: Option<[u32; 2]>,
    },
    /// The playhead jumped: play from `time`.
    Seek {
        stream: u64,
        time: Tick,
    },
    /// The app's playhead and what it holds: `received`, the newest chunk sequence number it has
    /// (or `None`). About every 100–250 ms while playing.
    Playhead {
        stream: u64,
        time: Tick,
        #[serde(default = "yes")]
        playing: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        received: Option<u64>,
    },
    /// Frames from `from` on changed (the app knows best which); render them again.
    Invalidate {
        stream: u64,
        from: Tick,
    },
    Stop {
        stream: u64,
    },
}

impl StreamMsg {
    pub fn stream(&self) -> u64 {
        match self {
            StreamMsg::Start { stream, .. }
            | StreamMsg::Seek { stream, .. }
            | StreamMsg::Playhead { stream, .. }
            | StreamMsg::Invalidate { stream, .. }
            | StreamMsg::Stop { stream } => *stream,
        }
    }
}

/// Server → app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum StreamReply {
    /// The stream runs: full-size frames are `width`×`height`.
    #[serde(rename_all = "camelCase")]
    Started {
        stream: u64,
        width: u32,
        height: u32,
        frame_rate: FrameRate,
        chunk_frames: u32,
        codec: StreamCodec,
    },
    /// One chunk; its encoded frames are the payload. `seq` grows with every chunk sent (also a
    /// chunk sent again). `scale`: its size relative to the full size of [`StreamReply::Started`].
    #[serde(rename_all = "camelCase")]
    Chunk {
        stream: u64,
        seq: u64,
        revision: u64,
        /// The chunk's index on the grid (see the module docs).
        index: i64,
        /// Comp frame index of its first frame, and that frame's time.
        first: i64,
        start: Tick,
        frames: u32,
        frame_rate: FrameRate,
        width: u32,
        height: u32,
        scale: f32,
        codec: StreamCodec,
        /// Render time of all its frames (wall, frames in parallel) and encode time.
        render_ms: f64,
        encode_ms: f64,
    },
    /// Chunks of revisions before `revision` from `from` on are out of date; new ones follow.
    Stale {
        stream: u64,
        revision: u64,
        from: Tick,
    },
    /// Every chunk up to `at` (the end of the range) is sent; the stream stays open for a seek.
    End {
        stream: u64,
        at: Tick,
    },
    Failed {
        stream: u64,
        error: String,
    },
}

/// Chunk `index` of the grid of `n` frames anchored at comp frame `anchor`: its first frame and
/// frame count inside `[lo, hi)` (comp frame indices).
pub fn chunk_frames(index: i64, n: u32, anchor: i64, lo: i64, hi: i64) -> Option<(i64, u32)> {
    let n = i64::from(n.clamp(1, MAX_CHUNK_FRAMES));
    let first = anchor.checked_add(index.checked_mul(n)?)?;
    let (a, b) = (first.max(lo), first.saturating_add(n).min(hi));
    (a < b).then(|| (a, u32::try_from(b - a).unwrap_or(0)))
}

/// The index of the chunk holding comp frame `frame` on the grid anchored at `anchor`.
pub fn chunk_of(frame: i64, n: u32, anchor: i64) -> i64 {
    frame.saturating_sub(anchor).div_euclid(i64::from(n.clamp(1, MAX_CHUNK_FRAMES)))
}

/// Most precomp levels followed by [`first_change`].
const MAX_DEPTH: usize = 32;

/// The earliest time of `comp` whose picture can differ between `old` and `new` (snapshots of
/// one project, sharing what did not change: comps and layers are compared by pointer first),
/// or `None` when nothing it shows changed. Conservative: a changed layer counts from its
/// in-point (keyframe edits change values back to the previous key), a layer that parents or
/// mattes it counts too, a precomp layer counts whole when anything inside it changed, and a
/// change outside the comps (settings, footage, items) counts from the start.
///
/// Expressions that read other layers are not followed.
pub fn first_change(old: &Project, new: &Project, comp: ItemId) -> Option<Tick> {
    if old.settings != new.settings {
        return Some(Tick(i64::MIN));
    }
    let mut memo = std::collections::HashMap::new();
    changed_from(old, new, comp, 0, &mut memo).flatten()
}

/// The project item a layer shows, if any.
fn source_item(s: &LayerSource) -> Option<ItemId> {
    match s {
        LayerSource::Footage { item } | LayerSource::Comp { item } | LayerSource::Solid { item } | LayerSource::Model { item } => Some(*item),
        _ => None,
    }
}

/// `Some(None)`: nothing changed; `Some(Some(t))`: changed from `t`; `None`: not known (depth).
fn changed_from(old: &Project, new: &Project, comp: ItemId, depth: usize, memo: &mut std::collections::HashMap<ItemId, Option<Tick>>) -> Option<Option<Tick>> {
    let start = Some(Tick(i64::MIN));
    if depth > MAX_DEPTH {
        return Some(start);
    }
    if let Some(m) = memo.get(&comp) {
        return Some(*m);
    }
    let (Some(oi), Some(ni)) = (old.items.get(&comp), new.items.get(&comp)) else { return Some(start) };
    let (ItemKind::Comp(oc), ItemKind::Comp(nc)) = (&oi.kind, &ni.kind) else { return Some(start) };
    // Mark as in progress (a cycle counts as unchanged on the way back).
    memo.insert(comp, None);
    let same_shell = std::sync::Arc::ptr_eq(oc, nc)
        || (oc.width == nc.width
            && oc.height == nc.height
            && oc.frame_rate == nc.frame_rate
            && oc.background == nc.background
            && oc.shutter_angle == nc.shutter_angle
            && oc.shutter_phase == nc.shutter_phase
            && oc.motion_blur_samples == nc.motion_blur_samples
            && oc.renderer == nc.renderer);
    if !same_shell {
        memo.insert(comp, start);
        return Some(start);
    }
    let mut earliest: Option<Tick> = None;
    let take = |t: Tick, e: &mut Option<Tick>| *e = Some(e.map_or(t, |x: Tick| x.min(t)));
    let old_by_id: std::collections::HashMap<_, _> = oc.layers.arcs().iter().map(|l| (l.id, l)).collect();
    let new_ids: std::collections::HashSet<_> = nc.layers.arcs().iter().map(|l| l.id).collect();
    let mut changed_ids = std::collections::HashSet::new();
    // Removed layers.
    for l in oc.layers.arcs() {
        if !new_ids.contains(&l.id) {
            take(l.in_point.min(l.out_point), &mut earliest);
            changed_ids.insert(l.id);
        }
    }
    let old_order: Vec<_> = oc.layers.arcs().iter().map(|l| l.id).filter(|id| new_ids.contains(id)).collect();
    let new_order: Vec<_> = nc.layers.arcs().iter().map(|l| l.id).filter(|id| old_by_id.contains_key(id)).collect();
    if old_order != new_order {
        // Reordered: the stack changed everywhere the moved layers are; take every layer.
        for l in nc.layers.arcs() {
            take(l.in_point.min(l.out_point), &mut earliest);
        }
    }
    for l in nc.layers.arcs() {
        let lo = l.in_point.min(l.out_point);
        match old_by_id.get(&l.id) {
            None => {
                take(lo, &mut earliest);
                changed_ids.insert(l.id);
            }
            Some(o) if !std::sync::Arc::ptr_eq(o, l) && ***o != **l => {
                take(lo.min(o.in_point.min(o.out_point)), &mut earliest);
                changed_ids.insert(l.id);
            }
            Some(_) => {
                if let Some(src) = source_item(&l.source) {
                    let inside = match (old.items.get(&src), new.items.get(&src)) {
                        (Some(a), Some(b)) => match (&a.kind, &b.kind) {
                            (ItemKind::Comp(_), ItemKind::Comp(_)) => changed_from(old, new, src, depth + 1, memo)?,
                            (x, y) if x == y && a.proxy == b.proxy => None,
                            _ => Some(lo),
                        },
                        (None, None) => None,
                        _ => Some(lo),
                    };
                    if inside.is_some() {
                        take(lo, &mut earliest);
                        changed_ids.insert(l.id);
                    }
                }
            }
        }
    }
    // Layers that follow a changed layer (parent, track matte) change with it.
    if !changed_ids.is_empty() {
        for _ in 0..MAX_DEPTH {
            let mut grew = false;
            for l in nc.layers.arcs() {
                if changed_ids.contains(&l.id) {
                    continue;
                }
                let follows = l.parent.is_some_and(|p| changed_ids.contains(&p)) || l.track_matte.as_ref().is_some_and(|m| changed_ids.contains(&m.layer));
                if follows {
                    take(l.in_point.min(l.out_point), &mut earliest);
                    changed_ids.insert(l.id);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
    }
    memo.insert(comp, earliest);
    Some(earliest)
}
