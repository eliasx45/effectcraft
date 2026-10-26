//! The ease taste check (`taste.*`, lint `off-taste`): his approved curves pass at the lengths he
//! uses them, the hard fails, the five criteria, warn-only move types, the one-call fix, ⏳
//! candidates and who may approve them, the graph PNG.

use std::sync::Arc;

use effectcraft_keyframe::{Ease, Eases, Interp};
use effectcraft_project::{ItemId, LayerId};
use serde_json::{Value, json};

use crate::Session;
use crate::config::MemoryConfig;
use crate::taste::{self, MoveType, Side, builtin_profile, measure};

/// A 1920×1080 comp at 30 fps with a solid "Card" at the centre; (session, comp, layer).
fn comp() -> (Session, ItemId, u64) {
    let mut s = Session { config: Some(Arc::new(MemoryConfig::default())), ..Default::default() };
    s.execute("comp.new", json!({"name": "Taste", "width": 1920, "height": 1080, "frameRate": 30, "duration": 10})).unwrap();
    let cid = s.active_comp_id().unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Card", "width": 400, "height": 200, "color": "#236197"})).unwrap()["layer"].as_u64().unwrap();
    (s, cid, l)
}

fn key(s: &mut Session, l: u64, path: &str, t: f64, v: Value) {
    s.execute("prop.addKey", json!({"layer": l, "path": path, "value": v, "time": t, "timeBase": "comp"})).unwrap();
}

/// Puts normalised eases (speed × average speed, influence %) on keys `i` → `i + 1`.
fn ease(s: &mut Session, cid: ItemId, l: u64, path: &str, i: usize, out: (f64, f64), inn: (f64, f64)) {
    let pr = Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(LayerId(l)).unwrap().props.prop_mut(path).unwrap();
    let spatial = pr.spatial;
    let n = if spatial { 1 } else { pr.keys[i].value.components().len() };
    let dt = pr.keys[i + 1].time.seconds() - pr.keys[i].time.seconds();
    let dv: Vec<f64> = if spatial {
        vec![effectcraft_keyframe::spatial_segment_length(&pr.keys, i)]
    } else {
        pr.keys[i].value.components().iter().zip(pr.keys[i + 1].value.components()).map(|(a, b)| b - a).collect()
    };
    let e = |sd: (f64, f64)| Eases::from((0..n).map(|d| Ease { speed: sd.0 * dv[d] / dt, influence: sd.1 / 100.0 }).collect::<Vec<_>>());
    let a = &mut pr.keys[i];
    (a.out_interp, a.auto_bezier, a.continuous) = (Interp::Bezier, false, false);
    a.out_ease = e(out);
    let b = &mut pr.keys[i + 1];
    (b.in_interp, b.auto_bezier, b.continuous) = (Interp::Bezier, false, false);
    b.in_ease = e(inn);
}

fn check(s: &mut Session) -> Value {
    s.execute_checked("taste.check", json!({})).unwrap()
}

fn moves(r: &Value) -> Vec<Value> {
    r["moves"].as_array().cloned().unwrap_or_default()
}

fn only(r: &Value, path: &str) -> Value {
    let m: Vec<Value> = moves(r).into_iter().filter(|m| m["path"] == path).collect();
    assert_eq!(m.len(), 1, "one move on {path}: {r:#}");
    m[0].clone()
}

/// An entrance: the card slides 100 px up from invisible over `frames`, fading in over 8 f.
fn entrance(s: &mut Session, cid: ItemId, l: u64, frames: f64, out: (f64, f64), inn: (f64, f64)) {
    let (t0, t1) = (1.0, 1.0 + frames / 30.0);
    key(s, l, "transform/position", t0, json!([960.0, 640.0]));
    key(s, l, "transform/position", t1, json!([960.0, 540.0]));
    ease(s, cid, l, "transform/position", 0, out, inn);
    key(s, l, "transform/opacity", t0, json!(0.0));
    key(s, l, "transform/opacity", t0 + 8.0 / 30.0, json!(100.0));
    ease(s, cid, l, "transform/opacity", 0, (-0.0073, 34.82), (-0.0134, 95.59));
}

#[test]
fn his_approved_curves_pass_at_the_lengths_he_uses() {
    let p = builtin_profile();
    let r = &p.rules;
    for (id, kind, frames) in [
        ("expo-out-1", MoveType::Entrance, 18.0),
        ("expo-out-1", MoveType::Entrance, 30.0),
        ("fast-in", MoveType::Entrance, 25.0),
        ("quick-launch", MoveType::Entrance, 20.0),
        ("fast-out", MoveType::Exit, 10.0),
        ("smooth-emphasis", MoveType::Move, 20.0),
        ("slow-launch", MoveType::Move, 20.0),
        ("expo-in-out-elias", MoveType::Move, 30.0),
        ("entrance-fade", MoveType::FadeIn, 8.0),
    ] {
        let c = p.find(id).unwrap();
        let (o, i) = c.at(frames / 30.0);
        let sh = measure(o, i, frames);
        assert!(sh.sharpness <= r.sharp_cap(frames), "{id} at {frames} f: {sh:?} cap {}", r.sharp_cap(frames));
        match kind {
            MoveType::Entrance => {
                assert!(sh.peak_pos <= r.entrance_peak_max, "{id}: {sh:?}");
                assert!(sh.tail10 >= r.entrance_tail_min / 100.0 * frames - 0.5, "{id}: {sh:?}");
            }
            MoveType::Exit => assert!(sh.peak_pos >= r.exit_peak_min, "{id}: {sh:?}"),
            MoveType::Move => assert!(sh.peak_pos >= r.move_peak.0 && sh.peak_pos <= r.move_peak.1, "{id}: {sh:?}"),
            MoveType::FadeIn => assert!(sh.peak_pos <= r.fade_in_peak_max, "{id}: {sh:?}"),
            _ => {}
        }
    }
    // Fast In and Expo Out 1 are the same shape; the distance is in percent of the move.
    let d = taste::distance(p.find("fast-in").unwrap().at(1.0), p.find("expo-out-1").unwrap().at(1.0));
    assert!(d < 1.0, "{d}");
    let d = taste::distance((Side { speed: 0.0, influence: 12.0 }, Side { speed: 0.0, influence: 98.0 }), p.find("expo-out-1").unwrap().at(1.0));
    assert!((15.0..30.0).contains(&d), "12/98 vs Expo Out 1: {d}");
}

#[test]
fn an_expo_out_1_entrance_with_its_fade_is_approved() {
    let (mut s, cid, l) = comp();
    entrance(&mut s, cid, l, 18.0, (0.0, 0.1), (0.0, 100.0));
    let r = check(&mut s);
    let pos = only(&r, "transform/position");
    assert_eq!(pos["moveType"], "entrance", "{pos:#}");
    assert_eq!(pos["verdict"], "approved", "{pos:#}");
    assert_eq!(pos["curve"], "Expo Out 1 (Elias)");
    let fade = only(&r, "transform/opacity");
    assert_eq!(fade["moveType"], "fade-in");
    assert_eq!(fade["verdict"], "approved", "{fade:#}");
    assert_eq!(r["clean"], true);
    assert!(r["summary"]["line"].as_str().unwrap().starts_with("✅ 2"), "{r:#}");
}

#[test]
fn linear_and_easy_ease_are_off_and_entrances_block() {
    let (mut s, cid, l) = comp();
    // prop.addKey keys are linear.
    key(&mut s, l, "transform/position", 1.0, json!([960.0, 640.0]));
    key(&mut s, l, "transform/position", 1.6, json!([960.0, 540.0]));
    key(&mut s, l, "transform/opacity", 1.0, json!(0.0));
    key(&mut s, l, "transform/opacity", 1.3, json!(100.0));
    let r = check(&mut s);
    let pos = only(&r, "transform/position");
    assert_eq!(pos["verdict"], "off");
    assert!(pos["problems"][0].as_str().unwrap().starts_with("linear"), "{pos:#}");
    assert_eq!(pos["blocking"], true, "entrances have 3 approved curves: {pos:#}");
    assert_eq!(pos["fix"]["command"], "taste.apply");
    // Fade-ins have one approved curve: a ❌ warns.
    let fade = only(&r, "transform/opacity");
    assert_eq!(fade["verdict"], "off");
    assert_eq!(fade["blocking"], false, "{fade:#}");
    assert_eq!(r["clean"], false);

    ease(&mut s, cid, l, "transform/position", 0, (0.0, 33.333), (0.0, 33.333));
    let pos = only(&check(&mut s), "transform/position");
    assert!(pos["problems"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().starts_with("Easy Ease")), "{pos:#}");
}

#[test]
fn the_criteria_catch_a_late_peak_a_short_landing_and_a_fast_move() {
    let (mut s, cid, l) = comp();
    // Slow launch on an entrance: peaks at 60 %.
    entrance(&mut s, cid, l, 20.0, (0.0, 83.0), (0.0, 60.1));
    let pos = only(&check(&mut s), "transform/position");
    assert_eq!(pos["verdict"], "off");
    assert!(pos["message"].as_str().unwrap().contains("entrances peak in the first 30 %"), "{pos:#}");
    // An entrance that lands early (out 0.1 %, in 20 %).
    ease(&mut s, cid, l, "transform/position", 0, (0.0, 0.1), (0.0, 20.0));
    let pos = only(&check(&mut s), "transform/position");
    assert!(pos["message"].as_str().unwrap().contains("lands too soon"), "{pos:#}");
    // 2060 px in 6 frames with Expo Out 1: too fast.
    let (mut s, cid, l) = comp();
    key(&mut s, l, "transform/position", 1.0, json!([-1100.0, 540.0]));
    key(&mut s, l, "transform/position", 1.2, json!([960.0, 540.0]));
    ease(&mut s, cid, l, "transform/position", 0, (0.0, 0.1), (0.0, 100.0));
    let pos = only(&check(&mut s), "transform/position");
    assert!(pos["message"].as_str().unwrap().contains("too fast"), "{pos:#}");
    assert!(pos["criteria"]["framePerFrame"].as_f64().unwrap() > 18.0, "{pos:#}");
}

#[test]
fn an_unapproved_shape_in_his_range_is_close_and_says_how_it_differs() {
    let (mut s, cid, l) = comp();
    // Devin's Snap in (11/100), not approved by him yet.
    entrance(&mut s, cid, l, 25.0, (0.0, 12.0), (0.0, 98.0));
    let pos = only(&check(&mut s), "transform/position");
    assert_eq!(pos["verdict"], "close", "{pos:#}");
    assert_eq!(pos["mark"], "🟡");
    let msg = pos["message"].as_str().unwrap();
    assert!(msg.contains("launch softer (out 12 % vs"), "{msg}");
    // The fix puts the nearest approved curve on it.
    let fix = pos["fix"].clone();
    let r = s.execute_checked(fix["command"].as_str().unwrap(), fix["params"].clone()).unwrap();
    assert_eq!(r["after"]["verdict"], "approved", "{r:#}");
    assert!(s.execute("edit.undo", json!({})).is_ok());
    assert_eq!(only(&check(&mut s), "transform/position")["verdict"], "close", "one undo step");
}

#[test]
fn a_speed_jump_and_baked_keys_are_off() {
    let (mut s, cid, l) = comp();
    // On → on, continuing at full speed into a second move that leaves at almost nothing.
    key(&mut s, l, "transform/position", 1.0, json!([600.0, 540.0]));
    key(&mut s, l, "transform/position", 2.0, json!([960.0, 540.0]));
    key(&mut s, l, "transform/position", 3.0, json!([1320.0, 540.0]));
    ease(&mut s, cid, l, "transform/position", 0, (0.0, 65.0), (2.0, 30.0));
    ease(&mut s, cid, l, "transform/position", 1, (0.1, 30.0), (0.0, 66.5));
    let r = check(&mut s);
    let second = moves(&r).into_iter().find(|m| m["key"] == 1).unwrap();
    assert!(second["message"].as_str().unwrap().contains("speed jump"), "{second:#}");

    let (mut s, _, l) = comp();
    for f in 0..10 {
        key(&mut s, l, "transform/rotation", 1.0 + f as f64 / 30.0, json!(f as f64 * 4.0));
    }
    let r = check(&mut s);
    let rot: Vec<Value> = moves(&r).into_iter().filter(|m| m["path"] == "transform/rotation").collect();
    assert_eq!(rot.len(), 1, "a baked run is one finding: {r:#}");
    assert!(rot[0]["message"].as_str().unwrap().contains("10 keys one frame apart"), "{rot:#?}");
}

#[test]
fn lint_reports_off_taste_with_the_fix() {
    let (mut s, _, l) = comp();
    key(&mut s, l, "transform/position", 1.0, json!([960.0, 640.0]));
    key(&mut s, l, "transform/position", 1.6, json!([960.0, 540.0]));
    let r = s.execute_checked("render.lint", json!({"start": 0.9, "end": 1.7, "rules": ["off-taste"]})).unwrap();
    let p = &r["problems"][0];
    assert_eq!(p["rule"], "off-taste", "{r:#}");
    assert!(p["message"].as_str().unwrap().contains("Fix: taste.apply"), "{p:#}");
    assert!(r["notes"][0].as_str().unwrap().starts_with("off-taste:"), "{r:#}");
}

#[test]
fn candidates_wait_for_elias_and_agents_cannot_approve() {
    let (mut s, cid, l) = comp();
    entrance(&mut s, cid, l, 25.0, (0.0, 12.0), (0.0, 98.0));
    let r = s.execute_checked("taste.propose", json!({"name": "Soft slide", "layer": l, "path": "transform/position", "key": 0})).unwrap();
    assert_eq!(r["curve"]["status"], "candidate", "{r:#}");
    assert_eq!(r["curve"]["roles"][0], "entrance");
    // Still 🟡: a candidate is not approved.
    assert_eq!(only(&check(&mut s), "transform/position")["verdict"], "close");
    let e = s.execute_checked("taste.approve", json!({"id": "softslide", "said": "ok", "client": "claude"})).unwrap_err();
    assert!(e.to_string().contains("only Elias approves"), "{e}");
    assert!(s.execute_checked("taste.approve", json!({"id": "softslide"})).is_err(), "needs his words");
    let r = s.execute_checked("taste.approve", json!({"id": "softslide", "said": "yes keep that one"})).unwrap();
    assert_eq!(r["curve"]["status"], "approved", "{r:#}");
    let pos = only(&check(&mut s), "transform/position");
    assert_eq!(pos["verdict"], "approved", "{pos:#}");
    assert_eq!(pos["curve"], "Soft slide");
    let prof = s.execute_checked("taste.profile", json!({})).unwrap();
    assert_eq!(prof["stored"], true);
}

#[test]
fn learn_lists_curves_the_profile_does_not_know() {
    let (mut s, cid, l) = comp();
    for (i, t) in [1.0, 3.0, 5.0].into_iter().enumerate() {
        key(&mut s, l, "transform/scale", t, json!([100.0, 100.0]));
        key(&mut s, l, "transform/scale", t + 0.5, json!([130.0, 130.0]));
        ease(&mut s, cid, l, "transform/scale", i * 3, (0.0, 50.0), (0.0, 10.0));
        if i < 2 {
            key(&mut s, l, "transform/scale", t + 1.0, json!([100.0, 100.0]));
        }
    }
    let r = s.execute_checked("taste.learn", json!({"minUses": 2, "save": true})).unwrap();
    let g = &r["groups"][0];
    assert!(g["uses"].as_u64().unwrap() >= 3, "{r:#}");
    assert_eq!(r["saved"].as_array().unwrap().len(), r["groups"].as_array().unwrap().len());
    let prof = s.execute_checked("taste.profile", json!({})).unwrap();
    assert!(prof["curves"].as_array().unwrap().iter().any(|c| c["status"] == "candidate" && c["id"].as_str().unwrap().starts_with("found-")));
}

#[test]
fn graph_writes_a_png_with_the_moves() {
    let (mut s, cid, l) = comp();
    entrance(&mut s, cid, l, 18.0, (0.0, 0.1), (0.0, 100.0));
    let dir = std::env::temp_dir().join(format!("taste-graph-{}", std::process::id()));
    let out = dir.join("g.png");
    let r =
        s.execute_checked("taste.graph", json!({"layer": l, "path": "transform/position", "out": out.to_string_lossy(), "width": 640, "height": 360})).unwrap();
    assert_eq!(r["moves"][0]["verdict"], "approved", "{r:#}");
    let img = image::open(&out).unwrap().to_rgba8();
    assert_eq!((img.width(), img.height()), (640, 360));
    // The approved speed curve is drawn green somewhere.
    assert!(img.pixels().any(|p| p.0[0] == 0x3c && p.0[1] == 0xcf && p.0[2] == 0x91));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(s.execute_checked("taste.graph", json!({"layer": l, "path": "transform/position", "out": "x.jpg"})).is_err());
}

/// Elias's real project, judged per comp (not part of the suite: `TASTE_PROJECT=… cargo test
/// -p effectcraft-engine --lib tests_taste::real_project -- --ignored --nocapture`).
#[test]
#[ignore]
fn real_project() {
    let Ok(path) = std::env::var("TASTE_PROJECT") else { return };
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    let want = std::env::var("TASTE_COMPS").unwrap_or_default();
    let comps: Vec<(ItemId, String)> = s
        .project
        .items
        .iter()
        .filter(|(_, i)| matches!(i.kind, effectcraft_project::ItemKind::Comp(_)))
        .filter(|(_, i)| want.is_empty() || want.split('|').any(|w| i.name == w))
        .map(|(id, i)| (*id, i.name.clone()))
        .collect();
    for (id, name) in comps {
        let r = s.execute_checked("taste.check", json!({"comp": id.0})).unwrap();
        println!("== {name}: {}", r["summary"]["line"]);
        for m in moves(&r) {
            println!(
                "  {} f{}→{} {} {}",
                m["mark"].as_str().unwrap_or(""),
                m["from"]["frame"],
                m["to"]["frame"],
                m["moveType"],
                m["message"].as_str().unwrap_or("")
            );
        }
        if let Ok(dir) = std::env::var("TASTE_GRAPHS") {
            for m in moves(&r).iter().filter(|m| m["verdict"] != "approved").take(3) {
                let out = format!("{dir}/{}-{}-{}.png", id.0, m["layer"], m["key"]);
                let _ = s.execute_checked("taste.graph", json!({"comp": id.0, "layer": m["layer"], "path": m["path"], "out": out}));
            }
        }
    }
}

#[test]
fn moves_without_an_approved_curve_get_no_fix_and_springs_are_left_to_their_expression() {
    let (mut s, cid, l) = comp();
    // A fade-out: no approved fade-out curve yet, so no cross-role fix.
    key(&mut s, l, "transform/opacity", 1.0, json!(100.0));
    key(&mut s, l, "transform/opacity", 1.3, json!(0.0));
    ease(&mut s, cid, l, "transform/opacity", 0, (0.0, 50.0), (0.0, 50.0));
    let fade = only(&check(&mut s), "transform/opacity");
    assert!(fade.get("fix").is_none(), "{fade:#}");
    assert!(fade["message"].as_str().unwrap().contains("No approved fade-out curve yet"), "{fade:#}");
    // A linear scale kick under a spring expression.
    key(&mut s, l, "transform/scale", 2.0, json!([0.0, 0.0]));
    key(&mut s, l, "transform/scale", 2.25, json!([100.0, 100.0]));
    s.execute_checked("prop.setExpression", json!({"layer": l, "path": "transform/scale", "expression": "// Motion Library spring v3\nvalue"})).unwrap();
    let pop = only(&check(&mut s), "transform/scale");
    assert_eq!(pop["verdict"], "close", "{pop:#}");
    assert!(pop["message"].as_str().unwrap().contains("expression drives"), "{pop:#}");
}

#[test]
fn rig_entrances_scrubs_and_his_fast_exits_read_as_he_animates() {
    // HT3 (A/B item 4): his finished 20-08 moves came out ❌.
    let (mut s, cid, card) = comp();
    let p = builtin_profile();
    let side = |c: &str| p.find(c).map(|c| ((c.out.speed, c.out.influence), (c.inn.speed, c.inn.influence))).unwrap();
    // A rig null in the frame carries a card that starts 1000 px below it, off the frame: the
    // rig's slide up is an entrance (it was an on-screen move).
    s.execute_checked("prop.set", json!({"layer": card, "path": "transform/position", "value": [960.0, 1540.0]})).unwrap();
    let rig = s.execute_checked("layer.newNull", json!({"name": "Rig"})).unwrap()["layer"].as_u64().unwrap();
    s.execute_checked("prop.set", json!({"layer": rig, "path": "transform/position", "value": [960.0, 540.0]})).unwrap();
    s.execute_checked("layer.setParent", json!({"layers": [card], "parent": rig, "compensate": true})).unwrap();
    key(&mut s, rig, "transform/position", 1.0, json!([960.0, 540.0]));
    key(&mut s, rig, "transform/position", 1.8, json!([960.0, -460.0]));
    let (o, i) = side("expo-out-1");
    ease(&mut s, cid, rig, "transform/position", 0, o, i);
    // A bar filling from its left edge: X scale only, anchor on the left edge.
    let bar = s.execute("layer.newSolid", json!({"name": "Bar", "width": 400, "height": 20, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap();
    s.execute_checked("prop.set", json!({"layer": bar, "path": "transform/anchor", "value": [0.0, 10.0]})).unwrap();
    key(&mut s, bar, "transform/scale", 2.0, json!([0.0, 100.0]));
    key(&mut s, bar, "transform/scale", 2.8, json!([100.0, 100.0]));
    let (o, i) = side("expo-in-out-elias");
    ease(&mut s, cid, bar, "transform/scale", 0, o, i);
    // The same X-only scale from the centre is still an entrance.
    let mid = s.execute("layer.newSolid", json!({"name": "Mid", "width": 400, "height": 20, "color": "#00ff00"})).unwrap()["layer"].as_u64().unwrap();
    key(&mut s, mid, "transform/scale", 2.0, json!([0.0, 100.0]));
    key(&mut s, mid, "transform/scale", 2.8, json!([100.0, 100.0]));
    // A row leaving right over 10 f with his Fast out: about 20 % of the frame per frame.
    let row = s.execute("layer.newSolid", json!({"name": "Row", "width": 100, "height": 100, "color": "#0000ff"})).unwrap()["layer"].as_u64().unwrap();
    key(&mut s, row, "transform/position", 3.0, json!([1100.0, 540.0]));
    key(&mut s, row, "transform/position", 3.0 + 10.0 / 30.0, json!([1990.0, 540.0]));
    let (o, i) = side("fast-out");
    ease(&mut s, cid, row, "transform/position", 0, o, i);

    let r = check(&mut s);
    let of = |l: u64| moves(&r).into_iter().find(|m| m["layer"] == l).unwrap_or_else(|| panic!("no move on {l}: {r:#}"));
    let m = of(rig);
    assert_eq!(m["moveType"], "entrance", "{m:#}");
    assert_eq!(m["verdict"], "approved", "{m:#}");
    let m = of(bar);
    assert_eq!(m["moveType"], "scrub", "{m:#}");
    assert_ne!(m["verdict"], "off", "{m:#}");
    assert_eq!(of(mid)["moveType"], "entrance", "{r:#}");
    let m = of(row);
    assert_eq!(m["moveType"], "exit", "{m:#}");
    let pct = m["criteria"]["framePerFrame"].as_f64().unwrap();
    assert!(pct > 18.0 && pct < p.rules.exit_speed_cap, "{m:#}");
    assert_eq!(m["verdict"], "approved", "{m:#}");
}

#[test]
fn align_peak_keeps_an_approved_curve() {
    // HT4 (A/B r2 #1): keys.alignPeak's auto-picked graph turned ✅ rig entrances into ❌.
    for (mode, frame) in [("shift", 40), ("stretch-out", 33)] {
        let (mut s, cid, l) = comp();
        key(&mut s, l, "transform/position", 1.0, json!([960.0, 1300.0]));
        key(&mut s, l, "transform/position", 1.6, json!([960.0, 540.0]));
        ease(&mut s, cid, l, "transform/position", 0, (0.0, 0.1), (0.0, 100.0));
        let before = only(&check(&mut s), "transform/position");
        assert_eq!((before["moveType"].as_str(), before["verdict"].as_str()), (Some("entrance"), Some("approved")), "{before:#}");
        let a = s.execute_checked("keys.alignPeak", json!({"layer": l, "path": "transform/position", "frame": frame, "mode": mode})).unwrap();
        let res = &a["results"][0];
        assert_eq!(res["graph"], "keep", "{mode}: {a:#}");
        assert!(res["offsetFrames"].as_f64().unwrap().abs() <= 0.5, "{mode}: {a:#}");
        let after = only(&check(&mut s), "transform/position");
        assert_eq!(after["verdict"], "approved", "{mode}: {after:#}");
        assert_eq!(after["curve"], before["curve"], "{mode}: {after:#}");
        assert_ne!(after["from"], before["from"], "{mode}: the keys moved: {a:#}");
    }
}
