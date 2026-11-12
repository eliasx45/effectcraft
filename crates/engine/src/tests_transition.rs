//! `transition.apply` (Animation ▸ Transitions ▸ Zoom Cut / FX Cut / Scene Cut): Elias's 12 zoom
//! cuts (ZC1), 15 FX cuts (ZC2) and 15 scene cuts (ZC3) from the motion-kit shelf tables, and the
//! keyframe-move kinds whip-pan, linear-wipe, point-iris and split-handoff (ZC5).

use effectcraft_keyframe::Interp;
use effectcraft_project::{ItemId, LayerId, LayerSource, Project};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::Session;
use crate::render::RenderOpts;

const KINDS: [&str; 13] = [
    "kick",
    "blur-settle",
    "push-in",
    "into-window",
    "twos-card",
    "twos-push",
    "zoom-through",
    "whip",
    "whip-pan",
    "zoom-out-through",
    "punch",
    "flash",
    "white-flash",
];

const FX_KINDS: [&str; 15] = [
    "light-leak",
    "overexposure-burn",
    "film-burn",
    "glitch",
    "chromatic-aberration",
    "ripple",
    "vhs-tape",
    "shutter",
    "clock-wipe",
    "grid-dissolve",
    "gravity-drop",
    "morph-circle",
    "blur-through",
    "directional-blur",
    "page-burn",
];

const SCENE_KINDS: [&str; 18] = [
    "push-slide",
    "vertical-push",
    "elastic-push",
    "squeeze",
    "circle-iris",
    "diamond-iris",
    "diagonal-split",
    "card-flip-3d",
    "crossfade",
    "blur-crossfade",
    "focus-pull",
    "color-dip",
    "staggered-blocks",
    "horizontal-blinds",
    "vertical-blinds",
    "linear-wipe",
    "point-iris",
    "split-handoff",
];

/// The scene cuts that move the new shot right above the old one.
const STACK_KINDS: [&str; 4] = ["elastic-push", "crossfade", "blur-crossfade", "focus-pull"];

/// The kinds with a track key at the cut itself (t = 0 in the table).
const CUT_KEY: [&str; 9] = ["kick", "into-window", "zoom-through", "whip", "whip-pan", "zoom-out-through", "punch", "flash", "white-flash"];

/// A 384×216 comp of `secs` seconds at `fps` with two full-frame solids meeting at `at` s: the
/// old one (red, below) plays up to it, the new one (blue, above) from it.
/// Returns (session, comp, old, new, the cut in seconds as the layers have it).
fn cut_at(fps: f64, secs: f64, at: f64) -> (Session, u64, u64, u64, f64) {
    cut_at_size(fps, secs, at, 384, 216)
}

/// [`cut_at`] in a `w`×`h` comp.
fn cut_at_size(fps: f64, secs: f64, at: f64, w: u32, h: u32) -> (Session, u64, u64, u64, f64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cut", "width": w, "height": h, "frameRate": fps, "duration": secs})).unwrap();
    let cid = s.active_comp_id().unwrap().0;
    let old = s.execute("layer.newSolid", json!({"name": "A Footage", "color": "#d02020"})).unwrap()["layer"].as_u64().unwrap();
    let new = s.execute("layer.newSolid", json!({"name": "B Footage", "color": "#2040d0"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.timing", json!({"layers": [old], "out": at})).unwrap();
    s.execute("layer.timing", json!({"layers": [new], "in": at})).unwrap();
    let cut = s.active_comp().unwrap().layer(LayerId(new)).unwrap().in_point.seconds();
    (s, cid, old, new, cut)
}

fn apply(s: &mut Session, cid: u64, old: u64, new: u64, kind: &str) -> crate::Result<Value> {
    s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": kind}))
}

fn transform(s: &Session, cid: u64, l: u64) -> Option<effectcraft_project::PropGroup> {
    s.project.comp(ItemId(cid))?.layer(LayerId(l))?.transform().cloned()
}

fn zoom_cut_effects(s: &Session, cid: u64, l: u64) -> Vec<String> {
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let Some(l) = comp.layer(LayerId(l)) else { return vec![] };
    l.effects().map(|fx| fx.groups().filter(|g| g.name.starts_with("ZoomCut ")).map(|g| g.name.clone()).collect()).unwrap_or_default()
}

/// Comp time of every key in the project, with the comp's frame rate and what it belongs to.
fn all_keys(p: &Project) -> Vec<(FrameRate, Tick, String)> {
    let mut out = vec![];
    for item in p.items.values() {
        let Some(c) = item.as_comp() else { continue };
        for l in c.layers.iter() {
            l.props.walk("", &mut |path, pr| {
                for k in &pr.keys {
                    out.push((c.frame_rate, l.comp_time(k.time), format!("{} {}/{path}", item.name, l.name)));
                }
            });
        }
    }
    out
}

#[test]
fn every_kind_applies_at_29_97_and_24_fps() {
    for fps in [29.97, 24.0] {
        for kind in KINDS {
            let (mut s, cid, old, new, cut) = cut_at(fps, 10.0, 4.0);
            let (t_old, t_new) = (transform(&s, cid, old), transform(&s, cid, new));
            let undo_before = s.history.undo.len();
            let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            assert_eq!(r["kind"], json!(kind));
            // The cut is the new layer's in point, on a frame.
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let at = Tick::from_seconds_f64(r["at"].as_f64().unwrap());
            assert!((at.seconds() - cut).abs() < 1e-9, "{kind} at {fps}: cut {} vs {cut}", at.seconds());
            assert_eq!(fr.snap_nearest(at), at, "{kind} at {fps}: the cut is on a frame");
            // Every key lands on a frame of its comp.
            let keys = all_keys(&s.project);
            assert!(!keys.is_empty(), "{kind} at {fps}: no keys");
            for (kfr, t, what) in &keys {
                assert_eq!(kfr.snap_nearest(*t), *t, "{kind} at {fps}: {what} key at {} s is between frames", t.seconds());
            }
            // The cut key sits exactly on the cut.
            if CUT_KEY.contains(&kind) {
                assert!(keys.iter().any(|(_, t, _)| *t == fr.tick_of(r["cutFrame"].as_i64().unwrap())), "{kind} at {fps}: no key on the cut");
            }
            // The shots' own transforms are never touched (into-window moves the new shot into its
            // window comp, where it is placed).
            assert_eq!(transform(&s, cid, old), t_old, "{kind} at {fps}: old transform changed");
            if kind != "into-window" {
                assert_eq!(transform(&s, cid, new), t_new, "{kind} at {fps}: new transform changed");
            }
            // Everything added is named ZoomCut; one undo step takes it all back.
            assert!(!r["effects"].as_array().unwrap().is_empty() || kind == "into-window", "{kind}: no effects");
            for e in r["effects"].as_array().unwrap() {
                assert!(e["name"].as_str().unwrap().starts_with(&format!("ZoomCut {kind} ")), "{kind}: {e}");
            }
            assert_eq!(s.history.undo.len(), undo_before + 1, "{kind} at {fps}: one undo step");
            s.execute("edit.undo", json!({})).unwrap();
            let comp = s.project.comp(ItemId(cid)).unwrap();
            assert_eq!(comp.layers.len(), 2, "{kind}: undo removes the added layers");
            assert!(zoom_cut_effects(&s, cid, old).is_empty() && zoom_cut_effects(&s, cid, new).is_empty(), "{kind}: undo removes the effects");
        }
    }
}

#[test]
fn kick_lands_245_percent_on_the_cut_and_settles_on_expo_out_1() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(zoom_cut_effects(&s, cid, new), ["ZoomCut kick Zoom"]);
    assert!(zoom_cut_effects(&s, cid, old).is_empty(), "kick leaves the old shot alone");
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let l = comp.layer(LayerId(new)).unwrap();
    let g = l.effects().unwrap().groups().find(|g| g.name == "ZoomCut kick Zoom").unwrap();
    assert_eq!(g.match_id, "ec.distort.transform");
    let sh = g.prop("scaleHeight").unwrap();
    assert_eq!(sh.keys.len(), 3);
    let fr = comp.frame_rate;
    let cut_f = fr.frame_at(fr.snap_nearest(Tick::from_seconds_f64(cut)));
    let frames: Vec<i64> = sh.keys.iter().map(|k| fr.frame_at(l.comp_time(k.time))).collect();
    // 0, 1 frame and round(1.333333 × 29.97) = 40 frames after the cut.
    assert_eq!(frames, [cut_f, cut_f + 1, cut_f + 40]);
    assert_eq!(l.comp_time(sh.keys[0].time), fr.tick_of(cut_f), "the cut key is exactly on the cut");
    let v: Vec<f64> = sh.keys.iter().map(|k| k.value.components()[0]).collect();
    assert_eq!(v, [244.6, 189.8, 100.0]);
    // Expo Out 1 (Elias): out 0.1 %, in 100 %, speed 0, on both segments.
    for i in 0..2 {
        let (a, b) = (&sh.keys[i], &sh.keys[i + 1]);
        assert_eq!((a.out_interp, b.in_interp), (Interp::Bezier, Interp::Bezier), "segment {i}");
        assert!((a.out_ease.as_slice()[0].influence - 0.001).abs() < 1e-9, "segment {i} out {:?}", a.out_ease);
        assert!((b.in_ease.as_slice()[0].influence - 1.0).abs() < 1e-9, "segment {i} in {:?}", b.in_ease);
        assert!(a.out_ease.as_slice()[0].speed.abs() < 1e-9);
    }
    // The zoom is about the layer's centre, and no mirrored edge is needed (never below 100 %).
    let anchor = g.prop("anchor").unwrap().value.components();
    assert!((anchor[0] - f64::from(comp.width) / 2.0).abs() < 1e-9 && (anchor[1] - f64::from(comp.height) / 2.0).abs() < 1e-9, "{anchor:?}");
    assert_eq!(r["land"].as_f64().unwrap(), fr.tick_of(cut_f + (1.266667f64 * fr.as_f64()).round() as i64).seconds());
}

#[test]
fn kick_cut_frame_shows_no_frame_edge() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let r = apply(&mut s, cid, old, new, "kick").unwrap();
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        for f in [r["cutFrame"].as_i64().unwrap(), r["cutFrame"].as_i64().unwrap() + 1, r["landFrame"].as_i64().unwrap()] {
            let img = s.render(ItemId(cid), fr.tick_of(f), RenderOpts { scale: 1.0, motion_blur: false, ..Default::default() });
            let (w, h) = (i64::from(img.width), i64::from(img.height));
            let edge = (0..w).flat_map(|x| [(x, 0), (x, h - 1)]).chain((0..h).flat_map(|y| [(0, y), (w - 1, y)]));
            for (x, y) in edge {
                let px = img.get(x, y);
                assert!(px[3] > 0.999, "{fps} fps frame {f}: transparent edge pixel at ({x}, {y}): {px:?}");
                // The new (blue) shot, never the old one, from the cut on.
                assert!(px[2] > px[0], "{fps} fps frame {f}: ({x}, {y}) isn't the new shot: {px:?}");
            }
        }
    }
}

#[test]
fn reapplying_on_the_same_pair_replaces_the_previous_zoom_cut() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "kick").unwrap();
    let r = apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(zoom_cut_effects(&s, cid, new), ["ZoomCut kick Zoom"], "no stacking");
    assert_eq!(r["removed"]["effects"], json!(1));
    // A kind with overlays: the second application replaces them.
    let r = apply(&mut s, cid, old, new, "white-flash").unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 6, "{r}");
    let n = s.project.comp(ItemId(cid)).unwrap().layers.len();
    assert_eq!(n, 2 + 6);
    apply(&mut s, cid, old, new, "white-flash").unwrap();
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), n, "the lights don't stack");
    // Another kind on the pair: the white flash's effects and lights go.
    let r = apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(r["removed"]["layers"], json!(6));
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
    assert!(zoom_cut_effects(&s, cid, old).is_empty());
    assert_eq!(zoom_cut_effects(&s, cid, new), ["ZoomCut kick Zoom"]);
    // A layer's own effects stay.
    s.execute("effect.apply", json!({"layers": [new], "effect": "ec.blur.gaussian"})).unwrap();
    apply(&mut s, cid, old, new, "blur-settle").unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let names: Vec<String> = comp.layer(LayerId(new)).unwrap().effects().unwrap().groups().map(|g| g.name.clone()).collect();
    assert_eq!(names, ["Gaussian Blur", "ZoomCut blur-settle Blur", "ZoomCut blur-settle Zoom"]);
}

#[test]
fn bad_requests_are_errors_and_change_nothing() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let e = apply(&mut s, cid, old, new, "zoomy").unwrap_err().to_string();
    assert!(e.contains("unknown kind") && e.contains("white-flash"), "{e}");
    // The layers don't meet at this cut.
    let e = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "at": 2.0})).unwrap_err().to_string();
    assert!(e.contains("don't meet"), "{e}");
    // The same layer twice, one of the pair, a layer that doesn't exist.
    assert!(apply(&mut s, cid, old, old, "kick").is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old})).is_err());
    assert!(apply(&mut s, cid, old, 999_999, "kick").is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "restScale": -5})).is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "centre": "middle"})).is_err());
    assert!(s.execute_checked("transition.apply", json!({"comp": cid, "old": old, "new": new, "bogus": 1})).is_err(), "unknown params");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    assert_eq!(s.history.undo.len(), undo);
    // The comp ends before the move settles (twos-card runs 3.04 s after the cut).
    let (mut s, cid, old, new, _) = cut_at(29.97, 5.0, 4.0);
    let e = apply(&mut s, cid, old, new, "twos-card").unwrap_err().to_string();
    assert!(e.contains("comp too short"), "{e}");
    // The old shot's move starts before the comp does (zoom-out-through: 1.2 s before the cut).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 0.5);
    let e = apply(&mut s, cid, old, new, "zoom-out-through").unwrap_err().to_string();
    assert!(e.contains("too close to the comp's start"), "{e}");
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
}

#[test]
fn the_selection_or_the_playhead_picks_the_pair() {
    let (mut s, _cid, old, new, cut) = cut_at(24.0, 10.0, 4.0);
    // Two selected layers, either order: the later in point is the new shot.
    for sel in [[old, new], [new, old]] {
        s.execute("layer.select", json!({"layers": sel})).unwrap();
        assert!(s.is_enabled("transition.apply"));
        let r = s.execute("transition.apply", json!({})).unwrap();
        assert_eq!((r["old"].as_u64(), r["new"].as_u64(), r["kind"].as_str()), (Some(old), Some(new), Some("kick")));
        s.execute("edit.undo", json!({})).unwrap();
    }
    // One selected layer: no pair.
    s.execute("layer.select", json!({"layers": [old]})).unwrap();
    assert!(!s.is_enabled("transition.apply"));
    // Nothing selected: the layer starting at the playhead and the one ending there.
    s.state.selected_layers.clear();
    s.execute("time.set", json!({"time": 2.0})).unwrap();
    assert!(!s.is_enabled("transition.apply"), "no cut at 2 s");
    s.execute("time.set", json!({"time": cut})).unwrap();
    assert!(s.is_enabled("transition.apply"));
    let r = s.execute("transition.apply", json!({"kind": "punch"})).unwrap();
    assert_eq!((r["old"].as_u64(), r["new"].as_u64()), (Some(old), Some(new)));
    // The menu entry runs it: Animation ▸ Transitions ▸ Zoom Cut.
    let spec = crate::commands::find("transition.apply").unwrap();
    assert_eq!(spec.menu, ["Animation", "Transitions", "Zoom Cut"]);
    let kinds = s.execute("transition.kinds", json!({})).unwrap();
    let ids = |family: &str| -> Vec<String> {
        kinds.as_array().unwrap().iter().filter(|k| k["family"] == json!(family)).map(|k| k["kind"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(ids("zoom-cut"), KINDS);
    assert_eq!(ids("fx-cut"), FX_KINDS);
    assert_eq!(ids("scene-cut"), SCENE_KINDS);
    assert_eq!(kinds.as_array().unwrap().len(), 46);
}

#[test]
fn twos_kinds_step_every_two_comp_frames() {
    let (mut s, cid, old, new, _) = cut_at(24.0, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "twos-card").unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let l = comp.layer(LayerId(new)).unwrap();
    let zoom = l.effects().unwrap().groups().find(|g| g.name == "ZoomCut twos-card Zoom").unwrap();
    let expr = zoom.prop("scaleHeight").unwrap().expr.clone().unwrap();
    assert!(expr.enabled && expr.text.contains("Math.floor"), "{}", expr.text);
    assert!(expr.text.contains(&format!("a = {}", r["cutFrame"].as_i64().unwrap() - 1)), "{}", expr.text);
    // The smear steps too; the Expo Out keys don't.
    let smear = l.effects().unwrap().groups().find(|g| g.name == "ZoomCut twos-card Smear").unwrap();
    assert!(smear.prop("amount").unwrap().expr.is_some());
}

#[test]
fn an_old_shot_key_in_the_last_reference_frame_stays_on_the_last_frame_before_the_cut() {
    // zoom-through's old zoom peaks 1 reference frame (23.976) before the cut: at 59.94 that is
    // still the last comp frame before the cut, not 2 frames earlier.
    let (mut s, cid, old, new, _) = cut_at(59.94, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "zoom-through").unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let l = comp.layer(LayerId(old)).unwrap();
    let g = l.effects().unwrap().groups().find(|g| g.name == "ZoomCut zoom-through Zoom").unwrap();
    let last = g.prop("scaleHeight").unwrap().keys.last().unwrap();
    assert_eq!(comp.frame_rate.frame_at(l.comp_time(last.time)), r["cutFrame"].as_i64().unwrap() - 1);
    // zoom-through rests the new shot at 122.55 %, its mirrored edges cover the 86.6 % frame.
    assert_eq!(r["restScale"].as_f64(), Some(122.55));
    let nl = comp.layer(LayerId(new)).unwrap();
    assert_eq!(nl.effects().unwrap().groups().next().map(|g| g.match_id.as_str()), Some("ec.stylize.motiontile"));
    // The comp's motion blur samples are raised for the Transform shutter.
    assert!(comp.motion_blur_samples >= 64);
}

#[test]
fn a_lower_rest_scale_brings_mirrored_edges() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    // punch rests at 113.49 % with no Motion Tile; at 100 % its 100 → 113.49 move starts at 88 %.
    let r = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "punch", "restScale": 100})).unwrap();
    assert_eq!(r["restScale"].as_f64(), Some(100.0));
    let names = zoom_cut_effects(&s, cid, new);
    assert_eq!(names.first().map(String::as_str), Some("ZoomCut punch Mirror edges"), "{names:?}");
    // A centre for both shots moves the Transform anchor.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "kick", "centre": [480, 270]})).unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let g = comp.layer(LayerId(new)).unwrap().effects().unwrap().groups().find(|g| g.name == "ZoomCut kick Zoom").unwrap().clone();
    // 480 × 384 / 1920 = 96, 270 × 216 / 1080 = 54.
    let anchor = g.prop("anchor").unwrap().value.components();
    assert!((anchor[0] - 96.0).abs() < 1e-9 && (anchor[1] - 54.0).abs() < 1e-9, "{anchor:?}");
}

// ---------------------------------------------------------------- FX cuts (ZC2)

/// Every expression in the project: (comp, layer, uid path, text).
fn all_exprs(p: &Project) -> Vec<(ItemId, LayerId, String, String)> {
    let mut out = vec![];
    for item in p.items.values() {
        let Some(c) = item.as_comp() else { continue };
        for l in c.layers.iter() {
            l.props.walk("", &mut |path, pr| {
                if let Some(e) = &pr.expr {
                    out.push((item.id, l.id, path.to_string(), e.text.clone()));
                }
            });
        }
    }
    out
}

/// Names every family's additions start with.
fn ours(name: &str) -> bool {
    name.starts_with("FxCut ") || name.starts_with("ZoomCut ") || name.starts_with("SceneCut ")
}

/// Names of the items the families add (`ZoomCut …` / `FxCut …` / `SceneCut …` comps).
fn transition_items(s: &Session) -> Vec<String> {
    s.project.items.values().filter(|i| ours(&i.name)).map(|i| i.name.clone()).collect()
}

/// Solid items nothing uses.
fn orphan_solids(s: &Session) -> Vec<String> {
    s.project
        .items
        .values()
        .filter(|i| matches!(i.kind, effectcraft_project::ItemKind::Solid(_)) && s.project.item_users(i.id).is_empty())
        .map(|i| i.name.clone())
        .collect()
}

/// Effects of every family on a layer.
fn transition_effects(s: &Session, cid: u64, l: u64) -> Vec<String> {
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let Some(l) = comp.layer(LayerId(l)) else { return vec![] };
    l.effects().map(|fx| fx.groups().filter(|g| ours(&g.name)).map(|g| g.name.clone()).collect()).unwrap_or_default()
}

fn timing(s: &Session, cid: u64, l: u64) -> (Tick, Tick) {
    let l = s.project.comp(ItemId(cid)).unwrap().layer(LayerId(l)).unwrap();
    (l.in_point, l.out_point)
}

#[test]
fn every_fx_kind_applies_at_29_97_and_24_fps() {
    for fps in [29.97, 24.0] {
        for kind in FX_KINDS {
            let (mut s, cid, old, new, cut) = cut_at(fps, 10.0, 4.0);
            let (t_old, t_new) = (transform(&s, cid, old), transform(&s, cid, new));
            let (time_old, time_new) = (timing(&s, cid, old), timing(&s, cid, new));
            let items_before = s.project.items.len();
            let undo_before = s.history.undo.len();
            let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            assert_eq!((r["kind"].as_str(), r["family"].as_str()), (Some(kind), Some("fx-cut")), "{r}");
            // The cut is the new layer's in point, on a frame.
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let at = Tick::from_seconds_f64(r["at"].as_f64().unwrap());
            assert!((at.seconds() - cut).abs() < 1e-9, "{kind} at {fps}: cut {} vs {cut}", at.seconds());
            assert_eq!(fr.snap_nearest(at), at, "{kind} at {fps}: the cut is on a frame");
            // Every key, in every comp (copies of the cut, VHS strips), lands on a frame of its comp.
            let keys = all_keys(&s.project);
            assert!(!keys.is_empty() || kind == "glitch", "{kind} at {fps}: no keys");
            for (kfr, t, what) in &keys {
                assert_eq!(kfr.snap_nearest(*t), *t, "{kind} at {fps}: {what} key at {} s is between frames", t.seconds());
            }
            // The shots' own transforms are never touched.
            assert_eq!(transform(&s, cid, old), t_old, "{kind} at {fps}: old transform changed");
            assert_eq!(transform(&s, cid, new), t_new, "{kind} at {fps}: new transform changed");
            // Everything added is named FxCut <kind> and tagged for the pair.
            let pre = format!("FxCut {kind} ");
            for e in r["effects"].as_array().unwrap() {
                assert!(e["name"].as_str().unwrap().starts_with(&pre), "{kind}: {e}");
            }
            let comp = s.project.comp(ItemId(cid)).unwrap();
            for l in r["layers"].as_array().unwrap() {
                let l = comp.layer(LayerId(l.as_u64().unwrap())).unwrap();
                assert!(l.name.starts_with(&pre), "{kind}: layer “{}”", l.name);
                assert!(l.comment.starts_with(&format!("FxCut {old}->{new}")), "{kind}: “{}” comment “{}”", l.name, l.comment);
            }
            assert_eq!(comp.layers.len(), 2 + r["layers"].as_array().unwrap().len(), "{kind}: the reply lists every added layer");
            for c in r["comps"].as_array().unwrap() {
                let it = s.project.item(ItemId(c.as_u64().unwrap())).unwrap();
                assert!(it.name.starts_with(&pre) && it.comment == format!("FxCut {old}->{new}"), "{kind}: item “{}” “{}”", it.name, it.comment);
            }
            assert!(r["effects"].as_array().unwrap().len() + r["layers"].as_array().unwrap().len() > 0, "{kind}: nothing added");
            // Every expression placeholder is filled in.
            for (_, _, path, text) in all_exprs(&s.project) {
                assert!(!text.contains("${"), "{kind} at {fps}: {path}: {text}");
            }
            // One undo step takes it all back, the shots' trims too.
            assert_eq!(s.history.undo.len(), undo_before + 1, "{kind} at {fps}: one undo step");
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2, "{kind}: undo removes the added layers");
            assert!(transition_effects(&s, cid, old).is_empty() && transition_effects(&s, cid, new).is_empty(), "{kind}: undo removes the effects");
            assert_eq!(s.project.items.len(), items_before, "{kind}: undo removes the added items");
            assert_eq!((timing(&s, cid, old), timing(&s, cid, new)), (time_old, time_new), "{kind}: undo restores the trims");
        }
    }
}

#[test]
fn fx_cut_keys_follow_the_comp_frames_and_dur() {
    for fps in [29.97, 24.0] {
        // light-leak's warm solid: 0 → 60 % from 0.433767 s before the cut to the cut.
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let r = apply(&mut s, cid, old, new, "light-leak").unwrap();
        let comp = s.project.comp(ItemId(cid)).unwrap();
        let fr = comp.frame_rate;
        let cut_f = r["cutFrame"].as_i64().unwrap();
        let warm = comp.layers.iter().find(|l| l.name == "FxCut light-leak Leak warm").unwrap();
        let op = warm.props.prop("transform/opacity").unwrap();
        let frames: Vec<i64> = op.keys.iter().map(|k| fr.frame_at(warm.comp_time(k.time))).collect();
        let f = |t: f64| cut_f + (t * fr.as_f64()).round() as i64;
        assert_eq!(frames, [f(-0.433767), cut_f, f(0.066733), f(0.467133)], "{fps}");
        let v: Vec<f64> = op.keys.iter().map(|k| k.value.components()[0]).collect();
        assert_eq!(v, [0.0, 60.0, 60.0, 0.0]);
        assert_eq!(fr.frame_at(warm.in_point), f(-0.433767));
        assert_eq!(fr.frame_at(warm.out_point), f(0.467133));
        assert_eq!(r["landFrame"].as_i64(), Some(f(0.467133)));
        // Screen over the shots, from the window's start.
        assert_eq!(warm.blend_mode, effectcraft_color::BlendMode::Screen);
        assert_eq!(r["startFrame"].as_i64(), Some(f(-0.433767)));
        // `dur` scales every time by dur / window (0.9009 s): twice as long.
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let r = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "light-leak", "dur": 1.8018})).unwrap();
        let comp = s.project.comp(ItemId(cid)).unwrap();
        let warm = comp.layers.iter().find(|l| l.name == "FxCut light-leak Leak warm").unwrap();
        let op = warm.props.prop("transform/opacity").unwrap();
        let first = fr.frame_at(warm.comp_time(op.keys[0].time));
        assert_eq!(first, r["cutFrame"].as_i64().unwrap() + (-0.433767f64 * 2.0 * fr.as_f64()).round() as i64, "{fps}: dur doubles the lead");
        assert!((r["dur"].as_f64().unwrap() - 1.8018).abs() < 1e-9);
    }
}

#[test]
fn light_leak_cut_frame_has_no_transparent_edge_pixel() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let r = apply(&mut s, cid, old, new, "light-leak").unwrap();
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let f = r["cutFrame"].as_i64().unwrap();
        let img = s.render(ItemId(cid), fr.tick_of(f), RenderOpts { scale: 1.0, motion_blur: false, ..Default::default() });
        let (w, h) = (i64::from(img.width), i64::from(img.height));
        let edge = (0..w).flat_map(|x| [(x, 0), (x, h - 1)]).chain((0..h).flat_map(|y| [(0, y), (w - 1, y)]));
        let mut warm = 0;
        for (x, y) in edge {
            let px = img.get(x, y);
            assert!(px[3] > 0.999, "{fps} fps cut frame {f}: transparent edge pixel at ({x}, {y}): {px:?}");
            if px[0] > 0.3 {
                warm += 1;
            }
        }
        // The leak lights the blue shot: the edge isn't plain blue.
        assert!(warm > 0, "{fps} fps: no light on the cut frame");
    }
}

#[test]
fn clock_wipe_starts_the_new_shot_under_the_old_one() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    // cut_at stacks the new shot above the old one: the old one goes on top while both show.
    let r = apply(&mut s, cid, old, new, "clock-wipe").unwrap();
    assert_eq!(r["restacked"], json!(true));
    let comp = s.project.comp(ItemId(cid)).unwrap();
    assert!(comp.index_of(LayerId(old)) < comp.index_of(LayerId(new)), "the old shot is above the new one");
    let fr = comp.frame_rate;
    let cut_f = r["cutFrame"].as_i64().unwrap();
    let early = cut_f + (-0.467133f64 * fr.as_f64()).round() as i64;
    let (nin, _) = timing(&s, cid, new);
    let (_, oout) = timing(&s, cid, old);
    assert_eq!(fr.frame_at(nin), early, "the new shot starts 0.467 s early");
    assert!((oout.seconds() - cut).abs() < 1e-9, "the old shot still ends on the cut");
    assert_eq!(transition_effects(&s, cid, old), ["FxCut clock-wipe Clock"]);
    let l = comp.layer(LayerId(old)).unwrap();
    let g = l.effects().unwrap().groups().find(|g| g.name == "FxCut clock-wipe Clock").unwrap();
    assert_eq!(g.match_id, "ec.transition.radialwipe");
    let done = g.prop("completion").unwrap();
    let frames: Vec<i64> = done.keys.iter().map(|k| fr.frame_at(l.comp_time(k.time))).collect();
    assert_eq!(frames, [early, cut_f]);
    // Re-applying (no `at`): the trims go back to the cut first, so it lands on the same cut.
    let r2 = apply(&mut s, cid, old, new, "clock-wipe").unwrap();
    assert_eq!(r2["cutFrame"], r["cutFrame"]);
    assert_eq!(r2["removed"]["effects"], json!(1));
    // A zoom cut after it: the new shot starts on the cut again.
    let r3 = apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(r3["cutFrame"], r["cutFrame"]);
    assert!((timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9);
}

#[test]
fn page_burn_ends_the_old_shot_early_and_burns_a_copy() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "page-burn").unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let fr = comp.frame_rate;
    let cut_f = r["cutFrame"].as_i64().unwrap();
    let early = cut_f + (-1.2012f64 * fr.as_f64()).round() as i64;
    assert_eq!(fr.frame_at(timing(&s, cid, old).1), early);
    assert_eq!(fr.frame_at(timing(&s, cid, new).0), early);
    // The burning page: a copy of the old shot (an oversize precomp), its matte inverted.
    let page = comp.layers.iter().find(|l| l.name == "FxCut page-burn Page").unwrap();
    let matte = page.track_matte.expect("the page has a track matte");
    let ml = comp.layer(matte.layer).unwrap();
    assert_eq!(ml.name, "FxCut page-burn Page matte");
    assert_eq!(format!("{:?}", matte.kind), "AlphaInverted");
    let LayerSource::Comp { item } = page.source else { panic!("the page is a precomp") };
    let scenes = s.project.comp(item).unwrap();
    assert!(scenes.width > comp.width && scenes.height > comp.height, "padded");
    assert_eq!(scenes.layers.len(), 1, "a copy of the old shot only");
    // The matte's circle grows: a keyed mask path.
    let mask_keys = ml.masks().unwrap().groups().next().unwrap().prop("path").unwrap().keys.len();
    assert_eq!(mask_keys, 2);
    // Re-applying: the shots go back to the cut first, then the burn trims them again.
    let r2 = apply(&mut s, cid, old, new, "page-burn").unwrap();
    assert_eq!(r2["cutFrame"], r["cutFrame"]);
    assert_eq!(fr.frame_at(timing(&s, cid, old).1), early);
    // A plain FX cut after it: both shots meet on the cut again.
    apply(&mut s, cid, old, new, "light-leak").unwrap();
    assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9 && (timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9);
}

#[test]
fn switching_kinds_on_the_same_pair_leaves_nothing_behind() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let items0 = s.project.items.len();
    // Every FX cut, every zoom cut but into-window (it moves the new shot into a window), and the
    // FX cuts again backwards: each replaces the one before.
    let seq: Vec<&str> = FX_KINDS.iter().chain(KINDS.iter().filter(|k| **k != "into-window")).chain(FX_KINDS.iter().rev()).copied().collect();
    for kind in seq {
        let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert!((r["at"].as_f64().unwrap() - cut).abs() < 1e-9, "{kind}: the cut moved to {}", r["at"]);
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert_eq!(comp.layers.len(), 2 + r["layers"].as_array().unwrap().len(), "{kind}: leftover layers");
        let pre = if r["family"] == json!("fx-cut") { format!("FxCut {kind} ") } else { format!("ZoomCut {kind} ") };
        for l in [old, new] {
            for e in transition_effects(&s, cid, l) {
                assert!(e.starts_with(&pre), "{kind}: leftover effect “{e}”");
            }
        }
        for l in comp.layers.iter().filter(|l| l.id != LayerId(old) && l.id != LayerId(new)) {
            assert!(l.name.starts_with(&pre), "{kind}: leftover layer “{}”", l.name);
        }
        for i in transition_items(&s) {
            assert!(i.starts_with(&pre), "{kind}: leftover item “{i}”");
        }
        assert!(orphan_solids(&s).is_empty(), "{kind}: orphan solids {:?}", orphan_solids(&s));
    }
    apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
    assert!(transition_items(&s).is_empty());
    assert_eq!(s.project.items.len(), items0, "no items left behind (solids, precomps)");
    assert_eq!(transition_effects(&s, cid, new), ["ZoomCut kick Zoom"]);
}

#[test]
fn zoom_cut_overlays_leave_no_orphan_solids() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let items0 = s.project.items.len();
    apply(&mut s, cid, old, new, "white-flash").unwrap();
    assert!(s.project.items.len() > items0, "the lights are solids");
    apply(&mut s, cid, old, new, "white-flash").unwrap();
    assert!(orphan_solids(&s).is_empty(), "{:?}", orphan_solids(&s));
    apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(s.project.items.len(), items0);
}

#[test]
fn fx_cut_expressions_evaluate() {
    // The FX kinds whose table has an `expr` (chromatic-aberration has none: its channels are keyed).
    for kind in ["glitch", "ripple", "vhs-tape", "page-burn"] {
        let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
        s.expr_check = Some(effectcraft_expr::check_syntax);
        let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind}: {e}"));
        let t = r["at"].as_f64().unwrap() - 0.05;
        let exprs = all_exprs(&s.project);
        assert!(!exprs.is_empty(), "{kind}: no expressions");
        for (c, l, path, text) in exprs {
            effectcraft_expr::eval_property(&s.project, c, l, &path, t).unwrap_or_else(|e| panic!("{kind}: {path} `{text}`: {e}"));
        }
    }
    // ${amount} reads the layer's own Amount slider; ${i} is each strip's index.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "vhs-tape").unwrap();
    let texts: Vec<String> = all_exprs(&s.project).into_iter().map(|e| e.3).collect();
    assert!(texts.iter().any(|t| t.contains("effect(\"FxCut vhs-tape Amount\")(\"Slider\")")));
    assert!(texts.iter().any(|t| t.contains("seedRandom(20, true)")), "the 20th strip");
    let strips = s.project.items.values().find(|i| i.name == "FxCut vhs-tape Strips").and_then(|i| i.as_comp()).unwrap();
    assert_eq!(strips.layers.len(), 20);
}

#[test]
fn bad_fx_requests_are_errors_and_change_nothing() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let e = apply(&mut s, cid, old, new, "sparkle").unwrap_err().to_string();
    assert!(e.contains("unknown kind") && e.contains("page-burn") && e.contains("kick"), "{e}");
    // Each family's own options.
    let e = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "glitch", "restScale": 100})).unwrap_err().to_string();
    assert!(e.contains("zoom cuts"), "{e}");
    let e = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "kick", "dur": 1})).unwrap_err().to_string();
    assert!(e.contains("FX cuts"), "{e}");
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "shutter", "dur": -1})).is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "shutter", "colour": "red"})).is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "grid-dissolve", "colour": []})).is_err());
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    assert_eq!(s.history.undo.len(), undo);
    // Both options are accepted parameters.
    let r = s.execute_checked("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "shutter", "colour": "#ff0000", "dur": 0.5})).unwrap();
    assert!((r["dur"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2, "undo goes back");
    // page-burn runs 1.2 s before the cut: too close to the comp's start.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 1.0);
    let before = s.project.clone();
    let e = apply(&mut s, cid, old, new, "page-burn").unwrap_err().to_string();
    assert!(e.contains("too close to the comp's start"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // The comp ends before it settles.
    let (mut s, cid, old, new, _) = cut_at(29.97, 4.5, 4.0);
    let e = apply(&mut s, cid, old, new, "page-burn").unwrap_err().to_string();
    assert!(e.contains("comp too short"), "{e}");
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
}

#[test]
fn fx_colour_tints_the_covers() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "grid-dissolve", "colour": ["#ff0000", "#00ff00"]})).unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let cells = comp.layers.iter().filter(|l| l.name.starts_with("FxCut grid-dissolve Cell ")).count();
    assert_eq!(cells, 12);
    // The menu: Animation ▸ Transitions ▸ FX Cut ▸ the 15 kinds.
    let bar = crate::menus::menu_bar();
    let json = serde_json::to_string(bar).unwrap();
    assert!(json.contains("\"label\":\"FX Cut\""), "no FX Cut submenu");
    for kind in FX_KINDS {
        assert!(json.contains(&format!("{{\"kind\":\"{kind}\"}}")), "{kind} isn't in the menus");
    }
}

// ---------------------------------------------------------------- scene cuts (ZC3)

fn apply_dir(s: &mut Session, cid: u64, old: u64, new: u64, kind: &str, dir: &str) -> crate::Result<Value> {
    s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": kind, "dir": dir}))
}

fn effect<'a>(s: &'a Session, cid: u64, l: u64, name: &str) -> &'a effectcraft_project::PropGroup {
    let l = s.project.comp(ItemId(cid)).unwrap().layer(LayerId(l)).unwrap();
    l.effects().unwrap().groups().find(|g| g.name == name).unwrap_or_else(|| panic!("no effect “{name}” on “{}”", l.name))
}

fn layer_named<'a>(s: &'a Session, cid: u64, name: &str) -> &'a effectcraft_project::Layer {
    s.project.comp(ItemId(cid)).unwrap().layers.iter().find(|l| l.name == name).unwrap_or_else(|| panic!("no layer “{name}”"))
}

/// The `moved` entries of a reply for one layer and what.
fn moved<'a>(r: &'a Value, layer: u64, what: &str) -> Vec<&'a Value> {
    r["moved"].as_array().unwrap().iter().filter(|m| m["layer"] == json!(layer) && m["what"] == json!(what)).collect()
}

#[test]
fn every_scene_kind_applies_at_29_97_and_24_fps() {
    for fps in [29.97, 24.0] {
        for kind in SCENE_KINDS {
            let (mut s, cid, old, new, cut) = cut_at(fps, 10.0, 4.0);
            let (t_old, t_new) = (transform(&s, cid, old), transform(&s, cid, new));
            let (time_old, time_new) = (timing(&s, cid, old), timing(&s, cid, new));
            let items_before = s.project.items.len();
            let undo_before = s.history.undo.len();
            let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            assert_eq!((r["kind"].as_str(), r["family"].as_str()), (Some(kind), Some("scene-cut")), "{r}");
            // The cut is the new layer's in point, on a frame.
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let at = Tick::from_seconds_f64(r["at"].as_f64().unwrap());
            assert!((at.seconds() - cut).abs() < 1e-9, "{kind} at {fps}: cut {} vs {cut}", at.seconds());
            assert_eq!(fr.snap_nearest(at), at, "{kind} at {fps}: the cut is on a frame");
            // Every key, in every comp (copies of the shots), lands on a frame of its comp.
            let keys = all_keys(&s.project);
            assert!(!keys.is_empty(), "{kind} at {fps}: no keys");
            for (kfr, t, what) in &keys {
                assert_eq!(kfr.snap_nearest(*t), *t, "{kind} at {fps}: {what} key at {} s is between frames", t.seconds());
            }
            // The shots' own transforms are never touched.
            assert_eq!(transform(&s, cid, old), t_old, "{kind} at {fps}: old transform changed");
            assert_eq!(transform(&s, cid, new), t_new, "{kind} at {fps}: new transform changed");
            // Everything added is named SceneCut <kind> and tagged for the pair.
            let pre = format!("SceneCut {kind} ");
            for e in r["effects"].as_array().unwrap() {
                assert!(e["name"].as_str().unwrap().starts_with(&pre), "{kind}: {e}");
            }
            let comp = s.project.comp(ItemId(cid)).unwrap();
            for l in r["layers"].as_array().unwrap() {
                let l = comp.layer(LayerId(l.as_u64().unwrap())).unwrap();
                assert!(l.name.starts_with(&pre), "{kind}: layer “{}”", l.name);
                assert!(l.comment.starts_with(&format!("SceneCut {old}->{new}")), "{kind}: “{}” comment “{}”", l.name, l.comment);
            }
            assert_eq!(comp.layers.len(), 2 + r["layers"].as_array().unwrap().len(), "{kind}: the reply lists every added layer");
            for c in r["comps"].as_array().unwrap() {
                let it = s.project.item(ItemId(c.as_u64().unwrap())).unwrap();
                assert!(it.name.starts_with(&pre) && it.comment == format!("SceneCut {old}->{new}"), "{kind}: item “{}” “{}”", it.name, it.comment);
            }
            assert!(r["effects"].as_array().unwrap().len() + r["layers"].as_array().unwrap().len() > 0, "{kind}: nothing added");
            // cut_at puts the new shot above the old one already: no layer moves in the order (but
            // linear-wipe and point-iris put the old one over it), and every in / out point the kind
            // changed is in `moved`.
            let restack = OLD_OVER_NEW.contains(&kind);
            assert_eq!(r["stacked"], json!(restack), "{kind}");
            assert_eq!(moved(&r, old, "order").len(), usize::from(restack), "{kind}: {}", r["moved"]);
            assert!(moved(&r, new, "order").is_empty(), "{kind}: {}", r["moved"]);
            let (now_old, now_new) = (timing(&s, cid, old), timing(&s, cid, new));
            assert_eq!(moved(&r, new, "in").len(), usize::from(now_new.0 != time_new.0), "{kind}: {}", r["moved"]);
            assert_eq!(moved(&r, old, "out").len(), usize::from(now_old.1 != time_old.1), "{kind}: {}", r["moved"]);
            // One undo step takes it all back, the shots' trims too.
            assert_eq!(s.history.undo.len(), undo_before + 1, "{kind} at {fps}: one undo step");
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2, "{kind}: undo removes the added layers");
            assert!(transition_effects(&s, cid, old).is_empty() && transition_effects(&s, cid, new).is_empty(), "{kind}: undo removes the effects");
            assert_eq!(s.project.items.len(), items_before, "{kind}: undo removes the added items");
            assert_eq!((timing(&s, cid, old), timing(&s, cid, new)), (time_old, time_new), "{kind}: undo restores the trims");
            if restack {
                let comp = s.project.comp(ItemId(cid)).unwrap();
                assert!(comp.index_of(LayerId(new)) < comp.index_of(LayerId(old)), "{kind}: undo restores the order");
            }
        }
    }
}

#[test]
fn push_slide_keys_follow_the_comp_frames_and_every_dir() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let r = apply(&mut s, cid, old, new, "push-slide").unwrap();
        assert_eq!(r["dir"], json!("left"), "the default dir");
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let cut_f = r["cutFrame"].as_i64().unwrap();
        let start = cut_f + (-0.5005 * fr.as_f64()).round() as i64;
        assert_eq!(r["startFrame"].as_i64(), Some(start));
        // The new shot starts at the window's start (it pushes in), the old one ends on the cut.
        assert_eq!(fr.frame_at(timing(&s, cid, new).0), start, "{fps}");
        assert_eq!(fr.frame_at(timing(&s, cid, old).1), cut_f, "{fps}");
        assert_eq!(moved(&r, new, "in").len(), 1, "{}", r["moved"]);
        let l = s.project.comp(ItemId(cid)).unwrap().layer(LayerId(old)).unwrap().clone();
        let g = effect(&s, cid, old, "SceneCut push-slide Push");
        let pos = g.prop("position").unwrap();
        let frames: Vec<i64> = pos.keys.iter().map(|k| fr.frame_at(l.comp_time(k.time))).collect();
        assert_eq!(frames, [start, cut_f], "{fps}");
        // [0, 0] → [-1920, 0] offsets from the centre of a 384×216 layer: 192 → 192 − 384.
        let xs: Vec<f64> = pos.keys.iter().map(|k| k.value.components()[0]).collect();
        assert!((xs[0] - 192.0).abs() < 1e-9 && (xs[1] + 192.0).abs() < 1e-9, "{xs:?}");
        // Expo In-Out (Elias) on the push.
        assert_eq!((pos.keys[0].out_interp, pos.keys[1].in_interp), (Interp::Bezier, Interp::Bezier));
    }
    // Every dir: right mirrors x, up / down swap the axes (the seam's Motion Tile grows in height).
    let want = [("left", [-192.0, 108.0]), ("right", [576.0, 108.0]), ("up", [192.0, -108.0]), ("down", [192.0, 324.0])];
    for (dir, end) in want {
        let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
        let r = apply_dir(&mut s, cid, old, new, "push-slide", dir).unwrap_or_else(|e| panic!("{dir}: {e}"));
        assert_eq!(r["dir"], json!(dir));
        let pos = effect(&s, cid, old, "SceneCut push-slide Push").prop("position").unwrap().clone();
        let last = pos.keys.last().unwrap().value.components();
        assert!((last[0] - end[0]).abs() < 1e-6 && (last[1] - end[1]).abs() < 1e-6, "{dir}: old shot ends at {last:?}, want {end:?}");
        let seam = effect(&s, cid, new, "SceneCut push-slide Seam");
        let (ow, oh) = (seam.prop("outputWidth").unwrap().value.components()[0], seam.prop("outputHeight").unwrap().value.components()[0]);
        let swap = dir == "up" || dir == "down";
        assert_eq!((ow, oh), if swap { (100.0, 100.4) } else { (100.4, 100.0) }, "{dir}");
    }
}

#[test]
fn every_dir_of_the_kinds_that_have_one_applies() {
    let (mut s0, ..) = cut_at(29.97, 10.0, 4.0);
    let kinds = s0.execute("transition.kinds", json!({})).unwrap();
    let mut n = 0;
    for row in kinds.as_array().unwrap() {
        let kind = row["kind"].as_str().unwrap();
        let dirs = row["dirs"].as_array().unwrap();
        if row["family"] != json!("scene-cut") {
            assert!(dirs.is_empty(), "{kind}: only scene cuts take a dir");
            continue;
        }
        assert_eq!(dirs.first(), row.get("dir").filter(|d| !d.is_null()), "{kind}: the default dir comes first");
        for d in dirs {
            let dir = d.as_str().unwrap();
            for fps in [29.97, 24.0] {
                let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
                let r = apply_dir(&mut s, cid, old, new, kind, dir).unwrap_or_else(|e| panic!("{kind} {dir} at {fps}: {e}"));
                assert_eq!(r["dir"], json!(dir));
                for (kfr, t, what) in &all_keys(&s.project) {
                    assert_eq!(kfr.snap_nearest(*t), *t, "{kind} {dir} at {fps}: {what} key between frames");
                }
                n += 1;
            }
        }
    }
    // push-slide, vertical-push, elastic-push, squeeze, staggered-blocks, linear-wipe: 4 each;
    // diagonal-split 4; the blinds 2 each; at two frame rates.
    assert_eq!(n, 2 * (7 * 4 + 2 + 2));
}

#[test]
fn blinds_follow_their_dir() {
    // horizontal-blinds left: the strips come in from the right (x 2880 → 960 → −960 of 1920).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply_dir(&mut s, cid, old, new, "horizontal-blinds", "left").unwrap();
    let b1 = layer_named(&s, cid, "SceneCut horizontal-blinds Blind 1");
    let xs: Vec<f64> = b1.props.prop("transform/position").unwrap().keys.iter().map(|k| k.value.components()[0]).collect();
    assert_eq!(xs.len(), 4);
    assert!((xs[0] - 576.0).abs() < 1e-6 && (xs[1] - 192.0).abs() < 1e-6 && (xs[3] + 192.0).abs() < 1e-6, "{xs:?}");
    // A straight path: no overshoot between keys.
    assert!(b1.props.prop("transform/position").unwrap().keys.iter().all(|k| !k.spatial_auto && k.spatial_in == [0.0; 3] && k.spatial_out == [0.0; 3]));
    // 8 strips of 1920×135 (of 1080) → 384×27 solids.
    let strips: Vec<&effectcraft_project::Layer> =
        s.project.comp(ItemId(cid)).unwrap().layers.iter().filter(|l| l.name.starts_with("SceneCut horizontal-blinds Blind ")).collect();
    assert_eq!(strips.len(), 8);
    let (w, h) = effectcraft_render::source_size(&s.project, strips[0]);
    assert_eq!((w, h), (384, 27));
    // vertical-blinds up: from below (y 1620 of 1080), 240×1080 strips → 48×216.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply_dir(&mut s, cid, old, new, "vertical-blinds", "up").unwrap();
    let b1 = layer_named(&s, cid, "SceneCut vertical-blinds Blind 1");
    let p0 = b1.props.prop("transform/position").unwrap().keys[0].value.components();
    assert!((p0[0] - 24.0).abs() < 1e-6 && (p0[1] - 324.0).abs() < 1e-6, "{p0:?}");
    assert_eq!(effectcraft_render::source_size(&s.project, b1), (48, 216));
}

#[test]
fn bad_scene_requests_are_errors_and_change_nothing() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    // A dir the kind doesn't have, a kind without one, a dir on the other families, not a string.
    let e = apply_dir(&mut s, cid, old, new, "push-slide", "sideways").unwrap_err().to_string();
    assert!(e.contains("no dir") && e.contains("right"), "{e}");
    let e = apply_dir(&mut s, cid, old, new, "horizontal-blinds", "up").unwrap_err().to_string();
    assert!(e.contains("no dir"), "{e}");
    let e = apply_dir(&mut s, cid, old, new, "crossfade", "left").unwrap_err().to_string();
    assert!(e.contains("takes no `dir`"), "{e}");
    let e = apply_dir(&mut s, cid, old, new, "kick", "left").unwrap_err().to_string();
    assert!(e.contains("scene cuts"), "{e}");
    let e = apply_dir(&mut s, cid, old, new, "light-leak", "left").unwrap_err().to_string();
    assert!(e.contains("scene cuts"), "{e}");
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "push-slide", "dir": 3})).is_err());
    // The zoom cuts' options.
    let e = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "squeeze", "restScale": 100})).unwrap_err().to_string();
    assert!(e.contains("zoom cuts") && e.contains("a scene cut"), "{e}");
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "staggered-blocks", "colour": "blue"})).is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "crossfade", "dur": 0})).is_err());
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    assert_eq!(s.history.undo.len(), undo);
    // dir, dur and colour are accepted parameters.
    let r = s
        .execute_checked(
            "transition.apply",
            json!({"comp": cid, "old": old, "new": new, "kind": "staggered-blocks", "dir": "up", "dur": 1.0, "colour": ["#ff0000", "#00ff00"]}),
        )
        .unwrap();
    assert!((r["dur"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    // The palette tints the two blocks.
    let a = layer_named(&s, cid, "SceneCut staggered-blocks Block A");
    let LayerSource::Solid { item } = a.source else { panic!("a block is a solid") };
    let effectcraft_project::ItemKind::Solid(sol) = &s.project.item(item).unwrap().kind else { panic!("a solid item") };
    assert!(sol.color[0] > 0.99 && sol.color[1] < 0.01, "{:?}", sol.color);
    s.execute("edit.undo", json!({})).unwrap();
    // The new shot must play past color-dip's late start; footage must reach card-flip-3d's end.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("layer.timing", json!({"layers": [new], "out": 4.0 + 1.0 / 29.97})).unwrap();
    let e = apply(&mut s, cid, old, new, "color-dip").unwrap_err().to_string();
    assert!(e.contains("play longer"), "{e}");
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
    // Too close to the comp's start; the comp ends before it settles.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 0.2);
    let e = apply(&mut s, cid, old, new, "push-slide").unwrap_err().to_string();
    assert!(e.contains("too close to the comp's start"), "{e}");
    let (mut s, cid, old, new, _) = cut_at(29.97, 4.2, 4.0);
    let e = apply(&mut s, cid, old, new, "horizontal-blinds").unwrap_err().to_string();
    assert!(e.contains("comp too short"), "{e}");
}

#[test]
fn stack_kinds_move_the_new_shot_above_the_old_one_and_say_so() {
    for kind in STACK_KINDS {
        let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
        // The old shot on top: the new one has to come up to fade / slide in over it.
        s.execute("layer.arrange", json!({"layers": [old], "to": "front"})).unwrap();
        let r = apply(&mut s, cid, old, new, kind).unwrap();
        assert_eq!(r["stacked"], json!(true), "{kind}");
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert_eq!(comp.index_of(LayerId(new)).map(|i| i + 1), comp.index_of(LayerId(old)), "{kind}: the new shot is right above the old one");
        let order = moved(&r, new, "order");
        assert_eq!(order.len(), 1, "{kind}: {}", r["moved"]);
        assert_eq!((order[0]["from"].as_u64(), order[0]["to"].as_u64()), (Some(2), Some(1)), "{kind}: {}", r["moved"]);
        assert!(moved(&r, old, "order").is_empty(), "{kind}: only the new shot moved: {}", r["moved"]);
        // Its in point moved too, to the kind's early start.
        let inn = moved(&r, new, "in");
        assert_eq!(inn.len(), 1, "{kind}: {}", r["moved"]);
        assert!((inn[0]["from"].as_f64().unwrap() - cut).abs() < 1e-9);
        assert!(inn[0]["to"].as_f64().unwrap() < cut, "{kind}: starts early");
        // Undo puts the order back.
        s.execute("edit.undo", json!({})).unwrap();
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert!(comp.index_of(LayerId(old)) < comp.index_of(LayerId(new)), "{kind}: undo restores the order");
    }
    // A kind without `stack` leaves the order alone.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("layer.arrange", json!({"layers": [old], "to": "front"})).unwrap();
    let r = apply(&mut s, cid, old, new, "push-slide").unwrap();
    assert_eq!(r["stacked"], json!(false));
    assert!(moved(&r, new, "order").is_empty() && moved(&r, old, "order").is_empty(), "{}", r["moved"]);
    // An FX cut's restack is reported the same way (clock-wipe puts the old shot on top).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "clock-wipe").unwrap();
    assert_eq!(r["restacked"], json!(true));
    assert_eq!(moved(&r, old, "order").len(), 1, "{}", r["moved"]);
}

#[test]
fn shape_kinds_mask_copies_of_the_shots() {
    // circle-iris: a copy of the new shot, from the window's start, opens in a keyed circle.
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "circle-iris").unwrap();
    let iris = layer_named(&s, cid, "SceneCut circle-iris Iris");
    let LayerSource::Comp { item } = iris.source else { panic!("the iris is a precomp") };
    let scenes = s.project.comp(item).unwrap();
    assert_eq!(scenes.layers.len(), 1, "a copy of the new shot only");
    let fr = scenes.frame_rate;
    let start = r["cutFrame"].as_i64().unwrap() + (-0.5005 * fr.as_f64()).round() as i64;
    assert_eq!(fr.frame_at(scenes.layers.iter().next().unwrap().in_point), start, "the copy starts at the window's start");
    assert!((timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9, "the shot itself still starts on the cut");
    let mp = iris.masks().unwrap().groups().next().unwrap().prop("path").unwrap();
    assert_eq!(mp.keys.len(), 2);
    // diamond-iris: a 4-vertex polygon with straight edges, from a point to past the corners.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "diamond-iris").unwrap();
    let iris = layer_named(&s, cid, "SceneCut diamond-iris Iris");
    let mp = iris.masks().unwrap().groups().next().unwrap().prop("path").unwrap();
    assert_eq!(mp.keys.len(), 2);
    for k in &mp.keys {
        let effectcraft_project::Value::Path(p) = &k.value else { panic!("a path key") };
        assert_eq!(p.vertices.len(), 4);
        assert!(p.closed && p.in_tangents.iter().chain(&p.out_tangents).all(|t| *t == [0.0, 0.0]), "straight edges");
    }
    // diagonal-split: the old shot ends at the window's start, a masked copy carries it; re-applying
    // with no `at` finds the same cut.
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "diagonal-split").unwrap();
    assert!(timing(&s, cid, old).1.seconds() < cut - 0.4, "the old shot ends early");
    assert_eq!(moved(&r, old, "out").len(), 1, "{}", r["moved"]);
    let r2 = apply(&mut s, cid, old, new, "diagonal-split").unwrap();
    assert_eq!(r2["cutFrame"], r["cutFrame"]);
    assert!(moved(&r2, old, "out").is_empty(), "the same trims again: nothing moved: {}", r2["moved"]);
    let r3 = apply_dir(&mut s, cid, old, new, "diagonal-split", "down-left").unwrap();
    assert_eq!(r3["cutFrame"], r["cutFrame"]);
    // A zoom cut after it: both shots meet on the cut again.
    apply(&mut s, cid, old, new, "kick").unwrap();
    assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9 && (timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9);
}

#[test]
fn card_flip_turns_3d_copies_over_the_dark_backing() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
    let cut_f = r["cutFrame"].as_i64().unwrap();
    let d = (0.3003 * fr.as_f64()).round() as i64;
    assert_eq!(fr.frame_at(timing(&s, cid, old).1), cut_f - d, "the old shot ends when the turn starts");
    assert_eq!(fr.frame_at(timing(&s, cid, new).0), cut_f + d, "the new shot starts when the turn ends");
    for (name, from, to) in [("SceneCut card-flip-3d Card old", 0.0, 180.0), ("SceneCut card-flip-3d Card new", -180.0, 0.0)] {
        let l = layer_named(&s, cid, name);
        assert!(l.is_3d(), "{name} is a 3-D layer");
        let ry = l.props.prop("transform/rotationY").unwrap();
        let v: Vec<f64> = ry.keys.iter().map(|k| k.value.components()[0]).collect();
        assert_eq!(v, [from, to], "{name}");
    }
    let bg = layer_named(&s, cid, "SceneCut card-flip-3d BG");
    let comp = s.project.comp(ItemId(cid)).unwrap();
    assert!(comp.index_of(bg.id) > comp.index_of(LayerId(old)) && comp.index_of(bg.id) > comp.index_of(LayerId(new)), "the backing is under both shots");
    // Re-applying finds the same cut (the trims go back first).
    let r2 = apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    assert_eq!(r2["cutFrame"], r["cutFrame"]);
    apply(&mut s, cid, old, new, "crossfade").unwrap();
    assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9, "the old shot ends on the cut again");
    // color-dip starts the new shot 2 frames after the cut.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "color-dip").unwrap();
    assert_eq!(fr.frame_at(timing(&s, cid, new).0), r["cutFrame"].as_i64().unwrap() + 2);
    let r2 = apply(&mut s, cid, old, new, "color-dip").unwrap();
    assert_eq!(r2["cutFrame"], r["cutFrame"], "re-applying finds the same cut");
}

#[test]
fn hold_keys_straight_paths_and_pivots() {
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "elastic-push").unwrap();
    // The mirrored edges widen on the cut, not before: a hold key.
    let tile = effect(&s, cid, new, "SceneCut elastic-push Mirror edges").prop("outputWidth").unwrap().clone();
    assert_eq!(tile.keys.len(), 2);
    assert_eq!(tile.keys[0].out_interp, Interp::Hold);
    // The push overshoots on a straight path; Expo Out 1 then Expo In-Out on its segments.
    let pos = effect(&s, cid, new, "SceneCut elastic-push Push").prop("position").unwrap().clone();
    assert_eq!(pos.keys.len(), 4);
    assert!(pos.keys.iter().all(|k| !k.spatial_auto && k.spatial_in == [0.0; 3] && k.spatial_out == [0.0; 3]), "spatially linear");
    assert!(pos.keys.iter().take(3).all(|k| k.out_interp == Interp::Bezier));
    // squeeze: each shot's Transform pivots on its own edge (0, 540 and 1920, 540 of 1920×1080).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "squeeze").unwrap();
    for (l, x) in [(old, 0.0), (new, 384.0)] {
        let g = effect(&s, cid, l, "SceneCut squeeze Squeeze");
        let a = g.prop("anchor").unwrap().value.components();
        let p = g.prop("position").unwrap().value.components();
        assert!((a[0] - x).abs() < 1e-9 && (a[1] - 108.0).abs() < 1e-9 && a == p, "{a:?} {p:?}");
        assert_eq!(g.prop("scaleWidth").unwrap().keys.len(), 2);
    }
    // squeeze up: the pivot moves to the top edge and the squeeze is in height.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply_dir(&mut s, cid, old, new, "squeeze", "up").unwrap();
    let g = effect(&s, cid, old, "SceneCut squeeze Squeeze");
    let a = g.prop("anchor").unwrap().value.components();
    assert!((a[0] - 192.0).abs() < 1e-9 && a[1].abs() < 1e-9, "{a:?}");
    assert_eq!(g.prop("scaleHeight").unwrap().keys.len(), 2);
    assert!(g.prop("scaleWidth").unwrap().keys.is_empty());
}

#[test]
fn switching_families_on_the_same_pair_leaves_nothing_behind() {
    let (mut s, cid, old, new, cut) = cut_at(29.97, 10.0, 4.0);
    let items0 = s.project.items.len();
    // Every scene cut, with a zoom cut and FX cuts (one that trims, one with copies) between them,
    // then backwards: each replaces the one before, whatever its family.
    let mut seq: Vec<&str> = vec![];
    for (i, k) in SCENE_KINDS.iter().enumerate() {
        seq.push(k);
        seq.push(["kick", "page-burn", "clock-wipe", "light-leak", "white-flash"][i % 5]);
    }
    seq.extend(SCENE_KINDS.iter().rev());
    for kind in seq {
        let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert!((r["at"].as_f64().unwrap() - cut).abs() < 1e-9, "{kind}: the cut moved to {}", r["at"]);
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert_eq!(comp.layers.len(), 2 + r["layers"].as_array().unwrap().len(), "{kind}: leftover layers");
        let pre = match r["family"].as_str() {
            Some("fx-cut") => format!("FxCut {kind} "),
            Some("scene-cut") => format!("SceneCut {kind} "),
            _ => format!("ZoomCut {kind} "),
        };
        for l in [old, new] {
            for e in transition_effects(&s, cid, l) {
                assert!(e.starts_with(&pre), "{kind}: leftover effect “{e}”");
            }
        }
        for l in comp.layers.iter().filter(|l| l.id != LayerId(old) && l.id != LayerId(new)) {
            assert!(l.name.starts_with(&pre), "{kind}: leftover layer “{}”", l.name);
        }
        for i in transition_items(&s) {
            assert!(i.starts_with(&pre), "{kind}: leftover item “{i}”");
        }
        assert!(orphan_solids(&s).is_empty(), "{kind}: orphan solids {:?}", orphan_solids(&s));
    }
    apply(&mut s, cid, old, new, "kick").unwrap();
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2);
    assert!(transition_items(&s).is_empty());
    assert_eq!(s.project.items.len(), items0, "no items left behind (solids, precomps)");
    assert_eq!(transition_effects(&s, cid, new), ["ZoomCut kick Zoom"]);
    assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9 && (timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9, "the shots meet on the cut");
}

#[test]
fn push_slide_mid_frame_has_no_transparent_pixel() {
    // At 1920×1080, the size the seam's 100.4 % mirrored edge was tuned for (3.8 px a side).
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at_size(fps, 10.0, 4.0, 1920, 1080);
        let r = apply(&mut s, cid, old, new, "push-slide").unwrap();
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let f = (r["startFrame"].as_i64().unwrap() + r["cutFrame"].as_i64().unwrap()) / 2;
        let img = s.render(ItemId(cid), fr.tick_of(f), RenderOpts { scale: 1.0, motion_blur: false, ..Default::default() });
        let (w, h) = (i64::from(img.width), i64::from(img.height));
        let (mut red, mut blue) = (0, 0);
        for y in 0..h {
            for x in 0..w {
                let px = img.get(x, y);
                assert!(px[3] > 0.999, "{fps} fps frame {f}: transparent pixel at ({x}, {y}): {px:?}");
                if px[0] > px[2] {
                    red += 1;
                } else {
                    blue += 1;
                }
            }
        }
        // Both shots show, side by side, mid-push.
        assert!(red > 0 && blue > 0, "{fps} fps: red {red} blue {blue}");
    }
}

#[test]
fn scene_cut_menu_lists_the_15_kinds() {
    let bar = crate::menus::menu_bar();
    let json = serde_json::to_string(bar).unwrap();
    assert!(json.contains("\"label\":\"Scene Cut\""), "no Scene Cut submenu");
    for kind in SCENE_KINDS {
        assert!(json.contains(&format!("{{\"kind\":\"{kind}\"}}")), "{kind} isn't in the menus");
    }
    // The shipped table is the shelf's copy with labels instead of local paths.
    assert!(!include_str!("../presets/scene-cut.kinds.json").contains(concat!("/", "Users", "/")));
}

// ---------------------------------------------------------------- safety rules (ZC4)

/// [`cut_at`] with two footage shots that have sound (stills with an audio stream: nothing is read).
fn cut_at_audio(fps: f64, secs: f64, at: f64) -> (Session, u64, u64, u64, f64) {
    use effectcraft_project::{Footage, FootageKind, ItemKind};
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cut", "width": 384, "height": 216, "frameRate": fps, "duration": secs})).unwrap();
    let cid = s.active_comp_id().unwrap().0;
    let add = |s: &mut Session, name: &str| {
        let f = Footage {
            path: format!("/nowhere/{name}.mov"),
            kind: FootageKind::Still,
            width: 384,
            height: 216,
            has_video: true,
            has_audio: true,
            ..Default::default()
        };
        let item = std::sync::Arc::make_mut(&mut s.project).add_item(name, Default::default(), None, ItemKind::Footage(f)).0;
        let l = s.execute("layer.addItem", json!({"comp": cid, "item": item})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.rename", json!({"layers": [l], "name": name})).unwrap();
        l
    };
    let old = add(&mut s, "A Footage");
    let new = add(&mut s, "B Footage");
    s.execute("layer.timing", json!({"layers": [old], "out": at})).unwrap();
    s.execute("layer.timing", json!({"layers": [new], "in": at})).unwrap();
    let cut = s.active_comp().unwrap().layer(LayerId(new)).unwrap().in_point.seconds();
    (s, cid, old, new, cut)
}

/// The audio keep copies in the comp: (id, name, in, out, video, audio, layer number from 1 like
/// `index_of`).
fn keeps(s: &Session, cid: u64) -> Vec<(u64, String, f64, f64, bool, bool, usize)> {
    let comp = s.project.comp(ItemId(cid)).unwrap();
    comp.layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.name.contains(" keep)"))
        .map(|(i, l)| (l.id.0, l.name.clone(), l.in_point.seconds(), l.out_point.seconds(), l.switches.video, l.switches.audio, i + 1))
        .collect()
}

fn index(s: &Session, cid: u64, l: u64) -> usize {
    s.project.comp(ItemId(cid)).unwrap().index_of(LayerId(l)).unwrap()
}

#[test]
fn trimmed_shots_keep_their_sound() {
    // diagonal-split ends the old shot 0.5 s early: its sound plays on to the cut from a copy. (It
    // starts the new shot 0.5 s early too: see the_picture_starting_early_never_starts_the_sound_early.)
    let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "diagonal-split").unwrap();
    let (_, oout) = timing(&s, cid, old);
    assert!(oout.seconds() < cut - 0.4, "the picture is trimmed");
    let k: Vec<_> = keeps(&s, cid).into_iter().filter(|k| k.1.starts_with("A Footage")).collect();
    assert_eq!(k.len(), 1, "{k:?}");
    let (kid, name, a, b, video, audio, i) = k[0].clone();
    assert_eq!(name, "A Footage audio (SceneCut keep)");
    assert!((a - oout.seconds()).abs() < 1e-9 && (b - cut).abs() < 1e-9, "the copy covers exactly the trimmed span: {a}–{b}");
    assert!(!video && audio, "audio only");
    assert_eq!(i, index(&s, cid, old) + 1, "right below its shot");
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let (ol, kl) = (comp.layer(LayerId(old)).unwrap(), comp.layer(LayerId(kid)).unwrap());
    assert_eq!((ol.source.item(), ol.comp_time(Tick::ZERO)), (kl.source.item(), kl.comp_time(Tick::ZERO)), "the same source time");
    assert!(ol.switches.audio, "the old shot's audio stays on");
    assert!(!kl.comment.ends_with(" · muted"), "{}", kl.comment);
    assert!(kl.comment.starts_with(&format!("SceneCut {old}->{new}")), "tagged for the pair: {}", kl.comment);
    // The reply lists it, with frames.
    let m = moved(&r, kid, "audio-keep");
    assert_eq!(m.len(), 1, "{}", r["moved"]);
    assert_eq!(m[0]["of"], json!(old));
    let fr = comp.frame_rate;
    assert_eq!(m[0]["toFrame"].as_i64(), Some(r["cutFrame"].as_i64().unwrap()));
    assert_eq!(m[0]["fromFrame"].as_i64(), Some(fr.frame_at(oout)));
    assert!(moved(&r, old, "out")[0]["fromFrame"].is_i64(), "in / out moves carry frames too");
    // The copy of the old shot inside the Split precomp is muted: the sound never doubles.
    let split = layer_named(&s, cid, "SceneCut diagonal-split Split");
    let LayerSource::Comp { item } = split.source else { panic!("a precomp") };
    assert!(s.project.comp(item).unwrap().layers.iter().all(|l| !l.switches.audio), "copies of a shot are muted");
    // color-dip starts the new shot 2 frames late: those 2 frames of its sound are kept.
    let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "color-dip").unwrap();
    let k = keeps(&s, cid);
    assert_eq!(k.len(), 1, "{k:?}");
    assert_eq!(k[0].1, "B Footage audio (SceneCut keep)");
    assert!((k[0].2 - cut).abs() < 1e-9 && (k[0].3 - timing(&s, cid, new).0.seconds()).abs() < 1e-9, "{k:?}");
    assert_eq!(k[0].6, index(&s, cid, new) + 1);
    // card-flip-3d trims both shots: one copy each.
    let (mut s, cid, old, new, _) = cut_at_audio(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    let names: Vec<String> = keeps(&s, cid).into_iter().map(|k| k.1).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    // An old shot that played on past the cut (under the new one) keeps that sound when a kind trims
    // it to the cut; an FX cut and a scene cut alike.
    for kind in ["crossfade", "light-leak"] {
        let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
        s.execute("layer.timing", json!({"layers": [old], "out": 6.0})).unwrap();
        let orig = timing(&s, cid, old).1.seconds();
        let r = apply(&mut s, cid, old, new, kind).unwrap();
        assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9, "{kind}: the picture ends on the cut");
        let k: Vec<_> = keeps(&s, cid).into_iter().filter(|k| k.1.starts_with("A Footage")).collect();
        assert_eq!(k.len(), 1, "{kind}: {k:?}");
        assert!((k[0].2 - cut).abs() < 1e-9 && (k[0].3 - orig).abs() < 1e-9, "{kind}: {k:?}");
        assert_eq!(moved(&r, k[0].0, "audio-keep").len(), 1, "{kind}: {}", r["moved"]);
    }
    // Shots without sound get no copy.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    assert!(keeps(&s, cid).is_empty());
}

#[test]
fn the_picture_starting_early_never_starts_the_sound_early() {
    // Kinds that start the new shot's picture before the cut (FX and scene cuts alike): one copy
    // carries its sound over its whole span as it was, and the shot's own audio goes off.
    for kind in ["clock-wipe", "push-slide", "crossfade", "diagonal-split", "page-burn"] {
        let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
        let n0 = timing(&s, cid, new);
        let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert!(timing(&s, cid, new).0.seconds() < cut - 0.2, "{kind}: the picture starts early");
        let k: Vec<_> = keeps(&s, cid).into_iter().filter(|k| k.1.starts_with("B Footage")).collect();
        assert_eq!(k.len(), 1, "{kind}: {k:?}");
        let (kid, name, a, b, video, audio, i) = k[0].clone();
        assert_eq!(name, format!("B Footage audio ({} keep)", if kind == "clock-wipe" || kind == "page-burn" { "FxCut" } else { "SceneCut" }));
        assert!((a - n0.0.seconds()).abs() < 1e-9 && (b - n0.1.seconds()).abs() < 1e-9, "{kind}: the shot's whole span as it was: {a}–{b}");
        assert!(!video && audio, "{kind}: audio only");
        assert_eq!(i, index(&s, cid, new) + 1, "{kind}: right below its shot");
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert!(!comp.layer(LayerId(new)).unwrap().switches.audio, "{kind}: the shot's own audio is off");
        assert!(comp.layer(LayerId(kid)).unwrap().comment.ends_with(" · muted"), "{kind}");
        let m = moved(&r, kid, "audio-keep");
        assert_eq!(m.len(), 1, "{kind}: {}", r["moved"]);
        assert_eq!((m[0]["of"].as_u64(), m[0]["shotAudio"].as_str()), (Some(new), Some("off")), "{kind}");
        // Switching to a zoom cut gives the shot its span and its audio back, no copy left.
        apply(&mut s, cid, old, new, "kick").unwrap();
        let comp = s.project.comp(ItemId(cid)).unwrap();
        assert!(comp.layer(LayerId(new)).unwrap().switches.audio, "{kind}: the shot's audio is back on");
        assert!(keeps(&s, cid).is_empty(), "{kind}");
        assert_eq!(timing(&s, cid, new), n0, "{kind}");
        // Undo twice: as it was.
        s.execute("edit.undo", json!({})).unwrap();
        s.execute("edit.undo", json!({})).unwrap();
        assert!(s.project.comp(ItemId(cid)).unwrap().layer(LayerId(new)).unwrap().switches.audio, "{kind}: undo");
    }
}

#[test]
fn audio_keep_copies_go_with_reapply_switching_and_undo() {
    let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
    s.execute("layer.timing", json!({"layers": [old], "out": 6.0})).unwrap();
    let (o0, n0) = (timing(&s, cid, old), timing(&s, cid, new));
    let items0 = s.project.items.len();
    // card-flip-3d: the old shot's sound from its early end to 6 s, the new one's to its late start.
    apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    assert_eq!(keeps(&s, cid).len(), 2);
    // Re-applying: the copies go with the transition and come back once, over the same spans.
    let before: Vec<(f64, f64)> = keeps(&s, cid).into_iter().map(|k| (k.2, k.3)).collect();
    let r = apply(&mut s, cid, old, new, "card-flip-3d").unwrap();
    let after: Vec<(f64, f64)> = keeps(&s, cid).into_iter().map(|k| (k.2, k.3)).collect();
    assert_eq!(before, after, "no stacking");
    assert!(moved(&r, old, "out").is_empty() && moved(&r, new, "in").is_empty(), "the same trims again: {}", r["moved"]);
    // Another family: the old shot gets its whole span back first (6 s), then that kind's trim.
    let r = apply(&mut s, cid, old, new, "kick").unwrap();
    assert!(keeps(&s, cid).is_empty(), "a zoom cut trims nothing");
    assert_eq!((timing(&s, cid, old), timing(&s, cid, new)), (o0, n0), "both shots have their spans back");
    assert!(r["moved"].as_array().unwrap().iter().all(|m| m["what"] != json!("audio-keep")), "{}", r["moved"]);
    // Undo brings the card flip back, copies and all; again: the comp as it was.
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(keeps(&s, cid).len(), 2);
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(keeps(&s, cid).is_empty());
    assert_eq!((timing(&s, cid, old), timing(&s, cid, new)), (o0, n0));
    assert_eq!(s.project.items.len(), items0);
    assert!((timing(&s, cid, old).1.seconds() - 6.0).abs() < 0.05 && (timing(&s, cid, new).0.seconds() - cut).abs() < 1e-9);
    // FX page-burn keeps the old shot's last 1.2 s of sound the same way.
    let (mut s, cid, old, new, cut) = cut_at_audio(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "page-burn").unwrap();
    let k: Vec<_> = keeps(&s, cid).into_iter().filter(|k| k.1.starts_with("A Footage")).collect();
    assert_eq!(k.len(), 1, "{k:?}");
    assert_eq!(k[0].1, "A Footage audio (FxCut keep)");
    assert!((k[0].3 - cut).abs() < 1e-9 && k[0].2 < cut - 1.1, "{k:?}");
}

#[test]
fn into_window_keeps_the_shots_sound_in_its_precomp() {
    let (mut s, cid, old, new, _) = cut_at_audio(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "into-window").unwrap();
    let win = r["new"].as_u64().unwrap();
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let wl = comp.layer(LayerId(win)).unwrap();
    let LayerSource::Comp { item } = wl.source else { panic!("the window is a precomp") };
    assert!(effectcraft_render::audio::comp_has_audio(&s.project, item), "the shot still sounds inside the window");
    assert!(effectcraft_render::audio::audible(&s.project, wl), "the window layer plays it");
    let inner = s.project.comp(item).unwrap();
    assert!(
        inner.layers.iter().any(|l| matches!(l.source, LayerSource::Footage { .. }) && effectcraft_render::audio::audible(&s.project, l)),
        "the shot inside keeps its audio switch"
    );
    assert_eq!(moved(&r, new, "precomposed").len(), 1, "{}", r["moved"]);
}

/// A solid named `name` placed right below `under`.
fn third(s: &mut Session, cid: u64, under: u64, name: &str) -> u64 {
    let t = s.execute("layer.newSolid", json!({"comp": cid, "name": name, "color": "#20d020"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.arrange", json!({"comp": cid, "layers": [t], "to": "front", "above": under})).unwrap();
    s.execute("layer.arrange", json!({"comp": cid, "layers": [under], "to": "front", "above": t})).unwrap();
    t
}

#[test]
fn a_third_layer_between_the_shots_is_never_moved_past() {
    // clock-wipe would put the old shot above the new one, past “Logo”.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    third(&mut s, cid, new, "Logo");
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let e = apply(&mut s, cid, old, new, "clock-wipe").unwrap_err().to_string();
    assert!(e.contains("“Logo”") && e.contains("next to each other"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    assert_eq!(s.history.undo.len(), undo);
    // crossfade would bring the new shot up over the old one, past “Logo”.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("layer.arrange", json!({"layers": [old], "to": "front"})).unwrap();
    third(&mut s, cid, old, "Logo");
    let before = s.project.clone();
    let e = apply(&mut s, cid, old, new, "crossfade").unwrap_err().to_string();
    assert!(e.contains("“Logo”"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // squeeze shows both shots over its backing: “Logo” between them while it runs is an error…
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let logo = third(&mut s, cid, new, "Logo");
    let e = apply(&mut s, cid, old, new, "squeeze").unwrap_err().to_string();
    assert!(e.contains("“Logo”") && e.contains("together"), "{e}");
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 3);
    // …one that isn't on then is fine, and stays where it is, untouched.
    s.execute("layer.timing", json!({"layers": [logo], "in": 8.0, "out": 9.0})).unwrap();
    let t0 = timing(&s, cid, logo);
    let r = apply(&mut s, cid, old, new, "squeeze").unwrap();
    assert_eq!(timing(&s, cid, logo), t0);
    assert!(r["moved"].as_array().unwrap().iter().all(|m| m["layer"] != json!(logo)), "{}", r["moved"]);
    // A zoom cut is a hard cut: a layer between the shots doesn't matter.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    third(&mut s, cid, new, "Logo");
    apply(&mut s, cid, old, new, "kick").unwrap();
}

#[test]
fn transitions_on_a_shared_shot_never_collide_or_touch_each_other() {
    // A 0–4 s, B 4–4.6 s, C from 4.6 s; C on top.
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cut", "width": 384, "height": 216, "frameRate": 29.97, "duration": 10.0})).unwrap();
    let solid =
        |s: &mut Session, name: &str, colour: &str| s.execute("layer.newSolid", json!({"name": name, "color": colour})).unwrap()["layer"].as_u64().unwrap();
    let (a, b, c) = (solid(&mut s, "A", "#d02020"), solid(&mut s, "B", "#20d020"), solid(&mut s, "C", "#2040d0"));
    let cid = s.active_comp_id().unwrap().0;
    s.execute("layer.timing", json!({"layers": [a], "out": 4.0})).unwrap();
    s.execute("layer.timing", json!({"layers": [b], "in": 4.0, "out": 4.6})).unwrap();
    s.execute("layer.timing", json!({"layers": [c], "in": 4.6})).unwrap();
    let pair = |s: &mut Session, o: u64, n: u64, kind: &str| s.execute("transition.apply", json!({"comp": cid, "old": o, "new": n, "kind": kind}));
    // B → C pushes in from 4.1 s: B's out-side move.
    pair(&mut s, b, c, "push-slide").unwrap();
    // A kick on A → B keys B from 4.0 to 5.27 s: it would overlap the push on B.
    let before = s.project.clone();
    let e = pair(&mut s, a, b, "kick").unwrap_err().to_string();
    assert!(e.contains("overlap") && e.contains("“B”"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // A crossfade on A → B (3.5–4.0 s) doesn't: both live on B.
    pair(&mut s, a, b, "crossfade").unwrap();
    let names = |s: &Session, l: u64| transition_effects(s, cid, l);
    assert!(
        names(&s, b).contains(&"SceneCut push-slide Push".to_string()) && names(&s, b).contains(&"SceneCut crossfade Fade".to_string()),
        "{:?}",
        names(&s, b)
    );
    // Re-applying one pair removes only its own effects from the shared shot.
    pair(&mut s, a, b, "crossfade").unwrap();
    assert_eq!(names(&s, b).iter().filter(|n| n.starts_with("SceneCut push-slide ")).count(), 1, "{:?}", names(&s, b));
    assert_eq!(names(&s, b).iter().filter(|n| n.starts_with("SceneCut crossfade ")).count(), 1, "{:?}", names(&s, b));
    pair(&mut s, b, c, "push-slide").unwrap();
    assert_eq!(names(&s, b).iter().filter(|n| n.starts_with("SceneCut crossfade ")).count(), 1, "{:?}", names(&s, b));
    assert_eq!(names(&s, b).iter().filter(|n| n.starts_with("SceneCut push-slide ")).count(), 1, "{:?}", names(&s, b));
    assert_eq!(names(&s, c), ["SceneCut push-slide Seam", "SceneCut push-slide Push"]);
    // Switching B → C to an FX cut leaves A → B's crossfade alone.
    pair(&mut s, b, c, "light-leak").unwrap();
    assert!(names(&s, b).iter().all(|n| !n.starts_with("SceneCut push-slide ")), "{:?}", names(&s, b));
    assert_eq!(names(&s, b).iter().filter(|n| n.starts_with("SceneCut crossfade ")).count(), 1, "{:?}", names(&s, b));
}

#[test]
fn a_mirrored_tile_goes_with_its_own_transition_on_a_shared_shot() {
    // white-flash zooms the old shot to 98 %: it gets a keyless `Mirror edges` tile the table lists
    // for the new shot only. A 0–3 s, B 3–7 s, C from 7 s (C on top); white-flash on both cuts.
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cut", "width": 384, "height": 216, "frameRate": 29.97, "duration": 10.0})).unwrap();
    let solid =
        |s: &mut Session, name: &str, colour: &str| s.execute("layer.newSolid", json!({"name": name, "color": colour})).unwrap()["layer"].as_u64().unwrap();
    let (a, b, c) = (solid(&mut s, "A", "#d02020"), solid(&mut s, "B", "#20d020"), solid(&mut s, "C", "#2040d0"));
    let cid = s.active_comp_id().unwrap().0;
    s.execute("layer.timing", json!({"layers": [a], "out": 3.0})).unwrap();
    s.execute("layer.timing", json!({"layers": [b], "in": 3.0, "out": 7.0})).unwrap();
    s.execute("layer.timing", json!({"layers": [c], "in": 7.0})).unwrap();
    let pair = |s: &mut Session, o: u64, n: u64, kind: &str| s.execute("transition.apply", json!({"comp": cid, "old": o, "new": n, "kind": kind})).unwrap();
    let (tile, zoom, blur) = ("ZoomCut white-flash Mirror edges", "ZoomCut white-flash Zoom", "ZoomCut white-flash Blur");
    pair(&mut s, a, b, "white-flash");
    assert_eq!(zoom_cut_effects(&s, cid, a), [tile, zoom], "the old shot's own tile");
    pair(&mut s, b, c, "white-flash");
    assert_eq!(zoom_cut_effects(&s, cid, b), [tile, zoom, blur, tile, zoom]);
    // Re-applying either cut replaces only its own effects on B, tile included.
    pair(&mut s, b, c, "white-flash");
    assert_eq!(zoom_cut_effects(&s, cid, b), [tile, zoom, blur, tile, zoom]);
    pair(&mut s, a, b, "white-flash");
    assert_eq!(zoom_cut_effects(&s, cid, b), [tile, zoom, tile, zoom, blur], "B → C's first, A → B's added again");
    assert_eq!(zoom_cut_effects(&s, cid, a), [tile, zoom], "no stacking on A");
    // Another kind on B → C: its tile and zoom go from B, A → B's stay.
    pair(&mut s, b, c, "kick");
    assert_eq!(zoom_cut_effects(&s, cid, b), [tile, zoom, blur]);
    assert_eq!(zoom_cut_effects(&s, cid, c), ["ZoomCut kick Zoom"]);
    // Another kind on A → B: nothing of the white flash is left anywhere.
    pair(&mut s, a, b, "kick");
    assert!(zoom_cut_effects(&s, cid, a).is_empty(), "{:?}", zoom_cut_effects(&s, cid, a));
    assert_eq!(zoom_cut_effects(&s, cid, b), ["ZoomCut kick Zoom"]);
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 3, "no lights left");
}

// ---------------------------------------------------------------- keyframe moves (ZC5)

#[test]
fn whip_pan_reads_its_track_half_a_frame_ahead_on_its_own_shutter() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let samples = s.project.comp(ItemId(cid)).unwrap().motion_blur_samples;
        let r = apply(&mut s, cid, old, new, "whip-pan").unwrap();
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let cut_f = r["cutFrame"].as_i64().unwrap();
        let n = (0.333667 * fr.as_f64()).round() as i64;
        for (l, want, xs) in [(old, [cut_f - n, cut_f], [192.0, 192.0 - 384.0]), (new, [cut_f, cut_f + n], [192.0 + 384.0, 192.0])] {
            let layer = s.project.comp(ItemId(cid)).unwrap().layer(LayerId(l)).unwrap().clone();
            let g = effect(&s, cid, l, "ZoomCut whip-pan Whip");
            // Its own 360° shutter: the comp's shutter is never touched.
            assert_eq!(g.prop("useCompositionShutterAngle").unwrap().value, effectcraft_project::Value::Bool(false), "{fps}");
            assert_eq!(g.prop("shutterAngle").unwrap().value.components(), [360.0], "{fps}");
            let pos = g.prop("position").unwrap();
            let frames: Vec<i64> = pos.keys.iter().map(|k| fr.frame_at(layer.comp_time(k.time))).collect();
            assert_eq!(frames, want, "{fps}: {} keys", layer.name);
            let got: Vec<f64> = pos.keys.iter().map(|k| k.value.components()[0]).collect();
            assert!((got[0] - xs[0]).abs() < 1e-6 && (got[1] - xs[1]).abs() < 1e-6, "{fps}: {got:?} vs {xs:?}");
            // lead 0.5: read half a frame later, so the shutter opens on the frame.
            assert_eq!(pos.expr.as_ref().map(|e| e.text.as_str()), Some("valueAtTime(time + 0.5 * thisComp.frameDuration)"), "{fps}");
            // Mirrored edges fill what slides in.
            let tile = effect(&s, cid, l, "ZoomCut whip-pan Mirror edges");
            assert_eq!(tile.prop("outputWidth").unwrap().value.components(), [300.0]);
        }
        assert_eq!(s.project.comp(ItemId(cid)).unwrap().motion_blur_samples, samples, "the comp's settings stay");
    }
}

/// The scene cuts that move the old shot right above the new one (a copy of the new shot is
/// uncovered over it).
const OLD_OVER_NEW: [&str; 2] = ["linear-wipe", "point-iris"];

#[test]
fn linear_wipe_and_point_iris_uncover_a_copy_of_the_new_shot_over_the_old_one() {
    for kind in OLD_OVER_NEW {
        for fps in [29.97, 24.0] {
            let (mut s, cid, old, new, cut) = cut_at(fps, 10.0, 4.0);
            let r = apply(&mut s, cid, old, new, kind).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let start = r["cutFrame"].as_i64().unwrap() + (-0.7007 * fr.as_f64()).round() as i64;
            assert_eq!(r["startFrame"].as_i64(), Some(start), "{kind} at {fps}");
            // The new shot starts at the window's start, under the old one (moved right above it).
            assert_eq!(fr.frame_at(timing(&s, cid, new).0), start, "{kind} at {fps}");
            assert!((timing(&s, cid, old).1.seconds() - cut).abs() < 1e-9, "{kind}: the old shot still ends on the cut");
            assert_eq!(r["stacked"], json!(true), "{kind}");
            let comp = s.project.comp(ItemId(cid)).unwrap();
            assert_eq!(comp.index_of(LayerId(old)).map(|i| i + 1), comp.index_of(LayerId(new)), "{kind}: the old shot right above the new one");
            assert_eq!(moved(&r, old, "order").len(), 1, "{kind}: {}", r["moved"]);
            // The copy: a precomp of the new shot from the window's start, on top, ending on the cut.
            let name = if kind == "linear-wipe" { "SceneCut linear-wipe Wipe" } else { "SceneCut point-iris Iris" };
            let c = layer_named(&s, cid, name);
            assert_eq!(comp.index_of(c.id), Some(1), "{kind}: the copy is on top");
            assert_eq!((fr.frame_at(c.in_point), fr.frame_at(c.out_point)), (start, r["cutFrame"].as_i64().unwrap()), "{kind}");
            let LayerSource::Comp { item } = c.source else { panic!("a precomp") };
            assert_eq!(s.project.comp(item).unwrap().layers.len(), 1, "{kind}: a copy of the new shot only");
            let mp = c.masks().unwrap().groups().next().unwrap().prop("path").unwrap();
            let frames: Vec<i64> = mp.keys.iter().map(|k| fr.frame_at(c.comp_time(k.time))).collect();
            assert_eq!(frames, [start, r["cutFrame"].as_i64().unwrap()], "{kind} at {fps}");
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 2, "{kind}: undo");
        }
    }
    // linear-wipe: a hard edge from the right edge (x 384 of a 384-wide comp) to the left one.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "linear-wipe").unwrap();
    let mp = layer_named(&s, cid, "SceneCut linear-wipe Wipe").masks().unwrap().groups().next().unwrap().prop("path").unwrap().clone();
    let xs = |k: &effectcraft_keyframe::Keyframe| {
        let effectcraft_project::Value::Path(p) = &k.value else { panic!("a path key") };
        p.vertices.iter().map(|v| v[0]).collect::<Vec<f64>>()
    };
    assert_eq!(xs(&mp.keys[0]), [384.0, 384.0, 384.0, 384.0]);
    assert_eq!(xs(&mp.keys[1]), [0.0, 384.0, 384.0, 0.0]);
    // dir right: from the left edge.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    apply_dir(&mut s, cid, old, new, "linear-wipe", "right").unwrap();
    let mp = layer_named(&s, cid, "SceneCut linear-wipe Wipe").masks().unwrap().groups().next().unwrap().prop("path").unwrap().clone();
    assert_eq!(xs(&mp.keys[0]), [0.0, 0.0, 0.0, 0.0]);
}

#[test]
fn point_iris_opens_where_centre_says_and_still_covers_the_frame() {
    // The circle's top vertex: [cx, cy − r] (layer px of the 384×216 copy: 1920×1080 × 0.2).
    let top = |s: &Session, cid: u64, key: usize| -> [f64; 2] {
        let mp = layer_named(s, cid, "SceneCut point-iris Iris").masks().unwrap().groups().next().unwrap().prop("path").unwrap().clone();
        let effectcraft_project::Value::Path(p) = &mp.keys[key].value else { panic!("a path key") };
        p.vertices[0]
    };
    // Default: the demo face [960, 440], the table's last radius 1250 already covers the frame.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let r = apply(&mut s, cid, old, new, "point-iris").unwrap();
    assert_eq!(r["centre"], json!([960, 440]));
    let t = top(&s, cid, 1);
    assert!((t[0] - 192.0).abs() < 1e-6 && (t[1] - (88.0 - 250.0)).abs() < 1e-6, "{t:?}");
    // A corner: the last radius grows to the farthest corner + 96 px (ceil(2067.1 + 96) = 2164).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let r = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "point-iris", "centre": [100, 100]})).unwrap();
    assert_eq!(r["centre"], json!([100.0, 100.0]));
    let t0 = top(&s, cid, 0);
    assert!((t0[0] - 20.0).abs() < 1e-6 && (t0[1] - (20.0 - 0.4)).abs() < 1e-6, "the circle opens at the centre: {t0:?}");
    let t = top(&s, cid, 1);
    assert!((t[0] - 20.0).abs() < 1e-6 && (t[1] - (20.0 - 2164.0 * 0.2)).abs() < 1e-6, "{t:?}");
    // centre on a kind without it, or not a point: errors that change nothing.
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    let before = s.project.clone();
    let e = s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "squeeze", "centre": [100, 100]})).unwrap_err().to_string();
    assert!(e.contains("point-iris") && e.contains("a scene cut"), "{e}");
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "point-iris", "centre": "face"})).is_err());
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "light-leak", "centre": [1, 2]})).is_err());
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // A zoom cut's centre still works.
    assert!(s.execute("transition.apply", json!({"comp": cid, "old": old, "new": new, "kind": "kick", "centre": [960, 400]})).is_ok());
}

#[test]
fn split_handoff_slides_masked_copies_of_both_shots_and_trims_nothing() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, old, new, _) = cut_at(fps, 10.0, 4.0);
        let (o0, n0) = (timing(&s, cid, old), timing(&s, cid, new));
        let r = apply(&mut s, cid, old, new, "split-handoff").unwrap_or_else(|e| panic!("{fps}: {e}"));
        assert_eq!((timing(&s, cid, old), timing(&s, cid, new)), (o0, n0), "{fps}: no trims");
        assert!(r["moved"].as_array().unwrap().is_empty(), "{fps}: {}", r["moved"]);
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let cut_f = r["cutFrame"].as_i64().unwrap();
        let start = cut_f + (-1.9019 * fr.as_f64()).round() as i64;
        assert_eq!(r["startFrame"].as_i64(), Some(start));
        for (name, shot_count) in [("SceneCut split-handoff Right", 1), ("SceneCut split-handoff Left", 1)] {
            let c = layer_named(&s, cid, name);
            let LayerSource::Comp { item } = c.source else { panic!("{name}: a precomp") };
            assert_eq!(s.project.comp(item).unwrap().layers.len(), shot_count, "{name}");
            assert_eq!((fr.frame_at(c.in_point), fr.frame_at(c.out_point)), (start, cut_f), "{fps}: {name}");
            // Its slide (straight) and its mask: 5 keys on the comp's frames.
            let pos = c.props.prop("transform/position").unwrap();
            assert_eq!(pos.keys.len(), 5, "{name}");
            assert!(pos.keys.iter().all(|k| !k.spatial_auto && k.spatial_in == [0.0; 3]), "{name}: a straight slide");
            let mp = c.masks().unwrap().groups().next().unwrap().prop("path").unwrap();
            assert_eq!(mp.keys.len(), 5, "{name}");
        }
        // The new shot's copy starts 1.9 s early, inside its precomp.
        let right = layer_named(&s, cid, "SceneCut split-handoff Right");
        let LayerSource::Comp { item } = right.source else { panic!() };
        assert_eq!(fr.frame_at(s.project.comp(item).unwrap().layers.iter().next().unwrap().in_point), start, "{fps}");
        let div = layer_named(&s, cid, "SceneCut split-handoff Divider");
        assert_eq!(div.props.prop("transform/opacity").unwrap().value.components(), [90.0]);
        assert_eq!(div.props.prop("transform/position").unwrap().keys.len(), 5);
        // The Left copy's slide starts at its half: [−540, 0] from the centre of 384×216 → x 84.
        let left = layer_named(&s, cid, "SceneCut split-handoff Left");
        let x0 = left.props.prop("transform/position").unwrap().keys[0].value.components()[0];
        assert!((x0 - (192.0 - 108.0)).abs() < 1e-6, "{x0}");
    }
    // The old shot must already play when its copy is on screen (1.9 s before the cut).
    let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
    s.execute("layer.timing", json!({"layers": [old], "in": 3.0})).unwrap();
    let before = s.project.clone();
    let e = apply(&mut s, cid, old, new, "split-handoff").unwrap_err().to_string();
    assert!(e.contains("needs to start by then"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // Shots with sound keep it all: nothing is trimmed, so no copy.
    let (mut s, cid, old, new, _) = cut_at_audio(29.97, 10.0, 4.0);
    apply(&mut s, cid, old, new, "split-handoff").unwrap();
    assert!(keeps(&s, cid).is_empty());
}

#[test]
fn keyframe_move_transitions_are_in_the_menus() {
    let json = serde_json::to_string(crate::menus::menu_bar()).unwrap();
    for kind in ["whip-pan", "linear-wipe", "point-iris", "split-handoff"] {
        assert!(json.contains(&format!("{{\"kind\":\"{kind}\"}}")), "{kind} isn't in the menus");
    }
    assert!(!include_str!("../presets/zoom-cut.kinds.json").contains(concat!("/", "Users", "/")));
}

/// Night run 12 Nov 2026: in the app Page Burn left ~40 undo steps (Keep Audio, Layer Comment,
/// Track Matte…) instead of one, and pushed the older steps off the line: it makes more steps
/// than the 32 Undo Levels. Every kind must stay one step whatever the limit.
#[test]
fn every_kind_is_one_undo_step_under_a_small_undo_limit() {
    for kind in ["page-burn", "white-flash", "card-flip-3d", "kick"] {
        let (mut s, cid, old, new, _) = cut_at(29.97, 10.0, 4.0);
        s.prefs.general.undo_levels = 8;
        let before: Vec<String> = s.history.undo.iter().map(|(l, _)| l.clone()).collect();
        let layers = s.project.comp(ItemId(cid)).unwrap().layers.len();
        let r = apply(&mut s, cid, old, new, kind).unwrap();
        let after: Vec<String> = s.history.undo.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(after.len(), before.len() + 1, "{kind}: one undo step, the older ones kept: {after:?}");
        assert_eq!(after[..before.len()], before[..], "{kind}: the older steps");
        assert!(after.last().is_some_and(|l| l.starts_with("Apply Transition")), "{kind}: {after:?} ({})", r["title"]);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), layers, "{kind}: one Undo takes it all back");
        assert!(transition_effects(&s, cid, old).is_empty() && transition_effects(&s, cid, new).is_empty(), "{kind}: undo removes the effects");
    }
}
