//! SFX1: `sfx.hits` (the animation hits the editing-sfx live map lands sounds on) and
//! `sfx.hitsRevision`.

use std::sync::Arc;

use effectcraft_project::{ItemId, LayerId};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

/// A session with the real expression engine.
fn session() -> Session {
    Session { expr: Some(Arc::new(effectcraft_expr::Expressions)), ..Session::default() }
}

/// A 640×360 comp `name`, 25 fps, `dur` s.
fn comp(s: &mut Session, name: &str, dur: f64) -> ItemId {
    s.execute("comp.new", json!({"name": name, "width": 640, "height": 360, "frameRate": 25, "duration": dur})).unwrap();
    s.active_comp_id().unwrap()
}

fn solid(s: &mut Session, name: &str) -> u64 {
    s.execute("layer.newSolid", json!({"name": name, "width": 100, "height": 60, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap()
}

/// Two keys on `path` at comp times `a` / `b`, Easy Ease unless `linear`.
fn keys(s: &mut Session, l: u64, path: &str, (a, va): (f64, Value), (b, vb): (f64, Value), linear: bool) {
    s.execute("prop.addKey", json!({"layer": l, "path": path, "value": va, "time": a, "timeBase": "comp"})).unwrap();
    s.execute("prop.addKey", json!({"layer": l, "path": path, "value": vb, "time": b, "timeBase": "comp"})).unwrap();
    if !linear {
        let uid = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop(path).unwrap().uid;
        let ks: Vec<Value> = [a, b].iter().map(|t| json!({"layer": l, "prop": uid, "time": t})).collect();
        s.execute_checked("keys.select", json!({"keys": ks})).unwrap();
        s.execute_checked("keys.easyEase", json!({})).unwrap();
    }
}

fn hits(s: &mut Session) -> Value {
    s.execute("sfx.hits", json!({})).unwrap()
}

fn list(r: &Value) -> Vec<Value> {
    r["hits"].as_array().cloned().unwrap_or_default()
}

fn near(v: &Value, want: f64, tol: f64) -> bool {
    v.as_f64().is_some_and(|x| (x - want).abs() <= tol)
}

/// `inner` placed in a new 25 fps comp `name` (12 s) at `start`, trimmed to `in_out`: (the new
/// comp, the precomp layer).
fn nest(s: &mut Session, inner: ItemId, name: &str, start: f64, in_out: (f64, f64)) -> (ItemId, u64) {
    let outer = comp(s, name, 12.0);
    let l = s.execute("layer.addItem", json!({"comp": outer.0, "item": inner.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    let p = Arc::make_mut(&mut s.project);
    let nl = p.comp_mut(outer).unwrap().layer_mut(LayerId(l)).unwrap();
    nl.start_time = Tick::from_seconds_f64(start);
    (nl.in_point, nl.out_point) = (Tick::from_seconds_f64(in_out.0), Tick::from_seconds_f64(in_out.1));
    (outer, l)
}

#[test]
fn a_null_camera_move_is_a_camera_hit_whose_id_survives_a_retime() {
    let mut s = session();
    let main = comp(&mut s, "MAIN", 6.0);
    let cam = s.execute("layer.newNull", json!({"name": "H1 05 Cam"})).unwrap()["layer"].as_u64().unwrap();
    let card = solid(&mut s, "H1 05 Card");
    s.execute("layer.setParent", json!({"layers": [card], "parent": cam})).unwrap();
    keys(&mut s, cam, "transform/position", (1.0, json!([320.0, 180.0])), (2.0, json!([520.0, 180.0])), false);
    s.execute(
        "speech.import",
        json!({"map": {"engine": "test", "words": [{"word": "content", "start": 1.4, "end": 1.8, "syllables": [{"t": 1.4, "vowel": 1.52, "stress": 1}]}]}}),
    )
    .unwrap();

    let r = hits(&mut s);
    assert_eq!(r["comp"].as_u64(), Some(main.0), "{r:#}");
    let h = list(&r);
    assert_eq!(h.len(), 1, "only the rig moves: {r:#}");
    let hit = &h[0];
    assert_eq!(hit["kind"], "camera", "{hit:#}");
    assert_eq!(hit["layerName"], "H1 05 Cam");
    assert_eq!(hit["source"], "keys");
    assert_eq!(hit["peakIs"], "fastest");
    let id = hit["id"].as_str().unwrap().to_string();
    assert!(id.starts_with(&format!("{cam}:@")) && id.ends_with(":k0"), "{id}");
    assert!(near(&hit["start"]["main"], 1.0, 1e-3) && near(&hit["end"]["main"], 2.0, 1e-3), "{hit:#}");
    assert!(near(&hit["peak"]["main"], 1.5, 0.01) && near(&hit["peak"]["mainFrame"], 37.5, 0.3), "{hit:#}");
    assert!(near(&hit["size"]["px"], 200.0, 0.01), "{hit:#}");
    // Nearest word and stress: accent = vowel 1.52 − 1 frame = 1.48; the peak is half a frame late.
    assert_eq!(hit["word"]["word"], "content");
    assert!(near(&hit["stress"]["accent"], 1.48, 1e-3) && near(&hit["stress"]["offsetFrames"], 0.5, 0.3), "{hit:#}");

    // Retimed 0.5 s later: same id, the peak follows; the revision changed.
    let rev = s.execute("sfx.hitsRevision", json!({})).unwrap()["revision"].as_u64().unwrap();
    assert_eq!(Some(rev), r["revision"].as_u64());
    s.edit("Retime", None, |p, _| {
        let l = p.comp_mut(main).unwrap().layer_mut(LayerId(cam)).unwrap();
        let pos = l.props.prop_mut("transform/position").unwrap();
        for k in &mut pos.keys {
            k.time = k.time + Tick::from_seconds_f64(0.5);
        }
        Ok(())
    })
    .unwrap();
    let r2 = hits(&mut s);
    let h2 = list(&r2);
    assert_eq!(h2.len(), 1, "{r2:#}");
    assert_eq!(h2[0]["id"].as_str(), Some(id.as_str()));
    assert!(near(&h2[0]["peak"]["main"], 2.0, 0.01), "{r2:#}");
    assert!(s.execute("sfx.hitsRevision", json!({})).unwrap()["revision"].as_u64().unwrap() > rev);
}

#[test]
fn an_expression_driven_slider_is_a_run_and_what_it_drives_folds_into_it() {
    let mut s = session();
    comp(&mut s, "MAIN", 4.0);
    let ctrl = s.execute("layer.newNull", json!({"name": "S1 04 CTRL"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("effect.apply", json!({"layers": [ctrl], "effect": "ec.control.slider"})).unwrap();
    s.execute("prop.setExpression", json!({"layer": ctrl, "path": "effects/#1/slider", "expression": "linear(time, 1, 2, 0, 30)"})).unwrap();
    let day = solid(&mut s, "S1 04 Day 1");
    s.execute(
        "prop.setExpression",
        json!({"layer": day, "path": "transform/opacity", "expression": "Math.min(100, thisComp.layer('S1 04 CTRL').effect('Slider Control')('Slider') * 10)"}),
    )
    .unwrap();

    let r = hits(&mut s);
    let h = list(&r);
    assert_eq!(h.len(), 1, "one run, the day's fade-in folded into it: {r:#}");
    let run = &h[0];
    assert_eq!((run["kind"].as_str(), run["source"].as_str()), (Some("run"), Some("expression")), "{run:#}");
    let id = run["id"].as_str().unwrap();
    assert!(id.starts_with(&format!("run:{ctrl}:@")) && id.ends_with(":x0"), "{id}");
    assert!(near(&run["start"]["main"], 1.0, 0.045) && near(&run["end"]["main"], 2.0, 0.045), "{run:#}");
    assert!(near(&run["size"]["value"], 30.0, 0.5), "{run:#}");
    assert_eq!(run["driven"].as_u64(), Some(1), "{run:#}");

    // A keyed slider ramp is a run too, with a key-index id.
    let keyed = s.execute("layer.newNull", json!({"name": "S1 05 CTRL"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("effect.apply", json!({"layers": [keyed], "effect": "ec.control.slider"})).unwrap();
    keys(&mut s, keyed, "effects/#1/slider", (3.0, json!(0.0)), (3.5, json!(12.0)), true);
    let r = hits(&mut s);
    let runs: Vec<Value> = list(&r).into_iter().filter(|h| h["kind"] == "run").collect();
    assert_eq!(runs.len(), 2, "{r:#}");
    assert!(runs[1]["id"].as_str().unwrap().starts_with(&format!("run:{keyed}:")) && runs[1]["id"].as_str().unwrap().ends_with(":k0"), "{r:#}");
    assert_eq!(runs[1]["peakIs"], "landing", "linear keys: no speed peak, so the landing");
}

#[test]
fn a_move_two_precomps_deep_is_found_at_the_main_frame() {
    // Card's box scales in 1.0–1.5 s (peak 1.25); Card in Middle from 1 s; Middle in MAIN from 2 s:
    // MAIN 4.0–4.5, peak 4.25.
    let mut s = session();
    let card = comp(&mut s, "Card", 4.0);
    let bx = solid(&mut s, "S2 01 Card Box");
    keys(&mut s, bx, "transform/scale", (1.0, json!([0.0, 0.0, 100.0])), (1.5, json!([100.0, 100.0, 100.0])), false);
    let (middle, inner_layer) = nest(&mut s, card, "Middle", 1.0, (1.0, 5.0));
    let (_, top) = nest(&mut s, middle, "MAIN", 2.0, (2.0, 8.0));

    let r = hits(&mut s);
    let h = list(&r);
    let appear = h.iter().find(|h| h["kind"] == "appear" && h["source"] == "keys").unwrap_or_else(|| panic!("{r:#}"));
    assert_eq!(appear["layerName"], "S2 01 Card Box");
    assert_eq!(appear["compName"], "Card");
    assert_eq!(appear["via"], json!([top, inner_layer]), "{appear:#}");
    assert_eq!(appear["scene"], "Middle", "the top-level layer it is in");
    assert!(appear["id"].as_str().unwrap().starts_with(&format!("{top}>{inner_layer}>{bx}:")), "{appear:#}");
    assert!(near(&appear["start"]["main"], 4.0, 1e-3) && near(&appear["end"]["main"], 4.5, 1e-3), "{appear:#}");
    assert!(near(&appear["peak"]["main"], 4.25, 0.01) && near(&appear["peak"]["comp"], 1.25, 0.01), "{appear:#}");
    assert!(near(&appear["size"]["scalePct"], 100.0, 0.01), "{appear:#}");
    // The scene change: Middle starts in MAIN at 2 s.
    let scene = h.iter().find(|h| h["kind"] == "scene" && h["id"] == format!("scene:{top}")).unwrap_or_else(|| panic!("{r:#}"));
    assert!(near(&scene["peak"]["main"], 2.0, 1e-3), "{scene:#}");
    // A window before it: only the scene.
    let r = s.execute("sfx.hits", json!({"start": 0, "end": 3.5})).unwrap();
    assert!(list(&r).iter().all(|h| h["kind"] == "scene"), "{r:#}");
}

#[test]
fn staggered_elements_are_one_hit_and_plates_guides_and_hidden_layers_are_skipped() {
    let mut s = session();
    let main = comp(&mut s, "MAIN", 6.0);
    let a = solid(&mut s, "S1 02 Row 1");
    let b = solid(&mut s, "S1 02 Row 2");
    keys(&mut s, a, "transform/opacity", (1.0, json!(0.0)), (1.2, json!(100.0)), true);
    keys(&mut s, b, "transform/opacity", (1.04, json!(0.0)), (1.24, json!(100.0)), true);
    keys(&mut s, a, "transform/opacity", (3.0, json!(100.0)), (3.2, json!(0.0)), true);
    for name in ["BG PLATE 1", "S1 02 Guide", "S1 02 Off"] {
        let l = solid(&mut s, name);
        keys(&mut s, l, "transform/opacity", (2.0, json!(0.0)), (2.2, json!(100.0)), false);
        let p = Arc::make_mut(&mut s.project);
        let layer = p.comp_mut(main).unwrap().layer_mut(LayerId(l)).unwrap();
        match name {
            "S1 02 Guide" => layer.switches.guide = true,
            "S1 02 Off" => layer.switches.video = false,
            _ => {}
        }
    }
    let r = hits(&mut s);
    let h = list(&r);
    assert_eq!(h.len(), 2, "{r:#}");
    assert_eq!((h[0]["kind"].as_str(), h[0]["n"].as_u64()), (Some("appear"), Some(2)), "{r:#}");
    assert_eq!(h[0]["layerName"], "S1 02 Row 1", "the first to start leads");
    assert_eq!(h[0]["peakIs"], "landing");
    assert!(near(&h[0]["peak"]["main"], 1.2, 1e-3) && near(&h[0]["end"]["main"], 1.24, 1e-3), "{r:#}");
    assert_eq!(h[0]["members"].as_array().map(Vec::len), Some(2));
    assert_eq!(h[1]["kind"], "exit");
    assert!(h[0]["id"].as_str().unwrap().ends_with(":k0") && h[1]["id"].as_str().unwrap().ends_with(":k2"), "{r:#}");
    assert_eq!(r["kinds"], json!({"appear": 1, "exit": 1}));
}

#[test]
fn bad_ranges_and_missing_comps_are_refused() {
    let mut s = session();
    assert!(s.execute("sfx.hits", json!({})).is_err(), "no comp at all");
    comp(&mut s, "Other", 2.0);
    assert!(s.execute("sfx.hits", json!({})).is_ok(), "no MAIN: the active comp");
    assert!(s.execute("sfx.hits", json!({"start": 2, "end": 1})).is_err());
    assert!(s.execute("sfx.hits", json!({"comp": "Nope"})).is_err());
    assert_eq!(s.execute("sfx.hits", json!({})).unwrap()["count"], 0);
}
