//! Shift-snapping in the Timeline's time graph, as in After Effects: while a layer bar is moved,
//! an In / Out point trimmed, a source slipped or keyframes dragged, holding Shift snaps the
//! dragged edge (or the grabbed key) to the nearest snap target within [`SNAP_PX`] pixels: the
//! current-time indicator, visible keyframes, other layers' In / Out points, layer and
//! composition markers, the work area's ends and the composition's start and end. Whatever the
//! drag moves is no target. The toolbar Snapping switch is the Composition viewer's and has no
//! say here (After Effects).
//!
//! The pure part (targets, tolerance, resolution) lives here so it can be tested without a UI;
//! `timeline.rs` calls it from the drags and draws the guide line at the target.

use effectcraft_engine::project::{Comp, LayerId};
use effectcraft_engine::time::Tick;

use crate::state::SnapGuide;

/// How close (screen pixels) a dragged edge or key must come to a target to snap.
pub(crate) const SNAP_PX: f64 = 8.0;

/// What a snap target is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Cti,
    Key,
    LayerIn,
    LayerOut,
    CompMarker,
    LayerMarker,
    WorkAreaStart,
    WorkAreaEnd,
    CompStart,
    CompEnd,
}

impl Kind {
    /// The id agents read in `snap_guide.kind`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Kind::Cti => "cti",
            Kind::Key => "key",
            Kind::LayerIn => "layerIn",
            Kind::LayerOut => "layerOut",
            Kind::CompMarker => "compMarker",
            Kind::LayerMarker => "layerMarker",
            Kind::WorkAreaStart => "workAreaStart",
            Kind::WorkAreaEnd => "workAreaEnd",
            Kind::CompStart => "compStart",
            Kind::CompEnd => "compEnd",
        }
    }
}

/// A time a drag can snap to (comp seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Target {
    pub time: f64,
    pub kind: Kind,
    /// The layer the in / out point, layer marker or key belongs to.
    pub layer: Option<LayerId>,
}

impl Target {
    pub(crate) fn guide(&self) -> SnapGuide {
        SnapGuide { time: self.time, kind: self.kind.id().into(), layer: self.layer.map(|l| l.0) }
    }
}

/// What the drag itself moves, which therefore can't be a target.
#[derive(Clone, Debug, Default)]
pub(crate) struct Skip {
    /// Layers whose In / Out points move (moved or trimmed layers).
    pub edges: Vec<LayerId>,
    /// Layers whose keys and layer markers move with them (moved or slipped layers).
    pub content: Vec<LayerId>,
    /// Keys being dragged: layer, property uid, key time (layer time).
    pub keys: Vec<(LayerId, u64, Tick)>,
}

/// The snap distance in seconds at `pps` pixels per second (the current zoom): [`SNAP_PX`]
/// pixels, so zooming in narrows it in time. No snapping at a broken zoom.
pub(crate) fn tolerance(pps: f64) -> f64 {
    if pps.is_finite() && pps > 0.0 { SNAP_PX / pps } else { 0.0 }
}

/// Every snap target of `comp`, in priority order (an exact tie goes to the earlier one: the
/// current time first). `key_props` are the property rows shown in the Timeline (layer, uid):
/// only visible keys are targets, as in After Effects. `cti` is the current time (comp seconds).
pub(crate) fn targets(comp: &Comp, key_props: &[(LayerId, u64)], cti: f64, skip: &Skip) -> Vec<Target> {
    let mut v = vec![Target { time: cti, kind: Kind::Cti, layer: None }];
    for (lid, uid) in key_props {
        if skip.content.contains(lid) {
            continue;
        }
        let Some(l) = comp.layer(*lid) else { continue };
        let Some(p) = l.props.find(*uid) else { continue };
        for k in &p.keys {
            if skip.keys.iter().any(|(sl, su, st)| sl == lid && su == uid && *st == k.time) {
                continue;
            }
            v.push(Target { time: l.comp_time(k.time).seconds(), kind: Kind::Key, layer: Some(*lid) });
        }
    }
    for l in &comp.layers {
        if !skip.edges.contains(&l.id) {
            v.push(Target { time: l.in_point.seconds(), kind: Kind::LayerIn, layer: Some(l.id) });
            v.push(Target { time: l.out_point.seconds(), kind: Kind::LayerOut, layer: Some(l.id) });
        }
    }
    v.extend(comp.markers.iter().map(|m| Target { time: m.time.seconds(), kind: Kind::CompMarker, layer: None }));
    for l in &comp.layers {
        if !skip.content.contains(&l.id) {
            v.extend(l.markers.iter().map(|m| Target { time: l.comp_time(m.time).seconds(), kind: Kind::LayerMarker, layer: Some(l.id) }));
        }
    }
    v.push(Target { time: comp.work_area.0.seconds(), kind: Kind::WorkAreaStart, layer: None });
    v.push(Target { time: comp.work_area.1.seconds(), kind: Kind::WorkAreaEnd, layer: None });
    v.push(Target { time: 0.0, kind: Kind::CompStart, layer: None });
    v.push(Target { time: comp.duration.seconds(), kind: Kind::CompEnd, layer: None });
    v.retain(|t| t.time.is_finite());
    v
}

/// A snap: add `delta` (seconds) to the drag to put `moving[index]` on `target`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hit {
    pub delta: f64,
    pub target: Target,
    /// Which of the moving times snapped.
    #[cfg_attr(not(test), allow(dead_code))]
    pub index: usize,
}

/// The closest (moving time, target) pair within `tol` seconds, if any. `moving` are the dragged
/// times where the pointer would put them (a moved bar's In and Out point, a trimmed edge, a
/// grabbed key). On an exact tie the earlier moving time, then the earlier target, wins.
pub(crate) fn resolve(targets: &[Target], moving: &[f64], tol: f64) -> Option<Hit> {
    if !(tol.is_finite() && tol > 0.0) {
        return None;
    }
    let mut best: Option<Hit> = None;
    for (index, m) in moving.iter().enumerate() {
        if !m.is_finite() {
            continue;
        }
        for t in targets {
            let delta = t.time - m;
            if delta.abs() <= tol && best.is_none_or(|b| delta.abs() < b.delta.abs()) {
                best = Some(Hit { delta, target: *t, index });
            }
        }
    }
    best
}

/// The property rows (layer, uid) whose keys the Timeline shows.
pub(super) fn key_props(rows: &[super::Row]) -> Vec<(LayerId, u64)> {
    rows.iter()
        .filter_map(|r| match r.kind {
            super::RowKind::Prop { uid } => Some((r.layer, uid)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(t: f64) -> Tick {
        Tick::from_seconds_f64(t)
    }

    /// A 10 s comp (30 fps), work area 1–9 s, layer A (#1, 0–4 s) with a position key at 2 s,
    /// layer B (#2, 5–8 s), a comp marker at 6.5 s. Returns the comp and the key's property uid.
    fn comp() -> (Comp, u64) {
        let mut ses = effectcraft_engine::Session::default();
        ses.execute("comp.new", json!({"name": "Snap", "width": 320, "height": 180, "duration": 10, "frameRate": 30})).unwrap();
        ses.execute("layer.newSolid", json!({"name": "A", "color": "#e04020", "width": 40, "height": 40})).unwrap();
        ses.execute("layer.newSolid", json!({"name": "B", "color": "#406080", "width": 40, "height": 40})).unwrap();
        let id = |ses: &effectcraft_engine::Session, n: &str| ses.active_comp().unwrap().layers.iter().find(|l| l.name == n).unwrap().id;
        let (a, b) = (id(&ses, "A"), id(&ses, "B"));
        ses.execute("layer.timing", json!({"layers": [a.0], "out": 4.0})).unwrap();
        ses.execute("layer.timing", json!({"layers": [b.0], "in": 5.0})).unwrap();
        ses.execute("layer.timing", json!({"layers": [b.0], "out": 8.0})).unwrap();
        ses.execute("comp.workArea", json!({"start": 1.0, "end": 9.0})).unwrap();
        ses.execute("time.set", json!({"time": 2.0})).unwrap();
        let uid = ses.active_comp().unwrap().layer(a).unwrap().transform().unwrap().get("position").unwrap().uid;
        ses.execute("prop.toggleAnimation", json!({"layer": a.0, "prop": uid})).unwrap();
        ses.execute("time.set", json!({"time": 6.5})).unwrap();
        ses.execute("comp.addMarker", json!({})).unwrap();
        let mut c = ses.active_comp().unwrap().clone();
        // Renumber so the tests read A = #1, B = #2.
        for l in &mut c.layers {
            l.id = if l.id == a {
                LayerId(1)
            } else if l.id == b {
                LayerId(2)
            } else {
                l.id
            };
        }
        (c, uid)
    }

    #[test]
    fn tolerance_is_eight_pixels_at_the_current_zoom() {
        assert!((tolerance(100.0) - 0.08).abs() < 1e-12);
        // Zoomed in 10x, the same 8 px are a tenth of the time.
        assert!((tolerance(1000.0) - 0.008).abs() < 1e-12);
        assert_eq!(tolerance(0.0), 0.0);
        assert_eq!(tolerance(f64::NAN), 0.0);
        assert_eq!(resolve(&[Target { time: 1.0, kind: Kind::Cti, layer: None }], &[1.0], 0.0), None);
    }

    #[test]
    fn the_closest_target_wins_and_ties_go_to_the_current_time() {
        let t = |time, kind| Target { time, kind, layer: None };
        let ts = [t(1.0, Kind::Cti), t(1.05, Kind::CompMarker), t(0.97, Kind::WorkAreaStart)];
        let hit = resolve(&ts, &[1.02], 0.08).expect("snaps");
        assert_eq!(hit.target.kind, Kind::Cti); // 0.02 away beats 0.03 and 0.05
        assert!((hit.delta + 0.02).abs() < 1e-12);
        // Exact tie: the earlier target (the CTI) wins.
        let tie = [t(2.0, Kind::Cti), t(2.0, Kind::CompMarker)];
        assert_eq!(resolve(&tie, &[2.01], 0.08).map(|h| h.target.kind), Some(Kind::Cti));
        // Out of range at this zoom: no snap; zoomed out it snaps.
        assert_eq!(resolve(&ts, &[1.2], tolerance(100.0)), None);
        assert!(resolve(&ts, &[1.2], tolerance(20.0)).is_some());
    }

    #[test]
    fn a_moved_bar_snaps_whichever_edge_is_closer() {
        let t = [Target { time: 5.0, kind: Kind::LayerIn, layer: Some(LayerId(2)) }];
        // A 0–4 s bar dragged to 0.97–4.97 s: its Out point is 0.03 s from B's In point.
        let hit = resolve(&t, &[0.97, 4.97], 0.08).expect("snaps");
        assert_eq!(hit.index, 1);
        assert!((hit.delta - 0.03).abs() < 1e-9);
    }

    #[test]
    fn layers_snap_to_their_neighbours_but_not_to_themselves() {
        let (c, _) = comp();
        // Moving layer A: its own edges, keys and markers are no targets.
        let skip = Skip { edges: vec![LayerId(1)], content: vec![LayerId(1)], keys: vec![] };
        let ts = targets(&c, &[], 3.0, &skip);
        assert!(!ts.iter().any(|t| t.layer == Some(LayerId(1))));
        // A dragged so its Out point nears B's In point (5 s) snaps there.
        let hit = resolve(&ts, &[1.04, 5.03], tolerance(100.0)).expect("snaps");
        assert_eq!((hit.target.kind, hit.target.layer), (Kind::LayerIn, Some(LayerId(2))));
        assert_eq!(hit.index, 1);
        // Trimming B's Out point toward the 6.5 s comp marker snaps to it, not to B's In point.
        let skip = Skip { edges: vec![LayerId(2)], ..Default::default() };
        let ts = targets(&c, &[], 0.0, &skip);
        assert_eq!(resolve(&ts, &[6.45], tolerance(100.0)).map(|h| h.target.kind), Some(Kind::CompMarker));
        // Every kind of target is there: CTI, work area, comp start and end.
        for k in [Kind::Cti, Kind::WorkAreaStart, Kind::WorkAreaEnd, Kind::CompStart, Kind::CompEnd, Kind::LayerOut] {
            assert!(ts.iter().any(|t| t.kind == k), "{k:?}");
        }
    }

    #[test]
    fn keys_snap_to_other_keys_and_layer_edges_but_not_to_the_moving_keys() {
        let (c, uid) = comp();
        // Visible keys are targets.
        let ts = targets(&c, &[(LayerId(1), uid)], 0.0, &Skip::default());
        assert!(ts.iter().any(|t| t.kind == Kind::Key && (t.time - 2.0).abs() < 1e-9));
        // Hidden properties' keys are not.
        assert!(!targets(&c, &[], 0.0, &Skip::default()).iter().any(|t| t.kind == Kind::Key));
        // Dragging that key: it is no target of its own, and it snaps to B's In point.
        let skip = Skip { keys: vec![(LayerId(1), uid, s(2.0))], ..Default::default() };
        let ts = targets(&c, &[(LayerId(1), uid)], 0.0, &skip);
        assert!(!ts.iter().any(|t| t.kind == Kind::Key));
        let hit = resolve(&ts, &[4.96], tolerance(100.0)).expect("snaps");
        // Layer A's Out point (4 s) is 0.96 s away; B's In point (5 s) wins.
        assert_eq!((hit.target.kind, hit.target.layer), (Kind::LayerIn, Some(LayerId(2))));
        // Another key snaps onto the 2 s key from 2.05 s.
        let ts = targets(&c, &[(LayerId(1), uid)], 0.0, &Skip::default());
        let hit = resolve(&ts, &[2.05], tolerance(100.0)).expect("snaps");
        assert_eq!(hit.target.kind, Kind::Key);
    }
}
