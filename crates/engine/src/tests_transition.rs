//! `transition.apply` (Animation ▸ Transitions ▸ Zoom Cut): Elias's 12 zoom cuts from the
//! motion-kit shelf table (ZC1).

use effectcraft_keyframe::Interp;
use effectcraft_project::{ItemId, LayerId, Project};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::Session;
use crate::render::RenderOpts;

const KINDS: [&str; 12] =
    ["kick", "blur-settle", "push-in", "into-window", "twos-card", "twos-push", "zoom-through", "whip", "zoom-out-through", "punch", "flash", "white-flash"];

/// The kinds with a track key at the cut itself (t = 0 in the table).
const CUT_KEY: [&str; 8] = ["kick", "into-window", "zoom-through", "whip", "zoom-out-through", "punch", "flash", "white-flash"];

/// A 384×216 comp of `secs` seconds at `fps` with two full-frame solids meeting at `at` s: the
/// old one (red, below) plays up to it, the new one (blue, above) from it.
/// Returns (session, comp, old, new, the cut in seconds as the layers have it).
fn cut_at(fps: f64, secs: f64, at: f64) -> (Session, u64, u64, u64, f64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cut", "width": 384, "height": 216, "frameRate": fps, "duration": secs})).unwrap();
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
    let (mut s, cid, old, new, cut) = cut_at(24.0, 10.0, 4.0);
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
    let ids: Vec<&str> = kinds.as_array().unwrap().iter().map(|k| k["kind"].as_str().unwrap()).collect();
    assert_eq!(ids, KINDS);
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
