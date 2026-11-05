//! `clip.move` (Animation ▸ Clip Moves ▸ …): Elias's 7 one-shot moves on one footage layer (CM1),
//! from the motion-kit shelf table `clip-move.kinds.json`.

use effectcraft_keyframe::Interp;
use effectcraft_project::{Footage, FootageKind, ItemId, ItemKind, Layer, LayerId, LayerSource, Project, Value as KV};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::Session;

const KINDS: [&str; 7] = ["punch", "zoom-ladder", "chain", "crop-reframe", "retime", "speed-ramp", "freeze"];
const FRAMING: [&str; 3] = ["punch", "zoom-ladder", "chain"];
const SPEED: [&str; 3] = ["retime", "speed-ramp", "freeze"];

/// A 384×216 comp of 10 s at `fps` with one 384×216 video layer “A Footage” (`src` s of footage,
/// with sound when `audio`), playing 0–`out` s. Returns (session, comp, layer).
fn shot(fps: f64, src: f64, out: f64, audio: bool) -> (Session, u64, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Shot", "width": 384, "height": 216, "frameRate": fps, "duration": 10.0})).unwrap();
    let cid = s.active_comp_id().unwrap().0;
    let f = Footage {
        path: "/nowhere/A Footage.mov".into(),
        kind: FootageKind::Video,
        width: 384,
        height: 216,
        frame_rate: FrameRate { num: 30, den: 1 },
        duration: Tick::from_seconds_f64(src),
        has_video: true,
        has_audio: audio,
        ..Default::default()
    };
    let item = std::sync::Arc::make_mut(&mut s.project).add_item("A Footage", Default::default(), None, ItemKind::Footage(f)).0;
    let l = s.execute("layer.addItem", json!({"comp": cid, "item": item})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.rename", json!({"layers": [l], "name": "A Footage"})).unwrap();
    s.execute("layer.timing", json!({"layers": [l], "in": 0.0, "out": out})).unwrap();
    (s, cid, l)
}

fn mv(s: &mut Session, cid: u64, l: u64, kind: &str, at: f64) -> crate::Result<Value> {
    s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": kind, "at": at}))
}

fn layer(s: &Session, cid: u64, l: u64) -> Layer {
    s.project.comp(ItemId(cid)).unwrap().layer(LayerId(l)).unwrap().clone()
}

fn layer_named(s: &Session, cid: u64, name: &str) -> Option<Layer> {
    s.project.comp(ItemId(cid)).unwrap().layers.iter().find(|l| l.name == name).cloned()
}

/// The shot's `ClipMove …` effect / mask names.
fn ours(l: &Layer) -> Vec<String> {
    let fx = l.effects().map(|g| g.groups().map(|g| g.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let ms = l.masks().map(|g| g.groups().map(|g| g.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
    fx.into_iter().chain(ms).filter(|n| n.starts_with("ClipMove ")).collect()
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

/// The kind's move times (s after `at`) from the shipped table.
fn table_times(kind: &str) -> Vec<f64> {
    let t: Value = serde_json::from_str(include_str!("../presets/clip-move.kinds.json")).unwrap();
    let k = &t["kinds"][kind];
    let track = k.get("frame").or_else(|| k.get("mask")).or_else(|| k.get("remap")).unwrap();
    track["t"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect()
}

fn frames_of(fr: FrameRate, at_f: i64, ts: &[f64]) -> Vec<i64> {
    ts.iter().map(|t| at_f + (t * fr.as_f64()).round() as i64).collect()
}

/// The keyed property a kind moves on the shot.
fn moved_prop(l: &Layer, kind: &str) -> effectcraft_project::Property {
    if FRAMING.contains(&kind) {
        l.effects().unwrap().groups().find(|g| g.name == format!("ClipMove {kind} Frame")).unwrap().prop("position").unwrap().clone()
    } else if kind == "crop-reframe" {
        l.masks().unwrap().groups().find(|g| g.name == "ClipMove crop-reframe Crop").unwrap().prop("path").unwrap().clone()
    } else {
        l.props.get("timeRemap").unwrap().clone()
    }
}

#[test]
fn every_kind_applies_at_29_97_and_24_fps() {
    for fps in [29.97, 24.0] {
        for kind in KINDS {
            let (mut s, cid, l) = shot(fps, 30.0, 9.0, true);
            let before = layer(&s, cid, l);
            let items0 = s.project.items.len();
            let undo0 = s.history.undo.len();
            let r = mv(&mut s, cid, l, kind, 1.0).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            assert_eq!((r["kind"].as_str(), r["layer"].as_u64()), (Some(kind), Some(l)), "{r}");
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let at_f = r["atFrame"].as_i64().unwrap();
            assert_eq!(fr.tick_of(at_f), fr.snap_nearest(Tick::from_seconds_f64(1.0)), "{kind} at {fps}: at on a frame");
            // Every key, in every comp, on a frame of its comp.
            for (kfr, t, what) in &all_keys(&s.project) {
                assert_eq!(kfr.snap_nearest(*t), *t, "{kind} at {fps}: {what} key at {} s is between frames", t.seconds());
            }
            // The move's keys sit on at + the table's times (comp frames).
            let now = layer(&s, cid, l);
            let pr = moved_prop(&now, kind);
            let want = frames_of(fr, at_f, &table_times(kind));
            let got: Vec<i64> = pr.keys.iter().map(|k| fr.frame_at(now.comp_time(k.time))).collect();
            assert!(want.iter().all(|f| got.contains(f)), "{kind} at {fps}: keys {got:?}, want {want:?}");
            // His transform, in / out points, parent and place in the stack are never touched.
            assert_eq!(now.transform(), before.transform(), "{kind} at {fps}: transform changed");
            assert_eq!(
                (now.in_point, now.out_point, now.start_time, now.stretch, now.parent),
                (before.in_point, before.out_point, before.start_time, before.stretch, before.parent),
                "{kind}"
            );
            assert_eq!(s.project.comp(ItemId(cid)).unwrap().index_of(LayerId(l)), Some(0), "{kind}: still on top");
            // Names and tags.
            for n in ours(&now) {
                assert!(n.starts_with(&format!("ClipMove {kind} ")), "{kind}: “{n}”");
            }
            let comp = s.project.comp(ItemId(cid)).unwrap();
            for o in comp.layers.iter().filter(|o| o.id != LayerId(l)) {
                assert!(o.comment.starts_with(&format!("ClipMove {l} · ")), "{kind}: “{}” comment “{}”", o.name, o.comment);
            }
            assert!(now.markers.iter().all(|m| m.comment.starts_with(&format!("ClipMove {kind} "))), "{kind}: {:?}", now.markers);
            // One hit per beat, on at + its time.
            let t: Value = serde_json::from_str(include_str!("../presets/clip-move.kinds.json")).unwrap();
            assert_eq!(r["hits"].as_array().unwrap().len(), t["kinds"][kind]["beats"].as_array().unwrap().len(), "{kind}");
            for h in r["hits"].as_array().unwrap() {
                assert!(h["id"].as_str().unwrap().starts_with("clip-move-"), "{h}");
            }
            // One undo step takes it all back.
            assert_eq!(s.history.undo.len(), undo0 + 1, "{kind} at {fps}: one undo step");
            s.execute("edit.undo", json!({})).unwrap();
            let back = layer(&s, cid, l);
            assert!(ours(&back).is_empty() && back.markers.is_empty() && back.props.get("timeRemap").is_none(), "{kind}: undo");
            assert!(back.switches.audio, "{kind}: undo turns the audio back on");
            assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 1, "{kind}: undo removes the added layers");
            assert_eq!(s.project.items.len(), items0, "{kind}: undo removes the added items");
        }
    }
}

#[test]
fn punch_frames_the_subject_and_never_shows_an_edge() {
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    let r = mv(&mut s, cid, l, "punch", 1.0).unwrap();
    let now = layer(&s, cid, l);
    let g = now.effects().unwrap().groups().find(|g| g.name == "ClipMove punch Frame").unwrap().clone();
    // Its own shutter off, anchor [0, 0].
    assert_eq!(g.prop("useCompositionShutterAngle").unwrap().value, KV::Bool(false));
    assert_eq!(g.prop("shutterAngle").unwrap().value.components(), [0.0]);
    assert_eq!(g.prop("anchor").unwrap().value.components()[..2], [0.0, 0.0]);
    let sc: Vec<f64> = g.prop("scaleHeight").unwrap().keys.iter().map(|k| k.value.components()[0]).collect();
    assert_eq!(sc, [100.0, 130.0, 100.0, 100.0, 130.0]);
    let pos = g.prop("position").unwrap();
    // Default centre [960, 540] = the built face [960, 440] moved down 100 px: 130 % on (960, 540).
    let p1 = pos.keys[1].value.components();
    assert!((p1[0] + 57.6).abs() < 1e-6 && (p1[1] + 32.4).abs() < 1e-6, "{p1:?}");
    // Hard punches are hold keys; the smooth one is a Graph; the path is straight.
    assert_eq!((pos.keys[0].out_interp, pos.keys[1].out_interp, pos.keys[2].out_interp), (Interp::Hold, Interp::Hold, Interp::Hold));
    assert_eq!((pos.keys[3].out_interp, pos.keys[4].in_interp), (Interp::Bezier, Interp::Bezier));
    assert!(pos.keys.iter().all(|k| !k.spatial_auto && k.spatial_in == [0.0; 3] && k.spatial_out == [0.0; 3]));
    assert_eq!(r["layers"]["frame"]["scale"], json!([100.0, 130.0, 100.0, 100.0, 130.0]));
    // Any centre, any kind: every framing stays inside the frame (P in [W (1 − m), 0]).
    for kind in FRAMING {
        for c in [[0.0, 0.0], [1920.0, 1080.0], [960.0, 440.0], [1500.0, 200.0]] {
            let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
            s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": kind, "at": 0.5, "centre": c})).unwrap();
            let g = layer(&s, cid, l).effects().unwrap().groups().find(|g| g.name == format!("ClipMove {kind} Frame")).unwrap().clone();
            let sc = g.prop("scaleHeight").unwrap().keys.clone();
            for (k, sk) in g.prop("position").unwrap().keys.iter().zip(&sc) {
                let (p, m) = (k.value.components(), sk.value.components()[0] / 100.0);
                assert!(p[0] <= 1e-9 && p[0] >= 384.0 * (1.0 - m) - 1e-9, "{kind} {c:?}: x {} at {m}", p[0]);
                assert!(p[1] <= 1e-9 && p[1] >= 216.0 * (1.0 - m) - 1e-9, "{kind} {c:?}: y {} at {m}", p[1]);
            }
        }
    }
    // A corner subject is clamped: 130 % on (0, 0) shows the top-left corner, never past it.
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "punch", "at": 1.0, "centre": [0, 0]})).unwrap();
    let g = layer(&s, cid, l).effects().unwrap().groups().find(|g| g.name == "ClipMove punch Frame").unwrap().clone();
    assert_eq!(g.prop("position").unwrap().keys[1].value.components()[..2], [0.0, 0.0]);
}

#[test]
fn chain_marks_each_beat_on_the_shot() {
    for fps in [29.97, 24.0] {
        let (mut s, cid, l) = shot(fps, 30.0, 9.0, false);
        let r = mv(&mut s, cid, l, "chain", 1.0).unwrap();
        let now = layer(&s, cid, l);
        let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
        let at_f = r["atFrame"].as_i64().unwrap();
        let comments: Vec<&str> = now.markers.iter().map(|m| m.comment.as_str()).collect();
        assert_eq!(
            comments,
            ["ClipMove chain B1 · push in", "ClipMove chain B2 · pan right", "ClipMove chain B3 · pan left + tilt down", "ClipMove chain B4 · pull back"]
        );
        let frames: Vec<i64> = now.markers.iter().map(|m| fr.frame_at(now.comp_time(m.time))).collect();
        assert_eq!(frames, frames_of(fr, at_f, &[0.4004, 1.5015, 2.6026, 3.7037]), "{fps}");
        let ids: Vec<&str> = r["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["clip-move-b1", "clip-move-b2", "clip-move-b3", "clip-move-b4", "clip-move-land"]);
    }
}

#[test]
fn crop_reframe_masks_inside_the_frame_over_a_dark_backing() {
    let rect = |l: &Layer, i: usize| -> Vec<[f64; 2]> {
        let mp = l.masks().unwrap().groups().find(|g| g.name == "ClipMove crop-reframe Crop").unwrap().prop("path").unwrap().clone();
        let KV::Path(p) = &mp.keys[i].value else { panic!("a path key") };
        p.vertices.clone()
    };
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    let r = mv(&mut s, cid, l, "crop-reframe", 1.0).unwrap();
    let now = layer(&s, cid, l);
    // The first key: the full frame; the last: the square on the subject (built on y 440, the
    // default centre 100 px lower), ×0.2.
    assert_eq!(rect(&now, 0), [[0.0, 0.0], [384.0, 0.0], [384.0, 216.0], [0.0, 216.0]]);
    let last = rect(&now, 4);
    assert!(
        (last[0][0] - 132.0).abs() < 1e-9 && (last[0][1] - 48.0).abs() < 1e-9 && (last[2][0] - 252.0).abs() < 1e-9 && (last[2][1] - 168.0).abs() < 1e-9,
        "{last:?}"
    );
    // The backing: right below the shot, from `at` to its out point, the table's #08080b, tagged.
    let comp = s.project.comp(ItemId(cid)).unwrap();
    let bg = layer_named(&s, cid, "ClipMove crop-reframe BG").unwrap();
    assert_eq!(comp.index_of(bg.id), Some(1), "right below the shot");
    assert_eq!(r["layers"]["crop"]["bg"], json!(bg.id.0));
    assert!((bg.in_point.seconds() - 1.0).abs() < 0.02 && bg.out_point == now.out_point, "{} {}", bg.in_point.seconds(), bg.out_point.seconds());
    assert_eq!(bg.comment, format!("ClipMove {l} · bg"));
    let LayerSource::Solid { item } = bg.source else { panic!("a solid") };
    let ItemKind::Solid(sol) = &s.project.item(item).unwrap().kind else { panic!("a solid item") };
    assert!(sol.color[0] < 0.05 && sol.color[2] < 0.06, "{:?}", sol.color);
    // A subject at the bottom: the square is clamped inside the frame.
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "crop-reframe", "at": 1.0, "centre": [960, 1080]})).unwrap();
    let last = rect(&layer(&s, cid, l), 4);
    assert!((last[0][1] - 96.0).abs() < 1e-9 && (last[2][1] - 216.0).abs() < 1e-9, "{last:?}");
    // bg: false → no backing; a colour → that colour; anything else is an error.
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "crop-reframe", "at": 1.0, "bg": false})).unwrap();
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 1);
    s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "crop-reframe", "at": 1.0, "bg": "#ff0000"})).unwrap();
    let bg = layer_named(&s, cid, "ClipMove crop-reframe BG").unwrap();
    let LayerSource::Solid { item } = bg.source else { panic!("a solid") };
    let ItemKind::Solid(sol) = &s.project.item(item).unwrap().kind else { panic!("a solid item") };
    assert!(sol.color[0] > 0.99 && sol.color[1] < 0.01, "{:?}", sol.color);
    assert!(s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "crop-reframe", "at": 1.0, "bg": "red"})).is_err());
}

/// The audio keep copies in the comp: (id, in, out, video, audio, index, comment).
fn keeps(s: &Session, cid: u64) -> Vec<(u64, f64, f64, bool, bool, usize, String)> {
    let comp = s.project.comp(ItemId(cid)).unwrap();
    comp.layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.name.ends_with(" audio (ClipMove keep)"))
        .map(|(i, l)| (l.id.0, l.in_point.seconds(), l.out_point.seconds(), l.switches.video, l.switches.audio, i, l.comment.clone()))
        .collect()
}

#[test]
fn speed_kinds_retime_the_picture_and_keep_the_sound() {
    for fps in [29.97, 24.0] {
        for kind in SPEED {
            let (mut s, cid, l) = shot(fps, 30.0, 9.0, true);
            let before = layer(&s, cid, l);
            let r = mv(&mut s, cid, l, kind, 1.0).unwrap_or_else(|e| panic!("{kind} at {fps}: {e}"));
            let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
            let at_f = r["atFrame"].as_i64().unwrap();
            let now = layer(&s, cid, l);
            // The picture: 1× up to `at` (a key at the in point), the move, 1× after it.
            let rm = now.props.get("timeRemap").unwrap();
            let first = rm.keys.first().unwrap();
            assert_eq!(now.comp_time(first.time), now.in_point, "{kind}");
            assert!((first.value.components()[0] - now.layer_time(now.in_point).seconds()).abs() < 1e-9, "{kind}");
            let at_key = rm.keys.iter().find(|k| fr.frame_at(now.comp_time(k.time)) == at_f).unwrap();
            assert!((at_key.value.components()[0] - now.layer_time(fr.tick_of(at_f)).seconds()).abs() < 1e-9, "{kind}: 1× up to at");
            let last = rm.keys.last().unwrap();
            assert_eq!(now.comp_time(last.time), now.out_point, "{kind}: a key at the out point");
            let vals = r["layers"]["remap"]["values"].as_array().unwrap();
            let end = fr.tick_of(*frames_of(fr, at_f, &table_times(kind)).last().unwrap()).seconds();
            let want_out = vals.last().unwrap().as_f64().unwrap() + (now.out_point.seconds() - end);
            assert!((last.value.components()[0] - want_out).abs() < 1e-9, "{kind}: 1× after the move");
            // The sound: an audio-only copy of the shot as it was (no remap), right below it; the
            // shot's own audio off.
            let k = keeps(&s, cid);
            assert_eq!(k.len(), 1, "{kind}: {k:?}");
            let (kid, a, b, video, audio, i, comment) = k[0].clone();
            assert!((a - before.in_point.seconds()).abs() < 1e-9 && (b - before.out_point.seconds()).abs() < 1e-9, "{kind}: the whole span");
            assert!(!video && audio && i == 1, "{kind}: audio only, right below");
            assert_eq!(comment, format!("ClipMove {l} · audio keep {l} · muted"));
            let kl = layer(&s, cid, kid);
            assert!(kl.props.get("timeRemap").is_none() && kl.start_time == before.start_time && kl.source == before.source, "{kind}: the same source time");
            assert!(!now.switches.audio, "{kind}: the shot's audio is off");
            // The reply: the retimed span and the copy.
            let m = r["moved"].as_array().unwrap();
            assert_eq!(m.len(), 2, "{kind}: {m:?}");
            assert_eq!((m[0]["what"].as_str(), m[0]["fromFrame"].as_i64()), (Some("time-remap"), Some(at_f)));
            assert_eq!((m[1]["what"].as_str(), m[1]["layer"].as_u64(), m[1]["shotAudio"].as_str()), (Some("audio-keep"), Some(kid), Some("off")));
            assert!(
                now.markers.iter().any(|mk| mk.comment == format!("ClipMove {kind} · time remap") && fr.frame_at(now.comp_time(mk.time)) == at_f),
                "{kind}"
            );
        }
    }
    // freeze holds one frame for 1.5 s; speed-ramp's keys carry their speeds.
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    let r = mv(&mut s, cid, l, "freeze", 1.0).unwrap();
    let v: Vec<f64> = r["layers"]["remap"]["values"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
    assert!((v[1] - v[2]).abs() < 1e-12 && v[1] > v[0], "{v:?}");
    let rm = layer(&s, cid, l).props.get("timeRemap").unwrap().clone();
    assert!(rm.keys.iter().any(|k| k.out_interp == Interp::Hold), "a hold key");
    assert!(keeps(&s, cid).is_empty(), "no sound: no copy");
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    mv(&mut s, cid, l, "speed-ramp", 1.0).unwrap();
    let rm = layer(&s, cid, l).props.get("timeRemap").unwrap().clone();
    let speeds: Vec<f64> = rm.keys.iter().filter(|k| k.in_interp == Interp::Bezier).map(|k| k.in_ease[0].speed).collect();
    assert_eq!(speeds, [1.0, 1.0, 0.25, 0.25, 2.5, 2.5, 1.0, 1.0]);
    assert!(rm.keys.iter().filter(|k| k.in_interp == Interp::Bezier).all(|k| (k.out_ease[0].influence - 0.33333).abs() < 1e-6));
}

#[test]
fn speed_kinds_refuse_his_own_remap_a_stretch_or_too_little_footage() {
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, true);
    s.execute("layer.enableTimeRemap", json!({"layers": [l]})).unwrap();
    let before = s.project.clone();
    let e = mv(&mut s, cid, l, "retime", 1.0).unwrap_err().to_string();
    assert!(e.contains("time remapping of its own"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // A framing kind on it is fine, and leaves his remap alone.
    mv(&mut s, cid, l, "punch", 1.0).unwrap();
    assert!(layer(&s, cid, l).props.get("timeRemap").is_some());
    // A time stretch.
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, true);
    s.execute("layer.timeStretch", json!({"layers": [l], "percent": 200})).unwrap();
    let e = mv(&mut s, cid, l, "freeze", 0.5).unwrap_err().to_string();
    assert!(e.contains("time stretch"), "{e}");
    // Footage that ends before the picture would: retime's picture runs 1 s ahead.
    let (mut s, cid, l) = shot(29.97, 5.5, 5.5, true);
    let before = s.project.clone();
    let e = mv(&mut s, cid, l, "retime", 0.0).unwrap_err().to_string();
    assert!(e.contains("footage") && e.contains("earlier"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    // A solid can't be remapped.
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "S", "width": 384, "height": 216, "frameRate": 29.97, "duration": 10.0})).unwrap();
    let sl = s.execute("layer.newSolid", json!({"name": "Card", "color": "#202020"})).unwrap()["layer"].as_u64().unwrap();
    let cid = s.active_comp_id().unwrap().0;
    let e = mv(&mut s, cid, sl, "freeze", 1.0).unwrap_err().to_string();
    assert!(e.contains("footage or composition"), "{e}");
}

#[test]
fn bad_requests_are_errors_and_change_nothing() {
    let (mut s, cid, l) = shot(29.97, 30.0, 4.0, false);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let e = mv(&mut s, cid, l, "zoom-out", 1.0).unwrap_err().to_string();
    assert!(e.contains("unknown kind") && e.contains("zoom-ladder"), "{e}");
    // The layer doesn't play over the whole move (chain runs 4.5 s).
    let e = mv(&mut s, cid, l, "chain", 1.0).unwrap_err().to_string();
    assert!(e.contains("play over the whole move"), "{e}");
    assert!(s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "punch", "at": 0.0, "centre": "face"})).is_err());
    assert!(s.execute("clip.move", json!({"comp": cid, "layer": l, "kind": "punch", "at": "soon"})).is_err());
    assert!(s.execute("clip.move", json!({"comp": cid, "layer": 999, "kind": "punch", "at": 0.0})).is_err());
    assert!(std::sync::Arc::ptr_eq(&before, &s.project), "nothing changed");
    assert_eq!(s.history.undo.len(), undo);
}

#[test]
fn calling_again_replaces_the_previous_clip_move() {
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, true);
    let items0 = s.project.items.len();
    // punch → chain: only the chain's effect and markers.
    mv(&mut s, cid, l, "punch", 1.0).unwrap();
    let r = mv(&mut s, cid, l, "chain", 1.0).unwrap();
    assert_eq!(ours(&layer(&s, cid, l)), ["ClipMove chain Frame"]);
    assert_eq!(r["removed"]["effects"], json!(1));
    assert_eq!(layer(&s, cid, l).markers.len(), 4);
    // → crop-reframe: the frame effect and the chain's markers go; its mask and backing come.
    let r = mv(&mut s, cid, l, "crop-reframe", 1.0).unwrap();
    assert_eq!((r["removed"]["effects"].as_u64(), r["removed"]["markers"].as_u64()), (Some(1), Some(4)));
    assert_eq!(ours(&layer(&s, cid, l)), ["ClipMove crop-reframe Crop"]);
    assert!(layer(&s, cid, l).markers.is_empty());
    // → freeze: the mask, the backing and its solid go (no orphan); time remap + an audio copy come.
    let r = mv(&mut s, cid, l, "freeze", 1.0).unwrap();
    assert_eq!((r["removed"]["masks"].as_u64(), r["removed"]["layers"].as_u64(), r["removed"]["items"].as_u64()), (Some(1), Some(1), Some(1)));
    assert!(layer_named(&s, cid, "ClipMove crop-reframe BG").is_none());
    assert_eq!(s.project.items.len(), items0, "no orphan solid");
    assert_eq!(keeps(&s, cid).len(), 1);
    // → retime: one copy (not two), one time remap of the ClipMove's own.
    mv(&mut s, cid, l, "retime", 1.0).unwrap();
    assert_eq!(keeps(&s, cid).len(), 1, "no stacking");
    assert!(!layer(&s, cid, l).switches.audio);
    assert_eq!(layer(&s, cid, l).markers.iter().filter(|m| m.comment.ends_with(" · time remap")).count(), 1);
    // → punch: the copy and the remap go, the shot's audio is back on.
    let r = mv(&mut s, cid, l, "punch", 1.0).unwrap();
    assert_eq!(r["removed"]["layers"], json!(1));
    let now = layer(&s, cid, l);
    assert!(keeps(&s, cid).is_empty() && now.switches.audio && now.props.get("timeRemap").is_none(), "{:?}", keeps(&s, cid));
    assert_eq!(s.project.comp(ItemId(cid)).unwrap().layers.len(), 1);
    // Undo steps back one move at a time.
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(keeps(&s, cid).len(), 1);
    assert!(layer(&s, cid, l).props.get("timeRemap").is_some());
}

#[test]
fn the_selection_and_the_playhead_pick_the_layer_and_at() {
    let (mut s, cid, l) = shot(29.97, 30.0, 9.0, false);
    s.state.selected_layers.clear();
    assert!(!s.is_enabled("clip.move"), "nothing selected");
    s.execute("layer.select", json!({"layers": [l]})).unwrap();
    s.execute("time.set", json!({"time": 2.0})).unwrap();
    assert!(s.is_enabled("clip.move"));
    let r = s.execute("clip.move", json!({"kind": "zoom-ladder"})).unwrap();
    assert_eq!(r["layer"].as_u64(), Some(l));
    let fr = s.project.comp(ItemId(cid)).unwrap().frame_rate;
    assert_eq!(fr.tick_of(r["atFrame"].as_i64().unwrap()), fr.snap_nearest(Tick::from_seconds_f64(2.0)));
    // The default kind is punch.
    let r = s.execute("clip.move", json!({})).unwrap();
    assert_eq!(r["kind"], json!("punch"));
}

#[test]
fn clip_move_menu_lists_the_7_kinds() {
    let mut s = Session::default();
    let kinds = s.execute("clip.moveKinds", json!({})).unwrap();
    let ids: Vec<&str> = kinds.as_array().unwrap().iter().map(|k| k["kind"].as_str().unwrap()).collect();
    assert_eq!(ids, KINDS);
    assert_eq!(kinds[0]["default"], json!(true));
    let json = serde_json::to_string(crate::menus::menu_bar()).unwrap();
    assert!(json.contains("\"label\":\"Clip Moves\""), "no Clip Moves submenu");
    for kind in KINDS {
        assert!(json.contains(&format!("{{\"kind\":\"{kind}\"}}")), "{kind} isn't in the menus");
    }
    let spec = crate::commands::find("clip.move").unwrap();
    assert_eq!(spec.menu, ["Animation", "Clip Moves"]);
    // The shipped table is the shelf's copy with labels instead of local paths.
    assert!(!include_str!("../presets/clip-move.kinds.json").contains(concat!("/", "Users", "/")));
}
